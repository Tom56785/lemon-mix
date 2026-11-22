//! The settings of strips and buses, as the user sets them.

use serde::{Deserialize, Serialize};

/// The gain range of strip and bus faders, in dB.
pub const MIN_GAIN_DB: f32 = -60.0;
pub const MAX_GAIN_DB: f32 = 12.0;
/// The EQ band range, in dB.
pub const MAX_EQ_DB: f32 = 12.0;
/// The gate and compressor knobs go from 0 (off) to this.
pub const MAX_DYNAMICS: f32 = 10.0;

/// One input strip's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StripParams {
    /// Fader, in dB; at or below `MIN_GAIN_DB` the strip is silent.
    pub gain_db: f32,
    pub mute: bool,
    /// While any strip is soloed, only soloed strips are heard.
    pub solo: bool,
    /// Sums left and right before the fader.
    pub mono: bool,
    /// Balance, from -1 (left) to 1 (right). The centre leaves both at unity.
    pub pan: f32,
    /// Bass, mid and treble, in dB.
    pub eq: [f32; 3],
    /// Noise gate amount, 0 (off) to 10.
    pub gate: f32,
    /// The noise gate's range and timing.
    pub gate_options: GateOptions,
    /// Compressor amount, 0 (off) to 10.
    pub comp: f32,
    /// The compressor's timing, knee, make-up and (when manual) curve.
    pub comp_options: CompOptions,
    /// How the strip is lowered while its ducking sources play. The sources
    /// are set apart, as routes are; with none, the strip is never ducked.
    pub duck: DuckParams,
}

impl Default for StripParams {
    fn default() -> StripParams {
        StripParams {
            gain_db: 0.0,
            mute: false,
            solo: false,
            mono: false,
            pan: 0.0,
            eq: [0.0; 3],
            gate: 0.0,
            comp: 0.0,
            gate_options: GateOptions::default(),
            comp_options: CompOptions::default(),
            duck: DuckParams::default(),
        }
    }
}

/// The noise gate's finer settings. Its threshold is set by the strip's
/// Gate knob.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateOptions {
    /// How far the gate closes, in dB.
    pub range_db: f32,
    /// How fast it opens once the level passes the threshold, in ms.
    pub attack_ms: f32,
    /// How long it stays open after the level drops, in ms.
    pub hold_ms: f32,
    /// How fast it closes after the hold, in ms.
    pub release_ms: f32,
}

pub const GATE_RANGE_DB: std::ops::RangeInclusive<f32> = 6.0..=90.0;
pub const GATE_ATTACK_MS: std::ops::RangeInclusive<f32> = 0.1..=50.0;
pub const GATE_HOLD_MS: std::ops::RangeInclusive<f32> = 0.0..=2000.0;
pub const GATE_RELEASE_MS: std::ops::RangeInclusive<f32> = 5.0..=2000.0;

impl Default for GateOptions {
    fn default() -> GateOptions {
        GateOptions { range_db: 60.0, attack_ms: 1.0, hold_ms: 50.0, release_ms: 80.0 }
    }
}

impl GateOptions {
    pub fn sanitized(self) -> GateOptions {
        let d = GateOptions::default();
        GateOptions {
            range_db: within(self.range_db, GATE_RANGE_DB, d.range_db),
            attack_ms: within(self.attack_ms, GATE_ATTACK_MS, d.attack_ms),
            hold_ms: within(self.hold_ms, GATE_HOLD_MS, d.hold_ms),
            release_ms: within(self.release_ms, GATE_RELEASE_MS, d.release_ms),
        }
    }
}

/// The compressor's finer settings. Its threshold and ratio follow the
/// strip's Comp knob, unless `manual`, when they are set here and the knob
/// only turns it on (above 0) and off.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompOptions {
    /// How fast it reduces the gain once the level passes the threshold, in ms.
    pub attack_ms: f32,
    /// How fast the gain recovers, in ms.
    pub release_ms: f32,
    /// How gradually it starts around the threshold, in dB; 0 is a hard knee.
    pub knee_db: f32,
    /// Make-up gain in dB; `None` is automatic, from the threshold and ratio.
    pub makeup_db: Option<f32>,
    /// Use `threshold_db` and `ratio` rather than the knob's.
    pub manual: bool,
    pub threshold_db: f32,
    pub ratio: f32,
}

