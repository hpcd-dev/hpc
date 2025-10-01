use anyhow::bail;
use clap::{Parser, Subcommand};
use proto::agent_client::AgentClient;
use proto::{
    AddClusterInit, AddClusterRequest, MfaAnswer, PingReply, PingRequest, StreamEvent,
    SubmitRequest, SubmitRequestInit, add_cluster_request, stream_event,
};
use std::io::Write;
use tokio::io;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
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
    Submit {
        local_path: String,
        remote_path: String,
    },
    AddCluster {
        ClusterID: String,
        Username: String,
        Hostname: String,
    }, //    Status { job_id: String },
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
            Ok(StreamEvent { event: Some(ev) }) => match ev {
                stream_event::Event::Stdout(bytes) => {
                    write_all(&mut std::io::stdout(), &bytes);
                }
                stream_event::Event::Stderr(bytes) => {
                    write_all(&mut std::io::stderr(), &bytes)?;
                }
                stream_event::Event::ExitCode(code) => {
                    exit_code = Some(code);
                    break;
                }
                stream_event::Event::Mfa(mfa) => {
                    // Prompt for all answers in this round
                    eprintln!();
                    if !mfa.name.is_empty() {
                        eprintln!("MFA: {}", mfa.name);
                    }
                    if !mfa.instructions.is_empty() {
                        eprintln!("{}", mfa.instructions);
                    }

                    let mut responses = Vec::with_capacity(mfa.prompts.len());
                    for p in &mfa.prompts {
                        let ans = prompt_value(&p.text, p.echo).await?;
                        responses.push(ans);
                    }

                    // Send the answers back
                    if tx_ans.send(MfaAnswer { responses }).await.is_err() {
                        eprintln!("server closed while sending MFA answers");
                        break;
                    }
                }
                stream_event::Event::Error(err) => {
                    // "command not allowed" goes here
                    // "SSH connect failed also goes here"
                    eprintln!("server error: {err}");
                    exit_code = Some(1);
                    break;
                }
            },
            Ok(StreamEvent { event: None }) => log::info!("received empty event"),
            Err(status) => {
                eprintln!("stream error: {}", status);
                exit_code = Some(1);
                break;
            }
        }
    }
    match exit_code {
        Some(num) => {
            if num != 0 {
                bail!("received exit code {num}");
            }
            return Ok(());
        }
        None => {
            return Ok(());
        }
    }
}

async fn send_submit(
    client: &mut AgentClient<Channel>,
    local_path: &str,
    remote_path: &str,
) -> anyhow::Result<()> {
    // outgoing stream client -> server with MFA answers
    let (tx_ans, rx_ans) = mpsc::channel::<SubmitRequest>(16);
    let outbound = ReceiverStream::new(rx_ans);
    tx_ans
        .send(SubmitRequest {
            msg: Some(proto::submit_request::Msg::Init(proto::SubmitRequestInit {
                local_path: local_path.to_owned(),
                remote_path: remote_path.to_owned(),
                hostid: "gpurig".to_owned(),
            })),
        })
        .await?;
    // Start LS RPC
    let response = client.submit(Request::new(outbound)).await?;
    let mut inbound = response.into_inner();
    let mut exit_code: Option<i32> = None;
    while let Some(item) = inbound.next().await {
        match item {
            Ok(StreamEvent { event: Some(ev) }) => match ev {
                stream_event::Event::Stdout(bytes) => {
                    write_all(&mut std::io::stdout(), &bytes);
                }
                stream_event::Event::Stderr(bytes) => {
                    write_all(&mut std::io::stderr(), &bytes)?;
                }
                stream_event::Event::ExitCode(code) => {
                    exit_code = Some(code);
                    break;
                }
                stream_event::Event::Mfa(mfa) => {
                    // Prompt for all answers in this round
                    eprintln!();
                    if !mfa.name.is_empty() {
                        eprintln!("MFA: {}", mfa.name);
                    }
                    if !mfa.instructions.is_empty() {
                        eprintln!("{}", mfa.instructions);
                    }

                    let mut responses = Vec::with_capacity(mfa.prompts.len());
                    for p in &mfa.prompts {
                        let ans = prompt_value(&p.text, p.echo).await?;
                        responses.push(ans);
                    }

                    // Send the answers back
                    if tx_ans
                        .send(SubmitRequest {
                            msg: Some(proto::submit_request::Msg::Mfa(MfaAnswer { responses })),
                        })
                        .await
                        .is_err()
                    {
                        eprintln!("server closed while sending MFA answers");
                        break;
                    }
                }
                stream_event::Event::Error(err) => {
                    // "command not allowed" goes here
                    // "SSH connect failed also goes here"
                    eprintln!("server error: {err}");
                    exit_code = Some(1);
                    break;
                }
            },
            Ok(StreamEvent { event: None }) => log::info!("received empty event"),
            Err(status) => {
                eprintln!("stream error: {}", status);
                exit_code = Some(1);
                break;
            }
        }
    }
    match exit_code {
        Some(num) => {
            if num != 0 {
                bail!("on client side: received exit code {num}");
            }
            return Ok(());
        }
        None => {
            return Ok(());
        }
    }
}

