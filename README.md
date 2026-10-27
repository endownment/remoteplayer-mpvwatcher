# remoteplayer-mpvwatcher

Follows the playback state of the **remoteplayer** mpv instance, so playback
can resume after mpv crashed, after systemd restarted it, and — when there is
somewhere to write — after the Raspberry Pi was rebooted or unplugged.

The watcher keeps one snapshot: the last state worth restoring. It is
updated from mpv's own change events (there is no polling) and offered on the
watcher's socket; [remoteplayer-restservice](https://github.com/endownment/remoteplayer-restservice)
asks for it (`last`) whenever a new, empty mpv appears.

## The snapshot survives what it is meant to survive

When mpv was restarted (its pid changed) and the new process is still empty,
the watcher **keeps** the previous snapshot instead of recording "nothing is
playing" — otherwise the restart that makes a restore necessary would
destroy the data for it. As soon as the new mpv plays something (restored or
started by a user), its state replaces the snapshot. Stopping playback inside
the same mpv process is recorded too, so a deliberate stop is never undone.

How far the snapshot reaches depends on the state file:

| What happened | Snapshot available? |
|---|---|
| mpv crashed / was restarted | yes, from the watcher's memory |
| remoteplayer-restservice restarted | yes, from the watcher's memory |
| the watcher itself restarted | only with a writable state file |
| reboot, power-off, unplugged | only with a writable state file |

## The state file

`state-file` is checked once at start-up by actually creating and deleting a
file next to it:

- **writable** — the previous file is loaded, and the snapshot is written to
  it: at once whenever anything but the position changes (file, playlist,
  pause, volume, tracks) and after a seek; while playing, the position alone
  at most every `position-save-seconds`; and once more when the watcher stops.
  The file is replaced atomically (temporary file, fsync, rename), so a power
  cut never leaves half a file.
- **not writable** (read-only mount, no permission, partition missing) or
  **commented out** — the snapshot lives in memory only. A file found in an
  unwritable place is **not** used: it may be arbitrarily old, e.g. baked into
  a read-only image.

On tmpfs or an overlay (the Raspberry Pi OS overlay file system) the file is
written, but the watcher warns that it lives in RAM and will not survive a
power-off. `status` reports all of this.

## The socket

Request/response, one line each way, nothing is sent unasked:

| Request | Answer |
|---|---|
| `get` | mpv's state right now, read on demand; `"mpv-reachable": false` when mpv is down |
| `last` | the snapshot to restore from; `"known": false` when there is none |
| `status` | mpv connection, state-file mode and last write |

```sh
echo last   | socat - UNIX-CONNECT:/run/remoteplayer-mpvwatcher/state.sock
echo status | socat - UNIX-CONNECT:/run/remoteplayer-mpvwatcher/state.sock
socat - UNIX-CONNECT:/run/remoteplayer-mpvwatcher/state.sock   # interactive: type get / last / status
```

The exact JSON is described in [docs/STATE-FORMAT.md](docs/STATE-FORMAT.md).

## Quick start on a development machine

You need Rust (see [BUILD.md](BUILD.md)) and mpv. No Raspberry Pi required:
the watcher only talks to mpv's IPC socket, and a desktop mpv in a window
provides exactly the same socket.

```sh
mkdir -p dev-data
mpv --idle=yes --force-window=yes --input-ipc-server=dev-data/mpv.sock &
cargo run -- --config config/dev.toml
echo last | socat - UNIX-CONNECT:dev-data/state.sock
```

To work with a locally running remoteplayer-restservice, point `mpv-socket`
at its mpv (`../remoteplayer-restservice/dev-data/run/mpv.sock`) and
`state-socket` at the restservice's `restore.watcher-socket`
(`../remoteplayer-restservice/dev-data/watcher/state.sock`).

## Installation on the Raspberry Pi

Target: Raspberry Pi OS (Debian 12 "bookworm" or 13 "trixie"), 64-bit
(`arm64`) or 32-bit (`armhf`), mpv running as `remoteplayer-mpv.service`
(from the remoteplayer-restservice package), the writable partition mounted
at `/srv/remoteplayer` (see "Read-only SD card" in the remoteplayer-restservice
README).

### From the APT repository (recommended)

