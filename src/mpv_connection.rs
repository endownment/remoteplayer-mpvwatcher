//! A deliberately simple mpv IPC client: one request at a time, wait for its
//! reply, done. It is used in two ways, each on its own connection:
//! - answering `get`: requests only, events switched off (`connect`);
//! - following mpv: subscribe with `observe_property`, then read events
//!   (`connect_with_events`, `observe_property`, `next_event`).
//!
//! Protocol: one JSON object per line. Requests carry a `request_id`, replies
//! echo it. See `man mpv`, section "JSON IPC".

use std::path::Path;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

#[derive(Debug, thiserror::Error)]
pub enum MpvError {
    #[error("cannot connect to {socket}: {source}")]
    Connect { socket: String, source: std::io::Error },
    #[error("connection to mpv lost: {0}")]
    Io(#[from] std::io::Error),
    #[error("mpv closed the connection")]
    Closed,
    #[error("mpv did not reply within {0:?}")]
    Timeout(Duration),
    #[error("mpv rejected `{property}`: {error}")]
    Rejected { property: String, error: String },
    #[error("unexpected value for `{property}`: {error}")]
    UnexpectedValue { property: String, error: serde_json::Error },
}

impl MpvError {
    /// Errors after which the connection must be dropped and re-established.
    pub fn breaks_connection(&self) -> bool {
        matches!(self, MpvError::Connect { .. } | MpvError::Io(_) | MpvError::Closed | MpvError::Timeout(_))
    }
}

/// An event from mpv. For `property-change`, `property` is the property name
/// and `data` its new value (`null` when it became unavailable).
#[derive(Debug, Clone, PartialEq)]
pub struct MpvEvent {
    pub name: String,
    pub property: Option<String>,
    pub data: Value,
}

pub struct MpvConnection {
    lines: Lines<BufReader<OwnedReadHalf>>,
    writer: OwnedWriteHalf,
    next_request_id: u64,
    timeout: Duration,
}

impl MpvConnection {
    pub async fn connect(socket: &Path, timeout: Duration) -> Result<MpvConnection, MpvError> {
        let mut connection: MpvConnection = MpvConnection::connect_with_events(socket, timeout).await?;
        // Events are noise for request/response use; this only affects our own connection.
        connection.command(&[json!("disable_event"), json!("all")]).await?;
        Ok(connection)
    }

    pub async fn connect_with_events(socket: &Path, timeout: Duration) -> Result<MpvConnection, MpvError> {
        let stream: UnixStream = UnixStream::connect(socket)
            .await
            .map_err(|source: std::io::Error| MpvError::Connect { socket: socket.display().to_string(), source })?;
        let (read_half, writer): (OwnedReadHalf, OwnedWriteHalf) = stream.into_split();
        Ok(MpvConnection { lines: BufReader::new(read_half).lines(), writer, next_request_id: 1, timeout })
    }

    /// Subscribes to changes of `property`. mpv answers with an immediate
    /// `property-change` event carrying the current value, then one per change.
    /// The reply is not awaited: waiting would skip that first event, and
    /// `next_event` ignores the reply when it arrives.
    pub async fn observe_property(&mut self, observer_id: u64, property: &str) -> Result<(), MpvError> {
        let request_id: u64 = self.take_request_id();
        let mut line: String =
            json!({ "command": ["observe_property", observer_id, property], "request_id": request_id }).to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        Ok(())
    }

    /// Waits for the next event, skipping replies. No timeout: an idle mpv may
    /// stay silent for hours. Cancel-safe — the only `.await` is `next_line`,
    /// so dropping this future inside `select!` loses no event.
    pub async fn next_event(&mut self) -> Result<MpvEvent, MpvError> {
        loop {
            let Some(line): Option<String> = self.lines.next_line().await? else {
                return Err(MpvError::Closed);
            };
            let Ok(message): Result<Value, serde_json::Error> = serde_json::from_str(&line) else {
                continue;
            };
            if let Some(name) = message.get("event").and_then(Value::as_str) {
                return Ok(MpvEvent {
                    name: name.to_string(),
                    property: message.get("name").and_then(Value::as_str).map(str::to_string),
                    data: message.get("data").cloned().unwrap_or(Value::Null),
                });
            }
        }
    }

    /// `Ok(None)` when mpv reports the property as unavailable (e.g. `time-pos` while idle).
    pub async fn get_property_optional<T: DeserializeOwned>(&mut self, property: &str) -> Result<Option<T>, MpvError> {
        let reply: Value = self.request(&[json!("get_property"), json!(property)]).await?;
        let error: &str = reply.get("error").and_then(Value::as_str).unwrap_or("missing error field");
        if error == "property unavailable" {
            return Ok(None);
        }
        if error != "success" {
            return Err(MpvError::Rejected { property: property.to_string(), error: error.to_string() });
        }
        let data: Value = reply.get("data").cloned().unwrap_or(Value::Null);
        if data.is_null() {
            return Ok(None);
        }
        serde_json::from_value(data)
            .map(Some)
            .map_err(|error: serde_json::Error| MpvError::UnexpectedValue { property: property.to_string(), error })
    }

    async fn command(&mut self, command: &[Value]) -> Result<(), MpvError> {
        let reply: Value = self.request(command).await?;
        match reply.get("error").and_then(Value::as_str) {
            Some("success") => Ok(()),
            other => Err(MpvError::Rejected {
                property: command.first().map(Value::to_string).unwrap_or_default(),
                error: other.unwrap_or("missing error field").to_string(),
            }),
        }
    }

    fn take_request_id(&mut self) -> u64 {
        let request_id: u64 = self.next_request_id;
        self.next_request_id += 1;
        request_id
    }

