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
            params: StripParams::default(),
            routes: routes.get(i).copied().unwrap_or(&[]).to_vec(),
            duck_sources: Vec::new(),
            identity: 0,
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
            params: StripParams::default(),
            routes: vec![0],
            duck_sources: Vec::new(),
            identity: 0,
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
    rig.engine.set_strip(1, StripParams { solo: true, ..Default::default() });
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
    rig.engine.set_strip(0, StripParams { gain_db: -6.0, ..Default::default() });
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
                params: StripParams { pan: -1.0, ..Default::default() },
                routes: vec![0],
                duck_sources: Vec::new(),
                identity: 0,
            },
            StripSpec {
                source: StripSource::Hardware(None),
                params: StripParams::default(),
                routes: vec![0],
                duck_sources: Vec::new(),
                identity: 0,
            },
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
    let meter = engine.meters().strips[0].take();
    assert_eq!(meter.peak, [0.5, 0.25]);
    assert_eq!(engine.meters().buses[0].take().peak, [0.5, 0.0]);
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
    rig.engine
        .set_strip(0, StripParams { denoise: 6.0, eq: [3.0, -2.0, 1.0], gate: 2.0, comp: 4.0, ..Default::default() });
    rig.engine.set_bus(1, BusParams { limiter: Some(-3.0), mono: true, ..Default::default() });
    // Ducking too: strip 0 lowered while strip 1 plays.
    rig.engine.set_duck_source(0, 1, true);
    // As the clock thread does when it starts.
    crate::denoise::prepare_thread();
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

/// There is no fixed number of strips or buses: a strip reaches a bus far
/// past 64, routes change by command, and every strip and bus has a meter.
#[test]
fn any_number_of_strips_and_buses() {
    let rig = rig(1, 100, &[&[]]);
    let (input, far) = (&rig.cables[0], &rig.cables[91]);
    let value = 0x12_3456;
    input.write_playback(8000, &mut [constant_packet(value).as_slice()].into_iter());
    assert!(samples(&capture(far, 8000 + LATENCY)).iter().all(|&s| s == 0), "not routed yet");
    rig.engine.set_route(0, 90, true);
    for frame in 8001..8003 {
        input.write_playback(frame, &mut [constant_packet(value).as_slice()].into_iter());
    }
    capture(far, 8001 + LATENCY); // the send fades in across this frame
    assert!(samples(&capture(far, 8002 + LATENCY)).iter().all(|&s| s == value));
    let meters = rig.engine.meters();
    assert_eq!((meters.strips.len(), meters.buses.len()), (1, 100));
    assert!(meters.buses[90].take().peak[0] > 0.0);
    assert_eq!(meters.buses[89].take().peak, [0.0, 0.0]);
}

/// A strip fed from a bus hears that bus's output one frame later, after
/// the bus's own fader, without a virtual cable in between.
#[test]
fn a_bus_feeds_a_strip_one_frame_later() {
    let engine = Arc::new(Engine::new());
    let output = Arc::new(TestOutput::default());
    let tone: Frame = std::array::from_fn(|i| if i % 2 == 0 { 0.5 } else { 0.25 });
    engine.set_topology(Topology {
        strips: vec![
            StripSpec {
                source: StripSource::Hardware(Some(Arc::new(TestInput(tone)))),
                params: StripParams::default(),
                routes: vec![0],
                duck_sources: Vec::new(),
                identity: 0,
            },
            // Bus 0 comes back in here, and goes on to bus 1.
            StripSpec {
                source: StripSource::Bus(0),
                params: StripParams::default(),
                routes: vec![1],
                duck_sources: Vec::new(),
                identity: 0,
            },
        ],
        buses: vec![
            BusSpec { sink: BusSink::Hardware(None), params: BusParams { gain_db: -6.0, ..Default::default() } },
            BusSpec {
                sink: BusSink::Hardware(Some(Arc::clone(&output) as Arc<dyn OutputPort>)),
                params: BusParams::default(),
            },
        ],
    });
    engine.process_until(900);
    engine.process_until(902);
    let frames = output.0.lock().unwrap().clone();
    assert_eq!(frames.len(), 3);
    assert!(frames[0].iter().all(|&s| s == 0.0), "nothing has left bus 0 yet");
    let half = db_to_gain(-6.0);
    for frame in &frames[1..] {
        for pair in frame.as_chunks::<2>().0 {
            assert!((pair[0] - 0.5 * half).abs() < 1e-6 && (pair[1] - 0.25 * half).abs() < 1e-6, "{pair:?}");
        }
    }
}

