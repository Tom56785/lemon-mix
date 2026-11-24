//! A real streamer's setup, read and played through the whole mixer:
//!
//! - The microphone (H1) is compressed, gated and denoised, then sent to the
//!   stream through a bus return: Mic PT (bus A3, read back by strip H3).
//! - Desktop audio (V4) and Discord (V5) play locally (A1) and on the stream
//!   through Audio PT (bus A4, strip H4), which is ducked while the
//!   microphone is live.
//! - Playback (V6) is for a media player or soundboard: on the stream through
//!   Playback PT (bus A2, strip H2), not heard locally.
//! - The stream goes out on three recording devices: Stream Audio (B1),
//!   Stream Mic (B2) and Stream Audio and Mic (B3), which the muted Monitor
//!   strip (H5) can play locally.
//!
//! The configuration is the one written for the app, in `streaming/`, read
//! from memory. Nothing touches real devices: the microphone and headphones
//! are stand-ins, and the virtual devices are driven packet by packet, as
//! Windows drives them.

use std::sync::{Arc, Mutex};

use cables::audio::{PACKET_BYTES, decode_int24, encode_int24};
use cables::uac2::{REQUEST_SET_CONFIGURATION, REQUEST_SET_INTERFACE, SetupPacket};
use engine::{FRAME_LEN, Frame, InputPort, OutputPort, db_to_gain};

use super::*;
use crate::config::{
    config_cables, config_mixer, config_profile, config_string, hardware_counts, parse_config, set_cable_entries,
    set_mixer_entries,
};

const CONFIG: &str = include_str!("streaming/config.toml");
const PROFILE: &str = include_str!("streaming/Streaming.toml");

/// The microphone: a 1 kHz square wave at a level that can be changed.
struct Mic(Arc<Mutex<f32>>);

impl InputPort for Mic {
    fn pull(&self, frame: &mut Frame) {
        let level = *self.0.lock().unwrap();
        for (i, pair) in frame.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            pair.fill(if i < 24 { level } else { -level });
        }
    }
}

/// The headphones: keeps the last sample they were given.
#[derive(Default)]
struct Headphones(Mutex<f32>);

impl OutputPort for Headphones {
    fn push(&self, frame: &Frame) {
        *self.0.lock().unwrap() = frame[FRAME_LEN - 2];
    }
}

/// The setup, running, with its devices and stand-ins.
struct Streaming {
    host: Arc<MixerHost>,
    cables: Vec<Cable>,
    /// The virtual devices, by name.
    devices: HashMap<String, Arc<Device>>,
    mic: Arc<Mutex<f32>>,
    headphones: Arc<Headphones>,
    /// The next bus frame to play.
    frame: u64,
}

impl Streaming {
    fn new() -> Streaming {
        let config = parse_config(CONFIG).unwrap();
        let cables = config_cables(&config);
        let profile = parse_config(PROFILE).unwrap();
        let (hardware_inputs, hardware_outputs) = hardware_counts(&profile);
        let settings = HostSettings {
            address: "127.0.0.1:0".into(),
            latency_ms: 20,
            stall_test: None,
            diagnostics: false,
            buffer_ms: 20,
            hardware_inputs,
            hardware_outputs,
            profile: "Streaming".into(),
            solo_bus: config_string(&config, "solo_bus", crate::config::DEFAULT_SOLO_BUS),
        };
        let host = MixerHost::new(settings, config_mixer(&profile), &Logger::discard(), "", false);
        let mic = Arc::new(Mutex::new(0.0));
        let headphones = Arc::new(Headphones::default());
        host.stand_in("H1", Some(Arc::new(Mic(Arc::clone(&mic)))), None);
        host.stand_in("A1", None, Some(Arc::clone(&headphones) as Arc<dyn OutputPort>));
        let mut state = lock(&host.server);
        let made = host.make_devices(&mut state, &cables).unwrap();
        drop(state);
        host.rebuild(&cables);
        // Windows opens each device's streams.
        for device in &made {
            device.handle_control(
                SetupPacket { request: REQUEST_SET_CONFIGURATION, value: 1, ..Default::default() },
                &[],
            );
            for iface in [1, 2] {
                let setup = SetupPacket {
                    request_type: 0x01,
                    request: REQUEST_SET_INTERFACE,
                    index: iface,
                    value: 1,
                    length: 0,
                };
                device.handle_control(setup, &[]);
            }
        }
        let devices = cables.iter().map(|c| c.name.clone()).zip(made).collect();
        Streaming { host, cables, devices, mic, headphones, frame: 50_000 }
    }

