// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use crate::errors::{format_server_error, format_status_error};
use crate::mfa::{clear_transient_mfa, collect_mfa_answers, collect_mfa_answers_transient};
use crate::stream::{
    MinDurationSpinner, Spinner, SubmitStreamOutcome, ensure_exit_code, handle_stream_events,
    handle_stream_events_with_progress, handle_submit_stream_events, print_with_green_check_stderr,
};
use anyhow::bail;
use proto::agent_client::AgentClient;
use proto::{
    AddClusterInit, AddClusterRequest, DeleteClusterRequest, DeleteClusterResponse,
    JobLogsRequest, JobLogsRequestInit, ListClustersRequest, ListClustersResponse, ListJobsRequest,
    ListJobsResponse, LsRequest, LsRequestInit, ResolveHomeDirRequest, ResolveHomeDirRequestInit,
    RetrieveJobRequest, RetrieveJobRequestInit, SubmitPathFilterRule, SubmitRequest,
    add_cluster_init, add_cluster_request, list_clusters_unit_response, resolve_home_dir_request,
    resolve_home_dir_request_init, stream_event,
};
use std::io::{IsTerminal, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Component, Path, PathBuf};
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tonic::{Request, transport::Channel};

pub async fn send_ping(client: &mut AgentClient<Channel>) -> anyhow::Result<()> {
    let ping_request = proto::PingRequest {
        message: "ping".into(),
    };
    let response = match timeout(Duration::from_secs(1), client.ping(ping_request)).await {
        Ok(res) => match res {
            Ok(response) => response,
            Err(status) => {
                bail!(format_status_error(&status));
            }
        },
        Err(elapsed) => bail!("Cancelled request after {elapsed} seconds"),
    };
    let message = response.get_ref().to_owned().message;
    match message.as_str() {
        "pong" => Ok(()),
        v => bail!("invalid response from server: expected 'pong', got '{v}'"),
    }
}

pub async fn fetch_list_clusters(
    client: &mut AgentClient<Channel>,
    filter: &str,
) -> anyhow::Result<ListClustersResponse> {
    let list_clusters_request = ListClustersRequest {
        filter: filter.to_string(),
    };
    let response = match timeout(
        Duration::from_secs(1),
        client.list_clusters(list_clusters_request),
    )
    .await
    {
        Ok(Ok(res)) => res.into_inner(),
        Ok(Err(status)) => {
            bail!(format_status_error(&status));
        }
        Err(e) => {
            bail!("operation timed out: {}", e)
        }
    };
    Ok(response)
}

pub async fn fetch_list_jobs(
    client: &mut AgentClient<Channel>,
    cluster: Option<String>,
) -> anyhow::Result<ListJobsResponse> {
    let list_jobs_request = ListJobsRequest { name: cluster };
    let response = match timeout(Duration::from_secs(5), client.list_jobs(list_jobs_request)).await
    {
        Ok(Ok(res)) => res.into_inner(),
        Ok(Err(status)) => {
            bail!(format_status_error(&status));
        }
        Err(e) => {
            bail!("operation timed out: {}", e)
        }
    };
    Ok(response)
}

pub async fn send_delete_cluster(
    client: &mut AgentClient<Channel>,
    name: &str,
) -> anyhow::Result<DeleteClusterResponse> {
    let delete_request = DeleteClusterRequest {
        name: name.to_string(),
    };
    let response = match timeout(
        Duration::from_secs(5),
        client.delete_cluster(delete_request),
    )
    .await
    {
        Ok(Ok(res)) => res.into_inner(),
        Ok(Err(status)) => {
            bail!(format_status_error(&status));
        }
        Err(e) => {
            bail!("operation timed out: {}", e)
        }
    };
    Ok(response)
}

async fn send_ls_request(
    client: &mut AgentClient<Channel>,
    name: String,
    job_id: Option<i64>,
    path: Option<String>,
) -> anyhow::Result<()> {
    let (tx_ans, rx_ans) = mpsc::channel::<LsRequest>(16);
    let outbound = ReceiverStream::new(rx_ans);
    tx_ans
        .send(LsRequest {
            msg: Some(proto::ls_request::Msg::Init(LsRequestInit {
                name,
                path,
                job_id,
            })),
        })
        .await?;

    let response = client
        .ls(Request::new(outbound))
        .await
        .map_err(|status| anyhow::Error::msg(format_status_error(&status)))?;
    let inbound = response.into_inner();
    let tx_mfa = tx_ans.clone();
    let exit_code = handle_stream_events(inbound, move |answers| {
        let tx_mfa = tx_mfa.clone();
        async move {
            tx_mfa
                .send(LsRequest {
                    msg: Some(proto::ls_request::Msg::Mfa(answers)),
                })
                .await
                .map_err(|_| anyhow::anyhow!("server closed while sending MFA answers"))
        }
    })
    .await?;
    ensure_exit_code(exit_code, "received exit code")
}

