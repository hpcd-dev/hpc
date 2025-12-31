use clap::{CommandFactory, FromArgMatches, Parser};
use log::LevelFilter;
use proto::agent_server::AgentServer;
use std::net::SocketAddr;
use tokio::time::Duration;
use tonic::transport::Server;

mod agent;
mod ssh;
mod state;
mod util;

#[derive(Parser)]
#[command(version,about,long_about = None)]
struct Opts {
    #[arg(short, long)]
    database_path: String,
    #[arg(long, default_value_t = 5, value_name = "SECONDS")]
    job_check_interval_secs: u64,
}

const HELP_TEMPLATE: &str = r#"██╗  ██╗██████╗  ██████╗
██║  ██║██╔══██╗██╔════╝
███████║██████╔╝██║
██╔══██║██╔═══╝ ██║
██║  ██║██║     ╚██████╗
╚═╝  ╚═╝╚═╝      ╚═════╝

{before-help}{about-with-newline}{usage-heading} {usage}

{all-args}{after-help}
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
    let db = state::db::HostStore::open(opts.database_path).await?;
    let server_addr: SocketAddr = "127.0.0.1:50056".parse()?;

    let svc = agent::AgentSvc::new(db);
    svc.spawn_job_checker(Duration::from_secs(opts.job_check_interval_secs));
    println!("server listening on {}", server_addr);
    Server::builder()
        .add_service(AgentServer::new(svc))
        .serve(server_addr)
        .await?;
    Ok(())
}
