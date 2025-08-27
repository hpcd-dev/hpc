use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow};
use tokio::sync::{Mutex, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use russh::ChannelMsg;
use russh::client::Config;
use russh::client::{Handler, KeyboardInteractiveAuthResponse};
use russh::keys::PrivateKeyWithHashAlg;
use russh::{Channel, ChannelId};

use proto::{MfaAnswer, MfaPrompt, Prompt, StreamEvent, stream_event};

/// Minimal russh client handler. We rely on default implementations.
/// You could extend this to verify host keys or log events.
#[derive(Clone, Debug, Default)]
pub struct ClientHandler;

impl Handler for ClientHandler {
    type Error = anyhow::Error;
}

/// Parameters for establishing the SSH connection.
#[derive(Clone, Debug)]
pub struct SshParams {
    pub addr: SocketAddr,
    pub username: String,
    pub identity_path: Option<String>,
    /// Preferred submethods hint for keyboard-interactive (often unused by servers).
    pub ki_submethods: Option<String>,
    /// Send TCP keepalives to keep long connections healthy.
    pub keepalive_secs: u64,
}

/// Manager that owns a single long-lived SSH connection.
pub struct SessionManager {
    params: SshParams,
    config: Arc<Config>,
    // The active handle, protected by a mutex because we serialize command use
    handle: Arc<Mutex<Option<russh::client::Handle<ClientHandler>>>>,
    // Background keepalive task
    keepalive_task_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl SessionManager {
    pub fn new(params: SshParams) -> Self {
        let mut cfg = Config::default();
        cfg.inactivity_timeout = Some(Duration::from_secs(30));
        cfg.keepalive_interval = Some(Duration::from_secs(params.keepalive_secs));
        // reasonable channel buffer and window sizes for streaming
        cfg.channel_buffer_size = 64;
        cfg.window_size = 1024 * 1024;
        Self {
            params,
            config: Arc::new(cfg),
            handle: Arc::new(Mutex::new(None)),
            keepalive_task_handle: Arc::new(Mutex::new(None)),
        }
    }

    /// Ensure we have a connected & authenticated handle.
    /// Streams any keyboard-interactive MFA prompts to `evt_tx`
    /// and consumes responses from `mfa_rx`.
    async fn ensure_connected(
        &self,
        evt_tx: &mpsc::Sender<Result<StreamEvent, tonic::Status>>,
        mfa_rx: &mut mpsc::Receiver<MfaAnswer>,
    ) -> Result<()> {
        let mut handle_field = self.handle.lock().await;

        // If handle exists but is closed, drop it so we reconnect.
        let needs_connect = match handle_field.as_ref() {
            None => true,
            Some(h) if h.is_closed() => true,
            Some(_) => false,
        };

        if needs_connect {
            // Establish TCP + SSH
            let handler = ClientHandler::default();
            let mut handle = russh::client::connect(self.config.clone(), self.params.addr, handler)
                .await
                .context("SSH connect failed")?;

            // Try publickey first if identity provided
            if let Some(path) = &self.params.identity_path {
                let key = russh::keys::load_secret_key(path, None)
                    .with_context(|| format!("failed to load secret key at {}", path))?;
                let key = Arc::new(key);
                // Prefer SHA-256 for RSA if applicable (ignored for non-RSA keys)
                let pk =
                    PrivateKeyWithHashAlg::new(key, Some(russh::keys::ssh_key::HashAlg::Sha256));
                match handle
                    .authenticate_publickey(self.params.username.clone(), pk)
                    .await?
                {
                    russh::client::AuthResult::Success => {
                        // Auth finished, good to go
                    }
                    russh::client::AuthResult::Failure {
                        remaining_methods,
                        partial_success,
                    } if partial_success
                        & remaining_methods.contains(&russh::MethodKind::KeyboardInteractive) =>
                    {
                        // Fall back to KI
                        self.do_keyboard_interactive(&mut handle, evt_tx, mfa_rx)
                            .await?;
                    }
                    russh::client::AuthResult::Failure {
                        remaining_methods,
                        partial_success,
                    } => {
                        return Err(anyhow!(
                            "authentication failure(remaining_methods={:?},partial_success={:?})",
                            remaining_methods,
                            partial_success
                        ));
                    }
                }
            } else {
                // No key -> go straight to keyboard interactive
                self.do_keyboard_interactive(&mut handle, evt_tx, mfa_rx)
                    .await?;
            }

            *handle_field = Some(handle);
            // Start a keepalive pinger in the background
            if let Some(interval) = self.config.keepalive_interval {
                let handle_clone = self.handle.clone();
                let want_reply = true;
                let jh = tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(interval);
                    loop {
                        ticker.tick().await;
                        if let Some(ref h) = *handle_clone.lock().await {
                            if h.is_closed() {
                                break;
                            }
                        }
                        // Ignore errors; connection may be closing.
                        let _ = match *handle_clone.lock().await {
                            Some(ref v) => {
                                v.send_keepalive(want_reply).await;
                            }
                            None => {}
                        };
                    }
                });

                *self.keepalive_task_handle.lock().await = Some(jh);
            }
        }

        Ok(())
    }

    /// Runs the keyboard-interactive auth loop, streaming prompts out
    /// and consuming answers from mfa_rx until Success/Failure.
    async fn do_keyboard_interactive(
        &self,
        handle: &mut russh::client::Handle<ClientHandler>,
        evt_tx: &mpsc::Sender<Result<StreamEvent, tonic::Status>>,
        mfa_rx: &mut mpsc::Receiver<MfaAnswer>,
    ) -> Result<()> {
        let mut ki = handle
            .authenticate_keyboard_interactive_start(
                self.params.username.clone(),
                self.params.ki_submethods.clone(),
            )
            .await
            .context("KI start failed")?;

        loop {
            match ki {
                KeyboardInteractiveAuthResponse::Success => return Ok(()),
                KeyboardInteractiveAuthResponse::Failure {
                    remaining_methods,
                    partial_success,
                } => {
                    // Auth failed. Surface a clear error to client.
                    let msg = format!(
                        "authentication failed (partial_success={}, remaining={:?})",
                        partial_success, remaining_methods
                    );
                    return Err(anyhow!(msg));
                }
                KeyboardInteractiveAuthResponse::InfoRequest {
                    name,
                    instructions,
                    prompts,
                } => {
                    // Stream MFA prompt to the client
                    let prompt_msg = MfaPrompt {
                        name,
                        instructions,
                        prompts: prompts
                            .into_iter()
                            .map(|p| Prompt {
                                text: p.prompt,
                                echo: p.echo,
                            })
                            .collect(),
                    };
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Mfa(prompt_msg)),
                        }))
                        .await;

                    // Wait for client answers
                    let answers = mfa_rx
                        .recv()
                        .await
                        .ok_or_else(|| anyhow!("client disconnected during MFA"))?;

                    // Respond to the server and continue the KI loop
                    ki = handle
                        .authenticate_keyboard_interactive_respond(answers.responses)
                        .await
                        .context("KI respond failed")?;
                }
            }
        }
    }

    /// Execute a single whitelisted command over the (shared) SSH connection,
    /// streaming stdout/stderr and exit code to `evt_tx`.
    ///
    /// Only one command is run at a time; a long-lived channel lock ensures that.
    pub async fn exec_whitelisted(
        &self,
        command: &str,
        evt_tx: mpsc::Sender<Result<StreamEvent, tonic::Status>>,
        mut mfa_rx: mpsc::Receiver<MfaAnswer>,
    ) -> Result<()> {
        // Enforce whitelist
        let cmd = match command {
            "ls" => "ls",
            "uname -a" => "uname -a",
            "w" => "w",
            other => return Err(anyhow!("command not allowed: {}", other)),
        };

        // Ensure connection & (re)authenticate if needed (may prompt MFA)
        self.ensure_connected(&evt_tx, &mut mfa_rx).await?;

        // From here on, hold the handle lock for the duration of the command
        let mut guard = self.handle.lock().await;
        let handle = guard
            .as_ref()
            .ok_or_else(|| anyhow!("SSH handle lost after connect"))?;

        let mut chan = handle
            .channel_open_session()
            .await
            .context("open session")?;

        chan.exec(true, cmd).await.context("exec request")?;

        // Read the remote process output and forward as gRPC stream items
        while let Some(msg) = chan.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::Stdout(data.to_vec())),
                        }))
                        .await;
                }
                ChannelMsg::ExtendedData { data, ext } => {
                    // SSH ext_type 1 == STDERR
                    if ext == 1 {
                        let _ = evt_tx
                            .send(Ok(StreamEvent {
                                event: Some(stream_event::Event::Stderr(data.to_vec())),
                            }))
                            .await;
                    }
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    let _ = evt_tx
                        .send(Ok(StreamEvent {
                            event: Some(stream_event::Event::ExitCode(exit_status as i32)),
                        }))
                        .await;
                }
                ChannelMsg::Eof => {
                    // ignore; loop will break after close/exit
                }
                ChannelMsg::Close => {
                    break;
                }
                _ => {
                    // Ignore other messages for this simple exec flow
                }
            }
        }

        // Be tidy
        let _ = chan.eof().await;
        let _ = chan.close().await;

        Ok(())
    }
}

/// Helper to wrap an mpsc receiver as a tonic stream.
pub fn receiver_to_stream(
    rx: mpsc::Receiver<Result<StreamEvent, tonic::Status>>,
) -> ReceiverStream<Result<StreamEvent, tonic::Status>> {
    ReceiverStream::new(rx)
}
