//! The mixing engine.
//!
//! Everything here runs on the USB bus frame clock that every virtual cable
//! shares, so mixing virtual sources needs no resampling: frame F of each
//! source is summed into frame F of each bus. The engine works in 1 ms frames
//! of 48 stereo samples, in frame order, and catches up in a batch when the
//! coarse clock jumps.
//!
//! - Strips are inputs: virtual inputs (cable playback endpoints that
//!   applications play into) and hardware inputs (physical capture devices,
//!   after their resampler).
//! - Buses are outputs: hardware buses (A) feed physical render devices, and
//!   virtual buses (B) are cable recording endpoints that applications record.
//! - Any strip can be routed to any combination of buses.
//!
//! Samples are f32 between endpoints and packed 24-bit at them. A single
//! source routed at unity gain through untouched processing stays bit exact,
//! since every 24-bit sample is exactly representable in f32.

pub mod cable;
pub mod core;
pub mod denoise;
pub mod dsp;
pub mod meters;
pub mod params;
pub mod ring;

pub use crate::cable::EngineCable;
pub use crate::core::{
    BusSink, BusSpec, Command, Engine, EngineStats, InputPort, OutputPort, StripSource, StripSpec, Topology,
};
pub use crate::meters::{MeterBank, MeterReading};
pub use crate::params::{BusParams, CompOptions, DuckParams, GateOptions, StripParams};
pub use crate::ring::BusRing;

/// Samples per channel in one 1 ms frame at 48 kHz.
pub const FRAME_SAMPLES: usize = 48;
/// Channels of every strip and bus.
pub const CHANNELS: usize = 2;
/// Interleaved samples in one frame.
pub const FRAME_LEN: usize = FRAME_SAMPLES * CHANNELS;
/// The sample rate of every virtual endpoint and of the engine.
pub const SAMPLE_RATE: u32 = 48_000;

/// One frame of interleaved stereo samples.
pub type Frame = [f32; FRAME_LEN];

/// The full scale of 24-bit samples: f32 sample 1.0 is 2^23.
const INT24_SCALE: f32 = 8_388_608.0;

/// Converts packed 24-bit samples to f32. Exact: every 24-bit value fits the
/// f32 mantissa, and the scale is a power of two.
pub fn decode_frame(bytes: &[u8], out: &mut Frame) {
    for (sample, b) in out.iter_mut().zip(bytes.as_chunks::<3>().0.iter()) {
        *sample = cables::audio::decode_int24(b[0], b[1], b[2]) as f32 / INT24_SCALE;
    }
}

/// Converts f32 samples to packed 24-bit, clamped to full scale. The inverse
/// of `decode_frame` for every value it produces.
pub fn encode_frame(samples: &Frame, out: &mut [u8]) {
    for (sample, b) in samples.iter().zip(out.as_chunks_mut::<3>().0.iter_mut()) {
        cables::audio::encode_int24(b, (sample * INT24_SCALE).round() as i32);
    }
}

/// Converts decibels to a linear gain. 0 dB is exactly 1.0.
pub fn db_to_gain(db: f32) -> f32 {
    if db <= -96.0 { 0.0 } else { 10f32.powf(db / 20.0) }
}

/// Converts a linear level to decibels, with silence at -inf.
pub fn gain_to_db(gain: f32) -> f32 {
    if gain <= 0.0 { f32::NEG_INFINITY } else { 20.0 * gain.log10() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cables::audio::PACKET_BYTES;

    #[test]
    fn int24_survives_f32_exactly() {
        let mut bytes = vec![0u8; PACKET_BYTES];
        let values = [0, 1, -1, 0x7f_ffff, -0x80_0000, 12345, -54321, 0x40_0000];
        for (i, chunk) in bytes.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            cables::audio::encode_int24(chunk, values[i % values.len()] * if i % 3 == 0 { 1 } else { -1 }.max(-1));
        }
        let mut frame = [0.0; FRAME_LEN];
        decode_frame(&bytes, &mut frame);
        let mut back = vec![0u8; PACKET_BYTES];
        encode_frame(&frame, &mut back);
        assert_eq!(back, bytes);
    }

    #[test]
    fn unity_is_exact() {
        assert_eq!(db_to_gain(0.0), 1.0);
        assert_eq!(db_to_gain(-200.0), 0.0);
        assert!((db_to_gain(-6.0) - 0.501).abs() < 0.001);
        assert!((gain_to_db(0.5) + 6.02).abs() < 0.01);
    }
}

#[cfg(test)]
pub(crate) mod alloc_counter {
    //! Counts heap calls (allocations, reallocations and frees: a free can
    //! block too) made by the current thread, and by threads being watched,
    //! such as the engine's clock thread, for another thread to read.

    #![allow(clippy::missing_const_for_thread_local)]

    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU64, Ordering};

    thread_local! {
        static COUNT: Cell<u64> = const { Cell::new(0) };
        static WATCHED: Cell<bool> = const { Cell::new(false) };
    }

    /// Heap calls made by watched threads.
    static WATCHED_CALLS: AtomicU64 = AtomicU64::new(0);

    fn count() {
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
        if WATCHED.try_with(Cell::get).unwrap_or(false) {
            WATCHED_CALLS.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct Counting;

    // SAFETY: every call is forwarded unchanged to the system allocator.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            count();
            unsafe { System.alloc(layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            count();
            unsafe { System.realloc(ptr, layout, new_size) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            count();
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;

    /// Heap calls made by this thread so far.
    pub fn heap_calls() -> u64 {
        COUNT.with(|c| c.get())
    }

    /// Starts or stops counting this thread's heap calls in `watched_calls`.
    pub fn watch_this_thread(on: bool) {
        WATCHED.with(|w| w.set(on));
    }

    /// Heap calls made by watched threads so far.
    pub fn watched_calls() -> u64 {
        WATCHED_CALLS.load(Ordering::Relaxed)
    }
}