async fn send_add_cluster(
    client: &mut AgentClient<Channel>,
    host_id: &str,
    username: &str,
    hostname: &str,
) -> anyhow::Result<()> {
    // outgoing stream client -> server with MFA answers
    let (tx_ans, rx_ans) = mpsc::channel::<AddClusterRequest>(16);
    let outbound = ReceiverStream::new(rx_ans);
    let init = AddClusterInit {
        hostid: host_id.to_owned(),
        hostname: hostname.to_owned(),
        username: username.to_owned(),
    };
    let acr = AddClusterRequest {
        msg: Some(add_cluster_request::Msg::Init(init)),
    };
    tx_ans.send(acr).await?;
    // Start LS RPC
    let response = client.add_cluster(Request::new(outbound)).await?;
    let mut inbound = response.into_inner();
    let mut exit_code: Option<i32> = None;
    // TODO: put MFA-handling logic in a separate function
    while let Some(item) = inbound.next().await {
        match item {
            Ok(StreamEvent { event: Some(ev) }) => match ev {
                stream_event::Event::Stdout(bytes) => {
                    write_all(&mut std::io::stdout(), &bytes);
                }
                stream_event::Event::Stderr(bytes) => {
                    write_all(&mut std::io::stderr(), &bytes)?;
                }
                stream_event::Event::ExitCode(code) => {
                    exit_code = Some(code);
                    break;
                }
                stream_event::Event::Mfa(mfa) => {
                    // Prompt for all answers in this round
                    eprintln!();
                    if !mfa.name.is_empty() {
                        eprintln!("MFA: {}", mfa.name);
                    }
                    if !mfa.instructions.is_empty() {
                        eprintln!("{}", mfa.instructions);
                    }

                    let mut responses = Vec::with_capacity(mfa.prompts.len());
                    for p in &mfa.prompts {
                        let ans = prompt_value(&p.text, p.echo).await?;
                        responses.push(ans);
                    }

                    // Send the answers back
                    if tx_ans
                        .send(AddClusterRequest {
                            msg: Some(proto::add_cluster_request::Msg::Mfa(MfaAnswer {
                                responses,
                            })),
                        })
                        .await
                        .is_err()
                    {
                        eprintln!("server closed while sending MFA answers");
                        break;
                    }
                }
                stream_event::Event::Error(err) => {
                    // "command not allowed" goes here
                    // "SSH connect failed also goes here"
                    eprintln!("server error: {err}");
                    exit_code = Some(1);
                    break;
                }
            },
            Ok(StreamEvent { event: None }) => log::info!("received empty event"),
            Err(status) => {
                eprintln!("stream error: {}", status);
                exit_code = Some(1);
                break;
            }
        }
    }
    match exit_code {
        Some(num) => {
            if num != 0 {
                bail!("on client side: received exit code {num}");
            }
            return Ok(());
        }
        None => {
            return Ok(());
        }
    }
}
fn write_all<W: Write>(w: &mut W, buf: &[u8]) -> anyhow::Result<()> {
    w.write_all(buf)?;
    w.flush()?;
    Ok(())
}

async fn prompt_value(prompt: &str, echo: bool) -> anyhow::Result<String> {
    let prompt = prompt.to_string();
    if echo {
        tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            print!("{}", prompt);
            std::io::stdout().flush()?;
            let mut s = String::new();
            std::io::stdin().read_line(&mut s)?;
            // Trim common line endings
            while s.ends_with('\n') || s.ends_with('\r') {
                s.pop();
            }
            Ok(s)
        })
        .await?
    } else {
        bail!("Method not supported")
        /*
        tokio::task::spawn_blocking(move || -> Result<String> {
            let s = rpassword::prompt_password(prompt)?;
            Ok(s)
        })
        .await?
        */
    }
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
        Cmd::Ls => send_ls(&mut client).await?,
        Cmd::Submit {
            local_path,
            remote_path,
        } => send_submit(&mut client, &local_path, &remote_path).await?,
        Cmd::AddCluster {
            ClusterID: cluster_id,
            Username: username,
            Hostname: hostname,
        } => send_add_cluster(&mut client, &cluster_id, &username, &hostname).await?,
    }
    Ok(())
}