pub async fn send_ls(
    client: &mut AgentClient<Channel>,
    name: &str,
    path: &Option<String>,
) -> anyhow::Result<()> {
    send_ls_request(client, name.to_owned(), None, path.to_owned()).await
}

pub async fn send_job_ls(
    client: &mut AgentClient<Channel>,
    job_id: i64,
    path: &Option<String>,
    cluster: &Option<String>,
) -> anyhow::Result<()> {
    let name = cluster.clone().unwrap_or_default();
    send_ls_request(client, name, Some(job_id), path.to_owned()).await
}

pub async fn send_job_logs(
    client: &mut AgentClient<Channel>,
    job_id: i64,
    stderr: bool,
) -> anyhow::Result<i32> {
    let (tx_ans, rx_ans) = mpsc::channel::<JobLogsRequest>(16);
    let outbound = ReceiverStream::new(rx_ans);
    tx_ans
        .send(JobLogsRequest {
            msg: Some(proto::job_logs_request::Msg::Init(JobLogsRequestInit {
                job_id,
                stderr,
            })),
        })
        .await?;

    let response = client
        .job_logs(Request::new(outbound))
        .await
        .map_err(|status| anyhow::Error::msg(format_status_error(&status)))?;
    let mut inbound = response.into_inner();
    let tx_mfa = tx_ans.clone();
    let mut exit_code: Option<i32> = None;
    while let Some(item) = inbound.next().await {
        match item {
            Ok(proto::StreamEvent { event: Some(ev) }) => match ev {
                stream_event::Event::Stdout(bytes) => {
                    std::io::stdout().write_all(&bytes)?;
                }
                stream_event::Event::Stderr(bytes) => {
                    std::io::stderr().write_all(&bytes)?;
                }
                stream_event::Event::ExitCode(code) => {
                    exit_code = Some(code);
                    break;
                }
                stream_event::Event::Mfa(mfa) => {
                    let answers = collect_mfa_answers(&mfa).await?;
                    tx_mfa
                        .send(JobLogsRequest {
                            msg: Some(proto::job_logs_request::Msg::Mfa(answers)),
                        })
                        .await
                        .map_err(|_| anyhow::anyhow!("server closed while sending MFA answers"))?;
                }
                stream_event::Event::Error(err) => {
                    if err != "not_found" && err != "invalid_argument" {
                        eprintln!("{}", format_server_error(&err));
                    }
                    exit_code = Some(job_logs_error_exit_code(&err));
                    break;
                }
            },
            Ok(proto::StreamEvent { event: None }) => {}
            Err(status) => {
                eprintln!("{}", format_status_error(&status));
                exit_code = Some(1);
                break;
            }
        }
    }

    Ok(exit_code.unwrap_or(0))
}

fn job_logs_error_exit_code(err: &str) -> i32 {
    match err {
        "invalid_argument" => 2,
        "not_found" => 3,
        _ => 1,
    }
}

fn job_retrieve_error_exit_code(err: &str) -> i32 {
    match err {
        "invalid_argument" => 2,
        "not_found" => 3,
        "conflict" => 4,
        _ => 1,
    }
}

fn normalize_path(p: impl AsRef<Path>) -> PathBuf {
    let mut out = PathBuf::new();
    let p = p.as_ref();
    let mut comps = p.components().peekable();
    while let Some(c) = comps.peek() {
        match c {
            Component::Prefix(prefix) => {
                out.push(Path::new(prefix.as_os_str()));
                comps.next();
            }
            Component::RootDir => {
                out.push(Path::new(std::path::MAIN_SEPARATOR_STR));
                comps.next();
            }
            _ => break,
        }
    }

    for comp in comps {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                let popped = out.pop();
                if !popped || out.as_os_str().is_empty() {
                    out.push("..");
                }
            }
            Component::Normal(seg) => {
                out.push(seg);
            }
            Component::Prefix(_) | Component::RootDir => {}
        }
    }

    out
}

