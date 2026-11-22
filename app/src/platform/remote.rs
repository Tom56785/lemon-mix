//! The named pipes served by the process that runs the mixer.
//!
//! The service pipe is how windows manage the Windows service: each
//! connection carries one request and its reply, each a line of JSON, and
//! every reply holds the service's state, so the window can show it straight
//! away. The control pipe takes the same requests from controllers (the
//! Stream Deck plugin, `--control`), and subscriptions, which keep the
//! connection open and send events.

use std::fs::File;
use std::io::{BufReader, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cables::logf;
use cables::logging::Logger;
use cables::win::{SecurityDescriptor, last_error, wide};
use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, FlushFileBuffers, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, PeekNamedPipe,
};

use super::control;
use crate::pipe_client::{CONTROL_PIPE, SERVICE_PIPE, call_pipe, open_pipe, read_line};
use crate::protocol::{Event, KeyedMeter, Meter, PROTOCOL_VERSION, Reply, Request, Snapshot};

/// SYSTEM and administrators have full access, and users signed in at the
/// computer may read and write.
const PIPE_SECURITY: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";
/// How often a subscription looks for changes and sends meters.
const SUBSCRIPTION_TICK: Duration = Duration::from_millis(66);

/// Sends a request to the service and waits for its reply. Fails when the
/// service is not running.
pub fn call(request: &Request) -> Result<Reply, String> {
    call_pipe(SERVICE_PIPE, request)
}

/// Carries out a request and returns the reply.
fn handle_request(request: Result<Request, serde_json::Error>) -> String {
    let reply = match request {
        Ok(request) => control::reply(&request),
        Err(_) => Reply::new(control::snapshot(), Some("invalid request".into())),
    };
    serde_json::to_string(&reply).unwrap_or_else(|_| "{}".into())
}

/// Serves a pipe, one thread per connection, until stopped.
pub struct PipeServer {
    name: &'static str,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PipeServer {
    /// The service pipe, in the Windows service.
    pub fn start(logger: &Logger) -> Result<PipeServer, String> {
        PipeServer::serve(SERVICE_PIPE, PIPE_SECURITY, false, logger)
    }

    /// The control pipe, in whichever process runs the mixer.
    pub fn start_control(logger: &Logger) -> Result<PipeServer, String> {
        PipeServer::serve(CONTROL_PIPE, PIPE_SECURITY, true, logger)
    }

    fn serve(name: &'static str, security: &str, subscriptions: bool, logger: &Logger) -> Result<PipeServer, String> {
        let security = SecurityDescriptor::new(security)?;
        // The first instance fails if another process already has the name.
        let first = create_instance(name, &security, true)?;
        let stopping = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stopping);
        let logger = logger.clone();
        let thread = std::thread::Builder::new()
            .name(format!("pipe {name}"))
            .spawn(move || accept_connections(name, first, security, subscriptions, &flag, &logger))
            .map_err(|err| err.to_string())?;
        Ok(PipeServer { name, stopping, thread: Some(thread) })
    }

    pub fn stop(mut self) {
        self.stop_now();
    }

    fn stop_now(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        // A connection of its own wakes the instance waiting for a client.
        let _ = open_pipe(self.name);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop_now();
        }
    }
}

fn create_instance(name: &str, security: &SecurityDescriptor, first: bool) -> Result<File, String> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: security.as_ptr(),
        bInheritHandle: 0,
    };
    let open_mode = PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
    let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    let wide_name = wide(name);
    // SAFETY: the name and attributes outlive the call, and the File takes
    // ownership of the handle.
    unsafe {
        let handle = CreateNamedPipeW(
            wide_name.as_ptr(),
            open_mode,
            pipe_mode,
            PIPE_UNLIMITED_INSTANCES,
            4096,
            4096,
            0,
            &attributes,
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(format!("create {name}: {}", last_error()));
        }
        Ok(File::from_raw_handle(handle))
    }
}

