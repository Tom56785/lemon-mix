//! What the window asks of whoever runs the mixer, and what it gets back.
//! The same requests go to this process's own mixer when it runs on demand,
//! and over the Windows service's pipe as JSON when the service runs it.
//!
//! Controllers (the Stream Deck plugin, `LemonMix.exe --control`, scripts)
//! use the same requests over the control pipe, which whichever process runs
//! the mixer serves; docs/control-protocol.md describes it for them.

use serde::{Deserialize, Serialize};

pub use engine::{BusParams, StripParams};

/// The version of the requests, replies and events, in every reply and
/// event. It goes up when a change would break an existing controller.
pub const PROTOCOL_VERSION: u32 = 1;

/// How a status line reads at a glance, shown as the colour of its dot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    Ok,
    Pending,
    Error,
}

/// A request to the mixer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Nothing: the reply's snapshot is all the window wants.
    Status,
    /// Add a cable with this name and kind (`input`, `output` or `loopback`).
    AddCable { name: String, kind: String },
    /// Remove the cable with this serial number.
    RemoveCable { serial: String },
    /// Rename a cable. Windows keeps a device's name, so the cable becomes a
    /// new device under the new name; its mixer settings are kept.
    RenameCable { serial: String, name: String },
    /// Change a cable's kind; it becomes a new device of that kind.
    SetCableKind { serial: String, kind: String },
    /// Start the server if it stopped, and attach every cable again.
    Sync,
    /// Change the mixer.
    Mixer { command: MixerCommand },
    /// A controller's change, naming strips and buses as people do.
    Control { action: ControlAction },
    /// Keep the connection open and send events: the state whenever it
    /// changes, and with `meters`, the meters about 15 times a second. Only
    /// the control pipe takes it.
    Subscribe {
        #[serde(default)]
        meters: bool,
    },
}

/// A change from a controller. A target names a strip or bus by its key, by
/// its ID (`H1`, `A2`, `V3`, `B3`), or by its name: `strip:<name>`,
/// `bus:<name>`, or the bare name when only one strip or bus has it. Names
/// and IDs are matched ignoring case. A switch is turned on (`true`), off
/// (`false`), or the other way from how it is (absent or `null`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ControlAction {
    /// Mute a strip or bus.
    Mute {
        target: String,
        #[serde(default)]
        on: Option<bool>,
    },
    /// Solo a strip.
    Solo {
        target: String,
        #[serde(default)]
        on: Option<bool>,
    },
    /// Sum a strip's or bus's left and right.
    Mono {
        target: String,
        #[serde(default)]
        on: Option<bool>,
    },
    /// Send a strip to a bus.
    Route {
        strip: String,
        bus: String,
        #[serde(default)]
        on: Option<bool>,
    },
    /// Set a fader, in dB.
    Gain { target: String, db: f32 },
    /// Move a fader by this many dB, up or down.
    AdjustGain { target: String, db: f32 },
}

/// What a subscription sends, one JSON line each.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The whole state, first and then whenever it changes. Its meters are
    /// left at zero: they come in `Meters`.
    State { protocol: u32, version: String, snapshot: Snapshot },
    /// Every strip's and bus's meter, in the order of the last state.
    Meters { strips: Vec<KeyedMeter>, buses: Vec<KeyedMeter> },
}

/// A meter reading with the key of its strip or bus.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyedMeter {
    pub key: String,
    #[serde(flatten)]
    pub meter: Meter,
}

/// A change to the mixer's strips, buses or devices.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum MixerCommand {
    /// New settings for a strip (routes are set with `Route`).
    Strip {
        id: String,
        params: StripParams,
    },
    /// Connect or disconnect a strip and a bus.
    Route {
        strip: String,
        bus: String,
        on: bool,
    },
    Bus {
        id: String,
        params: BusParams,
    },
    /// A hardware strip's or bus's name.
    Label {
        id: String,
        label: String,
    },
    /// The device of a hardware strip (`H1`) or bus (`A1`): "" for none,
    /// "default" for the system default, or an endpoint ID. A strip may
    /// instead take a bus's output: `bus:` and the bus's key, ID or name.
    Device {
        id: String,
        endpoint: String,
        exclusive: bool,
    },
    /// List the devices again.
    RefreshDevices,
    /// Add a hardware input strip after the last one.
    AddStrip,
    /// Add a hardware output bus after the last one.
    AddBus,
    /// Remove a hardware strip (`H2`); the strips after it move up a number.
    RemoveStrip {
        id: String,
    },
    /// Remove a hardware bus (`A2`) and every route to it; the buses after it
    /// move up a number.
    RemoveBus {
        id: String,
    },
}

