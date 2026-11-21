#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

//! Lemon Mix: an audio mixer whose virtual inputs and outputs are USB Audio
//! Class 2.0 devices served over USB/IP, so Windows' in-box USB audio driver
//! exposes them as standard playback and recording devices, mixed with the
//! computer's physical devices.

mod cli;
mod config;
#[cfg(windows)]
mod ctl;
#[cfg(windows)]
mod gui;
mod icon;
mod mixer;
#[cfg(windows)]
mod pipe_client;
#[cfg(windows)]
mod platform;
mod protocol;

use std::env;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cables::logf;
use cables::logging::{AsyncWriter, Logger, flush_log, set_app_log_writer};
use cables::uac2::{CableKind, Descriptors, Device, MAX_LATENCY_MS, MIN_LATENCY_MS};
use clap::Parser;

use crate::cli::Cli;
use crate::config::{
    Cable, Config, DEFAULT_BUFFER_MS, DEFAULT_HARDWARE_PORTS, DEFAULT_LATENCY_MS, DEFAULT_LISTEN, MAX_BUFFER_MS, MAX_CABLES, MIN_BUFFER_MS,
    config_bool, config_cables, config_int, config_string, ensure_config_file,
    load_config_content, numbered_cables, parse_config,
};
use crate::mixer::{HostSettings, MANUFACTURER, MixerHost};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The log's file name in the settings folder.
const LOG_FILE: &str = "LemonMix.log";

fn main() {
    let parsed = Cli::try_parse_from(env::args_os());
    // Controlling the mixer is a quick request, made often (from a Stream
    // Deck key, say): it leaves the log alone.
    #[cfg(windows)]
    if let Ok(Cli { control: Some(args), .. }) = &parsed {
        attach_parent_console();
        std::process::exit(ctl::run(args));
    }
    // The service keeps its settings and log in a folder shared by every user,
    // and has no desktop to show a dialog on.
    let service = matches!(&parsed, Ok(cli) if cli.service);
    if service {
        config::use_service_data_dir();
    }
    // Only a normal start clears the log. Helper runs (the administrator broker
    // in particular runs alongside the application), --help and command-line
    // errors append to it instead.
    let fresh = matches!(&parsed, Ok(cli) if !cli.is_helper_mode());
    let (logger, log_closer, log_path) = new_application_logger(fresh);
    let code = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&logger, &log_path, parsed))) {
        Ok(code) => code,
        Err(panic) => {
            let detail = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            let message = format!("Unexpected startup failure: {detail}");
            logf!(logger, "PANIC: {message}");
            if !service {
                show_fatal_dialog(&format!("{message}\n\nDiagnostic log:\n{log_path}"));
            }
            0
        }
    };
    if let Some(closer) = log_closer {
        closer.close();
    }
    if code != 0 {
        std::process::exit(code);
    }
}

