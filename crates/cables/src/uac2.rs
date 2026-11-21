//! A full-speed USB Audio Class 2.0 cable: its descriptors, control requests
//! and the delay line that loops playback back to capture.
//!
//! The cable is a UAC2 device so that Windows binds its in-box usbaudio2.sys,
//! a WaveRT driver that refills playback URBs from the audio engine's shared
//! buffer in its completion routine. For UAC1 devices Windows uses
//! usbaudio.sys, whose playback waits on buffers from the audio engine in user
//! mode: under system load it stops sending for tens of milliseconds, empties
//! the pipe and falls into resetting it after every URB.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::audio::{DelayLine, Gain, LineStatus, PACKET_BYTES};

/// Which audio endpoints a cable has in Windows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CableKind {
    /// A playback endpoint only: a speaker that applications play into.
    Input,
    /// A recording endpoint only: a microphone that applications record from.
    Output,
    /// Both endpoints, the recording side returning what is played to the
    /// playback side (the original virtual cable).
    #[default]
    Loopback,
}

impl CableKind {
    pub const ALL: [CableKind; 3] = [CableKind::Input, CableKind::Output, CableKind::Loopback];

    /// The name used in config.toml and on the pipe.
    pub fn name(self) -> &'static str {
        match self {
            CableKind::Input => "input",
            CableKind::Output => "output",
            CableKind::Loopback => "loopback",
        }
    }

    pub fn from_name(name: &str) -> Option<CableKind> {
        CableKind::ALL.into_iter().find(|kind| kind.name().eq_ignore_ascii_case(name.trim()))
    }

    /// The base of this kind's USB product IDs. Each kind has its own range,
    /// so a cable whose kind changes enumerates as a new device: Windows keeps
    /// the audio endpoints it found under a hardware ID and serial number.
    pub fn product_id_base(self) -> u32 {
        match self {
            CableKind::Loopback => PRODUCT_ID_BASE,
            CableKind::Input => INPUT_PRODUCT_ID_BASE,
            CableKind::Output => OUTPUT_PRODUCT_ID_BASE,
        }
    }

    pub fn has_playback(self) -> bool {
        self != CableKind::Output
    }

    pub fn has_recording(self) -> bool {
        self != CableKind::Input
    }

    /// The AudioStreaming interface numbers of the playback and recording
    /// sides. Interface numbers are contiguous from the AudioControl
    /// interface 0; the endpoint addresses stay 0x01 and 0x82 for every kind.
    pub fn streaming_interfaces(self) -> (Option<u8>, Option<u8>) {
        match self {
            CableKind::Loopback => (Some(1), Some(2)),
            CableKind::Input => (Some(1), None),
            CableKind::Output => (None, Some(1)),
        }
    }

    /// AudioControl plus one AudioStreaming interface per side.
    pub fn interface_count(self) -> u8 {
        1 + self.has_playback() as u8 + self.has_recording() as u8
    }
}

/// A feature unit's mute and volume, as Windows set them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeatureLevel {
    pub mute: bool,
    /// In 1/256 dB; `VOLUME_SILENT` (i16::MIN) is minus infinity.
    pub volume: i16,
}

impl FeatureLevel {
    /// The gain of this feature unit alone.
    pub fn gain(self) -> Gain {
        combined_gain(&[self])
    }
}

/// The gain of feature units in series.
pub fn combined_gain(levels: &[FeatureLevel]) -> Gain {
    if levels.iter().any(|f| f.mute || f.volume == VOLUME_SILENT) {
        return Gain::Scale(0.0);
    }
    let volume: i32 = levels.iter().map(|f| f.volume as i32).sum();
    if volume == 0 { Gain::Unity } else { Gain::Scale(10f32.powf(volume as f32 / 256.0 / 20.0)) }
}

/// What a cable does with its audio. The device calls it from the USB/IP
/// endpoint threads: playback packets as their URBs arrive, capture packets
/// as their URBs complete, both addressed by bus frame.
pub trait CableAudio: Send + Sync {
    /// Stores playback packets scheduled on frames `start`, `start + 1`, ...
    /// An empty packet carries no audio for its frame.
    fn write_playback(&self, start: u64, packets: &mut dyn Iterator<Item = &[u8]>);

    /// Fills capture packets for frames `start`, `start + 1`, ..., laid out
    /// back to back in `out` with the given lengths. `playback_active` says
    /// whether the cable's own playback side is streaming.
    fn read_capture(&self, start: u64, out: &mut [u8], lengths: &mut dyn Iterator<Item = usize>, playback_active: bool);

    /// The playback stream has stopped.
    fn end_playback(&self);

    /// The capture stream has stopped.
    fn end_capture(&self);

    /// The playback and recording interfaces were selected or deselected.
    fn streaming_changed(&self, _playback: bool, _capture: bool) {}

    /// Windows changed the playback or recording feature unit.
    fn set_feature_levels(&self, playback: FeatureLevel, recording: FeatureLevel);

    /// Counters for the log.
    fn status(&self) -> LineStatus;

    /// The latency in frames between a cable's playback and recording sides.
    fn delay(&self) -> u64;
}

/// On its own, a cable loops its playback side back to its recording side,
/// with both feature units applied in series.
impl CableAudio for DelayLine {
    fn write_playback(&self, start: u64, packets: &mut dyn Iterator<Item = &[u8]>) {
        self.write(start, packets);
    }

    fn read_capture(
        &self,
        start: u64,
        out: &mut [u8],
        lengths: &mut dyn Iterator<Item = usize>,
        playback_active: bool,
    ) {
        self.read(start, out, lengths, playback_active);
    }

    fn end_playback(&self) {
        DelayLine::end_playback(self);
    }

    fn end_capture(&self) {
        DelayLine::end_capture(self);
    }

    fn set_feature_levels(&self, playback: FeatureLevel, recording: FeatureLevel) {
        self.set_gain(combined_gain(&[playback, recording]));
    }

    fn status(&self) -> LineStatus {
        DelayLine::status(self)
    }

    fn delay(&self) -> u64 {
        DelayLine::delay(self)
    }
}

pub const DESCRIPTOR_DEVICE: u8 = 0x01;
pub const DESCRIPTOR_CONFIGURATION: u8 = 0x02;
pub const DESCRIPTOR_STRING: u8 = 0x03;
pub const DESCRIPTOR_INTERFACE: u8 = 0x04;
pub const DESCRIPTOR_ENDPOINT: u8 = 0x05;
pub const DESCRIPTOR_INTERFACE_ASSOCIATION: u8 = 0x0B;
pub const DESCRIPTOR_CS_INTERFACE: u8 = 0x24;
pub const DESCRIPTOR_CS_ENDPOINT: u8 = 0x25;