/// A physical device as the window shows it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// A hardware strip's or bus's device and its stream.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PortView {
    pub endpoint: String,
    pub exclusive: bool,
    /// off, opening, running or failed; `bus` for a strip fed from a bus,
    /// whose name is then in `device_name`.
    pub state: String,
    pub device_name: String,
    pub error: String,
    pub rate: u32,
    pub fill_ms: f64,
    pub correction_ppm: f64,
    pub underruns: u64,
    pub overruns: u64,
}

/// A meter reading: peak and RMS per channel, and gain reduction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Meter {
    pub peak: [f32; 2],
    pub rms: [f32; 2],
    pub reduction: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StripView {
    /// `H1`... for hardware strips, `V<cable id>` for virtual inputs.
    pub id: String,
    /// Stays with the strip when its ID changes, as when a strip before it is
    /// removed: what controllers remember it by.
    #[serde(default)]
    pub key: String,
    pub label: String,
    pub params: StripParams,
    pub routes: Vec<String>,
    pub meter: Meter,
    /// The device of a hardware strip.
    pub port: Option<PortView>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BusView {
    /// `A1`... for hardware buses, `B<cable id>` for virtual buses.
    pub id: String,
    /// Stays with the bus when its ID changes.
    #[serde(default)]
    pub key: String,
    pub label: String,
    pub params: BusParams,
    pub meter: Meter,
    pub port: Option<PortView>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CableView {
    pub id: u32,
    pub serial: String,
    pub name: String,
    pub kind: String,
}

/// Everything the window shows.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub cables: Vec<CableView>,
    /// Why the USB/IP server is not running; empty while it runs.
    pub server_error: String,
    /// Windows Sound synchronization, when there is something to say about it.
    pub status: Option<(Tone, String)>,
    pub strips: Vec<StripView>,
    pub buses: Vec<BusView>,
    pub capture_devices: Vec<DeviceView>,
    pub render_devices: Vec<DeviceView>,
    /// The latency between cable playback and the mixer, in ms.
    pub latency_ms: i64,
}

/// The answer to a request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub snapshot: Snapshot,
    /// The version of whoever answered, which may differ from the window's.
    pub version: String,
    /// `PROTOCOL_VERSION` of whoever answered.
    #[serde(default)]
    pub protocol: u32,
    /// Why the request failed, if it did.
    pub error: Option<String>,
}

impl Reply {
    /// A reply from this process, with its versions.
    pub fn new(snapshot: Snapshot, error: Option<String>) -> Reply {
        Reply { snapshot, version: crate::VERSION.to_string(), protocol: PROTOCOL_VERSION, error }
    }

    pub fn result(&self) -> Result<(), String> {
        match &self.error {
            Some(err) => Err(err.clone()),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_as_json() {
        let requests = [
            Request::Status,
            Request::AddCable { name: "Música (live)".into(), kind: "loopback".into() },
            Request::RemoveCable { serial: "8F3A2C1D".into() },
            Request::Mixer { command: MixerCommand::Route { strip: "H1".into(), bus: "B3".into(), on: true } },
            Request::Mixer {
                command: MixerCommand::Strip {
                    id: "V2".into(),
                    params: StripParams { gain_db: -6.0, ..Default::default() },
                },
            },
            Request::Mixer {
                command: MixerCommand::Device { id: "A1".into(), endpoint: "default".into(), exclusive: true },
            },
            Request::Mixer { command: MixerCommand::AddStrip },
            Request::Mixer { command: MixerCommand::RemoveBus { id: "A12".into() } },
            Request::Control { action: ControlAction::Route { strip: "strip:Mic".into(), bus: "A2".into(), on: None } },
            Request::Control { action: ControlAction::AdjustGain { target: "8f3a2c1d9e0b".into(), db: -1.5 } },
            Request::Subscribe { meters: true },
        ];
        for request in requests {
            let json = serde_json::to_string(&request).unwrap();
            assert!(!json.contains('\n'));
            assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
        }
        assert!(serde_json::from_str::<Request>("{\"op\":\"nonsense\"}").is_err());
        // A controller may leave out a switch to turn it over, and the meters.
        let toggle: Request =
            serde_json::from_str(r#"{"op":"control","action":{"action":"mute","target":"Mic"}}"#).unwrap();
        assert_eq!(toggle, Request::Control { action: ControlAction::Mute { target: "Mic".into(), on: None } });
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"op":"subscribe"}"#).unwrap(),
            Request::Subscribe { meters: false }
        );
        // Missing strip settings take their defaults.
        let partial: Request = serde_json::from_str(
            "{\"op\":\"mixer\",\"command\":{\"cmd\":\"strip\",\"id\":\"H1\",\"params\":{\"mute\":true}}}",
        )
        .unwrap();
        assert_eq!(
            partial,
            Request::Mixer {
                command: MixerCommand::Strip {
                    id: "H1".into(),
                    params: StripParams { mute: true, ..Default::default() }
                }
            }
        );
    }
}
