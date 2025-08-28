use tokio::sync::mpsc;

use anyhow::bail;
use clap::{Parser, Subcommand};
use proto::agent_client::AgentClient;
use proto::{MfaAnswer, PingReply, PingRequest, StreamEvent};
use tokio::time::{Duration, timeout};
use tokio_stream::{Stream, StreamExt, wrappers::ReceiverStream};
use tonic::{Request, Response, Status, transport::Channel};
use tonic_types::StatusExt;
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Ping,
    Ls,
    //    Submit { project: String, #[arg(last = true)] args: Vec<String> },
    //    Status { job_id: String },
    //    Logs   { job_id: String },
}

async fn send_ping(client: &mut AgentClient<Channel>) -> anyhow::Result<()> {
    let ping_request = PingRequest {
        message: "ping".into(),
    };
    let response = match timeout(Duration::from_secs(1), client.ping(ping_request)).await {
        Ok(res) => match res {
            Ok(response) => response,
            Err(status) => match status.code() {
                tonic::Code::InvalidArgument => {
                    bail!("invalid argument error - {}", status.message());
                }
                tonic::Code::Cancelled => {
                    bail!("operation was canceled - {}", status.message());
                }
                _ => {
                    bail!("occured: {}", status);
                }
            },
        },
        Err(elapsed) => bail!("Cancelled request after {elapsed} seconds"),
    };
    let message = response.get_ref().to_owned().message;
    match message.as_str() {
        "pong" => Ok(()),
        v => bail!("invalid response from server: expected 'pong', got '{v}'"),
    }
}

async fn send_ls(client: &mut AgentClient<Channel>) -> anyhow::Result<()> {
    // outgoing stream client -> server with MFA answers
    let (tx_ans, rx_ans) = mpsc::channel::<MfaAnswer>(16);
    let outbound = ReceiverStream::new(rx_ans);

    // Start LS RPC
    let response = client.ls(Request::new(outbound)).await?;
    let mut inbound = response.into_inner();
    let mut exit_code: Option<i32> = None;
    while let Some(item) = inbound.next().await {
        match item {
            Ok(StreamEvent { event: Some(ev) }) => {}
            Ok(StreamEvent { event: None }) => log::info!("received empty event"),
            Err(status) => {}
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut client = AgentClient::connect("http://127.0.0.1:50056").await?;
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Ping => match send_ping(&mut client).await {
            Ok(()) => println!("pong"),
            Err(e) => bail!(e),
        },
        Cmd::Ls => {}
    }
    Ok(())
}
