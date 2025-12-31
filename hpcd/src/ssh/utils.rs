use anyhow::Result;
use proto::StreamEvent;
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::fs as tokiofs;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Helper to wrap an mpsc receiver as a tonic stream.
pub fn receiver_to_stream(
    rx: mpsc::Receiver<Result<StreamEvent, tonic::Status>>,
) -> ReceiverStream<Result<StreamEvent, tonic::Status>> {
    ReceiverStream::new(rx)
}

/// Very small, safe-ish shell escaper for paths.
pub(crate) fn sh_escape(p: &str) -> String {
    let mut out = String::from("'");
    out.push_str(&p.replace('\'', r"'\''"));
    out.push('\'');
    out
}

/// Compute local per-block hashes (SHA-256) for a file.
pub(crate) async fn local_block_hashes(path: &Path, block_size: usize) -> Result<Vec<String>> {
    let mut f = tokiofs::File::open(path).await?;
    let size = f.metadata().await?.len();
    let capacity = size.div_ceil(block_size as u64) as usize;
    let mut out = Vec::with_capacity(capacity);
    let mut buf = vec![0u8; block_size];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let mut h = Sha256::new();
        h.update(&buf[..n]);
        out.push(format!("{:x}", h.finalize()));
    }
    Ok(out)
}

pub(crate) fn build_remote_hash_script(remote_path: &str, block_size: usize) -> String {
    let escaped_path = remote_path.replace("\\", "\\\\").replace("'", "\\'");
    let quoted_path = format!("'{escaped_path}'");
    format!(
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
        rp = quoted_path,
        bs = block_size
    )
}

pub(crate) fn parse_remote_hash_output(output: &str) -> Result<Vec<String>> {
    let mut hashes = Vec::new();
    for line in output.lines() {
        if line.starts_with("ERR ") {
            anyhow::bail!("remote: {}", line);
        }
        let mut it = line.split_whitespace();
        let _off = it.next();
        let _len = it.next();
        if let Some(hex) = it.next() {
            hashes.push(hex.to_string());
        }
    }
    Ok(hashes)
}

pub(crate) fn build_remote_dir_paths(remote_dir: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut cur = String::from("");
    for comp in Path::new(remote_dir).components() {
        let seg = match comp {
            std::path::Component::RootDir => "/".to_string(),
            std::path::Component::Normal(os) => os.to_string_lossy().to_string(),
            std::path::Component::CurDir => continue,
            std::path::Component::ParentDir => continue,
            _ => continue,
        };
        if seg.is_empty() {
            continue;
        }
        if cur != "/" && !cur.is_empty() {
            cur = format!("{}/{}", cur.trim_end_matches('/'), seg);
        } else {
            cur = format!("/{}", seg);
        }
        paths.push(cur.clone());
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::{
        build_remote_dir_paths, build_remote_hash_script, local_block_hashes,
        parse_remote_hash_output, receiver_to_stream, sh_escape,
    };
    use proto::{StreamEvent, stream_event};
    use sha2::{Digest, Sha256};
    use std::fs;
    use tempfile::tempdir;
    use tokio::sync::mpsc;
    use tokio_stream::StreamExt;

    fn hash_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    #[tokio::test]
    async fn local_block_hashes_splits_by_block_size() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("data.bin");
        fs::write(&path, b"hello world").unwrap();

        let hashes = local_block_hashes(&path, 4).await.unwrap();

        let expected = vec![hash_hex(b"hell"), hash_hex(b"o wo"), hash_hex(b"rld")];
        assert_eq!(hashes, expected);
    }

    #[test]
    fn sh_escape_wraps_and_escapes_quotes() {
        assert_eq!(sh_escape("plain"), "'plain'");
        assert_eq!(sh_escape("a'b"), "'a'\\''b'");
    }

    #[tokio::test]
    async fn receiver_to_stream_forwards_messages() {
        let (tx, rx) = mpsc::channel(2);
        tx.send(Ok(StreamEvent {
            event: Some(stream_event::Event::ExitCode(0)),
        }))
        .await
        .unwrap();
        tx.send(Ok(StreamEvent {
            event: Some(stream_event::Event::ExitCode(1)),
        }))
        .await
        .unwrap();
        drop(tx);

        let mut stream = receiver_to_stream(rx);
        let mut codes = Vec::new();
        while let Some(item) = stream.next().await {
            let evt = item.unwrap();
            if let Some(stream_event::Event::ExitCode(code)) = evt.event {
                codes.push(code);
            }
        }

        assert_eq!(codes, vec![0, 1]);
    }

    #[test]
    fn build_remote_hash_script_includes_path_and_block_size() {
        let script = build_remote_hash_script("/tmp/hi", 123);
        assert!(script.contains("path = r'/tmp/hi'"));
        assert!(script.contains("bs = 123"));
    }

    #[test]
    fn parse_remote_hash_output_collects_hashes() {
        let output = "0 4 abcd\n4 4 efgh\n";
        let hashes = parse_remote_hash_output(output).unwrap();
        assert_eq!(hashes, vec!["abcd".to_string(), "efgh".to_string()]);
    }

    #[test]
    fn parse_remote_hash_output_errors_on_err_line() {
        let err = parse_remote_hash_output("ERR nope").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("remote: ERR nope"));
    }

    #[test]
    fn build_remote_dir_paths_handles_root_and_dot_segments() {
        let paths = build_remote_dir_paths("/tmp/./cache/../files");
        assert_eq!(
            paths,
            vec![
                "//".to_string(),
                "/tmp".to_string(),
                "/tmp/cache".to_string(),
                "/tmp/cache/files".to_string()
            ]
        );
    }
}
