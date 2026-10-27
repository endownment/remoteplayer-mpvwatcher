//! Decides which captures are written to the state file.
//!
//! The important rule: when mpv was restarted (new pid) and came back empty,
//! the previous snapshot is kept. Otherwise a crash, a reboot or a restart by
//! systemd would immediately overwrite the very state that is needed to
//! resume playback. Once the new mpv plays something (restored or new), its
//! state is written normally — and an intentional stop in the same mpv
//! process is written too. The socket's live `get` does not go through here.

use crate::snapshot::{Capture, FORMAT_VERSION, SavedPlayerState};

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Publish(SavedPlayerState),
    Unchanged,
    HoldPrevious,
}

pub struct Recorder {
    latest: Option<SavedPlayerState>,
    sequence: u64,
}

impl Recorder {
    /// `previous` is the state file left by an earlier run, if any.
    pub fn new(previous: Option<SavedPlayerState>) -> Recorder {
        let sequence: u64 = previous.as_ref().map(|state: &SavedPlayerState| state.sequence).unwrap_or(0);
        Recorder { latest: previous, sequence }
    }

    #[cfg(test)]
    pub fn latest(&self) -> Option<&SavedPlayerState> {
        self.latest.as_ref()
    }

    // Ownership: `capture` is taken by value — its playlist (a Vec of Strings)
    // moves into the new state without being copied.
    pub fn consider(&mut self, capture: Capture, now_unix_ms: u64) -> Decision {
        if let Some(previous) = &self.latest {
            let same_process: bool = previous.mpv.pid == capture.process.pid;
            if !same_process && capture.player.is_empty() && !previous.player.is_empty() {
                return Decision::HoldPrevious;
            }
            if same_process && previous.player == capture.player {
                return Decision::Unchanged;
            }
        }
        self.sequence += 1;
        let state: SavedPlayerState = SavedPlayerState {
            format_version: FORMAT_VERSION,
            captured_at_unix_ms: now_unix_ms,
            sequence: self.sequence,
            mpv: capture.process,
            player: capture.player,
        };
        self.latest = Some(state.clone());
        Decision::Publish(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{MpvProcessInfo, PlayerSnapshot, PlaylistItem, TrackKeyword, TrackSelection};

    fn capture(pid: u32, files: &[&str], time_pos: Option<f64>) -> Capture {
        Capture {
            process: MpvProcessInfo { pid: Some(pid), version: Some("mpv 0.40.0".to_string()) },
            player: PlayerSnapshot {
                playlist: files
                    .iter()
                    .map(|file: &&str| PlaylistItem { filename: file.to_string(), title: None })
                    .collect(),
                playlist_pos: if files.is_empty() { None } else { Some(0) },
                time_pos,
                paused: false,
                volume: 100.0,
                muted: false,
                speed: 1.0,
                audio_track: TrackSelection::Keyword(TrackKeyword::Auto),
                subtitle_track: TrackSelection::Keyword(TrackKeyword::No),
                secondary_subtitle_track: TrackSelection::Keyword(TrackKeyword::No),
                audio_delay: 0.0,
                subtitle_delay: 0.0,
            },
        }
    }

    #[test]
    fn changes_are_published_with_growing_sequence_and_repeats_are_skipped() {
        let mut recorder: Recorder = Recorder::new(None);
        let first: Decision = recorder.consider(capture(1, &["/a.mkv"], Some(1.0)), 10);
        assert!(matches!(first, Decision::Publish(ref state) if state.sequence == 1));
        assert_eq!(recorder.consider(capture(1, &["/a.mkv"], Some(1.0)), 20), Decision::Unchanged);
        let moved: Decision = recorder.consider(capture(1, &["/a.mkv"], Some(8.0)), 30);
        assert!(
            matches!(moved, Decision::Publish(ref state) if state.sequence == 2 && state.captured_at_unix_ms == 30)
        );
    }

    #[test]
    fn empty_state_of_a_restarted_mpv_does_not_overwrite_the_snapshot() {
        let mut recorder: Recorder = Recorder::new(None);
        recorder.consider(capture(1, &["/a.mkv"], Some(100.0)), 10);
        assert_eq!(recorder.consider(capture(2, &[], None), 20), Decision::HoldPrevious);
        assert_eq!(recorder.latest().expect("kept").mpv.pid, Some(1));

        let restored: Decision = recorder.consider(capture(2, &["/a.mkv"], Some(100.0)), 30);
        assert!(matches!(restored, Decision::Publish(ref state) if state.mpv.pid == Some(2)));
    }

    #[test]
    fn stopping_playback_in_the_same_process_is_published() {
        let mut recorder: Recorder = Recorder::new(None);
        recorder.consider(capture(1, &["/a.mkv"], Some(100.0)), 10);
        let stopped: Decision = recorder.consider(capture(1, &[], None), 20);
        assert!(matches!(stopped, Decision::Publish(ref state) if state.player.is_empty()));
    }

    #[test]
    fn snapshot_from_a_previous_run_survives_a_fresh_mpv() {
        let mut first_run: Recorder = Recorder::new(None);
        let Decision::Publish(saved): Decision = first_run.consider(capture(1, &["/a.mkv"], Some(5.0)), 10) else {
            panic!("expected publish");
        };
        let mut second_run: Recorder = Recorder::new(Some(saved));
        assert_eq!(second_run.consider(capture(7, &[], None), 20), Decision::HoldPrevious);
    }
}
