//! Follows mpv through property-change events instead of polling it, keeps
//! the snapshot needed for a restore in memory, and writes it to the state
//! file when that file is usable.
//!
//! When is the file written?
//! - at once when anything but the position changed (file, playlist, pause,
//!   volume, tracks...) and after a seek;
//! - while playing, the position at most every `position-save-seconds`;
//! - once more when the watcher is stopped (e.g. during a reboot).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

use crate::mpv_connection::{MpvConnection, MpvError, MpvEvent};
use crate::output::write_state_file;
use crate::recorder::{Decision, Recorder};
use crate::sampler::unix_ms_now;
use crate::shared::SharedState;
use crate::snapshot::{
    Capture, MpvProcessInfo, PlayerSnapshot, PropertyValues, SNAPSHOT_PROPERTIES, SavedPlayerState, read_process,
};

const RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// mpv reports several properties in a burst (on connect, when a file starts);
/// waiting this long lets the burst finish before a snapshot is taken.
const SETTLE_DELAY: Duration = Duration::from_millis(200);
/// `time-pos` changes many times per second; one snapshot per second is enough.
const POSITION_SETTLE_DELAY: Duration = Duration::from_secs(1);

pub struct TrackerConfig {
    pub mpv_socket: PathBuf,
    pub request_timeout: Duration,
    /// `None`: no file (not configured or not writable).
    pub state_file: Option<PathBuf>,
    /// `None`: position-only changes are written only together with other changes.
    pub position_save_interval: Option<Duration>,
}

pub struct Tracker {
    config: TrackerConfig,
    recorder: Recorder,
    shared: Arc<SharedState>,
    last_written: Option<SavedPlayerState>,
    last_written_at: Option<Instant>,
}

enum ConnectionEnd {
    Lost,
    Shutdown,
}

enum Wakeup {
    Event(Result<MpvEvent, MpvError>),
    SnapshotDue,
    Shutdown,
}

/// A snapshot is due at `at`; `forced_write` marks a seek, which must reach the
/// file even though only the position changed.
struct PendingSnapshot {
    at: Instant,
    forced_write: bool,
}