The packages of every release are published in a signed APT repository,
[endownment/remoteplayer-apt](https://github.com/endownment/remoteplayer-apt),
so updates come with `apt upgrade`:

```sh
sudo curl -fsSLo /usr/share/keyrings/remoteplayer-archive-keyring.gpg \
    https://endownment.github.io/remoteplayer-apt/remoteplayer-archive-keyring.gpg
sudo curl -fsSLo /etc/apt/sources.list.d/remoteplayer.sources \
    https://endownment.github.io/remoteplayer-apt/remoteplayer.sources
sudo apt update
sudo apt install remoteplayer-mpvwatcher
sudo systemctl enable --now remoteplayer-mpvwatcher.service
```

Without the repository: tagged releases on GitHub carry `.deb` files for
`arm64`, `armhf` and `amd64` (BUILD.md, "Debian packages"), installed with
`sudo apt install ./remoteplayer-mpvwatcher_0.1.0-1_arm64.deb`.

It installs `/usr/bin/remoteplayer-mpvwatcher`, `/etc/remoteplayer/mpvwatcher.toml`
(kept on upgrades), `remoteplayer-mpvwatcher.service`, the system user and
group `remoteplayer` (shared with remoteplayer-restservice: mpv's socket is
open to the group only) and `/srv/remoteplayer/state/mpvwatcher`. Nothing is enabled on
installation. An upgrade restarts the watcher if it runs — it saves its
snapshot when stopping and loads it when starting.

### By hand

Build the binary first (BUILD.md), then:

```sh
sudo adduser --system --group --home /var/lib/remoteplayer remoteplayer   # if not there yet
sudo install -m 0755 target/release/remoteplayer-mpvwatcher /usr/bin/
sudo install -d /etc/remoteplayer
sudo install -m 0644 config/mpvwatcher.toml /etc/remoteplayer/mpvwatcher.toml
sudo install -d -o remoteplayer -g remoteplayer -m 2770 /srv/remoteplayer/state/mpvwatcher   # skip on a fully read-only system
sudo install -m 0644 deploy/systemd/remoteplayer-mpvwatcher.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now remoteplayer-mpvwatcher.service
```

The unit runs as `remoteplayer` with the group `remoteplayer`, keeps the
socket in `/run/remoteplayer-mpvwatcher/` (open to the group), may write only
`/srv/remoteplayer/state/mpvwatcher/`, uses systemd's watchdog, and is
sandboxed (read-only system, no devices, Unix sockets only). It has no
`StateDirectory=`, which systemd could not create on a read-only root, and
restarts without a limit (`StartLimitIntervalSec=0`).

Other programs may ask the watcher too: they need to be in the group
`remoteplayer`. To run the watcher as another user, change only `User=` in a
drop-in. Both are described for all remoteplayer units in the
remoteplayer-restservice README, "Other programs using mpv" and "Running as
another user".

### Checking and logs

```sh
systemctl status remoteplayer-mpvwatcher
echo status | socat - UNIX-CONNECT:/run/remoteplayer-mpvwatcher/state.sock
journalctl -u remoteplayer-mpvwatcher -f
journalctl -u remoteplayer-mpvwatcher -p warning -b      # only warnings and errors of this boot
```

Under systemd the watcher logs through the native journal protocol (levels
become priorities, fields such as `socket` become `F_SOCKET`). It writes one
line when mpv's socket is not there yet, one when it is picked up ("following
mpv"), one warning when it is lost and one when it is back — not one per
reconnect attempt. A broken configuration ends it with exit status 78, which
the unit does not restart (`RestartPreventExitStatus=78`).

## Configuration

[`config/mpvwatcher.toml`](config/mpvwatcher.toml):

| Key | Default | Meaning |
|---|---|---|
| `mpv-socket` | — | mpv's `--input-ipc-server` path |
| `request-timeout-seconds` | `2` | per-reply timeout towards mpv |
| `state-file` | — | keeps the snapshot across restarts and reboots; used only if writable |
| `position-save-seconds` | `10` | while playing, how often the position alone is written; `0`: only with other changes and on shutdown |
| `state-socket` | — | the request/response socket |

At least one of `state-file` and `state-socket` is required.

Upgrading from earlier versions: `interval-seconds` / `state-file-interval-seconds`
are gone (there is no polling any more), and `[output] file` / `socket` are
the top-level `state-file` / `state-socket`. Old keys are rejected with an
error that names them.

**Writes.** While a video plays, the file is rewritten every
`position-save-seconds` (360 small writes per hour at 10 s) plus once per real
change. Paused or idle, nothing is written.

## Repository layout

```
src/                  main.rs · tracker.rs (follows mpv, writes the file) · sampler.rs (`get`)
                      snapshot.rs · recorder.rs · shared.rs · mpv_connection.rs · output/{file,socket}.rs
config/               mpvwatcher.toml for the Pi, dev.toml for a desktop
deploy/systemd/       the unit file
docs/STATE-FORMAT.md  the JSON formats and the socket protocol
dev-notes/            how it works, as Rust learning material
```