pub async fn send_job_retrieve(
    client: &mut AgentClient<Channel>,
    job_id: i64,
    path: &str,
    output: &Option<PathBuf>,
    overwrite: bool,
    headless: bool,
) -> anyhow::Result<i32> {
    let display_name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    let retrieve_message = format!("Retrieving {display_name}");
    let use_tty = std::io::stderr().is_terminal();
    let min_spinner_duration = Duration::from_millis(500);
    let mut local_base = match output {
        Some(v) => v.clone(),
        None => std::env::current_dir()?,
    };
    if !local_base.is_absolute() {
        local_base = std::env::current_dir()?.join(local_base);
    }
    let local_path = local_base.to_string_lossy().into_owned();
    let local_target = if Path::new(path).is_absolute() {
        let normalized = normalize_path(Path::new(path));
        match normalized.file_name() {
            Some(name) => local_base.join(name),
            None => local_base.clone(),
        }
    } else {
        local_base.join(Path::new(path))
    };

    let (tx_ans, rx_ans) = mpsc::channel::<RetrieveJobRequest>(16);
    let outbound = ReceiverStream::new(rx_ans);
    tx_ans
        .send(RetrieveJobRequest {
            msg: Some(proto::retrieve_job_request::Msg::Init(
                RetrieveJobRequestInit {
                    job_id,
                    path: path.to_owned(),
                    local_path: Some(local_path),
                    overwrite,
                },
            )),
        })
        .await?;

    let mut spinner = if !headless && use_tty {
        Some(MinDurationSpinner::start(
            &retrieve_message,
            min_spinner_duration,
        ))
    } else if !headless {
        eprintln!("{retrieve_message}");
        None
    } else {
        None
    };

    let response = match client.retrieve_job(Request::new(outbound)).await {
        Ok(response) => response,
        Err(status) => {
            if let Some(spinner) = spinner.take() {
                spinner.stop(None).await;
            }
            return Err(anyhow::Error::msg(format_status_error(&status)));
        }
    };
    let mut inbound = response.into_inner();
    let tx_mfa = tx_ans.clone();
    let mut exit_code: Option<i32> = None;
    while let Some(item) = inbound.next().await {
        match item {
            Ok(proto::StreamEvent { event: Some(ev) }) => match ev {
                stream_event::Event::Stdout(bytes) => {
                    if let Some(spinner) = spinner.take() {
                        spinner.stop(None).await;
                    }
                    std::io::stdout().write_all(&bytes)?;
                }
                stream_event::Event::Stderr(bytes) => {
                    if let Some(spinner) = spinner.take() {
                        spinner.stop(None).await;
                    }
                    std::io::stderr().write_all(&bytes)?;
                }
                stream_event::Event::ExitCode(code) => {
                    if let Some(spinner) = spinner.take() {
                        spinner.stop(None).await;
                    }
                    exit_code = Some(code);
                    break;
                }
                stream_event::Event::Mfa(mfa) => {
                    if let Some(spinner) = spinner.take() {
                        spinner.stop(None).await;
                    }
                    let answers = collect_mfa_answers(&mfa).await?;
                    tx_mfa
                        .send(RetrieveJobRequest {
                            msg: Some(proto::retrieve_job_request::Msg::Mfa(answers)),
                        })
                        .await
                        .map_err(|_| anyhow::anyhow!("server closed while sending MFA answers"))?;
                    if !headless && use_tty {
                        spinner = Some(MinDurationSpinner::start(
                            &retrieve_message,
                            min_spinner_duration,
                        ));
                    }
                }
                stream_event::Event::Error(err) => {
                    if let Some(spinner) = spinner.take() {
                        spinner.stop(None).await;
                    }
                    if err != "not_found" && err != "conflict" {
                        eprintln!("{}", format_server_error(&err));
                    }
                    exit_code = Some(job_retrieve_error_exit_code(&err));
                    break;
                }
            },
            Ok(proto::StreamEvent { event: None }) => {}
            Err(status) => {
                if let Some(spinner) = spinner.take() {
                    spinner.stop(None).await;
                }
                eprintln!("{}", format_status_error(&status));
                exit_code = Some(1);
                break;
            }
        }
    }

    if let Some(spinner) = spinner.take() {
        spinner.stop(None).await;
    }

    let code = exit_code.unwrap_or(0);
    if code == 0 {
        eprintln!("Wrote to {}", local_target.display());
    }
    Ok(code)
}

