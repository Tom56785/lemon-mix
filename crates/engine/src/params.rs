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
    /// The buses this strip feeds, by bus index in the engine's topology.
    #[serde(skip)]
    pub routes: u64,
    /// Bass, mid and treble, in dB.
    pub eq: [f32; 3],
    /// Noise gate amount, 0 (off) to 10.
    pub gate: f32,
    /// Compressor amount, 0 (off) to 10.
    pub comp: f32,
}

impl Default for StripParams {
    fn default() -> StripParams {
        StripParams {
            gain_db: 0.0,
            mute: false,
            solo: false,
            mono: false,
            pan: 0.0,
            routes: 0,
            eq: [0.0; 3],
            gate: 0.0,
            comp: 0.0,
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
    }
}
