use clap::{ArgGroup, Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    Ping,
    Ls(LsArgs),
    Submit(SubmitArgs),
    Job(JobArgs),
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
    /// List jobs.
    List(ListJobsArgs),
    /// Show job details.
    Get(JobGetArgs),
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
    /// Add a new cluster.
    Add(AddClusterArgs),
    /// Update cluster parameters.
    Set(AddClusterArgs),
}

#[derive(Args, Debug)]
pub struct ClusterGetArgs {
    pub hostid: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct LsArgs {
    pub hostid: String,
    pub path: Option<String>,
}

#[derive(Args, Debug)]
pub struct SubmitArgs {
    pub hostid: String,
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
#[command(
    group(
        ArgGroup::new("addcluster")
            .multiple(false)
            .args(&["destination", "hostname", "ip"])
    )
)]
pub struct AddClusterArgs {
    /// Destination in ssh format: [user@]host[:port]
    #[arg(value_name = "DESTINATION")]
    pub destination: Option<String>,

    #[arg(long, value_name = "HOSTNAME")]
    pub hostname: Option<String>,

    /// Use a remote IP address as input
    #[arg(long, value_name = "IP")]
    pub ip: Option<String>,

    #[arg(long)]
    pub username: Option<String>,

    #[arg(long)]
    pub hostid: Option<String>,

    /// Defaults to 22.
    #[arg(long)]
    pub port: Option<u32>,

    /// Defaults to ~/.ssh/id_ed25519.
    #[arg(long)]
    pub identity_path: Option<String>,

    #[arg(long)]
    pub default_base_path: Option<String>,

    /// Disable prompts; missing values must have defaults or the command will fail.
    #[arg(long)]
    pub headless: bool,
}
