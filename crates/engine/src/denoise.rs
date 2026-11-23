//! Noise suppression for a strip: RNNoise (through nnnoiseless, a Rust port
//! of Xiph's library), a small neural network trained to tell speech from
//! noise. It takes out steady noise (fans, hum, hiss) and much of the rest
//! (keyboards, clicks, the room) while keeping voices.
//!
//! RNNoise works on 10 ms blocks of 48 kHz mono, so the strip gathers ten
//! 1 ms frames, denoises them, and releases them over the next ten: the
//! denoised sound lags by `DENOISE_DELAY`. The knob blends it with the
//! original, delayed to match, so a low setting softens noise without the
//! watery sound full suppression can give music or singing.

use std::sync::OnceLock;

use nnnoiseless::{DenoiseState, RnnModel};

use crate::{CHANNELS, FRAME_SAMPLES, Frame, db_to_gain};

/// The samples RNNoise takes at a time: 10 ms.
const BLOCK: usize = DenoiseState::FRAME_SIZE;
/// How far a denoising strip lags, in samples: the block being gathered,
/// less the frame it is released in, and RNNoise's own block of overlap.
/// 912 samples is 19 ms.
pub const DENOISE_DELAY: usize = 2 * BLOCK - FRAME_SAMPLES;
/// RNNoise takes samples on the scale of 16-bit audio.
const SCALE: f32 = 32768.0;
/// Each step of the knob (0 to 10) takes out this much more of the noise;
/// at 10 the sound is fully denoised.
const DB_PER_STEP: f32 = 4.0;
/// How long the blend takes to follow the knob.
const RAMP_SAMPLES: f32 = 0.02 * crate::SAMPLE_RATE as f32;

/// The network's weights, shared by every strip's denoiser.
fn model() -> &'static RnnModel {
    static MODEL: OnceLock<RnnModel> = OnceLock::new();
    MODEL.get_or_init(RnnModel::default)
}

/// Sets up what nnnoiseless keeps for each thread (the FFT's scratch space)
/// and for all (its tables), which it would otherwise allocate on the first
/// block a thread denoises. The audio thread calls this as it starts.
pub fn prepare_thread() {
    let mut state = DenoiseState::with_model(model());
    let input: [f32; BLOCK] = std::array::from_fn(|n| 1000.0 * (n as f32 * 0.05).sin());
    let mut output = [0.0; BLOCK];
    state.process_frame(&mut output, &input);
}

/// The share of the denoised sound in the blend, for a knob setting.
pub fn denoise_mix(amount: f32) -> f32 {
    if amount <= 0.0 {
        0.0
    } else if amount >= crate::params::MAX_DYNAMICS {
        1.0
    } else {
        1.0 - db_to_gain(-amount * DB_PER_STEP)
    }
}

pub struct Denoiser {
    /// One network state per channel.
    states: [Box<DenoiseState<'static>>; CHANNELS],
    /// The block being gathered, scaled for RNNoise.
    input: Box<[[f32; BLOCK]; CHANNELS]>,
    /// The last denoised block, being released.
    output: Box<[[f32; BLOCK]; CHANNELS]>,
    /// The original sound, delayed to line up with the denoised.
    dry: Box<[[f32; DENOISE_DELAY]; CHANNELS]>,
    dry_pos: usize,
    /// Samples gathered into `input`, and released from `output`.
    gathered: usize,
    released: usize,
    /// The share of denoised sound asked for, and the share now.
    target: f32,
    mix: f32,
    /// Whether it is in the path (and so delaying the strip).
    running: bool,
    /// Which of the ten frames of a block it runs the network on, so strips
    /// that start together do not all run it on the same frame.
    phase: usize,
}

impl Default for Denoiser {
    fn default() -> Denoiser {
        Denoiser::new()
    }
}

impl Denoiser {
    /// A denoiser, off. It allocates, so it is made with the layout, never
    /// on the audio thread.
    pub fn new() -> Denoiser {
        Denoiser {
            states: [DenoiseState::with_model(model()), DenoiseState::with_model(model())],
            input: Box::new([[0.0; BLOCK]; CHANNELS]),
            output: Box::new([[0.0; BLOCK]; CHANNELS]),
            dry: Box::new([[0.0; DENOISE_DELAY]; CHANNELS]),
            dry_pos: 0,
            gathered: 0,
            released: FRAME_SAMPLES,
            target: 0.0,
            mix: 0.0,
            running: false,
            phase: 0,
        }
    }

