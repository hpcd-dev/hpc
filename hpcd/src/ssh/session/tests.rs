use super::{SessionManager, SessionManagerTestHooks};
use anyhow::Result;
use crate::ssh::sync::{SyncFilterRule, SyncOptions, sync_dir_with_executor};
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tokio::sync::mpsc;

#[tokio::test]
async fn session_manager_executor_uses_test_hooks() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();

    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "lib").unwrap();

    let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let connect_calls = Arc::clone(&calls);
    let ensure_connected = Arc::new(
        move |_: &mpsc::Sender<Result<proto::StreamEvent, tonic::Status>>,
              _: &mut mpsc::Receiver<proto::MfaAnswer>| {
            let connect_calls = Arc::clone(&connect_calls);
            let fut: crate::ssh::sync::BoxFuture<'static, Result<()>> = Box::pin(async move {
                connect_calls.lock().unwrap().push("connect".to_string());
                Ok(())
            });
            fut
        },
    );

    let mkdir_calls = Arc::clone(&calls);
    let ensure_remote_dir = Arc::new(move |remote_dir: &str| {
        let mkdir_calls = Arc::clone(&mkdir_calls);
        let remote_dir = remote_dir.to_string();
        let fut: crate::ssh::sync::BoxFuture<'static, Result<()>> = Box::pin(async move {
            mkdir_calls
                .lock()
                .unwrap()
                .push(format!("mkdir:{remote_dir}"));
            Ok(())
        });
        fut
    });

    let sync_calls = Arc::clone(&calls);
    let sync_one_file = Arc::new(
        move |_local: &Path, remote: &str, _session_id: &str, _block_size: usize| {
            let sync_calls = Arc::clone(&sync_calls);
            let remote = remote.to_string();
            let fut: crate::ssh::sync::BoxFuture<'static, Result<()>> = Box::pin(async move {
                sync_calls.lock().unwrap().push(format!("sync:{remote}"));
                Ok(())
            });
            fut
        },
    );

    let hooks = SessionManagerTestHooks {
        ensure_connected,
        ensure_remote_dir,
        sync_one_file,
    };

    let params = super::SshParams {
        addr: "127.0.0.1:22".parse::<SocketAddr>().unwrap(),
        username: "test".to_string(),
        identity_path: None,
        ki_submethods: None,
        keepalive_secs: 1,
    };
    let mut manager = SessionManager::new(params);
    manager.set_test_hooks(hooks);

    let (evt_tx, _evt_rx) = mpsc::channel::<Result<proto::StreamEvent, tonic::Status>>(1);
    let (_mfa_tx, mfa_rx) = mpsc::channel::<proto::MfaAnswer>(1);
    let options = SyncOptions {
        block_size: None,
        parallelism: Some(1),
        filters: &[] as &[SyncFilterRule],
    };

    sync_dir_with_executor(&manager, root, "/remote", options, &evt_tx, mfa_rx)
        .await
        .unwrap();

    let calls = calls.lock().unwrap().clone();
    assert!(calls.contains(&"connect".to_string()));
    assert!(calls.contains(&"mkdir:/remote".to_string()));
    assert!(calls.contains(&"mkdir:/remote/src".to_string()));
    assert!(calls.contains(&"sync:/remote/src/lib.rs".to_string()));
}

#[tokio::test]
async fn needs_connect_true_without_handle() {
    let params = super::SshParams {
        addr: "127.0.0.1:22".parse::<SocketAddr>().unwrap(),
        username: "test".to_string(),
        identity_path: None,
        ki_submethods: None,
        keepalive_secs: 1,
    };
    let manager = SessionManager::new(params);
    assert!(manager.needs_connect().await);
}
