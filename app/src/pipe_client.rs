//! The client side of the named pipes: one line of JSON out, one back, or a
//! line per event on a subscription. Used by the window to reach the Windows
//! service and by `--control` (and LemonMixCtl.exe) to reach the mixer.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::time::Duration;

use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;

use crate::protocol::{Reply, Request};

/// The pipe through which the window manages the Windows service.
pub const SERVICE_PIPE: &str = r"\\.\pipe\LemonMix";
/// The pipe through which controllers reach the mixer, served by whichever
/// process runs it: the Windows service, or the window on demand.
pub const CONTROL_PIPE: &str = r"\\.\pipe\LemonMixControl";
/// The longest request or reply.
pub const MAX_LINE: u64 = 4 << 20;

/// Sends a request and waits for the reply.
pub fn call_pipe(name: &str, request: &Request) -> Result<Reply, String> {
    let pipe = open_pipe(name).map_err(|err| describe_open_error(name, &err))?;
    send(&pipe, request)?;
    serde_json::from_str(&read_line(&mut BufReader::new(&pipe))?)
        .map_err(|_| "the mixer sent an invalid reply".to_string())
}

/// Writes a request as a line.
pub fn send(pipe: &File, request: &Request) -> Result<(), String> {
    let line = serde_json::to_string(request).map_err(|err| err.to_string())?;
    let mut writer = pipe;
    writer.write_all(format!("{line}\n").as_bytes()).map_err(|err| err.to_string())
}

/// Opens a pipe, waiting a moment while every instance is busy.
pub fn open_pipe(name: &str) -> io::Result<File> {
    let mut attempts = 0;
    loop {
        // Whoever answers may identify this process, but not act as it.
        let opened = OpenOptions::new().read(true).write(true).security_qos_flags(SECURITY_IDENTIFICATION).open(name);
        match opened {
            // Every instance is busy until the server makes the next one.
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && attempts < 50 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            opened => return opened,
        }
    }
}

fn describe_open_error(name: &str, err: &io::Error) -> String {
    match err.kind() {
        io::ErrorKind::NotFound if name == CONTROL_PIPE => "Lemon Mix is not running.".into(),
        io::ErrorKind::NotFound => "the Windows service is not running".into(),
        _ => err.to_string(),
    }
}

/// Reads one line, without its newline.
pub fn read_line<R: Read>(reader: &mut BufReader<R>) -> Result<String, String> {
    let mut line = String::new();
    reader.take(MAX_LINE).read_line(&mut line).map_err(|err| err.to_string())?;
    if !line.ends_with('\n') {
        return Err("the message was cut short".into());
    }
    line.pop();
    Ok(line)
}