/// A hardware input whose level can be changed while the engine runs.
struct SwitchedInput(Arc<Mutex<f32>>);
impl InputPort for SwitchedInput {
    fn pull(&self, frame: &mut Frame) {
        *frame = [*self.0.lock().unwrap(); FRAME_LEN];
    }
}

/// Music (strip 0, a steady 0.5) and a microphone (strip 1, switched)
/// that ducks it, both on one hardware bus; and the microphone's level.
fn ducking_rig(mic_routed: bool) -> (Arc<Engine>, Arc<TestOutput>, Arc<Mutex<f32>>) {
    let engine = Arc::new(Engine::new());
    let output = Arc::new(TestOutput::default());
    let mic = Arc::new(Mutex::new(0.0f32));
    engine.set_topology(Topology {
        strips: vec![
            StripSpec {
                source: StripSource::Hardware(Some(Arc::new(TestInput([0.5; FRAME_LEN])))),
                params: StripParams::default(),
                routes: vec![0],
                duck_sources: vec![1],
                identity: 1,
            },
            StripSpec {
                source: StripSource::Hardware(Some(Arc::new(SwitchedInput(Arc::clone(&mic))))),
                params: StripParams::default(),
                routes: if mic_routed { vec![0] } else { Vec::new() },
                duck_sources: Vec::new(),
                identity: 0,
            },
        ],
        buses: vec![BusSpec {
            sink: BusSink::Hardware(Some(Arc::clone(&output) as Arc<dyn OutputPort>)),
            params: BusParams::default(),
        }],
    });
    (engine, output, mic)
}

/// The last sample the bus put out.
fn last_out(output: &TestOutput) -> f32 {
    output.0.lock().unwrap().last().map_or(0.0, |f| f[FRAME_LEN - 1])
}

/// Music is lowered by the amount while the microphone plays, held, then
/// brought back, smoothly; the meter shows how far.
#[test]
fn ducking_lowers_a_strip_while_its_source_plays() {
    let (engine, output, mic) = ducking_rig(false);
    let duck = crate::DuckParams::default(); // 12 dB, -40 dBFS, 10 ms, 300 ms, 500 ms
    let mut frame = 20_000;
    // In steps, as the clock does: one call processes at most 250 frames.
    let run = |frames: u64, frame: &mut u64| {
        for _ in 0..frames.div_ceil(50) {
            *frame += 50;
            engine.process_until(*frame);
        }
    };
    run(50, &mut frame);
    assert_eq!(last_out(&output), 0.5, "the microphone is silent: no ducking");
    assert_eq!(engine.meters().strips[0].take().duck, 1.0);

    *mic.lock().unwrap() = 0.3; // well above -40 dBFS
    run(100, &mut frame);
    let floor = 0.5 * db_to_gain(-duck.amount_db);
    assert!((last_out(&output) - floor).abs() < 0.002, "down by 12 dB: {}", last_out(&output));
    assert!(engine.meters().strips[0].take().duck < 0.3, "the meter shows the ducking");
    assert_eq!(engine.meters().strips[1].take().duck, 1.0, "the microphone itself is not ducked");

    // Quiet again: held for 300 ms, then back up over about 500 ms.
    *mic.lock().unwrap() = 0.0;
    run(250, &mut frame);
    assert!((last_out(&output) - floor).abs() < 0.002, "still held: {}", last_out(&output));
    run(6000, &mut frame); // the hold, then twelve release time constants
    assert!((last_out(&output) - 0.5).abs() < 1e-4, "back up: {}", last_out(&output));

    // No sample-to-sample step bigger than a smooth ramp would make.
    let frames = output.0.lock().unwrap().clone();
    let samples: Vec<f32> = frames.iter().flat_map(|f| f.iter().step_by(2).copied()).collect();
    let largest = samples.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
    assert!(largest < 0.01, "a click: a step of {largest}");
}

