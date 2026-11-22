//! The window's long operations, run on their own threads: switching between
//! running on demand and the Windows service, installing the driver, and the
//! shell calls they need.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use cables::attach::{combined_output, hide_command_window, single_line};
use cables::logf;
use cables::win::wide;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath, ShellExecuteW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use super::link::{Mode, SharedState, lock, repaint, report_error, send_now, set_notice};
use crate::config::{
    CONFIG_FILE, application_data_dir, config_cables, config_file_path, load_config_content, parse_config,
    service_data_dir,
};
use crate::platform::control;
use crate::platform::download::{download_latest_usbip_installer, is_name_resolution_error};
use crate::platform::remote;
use crate::platform::service::{run_elevated, service_state, start_service};
use crate::platform::window::main_window;
use crate::protocol::{Reply, Request, Tone};

const USBIP_RELEASE_PAGE: &str = "https://github.com/vadimgrn/usbip-win2/releases/latest";

/// Runs `job` on its own thread, showing `busy` meanwhile, and reports its
/// failure.
pub fn run_job(
    shared: &SharedState,
    busy: &str,
    job: impl FnOnce(&SharedState) -> Result<(), String> + Send + 'static,
) {
    lock(shared).busy = Some(busy.to_string());
    repaint();
    let shared = shared.clone();
    std::thread::spawn(move || {
        let result = job(&shared);
        let mut s = lock(&shared);
        s.service_state = service_state();
        s.busy = None;
        drop(s);
        if let Err(err) = result {
            logf!(control::logger(), "{}", single_line(&err));
            report_error(&shared, err);
        }
        repaint();
    });
}

/// Asks the service for its state, trying for up to `seconds` seconds.
pub fn call_service_within(seconds: u64) -> Result<Reply, String> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        match remote::call(&Request::Status) {
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            result => return result,
        }
    }
}

/// Runs the mixer in this process, as when no service runs it, with the
/// cables in this user's config.toml.
pub fn run_on_demand(shared: &SharedState) {
    let cables = parse_config(&load_config_content()).map(|c| config_cables(&c)).unwrap_or_default();
    lock(shared).mode = Mode::OnDemand;
    control::start(cables);
    let _ = send_now(shared, &Request::Status);
    let error = control::snapshot().server_error;
    if !error.is_empty() {
        // The window stays open, so the user can install the driver, inspect
        // the status and open the diagnostic log.
        report_error(
            shared,
            format!(
                "The USB/IP server could not start, so the virtual devices are not available:\n\n{error}\n\nDiagnostic log:\n{}",
                control::log_path()
            ),
        );
    }
}

/// Hands the mixer over to the service: stops this window's mixer if
/// `running_here`, which takes its cables out of Windows and lets its
/// devices go, and starts the service. Runs the mixer here again if the
/// service does not start.
pub fn switch_to_service(shared: &SharedState, running_here: bool) -> Result<(), String> {
    let on_demand = lock(shared).mode == Mode::OnDemand;
    if running_here {
        control::stop();
    }
    // The service opens its pipe before it reports that it runs.
    let started = start_service()
        .and_then(|()| call_service_within(10).map_err(|err| format!("the service does not answer: {err}")));
    match started {
        Ok(reply) => {
            let mut s = lock(shared);
            s.mode = Mode::Service;
            s.connected = true;
            s.snapshot = reply.snapshot;
            s.version = reply.version.clone();
            drop(s);
            logf!(control::logger(), "Managing the Windows service, version {}", reply.version);
            Ok(())
        }
        Err(err) => {
            if on_demand {
                run_on_demand(shared);
            }
            Err(format!("Could not start the Windows service:\n{err}"))
        }
    }
}

pub fn install_service(shared: &SharedState) -> Result<(), String> {
    let parameters = format!("--install-service \"{}\"", config_file_path().display());
    run_elevated(&parameters, main_window()).map_err(|err| format!("Could not install the Windows service:\n{err}"))?;
    let running_here = lock(shared).mode == Mode::OnDemand;
    switch_to_service(shared, running_here)
}

pub fn uninstall_service(shared: &SharedState) -> Result<(), String> {
    run_elevated("--uninstall-service", main_window())
        .map_err(|err| format!("Could not uninstall the Windows service:\n{err}"))?;
    if lock(shared).mode == Mode::Service {
        take_back_service_config();
        run_on_demand(shared);
    }
    Ok(())
}

/// Copies the service's settings, cables and profiles over this user's, so the
/// window carries on with what the service had.
fn take_back_service_config() {
    let logger = control::logger();
    let source = service_data_dir().join(CONFIG_FILE);
    let copied = std::fs::read(&source).and_then(|content| {
        std::fs::create_dir_all(application_data_dir())?;
        std::fs::write(config_file_path(), content)
    });
    match copied {
        Ok(()) => logf!(logger, "Copied the service's {} to {}", source.display(), config_file_path().display()),
        Err(err) => logf!(logger, "Keeping this user's settings: could not copy {}: {err}", source.display()),
    }
    match crate::config::copy_profiles(&service_data_dir(), &application_data_dir()) {
        Ok(0) => {}
        Ok(copied) => logf!(logger, "Copied the service's {copied} profile(s)"),
        Err(err) => logf!(logger, "Keeping this user's profiles: could not copy the service's: {err}"),
    }
}

