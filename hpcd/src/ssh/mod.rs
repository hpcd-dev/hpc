use anyhow::{Context, Result, anyhow};
use blake3::Hasher as Blake3;
use futures::stream::StreamExt;
use globset::{GlobBuilder, GlobMatcher};
use log;
use proto::{MfaAnswer, MfaPrompt, Prompt, StreamEvent, stream_event};
use rand::{Rng, distr::Alphanumeric};
use russh::ChannelMsg;
use russh::client::Config;
use russh::client::{Handler, KeyboardInteractiveAuthResponse};
use russh::keys::PrivateKeyWithHashAlg;
use russh::keys::ssh_key::private::Ed25519PrivateKey;
use russh::{Channel, ChannelId};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::fs as tokiofs;
use tokio::io::AsyncReadExt;
use tokio::io::{AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use walkdir::WalkDir;
/// Minimal russh client handler. We rely on default implementations.
/// You could extend this to verify host keys or log events.
#[derive(Clone, Debug, Default)]
pub struct ClientHandler;

impl Handler for ClientHandler {
    type Error = anyhow::Error;
    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        return Ok(true);
    }
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

#[derive(Clone, Copy, Debug)]
pub enum SyncFilterAction {
    Include,
    Exclude,
}

#[derive(Clone, Debug)]
pub struct SyncFilterRule {
    pub action: SyncFilterAction,
    pub pattern: String,
}

#[derive(Debug)]
struct CompiledFilterRule {
    action: SyncFilterAction,
    matcher: GlobMatcher,
    only_dir: bool,
    match_basename: bool,
}

#[derive(Debug)]
struct PathFilter {
    rules: Vec<CompiledFilterRule>,
}

impl PathFilter {
    fn new(rules: &[SyncFilterRule]) -> Result<Self> {
        let mut compiled = Vec::with_capacity(rules.len());
        for rule in rules {
            compiled.push(CompiledFilterRule::compile(rule)?);
        }
        Ok(Self { rules: compiled })
    }

    fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    fn should_include(&self, rel_path: &Path, is_dir: bool) -> bool {
        if self.rules.is_empty() {
            return true;
        }
        let rel_str = rel_path_to_slash(rel_path);
        let basename = rel_path
            .file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default();
        for rule in &self.rules {
            if rule.matches(&rel_str, &basename, is_dir) {
                return matches!(rule.action, SyncFilterAction::Include);
            }
        }
        true
    }
}

impl CompiledFilterRule {
    fn compile(rule: &SyncFilterRule) -> Result<Self> {
        let mut pattern = rule.pattern.trim().to_string();
        if pattern.is_empty() {
            anyhow::bail!("filter pattern cannot be empty");
        }

        let mut only_dir = false;
        if pattern.ends_with('/') {
            only_dir = true;
            while pattern.ends_with('/') {
                pattern.pop();
            }
        }
        if pattern.is_empty() {
            anyhow::bail!("filter pattern cannot be empty");
        }

        let anchored = pattern.starts_with('/');
        if anchored {
            pattern = pattern.trim_start_matches('/').to_string();
        }

        let has_slash = pattern.contains('/');
        let match_basename = !has_slash && !anchored;
        if has_slash && !anchored && !pattern.starts_with("**/") {
            pattern = format!("**/{}", pattern);
        }

        let mut builder = GlobBuilder::new(&pattern);
        builder.literal_separator(true);
        let matcher = builder
            .build()
            .with_context(|| format!("invalid filter pattern '{}'", rule.pattern))?
            .compile_matcher();
        Ok(Self {
            action: rule.action,
            matcher,
            only_dir,
            match_basename,
        })
    }

    fn matches(&self, rel_path: &str, basename: &str, is_dir: bool) -> bool {
        if self.only_dir && !is_dir {
            return false;
        }
        if self.match_basename {
            self.matcher.is_match(basename)
        } else {
            self.matcher.is_match(rel_path)
        }
    }
}

fn rel_path_to_slash(path: &Path) -> String {
    let mut out = String::new();
    for comp in path.components() {
        if let std::path::Component::Normal(os) = comp {
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(&os.to_string_lossy());
        }
    }
    out
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

    #[cfg(test)]
    pub fn test_params(&self) -> &SshParams {
        &self.params
    }
    pub async fn needs_connect(&self) -> bool {
        let handle_field = self.handle.lock().await;
        return match handle_field.as_ref() {
            None => true,
            Some(h) if h.is_closed() => true,
            Some(_) => false,
        };
    }
    /// Ensure we have a connected & authenticated handle.
    /// Streams any keyboard-interactive MFA prompts to `evt_tx`
    /// and consumes responses from `mfa_rx`.
    pub async fn ensure_connected(
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
            log::info!(
                "re-establishing connection with {}@{}",
                &self.params.username,
                &self.params.addr
            );
            // Establish TCP + SSH
            let handler = ClientHandler::default();
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
                        && remaining_methods.contains(&russh::MethodKind::KeyboardInteractive) =>
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
                    let mut ticker = tokio::time::interval(interval / 2);
                    loop {
                        ticker.tick().await;
                        if let Some(ref h) = *handle_clone.lock().await {
                            if h.is_closed() {
                                log::debug!("keepalive handle is closed");
                                break;
                            }
                        }
                        // Ignore errors; connection may be closing.
                        let _ = match *handle_clone.lock().await {
                            Some(ref v) => {
                                match v.send_keepalive(want_reply).await {
                                    Err(e) => {
                                        log::debug!("error when sending a keepalive: {}", e);
                                    }
                                    Ok(_) => {
                                        log::debug!("successfully sent a keepalive message");
                                    }
                                };
                            }
                            None => {}
                        };
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
    async fn exec_simple(&self, cmd: &str) -> Result<i32> {
        // Executes a command with "dummy" channels.
        // TODO: Refactor cases when this is needed.
        let mut guard = self.handle.lock().await;
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
        let mut guard = self.handle.lock().await;
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
            match msg {
                ChannelMsg::Data { ref data } => {
                    out.extend_from_slice(&data.to_vec());
                }
                ChannelMsg::ExtendedData { ref data, ext } if ext == 1 => {
                    err.extend_from_slice(&data)
                }
                ChannelMsg::ExitStatus { exit_status } => code = exit_status as i32,

                ChannelMsg::Close => break,

                _ => {}
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
    async fn sftp(&self) -> Result<SftpSession> {
        // TODO: this function is using dummy auth channels right now - for production use, they
        // MUST be hooked to user-guided MFA
        let guard = self.handle.lock().await;
        let handle = guard
            .as_ref()
            .ok_or_else(|| anyhow!("SSH handle lost before opening SFTP"))?;
        let channel = handle.clone().channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        let sftp = SftpSession::new(channel.into_stream()).await?;
        Ok(sftp)
    }
    async fn ensure_remote_dir(&self, sftp_session: &SftpSession, remote_dir: &str) -> Result<()> {
        // Normalize to components and create incrementally
        let mut cur = String::from("");
        log::debug!("remote dir: {remote_dir}");
        for comp in Path::new(remote_dir).components() {
            let seg = match comp {
                std::path::Component::RootDir => "/".to_string(),
                std::path::Component::Normal(os) => os.to_string_lossy().to_string(),
                std::path::Component::CurDir => continue,
                std::path::Component::ParentDir => continue, // TODO: this actually needs to remove component, I will need to fix this
                _ => continue,
            };
            log::debug!("seg: {}", &seg);
            if seg.is_empty() {
                log::debug!("empty seg");
                continue;
            }
            if cur != "/" && !cur.is_empty() {
                cur = format!("{}/{}", cur.trim_end_matches("/"), seg);
            } else {
                cur = format!("/{}", seg);
            }
            log::debug!("cur: {}", &cur);
            match sftp_session.metadata(&cur).await {
                Ok(meta) => {
                    if !meta.is_dir() {
                        return Err(anyhow!(
                            "remote path exists but is not a directory: {}",
                            cur
                        ));
                    }
                }
                Err(e) => {
                    log::debug!("got error when retrieving metadata: {}", e);
                    let mut attrs = FileAttributes::default();
                    attrs.permissions = Some(0o700);
                    let _ = sftp_session
                        .create_dir(&cur)
                        .await
                        .context(format!("creating path {}", &cur))?;
                    match sftp_session.set_metadata(&cur, attrs).await {
                        Ok(_) => {}
                        Err(e) => {
                            log::warn!("error when setting metadata for path {}: {}", &cur, e)
                        }
                    };
                }
            }
        }
        Ok(())
    }

    /// checks if remote hasher already exists on remote
    /// if it does - exit and do nothing
    /// if it doesn't - writes file to remote
    /// TODO - we don't need to do this within sync_one_file
    async fn ensure_remote_temp_exe(
        &self,
        content: &[u8],
        session_id: &str,
        suffix: &str,
    ) -> Result<String> {
        let sftp = self.sftp().await?;
        let path = format!(
            "/tmp/hpcd_{}.{}",
            session_id,
            suffix.trim_start_matches('.')
        );
        if sftp.try_exists(&path).await? {
            return Ok(path);
        }
        // Create & write
        let flags = OpenFlags::WRITE.union(OpenFlags::CREATE);
        let mut attrs = FileAttributes::default();
        attrs.permissions = Some(0o700);
        let mut f = sftp
            .open_with_flags_and_attributes(&path, flags, attrs)
            .await
            .with_context(|| format!("open remote temp {}", &path))?;
        f.write(content)
            .await
            .with_context(|| format!("write remote temp {}", &path))?;
        f.flush().await?;
        f.shutdown().await?;
        Ok(path)
    }

    /// computes hashes of blocks on a remote file
    /// Using a Python helper script.
    ///
    async fn remote_block_hashes(
        &self,
        remote_path: &str,
        session_id: &str,
        block_size: usize,
    ) -> Result<Vec<String>> {
        let py_script = format!(
            r#"#!/usr/bin/env python3
import sys, os, hashlib
path = r{rp}
bs = {bs}
try:
    st = os.stat(path)
except Exception as e:
    print("ERR", e, file=sys.stderr)
    sys.exit(2)
h = hashlib.sha256
off = 0
with open(path, 'rb') as f:
    while True:
        b = f.read(bs)
        if not b: break
        print(off, len(b), h(b).hexdigest(), file=sys.stdout)
        off += len(b)
sys.stdout.flush()
"#,
            rp = format!(
                "'{}'",
                remote_path.replace("\\", "\\\\").replace("'", "\\'")
            ),
            bs = block_size
        );
        // TODO: ensure_remote_path_exe can be run once per transfer, no need to do this for every
        // file: even if script on remote exists, it will require some network trafic to check if
        // it exists.
        let script_path = self
            .ensure_remote_temp_exe(py_script.as_bytes(), session_id, "py")
            .await?;
        let cmd_py3 = format!("python3 -u {}", &script_path);
        let (out, err, code) = self.exec_capture(&cmd_py3).await?;
        let (out, err, code) = if code != 0 {
            let cmd_py = format!("python -u {}", &script_path);
            self.exec_capture(&cmd_py).await?
        } else {
            (out, err, code)
        };
        // 3) Cleanup temp script (best-effort)
        //let _ = self.sftp().await?.remove_file(&script_path).await;
        if code != 0 {
            let err_message = String::from_utf8(err)?;
            return Err(anyhow!(err_message));
        }
        let out = String::from_utf8(out)?;
        let mut hashes = Vec::new();
        for line in out.lines() {
            if line.starts_with("ERR ") {
                return Err(anyhow!("remote: {}", line));
            }
            let mut it = line.split_whitespace();
            let _off = it.next();
            let _len = it.next();
            let hex = it.next();
            if let Some(h) = hex {
                hashes.push(h.to_string());
            }
        }
        Ok(hashes)
    }
    /// Syncs a single file from source to destination.
    /// Is kinda stable, but definitely needs more testing.
    async fn sync_one_file(
        &self,
        sftp: &SftpSession,
        local_path: &Path,
        remote_path: &str,
        session_id: &str,
        block_size: usize,
    ) -> Result<()> {
        let lmeta = tokiofs::metadata(local_path).await?;
        let lsize = lmeta.len();
        let lmtime = lmeta
            .modified()
            .unwrap_or(UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut rmeta = match sftp.metadata(remote_path).await {
            Ok(v) => v,
            Err(e) => {
                match e {
                    russh_sftp::client::error::Error::Status(ref estatus) => {
                        match estatus.status_code {
                            russh_sftp::protocol::StatusCode::NoSuchFile => {
                                // This is a good case - we just need to transfer file and exit,
                                // that's it
                                log::debug!("file {} doesn't exist, uploading", remote_path);
                                return upload_single_file(
                                    sftp,
                                    &local_path.to_path_buf(),
                                    remote_path,
                                    block_size,
                                )
                                .await;
                            }
                            _ => {
                                log::warn!("encountered Status error with status {:?}", estatus);
                                anyhow::bail!("encountered Status error with status {:?}", estatus)
                            }
                        }
                    }
                    /*
                    russh_sftp::client::error::Error::IO(ref ioerror) => {
                        log::debug!("IO error: {}", ioerror);
                    }
                    russh_sftp::client::error::Error::Timeout => {
                        log::debug!("encountered timeout error");
                    }
                    russh_sftp::client::error::Error::Limited(ref limiterror) => {
                        log::debug!("Limit exceeded error: {}", limiterror);
                    }
                    russh_sftp::client::error::Error::UnexpectedPacket => {
                        log::debug!("Unexpected packet");
                    }
                    russh_sftp::client::error::Error::UnexpectedBehavior(ref uerror) => {
                        log::debug!("Unexpected behavior: {}", uerror);
                    }
                    */
                    _ => anyhow::bail!("encountered error: {}", e),
                };
            }
        };
        if rmeta.is_dir() {
            anyhow::bail!("{} is a directory", remote_path);
        }
        if let Some(rmtime) = rmeta.mtime {
            if rmtime as u64 >= lmtime {
                log::debug!(
                    "remote file {remote_path} is at least as new as local file {local_path:?}, skipping transfer",
                );

                return Ok(());
            }
        } else {
            log::debug!("could not get mtime for remote path {remote_path}")
        }

        if rmeta.is_empty() {
            // Remote file is empty - weird but OK, we'll just upload our local file.
            return upload_single_file(sftp, &local_path.to_path_buf(), remote_path, block_size)
                .await;
        }
        log::debug!("Opening remote file for random-access writes");
        // Try to open remote file for random-access writes (create if absent)
        let flags = OpenFlags::WRITE.union(OpenFlags::READ);
        /*
            .union(OpenFlags::APPEND)
            .union(OpenFlags::TRUNCATE);
        */
        let mut rfile = sftp
            .open_with_flags(remote_path, flags)
            .await
            .context("opening remote file for random-access writes")?;
        // Plan: compute remote block hashes (without downloading file data) by running
        // a tiny Python hashing script *on the remote* over an exec channel.
        // If that fails (python missing, permission issues, etc), fall back to full upload.
        let remote_hashes = match self
            .remote_block_hashes(remote_path, session_id, block_size)
            .await
        {
            Ok(h) => Some(h),
            Err(e) => {
                log::warn!(
                    "remote hash plan unavailable for {}: {} ; falling back to full upload",
                    remote_path,
                    e
                );
                None
            }
        };
        if let Some(rblocks) = remote_hashes {
            // Compute local block hashes
            let lblocks = local_block_hashes(local_path, block_size).await?;

            // Compare and send only differing blocks
            for (i, lbh) in lblocks.iter().enumerate() {
                let offset = (i as u64) * (block_size as u64);
                let remaining = lsize.saturating_sub(offset);
                let this_block = remaining.min(block_size as u64) as usize;

                let differing = match rblocks.get(i) {
                    Some(rh) => rh != lbh,
                    None => true, // remote shorter; need to append
                };

                if differing {
                    log::debug!("found differing block at offset {offset}, index {i}");
                    // Read local block and write to remote at offset
                    let mut buf = vec![0u8; this_block];
                    let mut f = tokiofs::File::open(local_path).await.context(format!(
                        "opening local file {}",
                        local_path.to_string_lossy()
                    ))?;
                    f.seek(std::io::SeekFrom::Start(offset))
                        .await
                        .context(format!("seeking in remote file at offset {offset}"))?;
                    let n = f.read(&mut buf[..]).await.context(format!(
                        "reading from {:?} at offset {}",
                        local_path, offset
                    ))?;
                    // TODO: in case of errors, fallback to doing full transfer
                    rfile
                        .seek(std::io::SeekFrom::Start(offset))
                        .await
                        .context(format!("seeking in remote file at offset {offset}"))?;
                    let written = rfile
                        .write(&buf[..n])
                        .await
                        .with_context(|| format!("writing block @{} to {}", offset, remote_path))?;
                    rfile.flush().await?;
                }
            }

            // Truncate/extend remote file to match local size if needed
            // this is done in case the local file was truncated
            if rblocks.len() as u64 * block_size as u64 != lsize {
                log::info!("setting length medatata on remote file");
                // Use SFTP fsetstat(size) when supported; otherwise remote 'truncate'
                rmeta.size = Some(lsize);
                if let Err(_e) = rfile.set_metadata(rmeta).await {
                    log::warn!("setting metadata on remote failed - falling back to truncate");
                    // fallback via small exec
                    let cmd = format!("truncate -s {} {}", lsize, sh_escape(remote_path));
                    let _ = self.exec_simple(&cmd).await;
                }
            }
        } else {
            return upload_single_file(sftp, &local_path.to_path_buf(), remote_path, block_size)
                .await;
        }

        /*
        // Try to set remote mtime to local mtime so next run can skip
        log::debug!("setting remote mtime equal to local mtime");
        let mut attrs = FileAttributes::default();
        attrs.mtime = Some(lmtime as u32);
        let _ = sftp.set_metadata(remote_path, attrs).await;
        */
        Ok(())
    }

    /// Sync an entire local directory tree into a remote directory.
    /// - Creates remote directories as needed
    /// - Skips files where remote mtime >= local mtime
    /// - Uses block-delta writes for changed files
    pub async fn sync_dir<P: AsRef<Path>>(
        &self,
        local_dir: P,
        remote_dir: &str,
        block_size: Option<usize>,
        parallelism: Option<usize>,
        filters: &[SyncFilterRule],
        evt_tx: &mpsc::Sender<Result<StreamEvent, tonic::Status>>,
        mut mfa_rx: mpsc::Receiver<MfaAnswer>,
    ) -> Result<()> {
        let block_size = match block_size {
            Some(v) => v,
            None => 1024 * 1024, // 1 MiB block size
        };
        let parallelism = match parallelism {
            Some(v) => v,
            None => 8,
        };

        self.ensure_connected(evt_tx, &mut mfa_rx).await?;
        let local_dir = local_dir.as_ref().canonicalize()?;
        let path_filter = PathFilter::new(filters)?;
        let use_filter = !path_filter.is_empty();

        let sftp = self.sftp().await?;
        log::info!("making sure the remote directory exists");
        self.ensure_remote_dir(&sftp, remote_dir).await?;

        let session_id: String = rand::rng()
            .sample_iter(&Alphanumeric)
            .take(10)
            .map(char::from)
            .collect();

        let mut work: Vec<(PathBuf, String)> = Vec::new();

        let follow_links = false;
        let sftp_for_dirs = self.sftp().await?;
        for entry in WalkDir::new(&local_dir)
            .follow_links(follow_links)
            .into_iter()
            .filter_entry(|entry| {
                if !use_filter || entry.depth() == 0 {
                    return true;
                }
                if !entry.file_type().is_dir() {
                    return true;
                }
                match entry.path().strip_prefix(&local_dir) {
                    Ok(rel) => path_filter.should_include(rel, true),
                    Err(_) => true,
                }
            })
        {
            let direntry = match entry {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("encountered error when enumerating local files: {:?}", e);
                    continue;
                }
            };
            if !direntry.file_type().is_file() {
                // We skip directories - creating them is handled by file creation logic
                log::debug!(
                    "encountered directory {:?}, continuing",
                    direntry.into_path()
                );
                continue;
            }
            let path_local = direntry.path().to_path_buf();
            let rel = match path_local.strip_prefix(&local_dir) {
                Ok(v) => v.to_path_buf(),
                Err(_) => {
                    log::warn!(
                        "failed computing relative path for {:?} from {:?}",
                        path_local,
                        local_dir
                    );
                    continue;
                }
            };
            if use_filter && !path_filter.should_include(&rel, false) {
                continue;
            }
            let remote_path = join_remote(remote_dir, &rel);
            if let Some(parent_rel) = rel.parent() {
                let remote_parent = join_remote(remote_dir, parent_rel);
                self.ensure_remote_dir(&sftp, &remote_parent).await?;
            }
            work.push((path_local, remote_path));
        }
        drop(sftp_for_dirs);
        let results = futures::stream::iter(work.into_iter().map(|(local_path, remote_path)| {
            let session_id_clone = session_id.clone();
            async move {
                let sftp = self.sftp().await?;
                self.sync_one_file(
                    &sftp,
                    &local_path,
                    &remote_path,
                    &session_id_clone,
                    block_size,
                )
                .await
            }
        }))
        .buffer_unordered(parallelism)
        .collect::<Vec<_>>()
        .await;
        let mut errs = Vec::new();
        for res in results {
            if let Err(e) = res {
                errs.push(e);
            }
        }
        if !errs.is_empty() {
            // Build a helpful combined error message.
            use std::fmt::Write as _;
            let mut msg = String::new();
            for (i, e) in errs.iter().enumerate() {
                let _ = writeln!(&mut msg, "[{}] {:#}", i + 1, e);
            }
            anyhow::bail!("sync_dir encountered {} error(s):\n{}", errs.len(), msg);
        }
        Ok(())
    }
    pub async fn directory_exists(&self, dirname: &str) -> Result<bool, String> {
        let command = format!("ls {} 1>&2 2>/dev/null", dirname);
        let (_, _, code) = match self.exec_capture(&command).await {
            Ok(v) => v,
            Err(e) => return Err(e.to_string()),
        };
        match code {
            0 => Ok(true),
            _ => Ok(false),
        }
    }
}

/// Helper to wrap an mpsc receiver as a tonic stream.
pub fn receiver_to_stream(
    rx: mpsc::Receiver<Result<StreamEvent, tonic::Status>>,
) -> ReceiverStream<Result<StreamEvent, tonic::Status>> {
    ReceiverStream::new(rx)
}

/// Compute local per-block hashes (BLAKE3, 16-byte) for a file.
async fn local_block_hashes(path: &Path, block_size: usize) -> Result<Vec<String>> {
    let mut f = tokiofs::File::open(path).await?;
    let size = f.metadata().await?.len();
    let mut out = Vec::with_capacity(((size + block_size as u64 - 1) / block_size as u64) as usize);
    let mut offset = 0u64;
    let mut buf = vec![0u8; block_size];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let mut h = Sha256::new();
        h.update(&buf[..n]);
        let hash = h.finalize();
        // 16 bytes (similar to remote B2b-128) to keep lines small
        out.push(format!("{:x}", hash));
        offset += n as u64;
    }
    Ok(out)
}

/// Convert bytes to lowercase hex
fn hex16(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Helper: return mtime (seconds) from std metadata across platforms.
fn filetime_secs(meta: &std::fs::Metadata) -> Result<i64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(meta.mtime())
    }
    #[cfg(not(unix))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        let mt = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        Ok(mt.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64)
    }
}

/// Build a remote path by appending a relative path to a remote directory.
fn join_remote(base: &str, rel: &Path) -> String {
    let mut s = base.trim_end_matches('/').to_string();
    for comp in rel.components() {
        if let std::path::Component::Normal(os) = comp {
            s.push('/');
            s.push_str(&os.to_string_lossy());
        }
    }
    s
}

/// Very small, safe-ish shell escaper for paths.
pub(crate) fn sh_escape(p: &str) -> String {
    let mut out = String::from("'");
    out.push_str(&p.replace('\'', r"'\''"));
    out.push('\'');
    out
}

async fn upload_single_file(
    sftp: &SftpSession,
    local_path: &PathBuf,
    remote_path: &str,
    block_size: usize,
) -> anyhow::Result<()> {
    // File just doesn't exist yet we simply copy it over SFTP
    log::debug!(
        "Uploading file over sftp: {} -> {}",
        local_path.to_string_lossy(),
        remote_path
    );
    let mut lf = tokiofs::File::open(local_path).await?;
    // Removed flag APPEND because it's not needed
    let flags = OpenFlags::READ
        .union(OpenFlags::WRITE)
        .union(OpenFlags::CREATE);
    let mut rfile = sftp.open_with_flags(remote_path, flags).await?;
    let mut offset = 0u64;
    let mut buf = vec![0u8; block_size];
    loop {
        let n = lf.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        rfile.seek(std::io::SeekFrom::Start(offset)).await?;
        rfile.write_all(&buf[..n]).await?;
        offset += n as u64;
    }
    rfile.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{PathFilter, SyncFilterAction, SyncFilterRule};
    use std::path::Path;

    fn rule(action: SyncFilterAction, pattern: &str) -> SyncFilterRule {
        SyncFilterRule {
            action,
            pattern: pattern.to_string(),
        }
    }

    #[test]
    fn filter_order_first_match_wins() {
        let filter = PathFilter::new(&[
            rule(SyncFilterAction::Include, "*.rs"),
            rule(SyncFilterAction::Exclude, "*"),
        ])
        .unwrap();
        assert!(filter.should_include(Path::new("src/lib.rs"), false));
        assert!(!filter.should_include(Path::new("Cargo.toml"), false));

        let filter = PathFilter::new(&[
            rule(SyncFilterAction::Exclude, "*.rs"),
            rule(SyncFilterAction::Include, "*.rs"),
        ])
        .unwrap();
        assert!(!filter.should_include(Path::new("src/lib.rs"), false));
    }

    #[test]
    fn filter_matches_basename_and_paths() {
        let filter = PathFilter::new(&[
            rule(SyncFilterAction::Include, "src/*.rs"),
            rule(SyncFilterAction::Exclude, "*"),
        ])
        .unwrap();
        assert!(filter.should_include(Path::new("src/lib.rs"), false));
        assert!(!filter.should_include(Path::new("lib.rs"), false));

        let filter = PathFilter::new(&[
            rule(SyncFilterAction::Include, "*.rs"),
            rule(SyncFilterAction::Exclude, "*"),
        ])
        .unwrap();
        assert!(filter.should_include(Path::new("src/lib.rs"), false));
    }

    #[test]
    fn filter_respects_anchor_and_directory_only() {
        let filter = PathFilter::new(&[
            rule(SyncFilterAction::Include, "/src/*.rs"),
            rule(SyncFilterAction::Exclude, "*"),
        ])
        .unwrap();
        assert!(filter.should_include(Path::new("src/lib.rs"), false));
        assert!(!filter.should_include(Path::new("foo/src/lib.rs"), false));

        let filter = PathFilter::new(&[rule(SyncFilterAction::Exclude, "target/")]).unwrap();
        assert!(!filter.should_include(Path::new("target"), true));
        assert!(filter.should_include(Path::new("target"), false));
    }
}
