# State format, version 1

Keys are kebab-case. Readers must ignore unknown keys; any incompatible
change increments `format-version`.

- The **state file** holds the object below, pretty-printed.
- The **state socket** answers `get` with the same object on one line, minus
  `sequence` (see "Socket protocol").

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
| `captured-at-unix-ms` | integer | — | file: when this state was first observed; socket: when mpv was queried |
| `sequence` | integer | — | file only: +1 for every written change; survives watcher restarts |
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

| Request | Reply |
|---|---|
| `get` | mpv's state at this moment: the object above without `sequence` |
| empty line | nothing |
| anything else | `{"error": "unknown request `...`; supported: get"}` |

When mpv cannot be reached, `get` is answered with
`{"error": "cannot connect to /run/remoteplayer-mpv/mpv.sock: ..."}`. A reply
is a state exactly when it has no `error` key.

`get` reports mpv as it is, so right after a crash it shows the new, empty
mpv. The snapshot needed for a restore is the one in the state file.

## Semantics of the state file

- A new state is written only when the player part or the mpv pid changed.
  `captured-at-unix-ms` is therefore the moment the state was **first seen**,
  not the time of the last poll.
- If mpv's pid changed and the new mpv has an empty playlist, the previous
  state stays in the file (see README, "The state file keeps what is needed
  to resume").
- mpv deselects the audio track itself when it cannot open the audio output.
  That is not a user choice, so in that situation `"auto"` is recorded.
- An empty `playlist` from the same pid means playback was stopped on purpose.

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
