//! Strip and bus processing: a three-band EQ, a noise gate, a compressor and
//! a limiter. Each is bypassed entirely when off, so it cannot change audio
//! that should pass untouched. None allocates.

use crate::params::{CompOptions, DuckParams, GateOptions};
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

/// A noise gate: below its threshold the strip is attenuated by its range
/// (60 dB unless set otherwise). Its amount (0-10) sets the threshold from
/// -70 dBFS to -20 dBFS; its options set the range and timing.
#[derive(Clone, Debug)]
pub struct Gate {
    amount: f32,
    threshold: f32,
    floor: f32,
    open: f32,
    close: f32,
    hold_samples: u32,
    envelope: f32,
    gain: f32,
    hold: u32,
}

impl Default for Gate {
    fn default() -> Gate {
        let mut gate = Gate {
            amount: 0.0,
            threshold: 0.0,
            floor: 1.0,
            open: 1.0,
            close: 1.0,
            hold_samples: 0,
            envelope: 0.0,
            gain: 1.0,
            hold: 0,
        };
        gate.set(0.0, GateOptions::default());
        gate
    }
}

/// The threshold, in dBFS, a gate amount (0-10) sets.
pub fn gate_threshold_db(amount: f32) -> f32 {
    -70.0 + 5.0 * amount
}

impl Gate {
    pub fn set(&mut self, amount: f32, options: GateOptions) {
        if self.amount == 0.0 && amount > 0.0 {
            self.gain = 1.0;
            self.envelope = 0.0;
        }
        let options = options.sanitized();
        self.amount = amount;
        self.threshold = db_to_gain(gate_threshold_db(amount));
        self.floor = db_to_gain(-options.range_db);
        self.open = coefficient(options.attack_ms);
        self.close = coefficient(options.release_ms);
        self.hold_samples = (options.hold_ms * 0.001 * SAMPLE_RATE as f32).round() as u32;
    }

    pub fn active(&self) -> bool {
        self.amount > 0.0
    }