    /// Plays `ms` milliseconds: each playback device in `playing` at its
    /// level (the others silent), and the microphone as it is set.
    fn play(&mut self, ms: u64, playing: &[(&str, f32)]) {
        let packets: Vec<(Arc<Device>, Vec<u8>)> =
            playing.iter().map(|&(name, level)| (Arc::clone(&self.devices[name]), packet(level))).collect();
        for _ in 0..ms {
            for (device, packet) in &packets {
                device.write_playback(self.frame, [packet.as_slice()]);
            }
            // Reading a recording device processes the mixer up to its frame.
            self.level("Stream Audio");
            self.frame += 1;
        }
    }

    /// The level a recording device has now: its last sample, left.
    fn level(&self, name: &str) -> f32 {
        let mut out = vec![0u8; PACKET_BYTES];
        self.devices[name].read_capture(self.frame, &mut out, [PACKET_BYTES]);
        let last = &out[PACKET_BYTES - 6..PACKET_BYTES - 3];
        decode_int24(last[0], last[1], last[2]) as f32 / FULL_SCALE
    }

    /// Plays as `play` does, watching recording device `name`: returns the
    /// quietest and loudest it was, as magnitudes.
    fn watch(&mut self, name: &str, ms: u64, playing: &[(&str, f32)]) -> (f32, f32) {
        let (mut quietest, mut loudest) = (f32::MAX, 0.0f32);
        for _ in 0..ms {
            self.play(1, playing);
            let level = self.level(name).abs();
            (quietest, loudest) = (quietest.min(level), loudest.max(level));
        }
        (quietest, loudest)
    }

    fn headphones(&self) -> f32 {
        *self.headphones.0.lock().unwrap()
    }

    fn set_mic(&self, level: f32) {
        *self.mic.lock().unwrap() = level;
    }
}

const FULL_SCALE: f32 = (1 << 23) as f32;

/// A packet whose every sample is `level`.
fn packet(level: f32) -> Vec<u8> {
    let mut packet = vec![0u8; PACKET_BYTES];
    for sample in packet.as_chunks_mut::<3>().0 {
        encode_int24(sample, (level * FULL_SCALE) as i32);
    }
    packet
}

fn near(value: f32, expected: f32) -> bool {
    (value - expected).abs() < 2e-3
}

