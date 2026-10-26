mod config;
mod mpv_connection;
mod output;
mod recorder;
mod snapshot;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::time::{Interval, MissedTickBehavior};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::FromEnvError;

use crate::config::WatcherConfig;
use crate::mpv_connection::{MpvConnection, MpvError};
use crate::output::{SocketPublisher, read_state_file, write_state_file};
use crate::recorder::{Decision, Recorder};
use crate::snapshot::{Capture, SavedPlayerState, capture};

#[derive(Debug, Parser)]
#[command(version, about = "Records the mpv playback state every few seconds")]
struct CommandLine {
    /// Path to the TOML configuration file.
    #[arg(long, default_value = "/etc/remoteplayer/mpvwatcher.toml")]
    config: PathBuf,
}

// One thread is plenty for a poll every few seconds; it also keeps memory use
// minimal next to mpv on a 1 GB Raspberry Pi.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    init_logging();
    let command_line: CommandLine = CommandLine::parse();
    match run(command_line).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(command_line: CommandLine) -> Result<(), Box<dyn std::error::Error>> {
    let config: WatcherConfig = WatcherConfig::load(&command_line.config)?;
    let previous: Option<SavedPlayerState> = config.output.file.as_deref().and_then(read_state_file);
    let initial_line: Option<String> =
        previous.as_ref().and_then(|state: &SavedPlayerState| serde_json::to_string(state).ok());
    let mut watcher: Watcher = Watcher {
        recorder: Recorder::new(previous),
        socket: match &config.output.socket {
            Some(path) => Some(SocketPublisher::start(path, initial_line)?),
            None => None,
        },
        connection: None,
        was_connected: None,
        config,
    };

    let mut terminate: Signal = signal(SignalKind::terminate())?;
    let mut ticks: Interval = tokio::time::interval(Duration::from_secs(watcher.config.interval_seconds));
    // After a stall (e.g. the Pi was busy), continue at the normal pace instead
    // of firing the missed ticks in a burst.
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);

    notify_systemd(sd_notify::NotifyState::Ready);
    tracing::info!(interval_seconds = watcher.config.interval_seconds, "remoteplayer-mpvwatcher started");
    loop {
        tokio::select! {
            _ = ticks.tick() => watcher.tick().await,
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
        }
        notify_systemd(sd_notify::NotifyState::Watchdog);
    }
    tracing::info!("stopped");
    // Ownership: `watcher` is dropped here; SocketPublisher's Drop removes the socket file.
    Ok(())
}

struct Watcher {
    config: WatcherConfig,
    recorder: Recorder,
    socket: Option<SocketPublisher>,
    connection: Option<MpvConnection>,
    was_connected: Option<bool>,
}

impl Watcher {
    async fn tick(&mut self) {
        let captured: Result<Capture, MpvError> = self.capture_once().await;
        match captured {
            Ok(capture) => {
                self.report_connection(true, None);
                let decision: Decision = self.recorder.consider(capture, unix_ms_now());
                self.apply(decision).await;
            }
            Err(error) => {
                if error.breaks_connection() {
                    self.connection = None;
                }
                self.report_connection(false, Some(&error));
            }
        }
    }

    async fn capture_once(&mut self) -> Result<Capture, MpvError> {
        if self.connection.is_none() {
            let timeout: Duration = Duration::from_secs(self.config.request_timeout_seconds);
            self.connection = Some(MpvConnection::connect(&self.config.mpv_socket, timeout).await?);
        }
        // Borrowing: `as_mut()` turns `&mut Option<MpvConnection>` into
        // `Option<&mut MpvConnection>` — a mutable borrow of the value inside,
        // without moving it out of `self`.
        match self.connection.as_mut() {
            Some(connection) => capture(connection).await,
            None => Err(MpvError::Closed),
        }
    }

    async fn apply(&mut self, decision: Decision) {
        match decision {
            Decision::Publish(state) => {
                tracing::debug!(sequence = state.sequence, "publishing new state");
                self.publish(state).await;
            }
            Decision::HoldPrevious => {
                tracing::debug!("mpv restarted and is still empty; keeping the previous snapshot");
            }
            Decision::Unchanged => {}
        }
    }

    async fn publish(&mut self, state: SavedPlayerState) {
        if let Some(socket) = &self.socket {
            match serde_json::to_string(&state) {
                Ok(line) => socket.publish(line),
                Err(error) => tracing::warn!(%error, "cannot encode the state"),
            }
        }
        if let Some(path) = self.config.output.file.clone() {
            // fsync can take a while on an SD card; it must not stall the event
            // loop that also serves the socket clients.
            let written: Result<std::io::Result<()>, tokio::task::JoinError> =
                tokio::task::spawn_blocking(move || write_state_file(&path, &state)).await;
            match written {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "cannot write the state file"),
                Err(error) => tracing::warn!(%error, "state file writer failed"),
            }
        }
    }

    /// Logs only transitions, so an absent mpv does not flood the journal every few seconds.
    fn report_connection(&mut self, connected: bool, error: Option<&MpvError>) {
        if self.was_connected == Some(connected) {
            return;
        }
        self.was_connected = Some(connected);
        match error {
            None => tracing::info!(socket = %self.config.mpv_socket.display(), "connected to mpv"),
            Some(error) => tracing::warn!(%error, "mpv is not reachable; keeping the last snapshot"),
        }
    }
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed: Duration| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn init_logging() {
    let filter: EnvFilter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_unset: FromEnvError| EnvFilter::new("info"));
    let under_journald: bool = std::env::var_os("JOURNAL_STREAM").is_some();
    if under_journald {
        tracing_subscriber::fmt().with_env_filter(filter).without_time().with_ansi(false).init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

/// `Ready` lets `Type=notify` units proceed; `Watchdog` pings are only acted on
/// when the unit sets `WatchdogSec=`. Outside systemd both do nothing.
fn notify_systemd(state: sd_notify::NotifyState<'_>) {
    if let Err(error) = sd_notify::notify(false, &[state]) {
        tracing::debug!(%error, "cannot notify systemd");
    }
}