    /// A denoiser that runs its network on the frames of `slot` (the strip's
    /// index, say), apart from its neighbours'.
    pub fn staggered(slot: usize) -> Denoiser {
        Denoiser { phase: slot % (BLOCK / FRAME_SAMPLES), ..Denoiser::new() }
    }

    /// The knob, 0 (off) to 10 (fully denoised).
    pub fn set(&mut self, amount: f32) {
        self.target = denoise_mix(amount);
    }

    /// Whether `process` has anything to do: on, or fading out.
    pub fn active(&self) -> bool {
        self.running || self.target > 0.0
    }

    /// Takes up another's moment, so a strip carrying on into a new layout
    /// keeps its sound and delay. The other, from the layout being dropped,
    /// gets this one's fresh state: swapping allocates nothing.
    pub fn carry_from(&mut self, old: &mut Denoiser) {
        let target = self.target;
        std::mem::swap(self, old);
        self.target = target;
    }

    /// Starts afresh: the gathered and delayed sound is of the past, before
    /// it was last turned off.
    fn clear(&mut self) {
        for channel in 0..CHANNELS {
            self.input[channel].fill(0.0);
            self.output[channel].fill(0.0);
            self.dry[channel].fill(0.0);
        }
        self.dry_pos = 0;
        // The first block starts part filled (with silence) by its phase.
        // Until it is denoised, the silence after this frame's place is
        // released: a block completes on the frame that releases its start.
        self.gathered = self.phase * FRAME_SAMPLES;
        self.released = self.gathered + FRAME_SAMPLES;
        self.mix = 0.0;
    }

    pub fn process(&mut self, frame: &mut Frame) {
        let starting = !self.running;
        if starting {
            if self.target <= 0.0 {
                return;
            }
            self.clear();
            self.running = true;
        }
        let direct = *frame;

        for (i, pair) in direct.as_chunks::<CHANNELS>().0.iter().enumerate() {
            for (channel, &sample) in pair.iter().enumerate() {
                self.input[channel][self.gathered + i] = sample * SCALE;
            }
        }
        self.gathered += FRAME_SAMPLES;
        if self.gathered == BLOCK {
            for (state, (output, input)) in self.states.iter_mut().zip(self.output.iter_mut().zip(self.input.iter())) {
                state.process_frame(output, input);
            }
            self.gathered = 0;
            self.released = 0;
        }

        let step = 1.0 / RAMP_SAMPLES;
        for (i, pair) in frame.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
            self.mix = if self.mix < self.target {
                (self.mix + step).min(self.target)
            } else {
                (self.mix - step).max(self.target)
            };
            for (channel, sample) in pair.iter_mut().enumerate() {
                let delayed = self.dry[channel][self.dry_pos];
                self.dry[channel][self.dry_pos] = *sample;
                let denoised = self.output[channel][self.released + i] / SCALE;
                *sample = delayed + (denoised - delayed) * self.mix;
            }
            self.dry_pos = (self.dry_pos + 1) % DENOISE_DELAY;
        }
        self.released += FRAME_SAMPLES;

