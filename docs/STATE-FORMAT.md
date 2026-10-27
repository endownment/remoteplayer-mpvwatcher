# State format, version 1

Keys are kebab-case. Readers must ignore unknown keys; any incompatible
change increments `format-version`.

The **snapshot** — the object below — is what `last` returns inside `state`
and what the state file holds (pretty-printed). `get` returns the same
`mpv` and `player` parts for the live state (see "Socket protocol").

```json
{
  "format-version": 1,
  "captured-at-unix-ms": 1792976499186,
  "sequence": 42,
  "mpv": {
    "pid": 812,
    "version": "mpv 0.40.0"
  },
  "player": {
    "playlist": [
      { "filename": "/srv/remoteplayer/media/series/s01e01.mkv", "title": null },
      { "filename": "/srv/remoteplayer/media/series/s01e02.mkv", "title": null }
    ],
    "playlist-pos": 1,
    "time-pos": 1283.512,
    "paused": false,
    "volume": 80.0,
    "muted": false,
    "speed": 1.0,
    "audio-track": 2,
    "subtitle-track": "no",
    "secondary-subtitle-track": "no",
    "audio-delay": 0.0,
    "subtitle-delay": 0.25
  }
}
```

| Key | Type | mpv property | Notes |
|---|---|---|---|
| `format-version` | integer | — | `1` |
| `captured-at-unix-ms` | integer | — | when this state was first observed |
| `sequence` | integer | — | +1 for every recorded change; continues across restarts when the state file is writable |
| `mpv.pid` | integer or null | `pid` | the process the state belongs to |
| `mpv.version` | string or null | `mpv-version` | |
| `player.playlist[].filename` | string | `playlist` | absolute path or URL, as mpv reports it |
| `player.playlist[].title` | string or null | `playlist` | |
| `player.playlist-pos` | integer or null | `playlist-pos` | `null` when nothing is selected (mpv: -1) |
| `player.time-pos` | number or null | `time-pos` | seconds; `null` when idle |
| `player.paused` | boolean | `pause` | |
| `player.volume` | number | `volume` | 0–130 |
| `player.muted` | boolean | `mute` | |
| `player.speed` | number | `speed` | |
| `player.audio-track` | integer, `"no"` or `"auto"` | `aid` | `"auto"` instead of `"no"` while mpv has no audio output (see below) |
| `player.subtitle-track` | integer, `"no"` or `"auto"` | `sid` | |
| `player.secondary-subtitle-track` | integer, `"no"` or `"auto"` | `secondary-sid` | |
| `player.audio-delay` | number | `audio-delay` | seconds |
| `player.subtitle-delay` | number | `sub-delay` | seconds |

## Socket protocol

Request/response over a Unix stream socket, one line (terminated by `\n`)
each way. The watcher never sends anything unasked. A client may keep the
connection open and send any number of requests.

### `get` — the live state, read from mpv now

```json
{"format-version":1,"captured-at-unix-ms":1792976499186,"mpv-reachable":true,"mpv":{...},"player":{...}}
{"format-version":1,"captured-at-unix-ms":1792976499186,"mpv-reachable":false,"detail":"cannot connect to /run/remoteplayer-mpv/mpv.sock: ..."}
```

`get` shows mpv as it is: right after a crash that is the new, empty mpv.

### `last` — the snapshot to restore from

```json
{"format-version":1,"known":true,"state":{ ...the snapshot above... }}
{"format-version":1,"known":false}
```

`known` is false when the watcher has not seen mpv since it started and
had no usable state file — "the player was not running, its state is
unknown".

### `status` — diagnostics

```json
{
  "format-version": 1,
  "mpv-connected": true,
  "mpv-error": null,
  "last-known": true,
  "last-sequence": 42,
  "state-file": {
    "mode": "read-write", "path": "/srv/remoteplayer/state/mpvwatcher/state.json", "volatile": false,
    "last-written-sequence": 42, "last-write-error": null
  }
}
```

`state-file.mode` is `read-write` (with `volatile`: on tmpfs/overlay, lost on
power-off), `read-only` (with `reason`: why it cannot be written; the file is
not used) or `not-configured`.

### Anything else

`{"error": "unknown request `...`; supported: get, last, status"}`; empty lines
are ignored.

## Semantics of the snapshot

- It changes only when the player part or the mpv pid changes;
  `captured-at-unix-ms` is the moment the state was **first seen**.
- If mpv's pid changed and the new mpv has an empty playlist, the previous
  snapshot is kept (see README, "The snapshot survives what it is meant to
  survive").
- An empty `playlist` from the same pid means playback was stopped on purpose.
- mpv deselects the audio track itself when it cannot open the audio output.
  That is not a user choice, so in that situation `"auto"` is recorded.
- The position is followed at most once per second; in the state file it may
  be up to `position-save-seconds` old.

## Restoring from it

remoteplayer-restservice restores a state with these mpv commands (mpv ≥ 0.38):

```
stop
set_property pause <paused>
set_property volume <volume>              (likewise mute, speed, audio-delay, sub-delay)
loadfile <filename> append -1 ""          for every playlist entry except the current one
loadfile <filename> append -1 "start=<time-pos>,aid=<audio-track>,sid=<subtitle-track>,secondary-sid=<...>"
                                          for the current entry
set_property playlist-pos <playlist-pos>
```
