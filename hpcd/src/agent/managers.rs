// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use std::collections::HashSet;
use std::str::FromStr;
#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Eq, PartialEq, Clone, Copy, Hash)]
pub enum WorkloadManager {
    Slurm,
    Flux,
    PBS,
    HTCondor,
}

impl FromStr for WorkloadManager {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_uppercase().as_str() {
            "SLURM" => Ok(Self::Slurm),
            "PBS" => Ok(Self::PBS),
            "HTCONDOR" => Ok(Self::HTCondor),
            "FLUX" => Ok(Self::Flux),
            other => Err(format!("unknown Workload Manager: {other}")),
        }
    }
}

pub fn parse_wlms(output: &str) -> HashSet<WorkloadManager> {
    output
        .lines()
        .filter_map(|line| line.parse::<WorkloadManager>().ok()) // ignore unknown lines
        .collect()
}

pub const DETERMINE_HPC_WORKLOAD_MANAGERS_CMD: &str = r#"for p in srun:SLURM qsub:PBS condor_q:HTCONDOR flux:FLUX; do bin=${p%%:*}; name=${p#*:}; command -v "$bin" >/dev/null 2>&1 && echo "$name" || continue; done"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::str::FromStr;

    //

    #[test]
    fn from_str_parses_known_variants_case_insensitive_and_trims() {
        assert_eq!(
            WorkloadManager::from_str("slurm").unwrap(),
            WorkloadManager::Slurm
        );
        assert_eq!(
            WorkloadManager::from_str("  SLURM ").unwrap(),
            WorkloadManager::Slurm
        );

        assert_eq!(
            WorkloadManager::from_str("Flux").unwrap(),
            WorkloadManager::Flux
        );
        assert_eq!(
            WorkloadManager::from_str("  fLuX ").unwrap(),
            WorkloadManager::Flux
        );

        assert_eq!(
            WorkloadManager::from_str("HTCondor").unwrap(),
            WorkloadManager::HTCondor
        );
        assert_eq!(
            WorkloadManager::from_str("  htcondor ").unwrap(),
            WorkloadManager::HTCondor
        );
    }

    #[test]
    fn from_str_parses_pbs() {
        // This is the expected, real-world spelling.
        // CURRENT IMPLEMENTATION: This test will FAIL because the code matches on "BPS".
        assert_eq!(
            WorkloadManager::from_str("PBS").unwrap(),
            WorkloadManager::PBS
        );
        assert_eq!(
            WorkloadManager::from_str("  pBs ").unwrap(),
            WorkloadManager::PBS
        );
    }

    #[test]
    fn from_str_rejects_unknowns() {
        let err = WorkloadManager::from_str("Torque").unwrap_err();
        assert!(err.contains("unknown Workload Manager"));
    }

    // ---------- parse_wlms ----------

    #[test]
    fn parse_wlms_collects_unique_known_variants() {
        let input = "SLURM\nPBS\nHTCONDOR\nFLUX\n";
        let set = parse_wlms(input);

        let expected: HashSet<WorkloadManager> = [
            WorkloadManager::Slurm,
            WorkloadManager::PBS,
            WorkloadManager::HTCondor,
            WorkloadManager::Flux,
        ]
        .into_iter()
        .collect();

        assert_eq!(set, expected);
    }

    #[test]
    fn parse_wlms_ignores_unknown_and_deduplicates() {
        let input = "SLURM\nunknown\nPBS\nSLURM\n\nHTCONDOR\nTorque\n";
        let set = parse_wlms(input);

        let expected: HashSet<WorkloadManager> = [
            WorkloadManager::Slurm,
            WorkloadManager::PBS,
            WorkloadManager::HTCondor,
        ]
        .into_iter()
        .collect();

        assert_eq!(set, expected);
        assert_eq!(set.len(), 3); // no duplicates
    }

    // ---------- Shell command ----------

    #[test]
    fn determine_cmd_contains_expected_pairs() {
        // Expected tool-to-name mappings
        assert!(DETERMINE_HPC_WORKLOAD_MANAGERS_CMD.contains("srun:SLURM"));
        assert!(DETERMINE_HPC_WORKLOAD_MANAGERS_CMD.contains("condor_q:HTCONDOR"));
        assert!(DETERMINE_HPC_WORKLOAD_MANAGERS_CMD.contains("flux:FLUX"));

        // This is the expected spelling; the current const uses `qsub:BPS`, so this will FAIL
        // until you correct it to `qsub:PBS`.
        assert!(DETERMINE_HPC_WORKLOAD_MANAGERS_CMD.contains("qsub:PBS"));
    }

    #[test]
    fn parse_wlms_handles_output_from_command_example() {
        // Simulate plausible command output (one name per line)
        let simulated = "SLURM\nPBS\nHTCONDOR\nFLUX\n";
        let set = parse_wlms(simulated);

        assert!(set.contains(&WorkloadManager::Slurm));
        assert!(set.contains(&WorkloadManager::PBS));
        assert!(set.contains(&WorkloadManager::HTCondor));
        assert!(set.contains(&WorkloadManager::Flux));
        assert_eq!(set.len(), 4);
    }
}
