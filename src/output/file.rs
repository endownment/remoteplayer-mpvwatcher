use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use nix::sys::statfs::{FsType, OVERLAYFS_SUPER_MAGIC, Statfs, TMPFS_MAGIC};
use serde::Serialize;

use crate::snapshot::{FORMAT_VERSION, SavedPlayerState};

/// Whether the state file can be used, decided once at start-up.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum StateFileMode {
    /// `state-file` is commented out.
    NotConfigured,
    /// Configured, but the directory cannot be written (read-only mount, no
    /// permission, missing partition). A file found there is not trusted.
    ReadOnly { path: PathBuf, reason: String },
    /// `volatile`: the directory is on tmpfs or an overlay (e.g. the Raspberry
    /// Pi OS overlay file system), so the file is lost on power-off.
    ReadWrite { path: PathBuf, volatile: bool },
}

/// Tries to create and delete a file next to the state file: the only reliable
/// test, because permissions, read-only mounts and ACLs all play a part.
pub fn probe_state_file(path: Option<&Path>) -> StateFileMode {
    let Some(path): Option<&Path> = path else {
        return StateFileMode::NotConfigured;
    };
    let directory: &Path =
        path.parent().filter(|parent: &&Path| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
    match check_writable(directory) {
        Ok(()) => StateFileMode::ReadWrite { path: path.to_path_buf(), volatile: is_volatile(directory) },
        Err(error) => StateFileMode::ReadOnly {
            path: path.to_path_buf(),
            reason: format!("{}: {}", directory.display(), describe_io_error(&error)),
        },
    }
}

fn check_writable(directory: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    let probe: PathBuf = directory.join(format!(".remoteplayer-write-probe-{}", std::process::id()));
    std::fs::write(&probe, b"probe")?;
    std::fs::remove_file(&probe)
}

fn is_volatile(directory: &Path) -> bool {
    let Ok(statistics): nix::Result<Statfs> = nix::sys::statfs::statfs(directory) else {
        return false;
    };
    let kind: FsType = statistics.filesystem_type();
    kind == TMPFS_MAGIC || kind == OVERLAYFS_SUPER_MAGIC
}

pub fn describe_io_error(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::ReadOnlyFilesystem => "the filesystem is mounted read-only".to_string(),
        std::io::ErrorKind::PermissionDenied => "no permission to write".to_string(),
        _ => error.to_string(),
    }
}

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
    fn writable_directory_is_read_write_and_unusable_one_is_read_only() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let path: PathBuf = directory.path().join("state").join("state.json");
        assert!(matches!(probe_state_file(Some(&path)), StateFileMode::ReadWrite { .. }));
        assert_eq!(std::fs::read_dir(directory.path().join("state")).expect("list").count(), 0);

        // A regular file where the directory should be: the closest thing to an
        // unwritable location that also works when the tests run as root.
        let blocker: PathBuf = directory.path().join("blocker");
        std::fs::write(&blocker, b"").expect("seed");
        let mode: StateFileMode = probe_state_file(Some(&blocker.join("state.json")));
        assert!(matches!(mode, StateFileMode::ReadOnly { .. }), "{mode:?}");
        assert_eq!(probe_state_file(None), StateFileMode::NotConfigured);
    }

    #[test]
    fn garbage_is_ignored() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let path: PathBuf = directory.path().join("state.json");
        std::fs::write(&path, "not json").expect("seed");
        assert_eq!(read_state_file(&path), None);
    }
}
