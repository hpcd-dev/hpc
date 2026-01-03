// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use anyhow::{Context, Result, anyhow};
use proto::{
    MfaAnswer, MfaPrompt, Prompt, StreamEvent, SubmitStreamEvent, stream_event, submit_stream_event,
};
use russh::client::{AuthResult, KeyboardInteractiveAuthResponse};
use russh::keys::PrivateKeyWithHashAlg;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::{ClientHandler, SessionManager};

enum AuthDecision {
    Success,
    KeyboardInteractive,
    Failure { msg: String },
}

trait MfaEvent {
    fn from_prompt(prompt: MfaPrompt) -> Self;
}

impl MfaEvent for StreamEvent {
    fn from_prompt(prompt: MfaPrompt) -> Self {
        StreamEvent {
            event: Some(stream_event::Event::Mfa(prompt)),
        }
    }
}

impl MfaEvent for SubmitStreamEvent {
    fn from_prompt(prompt: MfaPrompt) -> Self {
        SubmitStreamEvent {
            event: Some(submit_stream_event::Event::Mfa(prompt)),
        }
    }
}

fn auth_decision(result: AuthResult) -> AuthDecision {
    match result {
        AuthResult::Success => AuthDecision::Success,
        AuthResult::Failure {
            remaining_methods,
            partial_success,
        } if partial_success
            && remaining_methods.contains(&russh::MethodKind::KeyboardInteractive) =>
        {
            AuthDecision::KeyboardInteractive
        }
        AuthResult::Failure {
            remaining_methods,
            partial_success,
        } => AuthDecision::Failure {
            msg: format!(
                "authentication failure(remaining_methods={:?},partial_success={:?})",
                remaining_methods, partial_success
            ),
        },
    }
}

impl SessionManager {
    /// Ensure we have a connected & authenticated handle.
    /// Streams any keyboard-interactive MFA prompts to `evt_tx`
    /// and consumes responses from `mfa_rx`.
    pub async fn ensure_connected(
        &self,
        evt_tx: &mpsc::Sender<Result<StreamEvent, tonic::Status>>,
        mfa_rx: &mut mpsc::Receiver<MfaAnswer>,
    ) -> Result<()> {
        self.ensure_connected_with(evt_tx, mfa_rx).await
    }

    pub async fn ensure_connected_submit(
        &self,
        evt_tx: &mpsc::Sender<Result<SubmitStreamEvent, tonic::Status>>,
        mfa_rx: &mut mpsc::Receiver<MfaAnswer>,
    ) -> Result<()> {
        self.ensure_connected_with(evt_tx, mfa_rx).await
    }

    async fn ensure_connected_with<E: MfaEvent>(
        &self,
        evt_tx: &mpsc::Sender<Result<E, tonic::Status>>,
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
            log::info!(
                "re-establishing connection with {}@{}",
                &self.params.username,
                &self.params.addr
            );
            // Establish TCP + SSH
            let handler = ClientHandler;
            let mut handle = russh::client::connect(self.config.clone(), self.params.addr, handler)
                .await
                .context("SSH connect failed")?;
            log::info!(
                "established initial connection with {}@{}, proceeding with auth",
                &self.params.username,
                &self.params.addr
            );
            // Try publickey first if identity provided
            if let Some(path) = &self.params.identity_path {
                let key = russh::keys::load_secret_key(path, None)
                    .with_context(|| format!("failed to load secret key at {}", path))?;
                let key = Arc::new(key);
                // Prefer SHA-256 for RSA if applicable (ignored for non-RSA keys)
                let pk = PrivateKeyWithHashAlg::new(
                    key,
                    handle.best_supported_rsa_hash().await?.flatten(),
                );
                let result = handle
                    .authenticate_publickey(self.params.username.clone(), pk)
                    .await?;
                match auth_decision(result) {
                    AuthDecision::Success => {
                        // Auth finished, good to go
                    }
                    AuthDecision::KeyboardInteractive => {
                        // Fall back to KI
                        self.do_keyboard_interactive(&mut handle, evt_tx, mfa_rx)
                            .await?;
                    }
                    AuthDecision::Failure { msg } => return Err(anyhow!(msg)),
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
                    let mut ticker = tokio::time::interval(interval / 2);
                    loop {
                        ticker.tick().await;
                        let guard = handle_clone.lock().await;
                        let Some(handle) = guard.as_ref() else {
                            continue;
                        };
                        if handle.is_closed() {
                            log::debug!("keepalive handle is closed");
                            break;
                        }
                        if let Err(e) = handle.send_keepalive(want_reply).await {
                            log::debug!("error when sending a keepalive: {}", e);
                        } else {
                            log::debug!("successfully sent a keepalive message");
                        }
                    }
                });

                *self.keepalive_task_handle.lock().await = Some(jh);
            }
        } else {
            log::info!(
                "don't need to re-establish connection to {}@{}",
                &self.params.username,
                &self.params.addr
            );
        }

        Ok(())
    }

    pub(crate) async fn ensure_connected_for_sync(
        &self,
        evt_tx: &mpsc::Sender<Result<SubmitStreamEvent, tonic::Status>>,
        mfa_rx: &mut mpsc::Receiver<MfaAnswer>,
    ) -> Result<()> {
        #[cfg(test)]
        if let Some(hooks) = &self.test_hooks {
            return (hooks.ensure_connected)(evt_tx, mfa_rx).await;
        }
        self.ensure_connected_submit(evt_tx, mfa_rx).await
    }

    /// Runs the keyboard-interactive auth loop, streaming prompts out
    /// and consuming answers from mfa_rx until Success/Failure.
    async fn do_keyboard_interactive<E: MfaEvent>(
        &self,
        handle: &mut russh::client::Handle<ClientHandler>,
        evt_tx: &mpsc::Sender<Result<E, tonic::Status>>,
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
                    let _ = evt_tx.send(Ok(E::from_prompt(prompt_msg))).await;

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
}

#[cfg(test)]
mod tests {
    use super::{AuthDecision, auth_decision};
    use russh::client::AuthResult;
    use russh::{MethodKind, MethodSet};

    #[test]
    fn auth_decision_success() {
        let decision = auth_decision(AuthResult::Success);
        assert!(matches!(decision, AuthDecision::Success));
    }

    #[test]
    fn auth_decision_keyboard_interactive_on_partial_success() {
        let methods = [MethodKind::KeyboardInteractive];
        let decision = auth_decision(AuthResult::Failure {
            remaining_methods: MethodSet::from(methods.as_slice()),
            partial_success: true,
        });
        assert!(matches!(decision, AuthDecision::KeyboardInteractive));
    }

    #[test]
    fn auth_decision_failure_without_keyboard_interactive() {
        let methods = [MethodKind::PublicKey];
        let decision = auth_decision(AuthResult::Failure {
            remaining_methods: MethodSet::from(methods.as_slice()),
            partial_success: false,
        });
        let AuthDecision::Failure { msg } = decision else {
            panic!("expected auth failure");
        };
        assert!(msg.contains("authentication failure"));
    }
}
