use anyhow::Result;
use proto::{MfaAnswer, StreamEvent};
use russh::client::Config;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};

use crate::ssh::sync::BoxFuture;

mod auth;
mod exec;
mod sftp;

#[cfg(test)]
mod tests;

/// Minimal russh client handler. We rely on default implementations.
/// TODO: add actual server key verification
#[derive(Clone, Debug, Default)]
struct ClientHandler;

impl russh::client::Handler for ClientHandler {
    type Error = anyhow::Error;
    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        Ok(true)
    }
}

/// Parameters for establishing the SSH connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshParams {
    pub addr: SocketAddr,
    pub username: String,
    pub identity_path: Option<String>,
    /// Preferred submethods hint for keyboard-interactive (often unused by servers).
    pub ki_submethods: Option<String>,
    /// Send TCP keepalives to keep long connections healthy.
    pub keepalive_secs: u64,
}

#[cfg(test)]
pub(crate) struct SessionManagerTestHooks {
    ensure_connected: Arc<
        dyn Fn(
                &mpsc::Sender<Result<StreamEvent, tonic::Status>>,
                &mut mpsc::Receiver<MfaAnswer>,
            ) -> BoxFuture<'static, Result<()>>
            + Send
            + Sync,
    >,
    ensure_remote_dir: Arc<dyn Fn(&str) -> BoxFuture<'static, Result<()>> + Send + Sync>,
    sync_one_file:
        Arc<dyn Fn(&Path, &str, &str, usize) -> BoxFuture<'static, Result<()>> + Send + Sync>,
}

/// Manager that owns a single long-lived SSH connection.
pub struct SessionManager {
    params: SshParams,
    config: Arc<Config>,
    // The active handle, protected by a mutex because we serialize command use
    handle: Arc<Mutex<Option<russh::client::Handle<ClientHandler>>>>,
    // Background keepalive task
    keepalive_task_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    #[cfg(test)]
    test_hooks: Option<SessionManagerTestHooks>,
}

impl SessionManager {
    pub fn new(params: SshParams) -> Self {
        let cfg = Config {
            inactivity_timeout: Some(Duration::from_secs(30)),
            keepalive_interval: Some(Duration::from_secs(params.keepalive_secs)),
            // reasonable channel buffer and window sizes for streaming
            channel_buffer_size: 64,
            window_size: 1024 * 1024,
            ..Default::default()
        };
        Self {
            params,
            config: Arc::new(cfg),
            handle: Arc::new(Mutex::new(None)),
            keepalive_task_handle: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            test_hooks: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_test_hooks(&mut self, hooks: SessionManagerTestHooks) {
        self.test_hooks = Some(hooks);
    }

    pub async fn needs_connect(&self) -> bool {
        let handle_field = self.handle.lock().await;
        match handle_field.as_ref() {
            None => true,
            Some(h) if h.is_closed() => true,
            Some(_) => false,
        }
    }
}
