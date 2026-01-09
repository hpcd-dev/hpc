// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Alex Sizykh

use anyhow::{Context, Result, anyhow};
use proto::{MfaAnswer, SubmitStreamEvent};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tokio::fs as tokiofs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;

use super::SessionManager;
use crate::ssh::sync::{BoxFuture, SyncExecutor, SyncOptions, sync_dir_with_executor};
use crate::ssh::utils::{
    build_remote_dir_paths, build_remote_hash_script, local_block_hashes, parse_remote_hash_output,
    sh_escape,
};

impl SessionManager {
    async fn sftp(&self) -> Result<SftpSession> {
        // TODO: this function is using dummy auth channels right now - for production use, they
        // MUST be hooked to user-guided MFA
        let guard = self.handle.lock().await;
        let handle = guard
            .as_ref()
            .ok_or_else(|| anyhow!("SSH handle lost before opening SFTP"))?;
        let channel = handle.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        let sftp = SftpSession::new(channel.into_stream()).await?;
        Ok(sftp)
    }

    pub(crate) async fn ensure_remote_dir_for_sync(&self, remote_dir: &str) -> Result<()> {
        #[cfg(test)]
        if let Some(hooks) = &self.test_hooks {
            return (hooks.ensure_remote_dir)(remote_dir).await;
        }
        let sftp = self.sftp().await?;
        self.ensure_remote_dir_with_sftp(&sftp, remote_dir).await
    }

    async fn ensure_remote_dir_with_sftp(
        &self,
        sftp_session: &SftpSession,
        remote_dir: &str,
    ) -> Result<()> {
        log::debug!("remote dir: {remote_dir}");
        for cur in build_remote_dir_paths(remote_dir) {
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
                    let attrs = FileAttributes {
                        permissions: Some(0o700),
                        ..Default::default()
                    };
                    sftp_session
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
        let attrs = FileAttributes {
            permissions: Some(0o700),
            ..Default::default()
        };
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
        let py_script = build_remote_hash_script(remote_path, block_size);
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
        parse_remote_hash_output(&out)
    }

    /// Syncs a single file from source to destination.
    /// Is kinda stable, but definitely needs more testing.
    async fn sync_one_file_with_sftp(
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
                    rfile
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

    async fn sync_one_file_for_sync(
        &self,
        local_path: &Path,
        remote_path: &str,
        session_id: &str,
        block_size: usize,
    ) -> Result<()> {
        #[cfg(test)]
        if let Some(hooks) = &self.test_hooks {
            return (hooks.sync_one_file)(local_path, remote_path, session_id, block_size).await;
        }
        let sftp = self.sftp().await?;
        self.sync_one_file_with_sftp(&sftp, local_path, remote_path, session_id, block_size)
            .await
    }

    /// Sync an entire local directory tree into a remote directory.
    /// - Creates remote directories as needed
    /// - Skips files where remote mtime >= local mtime
    /// - Uses block-delta writes for changed files
    pub async fn sync_dir<P: AsRef<Path>>(
        &self,
        local_dir: P,
        remote_dir: &str,
        options: SyncOptions<'_>,
        evt_tx: &mpsc::Sender<Result<SubmitStreamEvent, tonic::Status>>,
        mfa_rx: mpsc::Receiver<MfaAnswer>,
    ) -> Result<()> {
        sync_dir_with_executor(self, local_dir, remote_dir, options, evt_tx, mfa_rx).await
    }

    pub async fn retrieve_path(
        &self,
        remote_path: &str,
        local_path: &Path,
        overwrite: bool,
    ) -> Result<()> {
        let sftp = self.sftp().await?;
        let meta = sftp.metadata(remote_path).await?;
        if !overwrite {
            if meta.is_dir() {
                ensure_no_dir_overwrite(&sftp, remote_path, local_path).await?;
            } else {
                ensure_no_file_overwrite(local_path).await?;
            }
        }
        if meta.is_dir() {
            download_dir(&sftp, remote_path, local_path).await
        } else {
            download_file(&sftp, remote_path, local_path).await
        }
    }
}

impl SyncExecutor for SessionManager {
    fn ensure_connected<'a>(
        &'a self,
        evt_tx: &'a mpsc::Sender<Result<SubmitStreamEvent, tonic::Status>>,
        mfa_rx: &'a mut mpsc::Receiver<MfaAnswer>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.ensure_connected_for_sync(evt_tx, mfa_rx).await })
    }

    fn ensure_remote_dir<'a>(&'a self, remote_dir: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.ensure_remote_dir_for_sync(remote_dir).await })
    }

    fn sync_one_file<'a>(
        &'a self,
        local_path: &'a Path,
        remote_path: &'a str,
        session_id: &'a str,
        block_size: usize,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.sync_one_file_for_sync(local_path, remote_path, session_id, block_size)
                .await
        })
    }
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

async fn download_file(sftp: &SftpSession, remote_path: &str, local_path: &Path) -> Result<()> {
    if let Some(parent) = local_path.parent() {
        tokiofs::create_dir_all(parent).await?;
    }
    let mut rfile = sftp.open(remote_path).await?;
    let mut lfile = tokiofs::File::create(local_path).await?;
    tokio::io::copy(&mut rfile, &mut lfile).await?;
    lfile.flush().await?;
    Ok(())
}

async fn download_dir(sftp: &SftpSession, remote_dir: &str, local_dir: &Path) -> Result<()> {
    let mut stack: Vec<(String, PathBuf)> = vec![(
        remote_dir.trim_end_matches('/').to_string(),
        local_dir.to_path_buf(),
    )];

    while let Some((remote_base, local_base)) = stack.pop() {
        tokiofs::create_dir_all(&local_base).await?;
        let entries = sftp.read_dir(&remote_base).await?;
        for entry in entries {
            let name = entry.file_name();
            let remote_child = format!("{}/{}", remote_base, name);
            let local_child = local_base.join(&name);
            let meta = entry.metadata();
            if meta.is_dir() {
                stack.push((remote_child, local_child));
            } else {
                download_file(sftp, &remote_child, &local_child).await?;
            }
        }
    }
    Ok(())
}

async fn ensure_no_file_overwrite(path: &Path) -> Result<()> {
    match tokiofs::metadata(path).await {
        Ok(_) => Err(io::Error::new(
            ErrorKind::AlreadyExists,
            format!("local path exists: {}", path.display()),
        )
        .into()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

async fn ensure_no_dir_overwrite(
    sftp: &SftpSession,
    remote_dir: &str,
    local_dir: &Path,
) -> Result<()> {
    let mut stack: Vec<(String, PathBuf)> = vec![(
        remote_dir.trim_end_matches('/').to_string(),
        local_dir.to_path_buf(),
    )];

    while let Some((remote_base, local_base)) = stack.pop() {
        match tokiofs::metadata(&local_base).await {
            Ok(meta) => {
                if !meta.is_dir() {
                    return Err(io::Error::new(
                        ErrorKind::AlreadyExists,
                        format!("local path exists: {}", local_base.display()),
                    )
                    .into());
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        let entries = sftp.read_dir(&remote_base).await?;
        for entry in entries {
            let name = entry.file_name();
            let remote_child = format!("{}/{}", remote_base, name);
            let local_child = local_base.join(&name);
            let meta = entry.metadata();
            if meta.is_dir() {
                stack.push((remote_child, local_child));
            } else {
                ensure_no_file_overwrite(&local_child).await?;
            }
        }
    }
    Ok(())
}
