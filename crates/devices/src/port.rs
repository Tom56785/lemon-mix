//! The two ends of a physical device's path into the engine.
//!
//! A capture port's device thread pushes samples at the device's rate; the
//! engine pulls 48 resampled samples per bus frame. A render port works the
//! other way round: the engine pushes 48 samples per bus frame and the device
//! thread pulls what the device asks for, resampled to its rate. The
//! resampler always sits on the side with the fixed demand, and its drift
//! controller watches the ring between the two.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use engine::{FRAME_SAMPLES, Frame, InputPort, OutputPort};

use crate::asrc::{DriftController, Resampler};
use crate::ring::{Ring, Stereo};

/// Where a port's stream stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PortState {
    /// No device chosen.
    Off = 0,
    /// Opening, or waiting to retry after a failure.
    Opening = 1,
    Running = 2,
    /// The device is missing or failed; the port retries.
    Failed = 3,
}

impl PortState {
    fn from_u8(value: u8) -> PortState {
        match value {
            1 => PortState::Opening,
            2 => PortState::Running,
            3 => PortState::Failed,
            _ => PortState::Off,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            PortState::Off => "off",
            PortState::Opening => "opening",
            PortState::Running => "running",
            PortState::Failed => "failed",
        }
    }
}

/// A port's counters, updated without locks on the audio paths.
#[derive(Default)]
pub struct PortStats {
    state: AtomicU8,
    /// The device's sample rate while it streams.
    pub device_rate: AtomicU32,
    /// The resampler ratio and the ring fill in ms, as f64 bits.
    ratio: AtomicU64,
    fill_ms: AtomicU64,
    correction_ppm: AtomicU64,
    /// Times the ring ran dry, and times it overflowed or held far too much.
    pub underruns: AtomicU64,
    pub overruns: AtomicU64,
    /// Times the stream was (re)opened.
    pub opens: AtomicU64,
}

/// A snapshot of a port for the log and the UI.
#[derive(Clone, Debug, PartialEq)]
pub struct PortReading {
    pub state: PortState,
    pub device_rate: u32,
    pub ratio: f64,
    pub correction_ppm: f64,
    pub fill_ms: f64,
    pub underruns: u64,
    pub overruns: u64,
    pub opens: u64,
}

impl PortStats {
    pub fn set_state(&self, state: PortState) {
        self.state.store(state as u8, Ordering::Relaxed);
    }

    pub fn state(&self) -> PortState {
        PortState::from_u8(self.state.load(Ordering::Relaxed))
    }

    fn record(&self, controller: &DriftController, rate: f64) {
        self.ratio.store(controller.ratio().to_bits(), Ordering::Relaxed);
        self.correction_ppm.store(controller.correction_ppm().to_bits(), Ordering::Relaxed);
        self.fill_ms.store((controller.filtered_fill() / rate * 1000.0).to_bits(), Ordering::Relaxed);
    }

    pub fn reading(&self) -> PortReading {
        let f = |a: &AtomicU64| f64::from_bits(a.load(Ordering::Relaxed));
        PortReading {
            state: self.state(),
            device_rate: self.device_rate.load(Ordering::Relaxed),
            ratio: f(&self.ratio),
            correction_ppm: f(&self.correction_ppm),
            fill_ms: f(&self.fill_ms),
            underruns: self.underruns.load(Ordering::Relaxed),
            overruns: self.overruns.load(Ordering::Relaxed),
            opens: self.opens.load(Ordering::Relaxed),
        }
    }
}

/// The ring holds up to this long of audio.
const RING_SECONDS: f64 = 1.0;
/// Fill beyond the target by this much is dropped rather than steered away,
/// after the engine or device paused.
const EXCESS_MS: f64 = 60.0;

/// A physical capture device's port.
pub struct CapturePort {
    ring: Ring,
    side: Mutex<CaptureSide>,
    pub stats: PortStats,
    /// Set while a device stream feeds the ring.
    active: AtomicBool,
    /// Bus frames pulled so far, so the device thread can tell whether the
    /// engine is listening.
    pulls: AtomicU64,
    /// The device thread's view of `pulls`: its last value, and pushes since it changed.
    seen_pulls: AtomicU64,
    idle_pushes: AtomicU32,
    target_ms: f64,
}

