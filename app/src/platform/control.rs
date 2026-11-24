//! The cables, the mixer and their state in Windows Sound, kept by the
//! process that runs them: the window when it runs on demand, or the Windows
//! service. The window reaches them directly in the first case, and through
//! the service's pipe in the second; both send the same requests.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use cables::uac2::CableKind;

use super::broker::{forget_cable_devices, request_cable_sync, resync_cables, start_cable_broker, stop_cable_broker};
use super::remote::PipeServer;
use crate::config::{Cable, DEFAULT_LISTEN, new_cable, new_cable_serial, save_cables, validate_cable_name};
use crate::mixer::MixerHost;
use crate::protocol::{CableView, Reply, Request, Snapshot, Tone};
use cables::logf;
use cables::logging::Logger;

static HOST: OnceLock<Arc<MixerHost>> = OnceLock::new();
/// The cables, in list order.
static CABLES: Mutex<Vec<Cable>> = Mutex::new(Vec::new());
static STATUS: Mutex<Option<(Tone, String)>> = Mutex::new(None);
/// Serializes changes to the cables, which the service takes from any window.
static CHANGES: Mutex<()> = Mutex::new(());
/// Called whenever the snapshot may have changed, from any thread.
static ON_CHANGE: OnceLock<fn()> = OnceLock::new();
/// The control pipe, served while this process runs the mixer.
static CONTROL_PIPE: Mutex<Option<PipeServer>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

fn host() -> Option<&'static Arc<MixerHost>> {
    HOST.get()
}

fn changed() {
    if let Some(on_change) = ON_CHANGE.get() {
        on_change();
    }
}

/// Sets the host this process runs the mixer on.
pub fn init(host: &Arc<MixerHost>) {
    let _ = HOST.set(Arc::clone(host));
}

/// Sets what to call whenever the snapshot may have changed.
pub fn on_change(callback: fn()) {
    let _ = ON_CHANGE.set(callback);
}

/// Starts the mixer and the USB/IP server with `cables` and brings Windows
/// in line. A server that cannot start leaves its error in the snapshot.
pub fn start(cables: Vec<Cable>) {
    let Some(host) = host() else {
        return;
    };
    *lock(&CABLES) = cables.clone();
    host.start();
    // A failure is logged and kept as the server's error.
    let _ = host.restart(&cables);
    changed();
    start_cable_broker();
    let mut pipe = lock(&CONTROL_PIPE);
    if pipe.is_none() {
        *pipe = PipeServer::start_control(&host.logger)
            .inspect_err(|err| logf!(host.logger, "Controllers cannot reach the mixer: {err}"))
            .ok();
    }
}

/// Takes the cables out of Windows, stops the USB/IP server and lets the
/// physical devices go.
pub fn stop() {
    if let Some(pipe) = lock(&CONTROL_PIPE).take() {
        pipe.stop();
    }
    stop_cable_broker();
    if let Some(host) = host() {
        host.stop();
        host.suspend();
    }
    changed();
}

pub fn snapshot() -> Snapshot {
    let Some(host) = host() else {
        return Snapshot::default();
    };
    let (strips, buses) = host.views();
    let (capture_devices, render_devices) = host.device_lists();
    let (profile, profiles) = host.profiles();
    Snapshot {
        cables: lock(&CABLES)
            .iter()
            .map(|c| CableView { id: c.id, serial: c.serial.clone(), name: c.name.clone(), kind: c.kind.name().into() })
            .collect(),
        server_error: host.last_error(),
        status: lock(&STATUS).clone(),
        strips,
        buses,
        capture_devices,
        render_devices,
        apps: host.apps(),
        profile,
        profiles,
        latency_ms: host.latency_ms(),
        solo_bus: host.solo_bus(),
    }
}

/// The IDs of the cables Windows should have.
pub fn cable_ids() -> Vec<u32> {
    lock(&CABLES).iter().map(|c| c.id).collect()
}

/// The application log.
pub fn logger() -> Logger {
    host().map_or_else(Logger::discard, |h| h.logger.clone())
}

/// Where the application log is.
pub fn log_path() -> String {
    host().map(|h| h.log_path()).unwrap_or_default()
}

/// The address the USB/IP server listens on.
pub fn server_address() -> String {
    host().map(|h| h.address().to_string()).unwrap_or_else(|| DEFAULT_LISTEN.to_string())
}

/// Sets the Windows Sound status; safe from any thread.
pub fn set_status(tone: Tone, text: &str) {
    *lock(&STATUS) = Some((tone, text.to_string()));
    changed();
}

/// Removes the Windows Sound status; safe from any thread.
pub fn clear_status() {
    *lock(&STATUS) = None;
    changed();
}

fn parse_kind(kind: &str) -> Result<CableKind, String> {
    CableKind::from_name(kind).ok_or_else(|| format!("Unknown cable kind \"{kind}\"."))
}

