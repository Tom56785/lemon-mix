//! The mixer host: the USB/IP server and its cables, the engine, the
//! physical devices and the mixer's settings, run by the window on demand or
//! by the Windows service.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cables::logf;
use cables::logging::Logger;
use cables::uac2::{CableAudio, CableKind, Descriptors, Device, bus_id};
use cables::usbip::Server;
use devices::{DeviceChoice, DeviceManager, Direction, PortHandle, PortState};
use engine::core::Clock;
use engine::{BusRing, BusSink, BusSpec, Engine, EngineCable, StripSource, StripSpec, Topology};

use crate::config::{Cable, SavedBus, SavedMixer, SavedStrip};
use crate::protocol::{AppView, BusView, ControlAction, DeviceView, Meter, MixerCommand, PortView, StripView};

/// The manufacturer string of every cable.
pub const MANUFACTURER: &str = "Lemon Mix";

/// The settings the host starts with.
pub struct HostSettings {
    pub address: String,
    pub latency_ms: i64,
    pub stall_test: Option<Duration>,
    pub diagnostics: bool,
    pub buffer_ms: i64,
    pub hardware_inputs: usize,
    pub hardware_outputs: usize,
    /// The active profile.
    pub profile: String,
}

pub struct MixerHost {
    pub logger: Logger,
    log_path: String,
    settings: HostSettings,
    engine: Arc<Engine>,
    /// Makes the hardware ports, and reopens them when Windows' devices change.
    devices: DeviceManager,
    /// The ports of hardware strips H1... and buses A1..., in order. Locked
    /// after the mixer state when both are needed.
    inputs: Mutex<Vec<PortHandle>>,
    outputs: Mutex<Vec<PortHandle>>,
    server: Mutex<ServerState>,
    mixer: Mutex<MixerState>,
    clock: Mutex<Option<Clock>>,
    device_lists: Mutex<(Vec<DeviceView>, Vec<DeviceView>)>,
    /// The applications with sound, refreshed in the background.
    apps: Mutex<Vec<AppView>>,
    /// The name of the active profile.
    profile: Mutex<String>,
    /// Every profile, read from the profiles folder when they change.
    profile_list: Mutex<Vec<String>>,
    /// When the settings last changed, for saving them a moment later.
    dirty_since: Mutex<Option<Instant>>,
    /// The last meter readings, shared by everyone who reads them close together.
    meter_cache: Mutex<MeterCache>,
    persist: bool,
    stopping: Arc<AtomicBool>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

struct ServerState {
    server: Option<Arc<Server>>,
    done: Option<mpsc::Receiver<Result<(), String>>>,
    cables: Vec<Cable>,
    /// Each cable's audio, kept across server restarts so its strip and bus
    /// carry on.
    audio: HashMap<u32, (Cable, Arc<EngineCable>)>,
    last_error: String,
}

struct MixerState {
    saved: SavedMixer,
    /// The strip and bus IDs in the engine's order.
    strips: Vec<String>,
    buses: Vec<String>,
    /// The cables the topology was built for.
    cables: Vec<Cable>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// The ID of hardware strip `n` (from 1) and of cable `id`'s virtual strip.
pub fn hardware_strip_id(n: usize) -> String {
    format!("H{n}")
}
pub fn virtual_strip_id(cable: u32) -> String {
    format!("V{cable}")
}
pub fn hardware_bus_id(n: usize) -> String {
    format!("A{n}")
}
pub fn virtual_bus_id(cable: u32) -> String {
    format!("B{cable}")
}

impl MixerHost {
    /// A host with these settings and saved mixer. With `persist`, changes to
    /// the mixer are written back to config.toml.
    pub fn new(
        settings: HostSettings,
        saved: SavedMixer,
        logger: &Logger,
        log_path: &str,
        persist: bool,
    ) -> Arc<MixerHost> {
        let devices = DeviceManager::new(logger, settings.buffer_ms as f64);
        let inputs = (1..=settings.hardware_inputs).map(|n| devices.capture(&hardware_strip_id(n))).collect();
        let outputs = (1..=settings.hardware_outputs).map(|n| devices.render(&hardware_bus_id(n))).collect();
        let host = Arc::new(MixerHost {
            logger: logger.clone(),
            log_path: log_path.to_string(),
            engine: Arc::new(Engine::new()),
            devices,
            inputs: Mutex::new(inputs),
            outputs: Mutex::new(outputs),
            server: Mutex::new(ServerState {
                server: None,
                done: None,
                cables: Vec::new(),
                audio: HashMap::new(),
                last_error: String::new(),
            }),
            mixer: Mutex::new(MixerState { saved, strips: Vec::new(), buses: Vec::new(), cables: Vec::new() }),
            clock: Mutex::new(None),
            device_lists: Mutex::new((Vec::new(), Vec::new())),
            apps: Mutex::new(Vec::new()),
            profile: Mutex::new(settings.profile.clone()),
            profile_list: Mutex::new(crate::config::list_profiles(&settings.profile)),
            dirty_since: Mutex::new(None),
            meter_cache: Mutex::default(),
            persist,
            stopping: Arc::new(AtomicBool::new(false)),
            workers: Mutex::new(Vec::new()),
            settings,
        });
        host.rebuild(&[]);
        host
    }

    /// Starts the engine's clock, the hardware devices and the background
    /// work: saving changed settings and the health report.
    pub fn start(self: &Arc<Self>) {
        let mut clock = lock(&self.clock);
        if clock.is_none() {
            *clock = Some(self.engine.start_clock(&self.logger, self.settings.stall_test));
        }
        drop(clock);
        self.refresh_devices();
        {
            // A service in session 0 may see other devices than the signed-in
            // user, or none: the log says what this process can use.
            let (capture, render) = &*lock(&self.device_lists);
            let default =
                |list: &[DeviceView]| list.iter().find(|d| d.is_default).map_or("none".into(), |d| d.name.clone());
            logf!(
                self.logger,
                "Devices visible to this process: {} recording (default {}), {} playback (default {})",
                capture.len(),
                default(capture),
                render.len(),
                default(render)
            );
        }
        self.configure_ports();
        let mut workers = lock(&self.workers);
        if workers.is_empty() {
            self.stopping.store(false, Ordering::Relaxed);
            let host = Arc::downgrade(self);
            let stopping = Arc::clone(&self.stopping);
            workers.push(
                std::thread::Builder::new()
                    .name("mixer housekeeping".into())
                    .spawn(move || housekeeping(&host, &stopping))
                    .expect("start the housekeeping thread"),
            );
        }
    }

    /// Points each hardware port at its saved device: none for a strip fed
    /// from a bus.
    fn configure_ports(&self) {
        let mixer = lock(&self.mixer);
        for (n, port) in lock(&self.inputs).iter().enumerate() {
            let choice = match mixer.saved.strips.iter().find(|s| s.id == hardware_strip_id(n + 1)) {
                Some(strip) if bus_source_key(&strip.device).is_none() => {
                    DeviceChoice { endpoint: strip.device.clone(), exclusive: strip.exclusive }
                }
                _ => DeviceChoice::default(),
            };
            port.configure(choice);
        }
        for (n, port) in lock(&self.outputs).iter().enumerate() {
            let choice = match mixer.saved.buses.iter().find(|b| b.id == hardware_bus_id(n + 1)) {
                Some(bus) => DeviceChoice { endpoint: bus.device.clone(), exclusive: bus.exclusive },
                None => DeviceChoice::default(),
            };
            port.configure(choice);
        }
    }

    /// The active profile and every profile there is.
    pub fn profiles(&self) -> (String, Vec<String>) {
        (lock(&self.profile).clone(), lock(&self.profile_list).clone())
    }

    /// Reads the profiles folder again, after a profile is made or deleted.
    fn refresh_profiles(&self) {
        let active = lock(&self.profile).clone();
        *lock(&self.profile_list) = crate::config::list_profiles(&active);
    }

    /// Makes a profile, from the current settings or (with `from_defaults`)
    /// those of a new configuration, and switches to it.
    fn create_profile(&self, name: &str, from_defaults: bool) -> Result<(), String> {
        let (_, existing) = self.profiles();
        let name = crate::config::validate_profile_name(name, &existing)?;
        let settings = if from_defaults {
            // A new configuration's: five hardware inputs and outputs with
            // default settings. Cables' strips and buses get theirs when the
            // profile is taken up.
            let cables = lock(&self.mixer).cables.clone();
            let ports = crate::config::DEFAULT_HARDWARE_PORTS as usize;
            SavedMixer {
                strips: (1..=ports).map(|n| default_strip(&hardware_strip_id(n), &cables)).collect(),
                buses: (1..=ports).map(|n| default_bus(&hardware_bus_id(n))).collect(),
            }
        } else {
            self.saved_mixer()
        };
        crate::config::save_profile(&name, &settings)
            .map_err(|err| format!("The profile could not be saved: {err}"))?;
        self.refresh_profiles();
        logf!(self.logger, "Created profile \"{name}\"");
        self.switch_profile(&name)
    }

