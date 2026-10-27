//! What the tracker knows, shared with the socket clients that ask for it.

use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::Serialize;

use crate::output::StateFileMode;
use crate::snapshot::{FORMAT_VERSION, SavedPlayerState};

pub struct SharedState {
    // A std Mutex: every critical section is a quick copy in or out, never
    // held across an `.await`.
    inner: Mutex<StateData>,
}

struct StateData {
    last: Option<SavedPlayerState>,
    mpv_connected: bool,
    mpv_error: Option<String>,
    state_file: StateFileMode,
    last_written_sequence: Option<u64>,
    last_write_error: Option<String>,
}

/// The reply to `status`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct StatusReport {
    pub format_version: u32,
    pub mpv_connected: bool,
    pub mpv_error: Option<String>,
    pub last_known: bool,
    pub last_sequence: Option<u64>,
    pub state_file: StateFileReport,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct StateFileReport {
    #[serde(flatten)]
    pub mode: StateFileMode,
    pub last_written_sequence: Option<u64>,
    pub last_write_error: Option<String>,
}

impl SharedState {
    pub fn new(state_file: StateFileMode, last: Option<SavedPlayerState>) -> SharedState {
        SharedState {
            inner: Mutex::new(StateData {
                last,
                mpv_connected: false,
                mpv_error: None,
                state_file,
                last_written_sequence: None,
                last_write_error: None,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, StateData> {
        // Poisoning only means another thread panicked while holding the lock;
        // every field is still a complete value.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn last(&self) -> Option<SavedPlayerState> {
        self.lock().last.clone()
    }

    pub fn set_last(&self, state: SavedPlayerState) {
        self.lock().last = Some(state);
    }

    pub fn mpv_connected(&self) -> bool {
        self.lock().mpv_connected
    }

    /// `None`: connected; `Some(reason)`: not connected.
    pub fn set_mpv_connected(&self, error: Option<String>) {
        let mut data: MutexGuard<'_, StateData> = self.lock();
        data.mpv_connected = error.is_none();
        data.mpv_error = error;
    }

    pub fn record_write(&self, result: Result<u64, String>) {
        let mut data: MutexGuard<'_, StateData> = self.lock();
        match result {
            Ok(sequence) => {
                data.last_written_sequence = Some(sequence);
                data.last_write_error = None;
            }
            Err(error) => data.last_write_error = Some(error),
        }
    }

    pub fn status(&self) -> StatusReport {
        let data: MutexGuard<'_, StateData> = self.lock();
        StatusReport {
            format_version: FORMAT_VERSION,
            mpv_connected: data.mpv_connected,
            mpv_error: data.mpv_error.clone(),
            last_known: data.last.is_some(),
            last_sequence: data.last.as_ref().map(|state: &SavedPlayerState| state.sequence),
            state_file: StateFileReport {
                mode: data.state_file.clone(),
                last_written_sequence: data.last_written_sequence,
                last_write_error: data.last_write_error.clone(),
            },
        }
    }
}
