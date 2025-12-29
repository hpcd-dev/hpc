use crate::ssh::SessionManager;
use crate::ssh::SyncFilterAction;
use crate::ssh::SyncFilterRule;
use crate::ssh::SshParams;
use crate::ssh::receiver_to_stream;
use crate::ssh::sh_escape;
use crate::state::db::HostRecord;
use crate::state::db::HostStore;
use crate::state::db::HostStoreError;
use crate::state::db::JobRecord;
use crate::state::db::NewJob;
use crate::state::db::SlurmVersion;
use crate::util;
use crate::util::remote_path::normalize_path;
use proto::ListClustersRequest;
use proto::ListClustersResponse;
use proto::ListClustersUnitResponse;
use proto::agent_server::{Agent, AgentServer};
use proto::list_clusters_unit_response;
use proto::{
    AddClusterRequest, ListJobsRequest, ListJobsResponse, ListJobsUnitResponse, LsRequest,
    LsRequestInit, MfaAnswer, PingReply, PingRequest, RetrieveJobRequest, RetrieveJobRequestInit,
    StreamEvent, SubmitPathFilterAction, SubmitPathFilterRule, SubmitRequest, SubmitStatus,
    stream_event, submit_status,
};
use std::any::Any;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use thiserror::Error as ThisError;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio::time::Duration;
use tokio_stream::Stream;
use tonic::Status;
use uuid::Uuid;
mod managers;
mod os;
mod slurm;
use std::pin::Pin;
type OutStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, Status>> + Send + Sync + 'static>>;
#[derive(Debug, PartialEq, Eq, ThisError)]
pub enum AgentSvcError {
    #[error("unknown hostid")]
    UnknownHostId,

    #[error("connection to {hostid} (user={username},host={hostname}) failed: {error}")]
    ConnectionFailed {
        hostid: String,
        username: String,
        hostname: String,
        error: String,
    },
    #[error("network error: {0}")]
    NetworkError(String),

    #[error("database error: {error}")]
    DatabaseError { error: String },
}

#[derive(Clone)]
pub struct AgentSvc {
    mgr: Arc<RwLock<HashMap<String, Arc<SessionManager>>>>,
    hs: Arc<RwLock<HostStore>>,
}
impl AgentSvc {
    pub fn new(hs: HostStore) -> Self {
        let mgr = HashMap::new();
        let mgr_inner = Arc::new(RwLock::new(mgr));

        let hs_inner = Arc::new(RwLock::new(hs));
        AgentSvc {
            mgr: mgr_inner,
            hs: hs_inner,
        }
    }

    pub fn spawn_job_checker(&self, interval: Duration) {
        let svc = self.clone();
        tokio::spawn(async move {
            svc.run_job_check_loop(interval).await;
        });
    }