struct CaptureSide {
    resampler: Box<Resampler>,
    controller: DriftController,
    rate: f64,
    primed: bool,
    clear: bool,
    out: [Stereo; FRAME_SAMPLES],
}

impl CapturePort {
    /// A port that buffers `target_ms` of audio between device and engine.
    pub fn new(target_ms: f64) -> Arc<CapturePort> {
        let rate = 48_000.0;
        Arc::new(CapturePort {
            ring: Ring::new((192_000.0 * RING_SECONDS) as usize),
            side: Mutex::new(CaptureSide {
                resampler: Box::new(Resampler::new(1.0)),
                controller: DriftController::new(1.0, rate, rate * target_ms / 1000.0),
                rate,
                primed: false,
                clear: true,
                out: [[0.0; 2]; FRAME_SAMPLES],
            }),
            stats: PortStats::default(),
            active: AtomicBool::new(false),
            pulls: AtomicU64::new(0),
            seen_pulls: AtomicU64::new(0),
            idle_pushes: AtomicU32::new(0),
            target_ms,
        })
    }

    /// Device thread: a stream at `rate` is about to push samples. The
    /// resampler is built here, off the engine's path.
    pub fn begin_stream(&self, rate: u32) {
        let ratio = rate as f64 / 48_000.0;
        let resampler = Box::new(Resampler::new(ratio));
        let controller = DriftController::new(ratio, rate as f64, rate as f64 * self.target_ms / 1000.0);
        let old = {
            let mut side = self.side.lock().unwrap_or_else(|p| p.into_inner());
            side.controller = controller;
            side.rate = rate as f64;
            side.primed = false;
            side.clear = true;
            std::mem::replace(&mut side.resampler, resampler)
        };
        drop(old);
        self.stats.device_rate.store(rate, Ordering::Relaxed);
        self.stats.opens.fetch_add(1, Ordering::Relaxed);
        self.active.store(true, Ordering::Release);
    }

    /// Device thread: the stream has ended.
    pub fn end_stream(&self) {
        self.active.store(false, Ordering::Release);
    }