/// Downloads the latest usbip-win2 installer and starts it.
pub fn download_and_install_driver(shared: &SharedState) {
    set_notice(shared, Tone::Pending, "Driver: checking the official usbip-win2 release\u{2026}");
    let shared = shared.clone();
    std::thread::spawn(move || {
        let logger = control::logger();
        logf!(logger, "Driver installation requested from the window");
        let progress = |text: &str| {
            let text = text.trim_end_matches('.');
            let mut chars = text.chars();
            let first = chars.next().map(|c| c.to_lowercase().to_string()).unwrap_or_default();
            set_notice(&shared, Tone::Pending, &format!("Driver: {first}{}\u{2026}", chars.as_str()));
        };
        let download = download_latest_usbip_installer(&driver_downloads_dir(), &progress, &open_url, &|line| {
            logf!(logger, "{line}")
        });
        let (asset_path, asset_name) = match download {
            Ok(downloaded) => downloaded,
            Err(err) => {
                open_url(USBIP_RELEASE_PAGE);
                set_notice(
                    &shared,
                    Tone::Error,
                    "Driver: automatic download failed \u{00b7} the official release page was opened",
                );
                let mut message = "The automatic usbip-win2 download could not reach GitHub.\n\n\
                    The official release page has been opened in your browser. Download and run the Windows x64 installer there."
                    .to_string();
                if is_name_resolution_error(&err) {
                    message += "\n\nWindows reported a DNS name-resolution error. Check the Internet connection, DNS settings, firewall, VPN, or proxy on this PC.";
                }
                message += &format!("\n\nTechnical details:\n{err}");
                report_error(&shared, message);
                return;
            }
        };
        set_notice(&shared, Tone::Pending, "Driver: verifying the installer signature\u{2026}");
        if let Err(signature_err) = verify_authenticode_signature(&asset_path) {
            logf!(logger, "Installer Authenticode warning; continuing with the official GitHub asset: {signature_err}");
            if let Err(folder_err) = reveal_downloaded_file(&asset_path) {
                logf!(logger, "Could not reveal downloaded installer before launch: {folder_err}");
            }
            set_notice(
                &shared,
                Tone::Pending,
                "Driver: publisher signature not verified \u{00b7} starting the installer from the download folder\u{2026}",
            );
        } else {
            set_notice(&shared, Tone::Pending, "Driver: signature valid \u{00b7} starting the installer\u{2026}");
        }
        if let Err(err) = start_installer_elevated(&asset_path) {
            let folder_result = reveal_downloaded_file(&asset_path);
            set_notice(
                &shared,
                Tone::Error,
                "Driver: the installer could not be started \u{00b7} its download folder was opened",
            );
            let mut message = format!(
                "The installer was downloaded but Windows could not start it. The download folder has been opened and the installer selected.\n\nDownloaded file:\n{}\n\nStart error:\n{err}",
                asset_path.display()
            );
            if let Err(folder_err) = folder_result {
                message += &format!("\n\nThe folder could not be opened automatically:\n{folder_err}");
            }
            report_error(&shared, message);
            return;
        }
        set_notice(
            &shared,
            Tone::Ok,
            &format!("Driver: {asset_name} started \u{00b7} accept the administrator prompt and reboot if requested"),
        );
    });
}

fn verify_authenticode_signature(path: &Path) -> Result<(), String> {
    let system_root = std::env::var("SystemRoot").unwrap_or_default();
    let program_files = std::env::var("ProgramFiles").unwrap_or_default();
    let candidates = [
        PathBuf::from(system_root).join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe"),
        PathBuf::from(program_files).join("PowerShell").join("7").join("pwsh.exe"),
    ];
    let Some(powershell) = candidates.into_iter().find(|candidate| candidate.is_file()) else {
        return Err("Windows PowerShell was not found, so the Authenticode signature cannot be checked".into());
    };
    let escaped = path.display().to_string().replace('\'', "''");
    let script = format!(
        "$s=Get-AuthenticodeSignature -LiteralPath '{escaped}'; Write-Output ('Status=' + $s.Status); Write-Output ('Signer=' + $s.SignerCertificate.Subject); if ($s.Status -ne 'Valid') {{ exit 23 }}"
    );
    let mut command = Command::new(powershell);
    command.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script]);
    hide_command_window(&mut command);
    let (text, result) = combined_output(&mut command);
    logf!(control::logger(), "Installer Authenticode check: {}", single_line(&text));
    match result {
        Ok(()) => Ok(()),
        Err(err) => Err(format!("Authenticode status is not Valid: {}", if text.is_empty() { err } else { text })),
    }
}

