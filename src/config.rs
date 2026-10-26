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
    #[serde(default = "default_interval_seconds")]
    pub interval_seconds: u64,
    #[serde(default = "default_request_timeout_seconds")]
    pub request_timeout_seconds: u64,
    pub output: OutputConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct OutputConfig {
    /// JSON file with the latest snapshot, replaced atomically.
    pub file: Option<PathBuf>,
    /// Unix socket that streams every new snapshot as one line of JSON.
    pub socket: Option<PathBuf>,
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
        if self.output.file.is_none() && self.output.socket.is_none() {
            return Err(ConfigError::Invalid("configure at least one of output.file and output.socket".to_string()));
        }
        if self.interval_seconds == 0 || self.request_timeout_seconds == 0 {
            return Err(ConfigError::Invalid("interval and timeout must be greater than zero".to_string()));
        }
        Ok(())
    }
}

fn default_interval_seconds() -> u64 {
    7
}

fn default_request_timeout_seconds() -> u64 {
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_defaults_to_seven_seconds() {
        let config: WatcherConfig = toml::from_str(
            r#"
            mpv-socket = "/run/remoteplayer-mpv/mpv.sock"
            [output]
            file = "/tmp/state.json"
            "#,
        )
        .expect("parse");
        config.validate().expect("valid");
        assert_eq!(config.interval_seconds, 7);
    }

    #[test]
    fn at_least_one_output_is_required() {
        let config: WatcherConfig = toml::from_str(
            r#"
            mpv-socket = "/run/remoteplayer-mpv/mpv.sock"
            [output]
            "#,
        )
        .expect("parse");
        assert!(config.validate().is_err());
    }
}
