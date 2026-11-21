//! Physical audio devices.
//!
//! Each hardware strip and hardware bus has a port that persists while the
//! application runs. A device thread opens the chosen WASAPI endpoint
//! (shared mode by default, exclusive on request), streams between it and the
//! port, and reopens it when it disappears, changes format or, for the
//! default device, when Windows changes the default. The rest of the engine
//! never notices: a port without a device is silent.

pub mod asrc;
pub mod port;
pub mod ring;

mod manager;
#[cfg(windows)]
mod wasapi;

pub use manager::{
    APP_ENDPOINT, AppInfo, DEFAULT_ENDPOINT, DeviceChoice, DeviceInfo, DeviceManager, Direction, PortHandle,
    PortStatus, app_display_name, list_apps, list_devices,
};
pub use port::{CapturePort, PortReading, PortState, RenderPort};
#[cfg(windows)]
pub use wasapi::measure_latency;
