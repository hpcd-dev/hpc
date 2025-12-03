use crate::ssh::SessionManager;
use crate::ssh::SshParams;
use crate::ssh::receiver_to_stream;
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
    AddClusterRequest, ListJobsRequest, ListJobsResponse, ListJobsUnitResponse, MfaAnswer,
    MfaPrompt, PingReply, PingRequest, StreamEvent, SubmitRequest, agent_client, stream_event,
};
use std::any::Any;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use thiserror::Error as ThisError;
use tokio::sync::Mutex;
use tokio::sync::RwLock;
use tokio_stream::{Stream, StreamExt};
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

            // TODO: port and identity path should be stored in the database
            let port = 22;
            let identity_path = "/Users/alexsizykh/.ssh/id_ed25519".to_string();
            let connection_addr = match hr.address {
                crate::state::db::Address::Hostname(hn) => tokio::net::lookup_host((hn.clone(), 0))
                    .await
                    .map_err(|e| {
                        return AgentSvcError::NetworkError(format!(
                            "failed to lookup address for hostid {0} (hostname={1}): {2}",
                            hostid,
                            &hn,
                            e.to_string()
                        ));
                    })?
                    .map(|v| std::net::SocketAddr::new(v.ip(), 22))
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
                identity_path: Some(identity_path),
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
        command: &str,
        hostid: &str,
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
        let cmd = command.to_string();
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
        request: tonic::Request<tonic::Streaming<MfaAnswer>>,
    ) -> Result<tonic::Response<Self::LsStream>, Status> {
        self.run_command("ls", "winery", request).await
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
        let (local_path, remote_path, hostid, sbatchscript) = match init.msg {
            Some(proto::submit_request::Msg::Init(i)) => {
                (i.local_path, i.remote_path, i.hostid, i.sbatchscript)
            }
            _ => return Err(Status::invalid_argument("first message must be init(path)")),
        };
        let (evt_tx, evt_rx) = tokio::sync::mpsc::channel::<Result<StreamEvent, Status>>(64);
        let (mfa_tx, mfa_rx) = tokio::sync::mpsc::channel::<MfaAnswer>(16);
        tokio::spawn(async move {
            while let Ok(Some(item)) = inbound.message().await {
                if let Some(proto::submit_request::Msg::Mfa(ans)) = item.msg {
                    if mfa_tx.send(ans).await.is_err() {
                        break;
                    }
                }
            }
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
        // If remote_path is provided and is absolute -  just return it;
        // If provided and is relative - query default_base_path
        // If not provided - query default_base_path,
        //      if it is logged for this cluster in the database -
        //      randomize directory name and deploy to the random subdirectory of default_base_path.
        let remote_path: String = match remote_path {
            Some(v) => v,
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
                match host_data.default_base_path {
                    Some(v) => {
                        let base_path = std::path::PathBuf::from(v);
                        let random_str = Uuid::new_v4().to_string();
                        base_path.join(random_str).to_string_lossy().into_owned()
                    }
                    None => {
                        return Err(Status::invalid_argument(format!(
                            "remote_path is not provided and default_base_path is not set for {}",
                            &hostid,
                        )));
                    }
                }
            }
        };
        log::debug!(
            "transfering data from {} to {:?}",
            &local_path,
            &remote_path
        );

        // Pipe the remaining client messages (if any) into MFA answers

        tokio::spawn(async move {
            //1. Sync data
            if let Err(err) = mgr
                .sync_dir(
                    &local_path,
                    &remote_path,
                    Some(1024 * 1024), // TODO: this should be adjustable, and done per-file. Probably sqrt(file size in bytes) will be a good start.
                    None,
                    &evt_tx.clone(),
                    mfa_rx,
                )
                .await
            {
                let _ = evt_tx
                    .send(Ok(StreamEvent {
                        event: Some(stream_event::Event::Error(err.to_string())),
                    }))
                    .await;
            };
            // 2. figure out remote path to sbatch script
            let remote_sbatch_script_path =
                util::remote_path::resolve_relative(&remote_path, sbatchscript)
                    .to_string_lossy()
                    .into_owned();

            let sbatch_command =
                slurm::path_to_sbatch_command(&remote_sbatch_script_path, Some(&remote_path));
            log::debug!("running remote script {}", &remote_sbatch_script_path);
            // 3. submit the job
            let (out, err, code) = match mgr.exec_capture(&sbatch_command).await {
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

            log::debug!(
                "submitted remote script, received from sbatch code {}, error message: {}",
                code,
                String::from_utf8(err).unwrap()
            );
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
                job_id: jobId,
                host_id: hr.id,
                local_path: local_path,
                remote_path: remote_path,
            };
            match hs.insert_job(&nj).await {
                Ok(internal_job_id) => match jobId {
                    Some(v) => {
                        let _ = evt_tx
                                .send(Ok(StreamEvent {
                                    event: Some(stream_event::Event::Stdout(
                                        format!("Successfully submitted sbatch script with job id {} , internal job id {}", v, internal_job_id)
                                            .into(),
                                    )),
                                }))
                                .await;
                    }
                    None => {
                        let _ = evt_tx
                                .send(Ok(StreamEvent {
                                    event: Some(stream_event::Event::Stdout(
                                        format!("Successfully submitted sbatch script, did not receive a vaild job id from system; internal job id {}", internal_job_id).into()
                                    )),
                                }))
                                .await;
                    }
                },
                Err(e) => {
                    let _ = evt_tx
                            .send(Ok(StreamEvent {
                                event: Some(stream_event::Event::Error(
                                    format!("Successfully submitted sbatch script with job id {:?}, failed to create internal record: {}", jobId, e)
                                )),
                            }))
                            .await;
                }
            }
        });

        let out: OutStream = Box::pin(receiver_to_stream(evt_rx));
        Ok(tonic::Response::new(out))
    }

    /// 1. Test if cluster already exists; if it does - return error indicating that cluster exists
    /// 2. If cluster doesn't exist - create corresponding SessionManager, try to connect and gather
    ///    information
    /// 3. If managed to connect and gather the appropriate information - add it to the database and
    ///    return OK; otherwise - return appropriate error
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
            let (_, err, code) = match sm.exec_capture("sacct 1>/dev/null 2>&1").await {
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
            let accounting_enabled = matches!(code, 0);

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
            match hs.insert_host(&new_host).await {
                Ok(v) => {
                    log::debug!("successfully inserted host with id {v}")
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

#[cfg(test)]
mod tests {
    use super::*;
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
    };
    return rp;
}

fn db_job_record_to_api_unit_response(jr: &JobRecord) -> ListJobsUnitResponse {
    let rp = ListJobsUnitResponse {
        hostid: jr.host_id.clone(),
        internal_job_id: jr.id,
        job_id: jr.job_id,
        created_at: jr.created_at.clone(),
        finished_at: jr.finished_at.clone(),
        is_completed: jr.is_completed,
    };
    return rp;
}