pub const COMP_ATTACK_MS: std::ops::RangeInclusive<f32> = 0.1..=200.0;
pub const COMP_RELEASE_MS: std::ops::RangeInclusive<f32> = 10.0..=2000.0;
pub const COMP_KNEE_DB: std::ops::RangeInclusive<f32> = 0.0..=24.0;
pub const COMP_MAKEUP_DB: std::ops::RangeInclusive<f32> = 0.0..=24.0;
pub const COMP_THRESHOLD_DB: std::ops::RangeInclusive<f32> = -60.0..=0.0;
pub const COMP_RATIO: std::ops::RangeInclusive<f32> = 1.0..=20.0;

impl Default for CompOptions {
    fn default() -> CompOptions {
        CompOptions {
            attack_ms: 5.0,
            release_ms: 120.0,
            knee_db: 6.0,
            makeup_db: None,
            manual: false,
            threshold_db: -15.0,
            ratio: 2.5,
        }
    }
}

impl CompOptions {
    pub fn sanitized(self) -> CompOptions {
        let d = CompOptions::default();
        CompOptions {
            attack_ms: within(self.attack_ms, COMP_ATTACK_MS, d.attack_ms),
            release_ms: within(self.release_ms, COMP_RELEASE_MS, d.release_ms),
            knee_db: within(self.knee_db, COMP_KNEE_DB, d.knee_db),
            makeup_db: self.makeup_db.map(|db| within(db, COMP_MAKEUP_DB, 0.0)),
            manual: self.manual,
            threshold_db: within(self.threshold_db, COMP_THRESHOLD_DB, d.threshold_db),
            ratio: within(self.ratio, COMP_RATIO, d.ratio),
        }
    }
}

fn within(value: f32, range: std::ops::RangeInclusive<f32>, fallback: f32) -> f32 {
    clamp(value, *range.start(), *range.end(), fallback)
}

/// Ducking: lowering a strip while others play, such as music under a
/// microphone.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DuckParams {
    /// How far the strip is lowered, in dB.
    pub amount_db: f32,
    /// The level, in dBFS, above which a source counts as playing.
    pub threshold_db: f32,
    /// How long the strip takes to go down to the lowered level once a source
    /// plays, in ms: an even fade in dB. 0 is at once.
    pub fade_down_ms: f32,
    /// How long it stays down after the sources go quiet, in ms, so it does
    /// not come up between words.
    pub hold_ms: f32,
    /// How long it takes to come back up after the hold, in ms.
    pub fade_up_ms: f32,
}

/// The ranges of the ducking settings.
pub const MAX_DUCK_DB: f32 = 60.0;
pub const MIN_DUCK_THRESHOLD_DB: f32 = -80.0;
pub const DUCK_FADE_DOWN_MS: std::ops::RangeInclusive<f32> = 0.0..=5000.0;
pub const DUCK_HOLD_MS: std::ops::RangeInclusive<f32> = 0.0..=5000.0;
pub const DUCK_FADE_UP_MS: std::ops::RangeInclusive<f32> = 0.0..=10_000.0;

impl Default for DuckParams {
    fn default() -> DuckParams {
        DuckParams { amount_db: 12.0, threshold_db: -40.0, fade_down_ms: 50.0, hold_ms: 300.0, fade_up_ms: 1000.0 }
    }
}

impl DuckParams {
    pub fn sanitized(self) -> DuckParams {
        let default = DuckParams::default();
        let range = |v: f32, r: std::ops::RangeInclusive<f32>, fallback| clamp(v, *r.start(), *r.end(), fallback);
        DuckParams {
            amount_db: clamp(self.amount_db, 0.0, MAX_DUCK_DB, default.amount_db),
            threshold_db: clamp(self.threshold_db, MIN_DUCK_THRESHOLD_DB, 0.0, default.threshold_db),
            fade_down_ms: range(self.fade_down_ms, DUCK_FADE_DOWN_MS, default.fade_down_ms),
            hold_ms: range(self.hold_ms, DUCK_HOLD_MS, default.hold_ms),
            fade_up_ms: range(self.fade_up_ms, DUCK_FADE_UP_MS, default.fade_up_ms),
        }
    }
}

