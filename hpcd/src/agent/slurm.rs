use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

use crate::state::db::{ParseSlurmVersionError, SlurmVersion};

pub const DETERMINE_SLURM_VERSION_CMD: &str = r#"(scontrol --version 2>/dev/null || srun --version 2>/dev/null || sinfo --version 2>/dev/null || squeue --version 2>/dev/null; )"#;

pub const GATHER_PARTITIONS_SLURM_CMD: &str = "scontrol show partition -o";

// SLURM PARTITION INFO GATHERING

/// Represents a single SLURM partition as parsed from `scontrol show partition -o`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    /// The `PartitionName` value.
    pub name: String,
    /// All key=value pairs for the line (excluding `PartitionName`, which is in `name`).
    pub fields: HashMap<String, String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("partition is missing required key: {0}")]
    MissingField(&'static str),
}

/// Parse the output of `scontrol show partition -o` into a vector of Partitions.
///
/// Each non-empty line is expected to be a series of `key=value` tokens separated by whitespace.
/// Only the first '=' in a token is considered the key/value delimiter to handle values like
/// `TRESBillingWeights=CPU=1.0,Mem=0`.
pub fn parse_scontrol_partitions(input: &str) -> Result<Vec<Partition>, ParseError> {
    let mut parts = Vec::new();

    for line in input.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let mut map = HashMap::<String, String>::new();

        for tok in line.split_whitespace() {
            if let Some(eq) = tok.find('=') {
                let (k, v_with_eq) = tok.split_at(eq);
                let v = &v_with_eq[1..]; // skip '='
                map.insert(k.to_string(), v.to_string());
            }
        }

        let name = map
            .remove("PartitionName")
            .ok_or(ParseError::MissingField("PartitionName"))?;

        parts.push(Partition { name, fields: map });
    }

    Ok(parts)
}

impl Partition {
    /// Get a raw field value as &str.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(|s| s.as_str())
    }
    /// Parse a field that behaves like a boolean: YES/NO, TRUE/FALSE, ON/OFF, 1/0.
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        let s = self.get(key)?.trim().to_ascii_lowercase();
        match s.as_str() {
            "yes" | "true" | "on" | "1" => Some(true),
            "no" | "false" | "off" | "0" => Some(false),
            _ => None,
        }
    }

    /// Parse a field as u64, returning None for non-numeric values (e.g., UNLIMITED).
    pub fn get_u64(&self, key: &str) -> Option<u64> {
        let s = self.get(key)?.trim();
        if s.chars().all(|c| c.is_ascii_digit()) {
            s.parse::<u64>().ok()
        } else {
            None
        }
    }

    /// Parse a SLURM-style duration (e.g., "21-00:00:00" or "03:00:00") into `Duration`.
    /// Returns None for markers like "UNLIMITED", "NONE", or "N/A".
    pub fn get_duration(&self, key: &str) -> Option<Duration> {
        parse_slurm_duration(self.get(key)?)
    }
}

/// Parse SLURM durations like "D-HH:MM:SS" or "HH:MM:SS".
fn parse_slurm_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("unlimited")
        || s.eq_ignore_ascii_case("none")
        || s.eq_ignore_ascii_case("n/a")
    {
        return None;
    }

    // Split optional days: "D-HH:MM:SS" or just "HH:MM:SS"
    let (days, hms) = if let Some(dash) = s.find('-') {
        let (d, rest) = s.split_at(dash);
        let d: u64 = d.parse().ok()?;
        (d, &rest[1..])
    } else {
        (0, s)
    };

    let mut it = hms.split(':');
    let (h, m, sec) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() {
        return None; // too many components
    }

    let (h, m, sec): (u64, u64, u64) = (h.parse().ok()?, m.parse().ok()?, sec.parse().ok()?);
    let total = days
        .saturating_mul(24 * 3600)
        .saturating_add(h * 3600)
        .saturating_add(m * 60)
        .saturating_add(sec);

    Some(Duration::from_secs(total))
}

pub fn parse_job_id(line: &str) -> Option<i64> {
    // Expect message from sbatch like: "Submitted batch job 11"
    // Strategy:
    // 1. Find the substring "job "
    // 2. Take everything after it
    // 3. Parse that as an integer (11, for the example string above)

    // Find where "job " occurs
    let marker = "job ";
    let idx = line.find(marker)?;

    // Slice from the end of "job " to the end of the string
    let after_job = &line[idx + marker.len()..];

    // Trim spaces just in case, then parse as u64
    after_job.trim().parse::<i64>().ok()
}

