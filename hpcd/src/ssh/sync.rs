use anyhow::Result;
use futures_util::StreamExt;
use proto::{MfaAnswer, StreamEvent};
use rand::{Rng, distr::Alphanumeric};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use tokio::sync::mpsc;

use super::sync_plan::build_sync_plan;

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

#[derive(Clone, Copy, Debug)]
pub struct SyncOptions<'a> {
    pub block_size: Option<usize>,
    pub parallelism: Option<usize>,
    pub filters: &'a [SyncFilterRule],
}

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Abstraction over the remote side of the sync so it can be mocked in tests.
pub(crate) trait SyncExecutor: Sync {
    /// Ensure the remote connection is established and MFA prompts can flow.
    fn ensure_connected<'a>(
        &'a self,
        evt_tx: &'a mpsc::Sender<Result<StreamEvent, tonic::Status>>,
        mfa_rx: &'a mut mpsc::Receiver<MfaAnswer>,
    ) -> BoxFuture<'a, Result<()>>;

    /// Ensure the given remote directory exists.
    fn ensure_remote_dir<'a>(&'a self, remote_dir: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Sync one local file to its remote destination.
    fn sync_one_file<'a>(
        &'a self,
        local_path: &'a Path,
        remote_path: &'a str,
        session_id: &'a str,
        block_size: usize,
    ) -> BoxFuture<'a, Result<()>>;
}

pub(crate) async fn sync_dir_with_executor<E, P>(
    executor: &E,
    local_dir: P,
    remote_dir: &str,
    options: SyncOptions<'_>,
    evt_tx: &mpsc::Sender<Result<StreamEvent, tonic::Status>>,
    mut mfa_rx: mpsc::Receiver<MfaAnswer>,
) -> Result<()>
where
    E: SyncExecutor + ?Sized,
    P: AsRef<Path>,
{
    let block_size = options.block_size.unwrap_or(1024 * 1024); // 1 MiB block size
    let parallelism = options.parallelism.unwrap_or(8);

    executor.ensure_connected(evt_tx, &mut mfa_rx).await?;
    let plan = build_sync_plan(local_dir, remote_dir, options.filters)?;

    log::info!("making sure the remote directory exists");
    executor.ensure_remote_dir(remote_dir).await?;
    for remote_dir in &plan.remote_dirs {
        executor.ensure_remote_dir(remote_dir).await?;
    }

    let session_id: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(10)
        .map(char::from)
        .collect();

    let results = futures::stream::iter(plan.items.into_iter().map(|item| {
        let session_id_clone = session_id.clone();
        async move {
            executor
                .sync_one_file(
                    &item.local_path,
                    &item.remote_path,
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

#[cfg(test)]
mod executor_tests {
    use super::{
        MfaAnswer, StreamEvent, SyncExecutor, SyncFilterAction, SyncFilterRule, SyncOptions,
        sync_dir_with_executor,
    };
    use anyhow::{Result, anyhow};
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;
    use tokio::sync::mpsc;

    #[derive(Clone, Default)]
    struct FakeExecutor {
        calls: Arc<Mutex<Vec<String>>>,
        fail_on: HashSet<String>,
    }

    impl FakeExecutor {
        fn new(fail_on: &[&str]) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                fail_on: fail_on.iter().map(|s| s.to_string()).collect(),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SyncExecutor for FakeExecutor {
        fn ensure_connected<'a>(
            &'a self,
            _evt_tx: &'a mpsc::Sender<Result<StreamEvent, tonic::Status>>,
            _mfa_rx: &'a mut mpsc::Receiver<MfaAnswer>,
        ) -> super::BoxFuture<'a, Result<()>> {
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls.lock().unwrap().push("connect".to_string());
                Ok(())
            })
        }

        fn ensure_remote_dir<'a>(
            &'a self,
            remote_dir: &'a str,
        ) -> super::BoxFuture<'a, Result<()>> {
            let calls = Arc::clone(&self.calls);
            let remote_dir = remote_dir.to_string();
            Box::pin(async move {
                calls.lock().unwrap().push(format!("mkdir:{remote_dir}"));
                Ok(())
            })
        }

        fn sync_one_file<'a>(
            &'a self,
            _local_path: &'a Path,
            remote_path: &'a str,
            _session_id: &'a str,
            _block_size: usize,
        ) -> super::BoxFuture<'a, Result<()>> {
            let calls = Arc::clone(&self.calls);
            let remote_path = remote_path.to_string();
            let fail_on = self.fail_on.clone();
            Box::pin(async move {
                calls.lock().unwrap().push(format!("sync:{remote_path}"));
                if fail_on.contains(&remote_path) {
                    return Err(anyhow!("forced error for {remote_path}"));
                }
                Ok(())
            })
        }
    }

    fn rule(action: SyncFilterAction, pattern: &str) -> SyncFilterRule {
        SyncFilterRule {
            action,
            pattern: pattern.to_string(),
        }
    }

    #[tokio::test]
    async fn sync_dir_calls_remote_dirs_before_sync() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        fs::create_dir_all(root.join("src/bin")).unwrap();
        fs::write(root.join("README.md"), "readme").unwrap();
        fs::write(root.join("src/lib.rs"), "lib").unwrap();
        fs::write(root.join("src/bin/main.rs"), "bin").unwrap();

        let executor = FakeExecutor::default();
        let (evt_tx, _evt_rx) = mpsc::channel::<Result<StreamEvent, tonic::Status>>(1);
        let (_mfa_tx, mfa_rx) = mpsc::channel::<MfaAnswer>(1);
        let filters = [rule(SyncFilterAction::Exclude, "target/")];
        let options = SyncOptions {
            block_size: None,
            parallelism: Some(1),
            filters: &filters,
        };

        sync_dir_with_executor(
            &executor,
            root,
            "/remote",
            options,
            &evt_tx,
            mfa_rx,
        )
        .await
        .unwrap();

        let calls = executor.calls();
        let expected_prefix = vec![
            "connect",
            "mkdir:/remote",
            "mkdir:/remote/src",
            "mkdir:/remote/src/bin",
        ];
        assert!(calls.len() >= expected_prefix.len());
        assert_eq!(calls[..expected_prefix.len()], expected_prefix);
    }

    #[tokio::test]
    async fn sync_dir_aggregates_sync_errors() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "lib").unwrap();
        fs::write(root.join("src/extra.rs"), "extra").unwrap();

        let executor = FakeExecutor::new(&["/remote/src/lib.rs"]);
        let (evt_tx, _evt_rx) = mpsc::channel::<Result<StreamEvent, tonic::Status>>(1);
        let (_mfa_tx, mfa_rx) = mpsc::channel::<MfaAnswer>(1);
        let options = SyncOptions {
            block_size: None,
            parallelism: Some(1),
            filters: &[],
        };

        let err = sync_dir_with_executor(
            &executor,
            root,
            "/remote",
            options,
            &evt_tx,
            mfa_rx,
        )
        .await
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("sync_dir encountered 1 error(s):"));
        assert!(msg.contains("forced error for /remote/src/lib.rs"));
    }
}

