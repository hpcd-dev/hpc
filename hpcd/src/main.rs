// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use clap::{CommandFactory, FromArgMatches, Parser};
use log::LevelFilter;
use proto::agent_server::AgentServer;
use std::{net::SocketAddr, path::PathBuf};
use tokio::time::Duration;
use tonic::transport::Server;

mod agent;
mod config;
mod ssh;
mod state;
mod util;

#[derive(Parser)]
#[command(
    version,
    about,
    long_about = None,
    after_help = "hpcd server\n\
\n\
Configuration precedence: defaults < config file < command-line flags.\n\
If --config is omitted, hpcd tries the default config file location; missing default config is OK.\n\
Paths in the config file are resolved relative to the config file directory; paths passed as flags are resolved relative to the current working directory."
)]
struct Opts {
    #[arg(
        short,
        long,
        value_name = "PATH",
        help = "Path to a TOML config file. When omitted, hpcd uses the default config file location if available."
    )]
    config: Option<PathBuf>,
    #[arg(
        long,
        value_name = "PATH",
        help = "Path to the SQLite database file. Overrides `database_path` from the config file."
    )]
    database_path: Option<PathBuf>,
    #[arg(
        long,
        value_name = "SECS",
        help = "How often to check remote job states. Overrides `job_check_interval_secs` from the config file."
    )]
    job_check_interval_secs: Option<u64>,
}

const HELP_TEMPLATE: &str = r#"██╗  ██╗██████╗  ██████╗
██║  ██║██╔══██╗██╔════╝
███████║██████╔╝██║
██╔══██║██╔═══╝ ██║
██║  ██║██║     ╚██████╗
╚═╝  ╚═╝╚═╝      ╚═════╝

{before-help}{about-with-newline}{usage-heading} {usage}
{after-help}

{all-args}
"#;

fn apply_help_template_recursively(cmd: &mut clap::Command) {
    let mut owned = std::mem::take(cmd);
    owned = owned.help_template(HELP_TEMPLATE);
    for sub in owned.get_subcommands_mut() {
        apply_help_template_recursively(sub);
    }
    *cmd = owned;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::builder()
        .filter_level(LevelFilter::Debug)
        .init();

    let mut cmd = Opts::command();
    apply_help_template_recursively(&mut cmd);
    let matches = cmd.get_matches();
    let opts = Opts::from_arg_matches(&matches).unwrap_or_else(|err| err.exit());
    let config = config::load(
        opts.config,
        config::Overrides {
            database_path: opts.database_path,
            job_check_interval_secs: opts.job_check_interval_secs,
        },
    )?;
    log::debug!("config path: {:?}", config.config_path);
    config::ensure_database_dir(&config.database_path)?;
    let db = state::db::HostStore::open(&config.database_path).await?;
    let server_addr: SocketAddr = "127.0.0.1:50056".parse()?;

    let svc = agent::AgentSvc::new(db);
    svc.spawn_job_checker(Duration::from_secs(config.job_check_interval_secs));
    println!("server listening on {}", server_addr);
    Server::builder()
        .add_service(AgentServer::new(svc))
        .serve(server_addr)
        .await?;
    Ok(())
}
