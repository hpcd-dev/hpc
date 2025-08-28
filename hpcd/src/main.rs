use anyhow::Context;
use clap::Parser;
use proto::agent_server::{Agent, AgentServer};
use proto::{MfaAnswer, PingReply, PingRequest, StreamEvent, stream_event};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::{Stream, StreamExt};
use tonic::transport::Server;

use tonic::Status;
mod ssh;
use ssh::{SessionManager, SshParams};

use crate::ssh::receiver_to_stream;
#[derive(Parser)]
#[command(version,about,long_about = None)]
struct Opts {
    #[arg(short, long)]
    remote_server: String,

    #[arg(short, long)]
    username: String,

    #[arg(short, long)]
    identity_path: String,
}

type OutStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, Status>> + Send + Sync + 'static>>;

#[derive(Clone)]
struct AgentSvc {
    mgr: Arc<SessionManager>,
}

impl AgentSvc {
    async fn run_command(
        &self,
        command: &str,
        req: tonic::Request<tonic::Streaming<MfaAnswer>>,
    ) -> Result<tonic::Response<OutStream>, Status> {
        // Outbound stream server -> client
        let (evt_tx, evt_rx) = tokio::sync::mpsc::channel::<Result<StreamEvent, Status>>(64);

        // Setting up inbound stream (client -> server) carrying MFA answers from client to mfa_tx
        let mut inbound = req.into_inner();
        let (mfa_tx, mfa_rx) = tokio::sync::mpsc::channel::<MfaAnswer>(16);
        tokio::spawn(async move {
            while let Some(item) = inbound.next().await {
                match item {
                    Ok(ans) => {
                        if mfa_tx.send(ans).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        break;
                    }
                }
            }
        });

        // execute command over SSH
        let mgr = self.mgr.clone();
        let cmd = command.to_string();
        tokio::spawn(async move {
            if let Err(err) = mgr.exec_whitelisted(&cmd, evt_tx.clone(), mfa_rx).await {
                let _ = evt_tx
                    .send(Ok(StreamEvent {
                        event: Some(stream_event::Event::Error(err.to_string())),
                    }))
                    .await;
            }
        });
        let out: OutStream = Box::pin(receiver_to_stream(evt_rx));
        Ok(tonic::Response::new(out))
    }
}

#[tonic::async_trait]
impl Agent for AgentSvc {
    type LsStream = OutStream;
    async fn ping(
        &self,
        request: tonic::Request<PingRequest>,
    ) -> Result<tonic::Response<PingReply>, Status> {
        let req = request.into_inner();
        match req.message.trim() {
            "ping" => {
                return Ok(tonic::Response::new(PingReply {
                    message: "pong".into(),
                }));
            }
            m => {
                return Err(Status::invalid_argument(format!(
                    "expected message 'ping', got '{}'",
                    m
                )));
            }
        }
    }
    async fn ls(
        &self,
        request: tonic::Request<tonic::Streaming<MfaAnswer>>,
    ) -> Result<tonic::Response<Self::LsStream>, Status> {
        self.run_command("ls -l $HOME", request).await
    }
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = Opts::parse();
    let server_addr: SocketAddr = "127.0.0.1:50056".parse()?;
    let remote_server: SocketAddr = opts
        .remote_server
        .parse()
        .context("failed to parse remote_server address")?;

    let ssh_params = SshParams {
        addr: remote_server,
        username: opts.username,
        identity_path: Some(opts.identity_path),
        keepalive_secs: 60,
        ki_submethods: None,
    };
    let sm = Arc::new(SessionManager::new(ssh_params));
    let svc = AgentSvc { mgr: sm };
    println!("server listening on {}", server_addr);
    Server::builder()
        .add_service(AgentServer::new(svc))
        .serve(server_addr)
        .await?;
    Ok(())
}