impl Tracker {
    pub fn new(config: TrackerConfig, recorder: Recorder, shared: Arc<SharedState>) -> Tracker {
        // What was loaded from the file is, by definition, what the file holds.
        let last_written: Option<SavedPlayerState> = recorder.latest().cloned();
        Tracker { config, recorder, shared, last_written, last_written_at: None }
    }

    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        loop {
            if let ConnectionEnd::Shutdown = self.follow_connection(&mut shutdown).await {
                break;
            }
            tokio::select! {
                () = tokio::time::sleep(RECONNECT_DELAY) => {}
                _ = shutdown.changed() => break,
            }
        }
        self.write_if_unsaved().await;
    }

    async fn follow_connection(&mut self, shutdown: &mut watch::Receiver<bool>) -> ConnectionEnd {
        let (mut connection, process): (MpvConnection, MpvProcessInfo) = match self.subscribe().await {
            Ok(subscribed) => subscribed,
            Err(error) => {
                self.report_disconnected(&error);
                return ConnectionEnd::Lost;
            }
        };
        if !self.shared.mpv_connected() {
            tracing::info!(socket = %self.config.mpv_socket.display(), pid = ?process.pid, "following mpv");
        }
        self.shared.set_mpv_connected(None);

        let mut values: PropertyValues = PropertyValues::new();
        let mut pending: Option<PendingSnapshot> = None;
        loop {
            let due: Option<Instant> = pending.as_ref().map(|snapshot: &PendingSnapshot| snapshot.at);
            let wakeup: Wakeup = tokio::select! {
                event = connection.next_event() => Wakeup::Event(event),
                () = sleep_until(due) => Wakeup::SnapshotDue,
                _ = shutdown.changed() => Wakeup::Shutdown,
            };
            match wakeup {
                Wakeup::Event(Ok(event)) => record_event(event, &mut values, &mut pending),
                Wakeup::Event(Err(error)) => {
                    self.report_disconnected(&error);
                    return ConnectionEnd::Lost;
                }
                Wakeup::SnapshotDue => {
                    let forced_write: bool =
                        pending.take().is_some_and(|snapshot: PendingSnapshot| snapshot.forced_write);
                    self.take_snapshot(&process, &values, forced_write).await;
                }
                Wakeup::Shutdown => {
                    if let Some(snapshot) = pending.take() {
                        self.take_snapshot(&process, &values, snapshot.forced_write).await;
                    }
                    return ConnectionEnd::Shutdown;
                }
            }
        }
    }

    async fn subscribe(&self) -> Result<(MpvConnection, MpvProcessInfo), MpvError> {
        let mut connection: MpvConnection =
            MpvConnection::connect_with_events(&self.config.mpv_socket, self.config.request_timeout).await?;
        let process: MpvProcessInfo = read_process(&mut connection).await?;
        for (index, property) in SNAPSHOT_PROPERTIES.iter().enumerate() {
            connection.observe_property(index as u64 + 1, property).await?;
        }
        Ok((connection, process))
    }

    /// Logs the change only, not each reconnect attempt every second.
    fn report_disconnected(&self, error: &MpvError) {
        let socket: String = self.config.mpv_socket.display().to_string();
        if self.shared.mpv_connected() {
            tracing::warn!(socket, %error, "lost mpv; keeping the last snapshot and waiting for it to come back");
        } else if self.shared.status().mpv_error.is_none() {
            tracing::info!(socket, %error, "mpv is not reachable yet, waiting for its socket");
        }
        self.shared.set_mpv_connected(Some(error.to_string()));
    }

    async fn take_snapshot(&mut self, process: &MpvProcessInfo, values: &PropertyValues, forced_write: bool) {
        let capture: Capture = Capture { process: process.clone(), player: PlayerSnapshot::from_properties(values) };
        match self.recorder.consider(capture, unix_ms_now()) {
            Decision::Publish(state) => {
                self.shared.set_last(state.clone());
                if forced_write || self.write_is_due(&state) {
                    self.write(state).await;
                }
            }
            Decision::HoldPrevious => {
                tracing::debug!("mpv restarted and is still empty; keeping the previous snapshot")
            }
            Decision::Unchanged => {}
        }
    }

    fn write_is_due(&self, state: &SavedPlayerState) -> bool {
        let Some(written): Option<&SavedPlayerState> = self.last_written.as_ref() else {
            return true;
        };
        let more_than_position: bool =
            written.mpv.pid != state.mpv.pid || !written.player.same_except_position(&state.player);
        let position_due: bool = match (self.config.position_save_interval, self.last_written_at) {
            (Some(_), None) => true,
            (Some(interval), Some(at)) => at.elapsed() >= interval,
            (None, _) => false,
        };
        more_than_position || position_due
    }

    async fn write_if_unsaved(&mut self) {
        let latest: Option<SavedPlayerState> = self.recorder.latest().cloned();
        if let Some(state) = latest {
            if self.last_written.as_ref() != Some(&state) {
                self.write(state).await;
            }
        }
    }

    async fn write(&mut self, state: SavedPlayerState) {
        let Some(path): Option<PathBuf> = self.config.state_file.clone() else {
            return;
        };
        let sequence: u64 = state.sequence;
        let to_write: SavedPlayerState = state.clone();
        // fsync can take a while on an SD card; it must not stall the single
        // thread that also answers socket clients.
        let written: Result<std::io::Result<()>, tokio::task::JoinError> =
            tokio::task::spawn_blocking(move || write_state_file(&path, &to_write)).await;
        match written {
            Ok(Ok(())) => {
                tracing::debug!(sequence, "state file written");
                self.last_written = Some(state);
                self.last_written_at = Some(Instant::now());
                self.shared.record_write(Ok(sequence));
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "cannot write the state file");
                self.shared.record_write(Err(error.to_string()));
            }
            Err(error) => {
                tracing::warn!(%error, "state file writer failed");
                self.shared.record_write(Err(error.to_string()));
            }
        }
    }
}