/// Runs the application and returns the process exit code.
fn run(logger: &Logger, log_path: &str, parsed: Result<Cli, clap::Error>) -> i32 {
    logf!(logger, "============================================================");
    logf!(logger, "Lemon Mix {VERSION} ({}/{})", env::consts::OS, arch());
    logf!(logger, "Process started. Executable: {}", executable_path());
    logf!(logger, "Log file: {log_path}");
    logf!(logger, "USB/IP protocol: 0x{:04X}", cables::usbip::PROTOCOL_VERSION);
    cables::raise_process_priority(logger);

    let cli = match parsed {
        Ok(cli) => cli,
        Err(err) => {
            // --help prints to standard output and exits 0; errors exit 2.
            let _ = err.print();
            return err.exit_code();
        }
    };
    if cli.version {
        println!("Lemon Mix {VERSION} ({}/{})", env::consts::OS, arch());
        return 0;
    }

    let config = parse_config(&load_config_content()).unwrap_or_else(|err| {
        // The application still starts, with the defaults; saving refuses to
        // overwrite the file until it is fixed.
        logf!(logger, "Using the default settings: {err}");
        Config::default()
    });
    let cables = startup_cables(cli.cables, &config, logger);
    let settings = host_settings(&cli, &config, logger);
    logf!(logger, "Diagnostic logging: {}", settings.diagnostics);

    #[cfg(windows)]
    {
        if cli.attach_broker {
            let address = cli.broker_address.clone().unwrap_or_default();
            let token = cli.broker_token.clone().unwrap_or_default();
            if let Err(err) = platform::run_attach_broker(&address, &token, &settings.address, logger) {
                logf!(logger, "Administrator broker failed: {err}");
                flush_log();
                return 1;
            }
            return 0;
        }
        // The application starts these elevated and reports a failure itself.
        if let Some(config) = &cli.install_service {
            return match platform::install_service(Path::new(config), logger) {
                Ok(()) => 0,
                Err(err) => fail(logger, log_path, "Could not install the Windows service", &err, true),
            };
        }
        if cli.uninstall_service {
            return match platform::uninstall_service(logger) {
                Ok(()) => 0,
                Err(err) => fail(logger, log_path, "Could not uninstall the Windows service", &err, true),
            };
        }
        if let Some(names) = &cli.measure_latency {
            return match devices::measure_latency(&names[0], &names[1], 10, logger) {
                Ok(report) => {
                    println!("{report}");
                    logf!(logger, "{}", report.replace('\n', " "));
                    0
                }
                Err(err) => fail(logger, log_path, "Latency measurement failed", &err, true),
            };
        }
    }

    let self_test = cli.self_test;
    let dump = cli.dump_descriptors;
    if self_test || dump {
        // The diagnostics check one stand-in cable of each kind when none
        // are configured.
        let checked = if cables.is_empty() {
            CableKind::ALL
                .iter()
                .enumerate()
                .map(|(i, &kind)| Cable { kind, ..Cable::numbered(i as u32 + 1) })
                .collect()
        } else {
            cables.clone()
        };
        let descriptors: Result<Vec<Descriptors>, String> = checked
            .iter()
            .map(|c| {
                Descriptors::build(c.kind, c.id, &c.serial, &c.name, MANUFACTURER)
                    .map_err(|err| format!("create cable {} ({}): {err}", c.id, c.name))
            })
            .collect();
        let descriptors = match descriptors {
            Ok(d) => d,
            Err(err) => return fail(logger, log_path, "Could not create the virtual cable devices", &err, true),
        };
        if dump {
            for (cable, d) in checked.iter().zip(&descriptors) {
                print!("\n[{} | {} | bus ID {}]\n", d.product, cable.kind.name(), cables::uac2::bus_id(cable.id));
                print!("{}", hex_dump(&d.device));
                print!("{}", hex_dump(&d.config));
            }
            return 0;
        }
        if let Err(err) = run_self_test(&descriptors, logger) {
            return fail(logger, log_path, "Self-test failed", &err, true);
        }
        println!("Lemon Mix {VERSION} self-test passed.");
        logf!(logger, "Self-test passed");
        return 0;
    }

    // Frame numbers must match the ones usbip-win2 reports, or every stream
    // breaks: refuse to start on any other clock.
    if let Err(err) = cables::usbip::busclock::require_host_clock() {
        return fail(logger, log_path, "Could not read the USB frame clock", &err, cli.service);
    }

    ensure_config_file(logger);
    let mixer = config::load_active_profile(&settings.profile).map_or_else(
        |err| {
            logf!(logger, "Using default mixer settings: {err}");
            config::SavedMixer::default()
        },
        |(mixer, _)| mixer,
    );
    logf!(logger, "Profile: {}", settings.profile);
    let host = MixerHost::new(settings, mixer, logger, log_path, true);
    host.set_initial_cables(cables);

    #[cfg(windows)]
    {
        if cli.service {
            if let Err(err) = platform::run_service(&host) {
                return fail(logger, log_path, "Could not run the Windows service", &err, true);
            }
            logf!(logger, "Windows service stopped");
            return 0;
        }
        // The window starts the mixer, unless the Windows service runs it.
        gui::run_gui(&host);
        logf!(logger, "Lemon Mix window closed normally");
        platform::control::stop();
        host.shutdown();
        0
    }
    #[cfg(not(windows))]
    {
        // There is no window outside Windows: serve until the process stops.
        host.start();
        let _ = host.restart(&host.cables());
        loop {
            std::thread::park();
        }
    }
}

/// Logs and reports a fatal startup error, and returns the exit code.
fn fail(logger: &Logger, log_path: &str, title: &str, err: &str, console_only: bool) -> i32 {
    let message = format!("{title}: {err}");
    logf!(logger, "FATAL: {message}");
    eprintln!("[ERROR] {message}");
    if !console_only {
        show_fatal_dialog(&format!("{message}\n\nDiagnostic log:\n{log_path}"));
    }
    flush_log();
    1
}

/// Lets output reach the terminal this was started from, if any: the
/// executable is a Windows program, which has no console of its own. Output
/// already redirected to a file or pipe is left as it is.
#[cfg(windows)]
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE};
    // SAFETY: queries and attaches this process's standard handles.
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

