//! The one place that talks to mpv. Both the state-file timer and the socket
//! clients ask the sampler for a capture; it owns the single IPC connection and
//! remembers the latest capture, so a file update right after a `get` does not
//! query mpv a second time.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{Mutex, MutexGuard};

use crate::mpv_connection::{MpvConnection, MpvError};
use crate::snapshot::{Capture, capture};

#[derive(Debug, Clone)]
pub struct TimedCapture {
    pub capture: Capture,
    pub captured_at_unix_ms: u64,
    taken_at: Instant,
}

pub struct Sampler {
    mpv_socket: PathBuf,
    request_timeout: Duration,
    // A tokio (not std) Mutex: it stays locked across the `.await`s of a whole
    // capture, which also guarantees that only one capture uses the
    // connection at a time.
    state: Mutex<SamplerState>,
}

struct SamplerState {
    connection: Option<MpvConnection>,
    latest: Option<TimedCapture>,
    was_connected: Option<bool>,
}

impl Sampler {
    pub fn new(mpv_socket: PathBuf, request_timeout: Duration) -> Sampler {
        Sampler {
            mpv_socket,
            request_timeout,
            state: Mutex::new(SamplerState { connection: None, latest: None, was_connected: None }),
        }
    }

    /// Always asks mpv.
    pub async fn capture_live(&self) -> Result<TimedCapture, MpvError> {
        let mut state: MutexGuard<'_, SamplerState> = self.state.lock().await;
        self.poll(&mut state).await
    }

    /// Reuses the latest capture when it is younger than `max_age`, otherwise asks mpv.
    pub async fn capture_reusing(&self, max_age: Duration) -> Result<TimedCapture, MpvError> {
        let mut state: MutexGuard<'_, SamplerState> = self.state.lock().await;
        match &state.latest {
            Some(latest) if latest.taken_at.elapsed() < max_age => Ok(latest.clone()),
            _ => self.poll(&mut state).await,
        }
    }

    async fn poll(&self, state: &mut SamplerState) -> Result<TimedCapture, MpvError> {
        let result: Result<Capture, MpvError> = self.capture_with_connection(state).await;
        match result {
            Ok(captured) => {
                self.report_connection(state, None);
                let timed: TimedCapture =
                    TimedCapture { capture: captured, captured_at_unix_ms: unix_ms_now(), taken_at: Instant::now() };
                state.latest = Some(timed.clone());
                Ok(timed)
            }
            Err(error) => {
                if error.breaks_connection() {
                    state.connection = None;
                }
                self.report_connection(state, Some(&error));
                Err(error)
            }
        }
    }

    async fn capture_with_connection(&self, state: &mut SamplerState) -> Result<Capture, MpvError> {
        if state.connection.is_none() {
            state.connection = Some(MpvConnection::connect(&self.mpv_socket, self.request_timeout).await?);
        }
        // Borrowing: `as_mut()` turns `&mut Option<MpvConnection>` into
        // `Option<&mut MpvConnection>` — a mutable borrow of the value inside,
        // without moving it out of the state.
        match state.connection.as_mut() {
            Some(connection) => capture(connection).await,
            None => Err(MpvError::Closed),
        }
    }

    /// Logs only transitions, so an absent mpv does not flood the journal.
    fn report_connection(&self, state: &mut SamplerState, error: Option<&MpvError>) {
        let connected: bool = error.is_none();
        if state.was_connected == Some(connected) {
            return;
        }
        state.was_connected = Some(connected);
        match error {
            None => tracing::info!(socket = %self.mpv_socket.display(), "connected to mpv"),
            Some(error) => tracing::warn!(%error, "mpv is not reachable"),
        }
    }
}

fn unix_ms_now() -> u64 {
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
    async fn live_capture_always_asks_mpv_and_reuse_respects_max_age() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let (socket, requests): (PathBuf, Arc<AtomicUsize>) = start_counting_fake_mpv(directory.path(), idle_mpv());
        let sampler: Sampler = Sampler::new(socket, Duration::from_secs(1));

        let first: TimedCapture = sampler.capture_live().await.expect("first");
        let per_capture: usize = requests.load(Ordering::SeqCst);
        assert!(per_capture > 0);
        assert_eq!(first.capture.player.volume, 70.0);

        sampler.capture_live().await.expect("second");
        assert_eq!(requests.load(Ordering::SeqCst), 2 * per_capture);

        let reused: TimedCapture = sampler.capture_reusing(Duration::from_secs(60)).await.expect("reused");
        assert_eq!(requests.load(Ordering::SeqCst), 2 * per_capture);
        assert_eq!(reused.capture, first.capture);

        sampler.capture_reusing(Duration::ZERO).await.expect("fresh");
        assert_eq!(requests.load(Ordering::SeqCst), 3 * per_capture);
    }

    #[tokio::test]
    async fn unreachable_mpv_is_an_error_not_a_stale_capture() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let sampler: Sampler = Sampler::new(directory.path().join("absent.sock"), Duration::from_secs(1));
        assert!(sampler.capture_live().await.is_err());
    }
}