    /// Processes a frame and returns the gain applied at its end.
    pub fn process(&mut self, frame: &mut Frame) -> f32 {
        let (env_attack, env_release) = (coefficient(0.5), coefficient(50.0));
        let (open, close) = (self.open, self.close);
        for pair in frame.as_chunks_mut::<2>().0 {
            let level = pair[0].abs().max(pair[1].abs());
            let k = if level > self.envelope { env_attack } else { env_release };
            self.envelope += (level - self.envelope) * k;
            let target = if self.envelope >= self.threshold {
                self.hold = self.hold_samples;
                1.0
            } else if self.hold > 0 {
                self.hold -= 1;
                1.0
            } else {
                self.floor
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
/// ratio (1:1 to 4:1) together, unless its options set them by hand; the
/// options also set its timing, knee and make-up.
#[derive(Clone, Debug)]
pub struct Compressor {
    amount: f32,
    threshold_db: f32,
    ratio: f32,
    knee_db: f32,
    makeup: f32,
    attack: f32,
    release: f32,
    envelope_db: f32,
    gain_db: f32,
}

impl Default for Compressor {
    fn default() -> Compressor {
        let mut compressor = Compressor {
            amount: 0.0,
            threshold_db: 0.0,
            ratio: 1.0,
            knee_db: 0.0,
            makeup: 1.0,
            attack: 1.0,
            release: 1.0,
            envelope_db: -120.0,
            gain_db: 0.0,
        };
        compressor.set(0.0, CompOptions::default());
        compressor
    }
}

/// The threshold (dBFS) and ratio a compressor amount (0-10) sets.
pub fn comp_curve(amount: f32) -> (f32, f32) {
    (-2.5 * amount, 1.0 + 0.3 * amount)
}

impl Compressor {
    pub fn set(&mut self, amount: f32, options: CompOptions) {
        let options = options.sanitized();
        self.amount = amount;
        (self.threshold_db, self.ratio) =
            if options.manual { (options.threshold_db, options.ratio) } else { comp_curve(amount) };
        self.knee_db = options.knee_db;
        self.attack = coefficient(options.attack_ms);
        self.release = coefficient(options.release_ms);
        // Automatic make-up: half the reduction a full-scale signal would get.
        let auto = -self.threshold_db * (1.0 - 1.0 / self.ratio) * 0.5;
        self.makeup = db_to_gain(options.makeup_db.unwrap_or(auto));
    }

    pub fn active(&self) -> bool {
        self.amount > 0.0
    }

    /// The static curve: how many dB to reduce a level by.
    fn reduction_db(&self, level_db: f32) -> f32 {
        let over = level_db - self.threshold_db;
        let slope = 1.0 - 1.0 / self.ratio;
        let knee = self.knee_db;
        if over <= -knee / 2.0 {
            0.0
        } else if over >= knee / 2.0 {
            over.max(0.0) * slope
        } else {
            slope * (over + knee / 2.0).powi(2) / (2.0 * knee)
        }
    }

    /// Processes a frame and returns the gain reduction at its end, as a
    /// linear factor (make-up excluded).
    pub fn process(&mut self, frame: &mut Frame) -> f32 {
        let (attack, release) = (self.attack, self.release);
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
/// Ducking's gain, one step per 1 ms frame: down to the lowered level while a
/// source plays and for the hold after, then back up to unity. Each fade is
/// even in dB and takes its set duration from one end to the other; the sends
/// ramp across each frame, so even an instant fade does not click.
#[derive(Clone, Copy, Debug)]
pub struct Ducker {
    /// The current gain, in dB: 0 or below.
    gain_db: f32,
    gain: f32,
    amount_db: f32,
    threshold: f32,
    /// dB per frame of each fade.
    down_step: f32,
    up_step: f32,
    hold_frames: u32,
    held: u32,
}

impl Ducker {
    pub fn new(params: DuckParams) -> Ducker {
        let mut ducker = Ducker {
            gain_db: 0.0,
            gain: 1.0,
            amount_db: 0.0,
            threshold: 1.0,
            down_step: 0.0,
            up_step: 0.0,
            hold_frames: 0,
            held: 0,
        };
        ducker.set(params);
        ducker
    }

    pub fn set(&mut self, params: DuckParams) {
        let params = params.sanitized();
        // The whole amount over the fade's frames; at least one frame.
        let step = |ms: f32| params.amount_db / ms.round().max(1.0);
        self.amount_db = params.amount_db;
        self.threshold = db_to_gain(params.threshold_db);
        self.down_step = step(params.fade_down_ms);
        self.up_step = step(params.fade_up_ms);
        self.hold_frames = params.hold_ms.round() as u32;
    }

    /// The level, as a linear peak, above which a source counts as playing.
    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    /// Advances one frame, `triggered` while a source plays, and returns the
    /// gain for the frame.
    pub fn step(&mut self, triggered: bool) -> f32 {
        let target = if triggered {
            self.held = self.hold_frames;
            -self.amount_db
        } else if self.held > 0 {
            self.held -= 1;
            -self.amount_db
        } else {
            0.0
        };
        let before = self.gain_db;
        // With no amount there are no steps: straight to the target (unity).
        let pace = |step: f32| if step > 0.0 { step } else { f32::INFINITY };
        self.gain_db = if target < self.gain_db {
            (self.gain_db - pace(self.down_step)).max(target)
        } else {
            // Up, or to a new, smaller amount, at the up fade's pace.
            (self.gain_db + pace(self.up_step)).min(target)
        };
        if self.gain_db != before {
            self.gain = if self.gain_db == 0.0 { 1.0 } else { db_to_gain(self.gain_db) };
        }
        self.gain
    }
}

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

    /// Runs `frames` frames of a steady level through `f` and returns the
    /// level of the last sample.
    fn steady(level: f32, frames: usize, mut f: impl FnMut(&mut Frame)) -> f32 {
        let mut last = 0.0;
        for _ in 0..frames {
            let mut frame = [level; crate::FRAME_LEN];
            f(&mut frame);
            last = frame[crate::FRAME_LEN - 1];
        }
        last
    }

    #[test]
    fn gate_options_set_its_range_and_hold() {
        // -50 dBFS under a -45 dBFS threshold: closed down by its range.
        let quiet = db_to_gain(-50.0);
        let mut gate = Gate::default();
        gate.set(5.0, GateOptions { range_db: 20.0, ..Default::default() });
        let out = steady(quiet, 2000, |f| {
            gate.process(f);
        });
        assert!((out / quiet - db_to_gain(-20.0)).abs() < 1e-3, "closed by 20 dB: {}", out / quiet);

        // After a loud burst, a long hold keeps it open while a short one
        // lets it close.
        let closed_after = |hold_ms: f32| {
            let mut gate = Gate::default();
            gate.set(5.0, GateOptions { hold_ms, release_ms: 5.0, ..Default::default() });
            steady(0.5, 50, |f| {
                gate.process(f);
            });
            // The level detector takes about 225 ms to fall to the threshold.
            steady(quiet, 600, |f| {
                gate.process(f);
            }) / quiet
        };
        assert!(closed_after(0.0) < 0.01, "no hold: closed");
        assert!(closed_after(1000.0) > 0.99, "a 1 s hold: still open");
    }

    #[test]
    fn compressor_options_set_its_curve_makeup_and_attack() {
        // A manual hard-knee curve, -20 dBFS at 4:1, no make-up: -8 dBFS in is
        // 12 dB over, reduced by 9 dB to -17 dBFS.
        let manual = CompOptions {
            manual: true,
            threshold_db: -20.0,
            ratio: 4.0,
            knee_db: 0.0,
            makeup_db: Some(0.0),
            ..Default::default()
        };
        let mut comp = Compressor::default();
        comp.set(1.0, manual);
        let out = steady(db_to_gain(-8.0), 3000, |f| {
            comp.process(f);
        });
        assert!((20.0 * out.log10() - -17.0).abs() < 0.1, "{} dBFS", 20.0 * out.log10());

        // Below the threshold only the make-up applies.
        let mut comp = Compressor::default();
        comp.set(1.0, CompOptions { makeup_db: Some(6.0), ..manual });
        let out = steady(db_to_gain(-40.0), 3000, |f| {
            comp.process(f);
        });
        assert!((20.0 * out.log10() - -34.0).abs() < 0.1, "{} dBFS", 20.0 * out.log10());

        // A faster attack reaches the reduction sooner.
        let after_10_ms = |attack_ms: f32| {
            let mut comp = Compressor::default();
            comp.set(1.0, CompOptions { attack_ms, ..manual });
            steady(db_to_gain(-8.0), 10, |f| {
                comp.process(f);
            })
        };
        assert!(after_10_ms(1.0) < after_10_ms(100.0) * 0.8, "{} vs {}", after_10_ms(1.0), after_10_ms(100.0));

        // Not manual: the knob's curve, whatever the options say.
        let mut knob = Compressor::default();
        knob.set(10.0, CompOptions { threshold_db: -60.0, ratio: 20.0, ..Default::default() });
        assert_eq!((knob.threshold_db, knob.ratio), comp_curve(10.0));
    }

    /// Down to the lowered level over the fade-down time, held, then back
    /// up over the fade-up time, each an even fade in dB.
    #[test]
    fn ducking_fades_for_its_set_times() {
        let params =
            DuckParams { amount_db: 20.0, threshold_db: -30.0, fade_down_ms: 40.0, hold_ms: 50.0, fade_up_ms: 100.0 };
        let mut ducker = Ducker::new(params);
        let db = |gain: f32| 20.0 * gain.log10();
        assert!((ducker.threshold() - db_to_gain(-30.0)).abs() < 1e-6);
        assert_eq!(ducker.step(false), 1.0, "untriggered, nothing happens");
        let down: Vec<f32> = (0..60).map(|_| ducker.step(true)).collect();
        assert!((db(down[19]) - -10.0).abs() < 1e-3, "half way after 20 of 40 ms: {} dB", db(down[19]));
        assert_eq!(down[39], db_to_gain(-20.0), "down after exactly 40 ms");
        assert!(down[38] > down[39], "not before");
        assert!(down[40..].iter().all(|&g| g == down[39]), "and stays there");
        assert!(down[..40].windows(2).all(|w| (db(w[0]) - db(w[1]) - 0.5).abs() < 1e-3), "0.5 dB a frame");
        // Quiet: held for 50 ms, then up over 100 ms.
        assert!((0..50).all(|_| ducker.step(false) == db_to_gain(-20.0)), "held");
        let up: Vec<f32> = (0..100).map(|_| ducker.step(false)).collect();
        assert!((db(up[49]) - -10.0).abs() < 1e-3, "half way up after 50 of 100 ms: {} dB", db(up[49]));
        assert_eq!(up[99], 1.0, "back at unity after exactly 100 ms");
        assert!(up[98] < 1.0);
    }

    #[test]
    fn instant_fades_and_amount_changes() {
        let mut ducker =
            Ducker::new(DuckParams { fade_down_ms: 0.0, fade_up_ms: 0.0, hold_ms: 0.0, ..Default::default() });
        assert_eq!(ducker.step(true), db_to_gain(-12.0), "down in one frame");
        assert_eq!(ducker.step(false), 1.0, "and up in one");
        // Lowered, then the amount goes to nothing: back to unity at once.
        let mut ducker = Ducker::new(DuckParams::default());
        (0..100).for_each(|_| {
            ducker.step(true);
        });
        ducker.set(DuckParams { amount_db: 0.0, ..Default::default() });
        assert_eq!(ducker.step(true), 1.0);
    }

    #[test]
    fn ducking_amount_zero_changes_nothing() {
        let mut ducker = Ducker::new(DuckParams { amount_db: 0.0, ..Default::default() });
        assert!((0..100).all(|_| ducker.step(true) == 1.0));
    }
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
        gate.set(5.0, GateOptions::default()); // -45 dBFS
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
        comp.set(10.0, CompOptions::default()); // -25 dB, 4:1
        let mut loud = sine(440.0, 1.0, 500);
        let mut soft = sine(440.0, db_to_gain(-40.0), 500);
        let mut reduction = 1.0;
        for frame in &mut loud {
            reduction = comp.process(frame);
        }
        let mut comp2 = Compressor::default();
        comp2.set(10.0, CompOptions::default());
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
