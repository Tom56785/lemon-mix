use std::sync::atomic::AtomicBool;

use cables::audio::{FRAME_BYTES, decode_int24, encode_int24};
use cables::uac2::{
    CableAudio, CableKind, Descriptors, Device, FeatureLevel, REQUEST_SET_CONFIGURATION, REQUEST_SET_INTERFACE,
    SetupPacket,
};

use super::*;
use crate::cable::EngineCable;
use crate::{FRAME_SAMPLES, db_to_gain};

const LATENCY: u64 = 20;

/// A packet of pseudo-random 24-bit samples that identifies its frame.
fn noise_packet(frame: u64, seed: u64) -> Vec<u8> {
    let mut state = frame.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ seed;
    let mut packet = vec![0u8; PACKET_BYTES];
    for sample in packet.as_chunks_mut::<3>().0.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        encode_int24(sample, ((state >> 40) as i32) - 0x80_0000);
    }
    packet
}

/// A packet whose every sample is `value`.
fn constant_packet(value: i32) -> Vec<u8> {
    let mut packet = vec![0u8; PACKET_BYTES];
    for sample in packet.as_chunks_mut::<3>().0.iter_mut() {
        encode_int24(sample, value);
    }
    packet
}

fn samples(packet: &[u8]) -> Vec<i32> {
    packet.as_chunks::<3>().0.iter().map(|b| decode_int24(b[0], b[1], b[2])).collect()
}

fn routed(buses: &[usize]) -> StripParams {
    StripParams { routes: buses.iter().fold(0, |mask, &b| mask | 1 << b), ..Default::default() }
}

struct Rig {
    engine: Arc<Engine>,
    cables: Vec<Arc<EngineCable>>,
}

/// An engine with `inputs` playback-only cables as strips 0.. and `outputs`
/// recording-only cables as buses 0.., every strip routed by `routes`.
fn rig(inputs: usize, outputs: usize, routes: &[&[usize]]) -> Rig {
    let engine = Arc::new(Engine::new());
    let mut cables = Vec::new();
    let mut topology = Topology::default();
    for i in 0..inputs {
        let cable = Arc::new(EngineCable::new(&engine, Some(LATENCY), None));
        cable.streaming_changed(true, false);
        topology.strips.push(StripSpec {
            source: StripSource::Virtual {
                line: Arc::clone(cable.input.as_ref().unwrap()),
                playback_active: Arc::clone(&cable.playback_active),
            },
            params: routed(routes.get(i).copied().unwrap_or(&[])),
        });
        cables.push(cable);
    }
    for _ in 0..outputs {
        let ring = Arc::new(BusRing::new());
        let cable = Arc::new(EngineCable::new(&engine, None, Some(Arc::clone(&ring))));
        topology.buses.push(BusSpec { sink: BusSink::Virtual(ring), params: BusParams::default() });
        cables.push(cable);
    }
    engine.set_topology(topology);
    Rig { engine, cables }
}

fn capture(cable: &EngineCable, frame: u64) -> Vec<u8> {
    let mut out = vec![0xEE; PACKET_BYTES];
    cable.read_capture(frame, &mut out, &mut [PACKET_BYTES].into_iter(), false);
    out
}