pub async fn send_submit(
    client: &mut AgentClient<Channel>,
    name: &str,
    local_path: &str,
    remote_path: &Option<String>,
    force_new_directory: bool,
    sbatchscript: &str,
    filters: &[SubmitPathFilterRule],
) -> anyhow::Result<()> {
    // outgoing stream client -> server with MFA answers
    let (tx_ans, rx_ans) = mpsc::channel::<SubmitRequest>(16);
    let outbound = ReceiverStream::new(rx_ans);
    tx_ans
        .send(SubmitRequest {
            msg: Some(proto::submit_request::Msg::Init(proto::SubmitRequestInit {
                local_path: local_path.to_owned(),
                remote_path: remote_path.to_owned(),
                name: name.to_owned(),
                sbatchscript: sbatchscript.to_owned(),
                filters: filters.to_vec(),
                force_new_directory,
            })),
        })
        .await?;
    // Start Submit RPC
    let response = client
        .submit(Request::new(outbound))
        .await
        .map_err(|status| anyhow::Error::msg(format_status_error(&status)))?;
    let inbound = response.into_inner();
    let tx_mfa = tx_ans.clone();
    let outcome = handle_submit_stream_events(inbound, move |answers| {
        let tx_mfa = tx_mfa.clone();
        async move {
            tx_mfa
                .send(SubmitRequest {
                    msg: Some(proto::submit_request::Msg::Mfa(answers)),
                })
                .await
                .map_err(|_| anyhow::anyhow!("server closed while sending MFA answers"))
        }
    })
    .await?;
    match outcome {
        SubmitStreamOutcome::Completed(exit_code) => {
            ensure_exit_code(exit_code, "on client side: received exit code")?;
            Ok(())
        }
        SubmitStreamOutcome::Canceled => bail!("submission canceled"),
    }
}

const CHECK_CONNECT_TIMEOUT_SECS: u64 = 3;

fn check_cluster_reachable(host: &str, port: u16) -> anyhow::Result<()> {
    let mut addrs = (host, port)
        .to_socket_addrs()
        .map_err(|_| anyhow::anyhow!("destination host could not be resolved"))?;
    let mut resolved = false;
    let timeout = std::time::Duration::from_secs(CHECK_CONNECT_TIMEOUT_SECS);
    while let Some(addr) = addrs.next() {
        resolved = true;
        if TcpStream::connect_timeout(&addr, timeout).is_ok() {
            return Ok(());
        }
    }
    if !resolved {
        bail!("destination host could not be resolved");
    }
    bail!("destination host is unreachable");
}

pub async fn validate_cluster_live(
    client: &mut AgentClient<Channel>,
    cluster: &proto::ListClustersUnitResponse,
) -> anyhow::Result<()> {
    let host = match cluster.host.as_ref() {
        Some(list_clusters_unit_response::Host::Hostname(value)) => value.as_str(),
        Some(list_clusters_unit_response::Host::Ipaddr(value)) => value.as_str(),
        None => bail!("cluster '{}' has no configured host", cluster.name),
    };
    let port = u16::try_from(cluster.port)
        .map_err(|_| anyhow::anyhow!("cluster '{}' has invalid port", cluster.name))?;
    let check_message = format!("Checking {}", cluster.name);
    let mut spinner = Some(Spinner::start(&check_message));

    if let Err(err) = check_cluster_reachable(host, port) {
        if let Some(spinner) = spinner.take() {
            spinner.stop(None).await;
        }
        return Err(err);
    }

    let (tx_ans, rx_ans) = mpsc::channel::<LsRequest>(16);
    let outbound = ReceiverStream::new(rx_ans);
    tx_ans
        .send(LsRequest {
            msg: Some(proto::ls_request::Msg::Init(LsRequestInit {
                name: cluster.name.clone(),
                path: None,
                job_id: None,
            })),
        })
        .await?;

    let response = client
        .ls(Request::new(outbound))
        .await
        .map_err(|status| anyhow::Error::msg(format_status_error(&status)))?;
    let mut inbound = response.into_inner();
    let tx_mfa = tx_ans.clone();
    let mut exit_code: Option<i32> = None;
    let mut stderr = Vec::new();
    while let Some(item) = inbound.next().await {
        match item {
            Ok(proto::StreamEvent { event: Some(ev) }) => match ev {
                stream_event::Event::Stdout(_) => {}
                stream_event::Event::Stderr(bytes) => {
                    stderr.extend_from_slice(&bytes);
                }
                stream_event::Event::ExitCode(code) => {
                    exit_code = Some(code);
                    break;
                }
                stream_event::Event::Mfa(mfa) => {
                    if let Some(spinner) = spinner.take() {
                        spinner.stop(None).await;
                    }
                    eprintln!("Connection required: ");
                    let (answers, lines) = collect_mfa_answers_transient(&mfa).await?;
                    tx_mfa
                        .send(LsRequest {
                            msg: Some(proto::ls_request::Msg::Mfa(answers)),
                        })
                        .await
                        .map_err(|_| anyhow::anyhow!("server closed while sending MFA answers"))?;
                    clear_transient_mfa(lines.saturating_add(1))?;
                    spinner = Some(Spinner::start(&check_message));
                }
                stream_event::Event::Error(err) => {
                    if let Some(spinner) = spinner.take() {
                        spinner.stop(None).await;
                    }
                    return Err(anyhow::anyhow!(format_server_error(&err)));
                }
            },
            Ok(proto::StreamEvent { event: None }) => {}
            Err(status) => {
                if let Some(spinner) = spinner.take() {
                    spinner.stop(None).await;
                }
                return Err(anyhow::anyhow!(format_status_error(&status)));
            }
        }
    }

    if let Some(spinner) = spinner.take() {
        spinner.stop(None).await;
    }

    match exit_code {
        Some(0) => {
            print_with_green_check_stderr(&format!("{} live", cluster.name))?;
            Ok(())
        }
        Some(code) => {
            let detail = if stderr.is_empty() {
                format!("exit code {code}")
            } else {
                String::from_utf8_lossy(&stderr).to_string()
            };
            bail!(
                "cluster '{}' did not respond to checks: {}",
                cluster.name,
                detail.trim()
            );
        }
        None => bail!("cluster '{}' did not respond to checks", cluster.name),
    }
}