    async fn run_job_check_loop(self, interval: Duration) {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            if let Err(err) = self.check_running_jobs().await {
                log::warn!("job check failed: {err}");
            }
        }
    }

    async fn check_running_jobs(&self) -> anyhow::Result<()> {
        let hs = self.hs.clone().read_owned().await;
        let jobs = hs.list_running_jobs().await?;
        if jobs.is_empty() {
            return Ok(());
        }
        let hosts = hs.list_hosts(None).await?;
        drop(hs);

        let mut host_map = HashMap::new();
        for host in hosts {
            host_map.insert(host.hostid.clone(), host);
        }

        let mut jobs_by_host: HashMap<String, Vec<JobRecord>> = HashMap::new();
        for job in jobs {
            jobs_by_host
                .entry(job.host_id.clone())
                .or_default()
                .push(job);
        }

        let mut completed_ids: Vec<(i64, Option<String>)> = Vec::new();
        for (hostid, host_jobs) in jobs_by_host {
            let Some(host) = host_map.get(&hostid) else {
                log::warn!("host record missing for running job on '{hostid}'");
                continue;
            };

            let sm = match self.get_sessionmanager(&hostid).await {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("failed to get session for {hostid}: {e}");
                    continue;
                }
            };

            if sm.needs_connect().await {
                let (evt_tx, _evt_rx) =
                    tokio::sync::mpsc::channel::<Result<StreamEvent, Status>>(1);
                let (mfa_tx, mut mfa_rx) = tokio::sync::mpsc::channel::<MfaAnswer>(1);
                drop(mfa_tx);
                if let Err(e) = sm.ensure_connected(&evt_tx, &mut mfa_rx).await {
                    log::warn!("failed to connect to {hostid} for job checks: {e}");
                    continue;
                }
            }

            for job in host_jobs {
                let Some(job_id) = job.slurm_id else {
                    log::warn!("job {} has no slurm id; skipping", job.id);
                    continue;
                };

                if host.accounting_available {
                    let command = format!("sacct -j {job_id} -n -P -o State");
                    let (out, err, code) = match sm.exec_capture(&command).await {
                        Ok(v) => v,
                        Err(e) => {
                            log::warn!("sacct check failed on {hostid} for {job_id}: {e}");
                            continue;
                        }
                    };
                    if code != 0 {
                        log::warn!(
                            "sacct returned {} on {hostid} for {job_id}: {}",
                            code,
                            String::from_utf8_lossy(&err)
                        );
                        continue;
                    }
                    let output = String::from_utf8_lossy(&out);
                    let terminal_state = match slurm::sacct_terminal_state(&output) {
                        Some(v) => v,
                        None => {
                            log::debug!(
                                "sacct returned no terminal state for {hostid} job {job_id}"
                            );
                            continue;
                        }
                    };
                    completed_ids.push((job.id, Some(terminal_state)));
                } else {
                    let command = format!("squeue -j {job_id} -h -o %i");
                    let (out, err, code) = match sm.exec_capture(&command).await {
                        Ok(v) => v,
                        Err(e) => {
                            log::warn!("squeue check failed on {hostid} for {job_id}: {e}");
                            continue;
                        }
                    };
                    if code != 0 {
                        log::warn!(
                            "squeue returned {} on {hostid} for {job_id}: {}",
                            code,
                            String::from_utf8_lossy(&err)
                        );
                        continue;
                    }
                    let output = String::from_utf8_lossy(&out);
                    if output.trim().is_empty() {
                        completed_ids.push((job.id, None));
                    }
                }
            }
        }

        if completed_ids.is_empty() {
            return Ok(());
        }

        let hs = self.hs.clone().write_owned().await;
        for (id, terminal_state) in completed_ids {
            if let Err(e) = hs.mark_job_completed(id, terminal_state.as_deref()).await {
                log::warn!("failed to mark job {id} completed: {e}");
            }
        }
        Ok(())
    }
    /// Test if SessionManager object for hostid exists in the in-memory mapping
    async fn sessionmanager_exists(&self, hostid: &str) -> bool {
        return self.mgr.clone().read_owned().await.contains_key(hostid);
    }

    /// Logic for using SessionManager mapping is the following:
    /// 1. Check if a session for the given hostid already exists
    /// 2. If it does - all good, just return it
    /// 3 If it does not exist - go into the database, and check if connection credentials are known
    /// 4. If they are known - create connection, add it to mappping, return SessionManager
    /// 5.If credentials are unknown - return appropriate Error
    async fn get_sessionmanager(&self, hostid: &str) -> Result<Arc<SessionManager>, AgentSvcError> {
        if self.sessionmanager_exists(hostid).await {
            return Ok(self
                .mgr
                .clone()
                .read_owned()
                .await
                .get(hostid)
                .unwrap()
                .clone());
        } else {
            // 1. Check if hostid is known in the database
            let hs = self.hs.clone().read_owned().await;
            let maybe_hostrecord = match hs.get_by_hostid(hostid).await {
                Ok(v) => v,

                Err(e) => {
                    return Err(AgentSvcError::DatabaseError {
                        error: format!("database error: {}", e.to_string()),
                    });
                }
            };
            let Some(hr) = maybe_hostrecord else {
                return Err(AgentSvcError::UnknownHostId);
            };
            // After some housekeeping is done - we can finally create SessionManager and add it to
            // the sessionmanager storage

            let port = hr.port;
            let identity_path = hr.identity_path.clone();
            let connection_addr = match hr.address {
                crate::state::db::Address::Hostname(hn) => tokio::net::lookup_host((hn.clone(), port))
                    .await
                    .map_err(|e| {
                        return AgentSvcError::NetworkError(format!(
                            "failed to lookup address for hostid {0} (hostname={1}): {2}",
                            hostid,
                            &hn,
                            e.to_string()
                        ));
                    })?
                    .map(|v| std::net::SocketAddr::new(v.ip(), port))
                    .next()
                    .ok_or_else(|| {
                        AgentSvcError::NetworkError(format!(
                            "could not resolve {0} into a valid address: {}",
                            &hn
                        ))
                    })?,
                crate::state::db::Address::Ip(addr) => std::net::SocketAddr::new(addr, port),
            };
            let ssh_params = SshParams {
                addr: connection_addr,
                username: hr.username,
                identity_path,
                keepalive_secs: 60,
                ki_submethods: None,
            };
            let sm = SessionManager::new(ssh_params);
            // Since this function returns a SessionManager without any guarantees about
            // connection, actually connecting to the host should be handled externally.
            let sm_arc = Arc::new(sm);
            let mut mgr = self.mgr.clone().write_owned().await;
            mgr.insert(hostid.to_string(), sm_arc.clone());
            return Ok(sm_arc);
        }
    }
    async fn run_command(
        &self,
        command: String,
        hostid: &str,
        mfa_rx: tokio::sync::mpsc::Receiver<MfaAnswer>,
    ) -> Result<tonic::Response<OutStream>, Status> {
        // Outbound stream server -> client
        let (evt_tx, evt_rx) = tokio::sync::mpsc::channel::<Result<StreamEvent, Status>>(64);

        // get mgr from mapping
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
                    return Err(Status::internal(format!(
                        "unexpected error: {}",
                        other_error.to_string()
                    )));
                }
            },
        };
        let cmd = command;
        tokio::spawn(async move {
            if let Err(err) = mgr.exec(&cmd, evt_tx.clone(), mfa_rx).await {
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
    async fn list_partitions(&self) -> anyhow::Result<Vec<String>> {
        let res: Vec<String> = Vec::new();
        Ok(res)
    }
}

#[tonic::async_trait]
impl Agent for AgentSvc {
    type LsStream = OutStream;
    type RetrieveJobStream = OutStream;
    type SubmitStream = OutStream;
    type AddClusterStream = OutStream;
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
        request: tonic::Request<tonic::Streaming<LsRequest>>,
    ) -> Result<tonic::Response<Self::LsStream>, Status> {
        let mut inbound = request.into_inner();

        let init = inbound
            .message()
            .await
            .map_err(|e| Status::unknown(format!("read error: {e}")))?
            .ok_or_else(|| Status::invalid_argument("stream closed before init"))?;

        let (hostid, path) = match init.msg {
            Some(proto::ls_request::Msg::Init(LsRequestInit { hostid, path })) => {
                (hostid, path)
            }
            _ => return Err(Status::invalid_argument("first message must be init(hostid)")),
        };

        let (mfa_tx, mfa_rx) = tokio::sync::mpsc::channel::<MfaAnswer>(16);
        tokio::spawn(async move {
            while let Ok(Some(item)) = inbound.message().await {
                if let Some(proto::ls_request::Msg::Mfa(ans)) = item.msg {
                    if mfa_tx.send(ans).await.is_err() {
                        break;
                    }
                }
            }
        });

        let hs = self.hs.clone().read_owned().await;
        let list_path = match path {
            Some(v) => {
                if v.is_empty() {
                    return Err(Status::invalid_argument(
                        "path can't be empty: provide a path or omit it completely",
                    ));
                }
                if std::path::PathBuf::from(&v).is_absolute() {
                    normalize_path(v).to_string_lossy().into_owned()
                } else {
                    let default_base_path = get_default_base_path(&hs, &hostid).await?;
                    let base_path = std::path::PathBuf::from(default_base_path);
                    util::remote_path::resolve_relative(base_path, v)
                        .to_string_lossy()
                        .into_owned()
                }
            }
            None => normalize_path(get_default_base_path(&hs, &hostid).await?)
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
                if let Some(proto::retrieve_job_request::Msg::Mfa(ans)) = item.msg {
                    if mfa_tx.send(ans).await.is_err() {
                        break;
                    }
                }
            }
        });

        let (evt_tx, evt_rx) = tokio::sync::mpsc::channel::<Result<StreamEvent, Status>>(64);
        let hs = self.hs.clone();
        let path_is_absolute = std::path::PathBuf::from(&path).is_absolute();
        let svc = self.clone();

        tokio::spawn(async move {
            let (hostid, run_path) = {
                let hs = hs.read_owned().await;
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
                if let Some(expected) = hostid.as_deref() {
                    if expected != job.host_id {
                        let _ = evt_tx
                            .send(Ok(StreamEvent {
                                event: Some(stream_event::Event::Error(format!(
                                    "job id {job_id} not found in cluster '{expected}'",
                                ))),
                            }))
                            .await;
                        return;
                    }
                }
                (job.host_id.clone(), job.remote_path.clone())
            };

            let mgr = match svc.get_sessionmanager(&hostid).await {
                Ok(v) => v,
                Err(e) => {
                    let message = match e {
                        AgentSvcError::UnknownHostId => format!("unknown hostid {hostid}"),
                        AgentSvcError::NetworkError(e) => format!("network error: {e}"),
                        other_error => {
                            format!("unexpected error: {}", other_error.to_string())
                        }
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

            let mut local_base = std::path::PathBuf::from(local_path);
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

        let out: OutStream = Box::pin(receiver_to_stream(evt_rx));
        Ok(tonic::Response::new(out))
    }
    async fn list_clusters(
        &self,
        request: tonic::Request<ListClustersRequest>,
    ) -> Result<tonic::Response<ListClustersResponse>, Status> {
        log::info!("listing clusters");
        let mut inbound = request.into_inner();
        // TODO: implement filter feature
        let hs = self.hs.clone().read_owned().await;
        let hosts = match hs.list_hosts(None).await {
            Ok(v) => v,
            Err(e) => match e {
                HostStoreError::Sqlx(sql_err) => {
                    log::error!("unknown error at sqlx level: {}", sql_err.to_string());
                    return Err(Status::internal(
                        "internal error: please report this  error along with daemon logs",
                    ));
                }
                other_error => {
                    log::error!(
                        "internal error: unexpected error: {}",
                        other_error.to_string()
                    );
                    return Err(Status::internal(
                        "internal error: please report this error along with daemon logs",
                    ));
                }
            },
        };
        let mut clusters: Vec<ListClustersUnitResponse> = hosts
            .iter()
            .map(|x| db_host_record_to_api_unit_response(x))
            .collect();
        let mgr_inner = self.mgr.clone().write_owned().await;
        for cluster in clusters.iter_mut() {
            // mapping contains hostid - test if it is connected
            match mgr_inner.get(&cluster.hostid) {
                Some(ssh_mgr) => {
                    if !(ssh_mgr.needs_connect().await) {
                        cluster.connected = true;
                    }
                }
                None => {}
            }
        }
        return Ok(ListClustersResponse { clusters: clusters }.into());
    }

    /// Submit job to a cluster. the core of this app.
    async fn submit(
        &self,
        request: tonic::Request<tonic::Streaming<SubmitRequest>>,
    ) -> Result<tonic::Response<Self::SubmitStream>, Status> {
        log::info!("submit request");
        // Setting up inbound stream (client -> server) carrying data or MFA answers from client to mfa_tx
        let mut inbound = request.into_inner();

        // Spawning a thread to handle the incoming data
        let init = inbound
            .message()
            .await
            .map_err(|e| Status::unknown(format!("read error: {e}")))?
            .ok_or_else(|| Status::invalid_argument("stream closed before init"))?;

        let (local_path, remote_path, hostid, sbatchscript, filters) = match init.msg {
            Some(proto::submit_request::Msg::Init(i)) => {
                (
                    i.local_path,
                    i.remote_path,
                    i.hostid,
                    i.sbatchscript,
                    i.filters,
                )
            }
            _ => return Err(Status::invalid_argument("first message must be init(path)")),
        };
        let filters = build_sync_filters(filters)?;

        // Establish communication queues
        let (evt_tx, evt_rx) = tokio::sync::mpsc::channel::<Result<StreamEvent, Status>>(64);
        let (mfa_tx, mut mfa_rx) = tokio::sync::mpsc::channel::<MfaAnswer>(16);
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

        tokio::spawn(async move {
            while let Ok(Some(item)) = inbound.message().await {
                if let Some(proto::submit_request::Msg::Mfa(ans)) = item.msg {
                    if mfa_tx.send(ans).await.is_err() {
                        break;
                    }
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
                    return Err(Status::internal(format!(
                        "unexpected error: {}",
                        other_error.to_string()
                    )));
                }
            },
        };

        let hs = self.hs.clone().write_owned().await;

        // REMOTE PATH handling logic
        // If remote_path is provided and is absolute -  just return it;
        // If provided and is relative - query default_base_path
        // If not provided - query default_base_path,
        //      if it is logged for this cluster in the database -
        //      randomize directory name and deploy to the random subdirectory of default_base_path.
        let remote_path: String = match remote_path {
            Some(v) => {
                if v.is_empty() {
                    return Err(Status::invalid_argument(
                        "remote path can't be empty: either provide non-empty remote path or omit it completely",
                    ));
                }
                if std::path::PathBuf::from(&v).is_absolute() {
                    //If v is absolute - just use it
                    v
                } else {
                    // If v is not absolute -> it must be relative and thus requires default_base_path
                    let default_base_path = get_default_base_path(&hs, &hostid).await?;
                    let base_path = std::path::PathBuf::from(default_base_path);
                    // resolve_relative instead of just join because remote_path might be not normalized
                    util::remote_path::resolve_relative(base_path, v)
                        .to_string_lossy()
                        .into_owned()
                }
            }
            None => {
                // get host metadata;
                let host_data = match hs.get_by_hostid(&hostid).await {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        return Err(Status::invalid_argument(format!(
                            "remote path is not provided and default_base_path for {} is not set",
                            &hostid
                        )));
                    }
                    Err(e) => {
                        return Err(Status::invalid_argument(format!(
                            "could not retrieve default_base_path for {} from app's db: {}",
                            &hostid,
                            e.to_string()
                        )));
                    }
                };
                let default_base_path = get_default_base_path(&hs, &hostid).await?;
                let random_str = util::random::generate_run_directory_name();
                let base_path = std::path::PathBuf::from(default_base_path);

                base_path.join(random_str).to_string_lossy().into_owned()
            }
        };

        if evt_tx
            .send(Ok(StreamEvent {
                event: Some(stream_event::Event::SubmitStatus(SubmitStatus {
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

        // Pipe the remaining client messages (if any) into MFA answers
        match mgr.ensure_connected(&evt_tx.clone(), &mut mfa_rx).await {
            Ok(_) => {}
            Err(e) => {
                return Err(Status::internal(format!(
                    "could not establish connection to {}: {}",
                    &hostid,
                    e.to_string()
                )));
            }
        };

        let remote_path_exists = match mgr.directory_exists(&remote_path).await {
            Ok(v) => v,
            Err(e) => {
                return Err(Status::internal(format!(
                    "can't list {} on {}: {}",
                    &remote_path,
                    &hostid,
                    e.to_string()
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
        tokio::spawn(async move {
            if evt_tx
                .send(Ok(StreamEvent {
                    event: Some(stream_event::Event::SubmitStatus(SubmitStatus {
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
            //1. Sync data
            let sync_result = tokio::select! {
                res = mgr.sync_dir(
                    &local_path,
                    &remote_path,
                    Some(1024 * 1024), // TODO: this should be adjustable, and done per-file. Probably sqrt(file size in bytes) will be a good start.
                    None,
                    &filters,
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
                    .send(Ok(StreamEvent {
                        event: Some(stream_event::Event::Error(err.to_string())),
                    }))
                    .await;
                return;
            };
            if evt_tx
                .send(Ok(StreamEvent {
                    event: Some(stream_event::Event::SubmitStatus(SubmitStatus {
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
            // 2. figure out remote path to sbatch script
            let remote_sbatch_script_path =
                util::remote_path::resolve_relative(&remote_path, sbatchscript)
                    .to_string_lossy()
                    .into_owned();

            let sbatch_command =
                slurm::path_to_sbatch_command(&remote_sbatch_script_path, Some(&remote_path));
            log::debug!("running remote script {}", &remote_sbatch_script_path);
            // 3. submit the job
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
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Error(e.to_string())),
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
                    .send(Ok(StreamEvent {
                        event: Some(stream_event::Event::Error(format!(
                            "sbatch failed with exit code {}: {}",
                            code, detail
                        ))),
                    }))
                    .await;
                return;
            }
            let outString = String::from_utf8_lossy(&out);
            let jobId = slurm::parse_job_id(&outString);

            let Ok(Some(hr)) = hs.get_by_hostid(&hostid).await else {
                let _ = evt_tx
                    .send(Ok(StreamEvent {
                        event: Some(stream_event::Event::Error(format!(
                            "Could not resolve hostid '{}'",
                            hostid
                        ))),
                    }))
                    .await;
                return;
            };

            let nj = NewJob {
                slurm_id: jobId,
                host_id: hr.id,
                local_path: local_path,
                remote_path: remote_path,
            };
            match hs.insert_job(&nj).await {
                Ok(job_id) => match jobId {
                    Some(v) => {
                        let _ = evt_tx
                                .send(Ok(StreamEvent {
                                    event: Some(stream_event::Event::Stdout(
                                        format!("Successfully submitted sbatch script with slurm id {} , job id {}", v, job_id)
                                            .into(),
                                    )),
                                }))
                                .await;
                    }
                    None => {
                        let _ = evt_tx
                                .send(Ok(StreamEvent {
                                    event: Some(stream_event::Event::Stdout(
                                        format!("Successfully submitted sbatch script, did not receive a valid slurm id from system; job id {}", job_id).into()
                                    )),
                                }))
                                .await;
                    }
                },
                Err(e) => {
                    let _ = evt_tx
                            .send(Ok(StreamEvent {
                                event: Some(stream_event::Event::Error(
                                    format!("Successfully submitted sbatch script with slurm id {:?}, failed to create job record: {}", jobId, e)
                                )),
                            }))
                            .await;
                }
            }
        });

        let out: OutStream = Box::pin(receiver_to_stream(evt_rx));
        Ok(tonic::Response::new(out))
    }

    /// 1. Create SessionManager, try to connect and gather information.
    /// 2. If managed to connect and gather the appropriate information - upsert it in the database
    ///    and return OK; otherwise - return appropriate error.
    async fn add_cluster(
        &self,
        request: tonic::Request<tonic::Streaming<AddClusterRequest>>,
    ) -> Result<tonic::Response<Self::AddClusterStream>, Status> {
        log::debug!("adding cluster");

        let mut inbound = request.into_inner();

        // Spawning a thread to handle the incoming data
        let init = inbound
            .message()
            .await
            .map_err(|e| Status::unknown(format!("read error: {e}")))?
            .ok_or_else(|| Status::invalid_argument("stream closed before init"))?;
        let (username, host, hostid, identity_path, port, mut default_base_path) = match init.msg {
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
        let host = match host {
            Some(v) => v,
            None => return Err(Status::invalid_argument("empty host in initial message")),
        };
        let addr = match host {
            proto::add_cluster_init::Host::Hostname(v) => crate::state::db::Address::Hostname(v),
            proto::add_cluster_init::Host::Ipaddr(addr) => {
                let ip: IpAddr = match addr.parse() {
                    Ok(v) => v,
                    Err(e) => {
                        return Err(Status::invalid_argument(format!(
                            "could not parse {} into ip address: {:?}",
                            addr, e
                        )));
                    }
                };
                crate::state::db::Address::Ip(ip)
            }
        };
        log::info!(
            "adding cluster (hostid={},username={},address={:?})",
            &hostid,
            &username,
            &addr
        );
        let (evt_tx, evt_rx) = tokio::sync::mpsc::channel::<Result<StreamEvent, Status>>(64);

        // Pipe the remaining client messages (if any) into MFA answers
        let (mfa_tx, mut mfa_rx) = tokio::sync::mpsc::channel::<MfaAnswer>(16);
        tokio::spawn(async move {
            while let Ok(Some(item)) = inbound.message().await {
                if let Some(proto::add_cluster_request::Msg::Mfa(ans)) = item.msg {
                    if mfa_tx.send(ans).await.is_err() {
                        break;
                    }
                }
            }
        });

        let port = match u16::try_from(port) {
            Ok(v) => v,
            Err(e) => {
                log::debug!("could not case u32 port to u16 port: {}", e.to_string());
                return Err(Status::invalid_argument(format!(
                    "invalid port value: {port}"
                )));
            }
        };

        let connection_addr = match addr {
            crate::state::db::Address::Ip(v) => (v, port).into(),
            crate::state::db::Address::Hostname(ref hostname) => {
                match util::net::lookup_first_addr(hostname, port).await {
                    Ok(v) => v,
                    Err(e) => match e {
                        util::net::NetError::DnsNotFound(_) => {
                            return Err(Status::invalid_argument(format!(
                                "hostname {hostname} could not be resolved"
                            )));
                        }
                        util::net::NetError::NoAddrs(_) => {
                            return Err(Status::invalid_argument(format!(
                                "couldn't find any IP addresses for {hostname}"
                            )));
                        }
                        util::net::NetError::Resolve(h) => {
                            return Err(Status::internal(format!(
                                "encountered error when resolving {hostname}: {}",
                                h.to_string()
                            )));
                        }
                    },
                }
            }
        };
        let ssh_params = SshParams {
            username: username.to_string(),
            addr: connection_addr,
            identity_path: identity_path.clone(),
            keepalive_secs: 60,
            ki_submethods: None,
        };
        let hs = self.hs.clone().write_owned().await;
        let mut mgr = self.mgr.clone().write_owned().await;
        tokio::spawn(async move {
            let sm = SessionManager::new(ssh_params);
            if let Err(e) = sm.ensure_connected(&evt_tx, &mut mfa_rx).await {
                let _ = evt_tx
                    .send(Err(Status::aborted(format!(
                        "failed to connect to {hostid}: {}",
                        e.to_string()
                    ))))
                    .await;
                return;
            };

            // Determine workload manager
            let (out, err, code) = match sm
                .exec_capture(managers::DETERMINE_HPC_WORKLOAD_MANAGERS_CMD)
                .await
            {
                Ok((vo, ve, ec)) => (vo, ve, ec),
                Err(e) => {
                    let _ = evt_tx
                        .send(Err(Status::aborted(format!(
                            "failed to gather cluster metadata for {hostid}: {}",
                            e.to_string()
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
                        "failed to gather cluster metadata for {hostid}: could not decode the gathered output: {}",
                        e.to_string()
                    ))))
                    .await;
                    return;
                }
            };

            let wlms = managers::parse_wlms(&out);
            // TODO: add support for other WLMs
            if !wlms.contains(&managers::WorkloadManager::Slurm) {
                let _ = evt_tx
                    .send(Err(Status::aborted(format!(
                        "no supported workload managers found on {hostid}; identified workload managers: {:?}",
                        wlms
                    ))))
                    .await;
                return;
            }
            // Server is connected and we have Slurm on it - now let's gather some facts about it
            // and create a database record
            let (out, err, code) = match sm.exec_capture(os::GATHER_OS_INFO_CMD).await {
                Ok((vo, ve, ec)) => (vo, ve, ec),
                Err(e) => {
                    let _ = evt_tx
                        .send(Err(Status::aborted(format!(
                            "failed to gather cluster os metadata for {hostid}: {}",
                            e.to_string()
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
                        "failed to gather cluster os metadata for {hostid}: could not decode the gathered output: {}",
                        e.to_string()
                    ))))
                    .await;
                    return;
                }
            };

            let os_info = match os::parse_distro_info(&out) {
                Ok(v) => v,
                Err(e) => {
                    let _ = evt_tx
                    .send(Err(Status::aborted(format!(
                        "failed to gather cluster os metadata for {hostid}: could not parse the gathered output: {}",
                        e.to_string()
                    ))))
                    .await;
                    return;
                }
            };
            let distro_info = crate::state::db::Distro {
                name: os_info.id,
                version: os_info.version,
            };

            let (out, err, code) = match sm.exec_capture(slurm::DETERMINE_SLURM_VERSION_CMD).await {
                Ok((vo, ve, ec)) => (vo, ve, ec),
                Err(e) => {
                    let _ = evt_tx
                        .send(Err(Status::aborted(format!(
                            "failed to gather slurm version for {hostid}: {}",
                            e.to_string()
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
                        "failed to gather slurm version for {hostid}: could not decode the gathered output: {}",
                        e.to_string()
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
                            "failed to gather cluster config for {hostid}: {}",
                            e.to_string()
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
            let accounting_enabled = slurm::parse_accounting_enabled_from_scontrol(&config)
                .unwrap_or_else(|| {
                    log::warn!(
                        "unable to determine accounting storage type for {hostid}, assuming disabled"
                    );
                    false
                });

            let normalized_default_base_path =
                default_base_path.map(|v| normalize_path(v).to_owned());
            // if default_base_path variable is not none - ensure it is valid and create it
            if let Some(ref dbp) = normalized_default_base_path {
                if dbp.is_absolute() {
                    let command = format!("mkdir -p {}", dbp.to_string_lossy());
                    let (_, err, code) = match sm.exec_capture(&command).await {
                        Ok((vo, ve, ec)) => (vo, ve, ec),
                        Err(e) => {
                            let _ = evt_tx
                                .send(Err(Status::internal(format!(
                                    "failed to execute command `{}` on {}: {}",
                                    command,
                                    hostid,
                                    e.to_string()
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
                } else {
                    let _ = evt_tx
                        .send(Err(Status::invalid_argument(format!(
                            "default_base_path must be absolute, got: '{}'",
                            dbp.to_string_lossy()
                        ))))
                        .await;
                    return;
                }
            }

            let new_host = crate::state::db::NewHost {
                username: username,
                hostid: hostid.clone(),
                address: addr.clone(),
                distro: distro_info,
                kernel_version: os_info.kernel,
                slurm: slurm_version,
                port: port,
                identity_path: identity_path,
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

            mgr.insert(hostid.clone(), Arc::new(sm));
        });
        let out: OutStream = Box::pin(receiver_to_stream(evt_rx));
        Ok(tonic::Response::new(out))
    }

    async fn list_jobs(
        &self,
        request: tonic::Request<ListJobsRequest>,
    ) -> Result<tonic::Response<ListJobsResponse>, Status> {
        log::info!("listing clusters");
        let mut inbound = request.into_inner();

        let hs = self.hs.clone().read_owned().await;
        let jobs = match inbound.hostid {
            Some(ref v) => {
                // 1. resolve integer id
                let Ok(Some(hr)) = hs.get_by_hostid(v).await else {
                    return Err(Status::invalid_argument(format!("hostid {} is unknown", v)));
                };

                match hs.list_jobs_for_host(hr.id).await {
                    Ok(v) => v,
                    Err(e) => {
                        return Err(Status::internal(format!(
                            "couldn't list jobs for host '{}': {}",
                            hr.hostid, e
                        )));
                    }
                }
            }
            None => match hs.list_all_jobs().await {
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
            .map(|jr: JobRecord| db_job_record_to_api_unit_response(&jr))
            .collect();
        return Ok(tonic::Response::new(ListJobsResponse { jobs: api_jobs }));
    }
}

fn build_sync_filters(
    filters: Vec<SubmitPathFilterRule>,
) -> Result<Vec<SyncFilterRule>, Status> {
    let mut out = Vec::with_capacity(filters.len());
    for rule in filters {
        let action = match SubmitPathFilterAction::from_i32(rule.action) {
            Some(SubmitPathFilterAction::Include) => SyncFilterAction::Include,
            Some(SubmitPathFilterAction::Exclude) => SyncFilterAction::Exclude,
            _ => {
                return Err(Status::invalid_argument(
                    "submit filter action must be include or exclude",
                ));
            }
        };
        if rule.pattern.trim().is_empty() {
            return Err(Status::invalid_argument(
                "submit filter pattern cannot be empty",
            ));
        }
        out.push(SyncFilterRule {
            action,
            pattern: rule.pattern,
        });
    }
    Ok(out)
}

async fn get_default_base_path(hs: &HostStore, hid: &str) -> Result<String, Status> {
    // TODO: return domain errors and covert
    let host_data = match hs.get_by_hostid(hid).await {
        Ok(Some(v)) => v,
        Ok(None) => {
            return Err(Status::invalid_argument(format!(
                "hostid {} is unknown",
                hid
            )));
        }
        Err(e) => {
            return Err(Status::internal(format!(
                "could not retrieve default_base_path for {} from app's db: {}",
                hid,
                e.to_string()
            )));
        }
    };
    match host_data.default_base_path {
        Some(v) => Ok(v),
        None => Err(Status::invalid_argument(format!(
            "default_base_path for {} is not set",
            hid,
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::db::NewHost;

    fn make_host(hostid: &str, username: &str, addr: crate::state::db::Address) -> NewHost {
        NewHost {
            hostid: hostid.into(),
            username: username.into(),
            address: addr,
            port: 2222,
            identity_path: Some("/tmp/test_id_ed25519".to_string()),
            slurm: SlurmVersion {
                major: 23,
                minor: 11,
                patch: 5,
            },
            distro: crate::state::db::Distro {
                name: "ubuntu".into(),
                version: "22.04".into(),
            },
            kernel_version: "6.5.0-41-generic".into(),
            accounting_available: true,
            default_base_path: Some("/tmp/runs".into()),
        }
    }

    #[tokio::test]
    async fn get_sessionmanager_uses_stored_port_and_identity() {
        let hs = HostStore::open_memory().await.unwrap();
        let addr = crate::state::db::Address::Ip("127.0.0.1".parse().unwrap());
        let host = make_host("host-a", "alice", addr);
        hs.insert_host(&host).await.unwrap();

        let svc = AgentSvc::new(hs);
        let sm = svc.get_sessionmanager("host-a").await.unwrap();
        let params = sm.test_params();
        assert_eq!(params.addr.port(), 2222);
        assert_eq!(
            params.identity_path.as_deref(),
            Some("/tmp/test_id_ed25519")
        );
        assert_eq!(params.username, "alice");
    }
}

fn db_host_record_to_api_unit_response(hs: &HostRecord) -> ListClustersUnitResponse {
    let rp = ListClustersUnitResponse {
        username: hs.username.clone(),
        identity_path: hs.identity_path.to_owned(),
        host: match hs.address {
            crate::state::db::Address::Ip(ref ip) => {
                Some(list_clusters_unit_response::Host::Ipaddr(ip.to_string()))
            }
            crate::state::db::Address::Hostname(ref hostname) => Some(
                list_clusters_unit_response::Host::Hostname(hostname.to_owned()),
            ),
        },
        port: hs.port as i32,
        connected: false,
        hostid: hs.hostid.to_owned(),
        accounting_available: hs.accounting_available,
    };
    return rp;
}

fn db_job_record_to_api_unit_response(jr: &JobRecord) -> ListJobsUnitResponse {
    let rp = ListJobsUnitResponse {
        hostid: jr.host_id.clone(),
        job_id: jr.id,
        slurm_id: jr.slurm_id,
        created_at: jr.created_at.clone(),
        finished_at: jr.finished_at.clone(),
        is_completed: jr.is_completed,
        terminal_state: jr.terminal_state.clone(),
    };
    return rp;
}
