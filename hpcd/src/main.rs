use anyhow::Context;
use clap::Parser;
use log::LevelFilter;
use proto::agent_server::AgentServer;
use ssh::{SessionManager, SshParams};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_stream::{Stream, StreamExt};
use tonic::Status;
use tonic::transport::Server;

mod agent;
mod ssh;
mod state;
mod util;

#[derive(Parser)]
#[command(version,about,long_about = None)]
struct Opts {
    #[arg(short, long)]
    remote_server: String,

    #[arg(short, long)]
    username: String,

    #[arg(short, long)]
    identity_path: String,

    #[arg(short, long)]
    database_path: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::builder()
        .filter_level(LevelFilter::Debug)
        .init();

    let opts = Opts::parse();
    let db = state::db::HostStore::open(opts.database_path).await?;
    let server_addr: SocketAddr = "127.0.0.1:50056".parse()?;

    let svc = agent::AgentSvc::new(db);
    println!("server listening on {}", server_addr);
    Server::builder()
        .add_service(AgentServer::new(svc))
        .serve(server_addr)
        .await?;
    Ok(())
}