/// ShellExecuteW with the main window as the owner; returns its result code.
fn shell_execute(verb: &str, file: &str, parameters: Option<&str>, directory: Option<&str>, show: i32) -> isize {
    let (verb, file) = (wide(verb), wide(file));
    let parameters = parameters.map(wide);
    let directory = directory.map(wide);
    // SAFETY: all strings are NUL-terminated UTF-16 that outlive the call.
    unsafe {
        ShellExecuteW(
            main_window(),
            verb.as_ptr(),
            file.as_ptr(),
            parameters.as_ref().map_or(null(), |p| p.as_ptr()),
            directory.as_ref().map_or(null(), |d| d.as_ptr()),
            show,
        ) as isize
    }
}

fn start_installer_elevated(path: &Path) -> Result<(), String> {
    let path = std::path::absolute(path).map_err(|err| format!("resolve installer path: {err}"))?;
    let metadata = std::fs::metadata(&path).map_err(|err| format!("installer file is unavailable: {err}"))?;
    if metadata.is_dir() {
        return Err("installer path is a directory".into());
    }
    let path_text = path.display().to_string();
    let directory = path.parent().map(|d| d.display().to_string()).unwrap_or_default();
    let is_msi = path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("msi"));
    let (file, parameters) = if is_msi {
        ("msiexec.exe".to_string(), format!("/i \"{path_text}\""))
    } else {
        (path_text.clone(), String::new())
    };
    let result = shell_execute("runas", &file, Some(&parameters), Some(&directory), SW_SHOWNORMAL);
    let error = std::io::Error::last_os_error();
    if result > 32 {
        return Ok(());
    }
    // If the explicit elevation verb is unavailable, ask the normal Windows
    // installer association to launch the file.
    let fallback = shell_execute("open", &path_text, None, Some(&directory), SW_SHOWNORMAL);
    let fallback_error = std::io::Error::last_os_error();
    if fallback > 32 {
        return Ok(());
    }
    if fallback_error.raw_os_error().unwrap_or(0) != 0 {
        return Err(format!("elevated start returned {result}; normal start returned {fallback}: {fallback_error}"));
    }
    if error.raw_os_error().unwrap_or(0) != 0 {
        return Err(format!("elevated start returned {result}: {error}; normal start returned {fallback}"));
    }
    Err(format!("elevated start returned error code {result}; normal start returned error code {fallback}"))
}

fn reveal_downloaded_file(path: &Path) -> Result<(), String> {
    match std::fs::metadata(path) {
        Err(err) => return Err(format!("downloaded installer is unavailable: {err}")),
        Ok(metadata) if metadata.is_dir() => return Err("downloaded installer path is a directory".into()),
        Ok(_) => {}
    }
    open_folder(&driver_downloads_dir(), "Downloads")
}

fn driver_downloads_dir() -> PathBuf {
    if let Ok(downloads) = windows_downloads_dir() {
        return downloads;
    }
    if let Ok(home) = std::env::var("USERPROFILE")
        && !home.trim().is_empty()
    {
        return PathBuf::from(home).join("Downloads");
    }
    std::env::temp_dir().join("Downloads")
}

fn windows_downloads_dir() -> Result<PathBuf, String> {
    let mut raw: *mut u16 = null_mut();
    // SAFETY: SHGetKnownFolderPath allocates the path, which is freed below.
    let hr = unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, 0, null_mut(), &mut raw) };
    if hr < 0 || raw.is_null() {
        return Err(format!("SHGetKnownFolderPath failed with HRESULT 0x{:08X}", hr as u32));
    }
    // SAFETY: raw points to a NUL-terminated UTF-16 string owned by COM.
    let path = unsafe {
        let mut len = 0;
        while len < 32768 && *raw.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(raw, len));
        CoTaskMemFree(raw as *const c_void);
        text
    };
    if path.trim().is_empty() {
        return Err("Windows returned an empty Downloads path".into());
    }
    Ok(PathBuf::from(path))
}

/// Opens the folder holding config.toml and the log: the service's while it
/// runs the mixer.
pub fn open_settings_folder(shared: &SharedState) {
    let directory = if lock(shared).mode == Mode::Service { service_data_dir() } else { application_data_dir() };
    if let Err(err) = open_folder(&directory, "settings") {
        report_error(shared, format!("Could not open the settings folder:\n\n{}\n\n{err}", directory.display()));
    }
}

/// Opens `directory` in Explorer, creating it first if needed.
fn open_folder(directory: &Path, name: &str) -> Result<(), String> {
    std::fs::create_dir_all(directory).map_err(|err| format!("create {name} folder: {err}"))?;
    let result = shell_execute("open", &directory.display().to_string(), None, None, SW_SHOWNORMAL);
    let error = std::io::Error::last_os_error();
    if result <= 32 {
        let explorer = PathBuf::from(std::env::var("SystemRoot").unwrap_or_default()).join("explorer.exe");
        if Command::new(explorer).arg(directory).spawn().is_ok() {
            return Ok(());
        }
        if error.raw_os_error().unwrap_or(0) != 0 {
            return Err(format!("open {name} folder returned {result}: {error}"));
        }
        return Err(format!("open {name} folder returned error code {result}"));
    }
    Ok(())
}

pub fn open_url(url: &str) {
    if shell_execute("open", url, None, None, SW_SHOWNORMAL) <= 32 {
        let _ = Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
    }
}