impl StripParams {
    /// Brings every value into its range; NaN becomes the default.
    pub fn sanitized(mut self) -> StripParams {
        self.gain_db = clamp(self.gain_db, MIN_GAIN_DB, MAX_GAIN_DB, 0.0);
        self.pan = clamp(self.pan, -1.0, 1.0, 0.0);
        for band in &mut self.eq {
            *band = clamp(*band, -MAX_EQ_DB, MAX_EQ_DB, 0.0);
        }
        self.gate = clamp(self.gate, 0.0, MAX_DYNAMICS, 0.0);
        self.comp = clamp(self.comp, 0.0, MAX_DYNAMICS, 0.0);
        self.duck = self.duck.sanitized();
        self.gate_options = self.gate_options.sanitized();
        self.comp_options = self.comp_options.sanitized();
        self
    }

    /// The strip's linear gain for the left and right channels, before mute
    /// and solo: the fader with the balance law, which leaves the louder side
    /// at the fader's gain.
    pub fn channel_gains(&self) -> [f32; 2] {
        if self.gain_db <= MIN_GAIN_DB {
            return [0.0; 2];
        }
        let gain = crate::db_to_gain(self.gain_db);
        let pan = self.pan.clamp(-1.0, 1.0);
        [gain * (1.0 - pan).min(1.0), gain * (1.0 + pan).min(1.0)]
    }
}

/// One output bus's settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BusParams {
    pub gain_db: f32,
    pub mute: bool,
    pub mono: bool,
    /// The limiter's ceiling in dBFS, when it is on.
    pub limiter: Option<f32>,
}

impl Default for BusParams {
    fn default() -> BusParams {
        BusParams { gain_db: 0.0, mute: false, mono: false, limiter: None }
    }
}

impl BusParams {
    pub fn sanitized(mut self) -> BusParams {
        self.gain_db = clamp(self.gain_db, MIN_GAIN_DB, MAX_GAIN_DB, 0.0);
        self.limiter = self.limiter.map(|ceiling| clamp(ceiling, -40.0, 0.0, -1.0));
        self
    }

    /// The bus gain, with mute.
    pub fn gain(&self) -> f32 {
        if self.mute || self.gain_db <= MIN_GAIN_DB { 0.0 } else { crate::db_to_gain(self.gain_db) }
    }
}

fn clamp(value: f32, min: f32, max: f32, fallback: f32) -> f32 {
    if value.is_nan() { fallback } else { value.clamp(min, max) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centre_balance_is_unity() {
        assert_eq!(StripParams::default().channel_gains(), [1.0, 1.0]);
        let left = StripParams { pan: -1.0, ..Default::default() };
        assert_eq!(left.channel_gains(), [1.0, 0.0]);
        let half = StripParams { pan: 0.5, gain_db: MIN_GAIN_DB, ..Default::default() };
        assert_eq!(half.channel_gains(), [0.0, 0.0], "the bottom of the fader is silence");
    }

    #[test]
    fn values_are_kept_in_range() {
        let wild = StripParams {
            gain_db: 99.0,
            pan: f32::NAN,
            eq: [-40.0, 3.0, 40.0],
            gate: -1.0,
            comp: 11.0,
            ..Default::default()
        };
        let tame = wild.sanitized();
        assert_eq!((tame.gain_db, tame.pan, tame.eq, tame.gate, tame.comp), (12.0, 0.0, [-12.0, 3.0, 12.0], 0.0, 10.0));
        assert_eq!(BusParams { limiter: Some(3.0), ..Default::default() }.sanitized().limiter, Some(0.0));
        assert_eq!(BusParams { mute: true, ..Default::default() }.gain(), 0.0);
        let duck =
            DuckParams { amount_db: 99.0, threshold_db: f32::NAN, fade_down_ms: -1.0, hold_ms: -5.0, fade_up_ms: 1e9 };
        assert_eq!(
            duck.sanitized(),
            DuckParams { amount_db: 60.0, threshold_db: -40.0, fade_down_ms: 0.0, hold_ms: 0.0, fade_up_ms: 10_000.0 }
        );
        assert_eq!(
            StripParams { duck, ..Default::default() }.sanitized().duck.amount_db,
            60.0,
            "strips sanitize it too"
        );
    }
}
