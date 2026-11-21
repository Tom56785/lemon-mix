//! Asynchronous sample-rate conversion between a physical device's clock and
//! the bus frame clock.
//!
//! Every physical device runs on its own crystal, tens of ppm away from the
//! bus clock (50 ppm is one 48-sample frame every 20 seconds). Between each
//! device buffer and the engine sits a ring; a PI controller watches the
//! ring's fill and steers a resampler's ratio around its nominal value, so the
//! fill stays put and the drift is absorbed smoothly instead of by dropping or
//! repeating audio.

use crate::ring::{Ring, Stereo};

/// Filter taps per output sample: half before and half after its position.
const TAPS: usize = 32;
/// Filter phases per input sample; positions in between are interpolated.
const PHASES: usize = 256;

/// A polyphase windowed-sinc resampler whose ratio may change on every call.
/// It pulls input samples from a ring as it needs them.
pub struct Resampler {
    /// `PHASES + 1` rows of `TAPS` coefficients: row p is the filter for an
    /// output `p / PHASES` of an input sample past the newest tap's centre.
    table: Vec<f32>,
    /// Input history: `TAPS` samples, the oldest first, plus room to refill.
    history: Vec<Stereo>,
    /// Valid samples in `history`.
    len: usize,
    /// The position of the next output, in input samples from history[0].
    position: f64,
}

impl Resampler {
    /// A resampler for input-to-output rate ratios near `nominal_ratio`
    /// (input samples consumed per output sample). Its low-pass filter sits
    /// below the lower of the two Nyquist frequencies.
    pub fn new(nominal_ratio: f64) -> Resampler {
        let cutoff = 0.5 * (1.0 / nominal_ratio).min(1.0) * 0.91;
        let beta = 8.6;
        let half = TAPS as f64 / 2.0;
        let mut table = Vec::with_capacity((PHASES + 1) * TAPS);
        for phase in 0..=PHASES {
            let fraction = phase as f64 / PHASES as f64;
            let mut row: Vec<f64> = (0..TAPS)
                .map(|tap| {
                    // The tap's distance from the output position.
                    let t = tap as f64 - (half - 1.0) - fraction;
                    let sinc = if t.abs() < 1e-12 {
                        2.0 * cutoff
                    } else {
                        (2.0 * std::f64::consts::PI * cutoff * t).sin() / (std::f64::consts::PI * t)
                    };
                    let x = t / half;
                    let window =
                        if x.abs() >= 1.0 { 0.0 } else { bessel_i0(beta * (1.0 - x * x).sqrt()) / bessel_i0(beta) };
                    sinc * window
                })
                .collect();
            // Unity gain at DC for every phase.
            let sum: f64 = row.iter().sum();
            for c in &mut row {
                *c /= sum;
            }
            table.extend(row.into_iter().map(|c| c as f32));
        }
        Resampler { table, history: vec![[0.0; 2]; TAPS * 4 + 8192], len: TAPS, position: 0.0 }
    }

    /// Clears the history, as for the start of a stream.
    pub fn reset(&mut self) {
        self.history[..TAPS].fill([0.0; 2]);
        self.len = TAPS;
        self.position = 0.0;
    }

    /// Input samples taken from the ring but not yet consumed: they count
    /// towards the fill the controller steers.
    pub fn buffered(&self) -> f64 {
        (self.len as f64 - (TAPS as f64 / 2.0) - self.position).max(0.0)
    }

    /// Input samples needed to produce `outputs` samples at `ratio`, beyond
    /// those already buffered.
    pub fn needed(&self, outputs: usize, ratio: f64) -> usize {
        let last = self.position + ratio * outputs as f64;
        let end = last.floor() as usize + TAPS + 1;
        end.saturating_sub(self.len)
    }

    /// Fills `out` at `ratio`, taking input from `ring`. Returns false, with
    /// `out` silent, if the ring could not supply enough input.
    pub fn process(&mut self, ratio: f64, out: &mut [Stereo], ring: &Ring) -> bool {
        let needed = self.needed(out.len(), ratio);
        if needed > ring.len() || self.len + needed > self.history.len() {
            out.fill([0.0; 2]);
            return false;
        }
        let got = ring.pop(&mut self.history[self.len..self.len + needed]);
        self.len += got;
        for sample in out.iter_mut() {
            let base = self.position.floor();
            let index = base as usize;
            let phase_position = (self.position - base) * PHASES as f64;
            let phase = phase_position.floor() as usize;
            let blend = (phase_position - phase as f64) as f32;
            let row_a = &self.table[phase * TAPS..(phase + 1) * TAPS];
            let row_b = &self.table[(phase + 1) * TAPS..(phase + 2) * TAPS];
            let window = &self.history[index..index + TAPS];
            let mut acc = [0f32; 2];
            for ((x, a), b) in window.iter().zip(row_a).zip(row_b) {
                let c = a + (b - a) * blend;
                acc[0] += x[0] * c;
                acc[1] += x[1] * c;
            }
            *sample = acc;
            self.position += ratio;
        }
        // Drop the samples no future output will reach.
        let consumed = (self.position.floor() as usize).min(self.len.saturating_sub(TAPS));
        if consumed > 0 {
            self.history.copy_within(consumed..self.len, 0);
            self.len -= consumed;
            self.position -= consumed as f64;
        }
        true
    }
}

