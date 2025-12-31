use crate::agent::sessions::{DefaultSessionFactory, SessionCache, SessionFactory};
use crate::agent::types::{AgentSvcError, OutStream};
use crate::ssh::SessionManager;
use crate::ssh::receiver_to_stream;
use crate::state::db::{HostStore, JobRecord};
use proto::stream_event;
use proto::{MfaAnswer, StreamEvent};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::Duration;
use tonic::Status;

#[derive(Clone)]
pub struct AgentSvc {
    sessions: Arc<SessionCache>,
    hosts: Arc<HostStore>,
}

impl AgentSvc {
    pub fn new(hs: HostStore) -> Self {
        Self::with_factory(hs, Arc::new(DefaultSessionFactory))
    }

    pub fn with_factory(hs: HostStore, factory: Arc<dyn SessionFactory>) -> Self {
        Self {
            sessions: Arc::new(SessionCache::new(factory)),
            hosts: Arc::new(hs),
        }
    }

    pub fn hosts(&self) -> Arc<HostStore> {
        self.hosts.clone()
    }

    pub fn sessions(&self) -> Arc<SessionCache> {
        self.sessions.clone()
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

    pub async fn check_running_jobs(&self) -> anyhow::Result<()> {
        let jobs = self.hosts.list_running_jobs().await?;
        if jobs.is_empty() {
            return Ok(());
        }
        let hosts = self.hosts.list_hosts(None).await?;

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
                    let terminal_state = match crate::agent::slurm::sacct_terminal_state(&output) {
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

        for (id, terminal_state) in completed_ids {
            if let Err(e) = self
                .hosts
                .mark_job_completed(id, terminal_state.as_deref())
                .await
            {
                log::warn!("failed to mark job {id} completed: {e}");
            }
        }
        Ok(())
    }

    pub async fn get_sessionmanager(
        &self,
        hostid: &str,
    ) -> Result<Arc<SessionManager>, AgentSvcError> {
        if let Some(existing) = self.sessions.get(hostid).await {
            return Ok(existing);
        }

        let maybe_hostrecord = match self.hosts.get_by_hostid(hostid).await {
            Ok(v) => v,
            Err(e) => {
                return Err(AgentSvcError::DatabaseError {
                    error: format!("database error: {}", e),
                });
            }
        };
        let Some(host) = maybe_hostrecord else {
            return Err(AgentSvcError::UnknownHostId);
        };
        self.sessions.get_or_create(hostid, &host).await
    }

    pub async fn run_command(
        &self,
        command: String,
        hostid: &str,
        mfa_rx: tokio::sync::mpsc::Receiver<MfaAnswer>,
    ) -> Result<tonic::Response<OutStream>, Status> {
        let (evt_tx, evt_rx) = tokio::sync::mpsc::channel::<Result<StreamEvent, Status>>(64);
        let mgr = match self.get_sessionmanager(hostid).await {
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
                        other_error
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::db::{Address, NewHost};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecordingFactory {
        params: Arc<Mutex<Vec<crate::ssh::SshParams>>>,
    }

    impl RecordingFactory {
        fn recorded(&self) -> Vec<crate::ssh::SshParams> {
            self.params.lock().unwrap().clone()
        }
    }

    impl SessionFactory for RecordingFactory {
        fn build(&self, params: crate::ssh::SshParams) -> Arc<crate::ssh::SessionManager> {
            self.params.lock().unwrap().push(params.clone());
            Arc::new(crate::ssh::SessionManager::new(params))
        }
    }

    fn make_host(hostid: &str, username: &str, addr: Address) -> NewHost {
        NewHost {
            hostid: hostid.into(),
            username: username.into(),
            address: addr,
            port: 2222,
            identity_path: Some("/tmp/test_id_ed25519".to_string()),
            slurm: crate::state::db::SlurmVersion {
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
        let addr = Address::Ip("127.0.0.1".parse().unwrap());
        let host = make_host("host-a", "alice", addr);
        hs.insert_host(&host).await.unwrap();

        let factory = Arc::new(RecordingFactory::default());
        let svc = AgentSvc::with_factory(hs, factory.clone());
        let _sm = svc.get_sessionmanager("host-a").await.unwrap();

        let recorded = factory.recorded();
        assert_eq!(recorded.len(), 1);
        let params = &recorded[0];
        assert_eq!(params.addr.port(), 2222);
        assert_eq!(
            params.identity_path.as_deref(),
            Some("/tmp/test_id_ed25519")
        );
        assert_eq!(params.username, "alice");
    }
}
