//! The watcher's own IPC channel: a Unix socket that streams the state as
//! newline-delimited JSON (the same object as the state file, on one line).
//!
//! - on connect the client receives the latest state (if there is one),
//! - then one line per published change,
//! - sending the line `get` asks for the latest state again.
//!
//! Try it with:  socat - UNIX-CONNECT:/run/remoteplayer-mpvwatcher/state.sock

use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

/// The latest line, shared by all client tasks.
///
/// Ownership: `Arc<str>` is one heap string with an atomic reference count.
/// Every client gets a clone of the Arc (a pointer copy), not of the text —
/// comparable to handing out `std::shared_ptr<const std::string>`.
type LatestLine = Option<Arc<str>>;

pub struct SocketPublisher {
    path: PathBuf,
    latest: watch::Sender<LatestLine>,
}

enum ClientWakeup {
    Changed(Result<(), watch::error::RecvError>),
    Request(std::io::Result<Option<String>>),
}

impl SocketPublisher {
    pub fn start(path: &Path, initial: Option<String>) -> std::io::Result<SocketPublisher> {
        remove_stale_socket(path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener: UnixListener = UnixListener::bind(path)?;
        let (sender, receiver): (watch::Sender<LatestLine>, watch::Receiver<LatestLine>) =
            watch::channel(initial.map(Arc::from));
        tokio::spawn(accept_clients(listener, receiver));
        Ok(SocketPublisher { path: path.to_path_buf(), latest: sender })
    }

    pub fn publish(&self, line: String) {
        self.latest.send_replace(Some(Arc::from(line)));
    }
}

impl Drop for SocketPublisher {
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

async fn accept_clients(listener: UnixListener, latest: watch::Receiver<LatestLine>) {
    loop {
        match listener.accept().await {
            Ok((stream, _address)) => {
                tokio::spawn(serve_client(stream, latest.clone()));
            }
            Err(error) => {
                tracing::warn!(%error, "cannot accept a state socket client");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

async fn serve_client(stream: UnixStream, mut latest: watch::Receiver<LatestLine>) {
    let (read_half, mut write_half): (OwnedReadHalf, OwnedWriteHalf) = stream.into_split();
    let mut requests: Lines<BufReader<OwnedReadHalf>> = BufReader::new(read_half).lines();
    if send_latest(&mut write_half, &mut latest).await.is_err() {
        return;
    }
    loop {
        let wakeup: ClientWakeup = tokio::select! {
            changed = latest.changed() => ClientWakeup::Changed(changed),
            request = requests.next_line() => ClientWakeup::Request(request),
        };
        let sent: std::io::Result<()> = match wakeup {
            ClientWakeup::Changed(Ok(())) => send_latest(&mut write_half, &mut latest).await,
            ClientWakeup::Request(Ok(Some(line))) if line.trim() == "get" => {
                send_latest(&mut write_half, &mut latest).await
            }
            ClientWakeup::Request(Ok(Some(_other))) => Ok(()),
            ClientWakeup::Changed(Err(_)) | ClientWakeup::Request(Ok(None)) | ClientWakeup::Request(Err(_)) => return,
        };
        if sent.is_err() {
            return;
        }
    }
}

async fn send_latest(writer: &mut OwnedWriteHalf, latest: &mut watch::Receiver<LatestLine>) -> std::io::Result<()> {
    // Clone the Arc out, so the watch lock is released before the (slow) write.
    let line: LatestLine = latest.borrow_and_update().clone();
    match line {
        Some(text) => {
            writer.write_all(text.as_bytes()).await?;
            writer.write_all(b"\n").await
        }
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn read_line(lines: &mut Lines<BufReader<OwnedReadHalf>>) -> String {
        tokio::time::timeout(Duration::from_secs(2), lines.next_line())
            .await
            .expect("line in time")
            .expect("read")
            .expect("line")
    }

    #[tokio::test]
    async fn clients_get_the_latest_line_then_every_update() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let path: PathBuf = directory.path().join("state.sock");
        let publisher: SocketPublisher =
            SocketPublisher::start(&path, Some("{\"sequence\":1}".to_string())).expect("start");

        let stream: UnixStream = UnixStream::connect(&path).await.expect("connect");
        let (read_half, mut write_half): (OwnedReadHalf, OwnedWriteHalf) = stream.into_split();
        let mut lines: Lines<BufReader<OwnedReadHalf>> = BufReader::new(read_half).lines();
        assert_eq!(read_line(&mut lines).await, "{\"sequence\":1}");

        publisher.publish("{\"sequence\":2}".to_string());
        assert_eq!(read_line(&mut lines).await, "{\"sequence\":2}");

        write_half.write_all(b"get\n").await.expect("request");
        assert_eq!(read_line(&mut lines).await, "{\"sequence\":2}");
    }

    #[tokio::test]
    async fn a_regular_file_at_the_socket_path_is_not_deleted() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let path: PathBuf = directory.path().join("important.txt");
        std::fs::write(&path, "keep me").expect("seed");
        assert!(SocketPublisher::start(&path, None).is_err());
        assert_eq!(std::fs::read_to_string(&path).expect("still there"), "keep me");
    }
}