/// Stores the new value and schedules a snapshot. A burst of events results in
/// one snapshot: an already scheduled, earlier one is never pushed back.
fn record_event(event: MpvEvent, values: &mut PropertyValues, pending: &mut Option<PendingSnapshot>) {
    let (delay, forced_write): (Duration, bool) = match (event.name.as_str(), event.property) {
        ("property-change", Some(property)) => {
            let delay: Duration = if property == "time-pos" { POSITION_SETTLE_DELAY } else { SETTLE_DELAY };
            values.insert(property, event.data);
            (delay, false)
        }
        // Sent after a seek (and when a file starts): the new position matters
        // even if nothing else changed.
        ("playback-restart", _) => (SETTLE_DELAY, true),
        _ => return,
    };
    let at: Instant = Instant::now() + delay;
    match pending {
        Some(snapshot) => {
            snapshot.at = snapshot.at.min(at);
            snapshot.forced_write |= forced_write;
        }
        None => *pending = Some(PendingSnapshot { at, forced_write }),
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(instant) => tokio::time::sleep_until(instant).await,
        None => std::future::pending::<()>().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpv_connection::tests::start_fake_mpv;
    use crate::output::read_state_file;
    use serde_json::{Value, json};
    use std::collections::HashMap;

    fn playing_mpv() -> HashMap<String, Value> {
        HashMap::from([
            ("pid".to_string(), json!(4242)),
            ("mpv-version".to_string(), json!("mpv 0.40.0")),
            ("playlist".to_string(), json!([{"filename": "/media/a.mkv"}])),
            ("playlist-pos".to_string(), json!(0)),
            ("time-pos".to_string(), json!(12.5)),
            ("volume".to_string(), json!(80.0)),
        ])
    }

    fn event(name: &str, property: Option<&str>, data: Value) -> MpvEvent {
        MpvEvent { name: name.to_string(), property: property.map(str::to_string), data }
    }

    async fn wait_for<F: Fn() -> bool>(condition: F) {
        let deadline: Instant = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(Instant::now() < deadline, "condition not reached in time");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn events_become_the_last_state_and_reach_the_file() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let socket: PathBuf = start_fake_mpv(directory.path(), playing_mpv());
        let state_file: PathBuf = directory.path().join("state.json");
        let shared: Arc<SharedState> = Arc::new(SharedState::new(crate::output::StateFileMode::NotConfigured, None));
        let tracker: Tracker = Tracker::new(
            TrackerConfig {
                mpv_socket: socket,
                request_timeout: Duration::from_secs(1),
                state_file: Some(state_file.clone()),
                position_save_interval: Some(Duration::from_secs(10)),
            },
            Recorder::new(None),
            Arc::clone(&shared),
        );
        let (stop, stopped): (watch::Sender<bool>, watch::Receiver<bool>) = watch::channel(false);
        let task: tokio::task::JoinHandle<()> = tokio::spawn(tracker.run(stopped));

        wait_for(|| shared.last().is_some()).await;
        let last: SavedPlayerState = shared.last().expect("last state");
        assert_eq!(last.mpv.pid, Some(4242));
        assert_eq!(last.player.playlist[0].filename, "/media/a.mkv");
        assert_eq!(last.player.time_pos, Some(12.5));
        assert_eq!(last.player.volume, 80.0);
        assert!(shared.status().mpv_connected);

        stop.send_replace(true);
        task.await.expect("tracker stops");
        assert_eq!(read_state_file(&state_file), Some(last));
    }

    #[test]
    fn a_burst_of_events_schedules_one_snapshot_and_a_seek_forces_a_write() {
        let mut values: PropertyValues = PropertyValues::new();
        let mut pending: Option<PendingSnapshot> = None;
        record_event(event("property-change", Some("time-pos"), json!(1.0)), &mut values, &mut pending);
        let position_only: Instant = pending.as_ref().expect("scheduled").at;
        record_event(event("property-change", Some("pause"), json!(true)), &mut values, &mut pending);
        let after_pause: Instant = pending.as_ref().expect("scheduled").at;
        assert!(after_pause < position_only, "a discrete change brings the snapshot forward");
        assert!(!pending.as_ref().expect("scheduled").forced_write);

        record_event(event("playback-restart", None, Value::Null), &mut values, &mut pending);
        assert!(pending.as_ref().expect("scheduled").forced_write);
        record_event(event("end-file", None, Value::Null), &mut values, &mut pending);
        assert_eq!(values.len(), 2);
    }
}