/// The cables' USB product IDs are this plus the cable ID. The UAC1 cables used
/// 0xCA00: a new range makes Windows install the UAC2 driver afresh rather than
/// keep the UAC1 driver bound to devices it already knows.
pub const PRODUCT_ID_BASE: u32 = 0xCB00;
/// The product IDs of input-only and output-only cables.
pub const INPUT_PRODUCT_ID_BASE: u32 = 0xCC00;
pub const OUTPUT_PRODUCT_ID_BASE: u32 = 0xCD00;

/// The manufacturer string of the original cables.
pub const DEFAULT_MANUFACTURER: &str = "Virtual Cables";

/// The audio function's interface protocol: USB Audio 2.0.
const IP_VERSION_02_00: u8 = 0x20;

/// Entity IDs in the audio function's topology. Playback: USB streaming input
/// terminal, feature unit, speaker. Recording: microphone, feature unit, USB
/// streaming output terminal. Every terminal runs on the one clock source.
const CLOCK_SOURCE: u8 = 9;
const PLAYBACK_INPUT_TERMINAL: u8 = 1;
const PLAYBACK_FEATURE_UNIT: u8 = 2;
const PLAYBACK_OUTPUT_TERMINAL: u8 = 3;
const RECORDING_INPUT_TERMINAL: u8 = 4;
const RECORDING_FEATURE_UNIT: u8 = 5;
const RECORDING_OUTPUT_TERMINAL: u8 = 6;

/// The cable's only sample rate.
pub const SAMPLE_RATE: u32 = 48000;

/// Describes one full-speed USB Audio Class 2.0 cable.
pub struct Descriptors {
    pub cable_number: u32,
    pub kind: CableKind,
    pub product: String,
    pub serial: String,
    pub device: Vec<u8>,
    pub config: Vec<u8>,
    pub strings: HashMap<u8, Vec<u8>>,
}

impl Descriptors {
    /// Cable `cable_number` with its default name and serial number.
    #[cfg(test)]
    pub fn new(cable_number: u32) -> Result<Descriptors, String> {
        let name = format!("Virtual Cable {cable_number:02}");
        Descriptors::named(cable_number, &format!("VCABLE-{cable_number:03}"), &name)
    }

    /// Descriptors for loopback cable `cable_number`, whose product string,
    /// the name Windows Sound shows, is `name`. The number sets the product
    /// ID; with the serial number it identifies the device to Windows.
    pub fn named(cable_number: u32, serial: &str, name: &str) -> Result<Descriptors, String> {
        Descriptors::build(CableKind::Loopback, cable_number, serial, name, DEFAULT_MANUFACTURER)
    }

    /// Descriptors for a cable of any kind, with its manufacturer string.
    pub fn build(
        kind: CableKind,
        cable_number: u32,
        serial: &str,
        name: &str,
        manufacturer: &str,
    ) -> Result<Descriptors, String> {
        if !(1..=255).contains(&cable_number) {
            return Err("cable number must be 1..255".into());
        }
        let product = name.to_string();
        let serial = serial.to_string();
        let mut strings = HashMap::new();
        strings.insert(0, vec![4, DESCRIPTOR_STRING, 0x09, 0x04]); // en-US
        strings.insert(1, string_descriptor(manufacturer));
        strings.insert(2, string_descriptor(&product));
        strings.insert(3, string_descriptor(&serial));
        let d = Descriptors {
            cable_number,
            kind,
            device: device_descriptor(kind.product_id_base() + cable_number),
            config: configuration_descriptor(kind),
            product,
            serial,
            strings,
        };
        d.validate()?;
        Ok(d)
    }