    /// Deletes a profile other than the active one.
    fn delete_profile(&self, name: &str) -> Result<(), String> {
        if lock(&self.profile).eq_ignore_ascii_case(name) {
            return Err("The active profile cannot be deleted: switch to another one first.".into());
        }
        crate::config::delete_profile(name).map_err(|_| format!("The profile \"{name}\" was not found."))?;
        self.refresh_profiles();
        logf!(self.logger, "Deleted profile \"{name}\"");
        Ok(())
    }

    /// Saves the current settings to the active profile, and takes up those of
    /// profile `name`: its strips and buses, as many hardware ones as it has,
    /// and their devices.
    fn switch_profile(&self, name: &str) -> Result<(), String> {
        let (active, all) = self.profiles();
        let name =
            all.iter().find(|p| p.eq_ignore_ascii_case(name)).ok_or(format!("There is no profile \"{name}\"."))?;
        if *name == active {
            return Ok(());
        }
        let (saved, (inputs, outputs)) = crate::config::load_profile(name)?;
        crate::config::save_profile(&active, &self.saved_mixer())
            .map_err(|err| format!("The current settings could not be saved to \"{active}\": {err}"))?;
        let server = lock(&self.server);
        let mut mixer = lock(&self.mixer);
        let mut removed = Vec::new();
        for (ports, count, strip) in [(&self.inputs, inputs, true), (&self.outputs, outputs, false)] {
            let mut ports = lock(ports);
            while ports.len() > count {
                removed.extend(ports.pop());
            }
            while ports.len() < count {
                let n = ports.len() + 1;
                ports.push(if strip {
                    self.devices.capture(&hardware_strip_id(n))
                } else {
                    self.devices.render(&hardware_bus_id(n))
                });
            }
        }
        mixer.saved = saved;
        let cables = mixer.cables.clone();
        drop(mixer);
        self.rebuild_with(&cables, &server.audio);
        drop(server);
        // The ports keep their devices while the mixer runs; a suspended one
        // takes them up when it starts.
        if lock(&self.clock).is_some() {
            self.configure_ports();
        }
        // Closing a device waits for its stream to stop.
        drop(removed);
        *lock(&self.profile) = name.clone();
        self.refresh_profiles();
        crate::config::save_active_profile(name).map_err(|err| format!("The profile could not be recorded: {err}"))?;
        logf!(self.logger, "Switched to profile \"{name}\"");
        Ok(())
    }

    /// Stops the engine and lets the physical devices go, so another process
    /// (the Windows service) can take them; `start` takes them back.
    pub fn suspend(&self) {
        *lock(&self.clock) = None;
        for port in lock(&self.inputs).iter().chain(lock(&self.outputs).iter()) {
            port.configure(DeviceChoice::default());
        }
        self.save_now();
    }

    /// Stops the server, the engine and the devices, and saves the settings.
    pub fn shutdown(&self) {
        self.stop();
        self.stopping.store(true, Ordering::Relaxed);
        for worker in lock(&self.workers).drain(..) {
            let _ = worker.join();
        }
        self.suspend();
    }

    pub fn latency_ms(&self) -> i64 {
        self.settings.latency_ms
    }

    /// Makes the devices for `cables`, reusing each unchanged cable's audio.
    fn make_devices(&self, state: &mut ServerState, cables: &[Cable]) -> Result<Vec<Arc<Device>>, String> {
        let mut devices = Vec::new();
        for cable in cables {
            let audio = match state.audio.get(&cable.id) {
                Some((existing, audio)) if existing == cable => Arc::clone(audio),
                _ => {
                    let latency = cable.kind.has_playback().then_some(self.settings.latency_ms as u64);
                    let ring = cable.kind.has_recording().then(|| Arc::new(BusRing::new()));
                    let audio = Arc::new(EngineCable::new(&self.engine, latency, ring));
                    state.audio.insert(cable.id, (cable.clone(), Arc::clone(&audio)));
                    audio
                }
            };
            let descriptors = Descriptors::build(cable.kind, cable.id, &cable.serial, &cable.name, MANUFACTURER)
                .map_err(|err| format!("create cable {} ({}): {err}", cable.id, cable.name))?;
            devices.push(Arc::new(Device::with_audio(cable.id, descriptors, audio as Arc<dyn CableAudio>)));
        }
        state.audio.retain(|id, _| cables.iter().any(|c| c.id == *id));
        Ok(devices)
    }

    /// A device for `cable` wired to the engine, without the server: for the
    /// self-test.
    pub fn self_test_device(&self, cable: &Cable) -> Result<Arc<Device>, String> {
        let mut state = lock(&self.server);
        let devices = self.make_devices(&mut state, std::slice::from_ref(cable))?;
        drop(state);
        self.rebuild(std::slice::from_ref(cable));
        devices.into_iter().next().ok_or_else(|| "no device was made".to_string())
    }

    pub fn restart(&self, cables: &[Cable]) -> Result<(), String> {
        let mut state = lock(&self.server);
        self.stop_locked(&mut state);
        let devices = match self.make_devices(&mut state, cables) {
            Ok(devices) => devices,
            Err(err) => {
                self.set_error_locked(&mut state, &err);
                return Err(err);
            }
        };
        state.cables = cables.to_vec();
        self.rebuild_with(cables, &state.audio);
        let mut server = Server::new(self.settings.address.clone(), devices, self.logger.clone());
        server.diagnostics = self.settings.diagnostics;
        server.stall_test = self.settings.stall_test;
        let server = Arc::new(server);
        let (ready_tx, ready) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let running = Arc::clone(&server);
        std::thread::Builder::new()
            .name("usbip server".into())
            .spawn(move || {
                let result = running.serve(move |r| {
                    let _ = ready_tx.send(r);
                });
                let _ = done_tx.send(result);
            })
            .map_err(|e| e.to_string())?;

        let failure = match ready.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => {
                state.server = Some(server);
                state.done = Some(done);
                state.last_error.clear();
                logf!(self.logger, "Lemon Mix {} started with {} cable(s)", crate::VERSION, cables.len());
                return Ok(());
            }
            Ok(Err(err)) => err,
            Err(RecvTimeoutError::Disconnected) => "USB/IP server stopped before becoming ready".to_string(),
            Err(RecvTimeoutError::Timeout) => format!("timed out while opening USB/IP port {}", self.settings.address),
        };
        server.shutdown();
        self.set_error_locked(&mut state, &failure);
        Err(failure)
    }

    /// Brings the running server in line with `cables`: removed cables stop
    /// being exported, which closes their sessions, and new ones are added,
    /// while the others keep streaming. Starts the server if it is not
    /// running, and restarts it if the change cannot be applied in place.
    pub fn apply(&self, cables: &[Cable]) -> Result<(), String> {
        let mut state = lock(&self.server);
        let Some(server) = state.server.clone() else {
            drop(state);
            return self.restart(cables);
        };
        let added: Vec<Cable> = cables.iter().filter(|c| !state.cables.contains(c)).cloned().collect();
        let removed: Vec<Cable> = state.cables.iter().filter(|c| !cables.contains(c)).cloned().collect();
        for cable in &removed {
            server.remove_device(&bus_id(cable.id));
        }
        let devices = self.make_devices(&mut state, cables)?;
        state.cables = cables.to_vec();
        self.rebuild_with(cables, &state.audio);
        for device in devices.into_iter().filter(|d| added.iter().any(|c| c.id == d.number)) {
            if let Err(err) = server.add_device(device) {
                logf!(self.logger, "Could not add a cable in place ({err}); restarting the USB/IP server");
                drop(state);
                return self.restart(cables);
            }
        }
        if !added.is_empty() || !removed.is_empty() {
            logf!(
                self.logger,
                "Cables updated: {} added, {} removed, {} in all",
                added.len(),
                removed.len(),
                cables.len()
            );
        }
        Ok(())
    }

    pub fn stop(&self) {
        let mut state = lock(&self.server);
        self.stop_locked(&mut state);
    }

    pub fn last_error(&self) -> String {
        lock(&self.server).last_error.clone()
    }

    pub fn log_path(&self) -> String {
        self.log_path.clone()
    }

    pub fn address(&self) -> &str {
        &self.settings.address
    }

    pub fn cables(&self) -> Vec<Cable> {
        lock(&self.server).cables.clone()
    }

    /// Replaces the cables the host starts with, before it starts.
    pub fn set_initial_cables(&self, cables: Vec<Cable>) {
        lock(&self.server).cables = cables.clone();
        self.rebuild(&cables);
    }

