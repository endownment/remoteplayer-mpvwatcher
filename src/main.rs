mod config;
mod mpv_connection;
mod output;
mod recorder;
mod sampler;
mod snapshot;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::time::{Interval, MissedTickBehavior};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::FromEnvError;

use crate::config::WatcherConfig;
use crate::mpv_connection::MpvError;
use crate::output::{SocketServer, read_state_file, write_state_file};
use crate::recorder::{Decision, Recorder};
use crate::sampler::{Sampler, TimedCapture};
use crate::snapshot::SavedPlayerState;

#[derive(Debug, Parser)]
#[command(version, about = "Records the mpv playback state and serves it on request")]
struct CommandLine {
    /// Path to the TOML configuration file.
    #[arg(long, default_value = "/etc/remoteplayer/mpvwatcher.toml")]
    config: PathBuf,
}

// One thread is plenty for this workload; it also keeps memory use minimal
// next to mpv on a 1 GB Raspberry Pi.
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
    let sampler: Arc<Sampler> =
        Arc::new(Sampler::new(config.mpv_socket.clone(), Duration::from_secs(config.request_timeout_seconds)));

    // Ownership: `_socket_server` lives until the end of `run`; its Drop then
    // removes the socket file. Named with a leading underscore because it is
    // never used directly — a bare `_` would drop it immediately.
    let _socket_server: Option<SocketServer> = match &config.state_socket {
        Some(path) => {
            tracing::info!(socket = %path.display(), "answering `get` requests");
            Some(SocketServer::start(path, Arc::clone(&sampler))?)
        }
        None => None,
    };

    let interval: Duration = Duration::from_secs(config.state_file_interval_seconds);
    let mut file_recorder: Option<FileRecorder> = config.state_file.clone().map(|path: PathBuf| {
        tracing::info!(file = %path.display(), interval_seconds = interval.as_secs(), "recording the state file");
        FileRecorder { recorder: Recorder::new(read_state_file(&path)), path, interval, sampler: Arc::clone(&sampler) }
    });
    let mut file_timer: Option<Interval> = file_recorder.as_ref().map(|_recorder: &FileRecorder| repeating(interval));
    let mut watchdog_timer: Option<Interval> = systemd_watchdog_interval().map(repeating);

    let mut terminate: Signal = signal(SignalKind::terminate())?;
    notify_systemd(sd_notify::NotifyState::Ready);
    loop {
        tokio::select! {
            () = next_tick(&mut file_timer) => {
                if let Some(recorder) = file_recorder.as_mut() {
                    recorder.update().await;
                }
            }
            () = next_tick(&mut watchdog_timer) => notify_systemd(sd_notify::NotifyState::Watchdog),
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
        }
    }
    tracing::info!("stopped");
    Ok(())
}

/// Keeps the state file current, applying the rule that a restarted, still
/// empty mpv must not overwrite the snapshot needed to restore playback.
struct FileRecorder {
    path: PathBuf,
    interval: Duration,
    recorder: Recorder,
    sampler: Arc<Sampler>,
}

impl FileRecorder {
    async fn update(&mut self) {
        // A capture made for a socket client within the last interval is as good
        // as a new one, so mpv is not asked twice.
        let captured: Result<TimedCapture, MpvError> = self.sampler.capture_reusing(self.interval).await;
        let Ok(timed): Result<TimedCapture, MpvError> = captured else {
            // The sampler has already logged that mpv is unreachable; the file keeps the last snapshot.
            return;
        };
        match self.recorder.consider(timed.capture, timed.captured_at_unix_ms) {
            Decision::Publish(state) => self.write(state).await,
            Decision::HoldPrevious => {
                tracing::debug!("mpv restarted and is still empty; keeping the previous snapshot");
            }
            Decision::Unchanged => {}
        }
    }

    async fn write(&self, state: SavedPlayerState) {
        tracing::debug!(sequence = state.sequence, "writing the state file");
        let path: PathBuf = self.path.clone();
        // fsync can take a while on an SD card; it must not stall the single
        // thread that also answers socket clients.
        let written: Result<std::io::Result<()>, tokio::task::JoinError> =
            tokio::task::spawn_blocking(move || write_state_file(&path, &state)).await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "cannot write the state file"),
            Err(error) => tracing::warn!(%error, "state file writer failed"),
        }
    }
}

fn repeating(period: Duration) -> Interval {
    let mut timer: Interval = tokio::time::interval(period);
    // After a stall (e.g. the Pi was busy), continue at the normal pace instead
    // of firing the missed ticks in a burst.
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    timer
}

/// Completes on the next tick, or never when there is no timer, so a disabled
/// timer simply never wins the `select!`.
async fn next_tick(timer: &mut Option<Interval>) {
    match timer {
        Some(interval) => {
            interval.tick().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// With `WatchdogSec=` systemd sets `WATCHDOG_USEC`; pinging at half that
/// period is the recommended margin. `None` outside systemd or without a watchdog.
fn systemd_watchdog_interval() -> Option<Duration> {
    let mut microseconds: u64 = 0;
    if sd_notify::watchdog_enabled(false, &mut microseconds) && microseconds > 0 {
        Some(Duration::from_micros(microseconds / 2))
    } else {
        None
    }
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

/// `Ready` lets `Type=notify` units proceed; `Watchdog` proves the event loop is
/// alive. Outside systemd both do nothing.
fn notify_systemd(state: sd_notify::NotifyState<'_>) {
    if let Err(error) = sd_notify::notify(false, &[state]) {
        tracing::debug!(%error, "cannot notify systemd");
    }
}
