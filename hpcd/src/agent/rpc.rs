// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use crate::agent::add_cluster::{
    map_net_error, normalize_default_base_path, parse_add_cluster_host, parse_add_cluster_port,
    resolve_host_addr,
};
use crate::agent::helpers::{
    build_sync_filters, db_host_record_to_api_unit_response, db_job_record_to_api_unit_response,
    get_default_base_path,
};
use crate::agent::error_codes;
use crate::agent::sbatch;
use crate::agent::service::AgentSvc;
use crate::agent::submit::{resolve_remote_sbatch_path, resolve_submit_remote_path};
use crate::agent::types::{AgentSvcError, OutStream, SubmitOutStream};
use crate::ssh::sh_escape;
use crate::state::db::{Address, HostStoreError};
use crate::util;
use crate::util::reachability;
use crate::util::remote_path::normalize_path;
use proto::agent_server::Agent;
use proto::{
    AddClusterRequest, DeleteClusterRequest, DeleteClusterResponse, ListClustersRequest,
    ListClustersResponse, ListClustersUnitResponse, ListJobsRequest, ListJobsResponse,
    JobLogsRequest, JobLogsRequestInit, LsRequest, LsRequestInit, MfaAnswer, PingReply,
    PingRequest, RetrieveJobRequest, RetrieveJobRequestInit, StreamEvent, SubmitRequest,
    SubmitResult, SubmitStatus, SubmitStreamEvent, stream_event,
    submit_result, submit_status, submit_stream_event,
};
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::StatusCode as SftpStatusCode;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tonic::Status;

async fn fetch_remote_home_dir(
    sm: &crate::ssh::SessionManager,
    name: &str,
) -> Result<String, Status> {
    let command = "printf '%s' \"$HOME\"";
    let (out, err, code) = sm.exec_capture(command).await.map_err(|e| {
        log::debug!("failed to resolve home directory on {name}: {e}");
        Status::aborted(error_codes::REMOTE_ERROR)
    })?;
    if code != 0 {
        let err_message = String::from_utf8_lossy(&err);
        let detail = if err_message.trim().is_empty() {
            format!("exit code {code}")
        } else {
            err_message.trim().to_string()
        };
        log::debug!("failed to resolve home directory on {name}: {detail}");
        return Err(Status::aborted(error_codes::REMOTE_ERROR));
    }
    let home_raw = String::from_utf8(out).map_err(|e| {
        log::debug!("failed to decode home directory for {name}: {e}");
        Status::aborted(error_codes::REMOTE_ERROR)
    })?;
    let home = home_raw.trim();
    if home.is_empty() {
        log::debug!("home directory is empty on {name}");
        return Err(Status::aborted(error_codes::REMOTE_ERROR));
    }
    if !Path::new(home).is_absolute() {
        log::debug!("home directory for {name} is not absolute: {home}");
        return Err(Status::aborted(error_codes::REMOTE_ERROR));
    }
    Ok(home.to_string())
}

fn resolve_default_base_path(
    default_base_path: Option<String>,
    home_dir: &str,
) -> Result<Option<String>, Status> {
    let default_base_path = default_base_path.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    });

    let Some(raw) = default_base_path else {
        let expanded = PathBuf::from(home_dir).join("runs");
        return Ok(Some(expanded.to_string_lossy().into_owned()));
    };

    if raw == "~" {
        return Ok(Some(home_dir.to_string()));
    }

    if let Some(suffix) = raw.strip_prefix("~/") {
        if suffix.is_empty() {
            return Ok(Some(home_dir.to_string()));
        }
        let expanded = PathBuf::from(home_dir).join(suffix);
        return Ok(Some(expanded.to_string_lossy().into_owned()));
    }

    if raw.starts_with('~') {
        log::debug!("default_base_path must be absolute or start with '~/' (use '~')");
        return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT));
    }

    Ok(Some(raw))
}

fn is_sftp_missing_path(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let Some(sftp_error) = cause.downcast_ref::<SftpError>() else {
            return false;
        };
        matches!(
            sftp_error,
            SftpError::Status(status) if status.status_code == SftpStatusCode::NoSuchFile
        )
    })
}

fn is_local_path_conflict(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let Some(io_err) = cause.downcast_ref::<std::io::Error>() else {
            return false;
        };
        io_err.kind() == std::io::ErrorKind::AlreadyExists
    })
}

async fn send_add_cluster_progress(
    evt_tx: &tokio::sync::mpsc::Sender<Result<StreamEvent, Status>>,
    message: &str,
) {
    let line = format!("✓ {message}\n");
    let _ = evt_tx
        .send(Ok(StreamEvent {
            event: Some(stream_event::Event::Stderr(line.into_bytes())),
        }))
        .await;
}

fn parse_resolve_home_host(
    host: Option<proto::resolve_home_dir_request_init::Host>,
) -> Result<Address, Status> {
    let host = match host {
        Some(v) => v,
        None => return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT)),
    };
    match host {
        proto::resolve_home_dir_request_init::Host::Hostname(v) => Ok(Address::Hostname(v)),
        proto::resolve_home_dir_request_init::Host::Ipaddr(addr) => {
            let ip: IpAddr = match addr.parse() {
                Ok(v) => v,
                Err(e) => {
                    log::debug!("could not parse ip address {}: {:?}", addr, e);
                    return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT));
                }
            };
            Ok(Address::Ip(ip))
        }
    }
}