    pub fn get_descriptor(&self, descriptor_type: u8, index: u8) -> Option<Vec<u8>> {
        match descriptor_type {
            DESCRIPTOR_DEVICE if index == 0 => Some(self.device.clone()),
            DESCRIPTOR_CONFIGURATION if index == 0 => Some(self.config.clone()),
            DESCRIPTOR_STRING => self.strings.get(&index).cloned(),
            _ => None,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.device.len() != 18 || self.device[0] != 18 || self.device[1] != DESCRIPTOR_DEVICE {
            return Err("invalid device descriptor".into());
        }
        if self.config.len() < 9 || self.config[1] != DESCRIPTOR_CONFIGURATION {
            return Err("invalid configuration descriptor".into());
        }
        let total = u16::from_le_bytes([self.config[2], self.config[3]]) as usize;
        if total != self.config.len() {
            return Err(format!("configuration wTotalLength={total}, actual={}", self.config.len()));
        }
        let mut pos = 0;
        while pos < self.config.len() {
            let length = self.config[pos] as usize;
            if length < 2 || pos + length > self.config.len() {
                return Err(format!("invalid descriptor at offset {pos}"));
            }
            pos += length;
        }
        Ok(())
    }
}

fn device_descriptor(product_id: u32) -> Vec<u8> {
    let pid = product_id as u16;
    let mut b = vec![0u8; 18];
    b[0] = 18;
    b[1] = DESCRIPTOR_DEVICE;
    // USB 2.0, full speed only: the device qualifier request is stalled, and
    // there is no BOS descriptor before 2.01.
    b[2..4].copy_from_slice(&0x0200u16.to_le_bytes());
    // Miscellaneous / common class / interface association: the audio
    // function's interfaces are grouped by an interface association
    // descriptor, which UAC2 needs because its AC header no longer lists them.
    b[4] = 0xEF;
    b[5] = 0x02;
    b[6] = 0x01;
    b[7] = 64; // EP0 packet size
    b[8..10].copy_from_slice(&0xFFFFu16.to_le_bytes()); // experimental local VID
    b[10..12].copy_from_slice(&pid.to_le_bytes());
    b[12..14].copy_from_slice(&0x0001u16.to_le_bytes());
    b[14] = 1; // manufacturer
    b[15] = 2; // product
    b[16] = 3; // serial
    b[17] = 1; // configurations
    b
}

fn configuration_descriptor(kind: CableKind) -> Vec<u8> {
    let mut b: Vec<u8> = Vec::new();
    let c = DESCRIPTOR_CS_INTERFACE;
    let le16 = |v: u16| v.to_le_bytes();
    // Stereo: front left and front right.
    let channels: [u8; 4] = 0x0000_0003u32.to_le_bytes();
    let interfaces = kind.interface_count();

    // Configuration header; wTotalLength is patched after construction.
    b.extend_from_slice(&[9, DESCRIPTOR_CONFIGURATION, 0, 0, interfaces, 1, 0, 0x80, 50]);

    // Interface association: every interface forms one audio function.
    b.extend_from_slice(&[8, DESCRIPTOR_INTERFACE_ASSOCIATION, 0, interfaces, 0x01, 0x00, IP_VERSION_02_00, 0]);

    // AudioControl interface 0.
    b.extend_from_slice(&[9, DESCRIPTOR_INTERFACE, 0, 0, 0, 0x01, 0x01, IP_VERSION_02_00, 0]);
    let header = b.len();
    // AC header (ADC-2 4.7.2): ADC 2.0, category I/O box; wTotalLength is
    // patched below.
    b.extend_from_slice(&[9, c, 0x01, 0x00, 0x02, 0x08, 0, 0, 0x00]);
    // Clock source (4.7.2.1): internal fixed clock, synchronized to SOF, with
    // read-only frequency and validity controls.
    b.extend_from_slice(&[8, c, 0x0A, CLOCK_SOURCE, 0x05, 0x05, 0, 0]);
    if kind.has_playback() {
        // Playback: USB streaming input terminal (4.7.2.4) -> feature unit
        // (4.7.2.8) with master mute and volume -> speaker output terminal (4.7.2.5).
        b.extend_from_slice(&[17, c, 0x02, PLAYBACK_INPUT_TERMINAL]);
        b.extend_from_slice(&le16(0x0101));
        b.extend_from_slice(&[0, CLOCK_SOURCE, 2]);
        b.extend_from_slice(&channels);
        b.extend_from_slice(&[0, 0, 0, 0]);
        b.extend_from_slice(&feature_unit(PLAYBACK_FEATURE_UNIT, PLAYBACK_INPUT_TERMINAL));
        b.extend_from_slice(&[12, c, 0x03, PLAYBACK_OUTPUT_TERMINAL]);
        b.extend_from_slice(&le16(0x0301));
        b.extend_from_slice(&[0, PLAYBACK_FEATURE_UNIT, CLOCK_SOURCE, 0, 0, 0]);
    }
    if kind.has_recording() {
        // Recording: microphone input terminal -> feature unit -> USB streaming
        // output terminal.
        b.extend_from_slice(&[17, c, 0x02, RECORDING_INPUT_TERMINAL]);
        b.extend_from_slice(&le16(0x0201));
        b.extend_from_slice(&[0, CLOCK_SOURCE, 2]);
        b.extend_from_slice(&channels);
        b.extend_from_slice(&[0, 0, 0, 0]);
        b.extend_from_slice(&feature_unit(RECORDING_FEATURE_UNIT, RECORDING_INPUT_TERMINAL));
        b.extend_from_slice(&[12, c, 0x03, RECORDING_OUTPUT_TERMINAL]);
        b.extend_from_slice(&le16(0x0101));
        b.extend_from_slice(&[0, RECORDING_FEATURE_UNIT, CLOCK_SOURCE, 0, 0, 0]);
    }
    let ac_length = (b.len() - header) as u16;
    b[header + 6..header + 8].copy_from_slice(&le16(ac_length));

    // AudioStreaming interfaces: alternate setting 0 has no endpoint;
    // alternate setting 1 streams stereo 24-bit PCM at 48 kHz.
    let (playback, recording) = kind.streaming_interfaces();
    let streams =
        [playback.map(|i| (i, PLAYBACK_INPUT_TERMINAL, 0x01)), recording.map(|i| (i, RECORDING_OUTPUT_TERMINAL, 0x82))];
    for (interface, terminal, endpoint) in streams.into_iter().flatten() {
        b.extend_from_slice(&[9, DESCRIPTOR_INTERFACE, interface, 0, 0, 0x01, 0x02, IP_VERSION_02_00, 0]);
        b.extend_from_slice(&[9, DESCRIPTOR_INTERFACE, interface, 1, 1, 0x01, 0x02, IP_VERSION_02_00, 0]);
        // AS general (4.9.2): Type I PCM.
        b.extend_from_slice(&[16, c, 0x01, terminal, 0x00, 0x01]);
        b.extend_from_slice(&0x0000_0001u32.to_le_bytes());
        b.extend_from_slice(&[2]);
        b.extend_from_slice(&channels);
        b.extend_from_slice(&[0]);
        // Type I format (FMT-2 2.3.1.6): 3-byte subslots, 24 bits.
        b.extend_from_slice(&[6, c, 0x02, 0x01, 3, 24]);
        // Isochronous data endpoint, synchronous to the bus frame clock that
        // also clocks the delay line: exactly one 48-frame packet per frame
        // each way, with no feedback endpoint.
        b.extend_from_slice(&[7, DESCRIPTOR_ENDPOINT, endpoint, 0x0D]);
        b.extend_from_slice(&le16(PACKET_BYTES as u16));
        b.extend_from_slice(&[1]);
        // Class-specific endpoint (4.10.1.2).
        b.extend_from_slice(&[8, DESCRIPTOR_CS_ENDPOINT, 0x01, 0x00, 0x00, 0x00, 0, 0]);
    }

    let total = b.len() as u16;
    b[2..4].copy_from_slice(&total.to_le_bytes());
    b
}

/// A UAC2 feature unit (ADC-2 4.7.2.8) on a stereo stream, with host
/// programmable mute and volume on the master channel only.
fn feature_unit(id: u8, source: u8) -> Vec<u8> {
    let mut b = vec![18, DESCRIPTOR_CS_INTERFACE, 0x06, id, source];
    b.extend_from_slice(&0x0000_000Fu32.to_le_bytes()); // master: mute and volume
    b.extend_from_slice(&[0; 8]); // left and right: none
    b.push(0);
    b
}

fn string_descriptor(s: &str) -> Vec<u8> {
    let mut encoded: Vec<u16> = s.encode_utf16().collect();
    let mut length = 2 + encoded.len() * 2;
    if length > 255 {
        encoded.truncate(126);
        length = 254;
    }
    let mut b = vec![0u8; length];
    b[0] = length as u8;
    b[1] = DESCRIPTOR_STRING;
    for (i, v) in encoded.iter().enumerate() {
        b[2 + i * 2..4 + i * 2].copy_from_slice(&v.to_le_bytes());
    }
    b
}

/// The default delay between a cable's playback and recording sides. Windows
/// sends playback about 10 ms ahead of its frames, so in steady streaming any
/// delay works; this much also covers playback that arrives up to about 50 ms
/// late after a system stall.
pub const DEFAULT_LATENCY_MS: i64 = 40;

/// The delay range a cable accepts, in milliseconds (bus frames).
pub const MIN_LATENCY_MS: i64 = 1;
pub const MAX_LATENCY_MS: i64 = 1000;

pub const REQUEST_GET_STATUS: u8 = 0x00;
pub const REQUEST_CLEAR_FEATURE: u8 = 0x01;
pub const REQUEST_SET_FEATURE: u8 = 0x03;
pub const REQUEST_SET_ADDRESS: u8 = 0x05;
pub const REQUEST_GET_DESCRIPTOR: u8 = 0x06;
#[allow(dead_code)] // listed for completeness; stalled like any unsupported request
pub const REQUEST_SET_DESCRIPTOR: u8 = 0x07;
pub const REQUEST_GET_CONFIGURATION: u8 = 0x08;
pub const REQUEST_SET_CONFIGURATION: u8 = 0x09;
pub const REQUEST_GET_INTERFACE: u8 = 0x0A;
pub const REQUEST_SET_INTERFACE: u8 = 0x0B;
pub const REQUEST_SYNCH_FRAME: u8 = 0x0C;

/// UAC2 class requests (ADC-2 5.2.2); bmRequestType gives the direction.
pub const AUDIO_CUR: u8 = 0x01;
pub const AUDIO_RANGE: u8 = 0x02;

/// Clock source control selectors (ADC-2 A.17.1).
const CS_SAM_FREQ_CONTROL: u8 = 0x01;
const CS_CLOCK_VALID_CONTROL: u8 = 0x02;
/// Feature unit control selectors (ADC-2 A.17.7).
const FU_MUTE_CONTROL: u8 = 0x01;
const FU_VOLUME_CONTROL: u8 = 0x02;

/// EPIPE: the request is stalled.
const STALL: i32 = -32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetupPacket {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

pub fn parse_setup(b: &[u8]) -> Result<SetupPacket, String> {
    if b.len() < 8 {
        return Err("setup packet too short".into());
    }
    Ok(SetupPacket {
        request_type: b[0],
        request: b[1],
        value: u16::from_le_bytes([b[2], b[3]]),
        index: u16::from_le_bytes([b[4], b[5]]),
        length: u16::from_le_bytes([b[6], b[7]]),
    })
}

/// The USB/IP bus ID of the cable with ID `number`.
pub fn bus_id(number: u32) -> String {
    format!("1-{number}")
}

pub struct Device {
    pub number: u32,
    pub bus_id: String,
    pub descriptors: Descriptors,
    /// What the cable does with its audio: on its own, the delay line that
    /// carries the playback endpoint's audio to the capture endpoint.
    pub audio: Arc<dyn CableAudio>,
    state: Mutex<DeviceState>,
}

struct DeviceState {
    configuration: u8,
    alt_setting: HashMap<u8, u8>,
    /// The playback and recording feature units.
    features: [FeatureUnit; 2],
}

type FeatureUnit = FeatureLevel;

/// The feature unit IDs in the descriptors: playback, then recording.
const FEATURE_UNITS: [u8; 2] = [PLAYBACK_FEATURE_UNIT, RECORDING_FEATURE_UNIT];
/// The volume range the feature units report, in 1/256 dB.
const VOLUME_MIN: i16 = -60 * 256;
const VOLUME_MAX: i16 = 0;
const VOLUME_RES: i16 = 256;
/// The volume value for silence (minus infinity dB).
const VOLUME_SILENT: i16 = i16::MIN;

impl Device {
    /// Cable `number` with its default name and serial number.
    #[cfg(test)]
    pub fn new(number: u32, latency_ms: i64) -> Result<Device, String> {
        let (serial, name) = (format!("VCABLE-{number:03}"), format!("Virtual Cable {number:02}"));
        Device::named(number, &serial, &name, latency_ms)
    }

    /// Creates the cable with ID `number`, serial number `serial`, called
    /// `name`, whose recording side plays its playback side's audio
    /// `latency_ms` later.
    pub fn named(number: u32, serial: &str, name: &str, latency_ms: i64) -> Result<Device, String> {
        let descriptors = Descriptors::named(number, serial, name)?;
        let latency_ms = latency_ms.clamp(MIN_LATENCY_MS, MAX_LATENCY_MS);
        Ok(Device::with_audio(number, descriptors, Arc::new(DelayLine::new(latency_ms as u64))))
    }

    /// A cable with these descriptors whose audio goes to `audio`.
    pub fn with_audio(number: u32, descriptors: Descriptors, audio: Arc<dyn CableAudio>) -> Device {
        let interfaces = descriptors.kind.interface_count();
        Device {
            number,
            bus_id: bus_id(number),
            descriptors,
            audio,
            state: Mutex::new(DeviceState {
                configuration: 0,
                alt_setting: (0..interfaces).map(|i| (i, 0)).collect(),
                features: [FeatureUnit::default(); 2],
            }),
        }
    }

    pub fn kind(&self) -> CableKind {
        self.descriptors.kind
    }

    /// The streaming state of the playback and capture sides.
    fn streaming(&self, d: &DeviceState) -> (bool, bool) {
        let (playback, recording) = self.kind().streaming_interfaces();
        let active = |iface: Option<u8>| {
            iface.is_some_and(|iface| d.configuration == 1 && d.alt_setting.get(&iface) == Some(&1))
        };
        (active(playback), active(recording))
    }

    pub fn product(&self) -> &str {
        &self.descriptors.product
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DeviceState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Processes the standard requests and the UAC2 class requests
    /// usbaudio2.sys makes (Microsoft documents the subset it uses).
    pub fn handle_control(&self, setup: SetupPacket, out: &[u8]) -> (Vec<u8>, i32) {
        let mut d = self.lock();

        let standard = setup.request_type & 0x60 == 0x00;
        if standard {
            return match setup.request {
                REQUEST_GET_DESCRIPTOR => {
                    let typ = (setup.value >> 8) as u8;
                    let idx = setup.value as u8;
                    match self.descriptors.get_descriptor(typ, idx) {
                        Some(v) => (truncate(v, setup.length), 0),
                        None => (Vec::new(), STALL),
                    }
                }
                REQUEST_SET_ADDRESS => (Vec::new(), 0),
                REQUEST_SET_CONFIGURATION => {
                    let configuration = setup.value as u8;
                    if configuration > 1 {
                        return (Vec::new(), STALL);
                    }
                    d.configuration = configuration;
                    // Selecting a configuration resets every interface to alternate 0.
                    for iface in 0..self.kind().interface_count() {
                        d.alt_setting.insert(iface, 0);
                    }
                    self.audio.end_playback();
                    self.audio.end_capture();
                    self.audio.streaming_changed(false, false);
                    (Vec::new(), 0)
                }
                REQUEST_GET_CONFIGURATION => (truncate(vec![d.configuration], setup.length), 0),
                REQUEST_SET_INTERFACE => {
                    let iface = setup.index as u8;
                    let alt = setup.value as u8;
                    let last = self.kind().interface_count() - 1;
                    if d.configuration != 1 || iface > last || alt > 1 || (iface == 0 && alt != 0) {
                        return (Vec::new(), STALL);
                    }
                    d.alt_setting.insert(iface, alt);
                    let (playback, recording) = self.kind().streaming_interfaces();
                    if alt == 0 && Some(iface) == playback {
                        self.audio.end_playback();
                    }
                    if alt == 0 && Some(iface) == recording {
                        self.audio.end_capture();
                    }
                    let (playing, capturing) = self.streaming(&d);
                    self.audio.streaming_changed(playing, capturing);
                    (Vec::new(), 0)
                }
                REQUEST_GET_INTERFACE => {
                    let iface = setup.index as u8;
                    match d.alt_setting.get(&iface) {
                        Some(&alt) => (truncate(vec![alt], setup.length), 0),
                        None => (Vec::new(), STALL),
                    }
                }
                REQUEST_GET_STATUS => (truncate(vec![0, 0], setup.length), 0),
                // Full-speed isochronous endpoints use one frame per millisecond.
                // A stable zero frame is sufficient for this synthetic device and is
                // preferable to stalling a host-side synchronization query.
                REQUEST_SYNCH_FRAME => (truncate(vec![0, 0], setup.length), 0),
                REQUEST_CLEAR_FEATURE | REQUEST_SET_FEATURE => (Vec::new(), 0),
                _ => (Vec::new(), STALL),
            };
        }

        // UAC2 class requests go to an entity: wIndex is the entity ID and
        // the AudioControl interface, wValue the control selector and channel.
        let recipient = setup.request_type & 0x1f;
        let get = setup.request_type & 0x80 != 0;
        let entity_id = (setup.index >> 8) as u8;
        let interface_number = setup.index as u8;
        let control = (setup.value >> 8) as u8;
        let channel = setup.value as u8;
        if recipient != 0x01 || interface_number != 0 || channel != 0 {
            return (Vec::new(), STALL);
        }
        let reply = |value: Vec<u8>| (truncate(value, setup.length), 0);

        // The fixed 48 kHz clock: its frequency, and that it is valid.
        if entity_id == CLOCK_SOURCE {
            return match (control, setup.request, get) {
                (CS_SAM_FREQ_CONTROL, AUDIO_CUR, true) => reply(SAMPLE_RATE.to_le_bytes().to_vec()),
                (CS_SAM_FREQ_CONTROL, AUDIO_RANGE, true) => {
                    // One discrete subrange: MIN = MAX = 48000, RES = 0.
                    let mut range = 1u16.to_le_bytes().to_vec();
                    for value in [SAMPLE_RATE, SAMPLE_RATE, 0] {
                        range.extend_from_slice(&value.to_le_bytes());
                    }
                    reply(range)
                }
                // The frequency is read-only, but setting the one it has is harmless.
                (CS_SAM_FREQ_CONTROL, AUDIO_CUR, false) if out.get(..4) == Some(&SAMPLE_RATE.to_le_bytes()) => {
                    (Vec::new(), 0)
                }
                (CS_CLOCK_VALID_CONTROL, AUDIO_CUR, true) => reply(vec![1]),
                _ => (Vec::new(), STALL),
            };
        }

        // Feature-unit mute and volume. Windows treats them as hardware
        // controls, so they are applied to the audio on its way from the
        // playback side to the recording side.
        let Some(unit) = FEATURE_UNITS.iter().position(|&id| id == entity_id) else {
            return (Vec::new(), STALL);
        };
        // A cable without one of the sides has no feature unit for it.
        if (unit == 0 && !self.kind().has_playback()) || (unit == 1 && !self.kind().has_recording()) {
            return (Vec::new(), STALL);
        }
        let feature = d.features[unit];
        match (control, setup.request, get) {
            (FU_MUTE_CONTROL, AUDIO_CUR, true) => reply(vec![feature.mute as u8]),
            (FU_MUTE_CONTROL, AUDIO_CUR, false) => {
                if let Some(&first) = out.first() {
                    d.features[unit].mute = first != 0;
                    self.audio.set_feature_levels(d.features[0], d.features[1]);
                }
                (Vec::new(), 0)
            }
            // Signed 1/256 dB.
            (FU_VOLUME_CONTROL, AUDIO_CUR, true) => reply(feature.volume.to_le_bytes().to_vec()),
            (FU_VOLUME_CONTROL, AUDIO_CUR, false) => {
                if out.len() >= 2 {
                    let volume = i16::from_le_bytes([out[0], out[1]]);
                    d.features[unit].volume =
                        if volume == VOLUME_SILENT { volume } else { volume.clamp(VOLUME_MIN, VOLUME_MAX) };
                    self.audio.set_feature_levels(d.features[0], d.features[1]);
                }
                (Vec::new(), 0)
            }
            (FU_VOLUME_CONTROL, AUDIO_RANGE, true) => {
                let mut range = 1u16.to_le_bytes().to_vec();
                for value in [VOLUME_MIN, VOLUME_MAX, VOLUME_RES] {
                    range.extend_from_slice(&value.to_le_bytes());
                }
                reply(range)
            }
            _ => (Vec::new(), STALL),
        }
    }

    pub fn playback_active(&self) -> bool {
        self.streaming(&self.lock()).0
    }

    pub fn capture_active(&self) -> bool {
        self.streaming(&self.lock()).1
    }

    /// Stores a playback URB's packets, scheduled from bus frame `start`.
    /// Ignored until the playback interface is active: Windows can queue the
    /// first URB just before its SET_INTERFACE.
    pub fn write_playback<'a>(&self, start: u64, packets: impl IntoIterator<Item = &'a [u8]>) {
        if self.playback_active() {
            self.audio.write_playback(start, &mut packets.into_iter());
        }
    }

    /// Fills a capture URB's packets, scheduled from bus frame `start` and
    /// laid out back to back in `out` with the given lengths. Silence while
    /// the recording interface is inactive.
    pub fn read_capture(&self, start: u64, out: &mut [u8], lengths: impl IntoIterator<Item = usize>) {
        let (playing, capturing) = self.streaming(&self.lock());
        if !capturing {
            out.fill(0);
            return;
        }
        self.audio.read_capture(start, out, &mut lengths.into_iter(), playing);
    }
}

fn truncate(mut b: Vec<u8>, n: u16) -> Vec<u8> {
    b.truncate(n as usize);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le16(b: &[u8]) -> u16 {
        u16::from_le_bytes([b[0], b[1]])
    }

    /// Every descriptor in the configuration, in order.
    fn descriptors(d: &Descriptors) -> Vec<&[u8]> {
        let mut found = Vec::new();
        let mut pos = 0;
        while pos < d.config.len() {
            let length = d.config[pos] as usize;
            found.push(&d.config[pos..pos + length]);
            pos += length;
        }
        found
    }

    #[test]
    fn descriptors_validate() {
        let d = Descriptors::new(1).unwrap();
        d.validate().unwrap();
        assert_eq!(le16(&d.config[2..4]) as usize, d.config.len());
        assert_eq!(le16(&d.device[2..4]), 0x0200, "bcdUSB");
        assert_eq!(&d.device[4..7], &[0xEF, 0x02, 0x01], "interface association device class");
    }

    /// Windows binds usbaudio2.sys to audio interfaces with protocol 0x20, and
    /// needs the interface association to see them as one function.
    #[test]
    fn audio_function_is_uac2() {
        let d = Descriptors::new(1).unwrap();
        let all = descriptors(&d);
        let association = all[1];
        assert_eq!(association[1], DESCRIPTOR_INTERFACE_ASSOCIATION, "IAD follows the configuration");
        assert_eq!(&association[2..7], &[0, 3, 0x01, 0x00, IP_VERSION_02_00]);
        let interfaces: Vec<_> = all.iter().filter(|b| b[1] == DESCRIPTOR_INTERFACE).collect();
        assert_eq!(interfaces.len(), 5, "AC, and two alternate settings for each AS interface");
        for interface in interfaces {
            assert_eq!((interface[5], interface[7]), (0x01, IP_VERSION_02_00), "audio class, UAC2 protocol");
        }
    }

    /// The AC header's total length covers every class-specific AC descriptor,
    /// and every terminal is clocked by the one clock source.
    #[test]
    fn audio_control_topology() {
        let d = Descriptors::new(1).unwrap();
        let all = descriptors(&d);
        let start = all.iter().position(|b| b[1] == DESCRIPTOR_CS_INTERFACE && b[2] == 0x01).unwrap();
        let header = all[start];
        assert_eq!(le16(&header[3..5]), 0x0200, "bcdADC");
        let ac: Vec<_> = all[start..].iter().take_while(|b| b[1] == DESCRIPTOR_CS_INTERFACE).collect();
        assert_eq!(le16(&header[6..8]) as usize, ac.iter().map(|b| b.len()).sum::<usize>());
        let clock = ac.iter().find(|b| b[2] == 0x0A).expect("clock source");
        assert_eq!(clock[3], CLOCK_SOURCE);
        let input_terminals: Vec<_> = ac.iter().filter(|b| b[2] == 0x02).collect();
        let output_terminals: Vec<_> = ac.iter().filter(|b| b[2] == 0x03).collect();
        assert_eq!((input_terminals.len(), output_terminals.len()), (2, 2));
        for terminal in input_terminals {
            assert_eq!((terminal.len(), terminal[7], terminal[8]), (17, CLOCK_SOURCE, 2), "clock and channels");
        }
        for terminal in output_terminals {
            assert_eq!((terminal.len(), terminal[8]), (12, CLOCK_SOURCE));
        }
        assert_eq!(ac.iter().filter(|b| b[2] == 0x06 && b.len() == 18).count(), 2, "feature units");
    }

    #[test]
    fn audio_endpoint_descriptors() {
        let d = Descriptors::new(1).unwrap();
        let endpoints: Vec<_> = descriptors(&d).into_iter().filter(|b| b[1] == DESCRIPTOR_ENDPOINT).collect();
        assert_eq!(endpoints.len(), 2);
        assert_eq!(endpoints[0][2], 0x01, "playback endpoint");
        assert_eq!(endpoints[1][2], 0x82, "capture endpoint");
        for ep in endpoints {
            assert_eq!(ep.len(), 7, "standard endpoint descriptor");
            assert_eq!(ep[3], 0x0D, "isochronous, synchronous");
            assert_eq!(le16(&ep[4..6]) as usize, PACKET_BYTES, "wMaxPacketSize");
            assert_eq!(ep[6], 1, "bInterval");
        }
    }

    #[test]
    fn audio_format_is_24bit_pcm() {
        let d = Descriptors::new(1).unwrap();
        let all = descriptors(&d);
        let general: Vec<_> = all.iter().filter(|b| b.len() == 16 && b[1] == DESCRIPTOR_CS_INTERFACE).collect();
        let formats: Vec<_> = all.iter().filter(|b| b.len() == 6 && b[1] == DESCRIPTOR_CS_INTERFACE).collect();
        assert_eq!((general.len(), formats.len()), (2, 2));
        for g in general {
            assert_eq!((g[5], &g[6..10], g[10]), (0x01, &[1, 0, 0, 0][..], 2), "Type I PCM, stereo");
        }
        for f in formats {
            assert_eq!((f[2], f[3], f[4], f[5]), (0x02, 0x01, 3, 24), "subslot size / bit resolution");
        }
    }

    #[test]
    fn unique_product_id() {
        let a = Descriptors::new(1).unwrap();
        let b = Descriptors::new(2).unwrap();
        assert_eq!(le16(&a.device[10..12]), 0xCB01);
        assert_ne!(le16(&a.device[10..12]), le16(&b.device[10..12]));
    }

    #[test]
    fn get_device_descriptor_and_stall_qualifier() {
        let d = Device::new(1, DEFAULT_LATENCY_MS).unwrap();
        let get = |kind: u8, length| {
            let setup = SetupPacket {
                request_type: 0x80,
                request: REQUEST_GET_DESCRIPTOR,
                value: (kind as u16) << 8,
                length,
                ..Default::default()
            };
            d.handle_control(setup, &[])
        };
        let (data, status) = get(DESCRIPTOR_DEVICE, 18);
        assert_eq!((status, data.len()), (0, 18));
        assert_eq!(get(0x06, 10).1, STALL, "a full-speed-only device has no device qualifier");
    }

    #[test]
    fn set_and_get_interface() {
        let d = Device::new(1, DEFAULT_LATENCY_MS).unwrap();
        let control = |request_type, request, index, value, length| {
            d.handle_control(SetupPacket { request_type, request, value, index, length }, &[])
        };
        control(0x00, REQUEST_SET_CONFIGURATION, 0, 1, 0);
        control(0x01, REQUEST_SET_INTERFACE, 1, 1, 0);
        let (data, status) = control(0x81, REQUEST_GET_INTERFACE, 1, 0, 1);
        assert_eq!((status, data), (0, vec![1]));
    }

    /// The fixed 48 kHz clock answers GET CUR and GET RANGE, the requests
    /// usbaudio2.sys needs, and reports itself valid.
    #[test]
    fn clock_source_requests() {
        let d = Device::new(1, DEFAULT_LATENCY_MS).unwrap();
        let clock = (CLOCK_SOURCE as u16) << 8;
        let request = |request_type, request, control: u8, length, data: &[u8]| {
            let setup = SetupPacket { request_type, request, value: (control as u16) << 8, index: clock, length };
            d.handle_control(setup, data)
        };
        assert_eq!(request(0xA1, AUDIO_CUR, CS_SAM_FREQ_CONTROL, 4, &[]), (48000u32.to_le_bytes().to_vec(), 0));
        // usbaudio2.sys first reads the number of subranges, then all of them.
        assert_eq!(request(0xA1, AUDIO_RANGE, CS_SAM_FREQ_CONTROL, 2, &[]), (vec![1, 0], 0));
        let (range, status) = request(0xA1, AUDIO_RANGE, CS_SAM_FREQ_CONTROL, 14, &[]);
        assert_eq!(status, 0);
        let field = |at: usize| u32::from_le_bytes(range[at..at + 4].try_into().unwrap());
        assert_eq!((field(2), field(6), field(10)), (48000, 48000, 0));
        assert_eq!(request(0xA1, AUDIO_CUR, CS_CLOCK_VALID_CONTROL, 1, &[]), (vec![1], 0));
        assert_eq!(request(0x21, AUDIO_CUR, CS_SAM_FREQ_CONTROL, 4, &48000u32.to_le_bytes()).1, 0);
        assert_eq!(request(0x21, AUDIO_CUR, CS_SAM_FREQ_CONTROL, 4, &44100u32.to_le_bytes()).1, STALL);
        assert_eq!(request(0xA1, 0x04, CS_SAM_FREQ_CONTROL, 4, &[]).1, STALL, "unknown request");
    }

    #[test]
    fn latency_is_kept_in_range() {
        assert_eq!(Device::new(1, 40).unwrap().audio.delay(), 40);
        assert_eq!(Device::new(1, 0).unwrap().audio.delay(), MIN_LATENCY_MS as u64);
        assert_eq!(Device::new(1, 5000).unwrap().audio.delay(), MAX_LATENCY_MS as u64);
    }

    /// The playback and recording volume and mute controls are separate, and
    /// both apply to the audio on its way through the cable.
    #[test]
    fn volume_and_mute_apply_to_the_loopback() {
        let d = Device::new(1, 10).unwrap();
        let control = |request_type, request, value, index, data: &[u8], length: u16| {
            let setup = SetupPacket { request_type, request, value, index, length };
            d.handle_control(setup, data)
        };
        control(0x00, REQUEST_SET_CONFIGURATION, 1, 0, &[], 0);
        control(0x01, REQUEST_SET_INTERFACE, 1, 1, &[], 0);
        control(0x01, REQUEST_SET_INTERFACE, 1, 2, &[], 0);
        // Plays one sample of 0x400000 through the next frame and returns it.
        let frame = std::cell::Cell::new(1000);
        let sample = |d: &Device| {
            let mut packet = vec![0u8; PACKET_BYTES];
            packet[..3].copy_from_slice(&[0x00, 0x00, 0x40]);
            d.write_playback(frame.get(), [packet.as_slice()]);
            let mut out = vec![0u8; PACKET_BYTES];
            d.read_capture(frame.get() + 10, &mut out, [PACKET_BYTES]);
            frame.set(frame.get() + 1);
            i32::from_le_bytes([out[0], out[1], out[2], 0])
        };
        const PLAYBACK: u16 = (PLAYBACK_FEATURE_UNIT as u16) << 8;
        const RECORDING: u16 = (RECORDING_FEATURE_UNIT as u16) << 8;
        const MUTE: u16 = (FU_MUTE_CONTROL as u16) << 8;
        const VOLUME: u16 = (FU_VOLUME_CONTROL as u16) << 8;
        assert_eq!(sample(&d), 0x40_0000, "0 dB passes through unchanged");

        // The volume range: one subrange, -60 dB to 0 dB in 1 dB steps.
        let (range, status) = control(0xA1, AUDIO_RANGE, VOLUME, PLAYBACK, &[], 8);
        assert_eq!(status, 0);
        let field = |at: usize| i16::from_le_bytes([range[at], range[at + 1]]);
        assert_eq!((field(0), field(2), field(4), field(6)), (1, -60 * 256, 0, 256));

        let minus_6_db = (-6i16 * 256).to_le_bytes();
        assert_eq!(control(0x21, AUDIO_CUR, VOLUME, PLAYBACK, &minus_6_db, 2).1, 0);
        let scaled = sample(&d) as f64 / 0x40_0000 as f64;
        assert!((scaled - 0.501).abs() < 0.002, "playback volume -6 dB gave {scaled}");
        assert_eq!(control(0xA1, AUDIO_CUR, VOLUME, PLAYBACK, &[], 2).0, minus_6_db);
        assert_eq!(control(0xA1, AUDIO_CUR, VOLUME, RECORDING, &[], 2).0, [0, 0], "units are separate");

        control(0x21, AUDIO_CUR, MUTE, RECORDING, &[1], 1);
        assert_eq!(control(0xA1, AUDIO_CUR, MUTE, RECORDING, &[], 1).0, [1]);
        assert_eq!(sample(&d), 0, "recording mute");
        control(0x21, AUDIO_CUR, MUTE, RECORDING, &[0], 1);
        control(0x21, AUDIO_CUR, VOLUME, PLAYBACK, &[0, 0], 2);
        assert_eq!(sample(&d), 0x40_0000, "back to unity");
        control(0x21, AUDIO_CUR, VOLUME, RECORDING, &VOLUME_SILENT.to_le_bytes(), 2);
        assert_eq!(sample(&d), 0, "minus infinity dB is silence");
        assert_eq!(control(0xA1, AUDIO_CUR, VOLUME | 1, PLAYBACK, &[], 2).1, STALL, "no per-channel controls");
    }

    /// The original virtual cable's descriptors, as its executable dumped
    /// them: a loopback cable must stay byte for byte the same device.
    #[test]
    fn loopback_descriptors_are_unchanged() {
        let golden = "12010002ef020140ffff01cb0100010203010902f7000301008032080b0003010020000904000000010120000924010002086f000008240a0905050000112402010101000902030000000000000012240602010f0000000000000000000000000c2403030103000209000000112402040102000902030000000000000012240605040f0000000000000000000000000c2403060101000509000000090401000001022000090401010101022000102401010001010000000203000000000624020103180705010d2001010825010000000000090402000001022000090402010101022000102401060001010000000203000000000624020103180705820d2001010825010000000000";
        let d = Descriptors::new(1).unwrap();
        let bytes: Vec<u8> =
            (0..golden.len()).step_by(2).map(|i| u8::from_str_radix(&golden[i..i + 2], 16).unwrap()).collect();
        assert_eq!([d.device.as_slice(), d.config.as_slice()].concat(), bytes);
    }

    fn kind_descriptors(kind: CableKind) -> Descriptors {
        Descriptors::build(kind, 4, "8F3A2C1D", "Mix", "Lemon Mix").unwrap()
    }

    /// Input and output cables have one streaming interface, numbered 1, and
    /// keep the endpoint addresses of the loopback cable.
    #[test]
    fn single_sided_cables_have_one_streaming_interface() {
        for (kind, endpoint, terminal_type) in [(CableKind::Input, 0x01, 0x0101u16), (CableKind::Output, 0x82, 0x0101)]
        {
            let d = kind_descriptors(kind);
            d.validate().unwrap();
            let all = descriptors(&d);
            assert_eq!(d.config[4], 2, "{kind:?} bNumInterfaces");
            assert_eq!(&all[1][2..4], &[0, 2], "{kind:?} IAD covers both interfaces");
            let interfaces: Vec<_> = all.iter().filter(|b| b[1] == DESCRIPTOR_INTERFACE).collect();
            assert_eq!(interfaces.iter().map(|i| (i[2], i[3])).collect::<Vec<_>>(), [(0, 0), (1, 0), (1, 1)]);
            let endpoints: Vec<_> = all.iter().filter(|b| b[1] == DESCRIPTOR_ENDPOINT).collect();
            assert_eq!(endpoints.len(), 1);
            assert_eq!((endpoints[0][2], endpoints[0][3]), (endpoint, 0x0D), "{kind:?}");
            // The AC header's length still covers exactly its descriptors.
            let start = all.iter().position(|b| b[1] == DESCRIPTOR_CS_INTERFACE && b[2] == 0x01).unwrap();
            let ac: Vec<_> = all[start..].iter().take_while(|b| b[1] == DESCRIPTOR_CS_INTERFACE).collect();
            assert_eq!(le16(&all[start][6..8]) as usize, ac.iter().map(|b| b.len()).sum::<usize>());
            assert_eq!(ac.iter().filter(|b| b[2] == 0x06).count(), 1, "one feature unit");
            // The AS general descriptor links to the USB streaming terminal.
            let general = all.iter().find(|b| b.len() == 16 && b[1] == DESCRIPTOR_CS_INTERFACE).unwrap();
            let streaming = if kind == CableKind::Input { PLAYBACK_INPUT_TERMINAL } else { RECORDING_OUTPUT_TERMINAL };
            assert_eq!(general[3], streaming);
            let usb_terminal = ac.iter().find(|b| b[3] == streaming).unwrap();
            assert_eq!(le16(&usb_terminal[4..6]), terminal_type, "USB streaming terminal");
        }
        let pids: Vec<u16> = CableKind::ALL.iter().map(|&k| le16(&kind_descriptors(k).device[10..12])).collect();
        assert_eq!(pids, [0xCC04, 0xCD04, 0xCB04], "each kind has its own product ID range");
        assert_eq!(kind_descriptors(CableKind::Loopback).strings[&1], string_descriptor("Lemon Mix"));
    }

    /// An output cable streams capture on interface 1, endpoint 0x82; it has
    /// no playback side or playback feature unit.
    #[test]
    fn output_cable_streams_capture_on_interface_1() {
        let d = Device::with_audio(4, kind_descriptors(CableKind::Output), Arc::new(DelayLine::new(10)));
        let control = |request_type, request, value, index| {
            d.handle_control(SetupPacket { request_type, request, value, index, length: 2 }, &[0, 0])
        };
        control(0x00, REQUEST_SET_CONFIGURATION, 1, 0);
        assert_eq!(control(0x01, REQUEST_SET_INTERFACE, 1, 2).1, STALL, "there is no interface 2");
        assert_eq!(control(0x01, REQUEST_SET_INTERFACE, 1, 1).1, 0);
        assert!(d.capture_active() && !d.playback_active());
        const VOLUME: u16 = (FU_VOLUME_CONTROL as u16) << 8;
        assert_eq!(control(0xA1, AUDIO_CUR, VOLUME, (PLAYBACK_FEATURE_UNIT as u16) << 8).1, STALL);
        assert_eq!(control(0xA1, AUDIO_CUR, VOLUME, (RECORDING_FEATURE_UNIT as u16) << 8).1, 0);
        let input = Device::with_audio(5, kind_descriptors(CableKind::Input), Arc::new(DelayLine::new(10)));
        input.handle_control(SetupPacket { request: REQUEST_SET_CONFIGURATION, value: 1, ..Default::default() }, &[]);
        input.handle_control(
            SetupPacket { request_type: 0x01, request: REQUEST_SET_INTERFACE, value: 1, index: 1, length: 0 },
            &[],
        );
        assert!(input.playback_active() && !input.capture_active());
    }

    #[test]
    fn kind_names_round_trip() {
        for kind in CableKind::ALL {
            assert_eq!(CableKind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(CableKind::from_name(" Loopback "), Some(CableKind::Loopback));
        assert_eq!(CableKind::from_name("both"), None);
    }
}