fn show_fatal_dialog(message: &str) {
    #[cfg(windows)]
    gui::show_fatal_dialog(message);
    #[cfg(not(windows))]
    eprintln!("{message}");
}

/// The longest stall the stall test may simulate; it repeats every minute.
const MAX_STALL_TEST_MS: i64 = 10_000;

/// The settings, from the command line, config.toml or the defaults, with
/// invalid values replaced by the defaults and logged.
fn host_settings(cli: &Cli, config: &Config, logger: &Logger) -> HostSettings {
    let mut latency_ms = cli.latency_ms.unwrap_or_else(|| config_int(config, "latency_ms", DEFAULT_LATENCY_MS));
    if !(MIN_LATENCY_MS..=MAX_LATENCY_MS).contains(&latency_ms) {
        logf!(
            logger,
            "Invalid latency {latency_ms} ms (must be {MIN_LATENCY_MS}-{MAX_LATENCY_MS} ms); falling back to {DEFAULT_LATENCY_MS} ms"
        );
        latency_ms = DEFAULT_LATENCY_MS;
    }
    let mut buffer_ms = cli.buffer_ms.unwrap_or_else(|| config_int(config, "buffer_ms", DEFAULT_BUFFER_MS));
    if !(MIN_BUFFER_MS..=MAX_BUFFER_MS).contains(&buffer_ms) {
        logf!(
            logger,
            "Invalid device buffer {buffer_ms} ms (must be {MIN_BUFFER_MS}-{MAX_BUFFER_MS} ms); falling back to {DEFAULT_BUFFER_MS} ms"
        );
        buffer_ms = DEFAULT_BUFFER_MS;
    }
    // As many hardware strips and buses as the active profile has tables for.
    let profile = config::config_profile(config);
    let defaults = (DEFAULT_HARDWARE_PORTS as usize, DEFAULT_HARDWARE_PORTS as usize);
    let (hardware_inputs, hardware_outputs) =
        config::load_active_profile(&profile).map_or(defaults, |(_, counts)| counts);

    let mut stall_test_ms = cli.stall_test_ms.unwrap_or_else(|| config_int(config, "stall_test_ms", 0));
    if !(0..=MAX_STALL_TEST_MS).contains(&stall_test_ms) {
        logf!(logger, "Invalid stall test {stall_test_ms} ms (must be 0-{MAX_STALL_TEST_MS} ms); turning it off");
        stall_test_ms = 0;
    }
    let stall_test = (stall_test_ms > 0).then(|| Duration::from_millis(stall_test_ms as u64));

    logf!(
        logger,
        "Audio: {latency_ms} ms cable latency, {buffer_ms} ms device buffers, {hardware_inputs} hardware inputs, {hardware_outputs} hardware outputs"
    );
    if stall_test.is_some() {
        logf!(logger, "Stall test: holding audio transfers and the engine for {stall_test_ms} ms every minute");
    }
    HostSettings {
        address: cli.listen.clone().unwrap_or_else(|| config_string(config, "listen", DEFAULT_LISTEN)),
        latency_ms,
        stall_test,
        diagnostics: cli.diagnostics.unwrap_or_else(|| config_bool(config, "diagnostics", false)),
        buffer_ms,
        hardware_inputs,
        hardware_outputs,
        profile,
    }
}

/// The cables to start with: cables 1 to N for --cables N, otherwise the
/// [[cable]] tables of config.toml, which may be none.
fn startup_cables(cli_count: Option<i64>, config: &Config, logger: &Logger) -> Vec<Cable> {
    match cli_count {
        Some(count) if (1..=MAX_CABLES as i64).contains(&count) => numbered_cables(count),
        Some(count) => {
            logf!(logger, "Invalid cable count {count}; using the cables in config.toml");
            config_cables(config)
        }
        None => config_cables(config),
    }
}