/// A loopback cable routed through the engine to its own bus at unity is
/// the original cable: every packet comes out bit for bit, `latency` later.
#[test]
fn unity_single_route_is_bit_exact() {
    let engine = Arc::new(Engine::new());
    let ring = Arc::new(BusRing::new());
    let cable = Arc::new(EngineCable::new(&engine, Some(LATENCY), Some(Arc::clone(&ring))));
    let descriptors = Descriptors::build(CableKind::Loopback, 1, "S1", "Loop", "Lemon Mix").unwrap();
    let device = Device::with_audio(1, descriptors, Arc::clone(&cable) as Arc<dyn CableAudio>);
    device.handle_control(SetupPacket { request: REQUEST_SET_CONFIGURATION, value: 1, ..Default::default() }, &[]);
    for iface in [1, 2] {
        let setup =
            SetupPacket { request_type: 0x01, request: REQUEST_SET_INTERFACE, index: iface, value: 1, length: 0 };
        device.handle_control(setup, &[]);
    }
    engine.set_topology(Topology {
        strips: vec![StripSpec {
            source: StripSource::Virtual {
                line: Arc::clone(cable.input.as_ref().unwrap()),
                playback_active: Arc::clone(&cable.playback_active),
            },
            params: routed(&[0]),
        }],
        // A second, silent bus and a muted second strip must not disturb it.
        buses: vec![BusSpec { sink: BusSink::Virtual(ring), params: BusParams::default() }],
    });
    // Windows sends playback in 10-packet URBs, about 10 ms ahead.
    for urb in 0..30u64 {
        let start = 10_000 + urb * 10;
        let packets: Vec<Vec<u8>> = (start..start + 10).map(|f| noise_packet(f, 1)).collect();
        device.write_playback(start, packets.iter().map(Vec::as_slice));
        // The capture side reads the frames that have elapsed meanwhile.
        if urb >= 3 {
            let read_start = start - 10 + LATENCY;
            let mut out = vec![0u8; 10 * PACKET_BYTES];
            device.read_capture(read_start, &mut out, std::iter::repeat_n(PACKET_BYTES, 10));
            for (i, packet) in out.chunks(PACKET_BYTES).enumerate() {
                let frame = read_start + i as u64 - LATENCY;
                assert_eq!(packet, noise_packet(frame, 1).as_slice(), "capture frame {}", read_start + i as u64);
            }
        }
    }
    assert_eq!(device.audio.status(), LineStatus::default(), "nothing missing or realigned");
    assert!(engine.stats.pulled.load(Ordering::Relaxed) > 0, "capture pulled the frames it needed");
}

use cables::audio::LineStatus;

#[test]
fn sources_routed_to_one_bus_sum() {
    let rig = rig(3, 1, &[&[0], &[0], &[]]);
    let (a, b, c, bus) = (&rig.cables[0], &rig.cables[1], &rig.cables[2], &rig.cables[3]);
    a.write_playback(1000, &mut [constant_packet(1000).as_slice()].into_iter());
    b.write_playback(1000, &mut [constant_packet(-300).as_slice()].into_iter());
    c.write_playback(1000, &mut [constant_packet(5555).as_slice()].into_iter()); // not routed
    let out = capture(bus, 1000 + LATENCY);
    assert!(samples(&out).iter().all(|&s| s == 700), "{:?}", &samples(&out)[..4]);
    // Clamped once, at the bus: two loud sources clip only the sum.
    a.write_playback(1001, &mut [constant_packet(0x60_0000).as_slice()].into_iter());
    b.write_playback(1001, &mut [constant_packet(0x60_0000).as_slice()].into_iter());
    assert!(samples(&capture(bus, 1001 + LATENCY)).iter().all(|&s| s == 0x7f_ffff));
}

#[test]
fn routing_matrix_and_solo() {
    let rig = rig(2, 3, &[&[0, 2], &[1, 2]]);
    let (a, b) = (&rig.cables[0], &rig.cables[1]);
    let buses = &rig.cables[2..];
    let play = |frame: u64| {
        a.write_playback(frame, &mut [constant_packet(100).as_slice()].into_iter());
        b.write_playback(frame, &mut [constant_packet(20).as_slice()].into_iter());
    };
    let first = |cable: &EngineCable, frame: u64| samples(&capture(cable, frame + LATENCY))[FRAME_LEN - 1];
    play(2000);
    assert_eq!([first(&buses[0], 2000), first(&buses[1], 2000), first(&buses[2], 2000)], [100, 20, 120]);
    // Solo strip 1: strip 0 fades out over the next frame and stays out.
    rig.engine.set_strip(1, StripParams { solo: true, ..routed(&[1, 2]) });
    play(2001);
    play(2002);
    assert_eq!(first(&buses[2], 2001), 20, "the ramp ends at the target");
    assert_eq!([first(&buses[0], 2002), first(&buses[1], 2002), first(&buses[2], 2002)], [0, 20, 20]);
}

