// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use crate::agent::add_cluster::{
    normalize_default_base_path, parse_add_cluster_host, parse_add_cluster_port, resolve_host_addr,
};
use crate::agent::helpers::{
    build_sync_filters, db_host_record_to_api_unit_response, db_job_record_to_api_unit_response,
    get_default_base_path,
};
use crate::agent::service::AgentSvc;
use crate::agent::submit::{resolve_remote_sbatch_path, resolve_submit_remote_path};
use crate::agent::types::{AgentSvcError, OutStream, SubmitOutStream};
use crate::ssh::sh_escape;
use crate::state::db::HostStoreError;
use crate::util;
use crate::util::remote_path::normalize_path;
use proto::agent_server::Agent;
use proto::{
    AddClusterRequest, ListClustersRequest, ListClustersResponse, ListClustersUnitResponse,
    ListJobsRequest, ListJobsResponse, LsRequest, LsRequestInit, MfaAnswer, PingReply, PingRequest,
    RetrieveJobRequest, RetrieveJobRequestInit, StreamEvent, SubmitRequest, SubmitResult,
    SubmitStatus, SubmitStreamEvent, stream_event, submit_result, submit_status,
    submit_stream_event,
};
use std::path::PathBuf;
use std::sync::Arc;
use tonic::Status;

#[tonic::async_trait]
impl Agent for AgentSvc {
    type LsStream = OutStream;
    type RetrieveJobStream = OutStream;
    type SubmitStream = SubmitOutStream;
    type AddClusterStream = OutStream;

    async fn ping(
        &self,
        request: tonic::Request<PingRequest>,
    ) -> Result<tonic::Response<PingReply>, Status> {
        let req = request.into_inner();
        match req.message.trim() {
            "ping" => Ok(tonic::Response::new(PingReply {
                message: "pong".into(),
            })),
            m => Err(Status::invalid_argument(format!(
                "expected message 'ping', got '{}'",
                m
            ))),
        }
    }

    async fn ls(
        &self,
        request: tonic::Request<tonic::Streaming<LsRequest>>,
    ) -> Result<tonic::Response<Self::LsStream>, Status> {
        let mut inbound = request.into_inner();
        let init = inbound
            .message()
            .await
            .map_err(|e| Status::unknown(format!("read error: {e}")))?
            .ok_or_else(|| Status::invalid_argument("stream closed before init"))?;

        let (hostid, path) = match init.msg {
            Some(proto::ls_request::Msg::Init(LsRequestInit { hostid, path })) => (hostid, path),
            _ => {
                return Err(Status::invalid_argument(
                    "first message must be init(hostid)",
                ));
            }
        };

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

        let list_path = match path {
            Some(v) => {
                if v.is_empty() {
                    return Err(Status::invalid_argument(
                        "path can't be empty: provide a path or omit it completely",
                    ));
                }
                if PathBuf::from(&v).is_absolute() {
                    normalize_path(v).to_string_lossy().into_owned()
                } else {
                    let default_base_path =
                        get_default_base_path(self.hosts().as_ref(), &hostid).await?;
                    let base_path = PathBuf::from(default_base_path);
                    util::remote_path::resolve_relative(base_path, v)
                        .to_string_lossy()
                        .into_owned()
                }
            }
            None => normalize_path(get_default_base_path(self.hosts().as_ref(), &hostid).await?)
                .to_string_lossy()
                .into_owned(),
        };

        let command = format!("ls -- {}", sh_escape(&list_path));
        self.run_command(command, &hostid, mfa_rx).await
    }