pub fn parse_accounting_enabled_from_scontrol(config: &str) -> Option<bool> {
    for line in config.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("AccountingStorageType") {
            let value = value.trim();
            if value.is_empty() {
                return None;
            }
            let lowered = value.to_ascii_lowercase();
            let enabled = !(lowered == "none" || lowered == "accounting_storage/none");
            return Some(enabled);
        }
    }
    None
}

pub fn parse_sacct_states(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let state = line.split('|').next().unwrap_or(line).trim();
            if state.is_empty() {
                None
            } else {
                Some(state.to_string())
            }
        })
        .collect()
}

pub fn sacct_output_is_running(output: &str) -> Option<bool> {
    let states = parse_sacct_states(output);
    if states.is_empty() {
        return None;
    }
    for state in states {
        let normalized = normalize_slurm_state(&state);
        if is_slurm_state_active(&normalized) {
            return Some(true);
        }
        if !is_slurm_state_terminal(&normalized) {
            return Some(true);
        }
    }
    Some(false)
}

pub fn sacct_terminal_state(output: &str) -> Option<String> {
    let states = parse_sacct_states(output);
    if states.is_empty() {
        return None;
    }
    let mut normalized = Vec::with_capacity(states.len());
    for state in states {
        let token = normalize_slurm_state(&state);
        if is_slurm_state_active(&token) || !is_slurm_state_terminal(&token) {
            return None;
        }
        normalized.push(token);
    }
    if normalized.iter().all(|state| state == "COMPLETED") {
        return Some("COMPLETED".to_string());
    }
    normalized
        .into_iter()
        .find(|state| state != "COMPLETED")
}

fn normalize_slurm_state(state: &str) -> String {
    let token = state
        .split(|c| c == '+' || c == ':' || c == '(')
        .next()
        .unwrap_or(state)
        .trim();
    token.to_ascii_uppercase()
}

fn is_slurm_state_active(state: &str) -> bool {
    matches!(
        state,
        "PENDING"
            | "RUNNING"
            | "CONFIGURING"
            | "COMPLETING"
            | "SUSPENDED"
            | "RESIZING"
            | "REQUEUED"
            | "STAGE_OUT"
            | "STAGE_IN"
            | "SIGNALING"
    )
}

fn is_slurm_state_terminal(state: &str) -> bool {
    matches!(
        state,
        "COMPLETED"
            | "CANCELLED"
            | "FAILED"
            | "TIMEOUT"
            | "NODE_FAIL"
            | "PREEMPTED"
            | "BOOT_FAIL"
            | "OUT_OF_MEMORY"
            | "DEADLINE"
            | "SPECIAL_EXIT"
            | "REVOKED"
    )
}

