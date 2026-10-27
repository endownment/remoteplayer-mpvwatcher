use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("invalid config file {path}: {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct WatcherConfig {
    pub mpv_socket: PathBuf,
    #[serde(default = "default_request_timeout_seconds")]
    pub request_timeout_seconds: u64,
    /// Keeps the restore snapshot across watcher restarts and reboots; used only
    /// if its directory is writable. Absent: the snapshot lives in memory only.
    pub state_file: Option<PathBuf>,
    /// While playing, how often a position-only change is written to the state
    /// file. 0: the position is written only together with other changes and
    /// when the watcher stops.
    #[serde(default = "default_position_save_seconds")]
    pub position_save_seconds: u64,
    /// Unix socket answering `get`, `last` and `status`. Absent: no socket.
    pub state_socket: Option<PathBuf>,
}

impl WatcherConfig {
    pub fn load(path: &Path) -> Result<WatcherConfig, ConfigError> {
        let text: String = std::fs::read_to_string(path)
            .map_err(|source: std::io::Error| ConfigError::Read { path: path.to_path_buf(), source })?;
        let config: WatcherConfig = toml::from_str(&text)
            .map_err(|source: toml::de::Error| ConfigError::Parse { path: path.to_path_buf(), source })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.state_file.is_none() && self.state_socket.is_none() {
            return Err(ConfigError::Invalid("configure at least one of state-file and state-socket".to_string()));
        }
        if self.request_timeout_seconds == 0 {
            return Err(ConfigError::Invalid("request-timeout-seconds must be greater than zero".to_string()));
        }
        Ok(())
    }
}

fn default_position_save_seconds() -> u64 {
    10
}

fn default_request_timeout_seconds() -> u64 {
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_save_defaults_to_ten_seconds() {
        let config: WatcherConfig = toml::from_str(
            r#"
            mpv-socket = "/run/remoteplayer-mpv/mpv.sock"
            state-file = "/tmp/state.json"
            "#,
        )
        .expect("parse");
        config.validate().expect("valid");
        assert_eq!(config.position_save_seconds, 10);
        assert!(config.state_socket.is_none());
    }

    #[test]
    fn socket_alone_is_a_valid_configuration() {
        let config: WatcherConfig = toml::from_str(
            r#"
            mpv-socket = "/run/remoteplayer-mpv/mpv.sock"
            state-socket = "/tmp/state.sock"
            "#,
        )
        .expect("parse");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn at_least_one_output_is_required() {
        let config: WatcherConfig = toml::from_str(r#"mpv-socket = "/run/remoteplayer-mpv/mpv.sock""#).expect("parse");
        assert!(config.validate().is_err());
    }

    #[test]
    fn keys_of_the_previous_layout_are_rejected() {
        let result: Result<WatcherConfig, toml::de::Error> = toml::from_str(
            r#"
            mpv-socket = "/run/remoteplayer-mpv/mpv.sock"
            state-file-interval-seconds = 7
            "#,
        );
        assert!(result.is_err());
    }
}