/// The zeroth-order modified Bessel function, for the Kaiser window.
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..50 {
        term *= (half / k as f64).powi(2);
        sum += term;
        if term < 1e-12 * sum {
            break;
        }
    }
    sum
}

/// Steers a resampler's ratio to hold a ring's fill at its target.
///
/// With the fill error `x` in seconds of audio and the relative ratio
/// correction `d`, the fill moves as `x' = drift - d`. The controller sets
/// `d = kp x + ki ∫x`, a critically damped loop with a natural frequency of
/// `OMEGA` rad/s: slow enough that the correction is inaudible and the
/// sawtooth of bursty reads and writes averages out, fast enough to absorb a
/// change of drift within a minute.
pub struct DriftController {
    nominal: f64,
    /// Input samples per second, to turn the fill into seconds.
    input_rate: f64,
    target: f64,
    filtered: Option<f64>,
    integral: f64,
    correction: f64,
}

const OMEGA: f64 = 0.05;
const KP: f64 = 2.0 * OMEGA;
const KI: f64 = OMEGA * OMEGA;
/// The fill is smoothed over this long before the controller sees it.
const FILTER_SECONDS: f64 = 2.0;
/// The largest correction, in parts per million.
pub const MAX_CORRECTION_PPM: f64 = 3000.0;

impl DriftController {
    /// A controller for a resampler of nominal ratio `nominal`, whose input
    /// runs at `input_rate`, holding the fill at `target` input samples.
    pub fn new(nominal: f64, input_rate: f64, target: f64) -> DriftController {
        DriftController { nominal, input_rate, target, filtered: None, integral: 0.0, correction: 0.0 }
    }

    pub fn target(&self) -> f64 {
        self.target
    }

    /// Restarts from the nominal ratio, keeping what was learnt of the drift
    /// when `keep_drift` is set.
    pub fn reset(&mut self, keep_drift: bool) {
        self.filtered = None;
        if !keep_drift {
            self.integral = 0.0;
            self.correction = 0.0;
        }
    }

    /// Takes a fill measurement made `dt` seconds after the previous one and
    /// returns the ratio to use until the next.
    pub fn update(&mut self, fill: f64, dt: f64) -> f64 {
        let k = (dt / FILTER_SECONDS).min(1.0);
        let filtered = match self.filtered {
            Some(previous) => previous + (fill - previous) * k,
            None => fill,
        };
        self.filtered = Some(filtered);
        let error = (filtered - self.target) / self.input_rate;
        let limit = MAX_CORRECTION_PPM * 1e-6;
        let integral = self.integral + error * dt;
        let correction = KP * error + KI * integral;
        // Anti-windup: stop integrating while the correction is saturated.
        if correction.abs() <= limit {
            self.integral = integral;
        }
        self.correction = correction.clamp(-limit, limit);
        self.ratio()
    }

    pub fn ratio(&self) -> f64 {
        self.nominal * (1.0 + self.correction)
    }

    /// The current correction, in parts per million.
    pub fn correction_ppm(&self) -> f64 {
        self.correction * 1e6
    }

    /// The smoothed fill, in input samples.
    pub fn filtered_fill(&self) -> f64 {
        self.filtered.unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: f64, frequency: f64, start: usize, n: usize) -> Vec<Stereo> {
        (start..start + n)
            .map(|i| {
                let v = (2.0 * std::f64::consts::PI * frequency * i as f64 / rate).sin() as f32 * 0.5;
                [v, -v]
            })
            .collect()
    }

    /// 44.1 kHz in, 48 kHz out: a 1 kHz tone comes out as a clean 1 kHz tone.
    #[test]
    fn converts_a_tone_cleanly() {
        let (in_rate, out_rate) = (44_100.0, 48_000.0);
        let ratio = in_rate / out_rate;
        let mut resampler = Resampler::new(ratio);
        let ring = Ring::new(1 << 16);
        ring.push(&tone(in_rate, 1000.0, 0, 40_000));
        let mut out = vec![[0.0; 2]; 48 * 500];
        for chunk in out.chunks_mut(48) {
            assert!(resampler.process(ratio, chunk, &ring));
        }
        // Compare against the ideal tone, past the filter's start-up and
        // allowing for its constant delay of half the taps.
        let delay = (TAPS as f64 / 2.0 + 1.0) / in_rate;
        let mut error = 0f64;
        for (i, sample) in out.iter().enumerate().skip(1000) {
            let t = i as f64 / out_rate - delay;
            let ideal = (2.0 * std::f64::consts::PI * 1000.0 * t).sin() * 0.5;
            error = error.max((sample[0] as f64 - ideal).abs());
            assert_eq!(sample[0], -sample[1]);
        }
        assert!(error < 0.001, "largest error {error} (> -60 dB)");
    }

