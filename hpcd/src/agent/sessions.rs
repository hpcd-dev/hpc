// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use crate::agent::types::AgentSvcError;
use crate::ssh::{SessionManager, SshParams};
use crate::state::db::{Address, HostRecord};
use crate::util::net;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;

pub trait SessionFactory: Send + Sync {
    fn build(&self, params: SshParams) -> Arc<SessionManager>;
}

#[derive(Default)]
pub struct DefaultSessionFactory;

impl SessionFactory for DefaultSessionFactory {
    fn build(&self, params: SshParams) -> Arc<SessionManager> {
        Arc::new(SessionManager::new(params))
    }
}

pub struct SessionCache {
    sessions: RwLock<HashMap<String, Arc<SessionManager>>>,
    factory: Arc<dyn SessionFactory>,
}

impl SessionCache {
    pub fn new(factory: Arc<dyn SessionFactory>) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            factory,
        }
    }

    pub async fn get(&self, name: &str) -> Option<Arc<SessionManager>> {
        self.sessions.read().await.get(name).cloned()
    }

    pub async fn insert(&self, name: String, session: Arc<SessionManager>) {
        self.sessions.write().await.insert(name, session);
    }

    pub async fn remove_and_shutdown(&self, name: &str) -> bool {
        let session = self.sessions.write().await.remove(name);
        if let Some(session) = session {
            session.shutdown().await;
            return true;
        }
        false
    }

    pub async fn is_connected(&self, name: &str) -> bool {
        let Some(session) = self.get(name).await else {
            return false;
        };
        !session.needs_connect().await
    }

    pub async fn get_or_create(
        &self,
        name: &str,
        host: &HostRecord,
    ) -> Result<Arc<SessionManager>, AgentSvcError> {
        if let Some(existing) = self.get(name).await {
            return Ok(existing);
        }

        let connection_addr = resolve_host_addr(&host.address, host.port, name).await?;
        let ssh_params = SshParams {
            addr: connection_addr,
            username: host.username.clone(),
            identity_path: host.identity_path.clone(),
            keepalive_secs: 60,
            ki_submethods: None,
        };
        let session = self.factory.build(ssh_params);
        self.insert(name.to_string(), session.clone()).await;
        Ok(session)
    }
}

async fn resolve_host_addr(
    address: &Address,
    port: u16,
    name: &str,
) -> Result<SocketAddr, AgentSvcError> {
    match address {
        Address::Ip(addr) => Ok(SocketAddr::new(*addr, port)),
        Address::Hostname(hostname) => net::lookup_first_addr(hostname, port).await.map_err(|e| {
            AgentSvcError::NetworkError(format!(
                "failed to lookup address for name {name} (hostname={hostname}): {e}",
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionCache, SessionFactory};
    use crate::state::db::{Address, HostRecord};
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

    #[tokio::test]
    async fn get_or_create_records_params() {
        let factory = Arc::new(RecordingFactory::default());
        let cache = SessionCache::new(factory.clone());

        let host = HostRecord {
            id: 1,
            name: "host-a".to_string(),
            username: "alice".to_string(),
            address: Address::Ip("127.0.0.1".parse().unwrap()),
            distro: crate::state::db::Distro {
                name: "ubuntu".into(),
                version: "22.04".into(),
            },
            kernel_version: "6.0".into(),
            slurm: crate::state::db::SlurmVersion {
                major: 23,
                minor: 11,
                patch: 5,
            },
            port: 2222,
            identity_path: Some("/tmp/test_id".to_string()),
            accounting_available: true,
            default_base_path: Some("/tmp/runs".to_string()),
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
        };

        let _session = cache.get_or_create("host-a", &host).await.unwrap();
        let recording = factory.recorded();
        assert_eq!(recording.len(), 1);
        let params = &recording[0];
        assert_eq!(params.addr.port(), 2222);
        assert_eq!(params.identity_path.as_deref(), Some("/tmp/test_id"));
        assert_eq!(params.username, "alice");
    }
}
