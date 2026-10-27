//! The recorded state. The JSON layout is a contract with
//! remoteplayer-restservice, which reads the state file to restore playback;
//! it is documented in docs/STATE-FORMAT.md. Change `FORMAT_VERSION` on any
//! incompatible change.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mpv_connection::{MpvConnection, MpvError};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SavedPlayerState {
    pub format_version: u32,
    pub captured_at_unix_ms: u64,
    /// Grows by one with every change written to the state file; lets readers spot new data.
    pub sequence: u64,
    pub mpv: MpvProcessInfo,
    pub player: PlayerSnapshot,
}

/// The reply to `get` on the state socket: what mpv reports right now. It has
/// no `sequence` and does not apply the "keep the previous snapshot" rule —
/// that is what the state file is for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct LiveState {
    pub format_version: u32,
    pub captured_at_unix_ms: u64,
    pub mpv: MpvProcessInfo,
    pub player: PlayerSnapshot,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MpvProcessInfo {
    pub pid: Option<u32>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PlayerSnapshot {
    pub playlist: Vec<PlaylistItem>,
    pub playlist_pos: Option<usize>,
    pub time_pos: Option<f64>,
    pub paused: bool,
    pub volume: f64,
    pub muted: bool,
    pub speed: f64,
    pub audio_track: TrackSelection,
    pub subtitle_track: TrackSelection,
    pub secondary_subtitle_track: TrackSelection,
    pub audio_delay: f64,
    pub subtitle_delay: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PlaylistItem {
    pub filename: String,
    #[serde(default)]
    pub title: Option<String>,
}

/// A track id, or the keywords `no` (off) / `auto`. `untagged`: serialized as a
/// bare number or string, without a wrapping object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TrackSelection {
    Id(u32),
    Keyword(TrackKeyword),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackKeyword {
    No,
    Auto,
}

impl TrackSelection {
    /// mpv reports `aid`/`sid` as a number, or as `false`/"no" when disabled.
    pub fn from_mpv_value(value: &Value) -> TrackSelection {
        match value {
            Value::Number(number) => number
                .as_u64()
                .and_then(|id: u64| u32::try_from(id).ok())
                .map(TrackSelection::Id)
                .unwrap_or(TrackSelection::Keyword(TrackKeyword::Auto)),
            Value::Bool(false) => TrackSelection::Keyword(TrackKeyword::No),
            Value::String(text) if text == "no" => TrackSelection::Keyword(TrackKeyword::No),
            _ => TrackSelection::Keyword(TrackKeyword::Auto),
        }
    }
}

#[derive(Debug, Deserialize)]
struct MpvPlaylistEntry {
    filename: String,
    #[serde(default)]
    title: Option<String>,
}

/// What one poll of mpv returns: the playback state plus who produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct Capture {
    pub process: MpvProcessInfo,
    pub player: PlayerSnapshot,
}

impl PlayerSnapshot {
    pub fn is_empty(&self) -> bool {
        self.playlist.is_empty()
    }
}

pub async fn capture(connection: &mut MpvConnection) -> Result<Capture, MpvError> {
    let process: MpvProcessInfo = MpvProcessInfo {
        pid: connection.get_property_optional("pid").await?,
        version: connection.get_property_optional("mpv-version").await?,
    };
    let entries: Vec<MpvPlaylistEntry> = connection.get_property_optional("playlist").await?.unwrap_or_default();
    let playlist_pos: Option<i64> = connection.get_property_optional("playlist-pos").await?;
    let player: PlayerSnapshot = PlayerSnapshot {
        playlist: entries
            .into_iter()
            .map(|entry: MpvPlaylistEntry| PlaylistItem { filename: entry.filename, title: entry.title })
            .collect(),
        playlist_pos: playlist_pos.and_then(|position: i64| usize::try_from(position).ok()),
        time_pos: connection.get_property_optional("time-pos").await?,
        paused: connection.get_property_optional("pause").await?.unwrap_or(false),
        volume: connection.get_property_optional("volume").await?.unwrap_or(100.0),
        muted: connection.get_property_optional("mute").await?.unwrap_or(false),
        speed: connection.get_property_optional("speed").await?.unwrap_or(1.0),
        audio_track: read_audio_track(connection).await?,
        subtitle_track: read_track(connection, "sid").await?,
        secondary_subtitle_track: read_track(connection, "secondary-sid").await?,
        audio_delay: connection.get_property_optional("audio-delay").await?.unwrap_or(0.0),
        subtitle_delay: connection.get_property_optional("sub-delay").await?.unwrap_or(0.0),
    };
    Ok(Capture { process, player })
}

/// mpv deselects the audio track when it cannot open the audio output; that
/// "no" is not a user choice, so without an output it is recorded as `auto`.
async fn read_audio_track(connection: &mut MpvConnection) -> Result<TrackSelection, MpvError> {
    let selection: TrackSelection = read_track(connection, "aid").await?;
    let audio_output: Option<String> = connection.get_property_optional("current-ao").await?;
    match (&selection, audio_output) {
        (TrackSelection::Keyword(TrackKeyword::No), None) => Ok(TrackSelection::Keyword(TrackKeyword::Auto)),
        _ => Ok(selection),
    }
}

async fn read_track(connection: &mut MpvConnection, property: &str) -> Result<TrackSelection, MpvError> {
    let value: Option<Value> = connection.get_property_optional(property).await?;
    Ok(value
        .map(|raw: Value| TrackSelection::from_mpv_value(&raw))
        .unwrap_or(TrackSelection::Keyword(TrackKeyword::Auto)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpv_connection::tests::start_fake_mpv;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;

    fn playing_mpv() -> HashMap<String, Value> {
        HashMap::from([
            ("pid".to_string(), json!(4242)),
            ("mpv-version".to_string(), json!("mpv 0.40.0")),
            ("playlist".to_string(), json!([{"filename": "/media/a.mkv", "current": true, "id": 1}])),
            ("playlist-pos".to_string(), json!(0)),
            ("time-pos".to_string(), json!(12.5)),
            ("pause".to_string(), json!(false)),
            ("volume".to_string(), json!(80.0)),
            ("mute".to_string(), json!(false)),
            ("speed".to_string(), json!(1.0)),
            ("aid".to_string(), json!(false)),
            ("sid".to_string(), json!(2)),
            ("secondary-sid".to_string(), json!(false)),
            ("audio-delay".to_string(), json!(0.0)),
            ("sub-delay".to_string(), json!(0.5)),
        ])
    }

    #[tokio::test]
    async fn capture_reads_everything_needed_for_restore() {
        let directory: tempfile::TempDir = tempfile::tempdir().expect("temp dir");
        let socket: PathBuf = start_fake_mpv(directory.path(), playing_mpv());
        let mut connection: MpvConnection =
            MpvConnection::connect(&socket, Duration::from_secs(1)).await.expect("connect");

        let captured: Capture = capture(&mut connection).await.expect("capture");
        assert_eq!(captured.process.pid, Some(4242));
        assert_eq!(captured.player.playlist[0].filename, "/media/a.mkv");
        assert_eq!(captured.player.playlist_pos, Some(0));
        assert_eq!(captured.player.time_pos, Some(12.5));
        assert_eq!(captured.player.subtitle_track, TrackSelection::Id(2));
        // `current-ao` is missing in the fake: the deselected audio track becomes auto.
        assert_eq!(captured.player.audio_track, TrackSelection::Keyword(TrackKeyword::Auto));
    }

    #[test]
    fn json_layout_matches_the_documented_format() {
        let state: SavedPlayerState = SavedPlayerState {
            format_version: FORMAT_VERSION,
            captured_at_unix_ms: 1,
            sequence: 2,
            mpv: MpvProcessInfo { pid: Some(3), version: None },
            player: PlayerSnapshot {
                playlist: vec![PlaylistItem { filename: "/a.mkv".to_string(), title: None }],
                playlist_pos: Some(0),
                time_pos: None,
                paused: true,
                volume: 100.0,
                muted: false,
                speed: 1.0,
                audio_track: TrackSelection::Id(1),
                subtitle_track: TrackSelection::Keyword(TrackKeyword::No),
                secondary_subtitle_track: TrackSelection::Keyword(TrackKeyword::No),
                audio_delay: 0.0,
                subtitle_delay: 0.0,
            },
        };
        let value: Value = serde_json::to_value(&state).expect("encode");
        assert_eq!(value["format-version"], json!(1));
        assert_eq!(value["mpv"]["pid"], json!(3));
        assert_eq!(value["player"]["playlist-pos"], json!(0));
        assert_eq!(value["player"]["audio-track"], json!(1));
        assert_eq!(value["player"]["subtitle-track"], json!("no"));
    }
}
