# remoteplayer-mpvwatcher

Keeps track of the playback state of the **remoteplayer** mpv instance, so
playback can resume after mpv crashed, was restarted by systemd, or the
Raspberry Pi rebooted. It offers the state in two independent ways, each
enabled by one line of the configuration:

- **state file** — the latest snapshot as JSON, refreshed every
  `state-file-interval-seconds` (7 by default), replaced atomically and only
  when something changed.
  [remoteplayer-restservice](https://github.com/endownment/remoteplayer-restservice)
  reads it (`restore.watcher-state-file`) to restore playback automatically
  after a crash, or on `POST /api/v1/player/restore`. Handy for debugging
  too, and a web server such as nginx can serve it as is.
- **state socket** — request/response: send the line `get`, receive mpv's
  state *right now* as one line of JSON. Nothing is sent unasked, so the
  client decides how often mpv is queried.

What a snapshot contains — playlist, position in the playlist and in the file,
pause, volume, mute, speed, audio/subtitle tracks, delays, and which mpv
process it came from — and the exact JSON layout are in
[docs/STATE-FORMAT.md](docs/STATE-FORMAT.md).

## The state file keeps what is needed to resume

When mpv was restarted (its pid changed) and the new process is still empty,
the file **keeps** the previous snapshot instead of recording "nothing is
playing". Otherwise the restart that makes a restore necessary would destroy
the data for it. As soon as the new mpv plays something — restored or started
by a user — its state is recorded as usual. Stopping playback inside the same
mpv process is recorded too, so an intentional stop is not undone later. The
rule also holds across reboots: the watcher loads its previous file on start.

The socket does not apply this rule: `get` always answers with what mpv
reports at that moment. This is why crash recovery in remoteplayer-restservice
relies on the file. **Without `state-file` there is no automatic restore after
a crash.**

## Polling

The watcher talks to mpv only when it needs data:

- a `get` on the socket always queries mpv;
- a state-file update queries mpv unless a `get` already did within the last
  `state-file-interval-seconds` — then that result is reused;
- with `state-file` commented out there is no timer and no polling at all,
  only `get` requests.

## Quick start on a development machine

You need Rust (see [BUILD.md](BUILD.md)) and mpv. No Raspberry Pi required:
the watcher only talks to mpv's IPC socket, and a desktop mpv in a window
provides exactly the same socket.

```sh
mkdir -p dev-data
mpv --idle=yes --force-window=yes --input-ipc-server=dev-data/mpv.sock &
cargo run -- --config config/dev.toml
```

Play something in that mpv window (drag a file onto it), then:

```sh
echo get | socat - UNIX-CONNECT:dev-data/state.sock   # live state
cat dev-data/state.json                               # the recorded snapshot
```

For an interactive session run `socat - UNIX-CONNECT:dev-data/state.sock`
and type `get` whenever you want the state.

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
`/run/remoteplayer-mpvwatcher/`, uses systemd's watchdog, and is sandboxed
(read-only system, no devices, Unix sockets only).

Check it:

```sh
systemctl status remoteplayer-mpvwatcher
echo get | socat - UNIX-CONNECT:/run/remoteplayer-mpvwatcher/state.sock
cat /var/lib/remoteplayer-mpvwatcher/state.json
journalctl -u remoteplayer-mpvwatcher -f
```

## Configuration

[`config/mpvwatcher.toml`](config/mpvwatcher.toml):

| Key | Default | Meaning |
|---|---|---|
| `mpv-socket` | — | mpv's `--input-ipc-server` path |
| `request-timeout-seconds` | `2` | per-reply timeout; on expiry the connection to mpv is re-established |
| `state-file` | — | JSON state file; comment out to disable the file and all periodic polling |
| `state-file-interval-seconds` | `7` | how often the state file is refreshed; applies to the file only |
| `state-socket` | — | the watcher's request/response socket; comment out to disable it |

At least one of `state-file` and `state-socket` is required.

Upgrading from 0.1.0: `interval-seconds` became `state-file-interval-seconds`,
and `[output] file` / `socket` became the top-level `state-file` /
`state-socket`. Old keys are rejected with an error that names them.

**SD card wear.** While a video plays, the position changes between updates,
so the file is rewritten every `state-file-interval-seconds` (about 500 small
writes per hour of playback at 7 s). Paused or idle, nothing is written.

## Repository layout

```
src/                  main.rs (timers) · sampler.rs (talks to mpv) · snapshot.rs · recorder.rs
                      mpv_connection.rs · output/{file,socket}.rs
config/               mpvwatcher.toml for the Pi, dev.toml for a desktop
deploy/systemd/       the unit file
docs/STATE-FORMAT.md  the JSON formats and the socket protocol
dev-notes/            how it works, as Rust learning material
```