    fn set_error_locked(&self, state: &mut ServerState, err: &str) {
        state.last_error = err.to_string();
        logf!(self.logger, "USB/IP server error: {err}");
    }

    fn stop_locked(&self, state: &mut ServerState) {
        let Some(server) = state.server.take() else {
            return;
        };
        server.shutdown();
        if let Some(done) = state.done.take() {
            match done.recv_timeout(Duration::from_secs(2)) {
                Ok(Err(err)) => logf!(self.logger, "USB/IP server stopped with error: {err}"),
                Ok(Ok(())) | Err(RecvTimeoutError::Disconnected) => {}
                Err(RecvTimeoutError::Timeout) => logf!(self.logger, "USB/IP server shutdown timed out; continuing"),
            }
        }
    }

    /// Rebuilds the engine's strips and buses for `cables`: the hardware
    /// strips and buses first, then each cable's virtual strip and bus.
    /// Settings that do not exist yet get their defaults.
    fn rebuild(&self, cables: &[Cable]) {
        let state = lock(&self.server);
        self.rebuild_with(cables, &state.audio);
    }

    /// `rebuild` for a caller that holds the server lock and its cable audio.
    fn rebuild_with(&self, cables: &[Cable], audio: &HashMap<u32, (Cable, Arc<EngineCable>)>) {
        let audio: HashMap<u32, Arc<EngineCable>> =
            audio.iter().map(|(id, (_, audio))| (*id, Arc::clone(audio))).collect();
        let mut mixer = lock(&self.mixer);
        let inputs = lock(&self.inputs);
        let outputs = lock(&self.outputs);
        let mut strip_ids: Vec<String> = (1..=inputs.len()).map(hardware_strip_id).collect();
        let mut bus_ids: Vec<String> = (1..=outputs.len()).map(hardware_bus_id).collect();
        for cable in cables {
            if cable.kind.has_playback() {
                strip_ids.push(virtual_strip_id(cable.id));
            }
            if cable.kind.has_recording() {
                bus_ids.push(virtual_bus_id(cable.id));
            }
        }
        for id in &strip_ids {
            if !mixer.saved.strips.iter().any(|s| &s.id == id) {
                let strip = default_strip(id, cables);
                mixer.saved.strips.push(strip);
            }
        }
        for id in &bus_ids {
            if !mixer.saved.buses.iter().any(|b| &b.id == id) {
                mixer.saved.buses.push(default_bus(id));
            }
        }
        if ensure_keys(&mut mixer.saved) {
            self.mark_dirty();
        }
        let mut topology = Topology::default();
        for id in &strip_ids {
            let saved = mixer.saved.strips.iter().find(|s| &s.id == id).expect("added above");
            let source = if let Some(n) = id.strip_prefix('H').and_then(|n| n.parse::<usize>().ok()) {
                match bus_source_key(&saved.device) {
                    // Fed from a bus: its index now, or silence if it is gone.
                    Some(key) => mixer
                        .saved
                        .buses
                        .iter()
                        .find(|b| b.key == key)
                        .and_then(|b| bus_ids.iter().position(|id| *id == b.id))
                        .map_or(StripSource::Hardware(None), StripSource::Bus),
                    None => StripSource::Hardware(inputs.get(n - 1).and_then(PortHandle::input_port)),
                }
            } else {
                let cable = id[1..].parse::<u32>().unwrap_or(0);
                match audio.get(&cable).and_then(|a| a.input.as_ref().map(|line| (line, &a.playback_active))) {
                    Some((line, active)) => {
                        StripSource::Virtual { line: Arc::clone(line), playback_active: Arc::clone(active) }
                    }
                    None => StripSource::Hardware(None),
                }
            };
            let routes = saved.routes.iter().filter_map(|r| bus_ids.iter().position(|b| b == r)).collect();
            topology.strips.push(StripSpec { source, params: saved.params.clone(), routes });
        }
        for id in &bus_ids {
            let saved = mixer.saved.buses.iter().find(|b| &b.id == id).expect("added above");
            let sink = if let Some(n) = id.strip_prefix('A').and_then(|n| n.parse::<usize>().ok()) {
                BusSink::Hardware(outputs.get(n - 1).and_then(PortHandle::output_port))
            } else {
                let cable = id[1..].parse::<u32>().unwrap_or(0);
                match audio.get(&cable).and_then(|a| a.output.clone()) {
                    Some(ring) => BusSink::Virtual(ring),
                    None => BusSink::Hardware(None),
                }
            };
            topology.buses.push(BusSpec { sink, params: saved.params.clone() });
        }
        drop((inputs, outputs));
        mixer.strips = strip_ids;
        mixer.buses = bus_ids;
        mixer.cables = cables.to_vec();
        self.engine.set_topology(topology);
    }

    /// Forgets the mixer settings of a removed cable, or of the side a cable
    /// no longer has, and every route to a bus that is gone.
    pub fn forget_cable_settings(&self, cable: u32, kind: Option<CableKind>) {
        let mut mixer = lock(&self.mixer);
        let (strip, bus) = (virtual_strip_id(cable), virtual_bus_id(cable));
        let keep_strip = kind.is_some_and(CableKind::has_playback);
        let keep_bus = kind.is_some_and(CableKind::has_recording);
        if !keep_strip {
            mixer.saved.strips.retain(|s| s.id != strip);
        }
        if !keep_bus {
            forget_bus(&mut mixer.saved, &bus);
        }
        drop(mixer);
        self.mark_dirty();
    }

    /// Chooses a hardware strip's or bus's device: "" for none, "default",
    /// an endpoint ID, or for a strip, `bus:<bus>` to take a bus's output
    /// (by key, ID or name; it is kept by key).
    fn set_device(&self, id: &str, endpoint: &str, exclusive: bool) -> Result<(), String> {
        let strip = id.starts_with('H');
        let from_bus = match endpoint.strip_prefix(BUS_SOURCE) {
            Some(_) if !strip => return Err("Only input strips can take a bus's output.".into()),
            Some(bus) => Some(self.resolve(bus, Want::Bus)?.0),
            None => None,
        };
        let app = endpoint.strip_prefix(devices::APP_ENDPOINT).map(str::trim);
        match app {
            Some(_) if !strip => return Err("Only input strips can capture an application.".into()),
            Some("") => return Err("Name the application's program, such as app:Spotify.exe.".into()),
            _ => {}
        }
        // Capturing an application has no exclusive mode.
        let exclusive = exclusive && app.is_none();
        let mut mixer = lock(&self.mixer);
        let mut saved = mixer.saved.clone();
        let device = match &from_bus {
            Some(bus) => {
                let key = saved.buses.iter().find(|b| &b.id == bus).map(|b| b.key.clone()).unwrap_or_default();
                format!("{BUS_SOURCE}{key}")
            }
            None => endpoint.to_string(),
        };
        let exclusive = exclusive && from_bus.is_none();
        let was_from_bus;
        if strip {
            let s = saved.strips.iter_mut().find(|s| s.id == id).ok_or("That strip no longer exists.")?;
            was_from_bus = bus_source_key(&s.device).is_some();
            (s.device, s.exclusive) = (device.clone(), exclusive);
            if let Some(bus) = &from_bus {
                // A strip fed from a bus does not send back to it.
                s.routes.retain(|r| r != bus);
            }
            if let Some(bus) = feedback_loop(&saved) {
                return Err(format!("That would feed {} back into itself: stop sending it there first.", bus));
            }
        } else {
            let b = saved.buses.iter_mut().find(|b| b.id == id).ok_or("That bus no longer exists.")?;
            was_from_bus = false;
            (b.device, b.exclusive) = (device.clone(), exclusive);
        }
        // A strip fed from a bus lets its device go.
        let choice =
            if from_bus.is_some() { DeviceChoice::default() } else { DeviceChoice { endpoint: device, exclusive } };
        let ports = lock(if strip { &self.inputs } else { &self.outputs });
        let port = id[1..].parse::<usize>().ok().and_then(|n| ports.get(n.wrapping_sub(1)));
        port.ok_or("Only hardware strips and buses have devices.")?.configure(choice);
        drop(ports);
        mixer.saved = saved;
        let cables = mixer.cables.clone();
        drop(mixer);
        if from_bus.is_some() || was_from_bus {
            self.rebuild(&cables);
        }
        Ok(())
    }

