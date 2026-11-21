//! A virtual bus's output: one packed 24-bit packet per bus frame, which the
//! cable's capture endpoint reads.

use std::sync::Mutex;

use cables::audio::{Gain, PACKET_BYTES, apply_gain};

/// Frames a bus ring keeps. Capture URBs complete once their last frame has
/// elapsed, so they read frames from the last few milliseconds; this covers
/// a stalled capture stream for a few seconds before frames turn silent.
pub const BUS_RING_FRAMES: u64 = 4096;
const EMPTY: u64 = u64::MAX;

pub struct BusRing {
    state: Mutex<RingState>,
}

struct RingState {
    audio: Vec<u8>,
    frames: Vec<u64>,
}

impl Default for BusRing {
    fn default() -> BusRing {
        BusRing::new()
    }
}

impl BusRing {
    pub fn new() -> BusRing {
        BusRing {
            state: Mutex::new(RingState {
                audio: vec![0; BUS_RING_FRAMES as usize * PACKET_BYTES],
                frames: vec![EMPTY; BUS_RING_FRAMES as usize],
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RingState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Stores the bus output of `frame`.
    pub fn write(&self, frame: u64, packet: &[u8]) {
        let mut s = self.lock();
        let slot = (frame % BUS_RING_FRAMES) as usize;
        s.audio[slot * PACKET_BYTES..(slot + 1) * PACKET_BYTES].copy_from_slice(&packet[..PACKET_BYTES]);
        s.frames[slot] = frame;
    }

    /// Fills `dst` with the bus output of `frame` scaled by `gain`, or with
    /// silence if the bus has not produced it (or produced it too long ago).
    /// Returns whether the frame was there.
    pub fn read(&self, frame: u64, dst: &mut [u8], gain: Gain) -> bool {
        let s = self.lock();
        let slot = (frame % BUS_RING_FRAMES) as usize;
        if s.frames[slot] != frame {
            dst.fill(0);
            return false;
        }
        let n = dst.len().min(PACKET_BYTES) / 6 * 6;
        apply_gain(gain, &s.audio[slot * PACKET_BYTES..slot * PACKET_BYTES + n], &mut dst[..n]);
        dst[n..].fill(0);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_read_back_until_overwritten() {
        let ring = BusRing::new();
        let packet: Vec<u8> = (0..PACKET_BYTES).map(|i| i as u8).collect();
        ring.write(5000, &packet);
        let mut out = vec![0xEE; PACKET_BYTES];
        assert!(ring.read(5000, &mut out, Gain::Unity));
        assert_eq!(out, packet);
        assert!(!ring.read(5001, &mut out, Gain::Unity));
        assert!(out.iter().all(|&b| b == 0));
        assert!(!ring.read(5000 + BUS_RING_FRAMES, &mut out, Gain::Unity), "a stale slot is silent");
        let mut short = vec![0xEE; 100];
        ring.read(5000, &mut short, Gain::Unity);
        assert_eq!(&short[..96], &packet[..96]);
        assert!(short[96..].iter().all(|&b| b == 0), "a partial sample is silence");
    }
}