// returns a command to be executed on cluster to submit the job
pub fn path_to_sbatch_command(p: &str, remote_base_path: Option<&str>) -> String {
    if let Some(chdir_path) = remote_base_path {
        return format!("sbatch --chdir {} {}", chdir_path, p);
    } else {
        return format!("sbatch {}", p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"PartitionName=cpu_std_interactive AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=interactive DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=06:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=x[1-500],y[1-8] PriorityJobFactor=1 PriorityTier=1 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=120000 TotalNodes=508 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=192 MaxMemPerNode=UNLIMITED TRES=cpu=120000,mem=420000000M,node=508,billing=120000000 TRESBillingWeights=CPU=900,Mem=200G
PartitionName=cpu_flexbackfill AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=2-00:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=x[1-150,155-500],z[1-12] PriorityJobFactor=1 PriorityTier=1 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=122400 TotalNodes=508 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=192 MaxMemPerNode=UNLIMITED TRES=cpu=122400,mem=475000000M,node=508,billing=131000000 TRESBillingWeights=CPU=900,Mem=200G
PartitionName=cpu_bycore_q1 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=04:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=x[1-150,155-500],y[1-6] PriorityJobFactor=12 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=118800 TotalNodes=496 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=192 MaxMemPerNode=UNLIMITED TRES=cpu=118800,mem=410000000M,node=496,billing=118800000 TRESBillingWeights=CPU=900,Mem=200G
PartitionName=cpu_bycore_q2 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=10:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=x[1-150,155-500],y[1-6] PriorityJobFactor=10 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=118800 TotalNodes=496 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=192 MaxMemPerNode=UNLIMITED TRES=cpu=118800,mem=410000000M,node=496,billing=118800000 TRESBillingWeights=CPU=900,Mem=200G
PartitionName=cpu_bycore_q3 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=20:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=x[1-150,155-500],y[1-6] PriorityJobFactor=8 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=118800 TotalNodes=496 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=192 MaxMemPerNode=UNLIMITED TRES=cpu=118800,mem=410000000M,node=496,billing=118800000 TRESBillingWeights=CPU=900,Mem=200G
PartitionName=cpu_bycore_q4 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=4-00:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=x[1-150,155-500],y[1-6] PriorityJobFactor=6 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=118800 TotalNodes=496 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=192 MaxMemPerNode=UNLIMITED TRES=cpu=118800,mem=410000000M,node=496,billing=118800000 TRESBillingWeights=CPU=900,Mem=200G
PartitionName=cpu_bycore_q5 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=9-00:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=x[1-150,155-500],y[1-6] PriorityJobFactor=4 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=118800 TotalNodes=496 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=192 MaxMemPerNode=UNLIMITED TRES=cpu=118800,mem=410000000M,node=496,billing=118800000 TRESBillingWeights=CPU=900,Mem=200G
PartitionName=cpu_bynode_q1 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=04:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=x[20-500],y[1-6] PriorityJobFactor=12 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=116000 TotalNodes=487 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=192 MaxMemPerNode=UNLIMITED TRES=cpu=116000,mem=405000000M,node=487,billing=116000000 TRESBillingWeights=CPU=900,Mem=200G
PartitionName=cpu_bynode_q2 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=10:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=x[20-500],y[1-6] PriorityJobFactor=10 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=116000 TotalNodes=487 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=192 MaxMemPerNode=UNLIMITED TRES=cpu=116000,mem=405000000M,node=487,billing=116000000 TRESBillingWeights=CPU=900,Mem=200G
PartitionName=cpu_bynode_q3 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=1-06:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=x[20-500],y[1-6] PriorityJobFactor=8 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=116000 TotalNodes=487 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=192 MaxMemPerNode=UNLIMITED TRES=cpu=116000,mem=405000000M,node=487,billing=116000000 TRESBillingWeights=CPU=900,Mem=200G
PartitionName=cpu_bynode_q4 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=3-12:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=x[20-500],y[1-6] PriorityJobFactor=6 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=116000 TotalNodes=487 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=192 MaxMemPerNode=UNLIMITED TRES=cpu=116000,mem=405000000M,node=487,billing=116000000 TRESBillingWeights=CPU=900,Mem=200G
PartitionName=cpu_bynode_q5 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=8-00:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=x[20-500],y[1-6] PriorityJobFactor=4 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=116000 TotalNodes=487 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=192 MaxMemPerNode=UNLIMITED TRES=cpu=116000,mem=405000000M,node=487,billing=116000000 TRESBillingWeights=CPU=900,Mem=200G
PartitionName=cpu_large_interactive AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=interactive DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=06:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=v[100-600],z[1-6] PriorityJobFactor=12 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=80000 TotalNodes=507 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=80000,mem=380000000M,node=507,billing=90000000 TRESBillingWeights=CPU=950,Mem=230G
PartitionName=cpu_large_bycore_q1 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=02:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=v[400-600],z[1-6] PriorityJobFactor=12 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=36000 TotalNodes=203 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=36000,mem=200000000M,node=203,billing=47000000 TRESBillingWeights=CPU=950,Mem=230G
PartitionName=cpu_large_bycore_q2 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=10:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=v[400-600],z[1-6] PriorityJobFactor=10 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=36000 TotalNodes=203 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=36000,mem=200000000M,node=203,billing=47000000 TRESBillingWeights=CPU=950,Mem=230G
PartitionName=cpu_large_bycore_q3 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=22:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=v[400-600],z[1-6] PriorityJobFactor=8 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=36000 TotalNodes=203 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=36000,mem=200000000M,node=203,billing=47000000 TRESBillingWeights=CPU=950,Mem=230G
PartitionName=cpu_large_bycore_q4 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=3-06:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=v[400-600],z[1-6] PriorityJobFactor=6 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=36000 TotalNodes=203 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=36000,mem=200000000M,node=203,billing=47000000 TRESBillingWeights=CPU=950,Mem=230G
PartitionName=cpu_large_bycore_q5 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=8-12:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=v[450-600],z[1-6] PriorityJobFactor=4 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=19000 TotalNodes=157 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=19000,mem=130000000M,node=157,billing=30000000 TRESBillingWeights=CPU=950,Mem=230G
PartitionName=cpu_large_bynode_q1 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=02:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=z[1-8] PriorityJobFactor=12 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=1600 TotalNodes=8 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=1600,mem=48000G,node=8,billing=12000000 TRESBillingWeights=CPU=950,Mem=230G
PartitionName=cpu_large_bynode_q2 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=10:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=z[1-8] PriorityJobFactor=10 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=1600 TotalNodes=8 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=1600,mem=48000G,node=8,billing=12000000 TRESBillingWeights=CPU=950,Mem=230G
PartitionName=cpu_large_bynode_q3 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=1-06:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=z[1-8] PriorityJobFactor=8 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=1600 TotalNodes=8 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=1600,mem=48000G,node=8,billing=12000000 TRESBillingWeights=CPU=950,Mem=230G
PartitionName=cpu_large_bynode_q4 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=3-12:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=z[1-8] PriorityJobFactor=6 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=1600 TotalNodes=8 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=1600,mem=48000G,node=8,billing=12000000 TRESBillingWeights=CPU=950,Mem=230G
PartitionName=cpu_large_bynode_q5 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:10:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=8-12:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=z[1-8] PriorityJobFactor=4 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=1600 TotalNodes=8 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=1600,mem=48000G,node=8,billing=12000000 TRESBillingWeights=CPU=950,Mem=230G
PartitionName=burst AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=interactive DefaultTime=00:05:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=5-00:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=v250 PriorityJobFactor=1 PriorityTier=1 RootOnly=NO ReqResv=NO OverSubscribe=FORCE:3 OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=128 TotalNodes=1 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=128 MaxMemPerNode=UNLIMITED TRES=cpu=128,mem=500000M,node=1,billing=150000 TRESBillingWeights=CPU=800,Mem=40G,GRES/gpu:nvidia_h200_1g.12gb=1500,GRES/gpu:nvidia_h200_2g.24gb=3000,GRES/gpu:nvidia_h200_3g.36gb=4500,GRES/gpu:h200=11000
PartitionName=interactive_small AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=interactive DefaultTime=00:20:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=06:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=w[10-11],g[1-2],y8 PriorityJobFactor=1 PriorityTier=1 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=640 TotalNodes=4 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=640,mem=5200000M,node=4,billing=520000,gres/gpu=32,gres/gpu:nvidia_h200_1g.12gb=16,gres/gpu:nvidia_h200_2g.24gb=8,gres/gpu:nvidia_h200_3g.36gb=8 TRESBillingWeights=CPU=800,Mem=40G,GRES/gpu:nvidia_h200_1g.12gb=1500,GRES/gpu:nvidia_h200_2g.24gb=3000,GRES/gpu:nvidia_h200_3g.36gb=4500,GRES/gpu:h200=11000
PartitionName=gpu_flexbackfill AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:20:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=2-00:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=g[10-12,15-44] PriorityJobFactor=12 PriorityTier=1 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=4200 TotalNodes=33 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=4200,mem=72000G,node=33,billing=3600000,gres/gpu=384,gres/gpu:h200=128,gres/gpu:nvidia_h200_1g.12gb=128,gres/gpu:nvidia_h200_2g.24gb=64,gres/gpu:nvidia_h200_3g.36gb=64 TRESBillingWeights=CPU=800,Mem=40G,GRES/gpu:nvidia_h200_1g.12gb=1500,GRES/gpu:nvidia_h200_2g.24gb=3000,GRES/gpu:nvidia_h200_3g.36gb=4500,GRES/gpu:h200=11000
PartitionName=gpu_std_interactive AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=interactive DefaultTime=00:20:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=06:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=g[10-12,15-44] PriorityJobFactor=12 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=4200 TotalNodes=33 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=4200,mem=72000G,node=33,billing=3600000,gres/gpu=384,gres/gpu:h200=128,gres/gpu:nvidia_h200_1g.12gb=128,gres/gpu:nvidia_h200_2g.24gb=64,gres/gpu:nvidia_h200_3g.36gb=64 TRESBillingWeights=CPU=800,Mem=40G,GRES/gpu:nvidia_h200_1g.12gb=1500,GRES/gpu:nvidia_h200_2g.24gb=3000,GRES/gpu:nvidia_h200_3g.36gb=4500,GRES/gpu:h200=11000
PartitionName=gpu_bygpu_q1 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:20:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=02:30:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=g[20-44] PriorityJobFactor=12 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=3000 TotalNodes=25 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=3000,mem=52000G,node=25,billing=2500000,gres/gpu=280,gres/gpu:h200=120,gres/gpu:nvidia_h200_1g.12gb=80,gres/gpu:nvidia_h200_2g.24gb=40,gres/gpu:nvidia_h200_3g.36gb=40 TRESBillingWeights=CPU=800,Mem=40G,GRES/gpu:nvidia_h200_1g.12gb=1500,GRES/gpu:nvidia_h200_2g.24gb=3000,GRES/gpu:nvidia_h200_3g.36gb=4500,GRES/gpu:h200=11000
PartitionName=gpu_bygpu_q2 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:20:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=10:30:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=g[20-44] PriorityJobFactor=10 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=CANCEL State=UP TotalCPUs=3000 TotalNodes=25 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=3000,mem=52000G,node=25,billing=2500000,gres/gpu=280,gres/gpu:h200=120,gres/gpu:nvidia_h200_1g.12gb=80,gres/gpu:nvidia_h200_2g.24gb=40,gres/gpu:nvidia_h200_3g.36gb=40 TRESBillingWeights=CPU=800,Mem=40G,GRES/gpu:nvidia_h200_1g.12gb=1500,GRES/gpu:nvidia_h200_2g.24gb=3000,GRES/gpu:nvidia_h200_3g.36gb=4500,GRES/gpu:h200=11000
PartitionName=gpu_bygpu_q3 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:20:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=1-04:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=g[20-44] PriorityJobFactor=8 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=3000 TotalNodes=25 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=3000,mem=52000G,node=25,billing=2500000,gres/gpu=280,gres/gpu:h200=120,gres/gpu:nvidia_h200_1g.12gb=80,gres/gpu:nvidia_h200_2g.24gb=40,gres/gpu:nvidia_h200_3g.36gb=40 TRESBillingWeights=CPU=800,Mem=40G,GRES/gpu:nvidia_h200_1g.12gb=1500,GRES/gpu:nvidia_h200_2g.24gb=3000,GRES/gpu:nvidia_h200_3g.36gb=4500,GRES/gpu:h200=11000
PartitionName=gpu_bygpu_q4 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:20:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=3-04:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=g[20-44] PriorityJobFactor=6 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=3000 TotalNodes=25 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=3000,mem=52000G,node=25,billing=2500000,gres/gpu=280,gres/gpu:h200=120,gres/gpu:nvidia_h200_1g.12gb=80,gres/gpu:nvidia_h200_2g.24gb=40,gres/gpu:nvidia_h200_3g.36gb=40 TRESBillingWeights=CPU=800,Mem=40G,GRES/gpu:nvidia_h200_1g.12gb=1500,GRES/gpu:nvidia_h200_2g.24gb=3000,GRES/gpu:nvidia_h200_3g.36gb=4500,GRES/gpu:h200=11000
PartitionName=gpu_bygpu_q5 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:20:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=7-04:00:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=g[20-44] PriorityJobFactor=4 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=3000 TotalNodes=25 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=3000,mem=52000G,node=25,billing=2500000,gres/gpu=280,gres/gpu:h200=120,gres/gpu:nvidia_h200_1g.12gb=80,gres/gpu:nvidia_h200_2g.24gb=40,gres/gpu:nvidia_h200_3g.36gb=40 TRESBillingWeights=CPU=800,Mem=40G,GRES/gpu:nvidia_h200_1g.12gb=1500,GRES/gpu:nvidia_h200_2g.24gb=3000,GRES/gpu:nvidia_h200_3g.36gb=4500,GRES/gpu:h200=11000
PartitionName=gpu_bynode_q1 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:20:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=02:30:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=g[15-44] PriorityJobFactor=12 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=3400 TotalNodes=30 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=3400,mem=58.0T,node=30,billing=2900000,gres/gpu=360,gres/gpu:h200=140,gres/gpu:nvidia_h200_1g.12gb=96,gres/gpu:nvidia_h200_2g.24gb=48,gres/gpu:nvidia_h200_3g.36gb=48 TRESBillingWeights=CPU=800,Mem=40G,GRES/gpu:nvidia_h200_1g.12gb=1500,GRES/gpu:nvidia_h200_2g.24gb=3000,GRES/gpu:nvidia_h200_3g.36gb=4500,GRES/gpu:h200=11000
PartitionName=gpu_bynode_q2 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:20:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=10:30:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=g[15-44] PriorityJobFactor=10 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=3400 TotalNodes=30 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=3400,mem=58.0T,node=30,billing=2900000,gres/gpu=360,gres/gpu:h200=140,gres/gpu:nvidia_h200_1g.12gb=96,gres/gpu:nvidia_h200_2g.24gb=48,gres/gpu:nvidia_h200_3g.36gb=48 TRESBillingWeights=CPU=800,Mem=40G,GRES/gpu:nvidia_h200_1g.12gb=1500,GRES/gpu:nvidia_h200_2g.24gb=3000,GRES/gpu:nvidia_h200_3g.36gb=4500,GRES/gpu:h200=11000
PartitionName=gpu_bynode_q3 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:20:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=1-04:30:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=g[15-44] PriorityJobFactor=8 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=3400 TotalNodes=30 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=3400,mem=58.0T,node=30,billing=2900000,gres/gpu=360,gres/gpu:h200=140,gres/gpu:nvidia_h200_1g.12gb=96,gres/gpu:nvidia_h200_2g.24gb=48,gres/gpu:nvidia_h200_3g.36gb=48 TRESBillingWeights=CPU=800,Mem=40G,GRES/gpu:nvidia_h200_1g.12gb=1500,GRES/gpu:nvidia_h200_2g.24gb=3000,GRES/gpu:nvidia_h200_3g.36gb=4500,GRES/gpu:h200=11000
PartitionName=gpu_bynode_q4 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:20:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=3-04:30:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=g[15-44] PriorityJobFactor=6 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=3400 TotalNodes=30 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=3400,mem=58.0T,node=30,billing=2900000,gres/gpu=360,gres/gpu:h200=140,gres/gpu:nvidia_h200_1g.12gb=96,gres/gpu:nvidia_h200_2g.24gb=48,gres/gpu:nvidia_h200_3g.36gb=48 TRESBillingWeights=CPU=800,Mem=40G,GRES/gpu:nvidia_h200_1g.12gb=1500,GRES/gpu:nvidia_h200_2g.24gb=3000,GRES/gpu:nvidia_h200_3g.36gb=4500,GRES/gpu:h200=11000
PartitionName=gpu_bynode_q5 AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=NO QoS=N/A DefaultTime=00:20:00 DisableRootJobs=YES ExclusiveUser=NO ExclusiveTopo=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=7-04:30:00 MinNodes=1 LLN=NO MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=g[15-44] PriorityJobFactor=4 PriorityTier=10 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=3400 TotalNodes=30 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerCPU=256 MaxMemPerNode=UNLIMITED TRES=cpu=3400,mem=58.0T,node=30,billing=2900000,gres/gpu=360,gres/gpu:h200=140,gres/gpu:nvidia_h200_1g.12gb=96,gres/gpu:nvidia_h200_2g.24gb=48,gres/gpu:nvidia_h200_3g.36gb=48 TRESBillingWeights=CPU=800,Mem=40G,GRES/gpu:nvidia_h200_1g.12gb=1500,GRES/gpu:nvidia_h200_2g.24gb=3000,GRES/gpu:nvidia_h200_3g.36gb=4500,GRES/gpu:h200=11000
"#;

    #[test]
    fn parses_all_partitions() {
        let v = parse_scontrol_partitions(SAMPLE).unwrap();
        assert_eq!(v.len(), 37);
        assert_eq!(v[0].name, "cpu_std_interactive");
        assert_eq!(v[36].name, "gpu_bynode_q5");
    }

    #[test]
    fn keeps_values_with_internal_equals() {
        let v = parse_scontrol_partitions(SAMPLE).unwrap();
        let cpu_large_bynode_q4 = v.iter().find(|p| p.name == "cpu_large_bynode_q4").unwrap();
        assert_eq!(
            cpu_large_bynode_q4.get("TRESBillingWeights"),
            Some("CPU=950,Mem=230G")
        );
    }

    #[test]
    fn extracts_some_known_fields() {
        let v = parse_scontrol_partitions(SAMPLE).unwrap();
        let gpu_bygpu_q1 = v.iter().find(|p| p.name == "gpu_bygpu_q1").unwrap();
        assert_eq!(gpu_bygpu_q1.get("Nodes"), Some("g[20-44]"));
        assert_eq!(gpu_bygpu_q1.get("TotalNodes"), Some("25"));
        assert_eq!(gpu_bygpu_q1.get_u64("TotalNodes"), Some(25));
        assert_eq!(gpu_bygpu_q1.get_bool("ExclusiveUser"), Some(false));
    }

    #[test]
    fn parses_slurm_durations() {
        // From skylake
        let v = parse_scontrol_partitions(SAMPLE).unwrap();
        let cpu_bycore_q2 = v.iter().find(|p| p.name == "cpu_bycore_q2").unwrap();

        let dt = cpu_bycore_q2.get_duration("DefaultTime").unwrap();
        assert_eq!(dt.as_secs(), 10 * 60);

        let mt = cpu_bycore_q2.get_duration("MaxTime").unwrap();
        assert_eq!(mt.as_secs(), 10 * 60 * 60);

        // Markers become None
        assert_eq!(cpu_bycore_q2.get_duration("OverTimeLimit"), None);
        assert_eq!(cpu_bycore_q2.get_duration("QoS"), None);
    }

    #[test]
    fn gpu_related_fields_present() {
        let v = parse_scontrol_partitions(SAMPLE).unwrap();
        let gpu = v.iter().find(|p| p.name == "gpu_bygpu_q1").unwrap();
        assert!(gpu.get("TRES").unwrap().contains("gres/gpu=280"));
        assert_eq!(gpu.get("PreemptMode"), Some("OFF"));

        let gpu1 = v.iter().find(|p| p.name == "gpu_bygpu_q2").unwrap();
        assert_eq!(gpu1.get("PreemptMode"), Some("CANCEL"));
        assert_eq!(
            gpu1.get("TRESBillingWeights"),
            Some(
                "CPU=800,Mem=40G,GRES/gpu:nvidia_h200_1g.12gb=1500,GRES/gpu:nvidia_h200_2g.24gb=3000,GRES/gpu:nvidia_h200_3g.36gb=4500,GRES/gpu:h200=11000"
            )
        );
    }

    #[test]
    fn parses_accounting_storage_type_from_scontrol_config() {
        let enabled = r#"
        AccountingStorageType = accounting_storage/slurmdbd
        "#;
        let disabled = "AccountingStorageType=none";
        assert_eq!(
            parse_accounting_enabled_from_scontrol(enabled),
            Some(true)
        );
        assert_eq!(
            parse_accounting_enabled_from_scontrol(disabled),
            Some(false)
        );
        assert_eq!(parse_accounting_enabled_from_scontrol(""), None);
    }

    #[test]
    fn sacct_output_detects_running_states() {
        let output = "RUNNING|\nCOMPLETED|\n";
        assert_eq!(sacct_output_is_running(output), Some(true));
    }

    #[test]
    fn sacct_output_detects_terminal_states() {
        let output = "COMPLETED|\nFAILED|\n";
        assert_eq!(sacct_output_is_running(output), Some(false));
    }

    #[test]
    fn sacct_output_empty_is_unknown() {
        assert_eq!(sacct_output_is_running(""), None);
    }

    #[test]
    fn sacct_terminal_state_prefers_failure() {
        let output = "COMPLETED|\nFAILED|\n";
        assert_eq!(sacct_terminal_state(output).as_deref(), Some("FAILED"));
    }

    #[test]
    fn sacct_terminal_state_completed_only() {
        let output = "COMPLETED|\n";
        assert_eq!(sacct_terminal_state(output).as_deref(), Some("COMPLETED"));
    }

    #[test]
    fn sacct_terminal_state_running_is_none() {
        let output = "RUNNING|\nCOMPLETED|\n";
        assert_eq!(sacct_terminal_state(output), None);
    }

    #[test]
    fn error_if_partition_name_missing() {
        let bad = "AllowGroups=ALL QoS=N/A";
        let err = parse_scontrol_partitions(bad).unwrap_err();
        assert_eq!(err, ParseError::MissingField("PartitionName"));
    }
}
