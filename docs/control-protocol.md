# Lemon Mix control protocol

Controllers such as the Stream Deck plugin (`streamdeck/`), `LemonMix.exe --control` and your own scripts reach the mixer through a named pipe. This page describes the pipe and its messages, protocol version **1**.

## The pipe

- **Name:** `\\.\pipe\LemonMixControl`.
- **Who serves it:** whichever process runs the mixer, which is the Windows service or the window when it runs on demand. When Lemon Mix is not running, the pipe does not exist, so opening it fails with "file not found".
- **Who may connect:** users signed in at the computer may read and write; SYSTEM and administrators have full access. Connections from other computers are refused.
- **Messages:** every message is one line of UTF-8 JSON ending in `\n`. A line may be at most 4 MiB long.
- **One request per connection:** send one request line and read one reply line, then the server closes the connection. A subscription is the exception: it stays open (see below).

Node.js, for example:

```js
const socket = require("node:net").connect("\\\\.\\pipe\\LemonMixControl");
socket.on("connect", () => socket.write('{"op":"control","action":{"action":"mute","target":"Mic"}}\n'));
socket.on("data", (data) => console.log(data.toString()));
```

## Replies

Every request except `subscribe` gets one reply:

```json
{ "snapshot": { ... }, "version": "0.1.0", "protocol": 1, "error": null }
```

