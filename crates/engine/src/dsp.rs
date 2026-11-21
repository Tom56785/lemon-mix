//! Strip and bus processing: a three-band EQ, a noise gate, a compressor and
//! a limiter. Each is bypassed entirely when off, so it cannot change audio
//! that should pass untouched. None allocates.

use crate::{FRAME_SAMPLES, Frame, SAMPLE_RATE, db_to_gain};

/// A biquad filter in transposed direct form II, with coefficients from the
/// RBJ Audio EQ Cookbook. f64 keeps the low shelf stable and quiet.
#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    state: [[f64; 2]; 2],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape {
    LowShelf,
    Peaking,
    HighShelf,
}

impl Biquad {
    pub fn new(shape: Shape, frequency: f64, q: f64, gain_db: f64) -> Biquad {
        let mut filter = Biquad { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0, state: [[0.0; 2]; 2] };
        filter.design(shape, frequency, q, gain_db);
        filter
    }

    /// Sets the coefficients and keeps the filter state.
    pub fn design(&mut self, shape: Shape, frequency: f64, q: f64, gain_db: f64) {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * frequency / SAMPLE_RATE as f64;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let (b0, b1, b2, a0, a1, a2) = match shape {
            Shape::Peaking => {
                (1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a)
            }
            Shape::LowShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cos + s),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                    a * ((a + 1.0) - (a - 1.0) * cos - s),
                    (a + 1.0) + (a - 1.0) * cos + s,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                    (a + 1.0) + (a - 1.0) * cos - s,
                )
            }
            Shape::HighShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cos + s),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                    a * ((a + 1.0) + (a - 1.0) * cos - s),
                    (a + 1.0) - (a - 1.0) * cos + s,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos),
                    (a + 1.0) - (a - 1.0) * cos - s,
                )
            }
        };
        self.b0 = b0 / a0;
        self.b1 = b1 / a0;
        self.b2 = b2 / a0;
        self.a1 = a1 / a0;
        self.a2 = a2 / a0;
    }

    pub fn reset(&mut self) {
        self.state = [[0.0; 2]; 2];
    }

    pub fn process(&mut self, frame: &mut Frame) {
        for pair in frame.as_chunks_mut::<2>().0 {
            for (channel, sample) in pair.iter_mut().enumerate() {
                let x = *sample as f64;
                let [z1, z2] = &mut self.state[channel];
                let y = self.b0 * x + *z1;
                *z1 = self.b1 * x - self.a1 * y + *z2;
                *z2 = self.b2 * x - self.a2 * y;
                *sample = y as f32;
            }
        }
    }

    /// The filter's gain at `frequency`, for tests.
    #[cfg(test)]
    fn response(&self, frequency: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * frequency / SAMPLE_RATE as f64;
        let (z1, z2) = (num_complex(-w), num_complex(-2.0 * w));
        let num = add(add((self.b0, 0.0), scale(z1, self.b1)), scale(z2, self.b2));
        let den = add(add((1.0, 0.0), scale(z1, self.a1)), scale(z2, self.a2));
        (num.0.hypot(num.1)) / (den.0.hypot(den.1))
    }
}

#[cfg(test)]
fn num_complex(phase: f64) -> (f64, f64) {
    (phase.cos(), phase.sin())
}
#[cfg(test)]
fn add(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    (a.0 + b.0, a.1 + b.1)
}
#[cfg(test)]
fn scale(a: (f64, f64), k: f64) -> (f64, f64) {
    (a.0 * k, a.1 * k)
}

/// The EQ bands: a bass shelf, a mid peak and a treble shelf.
pub const EQ_BANDS: [(Shape, f64, f64); 3] =
    [(Shape::LowShelf, 120.0, 0.707), (Shape::Peaking, 1000.0, 0.8), (Shape::HighShelf, 8000.0, 0.707)];

/// A three-band EQ. Bands at 0 dB are skipped, and the whole EQ is bypassed
/// when every band is at 0 dB.
#[derive(Clone, Debug)]
pub struct Eq3 {
    gains: [f32; 3],
    bands: [Biquad; 3],
}