    /// Carries out a mixer command.
    pub fn command(&self, command: &MixerCommand) -> Result<(), String> {
        match command {
            MixerCommand::RefreshDevices => {
                self.refresh_devices();
                return Ok(());
            }
            MixerCommand::CreateProfile { name, from_defaults } => self.create_profile(name, *from_defaults)?,
            MixerCommand::DeleteProfile { name } => return self.delete_profile(name),
            MixerCommand::SwitchProfile { name } => self.switch_profile(name)?,
            MixerCommand::AddStrip => self.add_hardware(true),
            MixerCommand::AddBus => self.add_hardware(false),
            MixerCommand::RemoveStrip { id } => self.remove_hardware(id, true)?,
            MixerCommand::RemoveBus { id } => self.remove_hardware(id, false)?,
            MixerCommand::Device { id, endpoint, exclusive } => {
                if !(id.starts_with('H') || id.starts_with('A')) {
                    return Err("Only hardware strips and buses have devices.".into());
                }
                self.set_device(id, endpoint.trim(), *exclusive)?;
            }
            MixerCommand::Strip { id, params } => {
                let mut mixer = lock(&self.mixer);
                let index = mixer.strips.iter().position(|s| s == id).ok_or("That strip no longer exists.")?;
                let strip =
                    mixer.saved.strips.iter_mut().find(|s| &s.id == id).ok_or("That strip no longer exists.")?;
                strip.params = params.clone().sanitized();
                self.engine.set_strip(index, strip.params.clone());
            }
            MixerCommand::Route { strip: id, bus, on } => {
                let mut mixer = lock(&self.mixer);
                let index = mixer.strips.iter().position(|s| s == id).ok_or("That strip no longer exists.")?;
                let bus_index = mixer.buses.iter().position(|b| b == bus).ok_or("That bus no longer exists.")?;
                let buses = mixer.buses.clone();
                let mut saved = mixer.saved.clone();
                let strip = saved.strips.iter_mut().find(|s| &s.id == id).ok_or("That strip no longer exists.")?;
                strip.routes.retain(|r| r != bus);
                if *on {
                    strip.routes.push(bus.clone());
                    strip.routes.sort_by_key(|r| buses.iter().position(|b| b == r));
                }
                if let Some(looped) = feedback_loop(&saved) {
                    return Err(format!("That would feed {looped} back into itself."));
                }
                mixer.saved = saved;
                self.engine.set_route(index, bus_index, *on);
            }
            MixerCommand::Bus { id, params } => {
                let mut mixer = lock(&self.mixer);
                let index = mixer.buses.iter().position(|b| b == id).ok_or("That bus no longer exists.")?;
                let bus = mixer.saved.buses.iter_mut().find(|b| &b.id == id).ok_or("That bus no longer exists.")?;
                bus.params = params.clone().sanitized();
                self.engine.set_bus(index, bus.params.clone());
            }
            MixerCommand::Label { id, label } => {
                let label: String = label.trim().chars().take(crate::config::MAX_CABLE_NAME).collect();
                let mut mixer = lock(&self.mixer);
                if let Some(strip) = mixer.saved.strips.iter_mut().find(|s| &s.id == id) {
                    strip.label = label;
                } else if let Some(bus) = mixer.saved.buses.iter_mut().find(|b| &b.id == id) {
                    bus.label = label;
                } else {
                    return Err("That strip or bus no longer exists.".into());
                }
            }
        }
        self.mark_dirty();
        Ok(())
    }

    /// Lists the physical devices again.
    pub fn refresh_devices(&self) {
        let list = |direction| {
            devices::list_devices(direction)
                .unwrap_or_default()
                .into_iter()
                .map(|d| DeviceView { id: d.id, name: d.name, is_default: d.is_default })
                .collect::<Vec<_>>()
        };
        *lock(&self.device_lists) = (list(Direction::Capture), list(Direction::Render));
    }

    pub fn device_lists(&self) -> (Vec<DeviceView>, Vec<DeviceView>) {
        lock(&self.device_lists).clone()
    }

    /// Lists the applications with sound again.
    pub fn refresh_apps(&self) {
        let apps = devices::list_apps()
            .unwrap_or_default()
            .into_iter()
            .map(|a| AppView { exe: a.exe, name: a.name, active: a.active })
            .collect();
        *lock(&self.apps) = apps;
    }

    pub fn apps(&self) -> Vec<AppView> {
        lock(&self.apps).clone()
    }

    /// The strips and buses as the window shows them, with their meters
    /// since the last call.
    pub fn views(&self) -> (Vec<StripView>, Vec<BusView>) {
        let mixer = lock(&self.mixer);
        let cable_name = |id: &str| {
            let n: u32 = id[1..].parse().unwrap_or(0);
            mixer.cables.iter().find(|c| c.id == n).map(|c| c.name.clone()).unwrap_or_default()
        };
        let (strip_meters, bus_meters) = self.meter_readings();
        let meter = |m: Option<&engine::MeterReading>| {
            let r = m.copied().unwrap_or(engine::MeterReading { reduction: 1.0, ..Default::default() });
            Meter { peak: r.peak, rms: r.rms, reduction: r.reduction }
        };
        let inputs = lock(&self.inputs);
        let outputs = lock(&self.outputs);
        let port = |ports: &[PortHandle], index: usize, saved_device: &str, exclusive: bool| match ports.get(index) {
            Some(port) => port_view(port, saved_device.to_string(), exclusive),
            None => PortView { endpoint: saved_device.to_string(), exclusive, ..Default::default() },
        };
        // The name a bus is shown by, for strips fed from it.
        let bus_label = |key: &str| {
            let bus = mixer.saved.buses.iter().find(|b| b.key == key)?;
            Some(if !bus.label.is_empty() {
                bus.label.clone()
            } else if bus.id.starts_with('A') {
                bus.id.clone()
            } else {
                cable_name(&bus.id)
            })
        };
        let strips = mixer
            .strips
            .iter()
            .enumerate()
            .filter_map(|(index, id)| {
                let saved = mixer.saved.strips.iter().find(|s| &s.id == id)?;
                let hardware = id.starts_with('H');
                let label = if !saved.label.is_empty() {
                    saved.label.clone()
                } else if hardware {
                    format!("Hardware {}", &id[1..])
                } else {
                    cable_name(id)
                };
                let port = hardware.then(|| match bus_source_key(&saved.device) {
                    // Fed from a bus: no device of its own.
                    Some(key) => PortView {
                        endpoint: saved.device.clone(),
                        state: "bus".into(),
                        device_name: bus_label(key).unwrap_or_default(),
                        ..Default::default()
                    },
                    None => port(&inputs, index, &saved.device, saved.exclusive),
                });
                Some(StripView {
                    id: id.clone(),
                    key: saved.key.clone(),
                    label,
                    params: saved.params.clone(),
                    routes: saved.routes.clone(),
                    meter: meter(strip_meters.get(index)),
                    port,
                })
            })
            .collect();
        let buses = mixer
            .buses
            .iter()
            .enumerate()
            .filter_map(|(index, id)| {
                let saved = mixer.saved.buses.iter().find(|b| &b.id == id)?;
                let hardware = id.starts_with('A');
                let label = if !saved.label.is_empty() {
                    saved.label.clone()
                } else if hardware {
                    id.clone()
                } else {
                    cable_name(id)
                };
                let port = hardware.then(|| port(&outputs, index, &saved.device, saved.exclusive));
                Some(BusView {
                    id: id.clone(),
                    key: saved.key.clone(),
                    label,
                    params: saved.params.clone(),
                    meter: meter(bus_meters.get(index)),
                    port,
                })
            })
            .collect();
        (strips, buses)
    }

    /// Every strip's and bus's meter. Reading a meter resets its peak, so
    /// readers close together (the window and controllers) share one reading
    /// rather than each seeing part of the peaks.
    fn meter_readings(&self) -> (Vec<engine::MeterReading>, Vec<engine::MeterReading>) {
        let bank = self.engine.meters();
        let mut cache = lock(&self.meter_cache);
        let fresh = cache.at.is_some_and(|at| at.elapsed() < METER_SHARING)
            && cache.bank.as_ref().is_some_and(|b| Arc::ptr_eq(b, &bank));
        if !fresh {
            cache.strips = bank.strips.iter().map(engine::meters::Meter::take).collect();
            cache.buses = bank.buses.iter().map(engine::meters::Meter::take).collect();
            cache.at = Some(Instant::now());
            cache.bank = Some(bank);
        }
        (cache.strips.clone(), cache.buses.clone())
    }