/// Checks every cable's descriptors and control requests, the original
/// cable's loopback, and the same audio path through the mixer engine.
fn run_self_test(descriptors: &[Descriptors], logger: &Logger) -> Result<(), String> {
    use cables::audio::PACKET_BYTES;
    use cables::uac2::{REQUEST_SET_CONFIGURATION, REQUEST_SET_INTERFACE, SetupPacket};

    let activate = |dev: &Device| -> Result<(), String> {
        let (_, status) =
            dev.handle_control(SetupPacket { request: REQUEST_SET_CONFIGURATION, value: 1, ..Default::default() }, &[]);
        if status != 0 {
            return Err(format!("set configuration status {status}"));
        }
        for iface in 1..dev.kind().interface_count() as u16 {
            let setup =
                SetupPacket { request_type: 0x01, request: REQUEST_SET_INTERFACE, index: iface, value: 1, length: 0 };
            let (_, status) = dev.handle_control(setup, &[]);
            if status != 0 {
                return Err(format!("activate interface {iface} status {status}"));
            }
        }
        Ok(())
    };
    let packet: Vec<u8> = (0..PACKET_BYTES).map(|i| (i * 7 + 1) as u8).collect();
    let frame = 1_000_000;
    let check_loopback = |dev: &Device, label: &str| -> Result<(), String> {
        let latency = dev.audio.delay();
        dev.write_playback(frame, [packet.as_slice()]);
        let mut early = vec![0u8; PACKET_BYTES];
        dev.read_capture(frame + latency - 1, &mut early, [PACKET_BYTES]);
        let mut out = vec![0u8; PACKET_BYTES];
        dev.read_capture(frame + latency, &mut out, [PACKET_BYTES]);
        if early.iter().any(|&b| b != 0) {
            return Err(format!("{label}: PCM loopback came out early"));
        }
        if out != packet {
            return Err(format!("{label}: PCM loopback mismatch"));
        }
        Ok(())
    };

    for d in descriptors {
        d.validate().map_err(|err| format!("{}: {err}", d.product))?;
        let rebuilt = Descriptors::build(d.kind, d.cable_number, &d.serial, &d.product, MANUFACTURER)?;
        let dev = Device::with_audio(
            d.cable_number,
            rebuilt,
            Arc::new(cables::audio::DelayLine::new(DEFAULT_LATENCY_MS as u64)),
        );
        activate(&dev).map_err(|err| format!("{}: {err}", d.product))?;
        if d.kind == CableKind::Loopback {
            check_loopback(&dev, &format!("{} (on its own)", d.product))?;
        }
    }

    // The same loopback through the engine: a cable's strip routed to its
    // own bus at unity is bit exact.
    let settings = HostSettings {
        address: "127.0.0.1:0".into(),
        latency_ms: DEFAULT_LATENCY_MS,
        stall_test: None,
        diagnostics: false,
        buffer_ms: DEFAULT_BUFFER_MS,
        hardware_inputs: 1,
        hardware_outputs: 1,
        profile: config::DEFAULT_PROFILE.into(),
    };
    let host = MixerHost::new(settings, config::SavedMixer::default(), logger, "", false);
    let cable = Cable::numbered(1);
    host.set_initial_cables(vec![cable.clone()]);
    let device = host.self_test_device(&cable)?;
    activate(&device)?;
    check_loopback(&device, "loopback through the mixer")
}

/// Formats bytes as encoding/hex Dump does (the `hexdump -C` layout).
fn hex_dump(data: &[u8]) -> String {
    let mut text = String::new();
    for (line, chunk) in data.chunks(16).enumerate() {
        text.push_str(&format!("{:08x}  ", line * 16));
        for position in 0..16 {
            match chunk.get(position) {
                Some(b) => text.push_str(&format!("{b:02x} ")),
                None => text.push_str("   "),
            }
            if position == 7 || position == 15 {
                text.push(' ');
            }
        }
        text.push('|');
        text.extend(chunk.iter().map(|&b| if (32..=126).contains(&b) { b as char } else { '.' }));
        text.push_str("|\n");
    }
    text
}

fn arch() -> &'static str {
    match env::consts::ARCH {
        "x86_64" => "amd64",
        "x86" => "386",
        "aarch64" => "arm64",
        other => other,
    }
}

fn executable_path() -> String {
    env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "unknown".into())
}

/// Flushes and closes the log file at exit.
pub struct LogCloser(Arc<AsyncWriter>);

impl LogCloser {
    fn close(self) {
        self.0.close();
    }
}