impl Default for Eq3 {
    fn default() -> Eq3 {
        Eq3 { gains: [0.0; 3], bands: EQ_BANDS.map(|(shape, f, q)| Biquad::new(shape, f, q, 0.0)) }
    }
}

impl Eq3 {
    pub fn set(&mut self, gains: [f32; 3]) {
        for (band, (gain, (shape, frequency, q))) in self.bands.iter_mut().zip(gains.iter().zip(EQ_BANDS)) {
            band.design(shape, frequency, q, *gain as f64);
        }
        for (band, (old, new)) in self.bands.iter_mut().zip(self.gains.iter().zip(gains)) {
            if *old == 0.0 && new != 0.0 {
                band.reset(); // starts from rest rather than from stale state
            }
        }
        self.gains = gains;
    }

    pub fn active(&self) -> bool {
        self.gains.iter().any(|&g| g != 0.0)
    }

    pub fn process(&mut self, frame: &mut Frame) {
        for (band, gain) in self.bands.iter_mut().zip(self.gains) {
            if gain != 0.0 {
                band.process(frame);
            }
        }
    }
}

/// A one-pole smoothing coefficient for a time constant in milliseconds, at
/// one step per sample.
fn coefficient(ms: f32) -> f32 {
    1.0 - (-1.0 / (ms * 0.001 * SAMPLE_RATE as f32)).exp()
}

/// A noise gate: below its threshold the strip is attenuated by up to 60 dB.
/// Its amount (0-10) sets the threshold from -70 dBFS to -20 dBFS.
#[derive(Clone, Debug)]
pub struct Gate {
    amount: f32,
    threshold: f32,
    envelope: f32,
    gain: f32,
    hold: u32,
}

const GATE_FLOOR: f32 = 0.001; // -60 dB
const GATE_HOLD_SAMPLES: u32 = SAMPLE_RATE / 20; // 50 ms

impl Default for Gate {
    fn default() -> Gate {
        Gate { amount: 0.0, threshold: 0.0, envelope: 0.0, gain: 1.0, hold: 0 }
    }
}

impl Gate {
    pub fn set(&mut self, amount: f32) {
        if self.amount == 0.0 && amount > 0.0 {
            self.gain = 1.0;
            self.envelope = 0.0;
        }
        self.amount = amount;
        self.threshold = db_to_gain(-70.0 + 5.0 * amount);
    }

    pub fn active(&self) -> bool {
        self.amount > 0.0
    }

    /// Processes a frame and returns the gain applied at its end.
    pub fn process(&mut self, frame: &mut Frame) -> f32 {
        let (env_attack, env_release) = (coefficient(0.5), coefficient(50.0));
        let (open, close) = (coefficient(1.0), coefficient(80.0));
        for pair in frame.as_chunks_mut::<2>().0 {
            let level = pair[0].abs().max(pair[1].abs());
            let k = if level > self.envelope { env_attack } else { env_release };
            self.envelope += (level - self.envelope) * k;
            let target = if self.envelope >= self.threshold {
                self.hold = GATE_HOLD_SAMPLES;
                1.0
            } else if self.hold > 0 {
                self.hold -= 1;
                1.0
            } else {
                GATE_FLOOR
            };
            let k = if target > self.gain { open } else { close };
            self.gain += (target - self.gain) * k;
            pair[0] *= self.gain;
            pair[1] *= self.gain;
        }
        self.gain
    }
}

/// A stereo-linked feed-forward compressor with a soft knee and automatic
/// make-up gain. Its amount (0-10) sets the threshold (0 to -25 dBFS) and the
/// ratio (1:1 to 4:1) together.
#[derive(Clone, Debug)]
pub struct Compressor {
    amount: f32,
    threshold_db: f32,
    ratio: f32,
    makeup: f32,
    envelope_db: f32,
    gain_db: f32,
}

const KNEE_DB: f32 = 6.0;

impl Default for Compressor {
    fn default() -> Compressor {
        Compressor { amount: 0.0, threshold_db: 0.0, ratio: 1.0, makeup: 1.0, envelope_db: -120.0, gain_db: 0.0 }
    }
}

