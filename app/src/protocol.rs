//! What the window asks of whoever runs the mixer, and what it gets back.
//! The same requests go to this process's own mixer when it runs on demand,
//! and over the Windows service's pipe as JSON when the service runs it.

use serde::{Deserialize, Serialize};

pub use engine::{BusParams, StripParams};

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
    /// "default" for the system default, or an endpoint ID.
    Device {
        id: String,
        endpoint: String,
        exclusive: bool,
    },
    /// List the devices again.
    RefreshDevices,
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
    /// off, opening, running or failed.
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
    /// Why the request failed, if it did.
    pub error: Option<String>,
}

impl Reply {
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
        ];
        for request in requests {
            let json = serde_json::to_string(&request).unwrap();
            assert!(!json.contains('\n'));
            assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
        }
        assert!(serde_json::from_str::<Request>("{\"op\":\"nonsense\"}").is_err());
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