/// Opens the persistent log. With `fresh` set, the log is cleared so that it
/// holds only the current session and cannot grow forever.
fn new_application_logger(fresh: bool) -> (Logger, Option<LogCloser>, String) {
    let exe_logs = env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.join("Logs")));
    let candidates: Vec<PathBuf> =
        [Some(config::application_data_dir()), exe_logs, Some(env::temp_dir().join("Lemon Mix"))]
            .into_iter()
            .flatten()
            .collect();

    for dir in candidates {
        if dir.as_os_str().is_empty() || dir == *"." || fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let path = dir.join(LOG_FILE);
        if fresh && OpenOptions::new().write(true).create(true).truncate(true).open(&path).is_err() {
            continue;
        }
        // Append mode: the administrator broker writes to the same file.
        let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else {
            continue;
        };
        let writer = AsyncWriter::new(file);
        set_app_log_writer(Arc::clone(&writer));
        return (Logger::asynchronous(Arc::clone(&writer)), Some(LogCloser(writer)), path.display().to_string());
    }

    let path = env::temp_dir().join(LOG_FILE);
    (Logger::stderr(), None, path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn application_log_starts_fresh() {
        let _guard = config::tests::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = config::tests::temp_app_data("log");
        let path = dir.join("Lemon Mix").join(LOG_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        let open = |fresh: bool, line: &str| {
            fs::write(&path, "previous session\n").unwrap();
            let (logger, closer, logged_path) = new_application_logger(fresh);
            assert_eq!(logged_path, path.display().to_string());
            logf!(logger, "{line}");
            closer.unwrap().close();
            fs::read_to_string(&path).unwrap()
        };
        let fresh = open(true, "new session");
        assert!(!fresh.contains("previous session") && fresh.contains("new session"), "fresh log={fresh:?}");
        let appended = open(false, "helper");
        assert!(appended.contains("previous session") && appended.contains("helper"), "appended log={appended:?}");
    }

    #[test]
    fn hex_dump_matches() {
        let data = |n: usize| -> Vec<u8> { (0..n).map(|i| (i * 37 + 20) as u8).collect() };
        assert_eq!(hex_dump(&data(0)), "");
        assert_eq!(hex_dump(&data(1)), "00000000  14                                                |.|\n");
        assert_eq!(
            hex_dump(&data(18)),
            "00000000  14 39 5e 83 a8 cd f2 17  3c 61 86 ab d0 f5 1a 3f  |.9^.....<a.....?|\n00000010  64 89                                             |d.|\n"
        );
    }

    #[test]
    fn self_test_passes() {
        let descriptors: Vec<Descriptors> = CableKind::ALL
            .iter()
            .enumerate()
            .map(|(i, &kind)| Descriptors::build(kind, i as u32 + 1, "S", "Test", MANUFACTURER).unwrap())
            .collect();
        run_self_test(&descriptors, &Logger::discard()).unwrap();
    }

    #[test]
    fn startup_cables_sources() {
        let logger = Logger::discard();
        let named = parse_config(
            "[[cable]]\nid = 3\nserial = \"0000AAAA\"\nname = \"Game\"\n\n\
             [[cable]]\nid = 1\nserial = \"0000BBBB\"\nname = \"Chat\"\nkind = \"input\"\n",
        )
        .unwrap();
        let ids = |cables: Vec<Cable>| cables.iter().map(|c| c.id).collect::<Vec<_>>();
        assert_eq!(ids(startup_cables(None, &named, &logger)), [3, 1]);
        assert_eq!(ids(startup_cables(Some(2), &named, &logger)), [1, 2], "--cables overrides the configuration");
        let no_cables = parse_config("listen = \"x\"\n").unwrap();
        assert_eq!(ids(startup_cables(None, &no_cables, &logger)), [0u32; 0], "no default cable");
        assert_eq!(ids(startup_cables(Some(40), &named, &logger)), [3, 1], "invalid --cables is ignored");
    }

    #[test]
    fn invalid_settings_fall_back() {
        let config = parse_config("latency_ms = 0\nbuffer_ms = 1000\n").unwrap();
        let settings = host_settings(&Cli::default(), &config, &Logger::discard());
        assert_eq!((settings.latency_ms, settings.buffer_ms), (DEFAULT_LATENCY_MS, DEFAULT_BUFFER_MS));
    }

    /// The hardware strips and buses are the active profile's.
    #[test]
    fn hardware_strips_and_buses_come_from_the_active_profile() {
        let _guard = config::tests::ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        config::tests::temp_app_data("main-profile");
        let counts = |content: &str| {
            let settings = host_settings(&Cli::default(), &parse_config(content).unwrap(), &Logger::discard());
            (settings.profile, settings.hardware_inputs, settings.hardware_outputs)
        };
        let defaults = DEFAULT_HARDWARE_PORTS as usize;
        assert_eq!(counts(""), ("Default".into(), defaults, defaults), "no profile file yet");
        std::fs::create_dir_all(config::profiles_dir()).unwrap();
        let tables = "[[strip]]\nid = \"H1\"\n[[strip]]\nid = \"H3\"\n[[strip]]\nid = \"V2\"\n\n[[bus]]\nid = \"B2\"\n";
        std::fs::write(config::profiles_dir().join("Live.toml"), tables).unwrap();
        assert_eq!(counts("profile = \"Live\"\n"), ("Live".into(), 3, 0));
    }
}