/// A gain change ramps linearly across the next frame, so there is no step.
#[test]
fn gain_changes_ramp_across_a_frame() {
    let rig = rig(1, 1, &[&[0]]);
    let (input, bus) = (&rig.cables[0], &rig.cables[1]);
    let value = 0x10_0000;
    for frame in 3000..3003 {
        input.write_playback(frame, &mut [constant_packet(value).as_slice()].into_iter());
    }
    assert!(samples(&capture(bus, 3000 + LATENCY)).iter().all(|&s| s == value));
    rig.engine.set_strip(0, StripParams { gain_db: -6.0, ..routed(&[0]) });
    let ramped = samples(&capture(bus, 3001 + LATENCY));
    let target = db_to_gain(-6.0);
    for i in 0..FRAME_SAMPLES {
        let w = (i + 1) as f32 / FRAME_SAMPLES as f32;
        let expected = (value as f32 * (1.0 + (target - 1.0) * w)).round() as i32;
        assert!((ramped[i * 2] - expected).abs() <= 1, "sample {i}: {} vs {expected}", ramped[i * 2]);
    }
    let settled = samples(&capture(bus, 3002 + LATENCY));
    assert!(settled.iter().all(|&s| s == (value as f32 * target).round() as i32), "then constant");
    // Bus mute ramps too.
    rig.engine.set_bus(0, BusParams { mute: true, ..Default::default() });
    input.write_playback(3003, &mut [constant_packet(value).as_slice()].into_iter());
    input.write_playback(3004, &mut [constant_packet(value).as_slice()].into_iter());
    let fading = samples(&capture(bus, 3003 + LATENCY));
    assert!(fading[0] > 0 && fading[FRAME_LEN - 1] == 0, "fades to silence within the frame");
    assert!(samples(&capture(bus, 3004 + LATENCY)).iter().all(|&s| s == 0));
}

/// Where the playback stream was cut, the strip crossfades from the last
/// sample instead of stepping.
#[test]
fn cuts_are_crossfaded() {
    let rig = rig(1, 1, &[&[0]]);
    let (input, bus) = (&rig.cables[0], &rig.cables[1]);
    let loud = 0x40_0000;
    input.write_playback(4000, &mut [constant_packet(loud).as_slice(); 2].into_iter());
    assert!(samples(&capture(bus, 4000 + LATENCY)).iter().all(|&s| s == loud));
    assert!(samples(&capture(bus, 4001 + LATENCY)).iter().all(|&s| s == loud));
    // Frame 4002 never arrives: the strip fades out instead of cutting.
    let gap = samples(&capture(bus, 4002 + LATENCY));
    assert!((gap[0] - loud * 47 / 48).abs() <= 1 && gap[FRAME_LEN - 1] == 0, "{} {}", gap[0], gap[FRAME_LEN - 1]);
    // Audio that resumes after silence starts as it is.
    input.write_playback(4003, &mut [constant_packet(-loud).as_slice()].into_iter());
    assert!(samples(&capture(bus, 4003 + LATENCY)).iter().all(|&s| s == -loud));
    // A step within one continuous stream is the audio itself: untouched.
    input.write_playback(4004, &mut [constant_packet(loud).as_slice()].into_iter());
    assert!(samples(&capture(bus, 4004 + LATENCY)).iter().all(|&s| s == loud));
}

struct TestInput(Frame);
impl InputPort for TestInput {
    fn pull(&self, frame: &mut Frame) {
        *frame = self.0;
    }
}

#[derive(Default)]
struct TestOutput(Mutex<Vec<Frame>>);
impl OutputPort for TestOutput {
    fn push(&self, frame: &Frame) {
        self.0.lock().unwrap().push(*frame);
    }
}