    /// Device thread: queues captured samples.
    pub fn push(&self, samples: &[Stereo]) {
        if self.ring.push(samples) < samples.len() && self.listening() {
            self.stats.overruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Whether the engine pulled recently; the device thread samples this
    /// once per buffer.
    fn listening(&self) -> bool {
        let pulls = self.pulls.load(Ordering::Relaxed);
        let idle = if self.seen_pulls.swap(pulls, Ordering::Relaxed) == pulls {
            self.idle_pushes.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            self.idle_pushes.store(0, Ordering::Relaxed);
            0
        };
        idle < 20
    }
}

impl InputPort for CapturePort {
    fn pull(&self, frame: &mut Frame) {
        self.pulls.fetch_add(1, Ordering::Relaxed);
        let silence = |frame: &mut Frame| *frame = [0.0; engine::FRAME_LEN];
        if !self.active.load(Ordering::Acquire) {
            return silence(frame);
        }
        // Only the device thread's stream start contends, and only briefly.
        let Ok(mut guard) = self.side.try_lock() else {
            return silence(frame);
        };
        let side = &mut *guard;
        if side.clear {
            self.ring.clear();
            side.resampler.reset();
            side.clear = false;
            side.primed = false;
        }
        let target = side.controller.target();
        if !side.primed {
            if (self.ring.len() as f64) < target {
                return silence(frame);
            }
            side.primed = true;
            side.controller.reset(true);
        }
        // Too much queued: the engine or the device paused. Drop the excess.
        let fill = self.ring.len() as f64 + side.resampler.buffered();
        if fill > target + side.rate * EXCESS_MS / 1000.0 {
            discard(&self.ring, (fill - target) as usize);
            self.stats.overruns.fetch_add(1, Ordering::Relaxed);
        }
        let ratio = side.controller.ratio();
        if !side.resampler.process(ratio, &mut side.out, &self.ring) {
            self.stats.underruns.fetch_add(1, Ordering::Relaxed);
            side.primed = false;
            side.resampler.reset();
            return silence(frame);
        }
        let fill = self.ring.len() as f64 + side.resampler.buffered();
        side.controller.update(fill, 0.001);
        self.stats.record(&side.controller, side.rate);
        for (pair, sample) in frame.as_chunks_mut::<2>().0.iter_mut().zip(side.out.iter()) {
            pair[0] = sample[0];
            pair[1] = sample[1];
        }
    }
}

fn discard(ring: &Ring, mut count: usize) {
    let mut scratch = [[0.0f32; 2]; 256];
    while count > 0 {
        let n = ring.pop(&mut scratch[..count.min(256)]);
        if n == 0 {
            break;
        }
        count -= n;
    }
}

/// A physical render device's port.
pub struct RenderPort {
    ring: Ring,
    pub stats: PortStats,
    active: AtomicBool,
    target_ms: f64,
}

impl RenderPort {
    pub fn new(target_ms: f64) -> Arc<RenderPort> {
        Arc::new(RenderPort {
            ring: Ring::new((48_000.0 * RING_SECONDS) as usize),
            stats: PortStats::default(),
            active: AtomicBool::new(false),
            target_ms,
        })
    }

    /// Device thread: a stream at `rate` starts. The returned side does the
    /// resampling on the device thread.
    pub fn begin_stream(&self, rate: u32) -> RenderSide {
        let ratio = 48_000.0 / rate as f64;
        self.ring.clear();
        self.stats.device_rate.store(rate, Ordering::Relaxed);
        self.stats.opens.fetch_add(1, Ordering::Relaxed);
        self.active.store(true, Ordering::Release);
        RenderSide {
            resampler: Resampler::new(ratio),
            controller: DriftController::new(ratio, 48_000.0, 48.0 * self.target_ms),
            device_rate: rate as f64,
            primed: false,
        }
    }

    pub fn end_stream(&self) {
        self.active.store(false, Ordering::Release);
    }
}

impl OutputPort for RenderPort {
    fn push(&self, frame: &Frame) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let mut samples = [[0.0f32; 2]; FRAME_SAMPLES];
        for (sample, pair) in samples.iter_mut().zip(frame.as_chunks::<2>().0.iter()) {
            *sample = [pair[0], pair[1]];
        }
        if self.ring.push(&samples) < FRAME_SAMPLES {
            self.stats.overruns.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The device thread's half of a render port.
pub struct RenderSide {
    resampler: Resampler,
    controller: DriftController,
    device_rate: f64,
    primed: bool,
}

impl RenderSide {
    /// Fills `out` with the next samples for the device; silence until the
    /// ring has its target fill, and after it runs dry until it has again.
    pub fn render(&mut self, port: &RenderPort, out: &mut [Stereo]) {
        let target = self.controller.target();
        if !self.primed {
            // The stream starts with the device's buffer full of silence, so
            // the first requests are period-sized: prime to the target.
            if (port.ring.len() as f64) < target {
                out.fill([0.0; 2]);
                return;
            }
            self.primed = true;
            self.controller.reset(true);
        }
        let fill = port.ring.len() as f64 + self.resampler.buffered();
        if fill > target + 48.0 * EXCESS_MS {
            discard(&port.ring, (fill - target) as usize);
            port.stats.overruns.fetch_add(1, Ordering::Relaxed);
        }
        if !self.resampler.process(self.controller.ratio(), out, &port.ring) {
            port.stats.underruns.fetch_add(1, Ordering::Relaxed);
            self.primed = false;
            self.resampler.reset();
            return;
        }
        let fill = port.ring.len() as f64 + self.resampler.buffered();
        self.controller.update(fill, out.len() as f64 / self.device_rate);
        port.stats.record(&self.controller, 48_000.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A render port fed by the engine at the bus clock and drained by a
    /// device 100 ppm slow in 10 ms buffers holds its fill without a glitch.
    #[test]
    fn render_port_tracks_a_slow_device() {
        let port = RenderPort::new(20.0);
        let rate = 44_100u32;
        let mut side = port.begin_stream(rate);
        let device = rate as f64 * (1.0 - 100e-6);
        let frame: Frame = [0.25; engine::FRAME_LEN];
        let mut buffer = vec![[0.0; 2]; 441];
        let mut consumed = 0f64;
        let mut glitches_after_start = 0;
        let mut corrections = Vec::new();
        for ms in 1..=300_000u64 {
            port.push(&frame);
            let due = device * ms as f64 / 1000.0;
            while consumed + 441.0 <= due {
                let before = port.stats.underruns.load(Ordering::Relaxed);
                side.render(&port, &mut buffer);
                if ms > 1000 && port.stats.underruns.load(Ordering::Relaxed) > before {
                    glitches_after_start += 1;
                }
                consumed += 441.0;
                if ms > 150_000 && ms % 100 == 0 {
                    corrections.push(port.stats.reading().correction_ppm);
                }
            }
        }
        let reading = port.stats.reading();
        assert_eq!(glitches_after_start, 0);
        assert_eq!(reading.overruns, 0);
        assert!((reading.fill_ms - 20.0).abs() < 6.0, "{reading:?}");
        let mean = corrections.iter().sum::<f64>() / corrections.len() as f64;
        assert!((mean - 100.0).abs() < 15.0, "mean correction {mean:.1} ppm, {reading:?}");
        assert!(buffer.iter().all(|s| (s[0] - 0.25).abs() < 1e-3), "steady audio passes through");
    }

    /// A capture port at 48 kHz, 100 ppm fast, delivering 10 ms buffers.
    #[test]
    fn capture_port_tracks_a_fast_device() {
        let port = CapturePort::new(20.0);
        port.begin_stream(48_000);
        let device = 48_000.0 * (1.0 + 100e-6);
        let chunk = vec![[0.5f32, -0.5]; 480];
        let mut produced = 0f64;
        let mut frame = [0.0; engine::FRAME_LEN];
        let mut underruns_after_start = 0;
        let mut corrections = Vec::new();
        for ms in 1..=300_000u64 {
            let due = device * ms as f64 / 1000.0;
            while produced + 480.0 <= due {
                port.push(&chunk);
                produced += 480.0;
            }
            let before = port.stats.underruns.load(Ordering::Relaxed);
            port.pull(&mut frame);
            if ms > 150_000 && ms % 100 == 0 {
                corrections.push(port.stats.reading().correction_ppm);
            }
            if ms > 1000 && port.stats.underruns.load(Ordering::Relaxed) > before {
                underruns_after_start += 1;
            }
        }
        let reading = port.stats.reading();
        assert_eq!(underruns_after_start, 0);
        assert!((reading.fill_ms - 20.0).abs() < 6.0, "{reading:?}");
        let mean = corrections.iter().sum::<f64>() / corrections.len() as f64;
        assert!((mean - 100.0).abs() < 15.0, "mean correction {mean:.1} ppm, {reading:?}");
        assert!(frame.as_chunks::<2>().0.iter().all(|p| (p[0] - 0.5).abs() < 1e-3 && (p[1] + 0.5).abs() < 1e-3));
    }

    #[test]
    fn idle_ports_are_silent_and_drop_stale_audio() {
        let port = CapturePort::new(20.0);
        let mut frame = [1.0; engine::FRAME_LEN];
        port.pull(&mut frame);
        assert!(frame.iter().all(|&s| s == 0.0), "no stream: silence");
        port.begin_stream(48_000);
        port.pull(&mut frame); // clears the ring
        port.push(&vec![[0.1; 2]; 48_000 / 4]); // 250 ms, far above the target
        port.pull(&mut frame);
        assert_eq!(port.stats.reading().overruns, 1, "the excess was dropped");
        let render = RenderPort::new(20.0);
        render.push(&[0.5; engine::FRAME_LEN]);
        assert_eq!(render.stats.reading().overruns, 0, "pushes without a stream are discarded quietly");
    }
}
