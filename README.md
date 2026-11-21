# Lemon Mix

Lemon Mix is an audio mixer for Windows that creates its own virtual audio devices without a custom audio driver. It mixes those virtual devices with the computer's physical microphones, headsets and speakers, and routes any input to any combination of outputs.

Each virtual device is a USB Audio Class 2.0 device served over a local USB/IP server and attached by [usbip-win2](https://github.com/vadimgrn/usbip-win2), so Windows' in-box USB audio driver (`usbaudio2.sys`) exposes it as an ordinary playback and/or recording device. usbip-win2 is installed separately and is the only driver Lemon Mix needs.

## Features

- **Input strips**: up to 8 hardware inputs (physical recording devices) and one virtual input per virtual input or loopback device.
- **Output buses**: up to 8 hardware buses, A1 to A8 (physical playback devices), and one virtual bus, B1, B2, ..., per virtual output or loopback device.
- **Routing matrix**: any strip to any combination of buses, with a button per bus on every strip.
- **Per strip**: fader (-60 to +12 dB), mute, solo, mono, balance, three-band EQ (bass, mid, treble), noise gate and compressor, peak meters with gain-reduction display.
- **Per bus**: fader, mute, mono, peak limiter, meters.
- **Virtual devices** of three kinds, added, renamed and removed in the app: *input* (a playback device that apps play into), *output* (a recording device that apps record from, such as a microphone for Discord or OBS) and *loopback* (both, the recording side returning the playback side, like a classic virtual cable).
- **Physical devices** in WASAPI shared mode, or exclusive mode for the lowest latency, with automatic reconnection when a device is unplugged and plugged back, changes format, or (when set to "Default device") when Windows' default changes.
- Bit-exact paths: a single source at unity gain with no processing passes through untouched.
- Runs on demand from its window (and the notification area), or as a Windows service from start-up.
- Settings are saved as you change them, in a `config.toml` that keeps your own comments.

The layout follows the classic five-in, five-out virtual mixer: five hardware inputs and five hardware buses by default, plus as many virtual inputs and outputs as you create.

## How it works

The application runs a local USB/IP server that exports each virtual device. usbip-win2 attaches them to Windows, which finds standard USB audio devices.

Every virtual device runs on the USB bus frame clock that Windows reads, one 48-sample packet per 1 ms frame. The mixer runs on that same clock: frame F of every virtual input is mixed into frame F of every bus, so mixing virtual devices needs no resampling at all. A virtual input's packet for frame F is mixed `latency_ms` frames later, which leaves Windows room to send playback late after a system stall.

Physical devices run on their own crystals, tens of parts per million away from the bus clock (50 ppm is one 1 ms packet every 20 seconds). Each physical input and output therefore sits behind an adaptive resampler: a ring buffer between the device thread and the mixer, a polyphase windowed-sinc resampler, and a controller that watches the ring's fill and steers the resampling ratio around its nominal value (1.0 at 48 kHz, 0.91875 for a 44.1 kHz device), so drift is absorbed smoothly instead of by dropped or repeated audio.

The mixer works in f32 and converts from and to packed 24-bit only at the virtual devices. Gain and routing changes ramp across one packet, so there is no zipper noise; where a virtual input's stream is cut (a gap, or a late stream moved to keep playing), the strip crossfades for 1 ms instead of stepping.

## Audio format

Every virtual device uses a fixed format: 48,000 Hz, stereo, 24-bit PCM, on synchronous isochronous endpoints with no feedback endpoint. Physical devices may run at any rate and channel count Windows offers; strips and buses are stereo (the first two channels, or a mono device on both).

## Requirements

- Windows 10 version 1703 or later, or Windows 11, for the in-box USB Audio 2.0 driver
- x64 processor
- The usbip-win2 transport driver
- Administrator approval when the virtual devices are attached to Windows, or once to install the Windows service

## Getting started

1. Start Lemon Mix.
2. If usbip-win2 is not installed yet, select **Options > Download and install driver**, and reboot if the installer asks.
3. Under **Virtual devices**, type a name, choose a kind and select **Add**. Approve the one-time administrator prompt; the device then appears in Windows Sound under its name.
4. Pick physical devices for the hardware strips and buses from the device menus on each column. A1 starts on the default playback device.
5. Route each strip with its bus buttons. A new loopback device's input starts routed to its own output, so it behaves as a plain virtual cable; a new input device starts routed to A1.

Typical setups:

- *Game and music to headphones and stream*: make a "Game" input device and set it as the game's output; route it to A1 (headphones) and to the B bus of a "Stream" output device that OBS records.
- *Microphone with processing for calls*: pick the microphone on a hardware strip, turn up its gate and compressor, and route it to the B bus of a "Mic" output device; select "Mic" as the microphone in the call app.

Double-click a hardware strip's or bus's name to rename it, a fader or knob to reset it.

## On demand or as a Windows service

The header shows where the mixer runs.

- **On demand** (the default): the window runs the mixer. Closing the window keeps it running in the notification area; **Quit** (in the tray menu or the Options menu) stops it, and the virtual devices leave Windows Sound.
- **As a Windows service**: the service runs the mixer from Windows start-up, without the window and without an administrator prompt each time. The window only manages it: every change goes to the service through its pipe, and closing the window leaves the audio running.

**Options > Install as Windows service** asks for administrator approval once, copies the program to `%ProgramFiles%\Lemon Mix` and your `config.toml` to `%ProgramData%\Lemon Mix`, and hands the mixer over to the service. **Options > Uninstall Windows service** stops and removes it, copies its settings back, and runs the mixer on demand again.

The service runs as LocalSystem. Physical devices chosen by name are machine-wide and work from the service; check the log line `Devices visible to this process` in the service's log if a device is missing there.

## Configuration

Settings are in `config.toml` in the settings folder: `%LOCALAPPDATA%\Lemon Mix` on demand, or `%ProgramData%\Lemon Mix` for the service (**Options > Open settings folder**). The application writes the `[[cable]]`, `[[strip]]` and `[[bus]]` tables; everything else, comments included, is left as you wrote it.

```toml
listen = "127.0.0.1:3241"
latency_ms = 20
buffer_ms = 20
hardware_inputs = 5
hardware_outputs = 5
diagnostics = false
stall_test_ms = 0

[[cable]]
id = 1
serial = "8F3A2C1D"
name = "Game"
kind = "input"

[[strip]]
id = "H1"
label = "Mic"
device = "default"
exclusive = false
gain_db = 0.0
mute = false
solo = false
mono = false
pan = 0.0
eq = [0.0, 0.0, 0.0]
gate = 3.0
comp = 4.0
routes = ["A1", "B2"]

[[bus]]
id = "A1"
device = "default"
exclusive = false
gain_db = 0.0
mute = false
mono = false
limiter = -1.0
```

| Setting | Meaning | Default |
|---|---|---|
| `listen` | Address of the local USB/IP server. It differs from the usual 3240, so another USB/IP server can run beside Lemon Mix. | `"127.0.0.1:3241"` |
| `latency_ms` | Delay between a virtual input's playback side and the mixer, 1-1000 ms. Windows sends playback about 10 ms ahead; this is how late, after a system stall, playback can still arrive in time. | `20` |
| `buffer_ms` | Audio kept between each physical device and the mixer, 5-200 ms. Raise it if hardware inputs or outputs crackle. | `20` |
| `hardware_inputs`, `hardware_outputs` | Hardware strips and hardware buses, 1-8 each. | `5` |
| `diagnostics` | Detailed USB/IP traces in the log. | `false` |
| `stall_test_ms` | For troubleshooting: once a minute, hold every audio transfer and the mixer this long, to reproduce a system stall. 0-10000; 0 is off. | `0` |
| `[[cable]]` | One table per virtual device, up to 32. `serial` is its identity in Windows; `id` (1-255) sets its USB product ID and bus ID; `kind` is `input`, `output` or `loopback` (the default for tables without one). | none |
| `[[strip]]`, `[[bus]]` | The mixer's settings by ID: `H1`... hardware strips, `V<cable id>` virtual inputs, `A1`... hardware buses, `B<cable id>` virtual buses. `device` is `""` (none), `"default"` or a WASAPI endpoint ID. | written by the app |

Invalid values fall back to their defaults and the log says so. If the file is not valid TOML, the application starts with the defaults and does not overwrite the file until it is fixed.

Each kind of virtual device has its own USB product ID range (`FFFF:CBxx` loopback, `CCxx` input, `CDxx` output), so changing a device's kind or name makes it a new device in Windows, and the old one is removed.

### Command line

```
LemonMix.exe [--cables <COUNT>] [--listen <ADDRESS>] [--latency-ms <MS>] [--buffer-ms <MS>] [--diagnostics [<BOOL>]] [--stall-test-ms <MS>]
```

| Option | Action |
|---|---|
| `--self-test` | Validate every device kind's descriptors and control requests, and the bit-exact audio path on its own and through the mixer, then exit |
| `--dump-descriptors` | Print each virtual device's USB descriptors, then exit |
| `--measure-latency <PLAYBACK> <RECORDING>` | Play clicks into a playback device and time their arrival on a recording device (matched by name, for example a virtual input routed to a virtual output), then exit |
| `-V`, `--version` | Print the version, then exit |
| `-h`, `--help` | List all options |

The executable is a windowed application; to see its output, redirect it, for example `LemonMix.exe --self-test | more`.

## Logs and troubleshooting

The log is `LemonMix.log` in the settings folder, cleared at each start.

- `AUDIO HEALTH` (per virtual device, at most every 10 s, when something changed): `missing_ms` (silence where playback was due), `realigned` and `caught_up` (late streams moved later and back), stream resyncs and `halt_clears` (stream resets by Windows).
- `ENGINE HEALTH` (when the mixer had to catch up): `catch_up_batches` (the frame clock jumped by more than 1 ms), `max_batch_frames`, `skipped_frames` (after a stall longer than 250 ms) and `pulled_by_capture`.
- `DEVICE HEALTH` (per physical device, when something changed and once a minute while it runs): its rate, resampling `ratio` and `correction_ppm`, ring `fill_ms`, `underruns`, `overruns` and `opens`. A correction that settles at a steady value is the device's clock drift; underruns or overruns mean `buffer_ms` is too small for the device or the system.
- `DEVICE ... opened / lost / could not stream`: physical devices coming and going.

`stall_test_ms = 30` reproduces a system stall once a minute. `--measure-latency` measures the whole path through Windows and the mixer.

## Building from source

Lemon Mix is written in Rust, as a Cargo workspace.

1. Install Rust with [rustup](https://rustup.rs). The project builds with the self-contained GNU toolchain (no Visual Studio needed):
   ```
   rustup-init.exe --default-host x86_64-pc-windows-gnu
   ```
2. Build: `cargo build --release`. The result is `target\release\LemonMix.exe`, a single self-contained file.
3. Test: `cargo test --workspace` and `target\release\LemonMix.exe --self-test`.

`Cargo.lock` pins a few dependencies (`parking_lot`, `parking_lot_core`, `libloading`, `arboard`) and the `windows` crate at 0.58 to versions that use bundled import libraries: newer ones link with `raw-dylib`, which needs a full MinGW `dlltool` and assembler that the self-contained toolchain lacks. For the same reason `vendor/webbrowser` stands in for the crate egui uses to open links. Keep these pins when updating dependencies on the GNU toolchain.

| Path | Contents |
|---|---|
| `crates/cables` | The virtual devices: USB/IP server, UAC2 descriptors and control requests for each kind, the bus frame clock, the frame-indexed delay line, attaching with usbip-win2 |
| `crates/engine` | The mixer: routing graph, mixing, gain ramps, crossfades, EQ, gate, compressor, limiter, meters |
| `crates/devices` | Physical devices: WASAPI streams and notifications, ring buffers, the resampler and drift controller |
| `app` | The application: window, notification area icon, configuration, the mixer host, the Windows service and its pipe, the administrator helper and the driver download |

## Project status

Lemon Mix is experimental. Its USB descriptors, USB/IP protocol handling, mixer, resampling under simulated clock drift and settings are covered by automated tests; real-world behaviour depends on the system, the USB/IP transport and the devices. Test it before relying on it live.

Not included yet: VST plug-ins, ASIO, network audio, recording to file and macros.

## Privacy

Audio is processed locally and stays in memory. The USB/IP server listens only on the local machine by default. The only network access is the optional driver download from GitHub. The service's pipe accepts only local connections from users signed in at the computer.

## Licence

MIT; see [`LICENSE`](LICENSE). Third-party licences are listed in [`LICENSES`](LICENSES/README.md); after changing dependencies, regenerate them with `powershell -ExecutionPolicy Bypass -File scripts\update-licenses.ps1`.