/// The setup reads as written, writes back the same, and the mixer takes it
/// as it is: every strip, bus, route, bus return and setting.
#[test]
fn the_streaming_setup_reads_as_written() {
    let config = parse_config(CONFIG).unwrap();
    assert_eq!(config_profile(&config), "Streaming");
    assert_eq!(config_string(&config, "solo_bus", ""), "A1", "the headphones play soloed buses");
    let cables = config_cables(&config);
    let summary: Vec<(u32, &str, CableKind)> = cables.iter().map(|c| (c.id, c.name.as_str(), c.kind)).collect();
    assert_eq!(
        summary,
        [
            (1, "Stream Audio", CableKind::Output),
            (2, "Stream Mic", CableKind::Output),
            (3, "Stream Audio and Mic", CableKind::Output),
            (4, "Desktop Audio", CableKind::Input),
            (5, "Discord", CableKind::Input),
            (6, "Playback", CableKind::Input),
        ]
    );
    assert_eq!(config_cables(&parse_config(&set_cable_entries(CONFIG, &cables).unwrap()).unwrap()), cables);

    let profile = parse_config(PROFILE).unwrap();
    assert_eq!(hardware_counts(&profile), (5, 4), "H1 to H5 and A1 to A4");
    let saved = config_mixer(&profile);
    let rewritten = config_mixer(&parse_config(&set_mixer_entries(PROFILE, &saved).unwrap()).unwrap());
    assert_eq!(rewritten, saved, "written back, the same");
    assert_eq!(feedback_loop(&saved), None);

    // The mixer takes it as it is: nothing dropped, renumbered or forgotten.
    let streaming = Streaming::new();
    assert_eq!(streaming.host.saved_mixer(), saved);
    let (strips, buses) = streaming.host.views();
    let source = |port: &Option<PortView>| match port {
        Some(port) if port.state == "bus" => format!("bus {}", port.device_name),
        Some(port) => port.endpoint.clone(),
        None => "cable".into(),
    };
    let strips: Vec<String> = strips
        .iter()
        .map(|s| format!("{} {} <- {} -> {:?} ducked by {:?}", s.id, s.label, source(&s.port), s.routes, s.duck_by))
        .collect();
    assert_eq!(
        strips,
        [
            r#"H1 Mic <- {0.0.1.00000000}.{259a614a-f8bc-4dff-bd01-cdc6548de0ea} -> ["A3"] ducked by []"#,
            r#"H2 Playback PT <- bus Playback PT -> ["B1", "B3"] ducked by []"#,
            r#"H3 Mic PT <- bus Mic PT -> ["B2", "B3"] ducked by []"#,
            r#"H4 Audio PT <- bus Audio PT -> ["B1", "B3"] ducked by ["H3"]"#,
            r#"H5 Monitor <- bus Audio + Mic -> ["A1"] ducked by []"#,
            r#"V4 Audio <- cable -> ["A1", "A4"] ducked by []"#,
            r#"V5 Discord <- cable -> ["A1", "A4"] ducked by []"#,
            r#"V6 Playback <- cable -> ["A2"] ducked by []"#,
        ]
    );
    let buses: Vec<String> = buses.iter().map(|b| format!("{} {} -> {}", b.id, b.label, source(&b.port))).collect();
    assert_eq!(
        buses,
        [
            "A1 Local -> {0.0.0.00000000}.{5d220d7e-1b85-4e37-90d3-0dbbbda1f20b}",
            "A2 Playback PT -> ",
            "A3 Mic PT -> ",
            "A4 Audio PT -> ",
            "B1 Audio -> cable",
            "B2 Mic -> cable",
            "B3 Audio + Mic -> cable",
        ]
    );

    // The microphone's processing, and the ducking.
    let strip = |id: &str| saved.strips.iter().find(|s| s.id == id).unwrap().params.clone();
    let mic = strip("H1");
    assert_eq!((mic.gate, mic.comp, mic.denoise), (5.6, 5.0, 3.0));
    assert_eq!(engine::dsp::gate_threshold_db(mic.gate), -42.0, "the gate opens at -42 dB");
    let comp = mic.comp_options;
    assert!(comp.manual && comp.threshold_db == -18.0 && comp.ratio == 2.47 && comp.makeup_db == Some(0.0));
    assert_eq!((comp.attack_ms, comp.release_ms), (10.0, 100.0));
    let gate = mic.gate_options;
    assert_eq!((gate.range_db, gate.attack_ms, gate.hold_ms, gate.release_ms), (60.0, 10.0, 200.0, 300.0));
    let duck = strip("H4").duck;
    assert_eq!(
        (duck.amount_db, duck.threshold_db, duck.fade_down_ms, duck.hold_ms, duck.fade_up_ms),
        (8.0, -20.0, 400.0, 1500.0, 600.0)
    );
    assert!(strip("H5").mute, "the monitor starts muted");
    assert_eq!(streaming.cables.len(), 6);
}