/// A quiet, muted or self-pointing source ducks nothing; a source switched
/// on while the engine runs does.
#[test]
fn ducking_follows_what_is_heard() {
    // In steps, as the clock does: one call processes at most 250 frames.
    let mut frame = 30_000;
    let mut run = |engine: &Engine, frames: u64| {
        for _ in 0..frames.div_ceil(50) {
            frame += 50;
            engine.process_until(frame);
        }
    };
    // Below the threshold.
    let (engine, output, mic) = ducking_rig(true);
    *mic.lock().unwrap() = 0.005; // -46 dBFS
    run(&engine, 200);
    assert!((last_out(&output) - 0.505).abs() < 1e-4, "not ducked: {}", last_out(&output));

    // Muted: heard by no one, so it ducks no one.
    *mic.lock().unwrap() = 0.3;
    engine.set_strip(1, StripParams { mute: true, ..Default::default() });
    run(&engine, 200);
    assert!((last_out(&output) - 0.5).abs() < 1e-4, "a muted source: {}", last_out(&output));

    // Unmuted, it ducks the music; the microphone's own share is not lowered.
    engine.set_strip(1, StripParams::default());
    run(&engine, 200);
    let expected = 0.5 * db_to_gain(-12.0) + 0.3;
    assert!((last_out(&output) - expected).abs() < 0.002, "ducked music under the mic: {}", last_out(&output));

    // Taking the source away by command brings the music back.
    engine.set_duck_source(0, 1, false);
    run(&engine, 3400);
    assert!((last_out(&output) - 0.8).abs() < 1e-3, "{}", last_out(&output));

    // A strip cannot duck itself.
    engine.set_duck_source(1, 1, true);
    run(&engine, 400);
    assert!((last_out(&output) - 0.8).abs() < 1e-3, "{}", last_out(&output));
    assert_eq!(engine.meters().strips[1].take().duck, 1.0);
}

/// A new layout (a device plugged in, a strip added) does not undo ducking:
/// a strip that carries on keeps its moment, and is never heard jumping up.
#[test]
fn a_new_layout_keeps_a_strip_ducked() {
    let engine = Arc::new(Engine::new());
    let output = Arc::new(TestOutput::default());
    let mic = Arc::new(Mutex::new(0.3f32));
    let topology = |identities: [u64; 2]| Topology {
        strips: vec![
            StripSpec {
                source: StripSource::Hardware(Some(Arc::new(TestInput([0.5; FRAME_LEN])))),
                params: StripParams::default(),
                routes: vec![0],
                duck_sources: vec![1],
                identity: identities[0],
            },
            StripSpec {
                source: StripSource::Hardware(Some(Arc::new(SwitchedInput(Arc::clone(&mic))))),
                params: StripParams::default(),
                routes: Vec::new(),
                duck_sources: Vec::new(),
                identity: identities[1],
            },
        ],
        buses: vec![BusSpec {
            sink: BusSink::Hardware(Some(Arc::clone(&output) as Arc<dyn OutputPort>)),
            params: BusParams::default(),
        }],
    };
    let mut frame = 40_000;
    let mut run = |frames: u64| {
        for _ in 0..frames.div_ceil(50) {
            frame += 50;
            engine.process_until(frame);
        }
    };
    engine.set_topology(topology([7, 8]));
    run(200);
    let floor = 0.5 * db_to_gain(-12.0);
    assert!((last_out(&output) - floor).abs() < 0.002, "ducked: {}", last_out(&output));

    output.0.lock().unwrap().clear();
    engine.set_topology(topology([7, 8]));
    run(100);
    let loudest = output.0.lock().unwrap().iter().flatten().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(loudest < floor + 0.002, "it jumped up to {loudest} in the new layout");

    // Strips the engine cannot tell apart (identity 0) start afresh, as before.
    output.0.lock().unwrap().clear();
    engine.set_topology(topology([0, 0]));
    run(1);
    let first = output.0.lock().unwrap().first().map_or(0.0, |f| f[FRAME_LEN - 1]);
    assert!((first - 0.5).abs() < 1e-3, "a fresh strip starts unducked: {first}");
}