        // The delay comes or goes: crossfade over this frame between the
        // sound as it was and as it now is, so the jump does not click.
        let stopping = self.target <= 0.0 && self.mix <= 0.0;
        if starting || stopping {
            for (i, pair) in frame.as_chunks_mut::<CHANNELS>().0.iter_mut().enumerate() {
                let t = (i as f32 + 0.5) / FRAME_SAMPLES as f32;
                let processed = if starting { t } else { 1.0 - t };
                for (channel, sample) in pair.iter_mut().enumerate() {
                    let original = direct[i * CHANNELS + channel];
                    *sample = original + (*sample - original) * processed;
                }
            }
        }
        if stopping {
            self.running = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FRAME_LEN;

    /// Repeatable white noise, -1 to 1.
    struct Noise(u32);

    impl Noise {
        fn next(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (self.0 >> 8) as f32 / (1u32 << 23) as f32 - 1.0
        }
    }

    /// Runs `frames` frames of noise at `level` through the denoiser; returns
    /// the input and output, left channel.
    fn run(denoiser: &mut Denoiser, noise: &mut Noise, level: f32, frames: usize) -> (Vec<f32>, Vec<f32>) {
        let (mut input, mut output) = (Vec::new(), Vec::new());
        for _ in 0..frames {
            let mut frame: Frame = [0.0; FRAME_LEN];
            for pair in frame.as_chunks_mut::<CHANNELS>().0 {
                let sample = noise.next() * level;
                pair.fill(sample);
                input.push(sample);
            }
            denoiser.process(&mut frame);
            output.extend(frame.as_chunks::<CHANNELS>().0.iter().map(|pair| pair[0]));
        }
        (input, output)
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn the_knob_takes_out_more_noise_as_it_turns() {
        assert_eq!(denoise_mix(0.0), 0.0);
        assert_eq!(denoise_mix(10.0), 1.0);
        assert!((denoise_mix(5.0) - 0.9).abs() < 1e-3, "20 dB at 5: {}", denoise_mix(5.0));
        assert!(denoise_mix(2.5) < denoise_mix(5.0));
    }

    #[test]
    fn off_it_leaves_the_sound_alone() {
        let mut denoiser = Denoiser::new();
        let mut noise = Noise(1);
        let (input, output) = run(&mut denoiser, &mut noise, 0.3, 20);
        assert_eq!(input, output);
        assert!(!denoiser.active());
    }

    /// The original, delayed, lines up with the denoised sound to the
    /// sample: noise too quiet for RNNoise to act on comes through both the
    /// same, so the blend of the two is the input, `DENOISE_DELAY` later.
    #[test]
    fn the_original_and_denoised_sound_line_up() {
        for slot in [0, 7] {
            line_up(slot);
        }
    }

    fn line_up(slot: usize) {
        let mut denoiser = Denoiser::staggered(slot);
        denoiser.set(10.0);
        let mut noise = Noise(7);
        let (input, output) = run(&mut denoiser, &mut noise, 1e-6, 200);
        let lag_error = |lag: usize| -> f32 {
            let pairs = (DENOISE_DELAY + 2000..input.len()).map(|n| (output[n], input[n - lag]));
            pairs.map(|(o, i)| (o - i) * (o - i)).sum::<f32>()
        };
        let best = (DENOISE_DELAY - 100..DENOISE_DELAY + 100).min_by(|&a, &b| lag_error(a).total_cmp(&lag_error(b)));
        assert_eq!(best, Some(DENOISE_DELAY));
        let error = lag_error(DENOISE_DELAY).sqrt() / rms(&input[DENOISE_DELAY + 2000..]) / (input.len() as f32).sqrt();
        assert!(error < 0.05, "denoised quiet noise is the input, delayed: error {error}");
    }

    #[test]
    fn it_takes_out_steady_noise_and_comes_out_cleanly() {
        let mut denoiser = Denoiser::new();
        let mut noise = Noise(3);
        denoiser.set(10.0);
        let (input, output) = run(&mut denoiser, &mut noise, 0.05, 1000);
        let reduction = 20.0 * (rms(&output[24_000..]) / rms(&input[24_000..])).log10();
        assert!(reduction < -15.0, "fully on, steady noise drops by {reduction:.1} dB");

        // Half way: the original, 20 dB down, comes through the blend.
        denoiser.set(5.0);
        let (input, output) = run(&mut denoiser, &mut noise, 0.05, 500);
        let half = 20.0 * (rms(&output[12_000..]) / rms(&input[12_000..])).log10();
        assert!(half > reduction && half > -21.0 && half < -12.0, "half way: {half:.1} dB");

        // Off: it fades out, and the sound is the input again, undelayed.
        denoiser.set(0.0);
        run(&mut denoiser, &mut noise, 0.05, 30);
        assert!(!denoiser.active());
        let (input, output) = run(&mut denoiser, &mut noise, 0.05, 10);
        assert_eq!(input, output);
    }

    #[test]
    fn turning_it_on_and_off_does_not_jump() {
        // A low tone, where a jump in the sound would show as a step.
        let mut denoiser = Denoiser::new();
        let tone = |n: usize| 0.5 * (n as f32 * 2.0 * std::f32::consts::PI * 100.0 / 48_000.0).sin();
        let mut n = 0;
        let mut previous = 0.0f32;
        let mut largest = 0.0f32;
        for frame_index in 0..300 {
            match frame_index {
                50 => denoiser.set(3.0),
                150 => denoiser.set(0.0),
                _ => {}
            }
            let mut frame: Frame = [0.0; FRAME_LEN];
            for pair in frame.as_chunks_mut::<CHANNELS>().0 {
                pair.fill(tone(n));
                n += 1;
            }
            denoiser.process(&mut frame);
            for pair in frame.as_chunks::<CHANNELS>().0.iter() {
                largest = largest.max((pair[0] - previous).abs());
                previous = pair[0];
            }
        }
        // The tone itself moves at most 0.0066 a sample; a cut would be ~0.5.
        assert!(largest < 0.05, "largest step {largest}");
    }

    /// It runs on the audio thread: denoising, the network included, allocates
    /// nothing once made.
    #[test]
    fn denoising_does_not_allocate() {
        prepare_thread();
        let mut denoiser = Denoiser::new();
        denoiser.set(7.0);
        let mut noise = Noise(9);
        let mut frames: Vec<Frame> = (0..100)
            .map(|_| {
                let mut frame: Frame = [0.0; FRAME_LEN];
                frame.iter_mut().for_each(|s| *s = noise.next() * 0.2);
                frame
            })
            .collect();
        // From the very first block: what nnnoiseless sets up lazily was set up
        // for this thread.
        let before = crate::alloc_counter::allocations();
        for frame in &mut frames[20..] {
            denoiser.process(frame);
        }
        denoiser.set(0.0);
        for frame in &mut frames[20..] {
            denoiser.process(frame);
        }
        assert_eq!(crate::alloc_counter::allocations() - before, 0);
    }

    /// How long a frame takes, the ones that run the network included. Run
    /// with `cargo test --release -p engine denoise_timing -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn denoise_timing() {
        prepare_thread();
        let mut denoiser = Denoiser::new();
        denoiser.set(10.0);
        let mut noise = Noise(11);
        let mut worst = std::time::Duration::ZERO;
        let started = std::time::Instant::now();
        for _ in 0..10_000 {
            let mut frame: Frame = [0.0; FRAME_LEN];
            frame.iter_mut().for_each(|s| *s = noise.next() * 0.1);
            let t = std::time::Instant::now();
            denoiser.process(&mut frame);
            worst = worst.max(t.elapsed());
        }
        eprintln!("10 s of audio in {:?}; slowest frame {worst:?}", started.elapsed());
    }

    #[test]
    fn a_carried_denoiser_keeps_its_moment_and_takes_the_new_setting() {
        let mut old = Denoiser::new();
        old.set(10.0);
        let mut noise = Noise(5);
        run(&mut old, &mut noise, 0.05, 100);
        let mut new = Denoiser::new();
        new.set(4.0);
        new.carry_from(&mut old);
        assert!(new.running && new.mix > 0.99, "it carries on denoising");
        assert_eq!(new.target, denoise_mix(4.0), "at the new setting");
        assert!(!old.running, "the dropped one has the fresh state");
    }
}
