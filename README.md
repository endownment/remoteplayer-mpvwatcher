# remoteplayer-mpvwatcher

Records the playback state of the **remoteplayer** mpv instance every few
seconds (7 by default), so playback can resume after mpv crashed, was
restarted by systemd, or the Raspberry Pi rebooted.

The state goes to a JSON file and/or a Unix socket of its own:

- **file** — the latest snapshot, replaced atomically and only when it
  changes. [remoteplayer-restservice](https://github.com/endownment/remoteplayer-restservice)
  reads it (`restore.watcher-state-file`) to restore playback automatically
  after a crash, or on `POST /api/v1/player/restore`.
- **socket** — newline-delimited JSON: the latest snapshot on connect, then
  every change. Anything can subscribe, e.g. `socat - UNIX-CONNECT:/run/remoteplayer-mpvwatcher/state.sock`.

What a snapshot contains — playlist, position in the playlist and in the file,
pause, volume, mute, speed, audio/subtitle tracks, delays, and which mpv
process it came from — and the exact JSON layout are described in
[docs/STATE-FORMAT.md](docs/STATE-FORMAT.md).

## The one rule that matters

When mpv was restarted (its pid changed) and the new process is still empty,
the previous snapshot is **kept**, not overwritten with "nothing is playing".
Otherwise the restart that makes a restore necessary would destroy the data
for it. As soon as the new mpv plays something — restored or started by a
user — its state is recorded as usual. Stopping playback inside the same mpv
process is recorded too, so an intentional stop is not undone later.

The rule also applies across reboots: the watcher loads its previous file on
start-up.

## Quick start on a development machine

You need Rust (see [BUILD.md](BUILD.md)) and mpv. No Raspberry Pi required:
the watcher only talks to mpv's IPC socket, and a desktop mpv in a window
provides exactly the same socket.

```sh
mkdir -p dev-data
mpv --idle=yes --force-window=yes --input-ipc-server=dev-data/mpv.sock &
cargo run -- --config config/dev.toml
```

Play something in that mpv window (drag a file onto it), then look at
`dev-data/state.json` or connect to the socket:

```sh
socat - UNIX-CONNECT:dev-data/state.sock
```

To watch the mpv of a locally running remoteplayer-restservice instead, point
`mpv-socket` at its socket (`../remoteplayer-restservice/dev-data/run/mpv.sock`
with its dev config) and set the restservice's `restore.watcher-state-file` to
this watcher's file.

## Installation on the Raspberry Pi

Target: Raspberry Pi OS (Debian 13 "trixie"), user `coop`, mpv running as
`remoteplayer-mpv.service` (from the remoteplayer-restservice repository).
Build the binary first, see [BUILD.md](BUILD.md).

```sh
sudo install -m 0755 target/release/remoteplayer-mpvwatcher /usr/local/bin/
sudo install -d /etc/remoteplayer
sudo install -m 0644 config/mpvwatcher.toml /etc/remoteplayer/mpvwatcher.toml
sudo install -m 0644 deploy/systemd/remoteplayer-mpvwatcher.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now remoteplayer-mpvwatcher.service
```

The unit runs as the same user as mpv (the IPC socket is private to that
user), keeps the file in `/var/lib/remoteplayer-mpvwatcher/` and the socket in
`/run/remoteplayer-mpvwatcher/`, uses systemd's watchdog (the watcher pings
after every poll), and is sandboxed (read-only system, no devices, Unix
sockets only).

Check it:

```sh
systemctl status remoteplayer-mpvwatcher
cat /var/lib/remoteplayer-mpvwatcher/state.json
journalctl -u remoteplayer-mpvwatcher -f
```

## Configuration

[`config/mpvwatcher.toml`](config/mpvwatcher.toml):

| Key | Default | Meaning |
|---|---|---|
| `mpv-socket` | — | mpv's `--input-ipc-server` path |
| `interval-seconds` | `7` | time between polls |
| `request-timeout-seconds` | `2` | per-reply timeout; on expiry the connection is re-established |
| `output.file` | — | JSON state file (optional) |
| `output.socket` | — | the watcher's own socket (optional) |

At least one of `output.file` and `output.socket` is required.

**SD card wear.** While a video plays, the position changes on every poll,
so the file is rewritten every `interval-seconds` (about 500 small writes per
hour of playback). Paused or idle, nothing is written. If that matters, raise
the interval or use only the socket output.

## Repository layout

```
src/                  main.rs (loop) · mpv_connection.rs · snapshot.rs · recorder.rs · output/{file,socket}.rs
config/               mpvwatcher.toml for the Pi, dev.toml for a desktop
deploy/systemd/       the unit file
docs/STATE-FORMAT.md  the JSON contract with remoteplayer-restservice
dev-notes/            how it works, as Rust learning material
```
