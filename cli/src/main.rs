use anyhow::bail;
use clap::{Parser, Subcommand};
use proto::agent_client::AgentClient;
use proto::{PingReply, PingRequest};
use tokio::time::{Duration, timeout};
use tonic::Response;
use tonic::Status;
use tonic::transport::Channel;
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut client = AgentClient::connect("http://127.0.0.1:50057").await?;
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Ping => match send_ping(&mut client).await {
            Ok(()) => println!("pong"),
            Err(e) => bail!(e),
        },
    }
    Ok(())
}