/// The setup mixes as it should: each source reaches the right
/// devices, the stream's music ducks while the microphone is live and comes
/// back after the hold, and the monitor plays the whole stream locally.
#[test]
fn the_streaming_setup_mixes_each_source_where_it_belongs() {
    let mut s = Streaming::new();
    let desktop = ("Desktop Audio", 0.4);

    // Desktop audio: in the headphones and on the stream, not on the
    // microphone's device. It reaches the stream through Audio PT's bus
    // return, a millisecond later, unchanged.
    s.play(300, &[desktop]);
    assert!(near(s.headphones(), 0.4), "local: {}", s.headphones());
    assert!(near(s.level("Stream Audio"), 0.4), "{}", s.level("Stream Audio"));
    assert!(near(s.level("Stream Audio and Mic"), 0.4));
    assert_eq!(s.level("Stream Mic"), 0.0);

    // Discord joins it everywhere it goes; the playback deck only on the
    // stream, through Playback PT.
    s.play(300, &[desktop, ("Discord", 0.1), ("Playback", 0.2)]);
    assert!(near(s.headphones(), 0.5), "local: no playback: {}", s.headphones());
    assert!(near(s.level("Stream Audio"), 0.7), "{}", s.level("Stream Audio"));
    assert!(near(s.level("Stream Audio and Mic"), 0.7));
    assert_eq!(s.level("Stream Mic"), 0.0);

    // The microphone goes live: heard on its stream devices, not locally,
    // and the stream's music fades 8 dB down over 400 ms.
    let ducked = 0.4 * db_to_gain(-8.0);
    s.set_mic(0.8);
    s.play(250, &[desktop]);
    let fading = s.level("Stream Audio");
    assert!(fading > ducked + 0.02 && fading < 0.38, "part way down after 250 ms: {fading}");
    s.play(450, &[desktop]);
    assert!(near(s.level("Stream Audio"), ducked), "ducked: {}", s.level("Stream Audio"));
    assert!(s.level("Stream Mic").abs() > 0.05, "the microphone is on the stream: {}", s.level("Stream Mic"));
    assert!(near(s.headphones(), 0.4), "not ducked locally, nor the microphone heard: {}", s.headphones());

    // It stops: the music stays down for the 1.5 s hold, then fades back
    // over 600 ms.
    s.set_mic(0.0);
    s.play(1200, &[desktop]);
    assert!(near(s.level("Stream Audio"), ducked), "held: {}", s.level("Stream Audio"));
    s.play(650, &[desktop]);
    let rising = s.level("Stream Audio");
    assert!(rising > ducked + 0.02 && rising < 0.38, "part way back: {rising}");
    s.play(400, &[desktop]);
    assert!(near(s.level("Stream Audio"), 0.4), "back: {}", s.level("Stream Audio"));
    assert_eq!(s.level("Stream Mic"), 0.0, "the gate and denoiser leave nothing behind");

    // A microphone below the gate's -42 dB threshold (after the denoiser,
    // which comes first) is not on the stream: the gate holds it 60 dB down.
    // Nor does it duck the music.
    s.set_mic(db_to_gain(-50.0));
    let (_, mic) = s.watch("Stream Mic", 500, &[desktop]);
    assert!(mic < db_to_gain(-90.0), "below the gate: {mic} ({:.0} dB)", 20.0 * mic.log10());
    let (_, mic) = s.watch("Stream Audio and Mic", 1, &[desktop]);
    assert!(near(mic, 0.4), "the stream with the microphone carries only the music: {mic}");
    let (quietest, _) = s.watch("Stream Audio", 200, &[desktop]);
    assert!(near(quietest, 0.4), "the music is never ducked: {quietest}");

    // A quiet voice above the threshold is on the stream. At -24 dBFS it
    // reaches the gate at about -36 dB (the denoiser takes a steady tone for
    // noise and passes a quarter of it), between this gate's threshold and
    // one set too high, so this also shows where the threshold is. It is too
    // quiet to duck the music.
    s.set_mic(db_to_gain(-24.0));
    let (_, mic) = s.watch("Stream Mic", 300, &[desktop]);
    assert!(mic > db_to_gain(-50.0), "above the gate: {mic} ({:.0} dB)", 20.0 * mic.log10());
    let (quietest, _) = s.watch("Stream Audio", 1, &[desktop]);
    assert!(near(quietest, 0.4), "not ducked: {quietest}");
    s.set_mic(0.0);
    s.play(100, &[desktop]);

    // Soloing Stream Audio and Mic (B3), as `--control solo B3` does, plays
    // the stream on the headphones (A1, the solo bus) instead of the local
    // mix: the playback deck, which the local mix leaves out, is heard.
    s.host.control(&ControlAction::Solo { target: "B3".into(), on: Some(true) }).unwrap();
    s.play(100, &[desktop, ("Playback", 0.2)]);
    assert!(near(s.headphones(), 0.6), "the stream: {}", s.headphones());
    assert!(near(s.level("Stream Audio and Mic"), 0.6), "the stream plays on as ever");
    s.host.control(&ControlAction::Solo { target: "B3".into(), on: Some(false) }).unwrap();
    s.play(100, &[desktop]);
    assert!(near(s.headphones(), 0.4), "the local mix again: {}", s.headphones());

    // Unmuting the monitor plays the whole stream (Stream Audio and Mic)
    // locally too.
    let (strips, _) = s.host.views();
    let mut monitor = strips.into_iter().find(|s| s.label == "Monitor").unwrap().params;
    monitor.mute = false;
    s.host.command(&MixerCommand::Strip { id: "H5".into(), params: monitor }).unwrap();
    s.play(100, &[desktop]);
    assert!(near(s.headphones(), 0.8), "local plus the stream: {}", s.headphones());
}