- `error` is `null` when the request was carried out, or a sentence saying why it was not, written to be shown to people.
- `snapshot` is the state after the request (see [The snapshot](#the-snapshot)).
- `protocol` goes up only when a change would break existing controllers. Fields may be added to any message at any time, so ignore fields you don't know.

## Naming strips and buses

A *target* names one strip or bus in any of these ways. Names and IDs are matched ignoring case.

| Form | Example | Notes |
|---|---|---|
| Key | `5f06d376711a` | Permanent. It stays with the strip or bus when it is renumbered or renamed. Controllers should store keys. |
| ID | `H1`, `A2`, `V3`, `B3` | Hardware strips `H`, hardware buses `A`, virtual strips `V<cable id>`, virtual buses `B<cable id>`. Removing a hardware strip renumbers the ones after it. |
| Name | `Mic` | Only if exactly one strip or bus has that name. |
| Kind and name | `strip:Game`, `bus:Game` | For a loopback device, whose strip and bus share its name. |

## Requests

### `control`: change a strip or bus

```json
{ "op": "control", "action": { "action": "mute", "target": "Mic", "on": true } }
```

| `action` | Fields | Effect |
|---|---|---|
| `mute` | `target`, `on` | Mutes a strip or bus. |
| `solo` | `target`, `on` | Solos a strip. While any strip is soloed, only soloed strips are heard. |
| `mono` | `target`, `on` | Sums a strip's or bus's left and right channels. |
| `route` | `strip`, `bus`, `on` | Sends a strip to a bus. Refused if it would make a loop through a bus-fed strip. |
| `gain` | `target`, `db` | Sets a fader, from -60 (silent) to +12 dB. |
| `adjust_gain` | `target`, `db` | Moves a fader by `db`, up or down, kept within -60 to +12 dB. |

`on` is `true` to turn a switch on and `false` to turn it off; leave it out (or send `null`) to toggle it.

### `status`

```json
{ "op": "status" }
```

Returns the snapshot and changes nothing.

### `subscribe`: follow changes

```json
{ "op": "subscribe", "meters": true }
```

The connection stays open, and the server sends one event per line until you close it or Lemon Mix stops.

- **State event**, sent first and then whenever something a controller would show changes:

  ```json
  { "event": "state", "protocol": 1, "version": "0.1.0", "snapshot": { ... } }
  ```

  In this snapshot the meters are zero, and so are each device's buffer fill and drift correction, because they change all the time.

- **Meter event**, sent about 15 times a second, only if `meters` is `true`:

  ```json
  { "event": "meters",
    "strips": [{ "key": "5f06d376711a", "peak": [0.5, 0.4], "rms": [0.2, 0.18], "reduction": 1.0, "duck": 1.0 }],
    "buses": [ ... ] }
  ```

  `peak` and `rms` are linear (1.0 is full scale), left then right. `reduction` is the gain reduction from the gate or compressor (strips) or the limiter (buses), as a linear factor where 1.0 means none.

When the pipe closes, Lemon Mix has stopped. Connect again every few seconds to pick it up when it starts.

### `mixer`: what the window sends

The window uses `{"op": "mixer", "command": {...}}` for everything it changes. Its commands are in `MixerCommand` in `app/src/protocol.rs`. Controllers can use them too, but they name strips and buses only by ID.

`strip` and `bus` replace all of a strip's or bus's settings: send back the whole `params` from the snapshot with your changes, since any setting left out goes back to its default. For a single change, a `control` action is simpler.

`device` sets a hardware strip's or bus's device. A strip can take a bus's output instead of a device:

```json
{ "op": "mixer", "command": { "cmd": "device", "id": "H3", "endpoint": "bus:A2", "exclusive": false } }
```

The strip then hears the bus as it left the bus, 1 ms later, with no virtual device in between. Lemon Mix refuses a change that would let a bus hear itself, and turns off the strip's send to the bus it takes.

A strip is ducked, lowered while other strips play, by the strips named with `duck` (`strip` and `sources`, by ID; an empty list turns it off). How far and how fast are in its `params.duck`: `amount_db`, `threshold_db`, `fade_down_ms`, `hold_ms` and `fade_up_ms` (each fade's whole duration), set with `strip` like its other settings:

```json
{ "op": "mixer", "command": { "cmd": "duck", "strip": "V2", "sources": ["H1"] } }
```

Noise suppression is `params.denoise`, from 0 (off) to 10 (fully denoised).

Likewise the noise gate's finer settings are in `params.gate_options` (`range_db`, `attack_ms`, `hold_ms`, `release_ms`) and the compressor's in `params.comp_options` (`attack_ms`, `release_ms`, `knee_db`, `makeup_db`, which is `null` for automatic, and `manual` with `threshold_db` and `ratio`).

Profiles are managed with `create_profile` (`name`, and `from_defaults` to start from a new configuration's settings rather than the current ones; it switches to the new profile), `switch_profile` (`name`) and `delete_profile` (`name`, not the active one):

```json
{ "op": "mixer", "command": { "cmd": "switch_profile", "name": "Gaming" } }
```

A strip can also capture an application, named by its program's file name; the snapshot's `apps` lists the applications with sound:

```json
{ "op": "mixer", "command": { "cmd": "device", "id": "H3", "endpoint": "app:Spotify.exe", "exclusive": false } }
```

## The snapshot

The fields a controller usually needs (the full set is `Snapshot` in `app/src/protocol.rs`):

```json
{
  "strips": [
    { "id": "H1", "key": "5f06d376711a", "label": "Mic",
      "params": { "gain_db": -6.0, "mute": false, "solo": false, "mono": false, "pan": 0.0,
                  "eq": [0.0, 0.0, 0.0], "denoise": 0.0, "gate": 0.0, "comp": 0.0 },
      "routes": ["A1", "B2"],
      "port": { "endpoint": "default", "state": "running", "device_name": "Microphone (USB)" } }
  ],
  "buses": [
    { "id": "A1", "key": "80d995df5b55", "label": "A1",
      "params": { "gain_db": 0.0, "mute": false, "mono": false, "limiter": null },
      "port": { "endpoint": "default", "state": "running", "device_name": "Speakers" } }
  ],
  "cables": [{ "id": 1, "serial": "8F3A2C1D", "name": "Game", "kind": "loopback" }]
}
```

- `routes` lists the IDs of the buses a strip sends to. `duck_by` lists the IDs of the strips that duck it, and its meter's `duck` is how far it is ducked now, as a linear factor (1.0 is not at all).
- `port` is present only for hardware strips and buses. `state` is `off`, `opening`, `running` or `failed`, or `bus` for a strip fed from a bus; in that case `device_name` names the bus. A strip capturing an application is `running` while it captures, with the application's name in `device_name`, and `failed` while it waits for the application to start.
- `apps` lists the applications with sound: `{ "exe": "Spotify.exe", "name": "Spotify", "active": true }`, where `active` means playing now.
- `profile` is the active profile and `profiles` every profile, in alphabetical order.

## From the command line

`LemonMix.exe --control <command>` sends these requests for you. `LemonMixCtl.exe <command>` does the same as a console program, so terminals wait for it and show its output. Run either with `help` for the list of commands.

| Command | Request |
|---|---|
| `mute Mic` | `control` `mute`, toggled |
| `route Mic A2 on` | `control` `route` |
| `adjust Mic -3` | `control` `adjust_gain` |
| `gain Mic -12` | `control` `gain` |
| `status` | `status`, shown as a table with each strip's and bus's key |
| `profile Gaming` | `mixer` `switch_profile` |
| `watch meters` | `subscribe`, printing each event |
| `json '{"op":"status"}'` | any request as JSON |

The exit code is 0 when the request was carried out, 1 when it was refused or Lemon Mix is not running, and 2 when the command was not understood.