    #[test]
    fn reports_underflow_without_consuming() {
        let mut resampler = Resampler::new(1.0);
        let ring = Ring::new(1024);
        ring.push(&[[0.5; 2]; 40]);
        let mut out = [[1.0; 2]; 48];
        assert!(!resampler.process(1.0, &mut out, &ring));
        assert_eq!(out, [[0.0; 2]; 48]);
        assert_eq!(ring.len(), 40, "nothing taken");
        ring.push(&[[0.5; 2]; 100]);
        assert!(resampler.process(1.0, &mut out, &ring));
        assert!(resampler.buffered() > 0.0);
    }

    /// One simulated device: a producer at `produce_rate` pushing in chunks
    /// of `chunk`, the engine consuming 48 samples per ms at the bus clock
    /// in bursts of `burst` frames, with the controller steering. Returns the
    /// fill (ms) over time and the final correction (ppm).
    fn simulate(device_ppm: f64, nominal_in: f64, chunk: usize, burst: usize, seconds: f64) -> (Vec<f64>, f64) {
        let ratio0 = nominal_in / 48_000.0;
        let actual_in = nominal_in * (1.0 + device_ppm * 1e-6);
        let target = nominal_in * 0.02; // 20 ms
        let mut controller = DriftController::new(ratio0, nominal_in, target);
        let mut resampler = Resampler::new(ratio0);
        let ring = Ring::new(nominal_in as usize);
        // Prime to the target, as the ports do.
        ring.push(&vec![[0.0; 2]; target as usize]);
        let mut produced = 0f64;
        let mut ratio = ratio0;
        let mut fills = Vec::new();
        let mut corrections = Vec::new();
        let mut out = vec![[0.0; 2]; 48 * burst];
        let ms_total = (seconds * 1000.0) as usize;
        let samples = vec![[0.1; 2]; chunk];
        for ms in 1..=ms_total {
            // The device delivers whole chunks as its clock reaches them.
            let due = actual_in * ms as f64 / 1000.0;
            while produced + chunk as f64 <= due {
                ring.push(&samples);
                produced += chunk as f64;
            }
            // The engine runs every `burst` ms.
            if ms % burst == 0 {
                assert!(resampler.process(ratio, &mut out, &ring), "underrun at {ms} ms");
                let fill = ring.len() as f64 + resampler.buffered();
                ratio = controller.update(fill, burst as f64 / 1000.0);
                fills.push(fill / nominal_in * 1000.0);
                corrections.push(controller.correction_ppm());
            }
        }
        // The mean correction over the second half: the measured fill steps
        // as device chunks slip against the 1 ms engine ticks, so the
        // correction ripples around the drift.
        let settled = &corrections[corrections.len() / 2..];
        (fills, settled.iter().sum::<f64>() / settled.len() as f64)
    }

    /// At +/-100 ppm, with 10 ms device chunks and up to 15 ms engine bursts,
    /// the fill stays bounded and the ratio settles on the drift.
    #[test]
    fn fill_stays_bounded_under_drift() {
        for (ppm, rate, burst) in
            [(100.0, 48_000.0, 1), (-100.0, 48_000.0, 15), (100.0, 44_100.0, 4), (-100.0, 44_100.0, 1)]
        {
            let (fills, correction) = simulate(ppm, rate, (rate / 100.0) as usize, burst, 600.0);
            let settled = &fills[fills.len() / 2..];
            let (low, high) = settled.iter().fold((f64::MAX, f64::MIN), |(l, h), &f| (l.min(f), h.max(f)));
            assert!(low > 5.0 && high < 40.0, "{ppm} ppm at {rate}: fill {low:.1}-{high:.1} ms");
            let mean = settled.iter().sum::<f64>() / settled.len() as f64;
            assert!((mean - 20.0).abs() < 8.0, "{ppm} ppm at {rate}: mean fill {mean:.1} ms");
            // The resampler must consume faster when the device runs fast.
            assert!((correction - ppm).abs() < 15.0, "{ppm} ppm at {rate}: correction {correction:.1} ppm");
        }
    }

    #[test]
    fn controller_saturates_and_recovers() {
        let mut controller = DriftController::new(1.0, 48_000.0, 960.0);
        for _ in 0..1000 {
            controller.update(48_000.0, 0.01);
        }
        assert!((controller.correction_ppm() - MAX_CORRECTION_PPM).abs() < 1e-6);
        // Released into a real plant with no drift, the fill returns to the
        // target and the correction to zero.
        let mut fill = 48_000.0f64;
        for _ in 0..60_000 {
            let ratio = controller.update(fill, 0.01);
            fill -= (ratio - 1.0) * 480.0;
        }
        assert!((fill - 960.0).abs() < 5.0, "fill {fill}");
        assert!(controller.correction_ppm().abs() < 5.0, "{}", controller.correction_ppm());
    }
}