fn format_remote_addr(addr: Option<SocketAddr>) -> String {
    addr.map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn format_address(addr: &Address) -> String {
    match addr {
        Address::Hostname(host) => host.clone(),
        Address::Ip(ip) => ip.to_string(),
    }
}

#[tonic::async_trait]
impl Agent for AgentSvc {
    type LsStream = OutStream;
    type RetrieveJobStream = OutStream;
    type JobLogsStream = OutStream;
    type SubmitStream = SubmitOutStream;
    type AddClusterStream = OutStream;
    type ResolveHomeDirStream = OutStream;

    async fn ping(
        &self,
        request: tonic::Request<PingRequest>,
    ) -> Result<tonic::Response<PingReply>, Status> {
        let remote_addr = format_remote_addr(request.remote_addr());
        let req = request.into_inner();
        match req.message.trim() {
            "ping" => {
                log::info!("ping remote_addr={remote_addr}");
                Ok(tonic::Response::new(PingReply {
                    message: "pong".into(),
                }))
            }
            m => {
                log::warn!(
                    "ping rejected remote_addr={remote_addr} message={m}"
                );
                Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT))
            }
        }
    }

    async fn resolve_home_dir(
        &self,
        request: tonic::Request<tonic::Streaming<proto::ResolveHomeDirRequest>>,
    ) -> Result<tonic::Response<Self::ResolveHomeDirStream>, Status> {
        let remote_addr = format_remote_addr(request.remote_addr());

        let mut inbound = request.into_inner();
        let init = inbound
            .message()
            .await
            .map_err(|e| {
                log::debug!("read error in resolve_home_dir: {e}");
                Status::unknown(error_codes::INTERNAL_ERROR)
            })?
            .ok_or_else(|| Status::invalid_argument(error_codes::INVALID_ARGUMENT))?;

        let (username, host, identity_path, port, name) = match init.msg {
            Some(proto::resolve_home_dir_request::Msg::Init(i)) => {
                (i.username, i.host, i.identity_path, i.port, i.name)
            }
            _ => {
                return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT));
            }
        };

        let addr = parse_resolve_home_host(host)?;
        let port = parse_add_cluster_port(port)?;
        let connection_addr = resolve_host_addr(&addr, port).await?;
        let session_name = name.and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
        let session_name_label = session_name.as_deref().unwrap_or("<none>");
        let host_label = format_address(&addr);
        log::info!(
            "resolve_home_dir start remote_addr={remote_addr} session_name={session_name_label} username={username} host={host_label} port={port}"
        );

        let ssh_params = crate::ssh::SshParams {
            username: username.clone(),
            addr: connection_addr,
            identity_path: identity_path.clone(),
            keepalive_secs: 60,
            ki_submethods: None,
        };

        let (evt_tx, evt_rx) = tokio::sync::mpsc::channel::<Result<StreamEvent, Status>>(64);
        let (mfa_tx, mut mfa_rx) = tokio::sync::mpsc::channel::<MfaAnswer>(16);
        tokio::spawn(async move {
            while let Ok(Some(item)) = inbound.message().await {
                if let Some(proto::resolve_home_dir_request::Msg::Mfa(ans)) = item.msg
                    && mfa_tx.send(ans).await.is_err()
                {
                    break;
                }
            }
        });

        let target = match &addr {
            Address::Hostname(host) => format!("{username}@{host}"),
            Address::Ip(host) => format!("{username}@{host}"),
        };
        let sessions = self.sessions();
        let audit_remote_addr = remote_addr.clone();
        let audit_target = target.clone();
        let audit_session_name = session_name.clone().unwrap_or_else(|| "<none>".to_string());
        tokio::spawn(async move {
            let sm = Arc::new(crate::ssh::SessionManager::new(ssh_params));
            if let Err(e) = sm.ensure_connected(&evt_tx, &mut mfa_rx).await {
                log::warn!(
                    "resolve_home_dir failed remote_addr={audit_remote_addr} target={audit_target} session_name={audit_session_name} error={e}"
                );
                log::debug!("failed to connect to {target}: {e}");
                let code = error_codes::code_for_ssh_error(&e);
                let _ = evt_tx
                    .send(Err(Status::aborted(code)))
                    .await;
                return;
            };

            let home = match fetch_remote_home_dir(&sm, &target).await {
                Ok(v) => v,
                Err(e) => {
                    log::warn!(
                        "resolve_home_dir failed remote_addr={audit_remote_addr} target={audit_target} session_name={audit_session_name} error_code={:?}",
                        e.code()
                    );
                    let _ = evt_tx.send(Err(e)).await;
                    return;
                }
            };
            if let Some(name) = session_name {
                sessions.insert(name, sm.clone()).await;
            }
            log::info!(
                "resolve_home_dir completed remote_addr={audit_remote_addr} target={audit_target} session_name={audit_session_name} home={home}"
            );
            let _ = evt_tx
                .send(Ok(StreamEvent {
                    event: Some(stream_event::Event::Stdout(home.into_bytes())),
                }))
                .await;
            let _ = evt_tx
                .send(Ok(StreamEvent {
                    event: Some(stream_event::Event::ExitCode(0)),
                }))
                .await;
        });

        let out: OutStream = Box::pin(crate::ssh::receiver_to_stream(evt_rx));
        Ok(tonic::Response::new(out))
    }

    async fn ls(
        &self,
        request: tonic::Request<tonic::Streaming<LsRequest>>,
    ) -> Result<tonic::Response<Self::LsStream>, Status> {
        let remote_addr = format_remote_addr(request.remote_addr());
        let mut inbound = request.into_inner();
        let init = inbound
            .message()
            .await
            .map_err(|e| {
                log::debug!("read error in ls: {e}");
                Status::unknown(error_codes::INTERNAL_ERROR)
            })?
            .ok_or_else(|| Status::invalid_argument(error_codes::INVALID_ARGUMENT))?;

        let (name, path, job_id) = match init.msg {
            Some(proto::ls_request::Msg::Init(LsRequestInit {
                name,
                path,
                job_id,
            })) => (name, path, job_id),
            _ => {
                return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT));
            }
        };
        let requested_path_label = path.as_deref().unwrap_or("<none>");
        let job_id_label = job_id
            .as_ref()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<none>".to_string());
        log::info!(
            "ls start remote_addr={remote_addr} name={name} job_id={job_id_label} requested_path={requested_path_label}"
        );

        let (mfa_tx, mfa_rx) = tokio::sync::mpsc::channel::<MfaAnswer>(16);
        tokio::spawn(async move {
            while let Ok(Some(item)) = inbound.message().await {
                if let Some(proto::ls_request::Msg::Mfa(ans)) = item.msg
                    && mfa_tx.send(ans).await.is_err()
                {
                    break;
                }
            }
        });

        let (name, list_path) = if let Some(job_id) = job_id {
            let job = match self.hosts().get_job_by_job_id(job_id).await {
                Ok(Some(v)) => v,
                Ok(None) => {
                    log::warn!(
                        "ls failed remote_addr={remote_addr} job_id={job_id_label} reason=job_not_found"
                    );
                    return Err(Status::invalid_argument(error_codes::NOT_FOUND));
                }
                Err(e) => {
                    log::warn!(
                        "ls failed remote_addr={remote_addr} job_id={job_id_label} reason=db_error"
                    );
                    log::debug!("could not fetch job id {job_id}: {e}");
                    return Err(Status::internal(error_codes::INTERNAL_ERROR));
                }
            };
            if !name.is_empty() && name != job.name {
                log::warn!(
                    "ls failed remote_addr={remote_addr} job_id={job_id_label} name={name} reason=name_mismatch"
                );
                return Err(Status::invalid_argument(error_codes::NOT_FOUND));
            }
            let (job_name, run_path) = (job.name, job.remote_path);
            let list_path = match path {
                Some(v) => {
                    if v.is_empty() {
                        log::warn!(
                            "ls failed remote_addr={remote_addr} job_id={job_id_label} reason=empty_path"
                        );
                        return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT));
                    }
                    if PathBuf::from(&v).is_absolute() {
                        normalize_path(v).to_string_lossy().into_owned()
                    } else {
                        util::remote_path::resolve_relative(&run_path, v)
                            .to_string_lossy()
                            .into_owned()
                    }
                }
                None => normalize_path(&run_path).to_string_lossy().into_owned(),
            };
            (job_name, list_path)
        } else {
            if name.is_empty() {
                log::warn!(
                    "ls failed remote_addr={remote_addr} reason=empty_name"
                );
                return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT));
            }
            let list_path = match path {
                Some(v) => {
                    if v.is_empty() {
                        log::warn!(
                            "ls failed remote_addr={remote_addr} name={name} reason=empty_path"
                        );
                        return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT));
                    }
                    if PathBuf::from(&v).is_absolute() {
                        normalize_path(v).to_string_lossy().into_owned()
                    } else {
                        let default_base_path =
                            get_default_base_path(self.hosts().as_ref(), &name).await?;
                        let base_path = PathBuf::from(default_base_path);
                        util::remote_path::resolve_relative(base_path, v)
                            .to_string_lossy()
                            .into_owned()
                    }
                }
                None => normalize_path(get_default_base_path(self.hosts().as_ref(), &name).await?)
                    .to_string_lossy()
                    .into_owned(),
            };
            (name, list_path)
        };

        log::info!(
            "ls resolved remote_addr={remote_addr} name={name} list_path={list_path}"
        );
        let command = format!("ls -- {}", sh_escape(&list_path));
        self.run_command(command, &name, mfa_rx).await
    }

    async fn retrieve_job(
        &self,
        request: tonic::Request<tonic::Streaming<RetrieveJobRequest>>,
    ) -> Result<tonic::Response<Self::RetrieveJobStream>, Status> {
        let remote_addr = format_remote_addr(request.remote_addr());
        let mut inbound = request.into_inner();

        let init = inbound
            .message()
            .await
            .map_err(|e| {
                log::debug!("read error in retrieve_job: {e}");
                Status::unknown(error_codes::INTERNAL_ERROR)
            })?
            .ok_or_else(|| Status::invalid_argument(error_codes::INVALID_ARGUMENT))?;

        let (job_id, path, local_path, overwrite) = match init.msg {
            Some(proto::retrieve_job_request::Msg::Init(RetrieveJobRequestInit {
                job_id,
                path,
                local_path,
                overwrite,
            })) => (job_id, path, local_path, overwrite),
            _ => {
                return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT));
            }
        };
        let local_path_label = local_path.as_deref().unwrap_or("<none>");
        log::info!(
            "retrieve_job start remote_addr={remote_addr} job_id={job_id} path={path} local_path={local_path_label}"
        );

        if path.trim().is_empty() {
            log::warn!(
                "retrieve_job failed remote_addr={remote_addr} job_id={job_id} reason=empty_path"
            );
            return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT));
        }
        let local_path = match local_path {
            Some(v) if !v.trim().is_empty() => v,
            _ => {
                log::warn!(
                    "retrieve_job failed remote_addr={remote_addr} job_id={job_id} reason=empty_local_path"
                );
                return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT));
            }
        };

        let (mfa_tx, mut mfa_rx) = tokio::sync::mpsc::channel::<MfaAnswer>(16);
        tokio::spawn(async move {
            while let Ok(Some(item)) = inbound.message().await {
                if let Some(proto::retrieve_job_request::Msg::Mfa(ans)) = item.msg
                    && mfa_tx.send(ans).await.is_err()
                {
                    break;
                }
            }
        });

        let (evt_tx, evt_rx) = tokio::sync::mpsc::channel::<Result<StreamEvent, Status>>(64);
        let hs = self.hosts();
        let path_is_absolute = PathBuf::from(&path).is_absolute();
        let svc = self.clone();
        let audit_remote_addr = remote_addr.clone();

        tokio::spawn(async move {
            let (name, run_path) = {
                let job = match hs.get_job_by_job_id(job_id).await {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        log::warn!(
                            "retrieve_job failed remote_addr={audit_remote_addr} job_id={job_id} reason=job_not_found"
                        );
                        let _ = evt_tx
                            .send(Ok(StreamEvent {
                                event: Some(stream_event::Event::Error(
                                    error_codes::NOT_FOUND.to_string(),
                                )),
                            }))
                            .await;
                        return;
                    }
                    Err(e) => {
                        log::warn!(
                            "retrieve_job failed remote_addr={audit_remote_addr} job_id={job_id} reason=db_error"
                        );
                        log::debug!("could not fetch job id {job_id}: {e}");
                        let _ = evt_tx
                            .send(Ok(StreamEvent {
                                event: Some(stream_event::Event::Error(
                                    error_codes::INTERNAL_ERROR.to_string(),
                                )),
                            }))
                            .await;
                        return;
                    }
                };
                (job.name.clone(), job.remote_path.clone())
            };

            let mgr = match svc.get_sessionmanager(&name).await {
                Ok(v) => v,
                Err(e) => {
                    log::warn!(
                        "retrieve_job failed remote_addr={audit_remote_addr} job_id={job_id} name={name} reason=session_unavailable"
                    );
                    let message = match e {
                        AgentSvcError::UnknownName => error_codes::NOT_FOUND,
                        AgentSvcError::NetworkError(e) => {
                            log::debug!("network error for {name}: {e}");
                            error_codes::NETWORK_ERROR
                        }
                        other_error => {
                            log::debug!("unexpected session manager error for {name}: {other_error}");
                            error_codes::INTERNAL_ERROR
                        }
                    };
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Error(message.to_string())),
                        }))
                        .await;
                    return;
                }
            };

            if let Err(err) = mgr.ensure_connected(&evt_tx, &mut mfa_rx).await {
                log::warn!(
                    "retrieve_job failed remote_addr={audit_remote_addr} job_id={job_id} name={name} reason=connect_failed error={err}"
                );
                log::debug!("failed to connect for retrieve job on {name}: {err}");
                let _ = evt_tx
                    .send(Ok(StreamEvent {
                        event: Some(stream_event::Event::Error(
                            error_codes::code_for_ssh_error(&err).to_string(),
                        )),
                    }))
                    .await;
                return;
            }

            let remote_path = if path_is_absolute {
                normalize_path(&path).to_string_lossy().into_owned()
            } else {
                util::remote_path::resolve_relative(&run_path, &path)
                    .to_string_lossy()
                    .into_owned()
            };

            let mut local_base = PathBuf::from(local_path);
            if !local_base.is_absolute() {
                match std::env::current_dir() {
                    Ok(cwd) => local_base = cwd.join(local_base),
                    Err(e) => {
                        log::warn!(
                            "retrieve_job failed remote_addr={audit_remote_addr} job_id={job_id} name={name} reason=local_path_resolve_failed error={e}"
                        );
                        log::debug!("could not resolve local destination: {e}");
                        let _ = evt_tx
                            .send(Ok(StreamEvent {
                                event: Some(stream_event::Event::Error(
                                    error_codes::LOCAL_ERROR.to_string(),
                                )),
                            }))
                            .await;
                        return;
                    }
                }
            }

            let local_target = if path_is_absolute {
                let Some(name) = std::path::Path::new(&remote_path).file_name() else {
                    log::warn!(
                        "retrieve_job failed remote_addr={audit_remote_addr} job_id={job_id} name={name} reason=invalid_remote_path"
                    );
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Error(
                                error_codes::INVALID_ARGUMENT.to_string(),
                            )),
                        }))
                        .await;
                    return;
                };
                local_base.join(name)
            } else {
                local_base.join(std::path::Path::new(&path))
            };

            if let Err(err) = mgr
                .retrieve_path(&remote_path, &local_target, overwrite)
                .await
            {
                if is_sftp_missing_path(&err) {
                    log::warn!(
                        "retrieve_job failed remote_addr={audit_remote_addr} job_id={job_id} name={name} reason=remote_path_missing error={err}"
                    );
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Stderr(
                                b"No such file or directory\n".to_vec(),
                            )),
                        }))
                        .await;
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Error(
                                error_codes::NOT_FOUND.to_string(),
                            )),
                        }))
                        .await;
                    return;
                }
                if is_local_path_conflict(&err) {
                    log::warn!(
                        "retrieve_job failed remote_addr={audit_remote_addr} job_id={job_id} name={name} reason=local_path_exists error={err}"
                    );
                    let message = format!("{err}; use --overwrite to replace it.");
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Stderr(
                                format!("{message}\n").into_bytes(),
                            )),
                        }))
                        .await;
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Error(
                                error_codes::CONFLICT.to_string(),
                            )),
                        }))
                        .await;
                    return;
                }
                log::warn!(
                    "retrieve_job failed remote_addr={audit_remote_addr} job_id={job_id} name={name} reason=remote_retrieve_failed error={err}"
                );
                log::debug!("retrieve path failed: {err}");
                let _ = evt_tx
                    .send(Ok(StreamEvent {
                        event: Some(stream_event::Event::Error(
                            error_codes::REMOTE_ERROR.to_string(),
                        )),
                    }))
                    .await;
                return;
            }

            log::info!(
                "retrieve_job completed remote_addr={audit_remote_addr} job_id={job_id} name={name} remote_path={remote_path} local_target={}",
                local_target.to_string_lossy()
            );
            let _ = evt_tx
                .send(Ok(StreamEvent {
                    event: Some(stream_event::Event::ExitCode(0)),
                }))
                .await;
        });

        let out: OutStream = Box::pin(crate::ssh::receiver_to_stream(evt_rx));
        Ok(tonic::Response::new(out))
    }

    async fn job_logs(
        &self,
        request: tonic::Request<tonic::Streaming<JobLogsRequest>>,
    ) -> Result<tonic::Response<Self::JobLogsStream>, Status> {
        let remote_addr = format_remote_addr(request.remote_addr());
        let mut inbound = request.into_inner();

        let init = inbound
            .message()
            .await
            .map_err(|e| {
                log::debug!("read error in job_logs: {e}");
                Status::unknown(error_codes::INTERNAL_ERROR)
            })?
            .ok_or_else(|| Status::invalid_argument(error_codes::INVALID_ARGUMENT))?;

        let (job_id, stderr) = match init.msg {
            Some(proto::job_logs_request::Msg::Init(JobLogsRequestInit { job_id, stderr })) => {
                (job_id, stderr)
            }
            _ => {
                return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT));
            }
        };
        log::info!(
            "job_logs start remote_addr={remote_addr} job_id={job_id} stderr={stderr}"
        );

        let (mfa_tx, mut mfa_rx) = tokio::sync::mpsc::channel::<MfaAnswer>(16);
        tokio::spawn(async move {
            while let Ok(Some(item)) = inbound.message().await {
                if let Some(proto::job_logs_request::Msg::Mfa(ans)) = item.msg
                    && mfa_tx.send(ans).await.is_err()
                {
                    break;
                }
            }
        });

        let (evt_tx, evt_rx) = tokio::sync::mpsc::channel::<Result<StreamEvent, Status>>(64);
        let hs = self.hosts();
        let svc = self.clone();
        let audit_remote_addr = remote_addr.clone();
        tokio::spawn(async move {
            let job = match hs.get_job_by_job_id(job_id).await {
                Ok(Some(v)) => v,
                Ok(None) => {
                    log::warn!(
                        "job_logs failed remote_addr={audit_remote_addr} job_id={job_id} reason=job_not_found"
                    );
                    let message = format!(
                        "job {} does not exist; you can list all job with 'hpc job list'",
                        job_id
                    );
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Stderr(
                                format!("{message}\n").into_bytes(),
                            )),
                        }))
                        .await;
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Error(
                                error_codes::NOT_FOUND.to_string(),
                            )),
                        }))
                        .await;
                    return;
                }
                Err(e) => {
                    log::warn!(
                        "job_logs failed remote_addr={audit_remote_addr} job_id={job_id} reason=db_error"
                    );
                    log::debug!("could not fetch job id {job_id}: {e}");
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Error(
                                error_codes::INTERNAL_ERROR.to_string(),
                            )),
                        }))
                        .await;
                    return;
                }
            };

            let log_path = if stderr {
                match job.stderr_path.clone() {
                    Some(path) if !path.trim().is_empty() => path,
                    _ => {
                        let message = format!(
                            "stderr log file is not configured for job {}",
                            job_id
                        );
                        let _ = evt_tx
                            .send(Ok(StreamEvent {
                                event: Some(stream_event::Event::Stderr(
                                    format!("{message}\n").into_bytes(),
                                )),
                            }))
                            .await;
                        let _ = evt_tx
                            .send(Ok(StreamEvent {
                                event: Some(stream_event::Event::Error(
                                    error_codes::INVALID_ARGUMENT.to_string(),
                                )),
                            }))
                            .await;
                        return;
                    }
                }
            } else {
                if job.stdout_path.trim().is_empty() {
                    let message =
                        format!("stdout log file is not configured for job {}", job_id);
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Stderr(
                                format!("{message}\n").into_bytes(),
                            )),
                        }))
                        .await;
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Error(
                                error_codes::NOT_FOUND.to_string(),
                            )),
                        }))
                        .await;
                    return;
                }
                job.stdout_path.clone()
            };

            let mgr = match svc.get_sessionmanager(&job.name).await {
                Ok(v) => v,
                Err(e) => {
                    log::warn!(
                        "job_logs failed remote_addr={audit_remote_addr} job_id={job_id} name={} reason=session_unavailable",
                        job.name
                    );
                    let message = match e {
                        AgentSvcError::UnknownName => error_codes::NOT_FOUND,
                        AgentSvcError::NetworkError(e) => {
                            log::debug!("network error for {}: {e}", job.name);
                            error_codes::NETWORK_ERROR
                        }
                        other_error => {
                            log::debug!(
                                "unexpected session manager error for {}: {other_error}",
                                job.name
                            );
                            error_codes::INTERNAL_ERROR
                        }
                    };
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Error(message.to_string())),
                        }))
                        .await;
                    return;
                }
            };

            if let Err(err) = mgr.ensure_connected(&evt_tx, &mut mfa_rx).await {
                log::warn!(
                    "job_logs failed remote_addr={audit_remote_addr} job_id={job_id} name={} reason=connect_failed error={err}",
                    job.name
                );
                log::debug!("failed to connect for job logs on {}: {err}", job.name);
                let _ = evt_tx
                    .send(Ok(StreamEvent {
                        event: Some(stream_event::Event::Error(
                            error_codes::code_for_ssh_error(&err).to_string(),
                        )),
                    }))
                    .await;
                return;
            }

            let escaped = sh_escape(&log_path);
            let test_cmd = format!("test -f {}", escaped);
            match mgr.exec_capture(&test_cmd).await {
                Ok((_out, _err, code)) if code == 0 => {}
                Ok((_out, _err, _code)) => {
                    let message = if stderr {
                        format!("specified error file {} wasn't found", log_path)
                    } else {
                        format!("stdout log file does not exist at {}", log_path)
                    };
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Stderr(
                                format!("{message}\n").into_bytes(),
                            )),
                        }))
                        .await;
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Error(
                                error_codes::NOT_FOUND.to_string(),
                            )),
                        }))
                        .await;
                    return;
                }
                Err(e) => {
                    log::warn!(
                        "job_logs failed remote_addr={audit_remote_addr} job_id={job_id} name={} reason=remote_check_failed error={e}",
                        job.name
                    );
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Error(
                                error_codes::REMOTE_ERROR.to_string(),
                            )),
                        }))
                        .await;
                    return;
                }
            }

            let command = format!("cat -- {}", escaped);
            if let Err(err) = mgr.exec(&command, evt_tx.clone(), mfa_rx).await {
                log::warn!(
                    "job_logs failed remote_addr={audit_remote_addr} job_id={job_id} name={} reason=exec_failed error={err}",
                    job.name
                );
                let _ = evt_tx
                    .send(Ok(StreamEvent {
                        event: Some(stream_event::Event::Error(
                            error_codes::REMOTE_ERROR.to_string(),
                        )),
                    }))
                    .await;
            }
        });

        let out: OutStream = Box::pin(crate::ssh::receiver_to_stream(evt_rx));
        Ok(tonic::Response::new(out))
    }

    async fn list_clusters(
        &self,
        request: tonic::Request<ListClustersRequest>,
    ) -> Result<tonic::Response<ListClustersResponse>, Status> {
        let remote_addr = format_remote_addr(request.remote_addr());
        let _inbound = request.into_inner();
        let hosts = match self.hosts().list_hosts(None).await {
            Ok(v) => v,
            Err(e) => match e {
                HostStoreError::Sqlx(sql_err) => {
                    log::error!("unknown error at sqlx level: {sql_err}");
                    return Err(Status::internal(error_codes::INTERNAL_ERROR));
                }
                other_error => {
                    log::error!("internal error: unexpected error: {other_error}");
                    return Err(Status::internal(error_codes::INTERNAL_ERROR));
                }
            },
        };
        let mut clusters: Vec<ListClustersUnitResponse> = hosts
            .iter()
            .map(db_host_record_to_api_unit_response)
            .collect();

        for (cluster, host) in clusters.iter_mut().zip(hosts.iter()) {
            let reachable = match reachability::check_host_reachable(&host.address, host.port).await
            {
                Ok(value) => value,
                Err(err) => {
                    log::debug!(
                        "reachability check failed name={} host={} error={err}",
                        host.name,
                        format_address(&host.address)
                    );
                    false
                }
            };
            cluster.reachable = reachable;
            if reachable && self.sessions().is_connected(&cluster.name).await {
                cluster.connected = true;
            } else {
                cluster.connected = false;
            }
        }
        let connected_count = clusters.iter().filter(|cluster| cluster.connected).count();
        log::info!(
            "list_clusters remote_addr={remote_addr} count={} connected={connected_count}",
            clusters.len()
        );

        Ok(ListClustersResponse { clusters }.into())
    }

    async fn delete_cluster(
        &self,
        request: tonic::Request<DeleteClusterRequest>,
    ) -> Result<tonic::Response<DeleteClusterResponse>, Status> {
        let remote_addr = format_remote_addr(request.remote_addr());
        let name = request.into_inner().name.trim().to_string();
        if name.is_empty() {
            log::warn!(
                "delete_cluster failed remote_addr={remote_addr} reason=empty_name"
            );
            return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT));
        }
        log::info!("delete_cluster start remote_addr={remote_addr} name={name}");

        let deleted = match self.hosts().delete_by_name(&name).await {
            Ok(value) => value,
            Err(e) => match e {
                HostStoreError::Sqlx(sql_err) => {
                    log::error!("sqlx error deleting cluster '{}': {sql_err}", name);
                    return Err(Status::internal(error_codes::INTERNAL_ERROR));
                }
                other_error => {
                    log::error!(
                        "unexpected error deleting cluster '{}': {other_error}",
                        name
                    );
                    return Err(Status::internal(error_codes::INTERNAL_ERROR));
                }
            },
        };

        if deleted == 0 {
            log::warn!(
                "delete_cluster failed remote_addr={remote_addr} name={name} reason=not_found"
            );
            return Err(Status::invalid_argument(error_codes::NOT_FOUND));
        }

        self.sessions().remove_and_shutdown(&name).await;
        log::info!(
            "delete_cluster completed remote_addr={remote_addr} name={name}"
        );

        Ok(tonic::Response::new(DeleteClusterResponse {
            deleted: true,
        }))
    }

    async fn submit(
        &self,
        request: tonic::Request<tonic::Streaming<SubmitRequest>>,
    ) -> Result<tonic::Response<Self::SubmitStream>, Status> {
        let remote_addr = format_remote_addr(request.remote_addr());
        let mut inbound = request.into_inner();
        let init = inbound
            .message()
            .await
            .map_err(|e| {
                log::debug!("read error in submit: {e}");
                Status::unknown(error_codes::INTERNAL_ERROR)
            })?
            .ok_or_else(|| Status::invalid_argument(error_codes::INVALID_ARGUMENT))?;

        let (local_path, remote_path, name, sbatchscript, filters, force_new_directory) =
            match init.msg {
                Some(proto::submit_request::Msg::Init(i)) => (
                    i.local_path,
                    i.remote_path,
                    i.name,
                    i.sbatchscript,
                    i.filters,
                    i.force_new_directory,
                ),
            _ => return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT)),
        };
        let requested_remote_path = remote_path.as_deref().unwrap_or("<default>");
        log::info!(
            "submit start remote_addr={remote_addr} name={name} local_path={local_path} requested_remote_path={requested_remote_path} sbatch={sbatchscript}"
        );
        let filters = match build_sync_filters(filters) {
            Ok(value) => value,
            Err(e) => {
                log::warn!(
                    "submit failed remote_addr={remote_addr} name={name} reason=invalid_filters"
                );
                return Err(e);
            }
        };

        let (evt_tx, evt_rx) = tokio::sync::mpsc::channel::<Result<SubmitStreamEvent, Status>>(64);
        let (mfa_tx, mut mfa_rx) = tokio::sync::mpsc::channel::<MfaAnswer>(16);
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

        tokio::spawn(async move {
            while let Ok(Some(item)) = inbound.message().await {
                if let Some(proto::submit_request::Msg::Mfa(ans)) = item.msg
                    && mfa_tx.send(ans).await.is_err()
                {
                    break;
                }
            }
            let _ = cancel_tx.send(true);
        });

        let mgr = match self.get_sessionmanager(&name).await {
            Ok(v) => v,
            Err(e) => match e {
                AgentSvcError::UnknownName => {
                    log::warn!(
                        "submit failed remote_addr={remote_addr} name={name} reason=unknown_cluster"
                    );
                    return Err(Status::invalid_argument(error_codes::NOT_FOUND));
                }
                AgentSvcError::NetworkError(e) => {
                    log::warn!(
                        "submit failed remote_addr={remote_addr} name={name} reason=network_error error={e}"
                    );
                    log::debug!("network error resolving session for submit: {e}");
                    return Err(Status::internal(error_codes::NETWORK_ERROR));
                }
                other_error => {
                    log::warn!(
                        "submit failed remote_addr={remote_addr} name={name} reason=session_error error={other_error}"
                    );
                    log::debug!("unexpected error resolving session for submit: {other_error}");
                    return Err(Status::internal(error_codes::INTERNAL_ERROR));
                }
            },
        };

        let hs = self.hosts();
        let reuse_remote_path = if remote_path.is_none() && !force_new_directory {
            match hs
                .latest_remote_path_for_local_path(&name, &local_path)
                .await
            {
                Ok(value) => value,
                Err(e) => {
                    log::warn!(
                        "submit failed remote_addr={remote_addr} name={name} reason=job_lookup_failed error={e}"
                    );
                    return Err(Status::internal(error_codes::INTERNAL_ERROR));
                }
            }
        } else {
            None
        };
        let (remote_path, allow_existing_remote_path) = match reuse_remote_path {
            Some(value) => (value, true),
            None => {
                let resolved = match remote_path.as_deref() {
                    Some(v) if PathBuf::from(v).is_absolute() => {
                        match resolve_submit_remote_path(Some(v), v, "") {
                            Ok(value) => value,
                            Err(e) => {
                                log::warn!(
                                    "submit failed remote_addr={remote_addr} name={name} reason=invalid_remote_path"
                                );
                                return Err(e);
                            }
                        }
                    }
                    other => {
                        let default_base_path =
                            match get_default_base_path(hs.as_ref(), &name).await {
                                Ok(value) => value,
                                Err(e) => {
                                    log::warn!(
                                        "submit failed remote_addr={remote_addr} name={name} reason=default_base_path_unavailable"
                                    );
                                    return Err(e);
                                }
                            };
                        let random_suffix = util::random::generate_run_directory_name();
                        match resolve_submit_remote_path(other, &default_base_path, &random_suffix) {
                            Ok(value) => value,
                            Err(e) => {
                                log::warn!(
                                    "submit failed remote_addr={remote_addr} name={name} reason=invalid_remote_path"
                                );
                                return Err(e);
                            }
                        }
                    }
                };
                (resolved, false)
            }
        };
        log::info!(
            "submit resolved remote_addr={remote_addr} name={name} remote_path={remote_path}"
        );

        if evt_tx
            .send(Ok(SubmitStreamEvent {
                event: Some(submit_stream_event::Event::SubmitStatus(SubmitStatus {
                    name: name.clone(),
                    remote_path: remote_path.clone(),
                    phase: submit_status::Phase::Resolved as i32,
                })),
            }))
            .await
            .is_err()
        {
            log::warn!(
                "submit canceled remote_addr={remote_addr} name={name} reason=client_closed"
            );
            return Err(Status::cancelled(error_codes::CANCELED));
        }

        if *cancel_rx.borrow() {
            log::warn!(
                "submit canceled remote_addr={remote_addr} name={name} reason=client_canceled"
            );
            return Err(Status::cancelled(error_codes::CANCELED));
        }

        match mgr
            .ensure_connected_submit(&evt_tx.clone(), &mut mfa_rx)
            .await
        {
            Ok(_) => {}
            Err(e) => {
                log::warn!(
                    "submit failed remote_addr={remote_addr} name={name} reason=connect_failed error={e}"
                );
                log::debug!("could not establish connection to {}: {}", &name, e);
                let code = error_codes::code_for_ssh_error(&e);
                return Err(Status::internal(code));
            }
        };

        let remote_path_exists = match mgr.directory_exists(&remote_path).await {
            Ok(v) => v,
            Err(e) => {
                log::warn!(
                    "submit failed remote_addr={remote_addr} name={name} reason=remote_path_check_failed error={e}"
                );
                log::debug!("can't list {} on {}: {}", &remote_path, &name, e);
                return Err(Status::internal(error_codes::REMOTE_ERROR));
            }
        };

        if remote_path_exists && !allow_existing_remote_path {
            log::warn!(
                "submit failed remote_addr={remote_addr} name={name} reason=remote_path_exists remote_path={remote_path}"
            );
            return Err(Status::already_exists(error_codes::CONFLICT));
        }

        log::debug!(
            "transfering data from {} to {:?}",
            &local_path,
            &remote_path
        );

        let mut cancel_rx = cancel_rx.clone();
        let hs = hs.clone();
        let audit_remote_addr = remote_addr.clone();
        tokio::spawn(async move {
            if evt_tx
                .send(Ok(SubmitStreamEvent {
                    event: Some(submit_stream_event::Event::SubmitStatus(SubmitStatus {
                        name: name.clone(),
                        remote_path: remote_path.clone(),
                        phase: submit_status::Phase::TransferStart as i32,
                    })),
                }))
                .await
                .is_err()
            {
                return;
            }
            let options = crate::ssh::SyncOptions {
                block_size: Some(1024 * 1024),
                parallelism: None,
                filters: &filters,
            };
            let sync_result = tokio::select! {
                res = mgr.sync_dir(
                    &local_path,
                    &remote_path,
                    options,
                    &evt_tx,
                    mfa_rx,
                ) => res,
                _ = evt_tx.closed() => {
                    return;
                }
                _ = cancel_rx.changed() => {
                    return;
                }
            };
            if let Err(err) = sync_result {
                log::warn!(
                    "submit failed remote_addr={audit_remote_addr} name={name} reason=sync_failed error={err}"
                );
                log::debug!("sync failed for submit: {err}");
                let _ = evt_tx
                    .send(Ok(SubmitStreamEvent {
                        event: Some(submit_stream_event::Event::SubmitResult(SubmitResult {
                            status: submit_result::Status::Failed as i32,
                            job_id: None,
                            detail: error_codes::REMOTE_ERROR.to_string(),
                        })),
                    }))
                    .await;
                return;
            };
            if evt_tx
                .send(Ok(SubmitStreamEvent {
                    event: Some(submit_stream_event::Event::SubmitStatus(SubmitStatus {
                        name: name.clone(),
                        remote_path: remote_path.clone(),
                        phase: submit_status::Phase::TransferDone as i32,
                    })),
                }))
                .await
                .is_err()
            {
                return;
            }
            if evt_tx.is_closed() || *cancel_rx.borrow() {
                return;
            }
            let remote_sbatch_script_path = resolve_remote_sbatch_path(&remote_path, &sbatchscript);

            let sbatch_command = crate::agent::slurm::path_to_sbatch_command(
                &remote_sbatch_script_path,
                Some(&remote_path),
            );
            log::debug!("running remote script {}", &remote_sbatch_script_path);
            let exec_result = tokio::select! {
                res = mgr.exec_capture(&sbatch_command) => res,
                _ = evt_tx.closed() => {
                    return;
                }
                _ = cancel_rx.changed() => {
                    return;
                }
            };
            let (out, err, code) = match exec_result {
                Ok(v) => (v.0, v.1, v.2),
                Err(e) => {
                    log::warn!(
                        "submit failed remote_addr={audit_remote_addr} name={name} reason=sbatch_exec_failed error={e}"
                    );
                    log::debug!("sbatch execution failed: {e}");
                    let _ = evt_tx
                        .send(Ok(SubmitStreamEvent {
                            event: Some(submit_stream_event::Event::SubmitResult(SubmitResult {
                                status: submit_result::Status::Failed as i32,
                                job_id: None,
                                detail: error_codes::REMOTE_ERROR.to_string(),
                            })),
                        }))
                        .await;
                    return;
                }
            };

            let err_message = String::from_utf8_lossy(&err);
            log::debug!(
                "submitted remote script, received from sbatch code {}, error message: {}",
                code,
                err_message
            );
            if code != 0 {
                let out_message = String::from_utf8_lossy(&out);
                let detail = if err_message.trim().is_empty() {
                    if out_message.trim().is_empty() {
                        "no error output from sbatch"
                    } else {
                        out_message.trim()
                    }
                } else {
                    err_message.trim()
                };
                log::warn!(
                    "submit failed remote_addr={audit_remote_addr} name={name} reason=sbatch_nonzero_exit detail={detail}"
                );
                log::debug!("sbatch failed: {}", detail);
                let _ = evt_tx
                    .send(Ok(SubmitStreamEvent {
                        event: Some(submit_stream_event::Event::SubmitResult(SubmitResult {
                            status: submit_result::Status::Failed as i32,
                            job_id: None,
                            detail: error_codes::REMOTE_ERROR.to_string(),
                        })),
                    }))
                    .await;
                return;
            }
            let out_string = String::from_utf8_lossy(&out);
            let scheduler_id = crate::agent::slurm::parse_job_id(&out_string);
            if scheduler_id.is_none() {
                log::warn!(
                    "submit failed remote_addr={audit_remote_addr} name={name} reason=missing_job_id"
                );
                log::debug!("sbatch did not return a job id");
                let _ = evt_tx
                    .send(Ok(SubmitStreamEvent {
                        event: Some(submit_stream_event::Event::SubmitResult(SubmitResult {
                            status: submit_result::Status::Failed as i32,
                            job_id: None,
                            detail: error_codes::REMOTE_ERROR.to_string(),
                        })),
                    }))
                    .await;
                return;
            }
            let scheduler_id = scheduler_id.unwrap();

            let sbatch_path = {
                let sbatch_path = PathBuf::from(&sbatchscript);
                if sbatch_path.is_absolute() {
                    sbatch_path
                } else {
                    PathBuf::from(&local_path).join(&sbatchscript)
                }
            };
            let Ok(Some(hr)) = hs.get_by_name(&name).await else {
                log::warn!(
                    "submit failed remote_addr={audit_remote_addr} name={name} reason=unknown_cluster"
                );
                log::debug!("unknown name '{}' while creating job record", name);
                let _ = evt_tx
                    .send(Ok(SubmitStreamEvent {
                        event: Some(submit_stream_event::Event::SubmitResult(SubmitResult {
                            status: submit_result::Status::Failed as i32,
                            job_id: None,
                            detail: error_codes::NOT_FOUND.to_string(),
                        })),
                    }))
                    .await;
                return;
            };
            let templates = match std::fs::read_to_string(&sbatch_path) {
                Ok(contents) => sbatch::parse_sbatch_log_templates(&contents),
                Err(e) => {
                    log::warn!(
                        "submit log parse failed remote_addr={audit_remote_addr} name={name} sbatchscript={} error={e}",
                        sbatch_path.to_string_lossy()
                    );
                    sbatch::SbatchLogTemplates {
                        stdout: None,
                        stderr: None,
                        job_name: None,
                    }
                }
            };
            let sbatch::SbatchLogTemplates {
                stdout,
                stderr,
                job_name,
            } = templates;
            let default_job_name = sbatch_path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.trim().is_empty())
                .unwrap_or("job");
            let job_name = job_name.unwrap_or_else(|| default_job_name.to_string());
            let stdout_template = stdout.unwrap_or_else(|| sbatch::DEFAULT_STDOUT_TEMPLATE.to_string());
            let stdout_path = sbatch::resolve_log_path(
                &stdout_template,
                &remote_path,
                scheduler_id,
                Some(job_name.as_str()),
                Some(hr.username.as_str()),
            );
            let stderr_path = stderr.and_then(|value| {
                    let trimmed = value.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                })
                .map(|template| {
                    sbatch::resolve_log_path(
                        &template,
                        &remote_path,
                        scheduler_id,
                        Some(job_name.as_str()),
                        Some(hr.username.as_str()),
                    )
                });

            let nj = crate::state::db::NewJob {
                scheduler_id: Some(scheduler_id),
                host_id: hr.id,
                local_path,
                remote_path,
                stdout_path,
                stderr_path,
            };
            match hs.insert_job(&nj).await {
                Ok(job_id) => {
                    log::info!(
                        "submit completed remote_addr={audit_remote_addr} name={name} job_id={job_id} scheduler_id={:?} local_path={} remote_path={}",
                        nj.scheduler_id,
                        nj.local_path,
                        nj.remote_path
                    );
                    let _ = evt_tx
                        .send(Ok(SubmitStreamEvent {
                            event: Some(submit_stream_event::Event::SubmitResult(SubmitResult {
                                status: submit_result::Status::Submitted as i32,
                                job_id: Some(job_id),
                                detail: String::new(),
                            })),
                        }))
                        .await;
                }
                Err(e) => {
                    log::warn!(
                        "submit failed remote_addr={audit_remote_addr} name={name} reason=job_record_insert_failed error={e}"
                    );
                    log::debug!("failed to create job record: {e}");
                    let _ = evt_tx
                        .send(Ok(SubmitStreamEvent {
                            event: Some(submit_stream_event::Event::SubmitResult(SubmitResult {
                                status: submit_result::Status::Failed as i32,
                                job_id: None,
                                detail: error_codes::INTERNAL_ERROR.to_string(),
                            })),
                        }))
                        .await;
                }
            }
        });

        let out: SubmitOutStream = Box::pin(crate::ssh::receiver_to_stream(evt_rx));
        Ok(tonic::Response::new(out))
    }

    async fn add_cluster(
        &self,
        request: tonic::Request<tonic::Streaming<AddClusterRequest>>,
    ) -> Result<tonic::Response<Self::AddClusterStream>, Status> {
        let remote_addr = format_remote_addr(request.remote_addr());

        let mut inbound = request.into_inner();
        let init = inbound
            .message()
            .await
            .map_err(|e| {
                log::debug!("read error in add_cluster: {e}");
                Status::unknown(error_codes::INTERNAL_ERROR)
            })?
            .ok_or_else(|| Status::invalid_argument(error_codes::INVALID_ARGUMENT))?;
        let (username, host, name, identity_path, port, default_base_path) = match init.msg {
            Some(proto::add_cluster_request::Msg::Init(i)) => (
                i.username,
                i.host,
                i.name,
                i.identity_path,
                i.port,
                i.default_base_path,
            ),
            _ => {
                return Err(Status::invalid_argument(error_codes::INVALID_ARGUMENT));
            }
        };
        let addr = match parse_add_cluster_host(host) {
            Ok(value) => value,
            Err(e) => {
                log::warn!(
                    "cluster_upsert failed remote_addr={remote_addr} name={name} reason=invalid_host"
                );
                return Err(e);
            }
        };
        let host_label = format_address(&addr);
        let default_base_path_label = default_base_path.as_deref().unwrap_or("<none>");
        log::info!(
            "cluster_upsert start remote_addr={remote_addr} name={name} username={username} host={host_label} port={port} default_base_path={default_base_path_label}"
        );
        let (evt_tx, evt_rx) = tokio::sync::mpsc::channel::<Result<StreamEvent, Status>>(64);

        let (mfa_tx, mut mfa_rx) = tokio::sync::mpsc::channel::<MfaAnswer>(16);
        tokio::spawn(async move {
            while let Ok(Some(item)) = inbound.message().await {
                if let Some(proto::add_cluster_request::Msg::Mfa(ans)) = item.msg
                    && mfa_tx.send(ans).await.is_err()
                {
                    break;
                }
            }
        });

        let port = match parse_add_cluster_port(port) {
            Ok(value) => value,
            Err(e) => {
                log::warn!(
                    "cluster_upsert failed remote_addr={remote_addr} name={name} reason=invalid_port"
                );
                return Err(e);
            }
        };
        let connection_addr = match resolve_host_addr(&addr, port).await {
            Ok(value) => value,
            Err(e) => {
                log::warn!(
                    "cluster_upsert failed remote_addr={remote_addr} name={name} reason=host_resolution_failed error={e}"
                );
                return Err(e);
            }
        };
        let ssh_params = crate::ssh::SshParams {
            username: username.clone(),
            addr: connection_addr,
            identity_path: identity_path.clone(),
            keepalive_secs: 60,
            ki_submethods: None,
        };
        let hs = self.hosts();
        let sessions = self.sessions();
        let audit_remote_addr = remote_addr.clone();
        let audit_name = name.clone();
        let audit_host_label = host_label.clone();
        let reachability_addr = addr.clone();
        let reachability_port = port;
        tokio::spawn(async move {
            let reachable =
                match reachability::check_host_reachable(&reachability_addr, reachability_port).await
                {
                    Ok(value) => value,
                    Err(err) => {
                        log::warn!(
                            "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=reachability_failed error={err}"
                        );
                        let status = match &reachability_addr {
                            Address::Hostname(hostname) => map_net_error(hostname, err),
                            Address::Ip(_) => Status::aborted(error_codes::NETWORK_ERROR),
                        };
                        let _ = evt_tx.send(Err(status)).await;
                        return;
                    }
                };
            if !reachable {
                log::warn!(
                    "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=host_unreachable"
                );
                let _ = evt_tx
                    .send(Err(Status::aborted(error_codes::NETWORK_ERROR)))
                    .await;
                return;
            }
            let sm = match sessions.get(&name).await {
                Some(existing) if existing.matches_params(&ssh_params) => existing,
                _ => Arc::new(crate::ssh::SessionManager::new(ssh_params)),
            };
            if let Err(e) = sm.ensure_connected(&evt_tx, &mut mfa_rx).await {
                log::warn!(
                    "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=connect_failed error={e}"
                );
                log::debug!("failed to connect to {name}: {e}");
                let code = error_codes::code_for_ssh_error(&e);
                let _ = evt_tx
                    .send(Err(Status::aborted(code)))
                    .await;
                return;
            };

            let home_dir = match fetch_remote_home_dir(&sm, &name).await {
                Ok(v) => v,
                Err(e) => {
                    log::warn!(
                        "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=home_dir_failed error_code={:?}",
                        e.code()
                    );
                    let _ = evt_tx.send(Err(e)).await;
                    return;
                }
            };
            let (out, err, code) = match sm
                .exec_capture(crate::agent::managers::DETERMINE_HPC_WORKLOAD_MANAGERS_CMD)
                .await
            {
                Ok((vo, ve, ec)) => (vo, ve, ec),
                Err(e) => {
                    log::warn!(
                        "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=wlm_metadata_failed error={e}"
                    );
                    log::debug!("failed to gather cluster metadata for {name}: {e}");
                    let _ = evt_tx
                        .send(Err(Status::aborted(error_codes::REMOTE_ERROR)))
                        .await;
                    return;
                }
            };

            if code != 0 {
                let err_message =
                    String::from_utf8(err).unwrap_or("<error message could not be decoded>".into());
                log::warn!(
                    "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=wlm_metadata_exit code={code}"
                );
                log::debug!(
                    "failed to gather cluster metadata for {name}: exit {}: {}",
                    code,
                    err_message
                );
                let _ = evt_tx
                    .send(Err(Status::aborted(error_codes::REMOTE_ERROR)))
                    .await;
                return;
            }
            let out = match String::from_utf8(out) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!(
                        "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=wlm_metadata_decode_failed error={e}"
                    );
                    log::debug!(
                        "failed to gather cluster metadata for {name}: decode error: {e}"
                    );
                    let _ = evt_tx
                        .send(Err(Status::aborted(error_codes::REMOTE_ERROR)))
                        .await;
                    return;
                }
            };

            let wlms = crate::agent::managers::parse_wlms(&out);
            if !wlms.contains(&crate::agent::managers::WorkloadManager::Slurm) {
                log::warn!(
                    "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=unsupported_scheduler"
                );
                log::debug!(
                    "no supported workload managers found on {name}; identified: {:?}",
                    wlms
                );
                let _ = evt_tx
                    .send(Err(Status::aborted(error_codes::REMOTE_ERROR)))
                    .await;
                return;
            }
            send_add_cluster_progress(&evt_tx, "Scheduler: Slurm").await;
            let (out, err, code) = match sm.exec_capture(crate::agent::os::GATHER_OS_INFO_CMD).await
            {
                Ok((vo, ve, ec)) => (vo, ve, ec),
                Err(e) => {
                    log::warn!(
                        "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=os_metadata_failed error={e}"
                    );
                    log::debug!("failed to gather cluster os metadata for {name}: {e}");
                    let _ = evt_tx
                        .send(Err(Status::aborted(error_codes::REMOTE_ERROR)))
                        .await;
                    return;
                }
            };

            if code != 0 {
                let err_message =
                    String::from_utf8(err).unwrap_or("<error message could not be decoded>".into());
                log::warn!(
                    "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=os_metadata_exit code={code}"
                );
                log::debug!(
                    "failed to gather cluster os metadata for {name}: exit {}: {}",
                    code,
                    err_message
                );
                let _ = evt_tx
                    .send(Err(Status::aborted(error_codes::REMOTE_ERROR)))
                    .await;
                return;
            }
            let out = match String::from_utf8(out) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!(
                        "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=os_metadata_decode_failed error={e}"
                    );
                    log::debug!(
                        "failed to gather cluster os metadata for {name}: decode error: {e}"
                    );
                    let _ = evt_tx
                        .send(Err(Status::aborted(error_codes::REMOTE_ERROR)))
                        .await;
                    return;
                }
            };

            let os_info = match crate::agent::os::parse_distro_info(&out) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!(
                        "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=os_metadata_parse_failed error={e}"
                    );
                    log::debug!(
                        "failed to gather cluster os metadata for {name}: parse error: {e}"
                    );
                    let _ = evt_tx
                        .send(Err(Status::aborted(error_codes::REMOTE_ERROR)))
                        .await;
                    return;
                }
            };
            let os_label = format!(
                "OS: {} {}",
                os_info.id.as_str(),
                os_info.version.as_str()
            );
            send_add_cluster_progress(&evt_tx, &os_label).await;
            let kernel_label = format!("Kernel: {}", os_info.kernel.as_str());
            send_add_cluster_progress(&evt_tx, &kernel_label).await;
            let distro_info = crate::state::db::Distro {
                name: os_info.id,
                version: os_info.version,
            };

            let (out, err, code) = match sm
                .exec_capture(crate::agent::slurm::DETERMINE_SLURM_VERSION_CMD)
                .await
            {
                Ok((vo, ve, ec)) => (vo, ve, ec),
                Err(e) => {
                    log::warn!(
                        "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=slurm_version_failed error={e}"
                    );
                    log::debug!("failed to gather slurm version for {name}: {e}");
                    let _ = evt_tx
                        .send(Err(Status::aborted(error_codes::REMOTE_ERROR)))
                        .await;
                    return;
                }
            };

            if code != 0 {
                let err_message =
                    String::from_utf8(err).unwrap_or("<error message could not be decoded>".into());
                log::warn!(
                    "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=slurm_version_exit code={code}"
                );
                log::debug!(
                    "failed to gather slurm version for {name}: exit {}: {}",
                    code,
                    err_message
                );
                let _ = evt_tx
                    .send(Err(Status::aborted(error_codes::REMOTE_ERROR)))
                    .await;
                return;
            }
            let out = match String::from_utf8(out) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!(
                        "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=slurm_version_decode_failed error={e}"
                    );
                    log::debug!(
                        "failed to gather slurm version for {name}: decode error: {e}"
                    );
                    let _ = evt_tx
                        .send(Err(Status::aborted(error_codes::REMOTE_ERROR)))
                        .await;
                    return;
                }
            };
            let mut parts = out.split_whitespace();
            if parts.next().is_none() {
                log::warn!(
                    "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=slurm_version_unexpected_output"
                );
                log::debug!(
                    "failed to gather slurm version for {name}: unexpected output: {out}"
                );
                let _ = evt_tx
                    .send(Err(Status::aborted(error_codes::REMOTE_ERROR)))
                    .await;
                return;
            }

            let slurm_version: crate::state::db::SlurmVersion = match parts.next() {
                Some(v) => match v.parse() {
                    Ok(vv) => vv,
                    Err(e) => {
                        log::warn!(
                            "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=slurm_version_parse_failed error={e:?}"
                        );
                        log::debug!("failed to parse slurm version for {name}: '{e:?}'");
                        let _ = evt_tx
                            .send(Err(Status::aborted(error_codes::REMOTE_ERROR)))
                            .await;
                        return;
                    }
                },
                None => {
                    log::warn!(
                        "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=slurm_version_missing"
                    );
                    log::debug!(
                        "failed to gather slurm version for {name}: unexpected output: {out}"
                    );
                    let _ = evt_tx
                        .send(Err(Status::aborted(error_codes::REMOTE_ERROR)))
                        .await;
                    return;
                }
            };
            send_add_cluster_progress(&evt_tx, &format!("Slurm: {slurm_version}")).await;
            let (out, err, code) = match sm.exec_capture("scontrol show config").await {
                Ok((vo, ve, ec)) => (vo, ve, ec),
                Err(e) => {
                    log::warn!(
                        "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=slurm_config_failed error={e}"
                    );
                    log::debug!("failed to gather cluster config for {name}: {e}");
                    let _ = evt_tx
                        .send(Err(Status::aborted(error_codes::REMOTE_ERROR)))
                        .await;
                    return;
                }
            };
            if code != 0 {
                log::warn!(
                    "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=slurm_config_exit code={code}"
                );
                log::debug!(
                    "failed to run `scontrol show config` on {name}: {}",
                    String::from_utf8_lossy(&err)
                );
                let _ = evt_tx
                    .send(Err(Status::aborted(error_codes::REMOTE_ERROR)))
                    .await;
                return;
            }
            let config = String::from_utf8_lossy(&out);
            let accounting_enabled = crate::agent::slurm::parse_accounting_enabled_from_scontrol(
                &config,
            )
            .unwrap_or_else(|| {
                log::warn!(
                    "unable to determine accounting storage type for {name}, assuming disabled"
                );
                false
            });
            let accounting_state = if accounting_enabled {
                "enabled"
            } else {
                "disabled"
            };
            send_add_cluster_progress(&evt_tx, &format!("Accounting: {accounting_state}")).await;

            let resolved_default_base_path =
                match resolve_default_base_path(default_base_path, &home_dir) {
                    Ok(v) => v,
                    Err(e) => {
                        log::warn!(
                            "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=default_base_path_invalid error_code={:?}",
                            e.code()
                        );
                        let _ = evt_tx.send(Err(e)).await;
                        return;
                    }
                };
            let normalized_default_base_path =
                match normalize_default_base_path(resolved_default_base_path) {
                    Ok(v) => v,
                    Err(e) => {
                        log::warn!(
                            "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=default_base_path_invalid error_code={:?}",
                            e.code()
                        );
                        let _ = evt_tx.send(Err(e)).await;
                        return;
                    }
                };
            if let Some(ref dbp) = normalized_default_base_path {
                let command = format!("mkdir -p {}", dbp.to_string_lossy());
                let (_, err, code) = match sm.exec_capture(&command).await {
                    Ok((vo, ve, ec)) => (vo, ve, ec),
                    Err(e) => {
                        log::warn!(
                            "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=default_base_path_create_failed error={e}"
                        );
                        log::debug!("failed to execute command `{}` on {}: {}", command, name, e);
                        let _ = evt_tx
                            .send(Err(Status::internal(error_codes::REMOTE_ERROR)))
                            .await;
                        return;
                    }
                };
                if code != 0 {
                    log::warn!(
                        "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=default_base_path_create_exit code={code}"
                    );
                    log::debug!(
                        "failed to create default_base_path on {}: {}",
                        name,
                        String::from_utf8_lossy(&err)
                    );
                    let _ = evt_tx
                        .send(Err(Status::aborted(error_codes::REMOTE_ERROR)))
                        .await;
                    return;
                }
            }

            let new_host = crate::state::db::NewHost {
                username,
                name: name.clone(),
                address: addr.clone(),
                distro: distro_info,
                kernel_version: os_info.kernel,
                slurm: slurm_version,
                port,
                identity_path,
                accounting_available: accounting_enabled,
                default_base_path: normalized_default_base_path
                    .clone()
                    .map(|v| v.to_string_lossy().into_owned()),
            };
            match hs.upsert_host(&new_host).await {
                Ok(v) => {
                    log::info!(
                        "cluster_upsert completed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} host_id={v}"
                    );
                    log::debug!("successfully upserted host with id {v}")
                }
                Err(e) => {
                    log::warn!(
                        "cluster_upsert failed remote_addr={audit_remote_addr} name={audit_name} host={audit_host_label} reason=host_upsert_failed error={e}"
                    );
                    log::debug!("failed to upsert host {name}: {e}");
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Error(
                                error_codes::INTERNAL_ERROR.to_string(),
                            )),
                        }))
                        .await;
                }
            };

            sessions.insert(name.clone(), sm.clone()).await;
        });
        let out: OutStream = Box::pin(crate::ssh::receiver_to_stream(evt_rx));
        Ok(tonic::Response::new(out))
    }

    async fn list_jobs(
        &self,
        request: tonic::Request<ListJobsRequest>,
    ) -> Result<tonic::Response<ListJobsResponse>, Status> {
        let remote_addr = format_remote_addr(request.remote_addr());
        let inbound = request.into_inner();
        let name_filter = inbound.name.clone();

        let jobs = match inbound.name {
            Some(ref v) => {
                let Ok(Some(hr)) = self.hosts().get_by_name(v).await else {
                    log::warn!(
                        "list_jobs failed remote_addr={remote_addr} name={v} reason=not_found"
                    );
                    return Err(Status::invalid_argument(error_codes::NOT_FOUND));
                };

                match self.hosts().list_jobs_for_host(hr.id).await {
                    Ok(v) => v,
                    Err(e) => {
                        log::debug!("couldn't list jobs for host '{}': {}", hr.name, e);
                        return Err(Status::internal(error_codes::INTERNAL_ERROR));
                    }
                }
            }
            None => match self.hosts().list_all_jobs().await {
                Ok(v) => v,
                Err(e) => {
                    log::debug!("couldn't list jobs for all hosts: {}", e);
                    return Err(Status::internal(error_codes::INTERNAL_ERROR));
                }
            },
        };
        let api_jobs: Vec<_> = jobs
            .into_iter()
            .map(|jr| db_job_record_to_api_unit_response(&jr))
            .collect();
        let name_label = name_filter.as_deref().unwrap_or("<all>");
        log::info!(
            "list_jobs remote_addr={remote_addr} name={name_label} count={}",
            api_jobs.len()
        );
        Ok(tonic::Response::new(ListJobsResponse { jobs: api_jobs }))
    }
}
