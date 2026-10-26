use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::snapshot::{FORMAT_VERSION, SavedPlayerState};

/// Write to a temporary sibling, fsync, rename: a reader (or a power cut)
/// sees either the old or the new file, never half of one.
pub fn write_state_file(path: &Path, state: &SavedPlayerState) -> std::io::Result<()> {
    let json: String = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary: PathBuf = temporary_sibling(path);
    let mut file: std::fs::File = std::fs::File::create(&temporary)?;
    file.write_all(json.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)
}

/// `None` when there is no usable file; a broken or foreign file is reported
/// and then ignored, the next capture replaces it.
pub fn read_state_file(path: &Path) -> Option<SavedPlayerState> {
    let text: String = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(file = %path.display(), %error, "cannot read the previous state file");
            return None;
        }
    };
    match serde_json::from_str::<SavedPlayerState>(&text) {
        Ok(state) if state.format_version == FORMAT_VERSION => Some(state),
        Ok(state) => {
            tracing::warn!(file = %path.display(), version = state.format_version, "ignoring state file of another format");
            None
        }
        Err(error) => {
            tracing::warn!(file = %path.display(), %error, "ignoring unreadable state file");
            None
        }
    }
}

fn temporary_sibling(path: &Path) -> PathBuf {
    let mut name: OsString = OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{MpvProcessInfo, PlayerSnapshot, TrackKeyword, TrackSelection};

    #[test]
    fn written_state_reads_back_identically() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let path: PathBuf = directory.path().join("state").join("state.json");
        let state: SavedPlayerState = SavedPlayerState {
            format_version: FORMAT_VERSION,
            captured_at_unix_ms: 5,
            sequence: 9,
            mpv: MpvProcessInfo { pid: Some(1), version: Some("mpv 0.40.0".to_string()) },
            player: PlayerSnapshot {
                playlist: Vec::new(),
                playlist_pos: None,
                time_pos: None,
                paused: false,
                volume: 100.0,
                muted: false,
                speed: 1.0,
                audio_track: TrackSelection::Keyword(TrackKeyword::Auto),
                subtitle_track: TrackSelection::Keyword(TrackKeyword::Auto),
                secondary_subtitle_track: TrackSelection::Keyword(TrackKeyword::No),
                audio_delay: 0.0,
                subtitle_delay: 0.0,
            },
        };
        write_state_file(&path, &state).expect("write");
        assert_eq!(read_state_file(&path), Some(state));
    }

    #[test]
    fn garbage_is_ignored() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let path: PathBuf = directory.path().join("state.json");
        std::fs::write(&path, "not json").expect("seed");
        assert_eq!(read_state_file(&path), None);
    }
}
