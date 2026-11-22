//! Peak and RMS meters, written by the engine and read by the UI through
//! atomics: the UI never locks the engine.

use std::sync::atomic::{AtomicU32, Ordering};

/// One stereo meter. Peaks are the largest absolute sample since the last
/// read; positive f32 values order like their bit patterns, so `fetch_max`
/// on the bits keeps the largest. RMS is smoothed over about 300 ms.
#[derive(Default)]
pub struct Meter {
    peak: [AtomicU32; 2],
    rms: [AtomicU32; 2],
    /// Gain reduction by the gate or compressor (strips) or limiter (buses),
    /// as a linear factor; 1.0 is none. Stored as f32 bits.
    reduction: AtomicU32,
    /// How far ducking lowers a strip, as a linear factor; 1.0 is none.
    /// Stored as f32 bits.
    duck: AtomicU32,
}

/// What a meter showed since it was last read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeterReading {
    pub peak: [f32; 2],
    pub rms: [f32; 2],
    pub reduction: f32,
    /// The ducking gain; 1.0 is none.
    pub duck: f32,
}

impl Default for MeterReading {
    /// Silence, with nothing reduced or ducked.
    fn default() -> MeterReading {
        MeterReading { peak: [0.0; 2], rms: [0.0; 2], reduction: 1.0, duck: 1.0 }
    }
}

impl Meter {
    pub fn record(&self, peak: [f32; 2], rms: [f32; 2], reduction: f32) {
        for channel in 0..2 {
            self.peak[channel].fetch_max(peak[channel].abs().to_bits(), Ordering::Relaxed);
            self.rms[channel].store(rms[channel].to_bits(), Ordering::Relaxed);
        }
        self.reduction.store(reduction.to_bits(), Ordering::Relaxed);
    }

    /// Records how far ducking lowers the strip.
    pub fn record_duck(&self, gain: f32) {
        self.duck.store(gain.to_bits(), Ordering::Relaxed);
    }

    /// Reads the meter and resets its peaks.
    pub fn take(&self) -> MeterReading {
        let peak = |c: usize| f32::from_bits(self.peak[c].swap(0, Ordering::Relaxed));
        let rms = |c: usize| f32::from_bits(self.rms[c].load(Ordering::Relaxed));
        // Zero bits are a meter never written: nothing reduced or ducked.
        let factor = |a: &AtomicU32| match f32::from_bits(a.load(Ordering::Relaxed)) {
            0.0 => 1.0,
            value => value,
        };
        MeterReading {
            peak: [peak(0), peak(1)],
            rms: [rms(0), rms(1)],
            reduction: factor(&self.reduction),
            duck: factor(&self.duck),
        }
    }

    pub fn clear(&self) {
        for channel in 0..2 {
            self.peak[channel].store(0, Ordering::Relaxed);
            self.rms[channel].store(0, Ordering::Relaxed);
        }
        self.reduction.store(1f32.to_bits(), Ordering::Relaxed);
        self.duck.store(1f32.to_bits(), Ordering::Relaxed);
    }
}

/// The meters of every strip and bus of one topology, by index. Each
/// topology gets a bank of its own size, so the bank never grows while audio
/// runs.
pub struct MeterBank {
    pub strips: Vec<Meter>,
    pub buses: Vec<Meter>,
}

impl MeterBank {
    pub fn new(strips: usize, buses: usize) -> MeterBank {
        let meters = |n: usize| {
            (0..n)
                .map(|_| {
                    let meter = Meter::default();
                    meter.clear();
                    meter
                })
                .collect()
        };
        MeterBank { strips: meters(strips), buses: meters(buses) }
    }
}

/// A running mean square with a time constant of about 300 ms at one update
/// per 1 ms frame.
#[derive(Clone, Copy, Default)]
pub struct RmsFollower {
    mean_square: [f32; 2],
}

const RMS_COEFFICIENT: f32 = 1.0 / 300.0;

impl RmsFollower {
    /// Measures one interleaved frame and returns its peak and the running RMS.
    pub fn measure(&mut self, frame: &crate::Frame) -> ([f32; 2], [f32; 2]) {
        let mut peak = [0f32; 2];
        let mut square = [0f32; 2];
        for pair in frame.as_chunks::<2>().0 {
            for channel in 0..2 {
                let sample = pair[channel];
                peak[channel] = peak[channel].max(sample.abs());
                square[channel] += sample * sample;
            }
        }
        let mut rms = [0f32; 2];
        for channel in 0..2 {
            let mean = square[channel] / crate::FRAME_SAMPLES as f32;
            self.mean_square[channel] += (mean - self.mean_square[channel]) * RMS_COEFFICIENT;
            if !self.mean_square[channel].is_finite() || self.mean_square[channel] < 1e-12 {
                self.mean_square[channel] = 0.0;
            }
            rms[channel] = self.mean_square[channel].sqrt();
        }
        (peak, rms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaks_hold_the_largest_until_read() {
        let meter = Meter::default();
        meter.record([0.5, -0.25], [0.1, 0.1], 1.0);
        meter.record([0.2, 0.75], [0.2, 0.2], 0.5);
        let reading = meter.take();
        assert_eq!(reading.peak, [0.5, 0.75]);
        assert_eq!(reading.rms, [0.2, 0.2]);
        assert_eq!(reading.reduction, 0.5);
        assert_eq!(meter.take().peak, [0.0, 0.0], "reading resets the peaks");
    }

    #[test]
    fn rms_of_a_full_scale_square_wave_approaches_one() {
        let mut follower = RmsFollower::default();
        let frame: crate::Frame = std::array::from_fn(|i| if (i / 2) % 2 == 0 { 1.0 } else { -1.0 });
        let mut rms = [0.0; 2];
        for _ in 0..3000 {
            rms = follower.measure(&frame).1;
        }
        assert!((rms[0] - 1.0).abs() < 0.01, "{rms:?}");
    }
}