#[cfg(test)]
mod sync_option_tests {
    use super::{MfaAnswer, StreamEvent, SyncExecutor, SyncFilterRule, SyncOptions, sync_dir_with_executor};
    use anyhow::Result;
    use std::fs;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;
    use tokio::sync::mpsc;

    #[derive(Clone, Default)]
    struct BlockSizeExecutor {
        sizes: Arc<Mutex<Vec<usize>>>,
    }

    impl BlockSizeExecutor {
        fn sizes(&self) -> Vec<usize> {
            self.sizes.lock().unwrap().clone()
        }
    }

    impl SyncExecutor for BlockSizeExecutor {
        fn ensure_connected<'a>(
            &'a self,
            _evt_tx: &'a mpsc::Sender<Result<StreamEvent, tonic::Status>>,
            _mfa_rx: &'a mut mpsc::Receiver<MfaAnswer>,
        ) -> super::BoxFuture<'a, Result<()>> {
            Box::pin(async move { Ok(()) })
        }

        fn ensure_remote_dir<'a>(
            &'a self,
            _remote_dir: &'a str,
        ) -> super::BoxFuture<'a, Result<()>> {
            Box::pin(async move { Ok(()) })
        }

        fn sync_one_file<'a>(
            &'a self,
            _local_path: &'a Path,
            _remote_path: &'a str,
            _session_id: &'a str,
            block_size: usize,
        ) -> super::BoxFuture<'a, Result<()>> {
            let sizes = Arc::clone(&self.sizes);
            Box::pin(async move {
                sizes.lock().unwrap().push(block_size);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn sync_dir_uses_default_block_size_when_none() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();

        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "lib").unwrap();

        let executor = BlockSizeExecutor::default();
        let (evt_tx, _evt_rx) = mpsc::channel::<Result<StreamEvent, tonic::Status>>(1);
        let (_mfa_tx, mfa_rx) = mpsc::channel::<MfaAnswer>(1);
        let options = SyncOptions {
            block_size: None,
            parallelism: None,
            filters: &[] as &[SyncFilterRule],
        };

        sync_dir_with_executor(
            &executor,
            root,
            "/remote",
            options,
            &evt_tx,
            mfa_rx,
        )
        .await
        .unwrap();

        assert_eq!(executor.sizes(), vec![1024 * 1024]);
    }
}