fn accept_connections(
    name: &str,
    mut pipe: File,
    security: SecurityDescriptor,
    subscriptions: bool,
    stopping: &Arc<AtomicBool>,
    logger: &Logger,
) {
    loop {
        // SAFETY: waits for a client on a pipe instance this function owns.
        let connected = unsafe {
            ConnectNamedPipe(pipe.as_raw_handle(), null_mut()) != 0 || GetLastError() == ERROR_PIPE_CONNECTED
        };
        if stopping.load(Ordering::SeqCst) {
            return;
        }
        if connected {
            let (logger, stopping) = (logger.clone(), Arc::clone(stopping));
            std::thread::spawn(move || serve_connection(pipe, subscriptions, &stopping, &logger));
        }
        pipe = match create_instance(name, &security, false) {
            Ok(pipe) => pipe,
            Err(err) => {
                logf!(logger, "The pipe {name} stopped: {err}");
                return;
            }
        };
    }
}

fn serve_connection(pipe: File, subscriptions: bool, stopping: &AtomicBool, logger: &Logger) {
    let line = match read_line(&mut BufReader::new(&pipe)) {
        Ok(line) => line,
        Err(err) => {
            logf!(logger, "Pipe request failed: {err}");
            return;
        }
    };
    let request = serde_json::from_str::<Request>(&line);
    let finished = match request {
        Ok(Request::Subscribe { meters }) if subscriptions => {
            subscribe(&pipe, meters, stopping);
            true
        }
        request => (&pipe).write_all(format!("{}\n", handle_request(request)).as_bytes()).is_ok(),
    };
    if finished {
        // SAFETY: waits until the client has read the last line, then lets it go.
        unsafe {
            FlushFileBuffers(pipe.as_raw_handle());
            DisconnectNamedPipe(pipe.as_raw_handle());
        }
    }
}

/// Sends a subscription's events until the client goes or the pipe stops.
fn subscribe(pipe: &File, meters: bool, stopping: &AtomicBool) {
    let mut last_state = String::new();
    let mut next = Instant::now();
    while !stopping.load(Ordering::SeqCst) && client_connected(pipe) {
        let (state, meter_event) = subscription_events(control::snapshot());
        let mut lines = String::new();
        if state != last_state {
            lines.push_str(&state);
            lines.push('\n');
            last_state = state;
        }
        if meters {
            lines.push_str(&meter_event);
            lines.push('\n');
        }
        if !lines.is_empty() && (&*pipe).write_all(lines.as_bytes()).is_err() {
            return;
        }
        next += SUBSCRIPTION_TICK;
        let now = Instant::now();
        if next < now {
            next = now;
        }
        std::thread::sleep(next - now);
    }
}

/// Whether the client still has its end of the pipe open.
fn client_connected(pipe: &File) -> bool {
    // SAFETY: asks about a pipe this thread owns; no data is read.
    unsafe { PeekNamedPipe(pipe.as_raw_handle(), null_mut(), 0, null_mut(), null_mut(), null_mut()) != 0 }
}

