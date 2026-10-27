//! Answers `get`: reads everything from mpv on demand, over its own
//! connection, which is kept open between requests and re-established after
//! mpv restarts.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{Mutex, MutexGuard};

use crate::mpv_connection::{MpvConnection, MpvError};
use crate::snapshot::{Capture, capture};

#[derive(Debug, Clone)]
pub struct TimedCapture {
    pub capture: Capture,
    pub captured_at_unix_ms: u64,
}

pub struct Sampler {
    mpv_socket: PathBuf,
    request_timeout: Duration,
    // A tokio (not std) Mutex: it stays locked across the `.await`s of a whole
    // capture, so concurrent `get`s take turns on the one connection.
    connection: Mutex<Option<MpvConnection>>,
}

impl Sampler {
    pub fn new(mpv_socket: PathBuf, request_timeout: Duration) -> Sampler {
        Sampler { mpv_socket, request_timeout, connection: Mutex::new(None) }
    }

    pub async fn capture_live(&self) -> Result<TimedCapture, MpvError> {
        let mut guard: MutexGuard<'_, Option<MpvConnection>> = self.connection.lock().await;
        let result: Result<Capture, MpvError> = self.capture_with(&mut guard).await;
        match result {
            Ok(captured) => Ok(TimedCapture { capture: captured, captured_at_unix_ms: unix_ms_now() }),
            Err(error) => {
                if error.breaks_connection() {
                    *guard = None;
                }
                Err(error)
            }
        }
    }

    async fn capture_with(&self, slot: &mut Option<MpvConnection>) -> Result<Capture, MpvError> {
        if slot.is_none() {
            *slot = Some(MpvConnection::connect(&self.mpv_socket, self.request_timeout).await?);
        }
        // Borrowing: `as_mut()` turns `&mut Option<MpvConnection>` into
        // `Option<&mut MpvConnection>` — a mutable borrow of the value inside,
        // without moving it out of the slot.
        match slot.as_mut() {
            Some(connection) => capture(connection).await,
            None => Err(MpvError::Closed),
        }
    }
}

pub fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed: Duration| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::mpv_connection::tests::start_counting_fake_mpv;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub fn idle_mpv() -> HashMap<String, Value> {
        HashMap::from([
            ("pid".to_string(), json!(4242)),
            ("mpv-version".to_string(), json!("mpv 0.40.0")),
            ("volume".to_string(), json!(70.0)),
        ])
    }

    #[tokio::test]
    async fn every_capture_asks_mpv_over_one_reused_connection() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let (socket, requests): (PathBuf, Arc<AtomicUsize>) = start_counting_fake_mpv(directory.path(), idle_mpv());
        let sampler: Sampler = Sampler::new(socket, Duration::from_secs(1));

        let first: TimedCapture = sampler.capture_live().await.expect("first");
        let per_capture: usize = requests.load(Ordering::SeqCst);
        assert!(per_capture > 0);
        assert_eq!(first.capture.player.volume, 70.0);
        sampler.capture_live().await.expect("second");
        assert_eq!(requests.load(Ordering::SeqCst), 2 * per_capture);
    }

    #[tokio::test]
    async fn unreachable_mpv_is_an_error() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let sampler: Sampler = Sampler::new(directory.path().join("absent.sock"), Duration::from_secs(1));
        assert!(sampler.capture_live().await.is_err());
    }
}