    /// Finds the strip or bus a controller names (see `ControlAction`).
    /// Returns its ID and whether it is a strip.
    pub fn resolve(&self, target: &str, want: Want) -> Result<(String, bool), String> {
        let target = target.trim();
        let (strips, buses) = self.views();
        let strips = strips.iter().map(|s| (&s.id, &s.key, &s.label, true));
        let buses = buses.iter().map(|b| (&b.id, &b.key, &b.label, false));
        let all: Vec<_> = strips.chain(buses).filter(|&(.., strip)| want.allows(strip)).collect();
        let (name, kinds) = match target.split_once(':') {
            Some((kind, name)) if kind.eq_ignore_ascii_case("strip") => (name.trim(), Want::Strip),
            Some((kind, name)) if kind.eq_ignore_ascii_case("bus") => (name.trim(), Want::Bus),
            _ => (target, Want::Either),
        };
        if kinds == Want::Either
            && let Some(&(id, .., strip)) =
                all.iter().find(|(id, key, ..)| *key == target || id.eq_ignore_ascii_case(target))
        {
            return Ok((id.clone(), strip));
        }
        let named: Vec<_> = all
            .iter()
            .filter(|&&(_, _, label, strip)| kinds.allows(strip) && label.eq_ignore_ascii_case(name))
            .collect();
        match named.as_slice() {
            [(id, .., strip)] => Ok(((*id).clone(), *strip)),
            [] => Err(format!("There is no {} named \"{target}\".", want.noun())),
            _ => Err(format!(
                "More than one strip or bus is named \"{name}\": use strip:{name} or bus:{name}, or an ID such as {}.",
                named.iter().map(|(id, ..)| id.as_str()).collect::<Vec<_>>().join(" or ")
            )),
        }
    }

    /// Carries out a controller's change.
    pub fn control(&self, action: &ControlAction) -> Result<(), String> {
        let switch = |current: bool, on: &Option<bool>| on.unwrap_or(!current);
        match action {
            ControlAction::Mute { target, on } => match self.resolve(target, Want::Either)? {
                (id, true) => self.update_strip(&id, |p| p.mute = switch(p.mute, on)),
                (id, false) => self.update_bus(&id, |p| p.mute = switch(p.mute, on)),
            },
            ControlAction::Solo { target, on } => {
                let (id, _) = self.resolve(target, Want::Strip)?;
                self.update_strip(&id, |p| p.solo = switch(p.solo, on))
            }
            ControlAction::Mono { target, on } => match self.resolve(target, Want::Either)? {
                (id, true) => self.update_strip(&id, |p| p.mono = switch(p.mono, on)),
                (id, false) => self.update_bus(&id, |p| p.mono = switch(p.mono, on)),
            },
            ControlAction::Gain { target, db } | ControlAction::AdjustGain { target, db } => {
                if !db.is_finite() {
                    return Err("The gain must be a number of dB.".into());
                }
                let relative = matches!(action, ControlAction::AdjustGain { .. });
                let gain = |current: f32| if relative { current + db } else { *db };
                match self.resolve(target, Want::Either)? {
                    (id, true) => self.update_strip(&id, |p| p.gain_db = gain(p.gain_db)),
                    (id, false) => self.update_bus(&id, |p| p.gain_db = gain(p.gain_db)),
                }
            }
            ControlAction::Route { strip, bus, on } => {
                let (strip, _) = self.resolve(strip, Want::Strip)?;
                let (bus, _) = self.resolve(bus, Want::Bus)?;
                let mixer = lock(&self.mixer);
                let current =
                    mixer.saved.strips.iter().find(|s| s.id == strip).is_some_and(|s| s.routes.contains(&bus));
                drop(mixer);
                self.command(&MixerCommand::Route { strip, bus, on: switch(current, on) })
            }
        }
    }

    /// Changes a strip's settings in place.
    fn update_strip(&self, id: &str, change: impl FnOnce(&mut engine::StripParams)) -> Result<(), String> {
        let mut mixer = lock(&self.mixer);
        let index = mixer.strips.iter().position(|s| s == id).ok_or("That strip no longer exists.")?;
        let strip = mixer.saved.strips.iter_mut().find(|s| s.id == id).ok_or("That strip no longer exists.")?;
        change(&mut strip.params);
        strip.params = strip.params.clone().sanitized();
        self.engine.set_strip(index, strip.params.clone());
        drop(mixer);
        self.mark_dirty();
        Ok(())
    }

    /// Changes a bus's settings in place.
    fn update_bus(&self, id: &str, change: impl FnOnce(&mut engine::BusParams)) -> Result<(), String> {
        let mut mixer = lock(&self.mixer);
        let index = mixer.buses.iter().position(|b| b == id).ok_or("That bus no longer exists.")?;
        let bus = mixer.saved.buses.iter_mut().find(|b| b.id == id).ok_or("That bus no longer exists.")?;
        change(&mut bus.params);
        bus.params = bus.params.clone().sanitized();
        self.engine.set_bus(index, bus.params.clone());
        drop(mixer);
        self.mark_dirty();
        Ok(())
    }

    /// The settings to save: a table for every strip and bus, which is also
    /// how many hardware ones there are.
    pub fn saved_mixer(&self) -> SavedMixer {
        lock(&self.mixer).saved.clone()
    }

    /// Adds a hardware strip (`strip`) or bus after the last one, with no
    /// device and default settings.
    fn add_hardware(&self, strip: bool) {
        let server = lock(&self.server);
        let mut mixer = lock(&self.mixer);
        let mut ports = lock(if strip { &self.inputs } else { &self.outputs });
        let id = if strip { hardware_strip_id(ports.len() + 1) } else { hardware_bus_id(ports.len() + 1) };
        ports.push(if strip { self.devices.capture(&id) } else { self.devices.render(&id) });
        drop(ports);
        // Settings left from an earlier strip or bus of this number, as after
        // config.toml was edited by hand, do not come back with it.
        if strip {
            mixer.saved.strips.retain(|s| s.id != id);
        } else {
            forget_bus(&mut mixer.saved, &id);
        }
        let cables = mixer.cables.clone();
        drop(mixer);
        self.rebuild_with(&cables, &server.audio);
        logf!(self.logger, "Added hardware {} {id}", if strip { "strip" } else { "bus" });
    }

    /// Removes hardware strip `H<n>` or bus `A<n>` with its settings, and
    /// every route to a removed bus. The strips or buses after it move up a
    /// number, keeping their devices and settings.
    fn remove_hardware(&self, id: &str, strip: bool) -> Result<(), String> {
        let letter = if strip { 'H' } else { 'A' };
        let gone = if strip { "That strip no longer exists." } else { "That bus no longer exists." };
        let n = id.strip_prefix(letter).and_then(|n| n.parse::<usize>().ok()).filter(|&n| n >= 1).ok_or(
            if id.starts_with(['V', 'B']) {
                "A virtual device's strip and bus go with the device: remove it from the virtual devices instead."
            } else {
                gone
            },
        )?;
        let server = lock(&self.server);
        let mut mixer = lock(&self.mixer);
        let mut ports = lock(if strip { &self.inputs } else { &self.outputs });
        if n > ports.len() {
            return Err(gone.into());
        }
        let removed = ports.remove(n - 1);
        for (index, port) in ports.iter().enumerate().skip(n - 1) {
            port.set_label(&format!("{letter}{}", index + 1));
        }
        drop(ports);
        // H<n+1> becomes H<n>, and so on.
        let moved = |other: &str| -> Option<String> {
            let number = other.strip_prefix(letter)?.parse::<usize>().ok()?;
            (number > n).then(|| format!("{letter}{}", number - 1))
        };
        if strip {
            mixer.saved.strips.retain(|s| s.id != id);
            for s in &mut mixer.saved.strips {
                if let Some(new) = moved(&s.id) {
                    s.id = new;
                }
            }
        } else {
            forget_bus(&mut mixer.saved, id);
            for b in &mut mixer.saved.buses {
                if let Some(new) = moved(&b.id) {
                    b.id = new;
                }
            }
            for s in &mut mixer.saved.strips {
                for route in &mut s.routes {
                    if let Some(new) = moved(route) {
                        *route = new;
                    }
                }
            }
        }
        let cables = mixer.cables.clone();
        drop(mixer);
        self.rebuild_with(&cables, &server.audio);
        drop(server);
        // Closing the device waits for its stream to stop.
        drop(removed);
        logf!(self.logger, "Removed hardware {} {id}", if strip { "strip" } else { "bus" });
        Ok(())
    }

    fn mark_dirty(&self) {
        let mut dirty = lock(&self.dirty_since);
        if dirty.is_none() {
            *dirty = Some(Instant::now());
        }
    }