#[test]
fn hardware_strips_and_buses() {
    let engine = Arc::new(Engine::new());
    let output = Arc::new(TestOutput::default());
    let tone: Frame = std::array::from_fn(|i| if i % 2 == 0 { 0.5 } else { -0.25 });
    engine.set_topology(Topology {
        strips: vec![
            StripSpec {
                source: StripSource::Hardware(Some(Arc::new(TestInput(tone)))),
                params: StripParams { pan: -1.0, ..routed(&[0]) },
            },
            StripSpec { source: StripSource::Hardware(None), params: routed(&[0]) },
        ],
        buses: vec![
            BusSpec {
                sink: BusSink::Hardware(Some(Arc::clone(&output) as Arc<dyn OutputPort>)),
                params: BusParams::default(),
            },
            BusSpec { sink: BusSink::Hardware(None), params: BusParams::default() },
        ],
    });
    engine.process_until(500);
    engine.process_until(504);
    let frames = output.0.lock().unwrap().clone();
    assert_eq!(frames.len(), 5, "the first call processes one frame, then every frame up to the last");
    assert!(frames.iter().all(|f| f.as_chunks::<2>().0.iter().all(|p| *p == [0.5, 0.0])), "panned hard left");
    let meter = engine.meters.strips[0].take();
    assert_eq!(meter.peak, [0.5, 0.25]);
    assert_eq!(engine.meters.buses[0].take().peak, [0.5, 0.0]);
}

#[test]
fn catch_up_is_batched_and_bounded() {
    let engine = Arc::new(Engine::new());
    let output = Arc::new(TestOutput::default());
    engine.set_topology(Topology {
        strips: Vec::new(),
        buses: vec![BusSpec {
            sink: BusSink::Hardware(Some(Arc::clone(&output) as Arc<dyn OutputPort>)),
            params: BusParams::default(),
        }],
    });
    engine.process_until(100);
    engine.process_until(115); // a 15.6 ms clock tick
    assert_eq!(engine.stats.max_batch.load(Ordering::Relaxed), 15);
    engine.process_until(110); // already done
    engine.process_until(115 + 1000); // a one-second stall
    assert_eq!(engine.stats.skipped.load(Ordering::Relaxed), 1000 - MAX_CATCH_UP_FRAMES);
    assert_eq!(output.0.lock().unwrap().len() as u64, 1 + 15 + MAX_CATCH_UP_FRAMES);
    assert_eq!(engine.stats.catch_up_batches.load(Ordering::Relaxed), 2);
}

/// The playback feature unit scales the strip and the recording one the bus
/// output the cable returns.
#[test]
fn feature_units_apply_on_their_own_side() {
    let rig = rig(1, 1, &[&[0]]);
    let (input, bus) = (&rig.cables[0], &rig.cables[1]);
    let half = FeatureLevel { mute: false, volume: -6 * 256 };
    input.set_feature_levels(half, FeatureLevel::default());
    input.write_playback(6000, &mut [constant_packet(0x40_0000).as_slice()].into_iter());
    let value = samples(&capture(bus, 6000 + LATENCY))[0] as f32 / 0x40_0000 as f32;
    assert!((value - 0.501).abs() < 0.002, "{value}");
    bus.set_feature_levels(FeatureLevel::default(), FeatureLevel { mute: true, volume: 0 });
    input.write_playback(6001, &mut [constant_packet(0x40_0000).as_slice()].into_iter());
    assert!(samples(&capture(bus, 6001 + LATENCY)).iter().all(|&s| s == 0), "recording mute");
}

#[test]
fn processing_does_not_allocate() {
    let rig = rig(4, 4, &[&[0, 1], &[1, 2], &[2, 3], &[0, 3]]);
    rig.engine.set_strip(0, StripParams { eq: [3.0, -2.0, 1.0], gate: 2.0, comp: 4.0, ..routed(&[0, 1]) });
    rig.engine.set_bus(1, BusParams { limiter: Some(-3.0), mono: true, ..Default::default() });
    rig.engine.process_until(7000); // applies the commands and layout
    let before = crate::alloc_counter::allocations();
    rig.engine.process_until(7100);
    assert_eq!(crate::alloc_counter::allocations() - before, 0);
}

#[test]
fn stray_frame_bytes_constant() {
    assert_eq!(FRAME_BYTES * FRAME_SAMPLES, PACKET_BYTES);
    let _unused = AtomicBool::new(false);
}
