//! The watcher's own IPC channel: request/response over a Unix socket, one
//! line each way. Nothing is ever sent unasked, so how often mpv is queried is
//! decided by the client (e.g. remoteplayer-restservice), not by the watcher.
//!
//! - `get`  → mpv's state right now, as one line of JSON (`LiveState`),
//!            or `{"error": "..."}` when mpv cannot be reached;
//! - anything else → `{"error": "..."}`; empty lines are ignored.
//!
//! A client may keep the connection open and send `get` as often as it likes.
//! Try it with:  echo get | socat - UNIX-CONNECT:/run/remoteplayer-mpvwatcher/state.sock

use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};

use crate::mpv_connection::MpvError;
use crate::sampler::{Sampler, TimedCapture};
use crate::snapshot::{FORMAT_VERSION, LiveState};

pub struct SocketServer {
    path: PathBuf,
}

impl SocketServer {
    // Ownership: the server only needs shared access to the sampler; every
    // client task gets its own clone of the Arc (a reference-count increment).
    pub fn start(path: &Path, sampler: Arc<Sampler>) -> std::io::Result<SocketServer> {
        remove_stale_socket(path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener: UnixListener = UnixListener::bind(path)?;
        tokio::spawn(accept_clients(listener, sampler));
        Ok(SocketServer { path: path.to_path_buf() })
    }
}

impl Drop for SocketServer {
    fn drop(&mut self) {
        let _ignored: std::io::Result<()> = std::fs::remove_file(&self.path);
    }
}

/// A socket file left behind by a crashed run would make `bind` fail.
/// Only sockets are removed, so a misconfigured path cannot delete a real file.
fn remove_stale_socket(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(path),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a socket", path.display()),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

async fn accept_clients(listener: UnixListener, sampler: Arc<Sampler>) {
    loop {
        match listener.accept().await {
            Ok((stream, _address)) => {
                tokio::spawn(serve_client(stream, Arc::clone(&sampler)));
            }
            Err(error) => {
                tracing::warn!(%error, "cannot accept a state socket client");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

async fn serve_client(stream: UnixStream, sampler: Arc<Sampler>) {
    let (read_half, mut write_half): (OwnedReadHalf, OwnedWriteHalf) = stream.into_split();
    let mut requests: Lines<BufReader<OwnedReadHalf>> = BufReader::new(read_half).lines();
    while let Ok(Some(line)) = requests.next_line().await {
        let request: &str = line.trim();
        if request.is_empty() {
            continue;
        }
        let reply: String = answer(request, &sampler).await;
        if write_half.write_all(reply.as_bytes()).await.is_err() || write_half.write_all(b"\n").await.is_err() {
            return;
        }
    }
}

async fn answer(request: &str, sampler: &Sampler) -> String {
    if request != "get" {
        return json!({ "error": format!("unknown request `{request}`; supported: get") }).to_string();
    }
    let captured: Result<TimedCapture, MpvError> = sampler.capture_live().await;
    match captured {
        Ok(timed) => {
            let state: LiveState = LiveState {
                format_version: FORMAT_VERSION,
                captured_at_unix_ms: timed.captured_at_unix_ms,
                mpv: timed.capture.process,
                player: timed.capture.player,
            };
            serde_json::to_string(&state)
                .unwrap_or_else(|error: serde_json::Error| json!({ "error": error.to_string() }).to_string())
        }
        Err(error) => json!({ "error": error.to_string() }).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpv_connection::tests::start_fake_mpv;
    use crate::sampler::tests::idle_mpv;
    use serde_json::Value;
    use std::time::Duration;

    async fn read_line(lines: &mut Lines<BufReader<OwnedReadHalf>>) -> Value {
        let line: String = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
            .await
            .expect("line in time")
            .expect("read")
            .expect("line");
        serde_json::from_str(&line).expect("json line")
    }

    async fn connect(path: &Path) -> (Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf) {
        let stream: UnixStream = UnixStream::connect(path).await.expect("connect");
        let (read_half, write_half): (OwnedReadHalf, OwnedWriteHalf) = stream.into_split();
        (BufReader::new(read_half).lines(), write_half)
    }

    #[tokio::test]
    async fn nothing_is_sent_unasked_and_get_returns_live_state() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let mpv_socket: PathBuf = start_fake_mpv(directory.path(), idle_mpv());
        let sampler: Arc<Sampler> = Arc::new(Sampler::new(mpv_socket, Duration::from_secs(1)));
        let path: PathBuf = directory.path().join("state.sock");
        let _server: SocketServer = SocketServer::start(&path, sampler).expect("start");

        let (mut lines, mut writer): (Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf) = connect(&path).await;
        let unasked: Result<std::io::Result<Option<String>>, tokio::time::error::Elapsed> =
            tokio::time::timeout(Duration::from_millis(300), lines.next_line()).await;
        assert!(unasked.is_err(), "the server must not send anything before a request");

        writer.write_all(b"get\n").await.expect("request");
        let state: Value = read_line(&mut lines).await;
        assert_eq!(state["format-version"], json!(1));
        assert_eq!(state["mpv"]["pid"], json!(4242));
        assert_eq!(state["player"]["volume"], json!(70.0));
        assert!(state.get("sequence").is_none());

        writer.write_all(b"\nhello\n").await.expect("request");
        let error: Value = read_line(&mut lines).await;
        assert!(error["error"].as_str().expect("error text").contains("unknown request"));
    }

    #[tokio::test]
    async fn unreachable_mpv_is_reported_as_an_error_line() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let sampler: Arc<Sampler> =
            Arc::new(Sampler::new(directory.path().join("absent.sock"), Duration::from_secs(1)));
        let path: PathBuf = directory.path().join("state.sock");
        let _server: SocketServer = SocketServer::start(&path, sampler).expect("start");

        let (mut lines, mut writer): (Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf) = connect(&path).await;
        writer.write_all(b"get\n").await.expect("request");
        let reply: Value = read_line(&mut lines).await;
        assert!(reply["error"].as_str().expect("error text").contains("cannot connect"));
    }

    #[tokio::test]
    async fn a_regular_file_at_the_socket_path_is_not_deleted() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let path: PathBuf = directory.path().join("important.txt");
        std::fs::write(&path, "keep me").expect("seed");
        let sampler: Arc<Sampler> = Arc::new(Sampler::new(directory.path().join("mpv.sock"), Duration::from_secs(1)));
        assert!(SocketServer::start(&path, sampler).is_err());
        assert_eq!(std::fs::read_to_string(&path).expect("still there"), "keep me");
    }
}
