//! The named pipe through which windows manage the Windows service. Each
//! connection carries one request and its reply, each a line of JSON. Every
//! reply holds the service's state, so the window can show it straight away.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use cables::logf;
use cables::logging::Logger;
use cables::win::{SecurityDescriptor, last_error, wide};
use windows_sys::Win32::Foundation::{ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FlushFileBuffers, PIPE_ACCESS_DUPLEX, SECURITY_IDENTIFICATION,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use super::control;
use crate::protocol::{Reply, Request};

const PIPE_NAME: &str = r"\\.\pipe\LemonMix";
/// SYSTEM and administrators have full access, and users signed in at the
/// computer may read and write.
const PIPE_SECURITY: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";
/// The longest request or reply.
const MAX_LINE: u64 = 4 << 20;

/// Sends a request to the service and waits for its reply. Fails when the
/// service is not running.
pub fn call(request: &Request) -> Result<Reply, String> {
    call_pipe(PIPE_NAME, request)
}

fn call_pipe(name: &str, request: &Request) -> Result<Reply, String> {
    let pipe = open_pipe(name).map_err(|err| err.to_string())?;
    let line = serde_json::to_string(request).map_err(|err| err.to_string())?;
    (&pipe).write_all(format!("{line}\n").as_bytes()).map_err(|err| err.to_string())?;
    serde_json::from_str(&read_line(&pipe)?).map_err(|_| "the service sent an invalid reply".to_string())
}

fn open_pipe(name: &str) -> io::Result<File> {
    let mut attempts = 0;
    loop {
        // Whoever answers may identify this process, but not act as it.
        let opened = OpenOptions::new().read(true).write(true).security_qos_flags(SECURITY_IDENTIFICATION).open(name);
        match opened {
            // Every instance is busy until the service makes the next one.
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && attempts < 50 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            opened => return opened,
        }
    }
}

fn read_line(pipe: &File) -> Result<String, String> {
    let mut line = String::new();
    BufReader::new(pipe.take(MAX_LINE)).read_line(&mut line).map_err(|err| err.to_string())?;
    if !line.ends_with('\n') {
        return Err("the message was cut short".into());
    }
    line.pop();
    Ok(line)
}

/// Carries out a request in the service and returns the reply.
fn handle_request(line: &str) -> String {
    let reply = match serde_json::from_str::<Request>(line) {
        Ok(request) => control::reply(&request),
        Err(_) => Reply {
            snapshot: control::snapshot(),
            version: crate::VERSION.to_string(),
            error: Some("invalid request".into()),
        },
    };
    serde_json::to_string(&reply).unwrap_or_else(|_| "{}".into())
}

/// Serves the pipe in the service, one thread per connection, until stopped.
pub struct PipeServer {
    name: &'static str,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PipeServer {
    pub fn start(logger: &Logger) -> Result<PipeServer, String> {
        PipeServer::serve(PIPE_NAME, PIPE_SECURITY, logger)
    }

    fn serve(name: &'static str, security: &str, logger: &Logger) -> Result<PipeServer, String> {
        let security = SecurityDescriptor::new(security)?;
        // The first instance fails if another process already has the name.
        let first = create_instance(name, &security, true)?;
        let stopping = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stopping);
        let logger = logger.clone();
        let thread = std::thread::Builder::new()
            .name("service pipe".into())
            .spawn(move || accept_connections(name, first, security, &flag, &logger))
            .map_err(|err| err.to_string())?;
        Ok(PipeServer { name, stopping, thread: Some(thread) })
    }

    pub fn stop(mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        // A connection of its own wakes the instance waiting for a client.
        let _ = open_pipe(self.name);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
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
    stopping: &AtomicBool,
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
            let logger = logger.clone();
            std::thread::spawn(move || serve_connection(pipe, &logger));
        }
        pipe = match create_instance(name, &security, false) {
            Ok(pipe) => pipe,
            Err(err) => {
                logf!(logger, "The service pipe stopped: {err}");
                return;
            }
        };
    }
}

fn serve_connection(pipe: File, logger: &Logger) {
    let reply = match read_line(&pipe) {
        Ok(line) => handle_request(&line),
        Err(err) => {
            logf!(logger, "Service pipe request failed: {err}");
            return;
        }
    };
    if (&pipe).write_all(format!("{reply}\n").as_bytes()).is_ok() {
        // SAFETY: waits until the client has read the reply, then lets it go.
        unsafe {
            FlushFileBuffers(pipe.as_raw_handle());
            DisconnectNamedPipe(pipe.as_raw_handle());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_requests_are_refused() {
        for request in
            ["", "nonsense", "{\"op\": \"add_cable\"}", "{\"op\": \"remove_cable\", \"serial\": 7}", "{\"op\": \"x\"}"]
        {
            let reply: Reply = serde_json::from_str(&handle_request(request)).unwrap();
            assert_eq!(reply.result(), Err("invalid request".into()), "{request:?}");
        }
        let status: Reply =
            serde_json::from_str(&handle_request(&serde_json::to_string(&Request::Status).unwrap())).unwrap();
        assert!(status.result().is_ok());
        assert_eq!(status.version, crate::VERSION);
    }

    #[test]
    fn requests_travel_through_the_pipe() {
        // A pipe of its own, open to the test's user however the test runs.
        let name: &'static str = format!(r"\\.\pipe\LemonMixTest{}", std::process::id()).leak();
        let security = "D:P(A;;GA;;;OW)(A;;GA;;;AU)";
        let server = PipeServer::serve(name, security, &Logger::discard()).unwrap();
        assert!(PipeServer::serve(name, security, &Logger::discard()).is_err(), "the name is taken");
        let calls: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(move || call_pipe(name, &Request::Status).map(|reply| reply.result())))
            .collect();
        for call in calls {
            assert_eq!(call.join().unwrap(), Ok(Ok(())));
        }
        let refused = call_pipe(name, &Request::RemoveCable { serial: "NO-SUCH-CABLE".into() }).unwrap();
        assert_eq!(refused.result(), Err("That cable no longer exists.".into()));
        server.stop();
        assert!(call_pipe(name, &Request::Status).is_err(), "the pipe is closed");
    }
}
