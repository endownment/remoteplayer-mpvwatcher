mod config;
mod mpv_connection;
mod output;
mod recorder;
mod sampler;
mod shared;
mod snapshot;
mod tracker;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Interval, MissedTickBehavior};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::FromEnvError;

use crate::config::WatcherConfig;
use crate::output::{SocketServer, StateFileMode, probe_state_file, read_state_file};
use crate::recorder::Recorder;
use crate::sampler::Sampler;
use crate::shared::SharedState;
use crate::snapshot::SavedPlayerState;
use crate::tracker::{Tracker, TrackerConfig};

/// How long the tracker may take to write its final snapshot on shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Parser)]
#[command(version, about = "Follows the mpv playback state and serves it on request")]
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
    let request_timeout: Duration = Duration::from_secs(config.request_timeout_seconds);

    let state_file_mode: StateFileMode = probe_state_file(config.state_file.as_deref());
    let (writable_file, previous): (Option<PathBuf>, Option<SavedPlayerState>) = match &state_file_mode {
        StateFileMode::ReadWrite { path, volatile } => {
            if *volatile {
                tracing::warn!(file = %path.display(), "the state file is on a RAM-backed filesystem; it will not survive a power-off");
            }
            tracing::info!(file = %path.display(), "state file is writable");
            (Some(path.clone()), read_state_file(path))
        }
        // An old file in a place we cannot write to may be arbitrarily stale (for
        // example baked into a read-only image), so it is not used at all.
        StateFileMode::ReadOnly { path, reason } => {
            tracing::warn!(file = %path.display(), reason, "state file is not writable; the snapshot stays in memory only");
            (None, None)
        }
        StateFileMode::NotConfigured => {
            tracing::info!("no state file configured; the snapshot stays in memory only");
            (None, None)
        }
    };

    let shared: Arc<SharedState> = Arc::new(SharedState::new(state_file_mode, previous.clone()));
    let sampler: Arc<Sampler> = Arc::new(Sampler::new(config.mpv_socket.clone(), request_timeout));

    // Ownership: `_socket_server` lives until the end of `run`; its Drop then
    // removes the socket file. Named with a leading underscore because it is
    // never used directly — a bare `_` would drop it immediately.
    let _socket_server: Option<SocketServer> = match &config.state_socket {
        Some(path) => {
            tracing::info!(socket = %path.display(), "answering get, last and status requests");
            Some(SocketServer::start(path, Arc::clone(&sampler), Arc::clone(&shared))?)
        }
        None => None,
    };

    let tracker: Tracker = Tracker::new(
        TrackerConfig {
            mpv_socket: config.mpv_socket.clone(),
            request_timeout,
            state_file: writable_file,
            position_save_interval: (config.position_save_seconds > 0)
                .then(|| Duration::from_secs(config.position_save_seconds)),
        },
        Recorder::new(previous),
        Arc::clone(&shared),
    );
    let (stop_tracker, tracker_stopped): (watch::Sender<bool>, watch::Receiver<bool>) = watch::channel(false);
    let tracker_task: JoinHandle<()> = tokio::spawn(tracker.run(tracker_stopped));

    let mut watchdog_timer: Option<Interval> = systemd_watchdog_interval().map(repeating);
    let mut terminate: Signal = signal(SignalKind::terminate())?;
    notify_systemd(sd_notify::NotifyState::Ready);
    loop {
        tokio::select! {
            () = next_tick(&mut watchdog_timer) => notify_systemd(sd_notify::NotifyState::Watchdog),
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
        }
    }

    // The tracker writes the final snapshot (if the file is usable) before it ends.
    stop_tracker.send_replace(true);
    let finished: Result<Result<(), tokio::task::JoinError>, tokio::time::error::Elapsed> =
        tokio::time::timeout(SHUTDOWN_GRACE, tracker_task).await;
    if finished.is_err() {
        tracing::warn!("the tracker did not finish in time; the last snapshot may not be saved");
    }
    tracing::info!("stopped");
    Ok(())
}

fn repeating(period: Duration) -> Interval {
    let mut timer: Interval = tokio::time::interval(period);
    // After a stall, continue at the normal pace instead of a burst of ticks.
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
