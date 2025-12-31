use anyhow::{Context, Result, anyhow};
use proto::{MfaAnswer, StreamEvent, stream_event};
use russh::ChannelMsg;
use tokio::sync::mpsc;

use super::SessionManager;

fn handle_capture_message(
    msg: &ChannelMsg,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
    code: &mut i32,
) -> bool {
    match msg {
        ChannelMsg::Data { data } => {
            out.extend_from_slice(data);
            false
        }
        ChannelMsg::ExtendedData { data, ext: 1 } => {
            err.extend_from_slice(data);
            false
        }
        ChannelMsg::ExitStatus { exit_status } => {
            *code = *exit_status as i32;
            false
        }
        ChannelMsg::Close => true,
        _ => false,
    }
}

fn handle_exec_message(msg: &ChannelMsg) -> (Option<StreamEvent>, bool) {
    match msg {
        ChannelMsg::Data { data } => (
            Some(StreamEvent {
                event: Some(stream_event::Event::Stdout(data.to_vec())),
            }),
            false,
        ),
        ChannelMsg::ExtendedData { data, ext } if *ext == 1 => (
            Some(StreamEvent {
                event: Some(stream_event::Event::Stderr(data.to_vec())),
            }),
            false,
        ),
        ChannelMsg::ExitStatus { exit_status } => (
            Some(StreamEvent {
                event: Some(stream_event::Event::ExitCode(*exit_status as i32)),
            }),
            false,
        ),
        ChannelMsg::Close => (None, true),
        _ => (None, false),
    }
}

fn ls_exit_code_to_exists(code: i32) -> bool {
    code == 0
}

impl SessionManager {
    pub(super) async fn exec_simple(&self, cmd: &str) -> Result<i32> {
        // Executes a command with "dummy" channels.
        // TODO: Refactor cases when this is needed.
        let guard = self.handle.lock().await;
        let handle = guard.as_ref().ok_or_else(|| anyhow!("SSH handle lost"))?;
        let mut chan = handle.channel_open_session().await?;
        chan.exec(true, cmd).await?;
        let mut code: i32 = 0;
        while let Some(msg) = chan.wait().await {
            if let ChannelMsg::ExitStatus { exit_status } = msg {
                code = exit_status as i32;
            }
            if matches!(msg, ChannelMsg::Close) {
                break;
            }
        }
        let _ = chan.eof().await;
        let _ = chan.close().await;
        Ok(code)
    }

    // Execute command over SSH, retrieving stdout, stderr and exit code as output
    pub async fn exec_capture(&self, cmd: &str) -> Result<(Vec<u8>, Vec<u8>, i32)> {
        let guard = self.handle.lock().await;
        let handle = guard.as_ref().ok_or_else(|| anyhow!("SSH handle lost"))?;
        let mut chan = handle.channel_open_session().await?;
        let actual_command = cmd;
        log::debug!("executing '{}'", &actual_command);
        //r#"bash -lc 'echo "$SHELL"; echo "$PATH"; command -v python3; python3 -V'"#;
        chan.exec(true, actual_command)
            .await
            .context("exec request")?;
        //chan.eof().await?;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut code: i32 = 0;
        loop {
            let Some(msg) = chan.wait().await else {
                break;
            };
            if handle_capture_message(&msg, &mut out, &mut err, &mut code) {
                break;
            }
        }

        let _ = chan.close().await;
        Ok((out, err, code))
    }

    /// Execute a single command over the (shared) SSH connection,
    /// streaming stdout/stderr and exit code to `evt_tx`.
    ///
    /// Only one command is run at a time; a long-lived channel lock ensures that.
    pub async fn exec(
        &self,
        cmd: &str,
        evt_tx: mpsc::Sender<Result<StreamEvent, tonic::Status>>,
        mut mfa_rx: mpsc::Receiver<MfaAnswer>,
    ) -> Result<()> {
        // Enforce whitelist

        // Ensure connection & (re)authenticate if needed (may prompt MFA)
        self.ensure_connected(&evt_tx, &mut mfa_rx).await?;

        // From here on, hold the handle lock for the duration of the command
        let guard = self.handle.lock().await;
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
            let (event, should_break) = handle_exec_message(&msg);
            if let Some(event) = event {
                let _ = evt_tx.send(Ok(event)).await;
            }
            if should_break {
                break;
            }
        }

        // Be tidy
        let _ = chan.eof().await;
        let _ = chan.close().await;

        Ok(())
    }

    pub async fn directory_exists(&self, dirname: &str) -> Result<bool, String> {
        let command = format!("ls {} 1>&2 2>/dev/null", dirname);
        let (_, _, code) = match self.exec_capture(&command).await {
            Ok(v) => v,
            Err(e) => return Err(e.to_string()),
        };
        Ok(ls_exit_code_to_exists(code))
    }
}

#[cfg(test)]
mod tests {
    use super::{handle_capture_message, handle_exec_message, ls_exit_code_to_exists};
    use proto::stream_event;
    use russh::{ChannelMsg, CryptoVec};

    #[test]
    fn handle_capture_message_accumulates_output() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut code = 0;

        let msg = ChannelMsg::Data {
            data: CryptoVec::from_slice(b"hi"),
        };
        assert!(!handle_capture_message(&msg, &mut out, &mut err, &mut code));
        assert_eq!(out, b"hi");

        let msg = ChannelMsg::ExtendedData {
            data: CryptoVec::from_slice(b"err"),
            ext: 1,
        };
        assert!(!handle_capture_message(&msg, &mut out, &mut err, &mut code));
        assert_eq!(err, b"err");

        let msg = ChannelMsg::ExitStatus { exit_status: 42 };
        assert!(!handle_capture_message(&msg, &mut out, &mut err, &mut code));
        assert_eq!(code, 42);

        let msg = ChannelMsg::Close;
        assert!(handle_capture_message(&msg, &mut out, &mut err, &mut code));
    }

    #[test]
    fn handle_exec_message_maps_stream_events() {
        let msg = ChannelMsg::Data {
            data: CryptoVec::from_slice(b"hi"),
        };
        let (event, should_break) = handle_exec_message(&msg);
        assert!(!should_break);
        let Some(event) = event else { panic!("expected event") };
        assert!(matches!(
            event.event,
            Some(stream_event::Event::Stdout(bytes)) if bytes == b"hi"
        ));

        let msg = ChannelMsg::ExtendedData {
            data: CryptoVec::from_slice(b"err"),
            ext: 1,
        };
        let (event, should_break) = handle_exec_message(&msg);
        assert!(!should_break);
        let Some(event) = event else { panic!("expected event") };
        assert!(matches!(
            event.event,
            Some(stream_event::Event::Stderr(bytes)) if bytes == b"err"
        ));

        let msg = ChannelMsg::ExitStatus { exit_status: 7 };
        let (event, should_break) = handle_exec_message(&msg);
        assert!(!should_break);
        let Some(event) = event else { panic!("expected event") };
        assert!(matches!(event.event, Some(stream_event::Event::ExitCode(7))));

        let msg = ChannelMsg::ExtendedData {
            data: CryptoVec::from_slice(b"skip"),
            ext: 2,
        };
        let (event, should_break) = handle_exec_message(&msg);
        assert!(!should_break);
        assert!(event.is_none());

        let msg = ChannelMsg::Close;
        let (event, should_break) = handle_exec_message(&msg);
        assert!(should_break);
        assert!(event.is_none());
    }

    #[test]
    fn ls_exit_code_to_exists_returns_true_on_zero() {
        assert!(ls_exit_code_to_exists(0));
        assert!(!ls_exit_code_to_exists(1));
    }
}
