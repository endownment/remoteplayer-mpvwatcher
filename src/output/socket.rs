//! The watcher's own IPC channel: request/response over a Unix socket, one
//! line each way. Nothing is ever sent unasked.
//!
//! - `get`    → mpv's state right now (asks mpv), or `"mpv-reachable": false`;
//! - `last`   → the snapshot to restore from (`"known": false` if there is none);
//! - `status` → connection and state-file diagnostics;
//! - anything else → `{"error": "..."}`; empty lines are ignored.
//!
//! A client may keep the connection open and send any number of requests.
//! Try it with:  echo last | socat - UNIX-CONNECT:/run/remoteplayer-mpvwatcher/state.sock

use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};

use crate::mpv_connection::MpvError;
use crate::sampler::{Sampler, TimedCapture, unix_ms_now};
use crate::shared::SharedState;
use crate::snapshot::{FORMAT_VERSION, MpvProcessInfo, PlayerSnapshot, SavedPlayerState};

/// The reply to `get`. When mpv cannot be reached, `mpv` and `player` are
/// absent and `detail` says why.
#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
struct LiveReply {
    format_version: u32,
    captured_at_unix_ms: u64,
    mpv_reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    mpv: Option<MpvProcessInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    player: Option<PlayerSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

/// The reply to `last`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
struct LastReply {
    format_version: u32,
    known: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<SavedPlayerState>,
}

/// What every client task needs; cloned (cheaply, two Arcs) into each of them.
#[derive(Clone)]
struct Services {
    sampler: Arc<Sampler>,
    shared: Arc<SharedState>,
}

pub struct SocketServer {
    path: PathBuf,
}

impl SocketServer {
    // Ownership: the server only needs shared access to the sampler and the
    // shared state; every client task gets its own clone of the Arcs (a
    // reference-count increment each).
    pub fn start(path: &Path, sampler: Arc<Sampler>, shared: Arc<SharedState>) -> std::io::Result<SocketServer> {
        remove_stale_socket(path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener: UnixListener = UnixListener::bind(path)?;
        tokio::spawn(accept_clients(listener, Services { sampler, shared }));
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

async fn accept_clients(listener: UnixListener, services: Services) {
    loop {
        match listener.accept().await {
            Ok((stream, _address)) => {
                tokio::spawn(serve_client(stream, services.clone()));
            }
            Err(error) => {
                tracing::warn!(%error, "cannot accept a state socket client");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

async fn serve_client(stream: UnixStream, services: Services) {
    let (read_half, mut write_half): (OwnedReadHalf, OwnedWriteHalf) = stream.into_split();
    let mut requests: Lines<BufReader<OwnedReadHalf>> = BufReader::new(read_half).lines();
    while let Ok(Some(line)) = requests.next_line().await {
        let request: &str = line.trim();
        if request.is_empty() {
            continue;
        }
        let reply: String = answer(request, &services).await;
        if write_half.write_all(reply.as_bytes()).await.is_err() || write_half.write_all(b"\n").await.is_err() {
            return;
        }
    }
}

async fn answer(request: &str, services: &Services) -> String {
    let encoded: Result<String, serde_json::Error> = match request {
        "get" => serde_json::to_string(&live_reply(&services.sampler).await),
        "last" => {
            let last: Option<SavedPlayerState> = services.shared.last();
            serde_json::to_string(&LastReply { format_version: FORMAT_VERSION, known: last.is_some(), state: last })
        }
        "status" => serde_json::to_string(&services.shared.status()),
        _ => {
            return json!({ "error": format!("unknown request `{request}`; supported: get, last, status") })
                .to_string();
        }
    };
    encoded.unwrap_or_else(|error: serde_json::Error| json!({ "error": error.to_string() }).to_string())
}

async fn live_reply(sampler: &Sampler) -> LiveReply {
    let captured: Result<TimedCapture, MpvError> = sampler.capture_live().await;
    match captured {
        Ok(timed) => LiveReply {
            format_version: FORMAT_VERSION,
            captured_at_unix_ms: timed.captured_at_unix_ms,
            mpv_reachable: true,
            mpv: Some(timed.capture.process),
            player: Some(timed.capture.player),
            detail: None,
        },
        Err(error) => LiveReply {
            format_version: FORMAT_VERSION,
            captured_at_unix_ms: unix_ms_now(),
            mpv_reachable: false,
            mpv: None,
            player: None,
            detail: Some(error.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpv_connection::tests::start_fake_mpv;
    use crate::output::StateFileMode;
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

    fn services(mpv_socket: PathBuf, last: Option<SavedPlayerState>) -> (Arc<Sampler>, Arc<SharedState>) {
        (
            Arc::new(Sampler::new(mpv_socket, Duration::from_secs(1))),
            Arc::new(SharedState::new(StateFileMode::NotConfigured, last)),
        )
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
        let (sampler, shared): (Arc<Sampler>, Arc<SharedState>) = services(mpv_socket, None);
        let path: PathBuf = directory.path().join("state.sock");
        let _server: SocketServer = SocketServer::start(&path, sampler, shared).expect("start");

        let (mut lines, mut writer): (Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf) = connect(&path).await;
        let unasked: Result<std::io::Result<Option<String>>, tokio::time::error::Elapsed> =
            tokio::time::timeout(Duration::from_millis(300), lines.next_line()).await;
        assert!(unasked.is_err(), "the server must not send anything before a request");

        writer.write_all(b"get\n").await.expect("request");
        let state: Value = read_line(&mut lines).await;
        assert_eq!(state["format-version"], json!(1));
        assert_eq!(state["mpv-reachable"], json!(true));
        assert_eq!(state["mpv"]["pid"], json!(4242));
        assert_eq!(state["player"]["volume"], json!(70.0));
        assert!(state.get("sequence").is_none());

        writer.write_all(b"last\n").await.expect("request");
        let last: Value = read_line(&mut lines).await;
        assert_eq!(last, json!({"format-version": 1, "known": false}));

        writer.write_all(b"status\n").await.expect("request");
        let status: Value = read_line(&mut lines).await;
        assert_eq!(status["state-file"]["mode"], json!("not-configured"));
        assert_eq!(status["last-known"], json!(false));

        writer.write_all(b"\nhello\n").await.expect("request");
        let error: Value = read_line(&mut lines).await;
        assert!(error["error"].as_str().expect("error text").contains("unknown request"));
    }

    #[tokio::test]
    async fn without_mpv_get_says_unreachable_and_last_still_answers() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let (sampler, shared): (Arc<Sampler>, Arc<SharedState>) = services(directory.path().join("absent.sock"), None);
        let path: PathBuf = directory.path().join("state.sock");
        let _server: SocketServer = SocketServer::start(&path, sampler, shared).expect("start");

        let (mut lines, mut writer): (Lines<BufReader<OwnedReadHalf>>, OwnedWriteHalf) = connect(&path).await;
        writer.write_all(b"get\nlast\n").await.expect("requests");
        let live: Value = read_line(&mut lines).await;
        assert_eq!(live["mpv-reachable"], json!(false));
        assert!(live.get("player").is_none());
        assert!(live["detail"].as_str().expect("detail").contains("cannot connect"));
        let last: Value = read_line(&mut lines).await;
        assert_eq!(last["known"], json!(false));
    }

    #[tokio::test]
    async fn a_regular_file_at_the_socket_path_is_not_deleted() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let path: PathBuf = directory.path().join("important.txt");
        std::fs::write(&path, "keep me").expect("seed");
        let (sampler, shared): (Arc<Sampler>, Arc<SharedState>) = services(directory.path().join("mpv.sock"), None);
        assert!(SocketServer::start(&path, sampler, shared).is_err());
        assert_eq!(std::fs::read_to_string(&path).expect("still there"), "keep me");
    }
}
