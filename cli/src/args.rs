// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "hpc", version, about, long_about = None)]
pub struct Cli {
    #[arg(
        short,
        long,
        value_name = "PATH",
        help = "Path to a TOML config file. When omitted, hpc uses the default config file location if available."
    )]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Check that the daemon is reachable.
    Ping,
    /// Submit jobs, inspect status, and retrieve outputs.
    Job(JobArgs),
    /// Add clusters and manage their configuration.
    Cluster(ClusterArgs),
}

#[derive(clap::ValueEnum, Clone, Default, Debug, Serialize, Deserialize)]
pub enum WLM {
    #[default]
    Slurm,
}

impl ToString for WLM {
    fn to_string(&self) -> String {
        match self {
            Self::Slurm => "slurm".to_owned(),
        }
    }
}

#[derive(Args, Debug)]
pub struct JobArgs {
    #[command(subcommand)]
    pub cmd: JobCmd,
}

#[derive(Subcommand, Debug)]
pub enum JobCmd {
    /// Submit a project to a cluster.
    Submit(SubmitArgs),
    /// List jobs.
    List(ListJobsArgs),
    /// Show job details.
    Get(JobGetArgs),
    /// List files in a job work directory.
    Ls(JobLsArgs),
    /// Retrieve a file or directory from a job run folder.
    Retrieve(JobRetrieveArgs),
}

#[derive(Args, Debug)]
pub struct JobGetArgs {
    /// Job id from the daemon.
    pub job_id: i64,
    #[arg(long)]
    pub cluster: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ListJobsArgs {
    #[arg(long)]
    pub cluster: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct JobRetrieveArgs {
    /// Job id from the daemon.
    pub job_id: i64,
    pub path: String,
    #[arg(long)]
    pub dest: Option<PathBuf>,
    #[arg(long)]
    pub cluster: Option<String>,
}

#[derive(Args, Debug)]
pub struct JobLsArgs {
    /// Job id from the daemon.
    pub job_id: i64,
    /// Path to list (absolute or relative to the job root).
    pub path: Option<String>,
    #[arg(long)]
    pub cluster: Option<String>,
}

#[derive(Args, Debug)]
pub struct ListClustersArgs {
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ClusterArgs {
    #[command(subcommand)]
    pub cmd: ClusterCmd,
}

#[derive(Subcommand, Debug)]
pub enum ClusterCmd {
    /// List clusters.
    List(ListClustersArgs),
    /// Show cluster details.
    Get(ClusterGetArgs),
    /// List files on a cluster.
    Ls(ClusterLsArgs),
    /// Add a new cluster.
    Add(AddClusterArgs),
    /// Update cluster parameters.
    Set(SetClusterArgs),
    /// Delete a cluster and its job records.
    Delete(DeleteClusterArgs),
}

#[derive(Args, Debug)]
pub struct ClusterGetArgs {
    pub name: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ClusterLsArgs {
    pub name: String,
    /// Path to list (absolute or relative to the default base path).
    pub path: Option<String>,
}

#[derive(Args, Debug)]
pub struct SetClusterArgs {
    pub name: String,

    /// Use a remote IP address as input
    #[arg(long, value_name = "IP")]
    pub ip: Option<String>,

    #[arg(long)]
    pub port: Option<u32>,

    #[arg(long)]
    pub identity_path: Option<String>,

    #[arg(long)]
    pub default_base_path: Option<String>,
}

#[derive(Args, Debug)]
pub struct DeleteClusterArgs {
    pub name: String,
    /// Skip the confirmation prompt.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct SubmitArgs {
    pub name: String,
    pub local_path: String,
    pub sbatchscript: Option<String>,
    #[arg(long)]
    pub headless: bool,
    #[arg(long)]
    pub remote_path: Option<String>,
    /// Include paths matching PATTERN.
    /// Rules are checked in the order they appear across --include/--exclude;
    /// the first match wins, and unmatched paths are included.
    /// Patterns match the path relative to the submit root with '/' separators.
    /// A pattern without '/' matches the basename anywhere; a pattern with '/'
    /// but no leading '/' is treated as `**/PATTERN`.
    /// A leading '/' anchors to the root, and a trailing '/' matches directories
    /// only (and prunes their contents). Globs support `*`, `?`, `**`, `[]`, `{}`.
    #[arg(long, value_name = "PATTERN", action = clap::ArgAction::Append)]
    pub include: Vec<String>,
    /// Exclude paths matching PATTERN.
    /// Rules are checked in the order they appear across --include/--exclude;
    /// the first match wins, and unmatched paths are included.
    /// Patterns match the path relative to the submit root with '/' separators.
    /// A pattern without '/' matches the basename anywhere; a pattern with '/'
    /// but no leading '/' is treated as `**/PATTERN`.
    /// A leading '/' anchors to the root, and a trailing '/' matches directories
    /// only (and prunes their contents). Globs support `*`, `?`, `**`, `[]`, `{}`.
    #[arg(long, value_name = "PATTERN", action = clap::ArgAction::Append)]
    pub exclude: Vec<String>,
}

#[derive(Args, Debug)]
pub struct AddClusterArgs {
    /// Destination in ssh format: user@host[:port] (required in headless mode)
    #[arg(value_name = "DESTINATION")]
    pub destination: Option<String>,

    /// Friendly cluster name you’ll use in other commands (e.g. "gpu01" or "lab-cluster").
    #[arg(long)]
    pub name: Option<String>,

    /// Defaults to ~/.ssh/id_ed25519.
    #[arg(long)]
    pub identity_path: Option<String>,

    #[arg(long)]
    pub default_base_path: Option<String>,

    /// Disable prompts; missing values must have defaults or the command will fail.
    #[arg(long)]
    pub headless: bool,
}