/// Adds a cable with the name the user typed. The error says why it could
/// not be added, or that it was added but could not be saved.
pub fn add_cable(name: &str, kind: &str) -> Result<(), String> {
    let kind = parse_kind(kind)?;
    let _change = lock(&CHANGES);
    let mut updated = lock(&CABLES).clone();
    updated.push(new_cable(name, kind, &updated)?);
    apply_cables(updated, None, false)
}

/// Removes the cable with this serial number, its mixer settings, and its
/// devices from Windows.
pub fn remove_cable(serial: &str) -> Result<(), String> {
    let _change = lock(&CHANGES);
    let mut updated = lock(&CABLES).clone();
    let index = updated.iter().position(|c| c.serial == serial).ok_or("That cable no longer exists.")?;
    let removed = updated.remove(index);
    let result = apply_cables(updated, Some(removed.clone()), false);
    if let Some(host) = host() {
        host.forget_cable_settings(removed.id, None);
    }
    result
}

/// Renames a cable. Windows keeps the name a device first had, so the cable
/// becomes a new device: a new serial number under the same ID, which keeps
/// its mixer settings.
pub fn rename_cable(serial: &str, name: &str) -> Result<(), String> {
    let _change = lock(&CHANGES);
    let mut updated = lock(&CABLES).clone();
    let index = updated.iter().position(|c| c.serial == serial).ok_or("That cable no longer exists.")?;
    let others: Vec<Cable> = updated.iter().filter(|c| c.serial != serial).cloned().collect();
    let name = validate_cable_name(name, &others)?;
    if updated[index].name == name {
        return Ok(());
    }
    let old = updated[index].clone();
    updated[index] = Cable { name, serial: new_cable_serial(&updated), ..old.clone() };
    apply_cables(updated, Some(old), true)
}

/// Changes a cable's kind; it becomes a new device of that kind.
pub fn set_cable_kind(serial: &str, kind: &str) -> Result<(), String> {
    let kind = parse_kind(kind)?;
    let _change = lock(&CHANGES);
    let mut updated = lock(&CABLES).clone();
    let index = updated.iter().position(|c| c.serial == serial).ok_or("That cable no longer exists.")?;
    let old = updated[index].clone();
    if old.kind == kind {
        return Ok(());
    }
    updated[index] = Cable { kind, ..old.clone() };
    if let Some(host) = host() {
        host.forget_cable_settings(old.id, Some(kind));
    }
    apply_cables(updated, Some(old), true)
}

/// Applies `updated` to the running server, saves it and brings Windows in
/// line, deleting the Windows devices of the `replaced` cable. Cables that
/// did not change keep streaming. A cable that changed in place (renamed, or
/// of another kind) needs every cable attached again.
fn apply_cables(updated: Vec<Cable>, replaced: Option<Cable>, in_place: bool) -> Result<(), String> {
    let host = host().ok_or("The USB/IP server is not available.")?;
    host.apply(&updated).map_err(|err| format!("Could not update the USB/IP server:\n{err}"))?;
    *lock(&CABLES) = updated.clone();
    let saved = save_cables(&updated);
    changed();
    if let Some(replaced) = replaced {
        forget_cable_devices(&replaced);
    }
    if in_place {
        resync_cables()
    } else {
        request_cable_sync()
    }
    saved.map_err(|err| format!("The cables were changed, but config could not be saved:\n{err}"))
}

/// Brings the server and Windows Sound back in line with the cables, for
/// recovering from a failure: starts the USB/IP server if it is not running,
/// then detaches and attaches every cable again.
pub fn sync_now() -> Result<(), String> {
    let _change = lock(&CHANGES);
    let started = match host() {
        Some(host) => {
            host.apply(&lock(&CABLES).clone()).map_err(|err| format!("Could not start the USB/IP server:\n{err}"))
        }
        None => Ok(()),
    };
    changed();
    resync_cables();
    started
}

/// Carries out a request.
pub fn handle(request: &Request) -> Result<(), String> {
    match request {
        Request::Status => Ok(()),
        Request::AddCable { name, kind } => add_cable(name, kind),
        Request::RemoveCable { serial } => remove_cable(serial),
        Request::RenameCable { serial, name } => rename_cable(serial, name),
        Request::SetCableKind { serial, kind } => set_cable_kind(serial, kind),
        Request::Sync => sync_now(),
        Request::Mixer { command } => host().ok_or("The mixer is not available.")?.command(command),
        Request::Control { action } => {
            let result = host().ok_or("The mixer is not available.")?.control(action);
            changed();
            result
        }
        Request::Subscribe { .. } => Err("Subscribe on the control pipe.".into()),
    }
}

/// Carries out a request and answers it with the current state.
pub fn reply(request: &Request) -> Reply {
    let result = handle(request);
    Reply::new(snapshot(), result.err())
}