pub async fn send_add_cluster(
    client: &mut AgentClient<Channel>,
    name: &str,
    username: &str,
    hostname: &Option<String>,
    ip: &Option<String>,
    identity_path: Option<&str>,
    port: u32,
    default_base_path: &Option<String>,
    show_progress: bool,
) -> anyhow::Result<()> {
    // outgoing stream client -> server with MFA answers
    let (tx_ans, rx_ans) = mpsc::channel::<AddClusterRequest>(16);
    let outbound = ReceiverStream::new(rx_ans);
    let host: add_cluster_init::Host = match hostname {
        Some(v) => add_cluster_init::Host::Hostname(v.into()),
        None => match ip {
            Some(v) => add_cluster_init::Host::Ipaddr(v.into()),
            None => anyhow::bail!("both hostname and ip address can't be none"),
        },
    };
    let identity_path_expanded = match identity_path {
        Some(value) => Some(shellexpand::full(value)?.to_string()),
        None => None,
    };
    let init = AddClusterInit {
        name: name.to_owned(),
        username: username.to_owned(),
        host: Some(host),
        identity_path: identity_path_expanded,
        port: port,
        default_base_path: default_base_path.to_owned(),
    };
    let acr = AddClusterRequest {
        msg: Some(add_cluster_request::Msg::Init(init)),
    };
    tx_ans.send(acr).await?;
    // Start AddCluster RPC
    let response = client
        .add_cluster(Request::new(outbound))
        .await
        .map_err(|status| anyhow::Error::msg(format_status_error(&status)))?;
    let inbound = response.into_inner();
    let exit_code = if show_progress {
        let tx_mfa = tx_ans.clone();
        handle_stream_events_with_progress(
            inbound,
            move |answers| {
                let tx_mfa = tx_mfa.clone();
                async move {
                    tx_mfa
                        .send(AddClusterRequest {
                            msg: Some(proto::add_cluster_request::Msg::Mfa(answers)),
                        })
                        .await
                        .map_err(|_| anyhow::anyhow!("server closed while sending MFA answers"))
                }
            },
            "Gathering cluster information",
            Duration::from_millis(500),
        )
        .await?
    } else {
        let tx_mfa = tx_ans.clone();
        handle_stream_events(inbound, move |answers| {
            let tx_mfa = tx_mfa.clone();
            async move {
                tx_mfa
                    .send(AddClusterRequest {
                        msg: Some(proto::add_cluster_request::Msg::Mfa(answers)),
                    })
                    .await
                    .map_err(|_| anyhow::anyhow!("server closed while sending MFA answers"))
            }
        })
        .await?
    };
    ensure_exit_code(exit_code, "on client side: received exit code")
}

