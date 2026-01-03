// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use crate::mfa::collect_mfa_answers_transient;
use crate::stream::{
    SubmitStreamOutcome, ensure_exit_code, handle_stream_events, handle_submit_stream_events,
};
use anyhow::bail;
use proto::agent_client::AgentClient;
use proto::{
    AddClusterInit, AddClusterRequest, DeleteClusterRequest, DeleteClusterResponse,
    ListClustersRequest, ListClustersResponse, ListJobsRequest, ListJobsResponse, LsRequest,
    LsRequestInit, ResolveHomeDirRequest, ResolveHomeDirRequestInit, RetrieveJobRequest,
    RetrieveJobRequestInit, SubmitPathFilterRule, SubmitRequest, add_cluster_init,
    add_cluster_request, resolve_home_dir_request, resolve_home_dir_request_init, stream_event,
};
use std::path::PathBuf;
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
            Err(status) => match status.code() {
                tonic::Code::InvalidArgument => {
                    bail!("invalid argument: {}", status.message());
                }
                tonic::Code::Cancelled => {
                    bail!("operation was canceled:{}", status.message());
                }
                _ => {
                    bail!("error occured: {}", status);
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
        Ok(Err(status)) => match status.code() {
            tonic::Code::InvalidArgument => {
                bail!("invalid argument: '{}'", status.message())
            }
            tonic::Code::Internal => {
                bail!("internal error: '{}'", status.message())
            }
            _ => {
                bail!(
                    "error encountered: {} - '{}'",
                    status.code(),
                    status.message()
                )
            }
        },
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
        Ok(Err(status)) => match status.code() {
            tonic::Code::InvalidArgument => {
                bail!("invalid argument: '{}'", status.message())
            }
            tonic::Code::Internal => {
                bail!("internal error: '{}'", status.message())
            }
            _ => {
                bail!(
                    "error encountered: {} - '{}'",
                    status.code(),
                    status.message()
                )
            }
        },
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
        Ok(Err(status)) => match status.code() {
            tonic::Code::InvalidArgument => {
                bail!("invalid argument: '{}'", status.message())
            }
            tonic::Code::Internal => {
                bail!("internal error: '{}'", status.message())
            }
            _ => {
                bail!(
                    "error encountered: {} - '{}'",
                    status.code(),
                    status.message()
                )
            }
        },
        Err(e) => {
            bail!("operation timed out: {}", e)
        }
    };
    Ok(response)
}

pub async fn send_ls(
    client: &mut AgentClient<Channel>,
    name: &str,
    path: &Option<String>,
) -> anyhow::Result<()> {
    let (tx_ans, rx_ans) = mpsc::channel::<LsRequest>(16);
    let outbound = ReceiverStream::new(rx_ans);
    tx_ans
        .send(LsRequest {
            msg: Some(proto::ls_request::Msg::Init(LsRequestInit {
                name: name.to_owned(),
                path: path.to_owned(),
            })),
        })
        .await?;

    let response = client.ls(Request::new(outbound)).await?;
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

pub async fn send_job_retrieve(
    client: &mut AgentClient<Channel>,
    job_id: i64,
    path: &str,
    dest: &Option<PathBuf>,
    cluster: &Option<String>,
) -> anyhow::Result<()> {
    let mut local_base = match dest {
        Some(v) => v.clone(),
        None => std::env::current_dir()?,
    };
    if !local_base.is_absolute() {
        local_base = std::env::current_dir()?.join(local_base);
    }
    let local_path = local_base.to_string_lossy().into_owned();

    let (tx_ans, rx_ans) = mpsc::channel::<RetrieveJobRequest>(16);
    let outbound = ReceiverStream::new(rx_ans);
    tx_ans
        .send(RetrieveJobRequest {
            msg: Some(proto::retrieve_job_request::Msg::Init(
                RetrieveJobRequestInit {
                    job_id,
                    name: cluster.to_owned(),
                    path: path.to_owned(),
                    local_path: Some(local_path),
                },
            )),
        })
        .await?;

    let response = client.retrieve_job(Request::new(outbound)).await?;
    let inbound = response.into_inner();
    let tx_mfa = tx_ans.clone();
    let exit_code = handle_stream_events(inbound, move |answers| {
        let tx_mfa = tx_mfa.clone();
        async move {
            tx_mfa
                .send(RetrieveJobRequest {
                    msg: Some(proto::retrieve_job_request::Msg::Mfa(answers)),
                })
                .await
                .map_err(|_| anyhow::anyhow!("server closed while sending MFA answers"))
        }
    })
    .await?;
    ensure_exit_code(exit_code, "received exit code")
}

pub async fn send_submit(
    client: &mut AgentClient<Channel>,
    name: &str,
    local_path: &str,
    remote_path: &Option<String>,
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
            })),
        })
        .await?;
    // Start Submit RPC
    let response = client.submit(Request::new(outbound)).await?;
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

pub async fn send_add_cluster(
    client: &mut AgentClient<Channel>,
    name: &str,
    username: &str,
    hostname: &Option<String>,
    ip: &Option<String>,
    identity_path: Option<&str>,
    port: u32,
    default_base_path: &Option<String>,
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
    let response = client.add_cluster(Request::new(outbound)).await?;
    let inbound = response.into_inner();
    let tx_mfa = tx_ans.clone();
    let exit_code = handle_stream_events(inbound, move |answers| {
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
    .await?;
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
    let response = client.resolve_home_dir(Request::new(outbound)).await?;
    let mut inbound = response.into_inner();
    let tx_mfa = tx_ans.clone();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code = None;
    while let Some(item) = inbound.next().await {
        match item {
            Ok(event) => match event.event {
                Some(stream_event::Event::Stdout(bytes)) => {
                    stdout.extend_from_slice(&bytes);
                }
                Some(stream_event::Event::Stderr(bytes)) => {
                    stderr.extend_from_slice(&bytes);
                }
                Some(stream_event::Event::ExitCode(code)) => {
                    exit_code = Some(code);
                    break;
                }
                Some(stream_event::Event::Mfa(mfa)) => {
                    let answers = collect_mfa_answers_transient(&mfa).await?;
                    tx_mfa
                        .send(ResolveHomeDirRequest {
                            msg: Some(resolve_home_dir_request::Msg::Mfa(answers)),
                        })
                        .await
                        .map_err(|_| anyhow::anyhow!("server closed while sending MFA answers"))?;
                }
                Some(stream_event::Event::Error(err)) => {
                    bail!("server error: {err}");
                }
                None => {}
            },
            Err(status) => {
                bail!("stream error: {status}");
            }
        }
    }

    if exit_code != Some(0) {
        let detail = if stderr.is_empty() {
            "unknown error".to_string()
        } else {
            String::from_utf8_lossy(&stderr).to_string()
        };
        bail!("failed to resolve remote home directory: {detail}");
    }

    let home_raw =
        String::from_utf8(stdout).map_err(|e| anyhow::anyhow!("invalid UTF-8: {e}"))?;
    let home = home_raw.trim();
    if home.is_empty() {
        bail!("remote home directory is empty");
    }
    Ok(home.to_string())
}