/// A snapshot as a subscription's state and meter events. The state leaves
/// out what changes all the time (meters, and how full device buffers are),
/// so it is only sent when something a controller shows changes.
fn subscription_events(mut snapshot: Snapshot) -> (String, String) {
    let keyed = |key: &str, meter: &Meter| KeyedMeter { key: key.to_string(), meter: *meter };
    let meters = Event::Meters {
        strips: snapshot.strips.iter().map(|s| keyed(&s.key, &s.meter)).collect(),
        buses: snapshot.buses.iter().map(|b| keyed(&b.key, &b.meter)).collect(),
    };
    for strip in &mut snapshot.strips {
        strip.meter = Meter::default();
        if let Some(port) = &mut strip.port {
            (port.fill_ms, port.correction_ppm) = (0.0, 0.0);
        }
    }
    for bus in &mut snapshot.buses {
        bus.meter = Meter::default();
        if let Some(port) = &mut bus.port {
            (port.fill_ms, port.correction_ppm) = (0.0, 0.0);
        }
    }
    let state =
        Event::State { protocol: PROTOCOL_VERSION, version: crate::VERSION.to_string(), snapshot: Box::new(snapshot) };
    let json = |event: &Event| serde_json::to_string(event).unwrap_or_else(|_| "{}".into());
    (json(&state), json(&meters))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipe_client::send;
    use crate::protocol::{ControlAction, StripView};

    #[test]
    fn invalid_requests_are_refused() {
        for request in
            ["", "nonsense", "{\"op\": \"add_cable\"}", "{\"op\": \"remove_cable\", \"serial\": 7}", "{\"op\": \"x\"}"]
        {
            let reply: Reply = serde_json::from_str(&handle_request(serde_json::from_str(request))).unwrap();
            assert_eq!(reply.result(), Err("invalid request".into()), "{request:?}");
        }
        let status: Reply = serde_json::from_str(&handle_request(Ok(Request::Status))).unwrap();
        assert!(status.result().is_ok());
        assert_eq!((status.version.as_str(), status.protocol), (crate::VERSION, PROTOCOL_VERSION));
    }

    fn test_pipe(name: &str) -> &'static str {
        format!(r"\\.\pipe\LemonMixTest{name}{}", std::process::id()).leak()
    }

    /// A pipe of its own, open to the test's user however the test runs.
    const TEST_SECURITY: &str = "D:P(A;;GA;;;OW)(A;;GA;;;AU)";

    #[test]
    fn requests_travel_through_the_pipe() {
        let name = test_pipe("Requests");
        let server = PipeServer::serve(name, TEST_SECURITY, false, &Logger::discard()).unwrap();
        assert!(PipeServer::serve(name, TEST_SECURITY, false, &Logger::discard()).is_err(), "the name is taken");
        let calls: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(move || call_pipe(name, &Request::Status).map(|reply| reply.result())))
            .collect();
        for call in calls {
            assert_eq!(call.join().unwrap(), Ok(Ok(())));
        }
        let refused = call_pipe(name, &Request::RemoveCable { serial: "NO-SUCH-CABLE".into() }).unwrap();
        assert_eq!(refused.result(), Err("That cable no longer exists.".into()));
        let control = Request::Control { action: ControlAction::Mute { target: "H1".into(), on: None } };
        assert_eq!(call_pipe(name, &control).unwrap().result(), Err("The mixer is not available.".into()));
        // The service pipe does not take subscriptions.
        let refused = call_pipe(name, &Request::Subscribe { meters: true }).unwrap();
        assert!(refused.result().is_err());
        server.stop();
        assert!(call_pipe(name, &Request::Status).is_err(), "the pipe is closed");
    }

    #[test]
    fn subscriptions_send_the_state_then_meters() {
        let name = test_pipe("Subscribe");
        let server = PipeServer::serve(name, TEST_SECURITY, true, &Logger::discard()).unwrap();
        let pipe = open_pipe(name).unwrap();
        send(&pipe, &Request::Subscribe { meters: true }).unwrap();
        let mut reader = BufReader::new(&pipe);
        let first: Event = serde_json::from_str(&read_line(&mut reader).unwrap()).unwrap();
        assert!(matches!(first, Event::State { protocol: PROTOCOL_VERSION, .. }), "{first:?}");
        // The state does not change, so only meters follow.
        for _ in 0..3 {
            let event: Event = serde_json::from_str(&read_line(&mut reader).unwrap()).unwrap();
            assert!(matches!(event, Event::Meters { .. }), "{event:?}");
        }
        drop(reader);
        drop(pipe);
        server.stop();
    }

    #[test]
    fn subscription_state_leaves_out_what_always_changes() {
        let strip = |peak: f32, fill_ms: f64| StripView {
            id: "H1".into(),
            key: "k1".into(),
            label: "Mic".into(),
            params: Default::default(),
            routes: Vec::new(),
            duck_by: Vec::new(),
            meter: Meter { peak: [peak; 2], ..Default::default() },
            port: Some(crate::protocol::PortView { fill_ms, ..Default::default() }),
        };
        let a = subscription_events(Snapshot { strips: vec![strip(0.5, 18.0)], ..Default::default() });
        let b = subscription_events(Snapshot { strips: vec![strip(0.1, 21.0)], ..Default::default() });
        assert_eq!(a.0, b.0, "the state is the same");
        assert_ne!(a.1, b.1, "the meters differ");
        assert!(a.1.contains("\"key\":\"k1\""), "{}", a.1);
    }
}
