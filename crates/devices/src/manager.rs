//! Keeps each hardware port connected to the device the user chose.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cables::event::Event;
use cables::logf;
use cables::logging::Logger;
use engine::{InputPort, OutputPort};

use crate::port::{CapturePort, PortReading, PortState, RenderPort};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Capture,
    Render,
}

/// The endpoint `DeviceChoice::endpoint` names for the system default.
pub const DEFAULT_ENDPOINT: &str = "default";

/// Which device a port uses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceChoice {
    /// Empty for none, `DEFAULT_ENDPOINT` for the system default, otherwise
    /// a WASAPI endpoint ID.
    pub endpoint: String,
    /// Exclusive mode: the device for this port alone, at the lowest latency
    /// it supports. Shared mode mixes with other applications.
    pub exclusive: bool,
}

impl DeviceChoice {
    pub fn is_off(&self) -> bool {
        self.endpoint.trim().is_empty()
    }

    pub fn is_default(&self) -> bool {
        self.endpoint == DEFAULT_ENDPOINT
    }
}

/// An endpoint Windows lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// The name Windows has for endpoint `endpoint`, whatever its state (one
/// unplugged or disabled is still named), or `None` for the default device,
/// an application, or one Windows does not know.
pub fn endpoint_name(endpoint: &str) -> Option<String> {
    if endpoint.is_empty() || endpoint == DEFAULT_ENDPOINT || endpoint.starts_with(APP_ENDPOINT) {
        return None;
    }
    #[cfg(windows)]
    {
        crate::wasapi::endpoint_name(endpoint)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// The active endpoints in one direction.
pub fn list_devices(direction: Direction) -> Result<Vec<DeviceInfo>, String> {
    #[cfg(windows)]
    {
        crate::wasapi::enumerate(direction)
    }
    #[cfg(not(windows))]
    {
        let _ = direction;
        Ok(Vec::new())
    }
}

/// What a capture port's endpoint starts with when it records an
/// application's sound: `app:` and the program's file name, such as
/// `app:Spotify.exe`. The port follows the program as it closes and starts.
pub const APP_ENDPOINT: &str = "app:";

/// An application that has played sound since it started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppInfo {
    /// The program's file name, which a port's endpoint names it by.
    pub exe: String,
    /// The file name without `.exe`, to show.
    pub name: String,
    /// Whether it is playing now.
    pub active: bool,
}

/// The name an application is shown by: its file name without `.exe`.
pub fn app_display_name(exe: &str) -> String {
    let stem = exe.rsplit_once('.').filter(|(_, ext)| ext.eq_ignore_ascii_case("exe")).map_or(exe, |(stem, _)| stem);
    stem.to_string()
}

/// The applications with sound on any playback device, by program, in
/// alphabetical order.
pub fn list_apps() -> Result<Vec<AppInfo>, String> {
    #[cfg(windows)]
    {
        crate::wasapi::list_apps()
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}

/// What a port shows besides its counters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PortStatus {
    /// The device's name while it is open.
    pub device_name: String,
    /// Why the device could not be opened, if it could not.
    pub error: String,
    pub exclusive: bool,
}

pub(crate) struct PortShared {
    /// The strip or bus this port belongs to, for the log.
    label: Mutex<String>,
    pub direction: Direction,
    pub capture: Option<Arc<CapturePort>>,
    pub render: Option<Arc<RenderPort>>,
    choice: Mutex<DeviceChoice>,
    /// Bumped whenever the stream should reopen: a new choice, or a change in
    /// Windows that may affect it.
    pub generation: AtomicU64,
    pub wake: Event,
    stop: AtomicBool,
    pub status: Mutex<PortStatus>,
    pub logger: Logger,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

impl PortShared {
    pub fn label(&self) -> String {
        lock(&self.label).clone()
    }

    pub fn stats(&self) -> &crate::port::PortStats {
        match (&self.capture, &self.render) {
            (Some(capture), _) => &capture.stats,
            (_, Some(render)) => &render.stats,
            _ => unreachable!("a port is capture or render"),
        }
    }

    /// Whether the running stream should stop: the port is closing, or its
    /// generation moved on.
    pub fn should_stop(&self, generation: u64) -> bool {
        self.stop.load(Ordering::Relaxed) || self.generation.load(Ordering::Relaxed) != generation
    }

    fn reopen(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.wake.signal();
    }
}

/// A hardware strip's or bus's port. Dropping it closes the device.
pub struct PortHandle {
    shared: Arc<PortShared>,
    thread: Option<JoinHandle<()>>,
}

impl PortHandle {
    /// Selects the device; the port reopens if it changed.
    pub fn configure(&self, choice: DeviceChoice) {
        let mut current = lock(&self.shared.choice);
        if *current != choice {
            *current = choice;
            drop(current);
            self.shared.reopen();
        }
    }

    pub fn choice(&self) -> DeviceChoice {
        lock(&self.shared.choice).clone()
    }

    pub fn input_port(&self) -> Option<Arc<dyn InputPort>> {
        self.shared.capture.as_ref().map(|port| Arc::clone(port) as Arc<dyn InputPort>)
    }

    pub fn output_port(&self) -> Option<Arc<dyn OutputPort>> {
        self.shared.render.as_ref().map(|port| Arc::clone(port) as Arc<dyn OutputPort>)
    }

    pub fn reading(&self) -> PortReading {
        self.shared.stats().reading()
    }

    pub fn status(&self) -> PortStatus {
        lock(&self.shared.status).clone()
    }

    pub fn label(&self) -> String {
        self.shared.label()
    }

    /// Renames the port in the log, as when the strips after a removed one
    /// move up.
    pub fn set_label(&self, label: &str) {
        *lock(&self.shared.label) = label.to_string();
    }
}

impl Drop for PortHandle {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.wake.signal();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Creates ports and reopens them when Windows' devices change.
pub struct DeviceManager {
    logger: Logger,
    buffer_ms: f64,
    ports: Arc<Mutex<Vec<Weak<PortShared>>>>,
    /// How many device changes Windows has reported (see `device_changes`).
    changes: Arc<AtomicU64>,
    #[cfg(windows)]
    _notifier: Option<crate::wasapi::Notifier>,
}

impl DeviceManager {
    /// `buffer_ms` is how much audio each port keeps between its device and
    /// the engine: the margin for bursty device buffers and engine batches.
    pub fn new(logger: &Logger, buffer_ms: f64) -> DeviceManager {
        let ports: Arc<Mutex<Vec<Weak<PortShared>>>> = Arc::default();
        let changes = Arc::new(AtomicU64::new(0));
        #[cfg(windows)]
        let notifier = {
            let watched = Arc::downgrade(&ports);
            let counted = Arc::clone(&changes);
            let log = logger.clone();
            crate::wasapi::Notifier::start(Box::new(move |change| {
                counted.fetch_add(1, Ordering::Relaxed);
                on_device_change(&watched, change);
            }))
            .inspect_err(|err| logf!(log, "Device change notifications are unavailable: {err}"))
            .ok()
        };
        DeviceManager {
            logger: logger.clone(),
            buffer_ms,
            ports,
            changes,
            #[cfg(windows)]
            _notifier: notifier,
        }
    }

    /// How many times Windows has reported a device appearing, going,
    /// changing state or becoming the default: when it changes, the device
    /// lists are out of date.
    pub fn device_changes(&self) -> u64 {
        self.changes.load(Ordering::Relaxed)
    }

    pub fn capture(&self, label: &str) -> PortHandle {
        self.port(label, Direction::Capture)
    }

    pub fn render(&self, label: &str) -> PortHandle {
        self.port(label, Direction::Render)
    }

    fn port(&self, label: &str, direction: Direction) -> PortHandle {
        let shared = Arc::new(PortShared {
            label: Mutex::new(label.to_string()),
            direction,
            capture: (direction == Direction::Capture).then(|| CapturePort::new(self.buffer_ms)),
            render: (direction == Direction::Render).then(|| RenderPort::new(self.buffer_ms)),
            choice: Mutex::new(DeviceChoice::default()),
            generation: AtomicU64::new(0),
            wake: Event::new(),
            stop: AtomicBool::new(false),
            status: Mutex::new(PortStatus::default()),
            logger: self.logger.clone(),
        });
        let mut ports = lock(&self.ports);
        ports.retain(|p| p.strong_count() > 0);
        ports.push(Arc::downgrade(&shared));
        drop(ports);
        let running = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name(format!("device {label}"))
            .spawn(move || supervise(&running))
            .expect("start a device thread");
        PortHandle { shared, thread: Some(thread) }
    }
}

/// A change Windows reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeviceChange {
    /// The default device for a direction changed.
    Default(Direction),
    /// A device appeared, disappeared or changed state.
    Devices,
}

#[cfg_attr(not(windows), allow(dead_code))]
fn on_device_change(ports: &Weak<Mutex<Vec<Weak<PortShared>>>>, change: DeviceChange) {
    let Some(ports) = ports.upgrade() else {
        return;
    };
    for port in lock(&ports).iter().filter_map(Weak::upgrade) {
        let choice = lock(&port.choice).clone();
        if choice.is_off() {
            continue;
        }
        let reopen = match change {
            DeviceChange::Default(direction) => direction == port.direction && choice.is_default(),
            // A running stream notices its own device going; only ports
            // waiting for a device need a nudge.
            DeviceChange::Devices => port.stats().state() != PortState::Running,
        };
        if reopen {
            port.reopen();
        }
    }
}

/// How long a port waits before retrying a device that failed.
const RETRY: Duration = Duration::from_secs(3);

/// The port's thread: opens the chosen device and streams until told to
/// stop, the choice changes or the device fails, then starts over.
fn supervise(port: &PortShared) {
    loop {
        if port.stop.load(Ordering::Relaxed) {
            return;
        }
        let generation = port.generation.load(Ordering::Relaxed);
        let choice = lock(&port.choice).clone();
        if choice.is_off() {
            port.stats().set_state(PortState::Off);
            *lock(&port.status) = PortStatus::default();
            port.wake.wait(None);
            continue;
        }
        port.stats().set_state(PortState::Opening);
        let result = run(port, &choice, generation);
        if let Some(capture) = &port.capture {
            capture.end_stream();
        }
        if let Some(render) = &port.render {
            render.end_stream();
        }
        match result {
            Ok(()) => {}
            Err(err) => {
                port.stats().set_state(PortState::Failed);
                let mut status = lock(&port.status);
                if status.error != err {
                    logf!(port.logger, "DEVICE {} could not stream: {err}", port.label());
                }
                status.error = err;
                // The device it is trying to open, named if Windows can, so
                // the user can tell which one to look into.
                status.device_name = endpoint_name(&choice.endpoint).unwrap_or_default();
                drop(status);
                let until = Instant::now() + RETRY;
                while Instant::now() < until && !port.should_stop(generation) {
                    port.wake.wait(Some(until));
                }
            }
        }
    }
}

#[cfg(windows)]
fn run(port: &PortShared, choice: &DeviceChoice, generation: u64) -> Result<(), String> {
    match choice.endpoint.strip_prefix(APP_ENDPOINT) {
        Some(exe) if port.direction == Direction::Capture => crate::wasapi::run_app_stream(port, exe, generation),
        Some(_) => Err("only inputs can capture an application".into()),
        None => crate::wasapi::run_stream(port, choice, generation),
    }
}

#[cfg(not(windows))]
fn run(port: &PortShared, _: &DeviceChoice, generation: u64) -> Result<(), String> {
    while !port.should_stop(generation) {
        port.wake.wait(Some(Instant::now() + Duration::from_secs(1)));
    }
    Err("physical audio devices are only supported on Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_start_off_and_follow_their_choice() {
        let manager = DeviceManager::new(&Logger::discard(), 20.0);
        let port = manager.render("A1");
        assert!(port.output_port().is_some() && port.input_port().is_none());
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(port.reading().state, PortState::Off);
        port.configure(DeviceChoice { endpoint: "{no-such-device}".into(), exclusive: false });
        let deadline = Instant::now() + Duration::from_secs(10);
        while port.reading().state != PortState::Failed && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(port.reading().state, PortState::Failed);
        assert!(!port.status().error.is_empty());
        port.configure(DeviceChoice::default());
        let deadline = Instant::now() + Duration::from_secs(5);
        while port.reading().state != PortState::Off && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(port.reading().state, PortState::Off, "turning the port off stops the retries");
    }

    /// Any endpoint Windows knows is named, the default device, applications
    /// and unknown IDs are not.
    #[test]
    fn endpoints_are_named_as_windows_lists_them() {
        assert_eq!(endpoint_name(""), None);
        assert_eq!(endpoint_name(DEFAULT_ENDPOINT), None);
        assert_eq!(endpoint_name("app:Spotify.exe"), None);
        assert_eq!(endpoint_name("{0.0.1.00000000}.{00000000-0000-0000-0000-000000000000}"), None);
        for direction in [Direction::Capture, Direction::Render] {
            for device in list_devices(direction).unwrap_or_default() {
                assert_eq!(endpoint_name(&device.id).as_deref(), Some(device.name.as_str()));
            }
        }
    }
}