    /// Saves the mixer settings if they changed.
    fn save_now(&self) {
        if lock(&self.dirty_since).take().is_none() || !self.persist {
            return;
        }
        // The settings live in the active profile's file.
        let profile = lock(&self.profile).clone();
        if let Err(err) = crate::config::save_profile(&profile, &self.saved_mixer()) {
            logf!(self.logger, "Could not save the mixer settings to profile \"{profile}\": {err}");
        }
    }
}

/// What a hardware strip's device starts with when it takes a bus's output
/// instead: `bus:` and the bus's key.
pub const BUS_SOURCE: &str = "bus:";

/// The key of the bus a strip's device names, if it names one.
pub fn bus_source_key(device: &str) -> Option<&str> {
    device.strip_prefix(BUS_SOURCE)
}

/// Forgets a bus that is gone: its settings, every route to it, and every
/// strip fed from it.
fn forget_bus(saved: &mut SavedMixer, id: &str) {
    let key = saved.buses.iter().find(|b| b.id == id).map(|b| b.key.clone());
    saved.buses.retain(|b| b.id != id);
    for s in &mut saved.strips {
        s.routes.retain(|r| r != id);
        if key.as_deref().is_some_and(|key| bus_source_key(&s.device) == Some(key)) {
            s.device.clear();
        }
    }
}

/// A bus that would hear itself: one that feeds a strip that sends, directly
/// or through other fed strips, back to it. Strips fed from a bus hear it a
/// frame late, so a loop would not hang the engine, but it would howl.
fn feedback_loop(saved: &SavedMixer) -> Option<String> {
    // The buses a bus reaches in one step: where the strips it feeds send.
    let next = |bus: &str| -> Vec<String> {
        let Some(key) = saved.buses.iter().find(|b| b.id == bus).map(|b| b.key.as_str()) else {
            return Vec::new();
        };
        saved.strips.iter().filter(|s| bus_source_key(&s.device) == Some(key)).flat_map(|s| s.routes.clone()).collect()
    };
    for start in saved.buses.iter().map(|b| b.id.as_str()) {
        let mut seen: Vec<String> = Vec::new();
        let mut stack = next(start);
        while let Some(bus) = stack.pop() {
            if bus == start {
                return Some(start.to_string());
            }
            if !seen.contains(&bus) {
                stack.extend(next(&bus));
                seen.push(bus);
            }
        }
    }
    None
}

/// Which a controller's target may name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    Strip,
    Bus,
    Either,
}

impl Want {
    fn allows(self, strip: bool) -> bool {
        match self {
            Want::Strip => strip,
            Want::Bus => !strip,
            Want::Either => true,
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Want::Strip => "strip",
            Want::Bus => "bus",
            Want::Either => "strip or bus",
        }
    }
}

/// Meter readings taken less than this long ago are shared.
const METER_SHARING: Duration = Duration::from_millis(25);

#[derive(Default)]
struct MeterCache {
    /// The bank the readings came from: a new topology has a new one.
    bank: Option<Arc<engine::MeterBank>>,
    at: Option<Instant>,
    strips: Vec<engine::MeterReading>,
    buses: Vec<engine::MeterReading>,
}

/// Gives every strip and bus without a key, or with one an earlier one has
/// (as in a file edited by hand), a new key. Returns whether any changed.
fn ensure_keys(saved: &mut SavedMixer) -> bool {
    let mut taken: Vec<String> = Vec::new();
    let mut changed = false;
    let keys = saved.strips.iter_mut().map(|s| &mut s.key).chain(saved.buses.iter_mut().map(|b| &mut b.key));
    for key in keys {
        if key.is_empty() || taken.contains(key) {
            let refs: Vec<&str> = taken.iter().map(String::as_str).collect();
            *key = crate::config::new_key(&refs);
            changed = true;
        }
        taken.push(key.clone());
    }
    changed
}

/// The settings of a strip the user has not touched yet.
fn default_strip(id: &str, cables: &[Cable]) -> SavedStrip {
    let routes = match id.split_at(1) {
        // A loopback cable starts as the original cable: its playback side
        // goes to its own recording side.
        ("V", n) => match cables.iter().find(|c| Some(c.id) == n.parse().ok()) {
            Some(c) if c.kind == CableKind::Loopback => vec![virtual_bus_id(c.id)],
            _ => vec![hardware_bus_id(1)],
        },
        _ => vec![hardware_bus_id(1)],
    };
    SavedStrip {
        id: id.to_string(),
        key: String::new(),
        label: String::new(),
        params: engine::StripParams::default(),
        routes,
        device: String::new(),
        exclusive: false,
    }
}

/// The settings of a bus the user has not touched yet: A1 plays on the
/// system's default device.
fn default_bus(id: &str) -> SavedBus {
    SavedBus {
        id: id.to_string(),
        key: String::new(),
        label: String::new(),
        params: engine::BusParams::default(),
        device: if id == "A1" { devices::DEFAULT_ENDPOINT.to_string() } else { String::new() },
        exclusive: false,
    }
}

fn port_view(port: &PortHandle, endpoint: String, exclusive: bool) -> PortView {
    let reading = port.reading();
    let status = port.status();
    PortView {
        endpoint,
        exclusive,
        state: reading.state.name().to_string(),
        device_name: status.device_name,
        error: status.error,
        rate: reading.device_rate,
        fill_ms: reading.fill_ms,
        correction_ppm: reading.correction_ppm,
        underruns: reading.underruns,
        overruns: reading.overruns,
    }
}

/// How often the health report may log.
const HEALTH_INTERVAL: Duration = Duration::from_secs(10);
/// Running devices are reported at least this often, for their ratio.
const DEVICE_REPORT_INTERVAL: Duration = Duration::from_secs(60);
/// How often the list of applications with sound is refreshed.
const APPS_INTERVAL: Duration = Duration::from_secs(2);
/// Settings are saved this long after the last change.
const SAVE_DELAY: Duration = Duration::from_secs(1);