impl Compressor {
    pub fn set(&mut self, amount: f32) {
        self.amount = amount;
        self.threshold_db = -2.5 * amount;
        self.ratio = 1.0 + 0.3 * amount;
        // Half the reduction a full-scale signal would get.
        self.makeup = db_to_gain(-self.threshold_db * (1.0 - 1.0 / self.ratio) * 0.5);
    }

    pub fn active(&self) -> bool {
        self.amount > 0.0
    }

    /// The static curve: how many dB to reduce a level by.
    fn reduction_db(&self, level_db: f32) -> f32 {
        let over = level_db - self.threshold_db;
        let slope = 1.0 - 1.0 / self.ratio;
        if over <= -KNEE_DB / 2.0 {
            0.0
        } else if over >= KNEE_DB / 2.0 {
            over * slope
        } else {
            slope * (over + KNEE_DB / 2.0).powi(2) / (2.0 * KNEE_DB)
        }
    }

    /// Processes a frame and returns the gain reduction at its end, as a
    /// linear factor (make-up excluded).
    pub fn process(&mut self, frame: &mut Frame) -> f32 {
        let (attack, release) = (coefficient(5.0), coefficient(120.0));
        for pair in frame.as_chunks_mut::<2>().0 {
            let level = pair[0].abs().max(pair[1].abs()).max(1e-6);
            let level_db = 20.0 * level.log10();
            let k = if level_db > self.envelope_db { attack } else { release };
            self.envelope_db += (level_db - self.envelope_db) * k;
            self.gain_db = -self.reduction_db(self.envelope_db);
            let gain = db_to_gain(self.gain_db) * self.makeup;
            pair[0] *= gain;
            pair[1] *= gain;
        }
        db_to_gain(self.gain_db)
    }
}

/// A stereo-linked peak limiter with instant attack: no sample leaves above
/// the ceiling. Gain recovers over about 80 ms.
#[derive(Clone, Debug)]
pub struct Limiter {
    ceiling: Option<f32>,
    gain: f32,
}

impl Default for Limiter {
    fn default() -> Limiter {
        Limiter { ceiling: None, gain: 1.0 }
    }
}

impl Limiter {
    pub fn set(&mut self, ceiling_db: Option<f32>) {
        if self.ceiling.is_none() {
            self.gain = 1.0;
        }
        self.ceiling = ceiling_db.map(db_to_gain);
    }

    pub fn active(&self) -> bool {
        self.ceiling.is_some()
    }

    /// Processes a frame and returns the gain at its end.
    pub fn process(&mut self, frame: &mut Frame) -> f32 {
        let Some(ceiling) = self.ceiling else {
            return 1.0;
        };
        let release = coefficient(80.0);
        for pair in frame.as_chunks_mut::<2>().0 {
            let peak = pair[0].abs().max(pair[1].abs());
            let needed = if peak * self.gain > ceiling { ceiling / peak } else { 1.0 };
            if needed < self.gain {
                self.gain = needed;
            } else {
                self.gain += (needed.min(1.0) - self.gain) * release;
                if peak * self.gain > ceiling {
                    self.gain = ceiling / peak;
                }
            }
            pair[0] *= self.gain;
            pair[1] *= self.gain;
        }
        self.gain
    }
}