pub async fn send_resolve_home_dir(
    client: &mut AgentClient<Channel>,
    name: &str,
    username: &str,
    hostname: &Option<String>,
    ip: &Option<String>,
    identity_path: Option<&str>,
    port: u32,
) -> anyhow::Result<String> {
    let (tx_ans, rx_ans) = mpsc::channel::<ResolveHomeDirRequest>(16);
    let outbound = ReceiverStream::new(rx_ans);
    let host: resolve_home_dir_request_init::Host = match hostname {
        Some(v) => resolve_home_dir_request_init::Host::Hostname(v.into()),
        None => match ip {
            Some(v) => resolve_home_dir_request_init::Host::Ipaddr(v.into()),
            None => anyhow::bail!("both hostname and ip address can't be none"),
        },
    };
    let identity_path_expanded = match identity_path {
        Some(value) => Some(shellexpand::full(value)?.to_string()),
        None => None,
    };
    let init = ResolveHomeDirRequestInit {
        username: username.to_owned(),
        host: Some(host),
        identity_path: identity_path_expanded,
        port,
        name: Some(name.to_owned()),
    };
    let req = ResolveHomeDirRequest {
        msg: Some(resolve_home_dir_request::Msg::Init(init)),
    };
    tx_ans.send(req).await?;
    let response = client
        .resolve_home_dir(Request::new(outbound))
        .await
        .map_err(|status| anyhow::Error::msg(format_status_error(&status)))?;
    let mut inbound = response.into_inner();
    let tx_mfa = tx_ans.clone();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code = None;
    let mut mfa_lines = 0usize;
    let mut saw_mfa = false;
    let mut connect_spinner: Option<MinDurationSpinner> = None;
    let min_spinner = Duration::from_millis(500);
    while let Some(item) = inbound.next().await {
        match item {
            Ok(event) => match event.event {
                Some(stream_event::Event::Stdout(bytes)) => {
                    if saw_mfa && connect_spinner.is_none() {
                        connect_spinner = Some(MinDurationSpinner::start(
                            "Connecting to cluster",
                            min_spinner,
                        ));
                    }
                    stdout.extend_from_slice(&bytes);
                }
                Some(stream_event::Event::Stderr(bytes)) => {
                    if saw_mfa && connect_spinner.is_none() {
                        connect_spinner = Some(MinDurationSpinner::start(
                            "Connecting to cluster",
                            min_spinner,
                        ));
                    }
                    stderr.extend_from_slice(&bytes);
                }
                Some(stream_event::Event::ExitCode(code)) => {
                    if saw_mfa && connect_spinner.is_none() {
                        connect_spinner = Some(MinDurationSpinner::start(
                            "Connecting to cluster",
                            min_spinner,
                        ));
                    }
                    exit_code = Some(code);
                    break;
                }
                Some(stream_event::Event::Mfa(mfa)) => {
                    saw_mfa = true;
                    let (answers, lines) = collect_mfa_answers_transient(&mfa).await?;
                    mfa_lines = mfa_lines.saturating_add(lines);
                    tx_mfa
                        .send(ResolveHomeDirRequest {
                            msg: Some(resolve_home_dir_request::Msg::Mfa(answers)),
                        })
                        .await
                        .map_err(|_| anyhow::anyhow!("server closed while sending MFA answers"))?;
                }
                Some(stream_event::Event::Error(err)) => {
                    if let Some(spinner) = connect_spinner.take() {
                        spinner.cancel().await;
                    }
                    bail!(format_server_error(&err));
                }
                None => {}
            },
            Err(status) => {
                if let Some(spinner) = connect_spinner.take() {
                    spinner.cancel().await;
                }
                bail!(format_status_error(&status));
            }
        }
    }

    if exit_code != Some(0) {
        if let Some(spinner) = connect_spinner.take() {
            spinner.cancel().await;
        }
        let detail = if stderr.is_empty() {
            "unknown error".to_string()
        } else {
            String::from_utf8_lossy(&stderr).to_string()
        };
        bail!(
            "failed to resolve remote home directory: {}",
            format_server_error(detail.trim())
        );
    }

    if let Some(spinner) = connect_spinner.take() {
        spinner.stop(None).await;
    }
    if saw_mfa {
        if mfa_lines > 0 {
            clear_transient_mfa(mfa_lines)?;
        }
        eprintln!("Connected to cluster");
    }

    let home_raw =
        String::from_utf8(stdout).map_err(|e| anyhow::anyhow!("invalid UTF-8: {e}"))?;
    let home = home_raw.trim();
    if home.is_empty() {
        bail!("remote home directory is empty");
    }
    Ok(home.to_string())
}