    async fn retrieve_job(
        &self,
        request: tonic::Request<tonic::Streaming<RetrieveJobRequest>>,
    ) -> Result<tonic::Response<Self::RetrieveJobStream>, Status> {
        let mut inbound = request.into_inner();

        let init = inbound
            .message()
            .await
            .map_err(|e| Status::unknown(format!("read error: {e}")))?
            .ok_or_else(|| Status::invalid_argument("stream closed before init"))?;

        let (job_id, hostid, path, local_path) = match init.msg {
            Some(proto::retrieve_job_request::Msg::Init(RetrieveJobRequestInit {
                job_id,
                hostid,
                path,
                local_path,
            })) => (job_id, hostid, path, local_path),
            _ => {
                return Err(Status::invalid_argument(
                    "first message must be init(job_id, hostid, path, local_path)",
                ));
            }
        };

        if path.trim().is_empty() {
            return Err(Status::invalid_argument("path can't be empty"));
        }
        let local_path = match local_path {
            Some(v) if !v.trim().is_empty() => v,
            _ => {
                return Err(Status::invalid_argument(
                    "local_path can't be empty for retrieve",
                ));
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

        tokio::spawn(async move {
            let (hostid, run_path) = {
                let job = match hs.get_job_by_job_id(job_id).await {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        let _ = evt_tx
                            .send(Ok(StreamEvent {
                                event: Some(stream_event::Event::Error(format!(
                                    "job id {job_id} not found",
                                ))),
                            }))
                            .await;
                        return;
                    }
                    Err(e) => {
                        let _ = evt_tx
                            .send(Ok(StreamEvent {
                                event: Some(stream_event::Event::Error(format!(
                                    "could not fetch job id {job_id}: {e}",
                                ))),
                            }))
                            .await;
                        return;
                    }
                };
                if let Some(expected) = hostid.as_deref()
                    && expected != job.host_id
                {
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Error(format!(
                                "job id {job_id} not found in cluster '{expected}'",
                            ))),
                        }))
                        .await;
                    return;
                }
                (job.host_id.clone(), job.remote_path.clone())
            };

