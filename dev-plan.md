# Project brief: Voicemeeter-style mixer in Rust, built on virtual-cables

## Goal
Build a single Windows application that provides Voicemeeter-like audio mixing and routing,
using the existing `virtual-cables` codebase (this repo) as its foundation. The cables move
into the new application: one program runs the USB/IP server, the virtual devices, the mixer
engine, physical device I/O and the UI. It must still be able to run as a Windows service,
as virtual-cables does today. No custom kernel audio drivers; usbip-win2 remains the only
driver dependency.

## What already exists (keep these behaviours)
- Each cable is a USB Audio Class 2.0 device served over a local USB/IP server and attached by
  usbip-win2, so Windows' in-box `usbaudio2.sys` exposes it as playback + recording endpoints.
- Fixed format: 48 kHz, stereo, 24-bit PCM. Synchronous isochronous endpoints, one 48-sample
  packet per 1 ms frame, no feedback endpoint. Do not switch to UAC1 (it causes stream-reset
  loops under load) and do not switch to async/feedback endpoints.
- `busclock.rs`: frame numbers come from `QueryInterruptTime` (coarse) because usbip-win2
  reports `KeQueryInterruptTime / 10000` as the bus frame. Completions must never be reported
  on a frame the coarse clock hasn't reached. This clock is system-wide, so ALL cables share one
  frame counter. Keep this exactly.
- `audio.rs` `DelayLine`: frame-indexed ring; capture frame F reads playback frame F - delay;
  missing frames are silent; late streams realign and later catch up; stale slots never replay;
  unread audio never overwritten. Keep its semantics and tests.
- Cable identity via stable `serial`; PID `FFFF:CBxx` from cable id; Windows service install/
  uninstall flow; named pipe between window and service; `config.toml` rewriting that preserves
  user comments; `AUDIO HEALTH` log counters; `stall_test_ms`; `--self-test`.

## Clock domains (the core design constraint)
1. Virtual domain: every cable endpoint and the mixer engine, all on the bus frame clock.
   Mixing between virtual sources needs NO resampling, just frame-indexed summing.
2. Each physical device (WASAPI) runs on its own crystal. Expect tens of ppm of drift
   (50 ppm is about one 48-sample frame every 20 s), so every physical input and output needs
   an adaptive resampler (ASRC) between its device buffer and the virtual domain.

## Target architecture
Cargo workspace:
- `cables` (lib): extracted from the current code: usbip server, UAC2 device/descriptors,
  busclock, DelayLine-style frame buffers, attach/detach via usbip-win2. Behaviour unchanged.
- `engine` (lib): routing graph, mixing, gain, DSP, meters. Ticks on bus frames.
- `devices` (lib): WASAPI capture/render threads, ring buffers, ASRC + drift controller,
  device enumeration and hot-plug.
- `app` (bin): UI, config, Windows service, IPC between UI and service.

### Engine
- Processes audio in 1 ms frames (48 samples) in frame order. The coarse clock can jump by up
  to about 15.6 ms, so the engine must catch up by processing every pending frame in a batch.
- Strips (inputs): virtual inputs (cable playback endpoints that apps play into) and hardware
  inputs (physical capture devices, after ASRC).
- Buses (outputs): hardware buses A1..An (to physical render devices, via ASRC) and virtual
  buses B1..Bn (exposed as cable recording endpoints that apps like Discord/OBS select as mics).
- Routing matrix: any strip to any combination of buses.
- Mix in f32; convert from/to packed int24 only at endpoints; clamp once at each bus output.
  A single source routed at unity gain must remain bit-exact (int24 fits f32 exactly).
- Gain changes ramp linearly across a packet (no zipper noise). Add a 1-2 ms crossfade at
  DelayLine realign/catch-up points (currently hard cuts).
- Generalize the current fixed pairing (cable capture reads its own playback) into routing:
  capture endpoints read bus output for frame F; playback endpoints feed strip sources.

### Device kinds
Add a per-cable kind, implemented in the UAC2 descriptors:
- input: playback endpoint only (appears as a speaker apps play into),
- output: recording endpoint only (appears as a microphone),
- loopback: both, current virtual-cables behaviour.
Validate descriptor changes with `--dump-descriptors` and the self-test; usbaudio2 is strict
and fails silently on bad descriptors.

### Physical devices
- WASAPI event-driven, shared mode by default, exclusive mode optional. Run device threads under
  MMCSS ("Pro Audio") via AvSetMmThreadCharacteristics.
- Per device: lock-free SPSC ring between device thread and engine, adaptive resampler, and a
  PI controller on ring fill level that sets a resample ratio near the nominal one (1.0 at
  48 kHz, 0.91875 for a 44.1 kHz device). Candidate crate: `rubato` (verify its API supports
  changing the ratio while running).
- Handle device removal, re-plug, format changes and default-device changes
  (IMMNotificationClient) by reopening, without disturbing the rest of the engine.

### Real-time rules
- No allocation, blocking locks, logging or IPC on audio paths. DelayLine currently uses a
  Mutex: acceptable for phase 1, but keep critical sections to memcpy-sized work and plan to
  move to lock-free structures.
- Call `timeBeginPeriod(1)` at startup so the coarse bus clock advances finely instead of in
  15.6 ms steps. On Windows 11, check whether timer-resolution requests from a windowless
  service are honoured (possible opt-out: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION;
  verify in Microsoft docs).
- `busclock` falls back to a process-relative clock if QueryInterruptTime is unavailable.
  That gives frame numbers inconsistent with usbip-win2. On Windows, fail loudly instead.

### UI, config and service
- UI: strips with faders, mute, routing buttons, peak/RMS meters; bus section; cable
  management (add/remove/rename, kind); device selection per hardware strip and A bus.
  egui/eframe is a reasonable choice for faders and meters; keep a tray icon.
- Meters reach the UI through atomics or lock-free snapshots, never by locking the engine.
- UI-to-engine control through a command channel. In service mode, the same commands travel
  over the existing named pipe; closing the window leaves audio running.
- Persist everything in `config.toml` (strips, buses, routes, gains, devices, cables), keeping
  the current comment-preserving rewrite behaviour.
- RISK, verify early: WASAPI access to the user's physical devices from a LocalSystem service
  in session 0 (default devices and device access may differ from the interactive user's).
  If it is unreliable, fall back to running the engine in a per-user process at logon.

## Phases (each ends with passing tests and a working build)
1. Restructure into the workspace. Cable behaviour byte-identical; all existing tests pass.
2. Engine with virtual-only routing: device kinds, routing matrix, f32 mixing, gain ramps,
   crossfades. Test: unity single route is bit-exact; multi-source sums correctly.
3. Physical outputs (A buses) with ASRC. Test: unit tests that simulate device clocks at
   +/-100 ppm and confirm ring fill stays bounded; a manual run of 1+ hour with no
   under/overruns and a stable ratio.
4. Physical inputs (hardware strips) with ASRC.
5. UI, persistence, cable management in-app.
6. Service mode with the engine, pipe-based control.
7. DSP: per-strip EQ (biquads), gate, compressor; per-bus limiter.

Out of scope for now: VST hosting, ASIO, network audio, recording to file, macros.

## Diagnostics
Extend `AUDIO HEALTH` with per-device ASRC ratio, ring fill, under/overrun counts, and engine
catch-up batch sizes. Keep `stall_test_ms` working across the engine. Add a loopback latency
measurement (play an impulse into an input cable, detect it on an output bus).

## Naming
Don't use "Voicemeeter" or VB-Audio names in the product, UI or device names.