    async fn request(&mut self, command: &[Value]) -> Result<Value, MpvError> {
        let request_id: u64 = self.take_request_id();
        let mut line: String = json!({ "command": command, "request_id": request_id }).to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;

        let limit: Duration = self.timeout;
        let waited: Result<Result<Value, MpvError>, tokio::time::error::Elapsed> =
            tokio::time::timeout(limit, self.read_reply(request_id)).await;
        waited.map_err(|_elapsed: tokio::time::error::Elapsed| MpvError::Timeout(limit))?
    }

    /// Skips anything that is not the reply to `request_id`: events that were
    /// queued before `disable_event` took effect, or late replies.
    async fn read_reply(&mut self, request_id: u64) -> Result<Value, MpvError> {
        loop {
            let Some(line): Option<String> = self.lines.next_line().await? else {
                return Err(MpvError::Closed);
            };
            let Ok(message): Result<Value, serde_json::Error> = serde_json::from_str(&line) else {
                continue;
            };
            if message.get("request_id").and_then(Value::as_u64) == Some(request_id) {
                return Ok(message);
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::UnixListener;

    /// Serves `get_property` from a fixed map, the way mpv answers it, and sends
    /// an unsolicited event before every reply to prove that events are skipped.
    /// `observe_property` gets its reply plus the initial `property-change` event.
    /// `get_property` requests are counted, so tests can tell whether mpv was asked.
    pub async fn serve_fake_mpv(
        listener: UnixListener,
        properties: HashMap<String, Value>,
        property_requests: Arc<AtomicUsize>,
    ) {
        while let Ok((stream, _address)) = listener.accept().await {
            let properties: HashMap<String, Value> = properties.clone();
            let property_requests: Arc<AtomicUsize> = Arc::clone(&property_requests);
            tokio::spawn(async move {
                let (read_half, mut write_half): (OwnedReadHalf, OwnedWriteHalf) = stream.into_split();
                let mut lines: Lines<BufReader<OwnedReadHalf>> = BufReader::new(read_half).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let request: Value = serde_json::from_str(&line).expect("json");
                    let request_id: u64 = request["request_id"].as_u64().expect("id");
                    let command: &str = request["command"][0].as_str().unwrap_or_default();
                    if command == "observe_property" {
                        let observer_id: u64 = request["command"][1].as_u64().unwrap_or(0);
                        let name: &str = request["command"][2].as_str().unwrap_or_default();
                        let reply: Value = json!({"request_id": request_id, "error": "success"});
                        let mut event: Value = json!({"event": "property-change", "id": observer_id, "name": name});
                        if let Some(value) = properties.get(name) {
                            event["data"] = value.clone();
                        }
                        let output: String = format!("{reply}\n{event}\n");
                        if write_half.write_all(output.as_bytes()).await.is_err() {
                            return;
                        }
                        continue;
                    }
                    let reply: Value = match command {
                        "get_property" => {
                            property_requests.fetch_add(1, Ordering::SeqCst);
                            let name: &str = request["command"][1].as_str().unwrap_or_default();
                            match properties.get(name) {
                                Some(value) => json!({"request_id": request_id, "error": "success", "data": value}),
                                None => json!({"request_id": request_id, "error": "property unavailable"}),
                            }
                        }
                        _ => json!({"request_id": request_id, "error": "success"}),
                    };
                    let output: String = format!("{{\"event\":\"tick\"}}\n{reply}\n");
                    if write_half.write_all(output.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    }

    pub fn start_fake_mpv(directory: &Path, properties: HashMap<String, Value>) -> PathBuf {
        start_counting_fake_mpv(directory, properties).0
    }

    pub fn start_counting_fake_mpv(
        directory: &Path,
        properties: HashMap<String, Value>,
    ) -> (PathBuf, Arc<AtomicUsize>) {
        let socket: PathBuf = directory.join("mpv.sock");
        let listener: UnixListener = UnixListener::bind(&socket).expect("bind");
        let property_requests: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        tokio::spawn(serve_fake_mpv(listener, properties, Arc::clone(&property_requests)));
        (socket, property_requests)
    }

    #[tokio::test]
    async fn properties_are_read_and_events_skipped() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let properties: HashMap<String, Value> = HashMap::from([("volume".to_string(), json!(42.0))]);
        let socket: PathBuf = start_fake_mpv(directory.path(), properties);

        let mut connection: MpvConnection =
            MpvConnection::connect(&socket, Duration::from_secs(1)).await.expect("connect");
        let volume: Option<f64> = connection.get_property_optional("volume").await.expect("volume");
        assert_eq!(volume, Some(42.0));
        let missing: Option<f64> = connection.get_property_optional("time-pos").await.expect("time-pos");
        assert_eq!(missing, None);
    }

    #[tokio::test]
    async fn observing_yields_the_current_value_as_an_event() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let properties: HashMap<String, Value> = HashMap::from([("pause".to_string(), json!(true))]);
        let socket: PathBuf = start_fake_mpv(directory.path(), properties);

        let mut connection: MpvConnection =
            MpvConnection::connect_with_events(&socket, Duration::from_secs(1)).await.expect("connect");
        connection.observe_property(1, "pause").await.expect("observe");
        connection.observe_property(2, "time-pos").await.expect("observe");
        let pause: MpvEvent = connection.next_event().await.expect("pause event");
        assert_eq!(pause.property.as_deref(), Some("pause"));
        assert_eq!(pause.data, json!(true));
        let position: MpvEvent = connection.next_event().await.expect("time-pos event");
        assert_eq!(position.property.as_deref(), Some("time-pos"));
        assert_eq!(position.data, Value::Null);
    }
}