            let mgr = match svc.get_sessionmanager(&hostid).await {
                Ok(v) => v,
                Err(e) => {
                    let message = match e {
                        AgentSvcError::UnknownHostId => format!("unknown hostid {hostid}"),
                        AgentSvcError::NetworkError(e) => format!("network error: {e}"),
                        other_error => format!("unexpected error: {other_error}"),
                    };
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Error(message)),
                        }))
                        .await;
                    return;
                }
            };

            if let Err(err) = mgr.ensure_connected(&evt_tx, &mut mfa_rx).await {
                let _ = evt_tx
                    .send(Ok(StreamEvent {
                        event: Some(stream_event::Event::Error(err.to_string())),
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
                        let _ = evt_tx
                            .send(Ok(StreamEvent {
                                event: Some(stream_event::Event::Error(format!(
                                    "could not resolve local destination: {e}",
                                ))),
                            }))
                            .await;
                        return;
                    }
                }
            }

            let local_target = if path_is_absolute {
                let Some(name) = std::path::Path::new(&remote_path).file_name() else {
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Error(
                                "remote path has no basename".to_string(),
                            )),
                        }))
                        .await;
                    return;
                };
                local_base.join(name)
            } else {
                local_base.join(std::path::Path::new(&path))
            };

            if let Err(err) = mgr.retrieve_path(&remote_path, &local_target).await {
                let _ = evt_tx
                    .send(Ok(StreamEvent {
                        event: Some(stream_event::Event::Error(err.to_string())),
                    }))
                    .await;
                return;
            }

            let _ = evt_tx
                .send(Ok(StreamEvent {
                    event: Some(stream_event::Event::ExitCode(0)),
                }))
                .await;
        });

        let out: OutStream = Box::pin(crate::ssh::receiver_to_stream(evt_rx));
        Ok(tonic::Response::new(out))
    }

    async fn list_clusters(
        &self,
        request: tonic::Request<ListClustersRequest>,
    ) -> Result<tonic::Response<ListClustersResponse>, Status> {
        log::info!("listing clusters");
        let _inbound = request.into_inner();
        let hosts = match self.hosts().list_hosts(None).await {
            Ok(v) => v,
            Err(e) => match e {
                HostStoreError::Sqlx(sql_err) => {
                    log::error!("unknown error at sqlx level: {sql_err}");
                    return Err(Status::internal(
                        "internal error: please report this  error along with daemon logs",
                    ));
                }
                other_error => {
                    log::error!("internal error: unexpected error: {other_error}");
                    return Err(Status::internal(
                        "internal error: please report this error along with daemon logs",
                    ));
                }
            },
        };
        let mut clusters: Vec<ListClustersUnitResponse> = hosts
            .iter()
            .map(db_host_record_to_api_unit_response)
            .collect();

        for cluster in clusters.iter_mut() {
            if self.sessions().is_connected(&cluster.hostid).await {
                cluster.connected = true;
            }
        }

        Ok(ListClustersResponse { clusters }.into())
    }

    async fn submit(
        &self,
        request: tonic::Request<tonic::Streaming<SubmitRequest>>,
    ) -> Result<tonic::Response<Self::SubmitStream>, Status> {
        log::info!("submit request");
        let mut inbound = request.into_inner();
        let init = inbound
            .message()
            .await
            .map_err(|e| Status::unknown(format!("read error: {e}")))?
            .ok_or_else(|| Status::invalid_argument("stream closed before init"))?;

        let (local_path, remote_path, hostid, sbatchscript, filters) = match init.msg {
            Some(proto::submit_request::Msg::Init(i)) => (
                i.local_path,
                i.remote_path,
                i.hostid,
                i.sbatchscript,
                i.filters,
            ),
            _ => return Err(Status::invalid_argument("first message must be init(path)")),
        };
        let filters = build_sync_filters(filters)?;

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

        let mgr = match self.get_sessionmanager(&hostid).await {
            Ok(v) => v,
            Err(e) => match e {
                AgentSvcError::UnknownHostId => {
                    return Err(Status::invalid_argument(format!("unknown hostid {hostid}")));
                }
                AgentSvcError::NetworkError(e) => {
                    return Err(Status::internal(format!("network error: {e}")));
                }
                other_error => {
                    return Err(Status::internal(format!("unexpected error: {other_error}")));
                }
            },
        };

        let hs = self.hosts();
        let remote_path = match remote_path.as_deref() {
            Some(v) if PathBuf::from(v).is_absolute() => {
                resolve_submit_remote_path(Some(v), v, "")?
            }
            other => {
                let default_base_path = get_default_base_path(hs.as_ref(), &hostid).await?;
                let random_suffix = util::random::generate_run_directory_name();
                resolve_submit_remote_path(other, &default_base_path, &random_suffix)?
            }
        };

        if evt_tx
            .send(Ok(SubmitStreamEvent {
                event: Some(submit_stream_event::Event::SubmitStatus(SubmitStatus {
                    hostid: hostid.clone(),
                    remote_path: remote_path.clone(),
                    phase: submit_status::Phase::Resolved as i32,
                })),
            }))
            .await
            .is_err()
        {
            return Err(Status::cancelled("client disconnected"));
        }

        if *cancel_rx.borrow() {
            return Err(Status::cancelled("client disconnected"));
        }

        match mgr
            .ensure_connected_submit(&evt_tx.clone(), &mut mfa_rx)
            .await
        {
            Ok(_) => {}
            Err(e) => {
                return Err(Status::internal(format!(
                    "could not establish connection to {}: {}",
                    &hostid, e
                )));
            }
        };

        let remote_path_exists = match mgr.directory_exists(&remote_path).await {
            Ok(v) => v,
            Err(e) => {
                return Err(Status::internal(format!(
                    "can't list {} on {}: {}",
                    &remote_path, &hostid, e
                )));
            }
        };

        if remote_path_exists {
            return Err(Status::invalid_argument(format!(
                "can't use {} as remote path on {}: directory already exists on remote",
                &remote_path, &hostid
            )));
        }

        log::debug!(
            "transfering data from {} to {:?}",
            &local_path,
            &remote_path
        );

        let mut cancel_rx = cancel_rx.clone();
        let hs = hs.clone();
        tokio::spawn(async move {
            if evt_tx
                .send(Ok(SubmitStreamEvent {
                    event: Some(submit_stream_event::Event::SubmitStatus(SubmitStatus {
                        hostid: hostid.clone(),
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
                let _ = evt_tx
                    .send(Ok(SubmitStreamEvent {
                        event: Some(submit_stream_event::Event::SubmitResult(SubmitResult {
                            status: submit_result::Status::Failed as i32,
                            job_id: None,
                            detail: err.to_string(),
                        })),
                    }))
                    .await;
                return;
            };
            if evt_tx
                .send(Ok(SubmitStreamEvent {
                    event: Some(submit_stream_event::Event::SubmitStatus(SubmitStatus {
                        hostid: hostid.clone(),
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
                    let _ = evt_tx
                        .send(Ok(SubmitStreamEvent {
                            event: Some(submit_stream_event::Event::SubmitResult(SubmitResult {
                                status: submit_result::Status::Failed as i32,
                                job_id: None,
                                detail: e.to_string(),
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
                let _ = evt_tx
                    .send(Ok(SubmitStreamEvent {
                        event: Some(submit_stream_event::Event::SubmitResult(SubmitResult {
                            status: submit_result::Status::Failed as i32,
                            job_id: None,
                            detail: format!("sbatch failed with exit code {}: {}", code, detail),
                        })),
                    }))
                    .await;
                return;
            }
            let out_string = String::from_utf8_lossy(&out);
            let slurm_id = crate::agent::slurm::parse_job_id(&out_string);
            if slurm_id.is_none() {
                let _ = evt_tx
                    .send(Ok(SubmitStreamEvent {
                        event: Some(submit_stream_event::Event::SubmitResult(SubmitResult {
                            status: submit_result::Status::Failed as i32,
                            job_id: None,
                            detail: "sbatch did not return a job id".to_string(),
                        })),
                    }))
                    .await;
                return;
            }

            let Ok(Some(hr)) = hs.get_by_hostid(&hostid).await else {
                let _ = evt_tx
                    .send(Ok(SubmitStreamEvent {
                        event: Some(submit_stream_event::Event::SubmitResult(SubmitResult {
                            status: submit_result::Status::Failed as i32,
                            job_id: None,
                            detail: format!("unknown hostid '{}'", hostid),
                        })),
                    }))
                    .await;
                return;
            };

            let nj = crate::state::db::NewJob {
                slurm_id,
                host_id: hr.id,
                local_path,
                remote_path,
            };
            match hs.insert_job(&nj).await {
                Ok(job_id) => {
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
                    let _ = evt_tx
                        .send(Ok(SubmitStreamEvent {
                            event: Some(submit_stream_event::Event::SubmitResult(SubmitResult {
                                status: submit_result::Status::Failed as i32,
                                job_id: None,
                                detail: format!("failed to create job record: {}", e),
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
        log::debug!("adding cluster");

        let mut inbound = request.into_inner();
        let init = inbound
            .message()
            .await
            .map_err(|e| Status::unknown(format!("read error: {e}")))?
            .ok_or_else(|| Status::invalid_argument("stream closed before init"))?;
        let (username, host, hostid, identity_path, port, default_base_path) = match init.msg {
            Some(proto::add_cluster_request::Msg::Init(i)) => (
                i.username,
                i.host,
                i.hostid,
                i.identity_path,
                i.port,
                i.default_base_path,
            ),
            _ => {
                return Err(Status::invalid_argument(
                    "first message must be init(username, hostname, hostid)",
                ));
            }
        };
        let addr = parse_add_cluster_host(host)?;
        log::info!(
            "adding cluster (hostid={},username={},address={:?})",
            &hostid,
            &username,
            &addr
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

        let port = parse_add_cluster_port(port)?;
        let connection_addr = resolve_host_addr(&addr, port).await?;
        let ssh_params = crate::ssh::SshParams {
            username: username.clone(),
            addr: connection_addr,
            identity_path: identity_path.clone(),
            keepalive_secs: 60,
            ki_submethods: None,
        };
        let hs = self.hosts();
        let sessions = self.sessions();
        tokio::spawn(async move {
            let sm = crate::ssh::SessionManager::new(ssh_params);
            if let Err(e) = sm.ensure_connected(&evt_tx, &mut mfa_rx).await {
                let _ = evt_tx
                    .send(Err(Status::aborted(format!(
                        "failed to connect to {hostid}: {e}",
                    ))))
                    .await;
                return;
            };

            let (out, err, code) = match sm
                .exec_capture(crate::agent::managers::DETERMINE_HPC_WORKLOAD_MANAGERS_CMD)
                .await
            {
                Ok((vo, ve, ec)) => (vo, ve, ec),
                Err(e) => {
                    let _ = evt_tx
                        .send(Err(Status::aborted(format!(
                            "failed to gather cluster metadata for {hostid}: {e}",
                        ))))
                        .await;
                    return;
                }
            };

            if code != 0 {
                let err_message =
                    String::from_utf8(err).unwrap_or("<error message could not be decoded>".into());
                let _ = evt_tx
                    .send(Err(Status::aborted(format!(
                        "failed to gather cluster metadata for {hostid}: remote command returned non-zero exit code {}, and error message : {}",
                        code,
                        err_message
                    ))))
                    .await;
                return;
            }
            let out = match String::from_utf8(out) {
                Ok(v) => v,
                Err(e) => {
                    let _ = evt_tx
                    .send(Err(Status::aborted(format!(
                        "failed to gather cluster metadata for {hostid}: could not decode the gathered output: {e}",
                    ))))
                    .await;
                    return;
                }
            };

            let wlms = crate::agent::managers::parse_wlms(&out);
            if !wlms.contains(&crate::agent::managers::WorkloadManager::Slurm) {
                let _ = evt_tx
                    .send(Err(Status::aborted(format!(
                        "no supported workload managers found on {hostid}; identified workload managers: {:?}",
                        wlms
                    ))))
                    .await;
                return;
            }
            let (out, err, code) = match sm.exec_capture(crate::agent::os::GATHER_OS_INFO_CMD).await
            {
                Ok((vo, ve, ec)) => (vo, ve, ec),
                Err(e) => {
                    let _ = evt_tx
                        .send(Err(Status::aborted(format!(
                            "failed to gather cluster os metadata for {hostid}: {e}",
                        ))))
                        .await;
                    return;
                }
            };

            if code != 0 {
                let err_message =
                    String::from_utf8(err).unwrap_or("<error message could not be decoded>".into());
                let _ = evt_tx
                    .send(Err(Status::aborted(format!(
                        "failed to gather cluster os metadata for {hostid}: remote command returned non-zero exit code {}, and error message : {}",
                        code,
                        err_message
                    ))))
                    .await;
                return;
            }
            let out = match String::from_utf8(out) {
                Ok(v) => v,
                Err(e) => {
                    let _ = evt_tx
                    .send(Err(Status::aborted(format!(
                        "failed to gather cluster os metadata for {hostid}: could not decode the gathered output: {e}",
                    ))))
                    .await;
                    return;
                }
            };

            let os_info = match crate::agent::os::parse_distro_info(&out) {
                Ok(v) => v,
                Err(e) => {
                    let _ = evt_tx
                    .send(Err(Status::aborted(format!(
                        "failed to gather cluster os metadata for {hostid}: could not parse the gathered output: {e}",
                    ))))
                    .await;
                    return;
                }
            };
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
                    let _ = evt_tx
                        .send(Err(Status::aborted(format!(
                            "failed to gather slurm version for {hostid}: {e}",
                        ))))
                        .await;
                    return;
                }
            };

            if code != 0 {
                let err_message =
                    String::from_utf8(err).unwrap_or("<error message could not be decoded>".into());
                let _ = evt_tx
                    .send(Err(Status::aborted(format!(
                        "failed to gather slurm version for {hostid}: remote command returned non-zero exit code {}, and error message : {}",
                        code,
                        err_message
                    ))))
                    .await;
                return;
            }
            let out = match String::from_utf8(out) {
                Ok(v) => v,
                Err(e) => {
                    let _ = evt_tx
                    .send(Err(Status::aborted(format!(
                        "failed to gather slurm version for {hostid}: could not decode the gathered output: {e}",
                    ))))
                    .await;
                    return;
                }
            };
            let mut parts = out.split_whitespace();
            if parts.next().is_none() {
                let _ = evt_tx
                    .send(Err(Status::aborted(format!(
                        "failed to gather slurm version for {hostid}: server returned an unexpected output: {out}",

                    ))))
                    .await;
                return;
            }

            let slurm_version: crate::state::db::SlurmVersion = match parts.next() {
                Some(v) => match v.parse() {
                    Ok(vv) => vv,
                    Err(e) => {
                        let _ = evt_tx
                            .send(Err(Status::aborted(format!(
                                "failed to parse slurm version for {hostid}: '{e:?}'",
                            ))))
                            .await;
                        return;
                    }
                },
                None => {
                    let _ = evt_tx
                    .send(Err(Status::aborted(format!(
                        "failed to gather slurm version for {hostid}: server returned an unexpected output: {out}",

                    ))))
                    .await;
                    return;
                }
            };
            let (out, err, code) = match sm.exec_capture("scontrol show config").await {
                Ok((vo, ve, ec)) => (vo, ve, ec),
                Err(e) => {
                    let _ = evt_tx
                        .send(Err(Status::aborted(format!(
                            "failed to gather cluster config for {hostid}: {e}",
                        ))))
                        .await;
                    return;
                }
            };
            if code != 0 {
                let _ = evt_tx
                    .send(Err(Status::aborted(format!(
                        "failed to run `scontrol show config` on {hostid}: {}",
                        String::from_utf8_lossy(&err)
                    ))))
                    .await;
                return;
            }
            let config = String::from_utf8_lossy(&out);
            let accounting_enabled = crate::agent::slurm::parse_accounting_enabled_from_scontrol(
                &config,
            )
            .unwrap_or_else(|| {
                log::warn!(
                    "unable to determine accounting storage type for {hostid}, assuming disabled"
                );
                false
            });

            let normalized_default_base_path = match normalize_default_base_path(default_base_path)
            {
                Ok(v) => v,
                Err(e) => {
                    let _ = evt_tx.send(Err(e)).await;
                    return;
                }
            };
            if let Some(ref dbp) = normalized_default_base_path {
                let command = format!("mkdir -p {}", dbp.to_string_lossy());
                let (_, err, code) = match sm.exec_capture(&command).await {
                    Ok((vo, ve, ec)) => (vo, ve, ec),
                    Err(e) => {
                        let _ = evt_tx
                            .send(Err(Status::internal(format!(
                                "failed to execute command `{}` on {}: {}",
                                command, hostid, e
                            ))))
                            .await;
                        return;
                    }
                };
                if code != 0 {
                    let _ = evt_tx
                        .send(Err(Status::aborted(format!(
                            "failed to create default_base_path on {}: {}",
                            hostid,
                            String::from_utf8_lossy(&err)
                        ))))
                        .await;
                    return;
                }
            }

            let new_host = crate::state::db::NewHost {
                username,
                hostid: hostid.clone(),
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
                    log::debug!("successfully upserted host with id {v}")
                }
                Err(e) => {
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Error(e.to_string())),
                        }))
                        .await;
                }
            };

            sessions.insert(hostid.clone(), Arc::new(sm)).await;
        });
        let out: OutStream = Box::pin(crate::ssh::receiver_to_stream(evt_rx));
        Ok(tonic::Response::new(out))
    }

    async fn list_jobs(
        &self,
        request: tonic::Request<ListJobsRequest>,
    ) -> Result<tonic::Response<ListJobsResponse>, Status> {
        log::info!("listing jobs");
        let inbound = request.into_inner();

        let jobs = match inbound.hostid {
            Some(ref v) => {
                let Ok(Some(hr)) = self.hosts().get_by_hostid(v).await else {
                    return Err(Status::invalid_argument(format!("hostid {} is unknown", v)));
                };

                match self.hosts().list_jobs_for_host(hr.id).await {
                    Ok(v) => v,
                    Err(e) => {
                        return Err(Status::internal(format!(
                            "couldn't list jobs for host '{}': {}",
                            hr.hostid, e
                        )));
                    }
                }
            }
            None => match self.hosts().list_all_jobs().await {
                Ok(v) => v,
                Err(e) => {
                    return Err(Status::internal(format!(
                        "couldn't list jobs for all hosts: {}",
                        e
                    )));
                }
            },
        };
        let api_jobs = jobs
            .into_iter()
            .map(|jr| db_job_record_to_api_unit_response(&jr))
            .collect();
        Ok(tonic::Response::new(ListJobsResponse { jobs: api_jobs }))
    }
}