/// Removes the step where a stream was cut: over one frame, the last sample
/// before the cut decays to nothing while the new audio fades in, so a jump
/// becomes a 1 ms crossfade and audio that stops fades out.
pub fn declick(frame: &mut Frame, last: [f32; 2]) {
    for (i, pair) in frame.as_chunks_mut::<2>().0.iter_mut().enumerate() {
        let w = (i + 1) as f32 / FRAME_SAMPLES as f32;
        for channel in 0..2 {
            pair[channel] = pair[channel] * w + last[channel] * (1.0 - w);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FRAME_LEN;

    fn sine(frequency: f32, amplitude: f32, frames: usize) -> Vec<Frame> {
        (0..frames)
            .map(|f| {
                std::array::from_fn(|i| {
                    let n = (f * FRAME_SAMPLES + i / 2) as f32;
                    amplitude * (2.0 * std::f32::consts::PI * frequency * n / SAMPLE_RATE as f32).sin()
                })
            })
            .collect()
    }

    fn peak(frames: &[Frame]) -> f32 {
        frames.iter().flatten().fold(0f32, |m, s| m.max(s.abs()))
    }

    #[test]
    fn eq_bands_boost_their_frequencies() {
        let mut eq = Eq3::default();
        assert!(!eq.active());
        eq.set([6.0, 0.0, -6.0]);
        assert!(eq.active());
        let db = |filter: &Biquad, f: f64| 20.0 * filter.response(f).log10();
        assert!((db(&eq.bands[0], 20.0) - 6.0).abs() < 0.3, "bass shelf");
        assert!(db(&eq.bands[0], 5000.0).abs() < 0.3, "bass shelf leaves the treble");
        assert!((db(&eq.bands[2], 20000.0) + 6.0).abs() < 0.6, "treble shelf");
        let mid = Biquad::new(Shape::Peaking, 1000.0, 0.8, 9.0);
        assert!((db(&mid, 1000.0) - 9.0).abs() < 0.01);
        // A measured sine agrees with the response.
        let mut frames = sine(50.0, 0.25, 400);
        for frame in &mut frames {
            eq.process(frame);
        }
        let gain = peak(&frames[200..]) / 0.25;
        assert!((20.0 * gain.log10() - 6.0).abs() < 0.5, "measured {gain}");
    }

    #[test]
    fn gate_closes_on_quiet_and_opens_on_loud() {
        let mut gate = Gate::default();
        gate.set(5.0); // -45 dBFS
        let mut quiet = sine(440.0, db_to_gain(-60.0), 1000);
        for frame in &mut quiet {
            gate.process(frame);
        }
        assert!(peak(&quiet[900..]) < 1e-5, "closed: {}", peak(&quiet[900..]));
        let mut loud = sine(440.0, 0.5, 50);
        for frame in &mut loud {
            gate.process(frame);
        }
        assert!(peak(&loud[10..]) > 0.49, "open");
    }

    #[test]
    fn compressor_reduces_loud_signals_more() {
        let mut comp = Compressor::default();
        comp.set(10.0); // -25 dB, 4:1
        let mut loud = sine(440.0, 1.0, 500);
        let mut soft = sine(440.0, db_to_gain(-40.0), 500);
        let mut reduction = 1.0;
        for frame in &mut loud {
            reduction = comp.process(frame);
        }
        let mut comp2 = Compressor::default();
        comp2.set(10.0);
        for frame in &mut soft {
            comp2.process(frame);
        }
        assert!(reduction < db_to_gain(-12.0), "loud signals are reduced: {reduction}");
        let soft_gain = peak(&soft[400..]) / db_to_gain(-40.0);
        assert!((soft_gain - comp2.makeup).abs() < 0.01, "quiet signals get only make-up gain");
    }

    #[test]
    fn limiter_never_exceeds_its_ceiling() {
        let mut limiter = Limiter::default();
        let mut frame: Frame = [0.9; FRAME_LEN];
        assert_eq!(limiter.process(&mut frame), 1.0);
        assert_eq!(frame, [0.9; FRAME_LEN], "off: untouched");
        limiter.set(Some(-6.0));
        let ceiling = db_to_gain(-6.0);
        let mut frames = sine(100.0, 2.0, 100);
        frames.extend(sine(100.0, 0.1, 400));
        for frame in &mut frames {
            limiter.process(frame);
        }
        assert!(peak(&frames[..100]) <= ceiling * 1.0001, "peak {}", peak(&frames[..100]));
        assert!(peak(&frames[400..]) > 0.099, "recovers to unity for quiet audio");
    }

    #[test]
    fn declick_crossfades_from_the_last_sample() {
        let mut frame: Frame = [0.0; FRAME_LEN];
        declick(&mut frame, [1.0, -1.0]);
        assert!((frame[0] - (1.0 - 1.0 / 48.0)).abs() < 1e-6);
        assert_eq!(frame[FRAME_LEN - 2], 0.0, "silence by the end of the frame");
        let steps = frame.as_chunks::<2>().0.iter().map(|p| p[0]).collect::<Vec<_>>();
        assert!(steps.windows(2).all(|w| w[1] <= w[0]), "a monotonic fade");
    }
}