/// Saves changed settings and logs the engine's and devices' health.
fn housekeeping(host: &std::sync::Weak<MixerHost>, stopping: &AtomicBool) {
    let mut next_health = Instant::now() + HEALTH_INTERVAL;
    let mut next_apps = Instant::now();
    let mut engine_seen: Option<(u64, u64)> = None;
    let mut devices_seen: HashMap<String, (PortState, u64, u64, u64, Instant)> = HashMap::new();
    while !stopping.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
        let Some(host) = host.upgrade() else {
            return;
        };
        let due = lock(&host.dirty_since).is_some_and(|since| since.elapsed() >= SAVE_DELAY);
        if due {
            host.save_now();
        }
        if Instant::now() >= next_apps {
            next_apps = Instant::now() + APPS_INTERVAL;
            host.refresh_apps();
        }
        if Instant::now() < next_health {
            continue;
        }
        next_health = Instant::now() + HEALTH_INTERVAL;
        let stats = &host.engine.stats;
        let catch_up = stats.catch_up_batches.load(Ordering::Relaxed);
        let skipped = stats.skipped.load(Ordering::Relaxed);
        let max_batch = stats.max_batch.swap(0, Ordering::Relaxed);
        // Batches of a frame or two are the clock thread's normal wake-up
        // jitter; longer ones mean the coarse clock jumped or the thread was
        // held up.
        if engine_seen.is_some_and(|seen| seen.1 != skipped) || max_batch >= 5 {
            logf!(
                host.logger,
                "ENGINE HEALTH frames={} batches={} catch_up_batches={catch_up} max_batch_frames={max_batch} skipped_frames={skipped} pulled_by_capture={}",
                stats.frames.load(Ordering::Relaxed),
                stats.batches.load(Ordering::Relaxed),
                stats.pulled.load(Ordering::Relaxed)
            );
        }
        engine_seen = Some((catch_up, skipped));
        let ports: Vec<(String, devices::PortReading, devices::PortStatus)> = lock(&host.inputs)
            .iter()
            .chain(lock(&host.outputs).iter())
            .map(|port| (port.label(), port.reading(), port.status()))
            .collect();
        for (key, r, status) in ports {
            if r.state == PortState::Off {
                continue;
            }
            let now = Instant::now();
            let changed = match devices_seen.get(&key) {
                Some(&(state, underruns, overruns, opens, at)) => {
                    (state, underruns, overruns, opens) != (r.state, r.underruns, r.overruns, r.opens)
                        || (r.state == PortState::Running && now - at >= DEVICE_REPORT_INTERVAL)
                }
                None => r.state == PortState::Running,
            };
            if changed {
                logf!(
                    host.logger,
                    "DEVICE HEALTH {key} \"{}\" state={} rate={} ratio={:.6} correction_ppm={:+.1} fill_ms={:.1} underruns={} overruns={} opens={}",
                    status.device_name,
                    r.state.name(),
                    r.device_rate,
                    r.ratio,
                    r.correction_ppm,
                    r.fill_ms,
                    r.underruns,
                    r.overruns,
                    r.opens
                );
                devices_seen.insert(key, (r.state, r.underruns, r.overruns, r.opens, now));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cables::audio::PACKET_BYTES;
    use cables::uac2::{REQUEST_SET_CONFIGURATION, REQUEST_SET_INTERFACE, SetupPacket};

    fn settings() -> HostSettings {
        HostSettings {
            address: "127.0.0.1:0".into(),
            latency_ms: 20,
            stall_test: None,
            diagnostics: false,
            buffer_ms: 20,
            hardware_inputs: 2,
            hardware_outputs: 2,
            profile: crate::config::DEFAULT_PROFILE.into(),
        }
    }

    /// How many hardware strips and buses the saved settings have tables for.
    fn hardware_tables(saved: &SavedMixer) -> (usize, usize) {
        let count = |ids: Vec<&String>, letter| ids.iter().filter(|id| id.starts_with(letter)).count();
        (
            count(saved.strips.iter().map(|s| &s.id).collect(), 'H'),
            count(saved.buses.iter().map(|b| &b.id).collect(), 'A'),
        )
    }

    fn cable(id: u32, kind: CableKind) -> Cable {
        Cable { id, serial: format!("S{id}"), name: format!("C{id}"), kind }
    }

    #[test]
    fn topology_follows_the_cables() {
        let host = MixerHost::new(settings(), SavedMixer::default(), &Logger::discard(), "", false);
        host.apply(&[cable(1, CableKind::Loopback), cable(2, CableKind::Input), cable(3, CableKind::Output)]).unwrap();
        let (strips, buses) = host.views();
        let ids = |v: Vec<String>| v;
        assert_eq!(ids(strips.iter().map(|s| s.id.clone()).collect()), ["H1", "H2", "V1", "V2"]);
        assert_eq!(ids(buses.iter().map(|b| b.id.clone()).collect()), ["A1", "A2", "B1", "B3"]);
        assert_eq!(strips[2].routes, ["B1"], "a loopback cable starts as a cable");
        assert_eq!(strips[3].routes, ["A1"], "an input cable starts on A1");
        assert_eq!(strips[2].label, "C1");
        assert!(strips[0].port.is_some() && strips[2].port.is_none());
        assert_eq!(buses[0].port.as_ref().unwrap().endpoint, "default");

        // Routing and settings go to the engine and the saved state.
        host.command(&MixerCommand::Route { strip: "V2".into(), bus: "B3".into(), on: true }).unwrap();
        host.command(&MixerCommand::Route { strip: "V2".into(), bus: "A1".into(), on: false }).unwrap();
        assert!(host.command(&MixerCommand::Route { strip: "V9".into(), bus: "A1".into(), on: true }).is_err());
        let saved = host.saved_mixer();
        assert_eq!(saved.strips.iter().find(|s| s.id == "V2").unwrap().routes, ["B3"]);

        // Removing cable 3 forgets its bus and every route to it.
        host.apply(&[cable(1, CableKind::Loopback), cable(2, CableKind::Input)]).unwrap();
        host.forget_cable_settings(3, None);
        let saved = host.saved_mixer();
        assert!(saved.buses.iter().all(|b| b.id != "B3"));
        assert!(saved.strips.iter().find(|s| s.id == "V2").unwrap().routes.is_empty());
        host.shutdown();
    }

    /// Hardware strips and buses are added and removed while the mixer runs;
    /// the ones after a removed one move up with their settings and routes.
    #[test]
    fn hardware_strips_and_buses_come_and_go() {
        let host = MixerHost::new(settings(), SavedMixer::default(), &Logger::discard(), "", false);
        host.apply(&[cable(1, CableKind::Input)]).unwrap();
        let ids = |host: &MixerHost| {
            let (strips, buses) = host.views();
            (strips.into_iter().map(|s| s.id).collect::<Vec<_>>(), buses.into_iter().map(|b| b.id).collect::<Vec<_>>())
        };
        for _ in 0..70 {
            host.command(&MixerCommand::AddBus).unwrap();
        }
        host.command(&MixerCommand::AddStrip).unwrap();
        let (strips, buses) = ids(&host);
        assert_eq!(strips, ["H1", "H2", "H3", "V1"]);
        assert_eq!(buses.len(), 72);
        assert_eq!(buses[71], "A72", "past the 64 buses a bit mask could hold");
        assert_eq!(hardware_tables(&host.saved_mixer()), (3, 72));

        // H3 sends to A72 and A3, and has a label.
        host.command(&MixerCommand::Route { strip: "H3".into(), bus: "A72".into(), on: true }).unwrap();
        host.command(&MixerCommand::Route { strip: "H3".into(), bus: "A3".into(), on: true }).unwrap();
        host.command(&MixerCommand::Label { id: "H3".into(), label: "Mic".into() }).unwrap();

        // Removing H2 and A3: H3 becomes H2 and A72 becomes A71.
        host.command(&MixerCommand::RemoveStrip { id: "H2".into() }).unwrap();
        host.command(&MixerCommand::RemoveBus { id: "A3".into() }).unwrap();
        let (strips, buses) = ids(&host);
        assert_eq!(strips, ["H1", "H2", "V1"]);
        assert_eq!(buses.len(), 71);
        let (views, _) = host.views();
        assert_eq!(views[1].label, "Mic");
        assert_eq!(views[1].routes, ["A1", "A71"], "the route to A3 went with it");
        assert_eq!(hardware_tables(&host.saved_mixer()), (2, 71));
        assert_eq!(lock(&host.inputs)[1].label(), "H2", "the log follows the new numbers");

        // A new strip starts afresh, and a cable's strip goes with its cable.
        host.command(&MixerCommand::AddStrip).unwrap();
        assert_eq!(host.views().0[2].label, "Hardware 3");
        assert!(host.command(&MixerCommand::RemoveStrip { id: "V1".into() }).is_err());
        assert!(host.command(&MixerCommand::RemoveStrip { id: "H9".into() }).is_err());
        host.shutdown();
    }

    /// Controllers name strips and buses by key, ID or name, and switch
    /// settings on, off or over.
    #[test]
    fn controllers_name_strips_and_buses_as_people_do() {
        let host = MixerHost::new(settings(), SavedMixer::default(), &Logger::discard(), "", false);
        host.apply(&[cable(1, CableKind::Loopback)]).unwrap();
        host.command(&MixerCommand::Label { id: "H2".into(), label: "Mic".into() }).unwrap();
        let (strips, buses) = host.views();
        let keys: Vec<&str> =
            strips.iter().map(|s| s.key.as_str()).chain(buses.iter().map(|b| b.key.as_str())).collect();
        assert!(keys.iter().all(|k| k.len() == 12), "{keys:?}");
        let mut unique = keys.clone();
        unique.dedup();
        assert_eq!(unique.len(), keys.len());

        let resolve = |target: &str, want| host.resolve(target, want);
        assert_eq!(resolve("mic", Want::Either), Ok(("H2".into(), true)));
        assert_eq!(resolve("h2", Want::Either), Ok(("H2".into(), true)));
        assert_eq!(resolve(&strips[1].key, Want::Either), Ok(("H2".into(), true)));
        assert_eq!(resolve("A2", Want::Bus), Ok(("A2".into(), false)));
        assert!(resolve("A2", Want::Strip).is_err(), "a bus is not a strip");
        // The loopback cable's strip and bus share its name.
        let ambiguous = resolve("C1", Want::Either).unwrap_err();
        assert!(ambiguous.contains("strip:C1") && ambiguous.contains("V1 or B1"), "{ambiguous}");
        assert_eq!(resolve("bus:c1", Want::Either), Ok(("B1".into(), false)));
        assert_eq!(resolve("C1", Want::Strip), Ok(("V1".into(), true)));
        assert!(resolve("Nobody", Want::Either).is_err());

        let control = |action| host.control(&action);
        control(ControlAction::Mute { target: "Mic".into(), on: None }).unwrap();
        control(ControlAction::Solo { target: "Mic".into(), on: Some(true) }).unwrap();
        control(ControlAction::Mute { target: "A1".into(), on: Some(true) }).unwrap();
        control(ControlAction::Gain { target: "Mic".into(), db: -10.0 }).unwrap();
        control(ControlAction::AdjustGain { target: "Mic".into(), db: 3.5 }).unwrap();
        control(ControlAction::AdjustGain { target: "A1".into(), db: 99.0 }).unwrap();
        control(ControlAction::Route { strip: "Mic".into(), bus: "A2".into(), on: None }).unwrap();
        control(ControlAction::Route { strip: "Mic".into(), bus: "A1".into(), on: None }).unwrap();
        assert!(control(ControlAction::Solo { target: "A1".into(), on: None }).is_err(), "buses have no solo");
        assert!(control(ControlAction::Gain { target: "Mic".into(), db: f32::NAN }).is_err());
        let (strips, buses) = host.views();
        let mic = &strips[1].params;
        assert!(mic.mute && mic.solo && !mic.mono);
        assert_eq!(mic.gain_db, -6.5);
        assert_eq!(strips[1].routes, ["A2"], "A1 toggled off, A2 on");
        assert!(buses[0].params.mute);
        assert_eq!(buses[0].params.gain_db, engine::params::MAX_GAIN_DB, "kept in range");
        host.control(&ControlAction::Mute { target: "Mic".into(), on: None }).unwrap();
        assert!(!host.views().0[1].params.mute, "toggled back");

        // The key stays with the strip when its ID changes.
        let key = host.views().0[1].key.clone();
        host.command(&MixerCommand::RemoveStrip { id: "H1".into() }).unwrap();
        assert_eq!(host.resolve(&key, Want::Either), Ok(("H1".into(), true)));
        host.shutdown();
    }

    /// A hardware strip can take a bus's output; loops are refused, and a
    /// strip fed from a bus that goes falls silent.
    #[test]
    fn buses_feed_strips_without_loops() {
        let host = MixerHost::new(settings(), SavedMixer::default(), &Logger::discard(), "", false);
        host.apply(&[cable(1, CableKind::Loopback)]).unwrap();
        host.command(&MixerCommand::AddStrip).unwrap();
        let device = |id: &str, endpoint: &str| {
            host.command(&MixerCommand::Device { id: id.into(), endpoint: endpoint.into(), exclusive: true })
        };
        let route = |strip: &str, bus: &str, on| {
            host.command(&MixerCommand::Route { strip: strip.into(), bus: bus.into(), on })
        };
        // H1 takes A2's output; its send to A2 would be a loop, so it goes.
        route("H1", "A2", true).unwrap();
        device("H1", "bus:A2").unwrap();
        let (strips, _) = host.views();
        let port = strips[0].port.as_ref().unwrap();
        assert_eq!((port.state.as_str(), port.device_name.as_str(), port.exclusive), ("bus", "A2", false));
        assert!(port.endpoint.starts_with(BUS_SOURCE) && !port.endpoint.ends_with("A2"), "kept by key");
        assert_eq!(strips[0].routes, ["A1"]);

        // H2 takes A1, which H1 sends to: sending H2 to A2 would close the loop.
        device("H2", "bus:A1").unwrap();
        let refused = route("H2", "A2", true).unwrap_err();
        assert!(refused.contains("back into itself"), "{refused}");
        route("H2", "B1", true).unwrap();
        // And a strip cannot take a bus that it feeds through others.
        route("H1", "A1", false).unwrap();
        route("H1", "B1", true).unwrap();
        // H3 sends to A1, which comes back through H2 to B1.
        let refused = device("H3", "bus:B1").unwrap_err();
        assert!(refused.contains("back into itself"), "{refused}");
        route("H2", "B1", false).unwrap();
        device("H3", "bus:B1").unwrap();
        // A2 reaches B1 through H1, so B1's strip cannot send to A2.
        route("H3", "A2", true).unwrap_err();
        assert!(device("A1", "bus:B1").is_err(), "buses do not take buses");
        assert!(device("H4", "bus:Nobody").is_err());

        // Removing A2 leaves H1 with no source, and its key is not reused.
        host.command(&MixerCommand::RemoveBus { id: "A2".into() }).unwrap();
        let (strips, _) = host.views();
        assert_eq!(strips[0].port.as_ref().unwrap().state, "off");
        assert_eq!(host.saved_mixer().strips[0].device, "");
        // Back to a device: the strip lets the bus go.
        device("H2", "").unwrap();
        assert_eq!(host.views().0[1].port.as_ref().unwrap().state, "off");
        host.shutdown();
    }

    /// An input strip can capture an application, named by its program; a
    /// bus cannot, and an application has no exclusive mode.
    #[test]
    fn input_strips_capture_applications() {
        let host = MixerHost::new(settings(), SavedMixer::default(), &Logger::discard(), "", false);
        let device = |id: &str, endpoint: &str| {
            host.command(&MixerCommand::Device { id: id.into(), endpoint: endpoint.into(), exclusive: true })
        };
        device("H1", "app:NoSuchProgram.exe").unwrap();
        let saved = host.saved_mixer();
        let h1 = saved.strips.iter().find(|s| s.id == "H1").unwrap();
        assert_eq!((h1.device.as_str(), h1.exclusive), ("app:NoSuchProgram.exe", false));
        assert!(device("A1", "app:NoSuchProgram.exe").is_err(), "buses do not capture applications");
        assert!(device("H2", "app:  ").is_err(), "an application needs a name");
        host.shutdown();
    }

    /// Profiles keep their own strip and bus settings, and their own number
    /// of hardware strips and buses; switching saves the active one first.
    #[test]
    fn profiles_switch_settings_and_hardware() {
        let _guard = crate::config::tests::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        crate::config::tests::temp_app_data("profiles");
        let host = MixerHost::new(settings(), SavedMixer::default(), &Logger::discard(), "", false);
        let gain = |host: &MixerHost| host.views().0[0].params.gain_db;
        let hardware = |host: &MixerHost| {
            let (strips, buses) = host.views();
            (
                strips.iter().filter(|s| s.id.starts_with('H')).count(),
                buses.iter().filter(|b| b.id.starts_with('A')).count(),
            )
        };
        let set_gain = |db: f32| {
            host.control(&ControlAction::Gain { target: "H1".into(), db }).unwrap();
        };
        let command = |command: MixerCommand| host.command(&command);

        set_gain(-6.0);
        command(MixerCommand::CreateProfile { name: " Gaming   night ".into(), from_defaults: false }).unwrap();
        assert_eq!(
            host.profiles(),
            ("Gaming night".to_string(), vec!["Default".to_string(), "Gaming night".to_string()])
        );
        assert_eq!(gain(&host), -6.0, "a copy of the current settings");
        set_gain(-20.0);
        command(MixerCommand::AddStrip).unwrap();
        assert_eq!(hardware(&host), (3, 2));

        command(MixerCommand::SwitchProfile { name: "default".into() }).unwrap();
        assert_eq!((gain(&host), hardware(&host)), (-6.0, (2, 2)));
        command(MixerCommand::SwitchProfile { name: "Gaming night".into() }).unwrap();
        assert_eq!((gain(&host), hardware(&host)), (-20.0, (3, 2)), "the settings it was left with");

        command(MixerCommand::CreateProfile { name: "Fresh".into(), from_defaults: true }).unwrap();
        assert_eq!((gain(&host), hardware(&host)), (0.0, (5, 5)), "a new configuration's");
        let file = |name: &str| std::fs::read_to_string(crate::config::profiles_dir().join(format!("{name}.toml"))).unwrap();
        let fresh = file("Fresh");
        assert!(fresh.starts_with("[[strip]]
id = \"H1\""), "{fresh}");
        assert!(fresh.contains("id = \"H5\"") && fresh.contains("id = \"A5\"") && fresh.contains("device = \"default\""), "{fresh}");
        assert!(!fresh.contains('#'), "no comments: {fresh}");
        assert!(file("Default").starts_with("[[strip]]"), "the profile switched away from was saved");
        assert!(command(MixerCommand::CreateProfile { name: "fresh".into(), from_defaults: true }).is_err());
        assert!(command(MixerCommand::CreateProfile { name: "a/b".into(), from_defaults: true }).is_err());
        assert!(command(MixerCommand::DeleteProfile { name: "Fresh".into() }).is_err(), "it is active");
        assert!(command(MixerCommand::SwitchProfile { name: "Nowhere".into() }).is_err());
        command(MixerCommand::DeleteProfile { name: "Gaming night".into() }).unwrap();
        assert_eq!(host.profiles().1, ["Default", "Fresh"]);
        let config =
            crate::config::parse_config(&std::fs::read_to_string(crate::config::config_file_path()).unwrap()).unwrap();
        assert_eq!(crate::config::config_profile(&config), "Fresh");
        host.shutdown();
    }

    /// A loopback cable through the whole host: its device, the engine and
    /// back out of the recording side, bit for bit.
    #[test]
    fn loopback_cable_passes_audio_through_the_engine() {
        let host = MixerHost::new(settings(), SavedMixer::default(), &Logger::discard(), "", false);
        let cables = [cable(1, CableKind::Loopback)];
        let mut state = lock(&host.server);
        let devices = host.make_devices(&mut state, &cables).unwrap();
        drop(state);
        host.rebuild(&cables);
        let device = &devices[0];
        device.handle_control(SetupPacket { request: REQUEST_SET_CONFIGURATION, value: 1, ..Default::default() }, &[]);
        for iface in [1, 2] {
            device.handle_control(
                SetupPacket { request_type: 0x01, request: REQUEST_SET_INTERFACE, index: iface, value: 1, length: 0 },
                &[],
            );
        }
        let packet: Vec<u8> = (0..PACKET_BYTES).map(|i| (i * 7 + 1) as u8).collect();
        device.write_playback(50_000, [packet.as_slice()]);
        let mut out = vec![0u8; PACKET_BYTES];
        device.read_capture(50_000 + 20, &mut out, [PACKET_BYTES]);
        assert_eq!(out, packet);
    }
}
