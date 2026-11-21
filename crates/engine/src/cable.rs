//! A cable whose audio goes through the engine: its playback side feeds a
//! strip, and its recording side returns a bus.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cables::audio::{DelayLine, Gain, LineStatus, PACKET_BYTES};
use cables::uac2::{CableAudio, FeatureLevel};

use crate::core::Engine;
use crate::ring::BusRing;

pub struct EngineCable {
    engine: Arc<Engine>,
    /// The playback side, which a strip reads frame by frame. Its gain is the
    /// playback feature unit's.
    pub input: Option<Arc<DelayLine>>,
    /// Whether the playback side is streaming, for the strip's missing-audio
    /// count.
    pub playback_active: Arc<AtomicBool>,
    /// The bus the recording side returns.
    pub output: Option<Arc<BusRing>>,
    /// The recording feature unit's gain.
    recording_gain: Mutex<Gain>,
    latency: u64,
}

impl EngineCable {
    /// A cable with a playback side (when `latency` is given, in frames) and
    /// a recording side (when `output` is).
    pub fn new(engine: &Arc<Engine>, latency: Option<u64>, output: Option<Arc<BusRing>>) -> EngineCable {
        EngineCable {
            engine: Arc::clone(engine),
            input: latency.map(|latency| Arc::new(DelayLine::new(latency))),
            playback_active: Arc::new(AtomicBool::new(false)),
            output,
            recording_gain: Mutex::new(Gain::Unity),
            latency: latency.unwrap_or(0),
        }
    }
}

impl CableAudio for EngineCable {
    fn write_playback(&self, start: u64, packets: &mut dyn Iterator<Item = &[u8]>) {
        if let Some(line) = &self.input {
            line.write(start, packets);
        }
    }

    fn read_capture(
        &self,
        start: u64,
        out: &mut [u8],
        lengths: &mut dyn Iterator<Item = usize>,
        _playback_active: bool,
    ) {
        let Some(ring) = &self.output else {
            out.fill(0);
            return;
        };
        let gain = *self.recording_gain.lock().unwrap_or_else(|p| p.into_inner());
        let mut pos = 0;
        let mut processed = None;
        for (frame, length) in (start..).zip(lengths) {
            // The bus frame must exist before it is read: process up to it if
            // the clock thread has not yet.
            if processed.is_none_or(|done| done < frame) {
                self.engine.pull_until(frame);
                processed = Some(frame);
            }
            let length = length.min(out.len() - pos);
            ring.read(frame, &mut out[pos..pos + length], gain);
            pos += length;
        }
        out[pos..].fill(0);
    }

    fn end_playback(&self) {
        if let Some(line) = &self.input {
            line.end_playback();
        }
    }

    fn end_capture(&self) {}

    fn streaming_changed(&self, playback: bool, _capture: bool) {
        // The strip reads the delay line every frame in place of a capture
        // stream, so the line always knows the last frame played: a playback
        // stream that falls behind it is realigned exactly as before.
        self.playback_active.store(playback, Ordering::Relaxed);
    }

    fn set_feature_levels(&self, playback: FeatureLevel, recording: FeatureLevel) {
        if let Some(line) = &self.input {
            line.set_gain(playback.gain());
        }
        *self.recording_gain.lock().unwrap_or_else(|p| p.into_inner()) = recording.gain();
    }

    fn status(&self) -> LineStatus {
        self.input.as_ref().map(|line| line.status()).unwrap_or_default()
    }

    fn delay(&self) -> u64 {
        self.latency
    }
}

/// Bytes in one packet, for callers sizing buffers.
pub const CABLE_PACKET_BYTES: usize = PACKET_BYTES;
