//! config.toml: the settings, the cables and the mixer, next to the log in
//! the application data folder. Command-line flags override the settings.
//! The application edits the file in place, rewriting only the tables it
//! owns, so comments and anything added by hand are kept.

use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

use cables::logf;
use cables::logging::Logger;
use cables::uac2::{CableKind, MAX_LATENCY_MS, MIN_LATENCY_MS};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

pub const MAX_CABLES: usize = 32;
/// Cable IDs fit the USB product IDs 0xCB01-0xCBFF (and the input and output
/// ranges beside them).
pub const MAX_CABLE_ID: u32 = 255;
pub const MAX_CABLE_NAME: usize = 64;
/// Built-in settings, used when neither config.toml nor a command-line flag
/// sets a value. The port is not the usual 3240, so a separate virtual cable
/// server can run beside the mixer.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:3241";
/// The delay between a cable's playback side and the mixer reading it. Windows
/// sends playback about 10 ms ahead, so this leaves 10 ms for system stalls.
pub const DEFAULT_LATENCY_MS: i64 = 20;
/// Audio kept between each physical device and the mixer.
pub const DEFAULT_BUFFER_MS: i64 = 20;
pub const MIN_BUFFER_MS: i64 = 5;
pub const MAX_BUFFER_MS: i64 = 200;
/// Hardware strips and buses to start with, as in the classic five-in, five-out
/// layout. There is no maximum: more can be added in the window.
pub const DEFAULT_HARDWARE_PORTS: i64 = 5;

/// The file name of the configuration.
pub const CONFIG_FILE: &str = "config.toml";
/// The keys of the tables the application rewrites.
const CABLE_KEY: &str = "cable";
const STRIP_KEY: &str = "strip";
const BUS_KEY: &str = "bus";

/// The folder this process uses instead of the per-user one: the service's.
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The folder for the configuration and the log: the user's own, or the
/// shared one in the Windows service.
pub fn application_data_dir() -> PathBuf {
    if let Some(dir) = DATA_DIR.get() {
        return dir.clone();
    }
    if let Ok(local) = env::var("LOCALAPPDATA")
        && !local.trim().is_empty()
    {
        let local = if cfg!(windows) { local.replace('/', "\\") } else { local };
        return PathBuf::from(local).join("Lemon Mix");
    }
    if let Some(base) = user_cache_dir() {
        return base.join("Lemon Mix");
    }
    env::temp_dir().join("Lemon Mix")
}

/// The Windows service's folder for its configuration and log, shared by every
/// user: `%ProgramData%\Lemon Mix`.
pub fn service_data_dir() -> PathBuf {
    let base =
        env::var("ProgramData").ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| r"C:\ProgramData".into());
    PathBuf::from(base).join("Lemon Mix")
}

/// Makes this process use the service's folder, as the service does.
pub fn use_service_data_dir() {
    let _ = DATA_DIR.set(service_data_dir());
}

/// The platform's per-user cache folder
fn user_cache_dir() -> Option<PathBuf> {
    let non_empty = |name: &str| env::var(name).ok().filter(|v| !v.trim().is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        non_empty("LocalAppData")
    } else if cfg!(target_os = "macos") {
        non_empty("HOME").map(|home| home.join("Library/Caches"))
    } else {
        non_empty("XDG_CACHE_HOME").or_else(|| non_empty("HOME").map(|home| home.join(".cache")))
    }
}

pub fn config_file_path() -> PathBuf {
    application_data_dir().join(CONFIG_FILE)
}

/// The configuration files read, in order; the first that exists is used.
fn config_paths() -> [PathBuf; 2] {
    [config_file_path(), PathBuf::from(CONFIG_FILE)]
}

/// The text of the first config.toml found, or "" when there is none.
pub fn load_config_content() -> String {
    for path in config_paths() {
        if let Ok(content) = fs::read(&path) {
            return String::from_utf8_lossy(&content).into_owned();
        }
    }
    String::new()
}

/// The parsed configuration; empty when there is no file.
#[derive(Default)]
pub struct Config {
    document: DocumentMut,
}

/// Parses config.toml. The error describes a TOML syntax error.
pub fn parse_config(content: &str) -> Result<Config, String> {
    let document = content.parse::<DocumentMut>().map_err(|err| format!("{CONFIG_FILE} is not valid TOML: {err}"))?;
    Ok(Config { document })
}

/// One virtual cable. Its serial number, created with the cable, is its
/// identity in Windows: a new cable is always a new device there, with its own
/// name, while an existing one keeps its settings across restarts. The ID sets
/// the USB product ID and the bus ID (1-<id>) among the running cables.
#[derive(Clone, Debug, PartialEq)]
pub struct Cable {
    pub id: u32,
    pub serial: String,
    pub name: String,
    pub kind: CableKind,
}

impl Cable {
    /// Loopback cable `id` with its default name and a serial number derived
    /// from the ID, for --cables and the diagnostics.
    pub fn numbered(id: u32) -> Cable {
        Cable {
            id,
            serial: format!("VCABLE-{id:03}"),
            name: format!("Virtual Cable {id:02}"),
            kind: CableKind::Loopback,
        }
    }
}

/// Cables 1 to `count` with their default names.
pub fn numbered_cables(count: i64) -> Vec<Cable> {
    (1..=count.clamp(0, MAX_CABLES as i64) as u32).map(Cable::numbered).collect()
}

/// The [[cable]] tables of the configuration, in file order. A table with an
/// out-of-range ID, an invalid serial number, name or kind is skipped, as are
/// repeated IDs, serial numbers or names and any beyond the first 32. A table
/// without a kind is a loopback cable, as every cable was before kinds.
pub fn config_cables(config: &Config) -> Vec<Cable> {
    let Some(tables) = config.document.get(CABLE_KEY).and_then(Item::as_array_of_tables) else {
        return Vec::new();
    };
    let mut cables: Vec<Cable> = Vec::new();
    for table in tables.iter() {
        let id = table.get("id").and_then(Item::as_integer).and_then(|id| u32::try_from(id).ok());
        let serial = table.get("serial").and_then(Item::as_str).map(|s| s.trim().to_ascii_uppercase());
        let name = table.get("name").and_then(Item::as_str).and_then(|n| normalize_cable_name(n).ok());
        let kind = match table.get("kind") {
            None => Some(CableKind::Loopback),
            Some(kind) => kind.as_str().and_then(CableKind::from_name),
        };
        let (Some(id), Some(serial), Some(name), Some(kind)) = (id, serial, name, kind) else {
            continue;
        };
        let duplicate = cables.iter().any(|c| c.id == id || c.serial == serial || same_cable_name(&c.name, &name));
        if (1..=MAX_CABLE_ID).contains(&id) && is_valid_serial(&serial) && !duplicate {
            cables.push(Cable { id, serial, name, kind });
        }
    }
    cables.truncate(MAX_CABLES);
    cables
}

/// Whether `serial` can be a cable's USB serial number: 1 to 32 letters,
/// digits or hyphens. Windows builds the device's instance ID from it.
pub fn is_valid_serial(serial: &str) -> bool {
    (1..=32).contains(&serial.len()) && serial.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The lowest ID no cable uses.
fn next_cable_id(cables: &[Cable]) -> Option<u32> {
    (1..=MAX_CABLE_ID).find(|id| !cables.iter().any(|c| c.id == *id))
}

/// A random serial number for a new cable: 8 hexadecimal digits, different
/// from every existing cable's.
pub fn new_cable_serial(cables: &[Cable]) -> String {
    use std::hash::{BuildHasher, Hasher};
    loop {
        // RandomState is seeded randomly per process and per instance.
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        hasher.write_u128(now);
        let serial = format!("{:08X}", hasher.finish() as u32);
        if !cables.iter().any(|c| c.serial == serial) {
            return serial;
        }
    }
}

/// Puts a cable name in its standard form: trimmed, with each run of spaces
/// reduced to one. Names may contain only letters, digits, spaces, hyphens,
/// underscores and parentheses, and at most 64 characters.
pub fn normalize_cable_name(name: &str) -> Result<String, String> {
    let name = name.split(' ').filter(|word| !word.is_empty()).collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        return Err("Enter a name for the cable.".into());
    }
    if !name.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '(' | ')')) {
        return Err(
            "Cable names can contain only letters, digits, spaces, hyphens, underscores and parentheses.".into()
        );
    }
    if name.chars().count() > MAX_CABLE_NAME {
        return Err(format!("Cable names can be at most {MAX_CABLE_NAME} characters long."));
    }
    Ok(name)
}

/// The cable to add for a name the user typed: the name checked and put in
/// its standard form, the lowest free ID and a new serial number. The error
/// says why the cable cannot be added.
pub fn new_cable(typed_name: &str, kind: CableKind, cables: &[Cable]) -> Result<Cable, String> {
    if cables.len() >= MAX_CABLES {
        return Err(format!("A maximum of {MAX_CABLES} virtual cables is supported."));
    }
    let name = validate_cable_name(typed_name, cables)?;
    let id = next_cable_id(cables).ok_or("Every cable ID is in use.")?;
    Ok(Cable { id, serial: new_cable_serial(cables), name, kind })
}

/// Whether two names in their standard form are the same name: letter case
/// does not tell cables apart in Windows Sound.
fn same_cable_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// Checks a name for a new cable and returns it in its standard form. Names
/// must be unique so the cables can be told apart in Windows Sound.
pub fn validate_cable_name(name: &str, cables: &[Cable]) -> Result<String, String> {
    let name = normalize_cable_name(name)?;
    if cables.iter().any(|c| same_cable_name(&c.name, &name)) {
        return Err(format!("There is already a cable called \"{name}\"."));
    }
    Ok(name)
}

/// Returns an integer setting, or `fallback` when it is missing or not an
/// integer. Range checks are left to the caller, which logs them.
pub fn config_int(config: &Config, key: &str, fallback: i64) -> i64 {
    config.document.get(key).and_then(Item::as_integer).unwrap_or(fallback)
}

/// Returns a boolean setting, or `fallback` when it is missing or not a
/// boolean.
pub fn config_bool(config: &Config, key: &str, fallback: bool) -> bool {
    config.document.get(key).and_then(Item::as_bool).unwrap_or(fallback)
}

/// Returns a string setting, or `fallback` when it is missing, empty or not a
/// string.
pub fn config_string(config: &Config, key: &str, fallback: &str) -> String {
    match config.document.get(key).and_then(Item::as_str) {
        Some(value) if !value.is_empty() => value.to_string(),
        _ => fallback.to_string(),
    }
}

/// Parses a boolean command-line value
pub fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// Written when config.toml does not exist yet, so every available setting is
/// visible in the file. There are no cables until the user adds one.
pub fn default_config_content() -> String {
    [
        "# Lemon Mix configuration. Cables are set up in the application, which".to_string(),
        "# rewrites only their tables and the active profile and keeps the rest".to_string(),
        "# of this file as it is. Strips and buses are in the profiles folder.".to_string(),
        String::new(),
        "# Address of the local USB/IP server.".to_string(),
        format!("listen = \"{DEFAULT_LISTEN}\""),
        format!("# Delay between a cable's playback side and the mixer, {MIN_LATENCY_MS}-{MAX_LATENCY_MS} ms."),
        "# Playback that Windows sends later than this plays after a short gap.".to_string(),
        format!("latency_ms = {DEFAULT_LATENCY_MS}"),
        format!("# Audio kept between each physical device and the mixer, {MIN_BUFFER_MS}-{MAX_BUFFER_MS} ms."),
        "# Raise it if hardware inputs or outputs crackle.".to_string(),
        format!("buffer_ms = {DEFAULT_BUFFER_MS}"),
        "# Detailed USB/IP traces in the log, for troubleshooting.".to_string(),
        "diagnostics = false".to_string(),
        "# For troubleshooting: hold all audio transfers this many milliseconds once a".to_string(),
        "# minute, to reproduce a system stall. 0 turns it off.".to_string(),
        "stall_test_ms = 0".to_string(),
    ]
    .iter()
    .map(|line| format!("{line}\n"))
    .collect()
}

/// Replaces the tables under `key` with `tables`, keeping everything else.
fn replace_tables(document: &mut DocumentMut, key: &str, tables: Vec<Table>) {
    document.remove(key);
    if tables.is_empty() {
        return;
    }
    let mut array = ArrayOfTables::new();
    for mut table in tables {
        // A blank line before each table keeps the file easy to read.
        table.decor_mut().set_prefix("\n");
        array.push(table);
    }
    document.insert(key, Item::ArrayOfTables(array));
}

/// Keeps a file written with Windows line endings consistent.
fn keep_line_endings(original: &str, text: String) -> String {
    if original.contains("\r\n") { text.replace("\r\n", "\n").replace('\n', "\r\n") } else { text }
}

fn cable_tables(cables: &[Cable]) -> Vec<Table> {
    cables
        .iter()
        .map(|cable| {
            let mut table = Table::new();
            table.insert("id", value(i64::from(cable.id)));
            table.insert("serial", value(cable.serial.as_str()));
            table.insert("name", value(cable.name.as_str()));
            table.insert("kind", value(cable.kind.name()));
            table
        })
        .collect()
}

/// Replaces the [[cable]] tables in `content` with one per cable and keeps
/// everything else, comments included. Fails when `content` is not valid
/// TOML, so that a file being edited by hand is not overwritten.
pub fn set_cable_entries(content: &str, cables: &[Cable]) -> Result<String, String> {
    let mut document = parse_config(content)?.document;
    replace_tables(&mut document, CABLE_KEY, cable_tables(cables));
    Ok(keep_line_endings(content, document.to_string()))
}

/// Reads config.toml for rewriting: the existing text, or the defaults.
fn current_content() -> String {
    match fs::read(config_file_path()) {
        Ok(existing) => String::from_utf8_lossy(&existing).into_owned(),
        Err(_) => default_config_content(),
    }
}

pub fn save_cables(cables: &[Cable]) -> io::Result<()> {
    fs::create_dir_all(application_data_dir())?;
    let updated = set_cable_entries(&current_content(), cables).map_err(io::Error::other)?;
    fs::write(config_file_path(), updated)
}

/// Writes config.toml with the default settings when no configuration file
/// exists, so every available setting can be edited.
pub fn ensure_config_file(logger: &Logger) {
    if config_paths().iter().any(|path| path.exists()) {
        return;
    }
    let dir = application_data_dir();
    if let Err(err) = fs::create_dir_all(&dir) {
        logf!(logger, "Could not create {}: {err}", dir.display());
        return;
    }
    if let Err(err) = fs::write(config_file_path(), default_config_content()) {
        logf!(logger, "Could not write default configuration: {err}");
        return;
    }
    logf!(logger, "Wrote default configuration to {}", config_file_path().display());
}

/// One strip's saved settings: what the user set, by the strip's ID (`H1`
/// for hardware strip 1, `V3` for cable 3's playback side).
#[derive(Clone, Debug, PartialEq)]
pub struct SavedStrip {
    pub id: String,
    /// What controllers know the strip by; it stays when the ID changes.
    /// Empty until the mixer gives it one.
    pub key: String,
    pub label: String,
    pub params: engine::StripParams,
    /// The buses it feeds, by bus ID (`A1`, `B3`).
    pub routes: Vec<String>,
    /// The strips that duck it while they play, by strip ID (`H1`, `V3`).
    pub duck_by: Vec<String>,
    /// The physical device of a hardware strip: "" for none, "default" for
    /// the system default, or an endpoint ID.
    pub device: String,
    pub exclusive: bool,
}

/// One bus's saved settings, by ID (`A1` for hardware bus 1, `B3` for cable
/// 3's recording side).
#[derive(Clone, Debug, PartialEq)]
pub struct SavedBus {
    pub id: String,
    pub key: String,
    pub label: String,
    pub params: engine::BusParams,
    pub device: String,
    pub exclusive: bool,
}

/// The mixer's saved strips and buses.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SavedMixer {
    pub strips: Vec<SavedStrip>,
    pub buses: Vec<SavedBus>,
}

/// How many hardware strips (`H1`...) and buses (`A1`...) there are: up to
/// the highest numbered [[strip]] and [[bus]] table, since the application
/// writes one for each. With no strip tables at all, as in a new file, there
/// are `DEFAULT_HARDWARE_PORTS`, and likewise for buses; an empty list
/// (`strip = []`) means none.
pub fn hardware_counts(config: &Config) -> (usize, usize) {
    let mixer = config_mixer(config);
    let highest = |ids: &mut dyn Iterator<Item = &String>, letter: char| {
        ids.filter_map(|id| id.strip_prefix(letter)?.parse::<usize>().ok()).max().unwrap_or(0)
    };
    let count = |key: &str, highest: usize| {
        if config.document.contains_key(key) { highest } else { DEFAULT_HARDWARE_PORTS as usize }
    };
    (
        count(STRIP_KEY, highest(&mut mixer.strips.iter().map(|s| &s.id), 'H')),
        count(BUS_KEY, highest(&mut mixer.buses.iter().map(|b| &b.id), 'A')),
    )
}

fn float(table: &Table, key: &str, fallback: f32) -> f32 {
    match table.get(key) {
        Some(item) => {
            item.as_float().map(|f| f as f32).or_else(|| item.as_integer().map(|i| i as f32)).unwrap_or(fallback)
        }
        None => fallback,
    }
}

fn boolean(table: &Table, key: &str) -> bool {
    table.get(key).and_then(Item::as_bool).unwrap_or(false)
}

fn text(table: &Table, key: &str) -> String {
    table.get(key).and_then(Item::as_str).unwrap_or_default().trim().to_string()
}

/// A strip's or bus's key: lowercase letters and digits, or empty.
fn key_text(table: &Table) -> String {
    let key = text(table, "key").to_ascii_lowercase();
    if (1..=32).contains(&key.len()) && key.chars().all(|c| c.is_ascii_alphanumeric()) { key } else { String::new() }
}

/// A new key for a strip or bus, unlike any in `taken`.
pub fn new_key(taken: &[&str]) -> String {
    use std::hash::{BuildHasher, Hasher};
    loop {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        hasher.write_u128(now);
        let key = format!("{:012x}", hasher.finish() & 0xffff_ffff_ffff);
        if !taken.contains(&key.as_str()) {
            return key;
        }
    }
}

/// Whether `id` names a strip or bus: a letter and a number, up to the
/// largest cable ID for a cable's (`V`, `B`), and any for hardware (`H`, `A`).
fn is_valid_port_id(id: &str, letters: &[char]) -> bool {
    let mut chars = id.chars();
    let Some(letter) = chars.next().filter(|c| letters.contains(c)) else {
        return false;
    };
    let digits = chars.as_str();
    let max = if matches!(letter, 'V' | 'B') { MAX_CABLE_ID } else { u32::MAX };
    !digits.starts_with('0') && digits.parse::<u32>().is_ok_and(|n| (1..=max).contains(&n))
}

/// The [[strip]] and [[bus]] tables. Invalid tables, repeated IDs and values
/// out of range are skipped or brought into range.
pub fn config_mixer(config: &Config) -> SavedMixer {
    let mut mixer = SavedMixer::default();
    if let Some(tables) = config.document.get(STRIP_KEY).and_then(Item::as_array_of_tables) {
        for table in tables.iter() {
            let id = text(table, "id").to_ascii_uppercase();
            if !is_valid_port_id(&id, &['H', 'V']) || mixer.strips.iter().any(|s| s.id == id) {
                continue;
            }
            let eq = table
                .get("eq")
                .and_then(Item::as_array)
                .map(|a| {
                    let band = |i: usize| {
                        a.get(i)
                            .and_then(|v| v.as_float().map(|f| f as f32).or_else(|| v.as_integer().map(|n| n as f32)))
                            .unwrap_or(0.0)
                    };
                    [band(0), band(1), band(2)]
                })
                .unwrap_or([0.0; 3]);
            let params = engine::StripParams {
                gain_db: float(table, "gain_db", 0.0),
                mute: boolean(table, "mute"),
                solo: boolean(table, "solo"),
                mono: boolean(table, "mono"),
                pan: float(table, "pan", 0.0),
                denoise: float(table, "denoise", 0.0),
                eq,
                gate: float(table, "gate", 0.0),
                comp: float(table, "comp", 0.0),
                gate_options: {
                    let d = engine::GateOptions::default();
                    engine::GateOptions {
                        range_db: float(table, "gate_range_db", d.range_db),
                        attack_ms: float(table, "gate_attack_ms", d.attack_ms),
                        hold_ms: float(table, "gate_hold_ms", d.hold_ms),
                        release_ms: float(table, "gate_release_ms", d.release_ms),
                    }
                },
                comp_options: {
                    let d = engine::CompOptions::default();
                    engine::CompOptions {
                        attack_ms: float(table, "comp_attack_ms", d.attack_ms),
                        release_ms: float(table, "comp_release_ms", d.release_ms),
                        knee_db: float(table, "comp_knee_db", d.knee_db),
                        makeup_db: table.contains_key("comp_makeup_db").then(|| float(table, "comp_makeup_db", 0.0)),
                        manual: boolean(table, "comp_manual"),
                        threshold_db: float(table, "comp_threshold_db", d.threshold_db),
                        ratio: float(table, "comp_ratio", d.ratio),
                    }
                },
                duck: {
                    let defaults = engine::DuckParams::default();
                    engine::DuckParams {
                        amount_db: float(table, "duck_amount_db", defaults.amount_db),
                        threshold_db: float(table, "duck_threshold_db", defaults.threshold_db),
                        fade_down_ms: float(table, "duck_fade_down_ms", defaults.fade_down_ms),
                        hold_ms: float(table, "duck_hold_ms", defaults.hold_ms),
                        fade_up_ms: float(table, "duck_fade_up_ms", defaults.fade_up_ms),
                    }
                },
            }
            .sanitized();
            let routes = table
                .get("routes")
                .and_then(Item::as_array)
                .map(|a| {
                    let mut routes: Vec<String> = Vec::new();
                    for bus in a.iter().filter_map(|v| v.as_str()).map(|s| s.trim().to_ascii_uppercase()) {
                        if is_valid_port_id(&bus, &['A', 'B']) && !routes.contains(&bus) {
                            routes.push(bus);
                        }
                    }
                    routes
                })
                .unwrap_or_default();
            let duck_by = table
                .get("duck_by")
                .and_then(Item::as_array)
                .map(|a| {
                    let mut sources: Vec<String> = Vec::new();
                    for strip in a.iter().filter_map(|v| v.as_str()).map(|s| s.trim().to_ascii_uppercase()) {
                        if is_valid_port_id(&strip, &['H', 'V']) && strip != id && !sources.contains(&strip) {
                            sources.push(strip);
                        }
                    }
                    sources
                })
                .unwrap_or_default();
            mixer.strips.push(SavedStrip {
                duck_by,
                key: key_text(table),
                label: text(table, "label").chars().take(MAX_CABLE_NAME).collect(),
                device: text(table, "device"),
                exclusive: boolean(table, "exclusive"),
                id,
                params,
                routes,
            });
        }
    }
    if let Some(tables) = config.document.get(BUS_KEY).and_then(Item::as_array_of_tables) {
        for table in tables.iter() {
            let id = text(table, "id").to_ascii_uppercase();
            if !is_valid_port_id(&id, &['A', 'B']) || mixer.buses.iter().any(|b| b.id == id) {
                continue;
            }
            let limiter = table
                .get("limiter")
                .and_then(|v| v.as_float().map(|f| f as f32).or_else(|| v.as_integer().map(|n| n as f32)));
            let params = engine::BusParams {
                gain_db: float(table, "gain_db", 0.0),
                mute: boolean(table, "mute"),
                mono: boolean(table, "mono"),
                limiter,
            }
            .sanitized();
            mixer.buses.push(SavedBus {
                key: key_text(table),
                label: text(table, "label").chars().take(MAX_CABLE_NAME).collect(),
                device: text(table, "device"),
                exclusive: boolean(table, "exclusive"),
                id,
                params,
            });
        }
    }
    mixer
}

/// Rounds for the file: settings are made with sliders, and 0.30000001 reads
/// badly.
fn rounded(v: f32) -> f64 {
    (v as f64 * 100.0).round() / 100.0
}

/// Replaces the [[strip]] and [[bus]] tables in `content`, keeping the rest.
pub fn set_mixer_entries(content: &str, mixer: &SavedMixer) -> Result<String, String> {
    let mut document = parse_config(content)?.document;
    let strips: Vec<Table> = mixer
        .strips
        .iter()
        .map(|s| {
            let mut t = Table::new();
            t.insert("id", value(s.id.as_str()));
            if !s.key.is_empty() {
                t.insert("key", value(s.key.as_str()));
            }
            if !s.label.is_empty() {
                t.insert("label", value(s.label.as_str()));
            }
            if s.id.starts_with('H') {
                t.insert("device", value(s.device.as_str()));
                t.insert("exclusive", value(s.exclusive));
            }
            let p = &s.params;
            t.insert("gain_db", value(rounded(p.gain_db)));
            t.insert("mute", value(p.mute));
            t.insert("solo", value(p.solo));
            t.insert("mono", value(p.mono));
            t.insert("pan", value(rounded(p.pan)));
            let mut eq = Array::new();
            for band in p.eq {
                eq.push(rounded(band));
            }
            t.insert("eq", value(eq));
            t.insert("gate", value(rounded(p.gate)));
            t.insert("comp", value(rounded(p.comp)));
            if p.denoise > 0.0 {
                t.insert("denoise", value(rounded(p.denoise)));
            }
            let mut routes = Array::new();
            for bus in &s.routes {
                routes.push(bus.as_str());
            }
            t.insert("routes", value(routes));
            // The gate's and compressor's options, once changed.
            if p.gate_options != engine::GateOptions::default() {
                let g = &p.gate_options;
                t.insert("gate_range_db", value(rounded(g.range_db)));
                t.insert("gate_attack_ms", value(rounded(g.attack_ms)));
                t.insert("gate_hold_ms", value(rounded(g.hold_ms)));
                t.insert("gate_release_ms", value(rounded(g.release_ms)));
            }
            if p.comp_options != engine::CompOptions::default() {
                let c = &p.comp_options;
                t.insert("comp_attack_ms", value(rounded(c.attack_ms)));
                t.insert("comp_release_ms", value(rounded(c.release_ms)));
                t.insert("comp_knee_db", value(rounded(c.knee_db)));
                if let Some(makeup) = c.makeup_db {
                    t.insert("comp_makeup_db", value(rounded(makeup)));
                }
                t.insert("comp_manual", value(c.manual));
                t.insert("comp_threshold_db", value(rounded(c.threshold_db)));
                t.insert("comp_ratio", value(rounded(c.ratio)));
            }
            // Ducking, once the strip has sources or settings of its own.
            if !s.duck_by.is_empty() || p.duck != engine::DuckParams::default() {
                let mut sources = Array::new();
                for strip in &s.duck_by {
                    sources.push(strip.as_str());
                }
                t.insert("duck_by", value(sources));
                let d = &p.duck;
                t.insert("duck_amount_db", value(rounded(d.amount_db)));
                t.insert("duck_threshold_db", value(rounded(d.threshold_db)));
                t.insert("duck_fade_down_ms", value(rounded(d.fade_down_ms)));
                t.insert("duck_hold_ms", value(rounded(d.hold_ms)));
                t.insert("duck_fade_up_ms", value(rounded(d.fade_up_ms)));
            }
            t
        })
        .collect();
    let buses: Vec<Table> = mixer
        .buses
        .iter()
        .map(|b| {
            let mut t = Table::new();
            t.insert("id", value(b.id.as_str()));
            if !b.key.is_empty() {
                t.insert("key", value(b.key.as_str()));
            }
            if !b.label.is_empty() {
                t.insert("label", value(b.label.as_str()));
            }
            if b.id.starts_with('A') {
                t.insert("device", value(b.device.as_str()));
                t.insert("exclusive", value(b.exclusive));
            }
            t.insert("gain_db", value(rounded(b.params.gain_db)));
            t.insert("mute", value(b.params.mute));
            t.insert("mono", value(b.params.mono));
            if let Some(ceiling) = b.params.limiter {
                t.insert("limiter", value(rounded(ceiling)));
            }
            t
        })
        .collect();
    // The tables are how many hardware strips and buses there are, so none
    // is written as an empty list, which a new file does not have.
    for (key, tables) in [(STRIP_KEY, strips), (BUS_KEY, buses)] {
        if tables.is_empty() {
            document.remove(key);
            document.insert(key, value(Array::new()));
        } else {
            replace_tables(&mut document, key, tables);
        }
    }
    Ok(keep_line_endings(content, document.to_string()))
}

// ---- Profiles ----------------------------------------------------------------
//
// The strips' and buses' settings live in profiles: a file each in the profiles
// folder beside config.toml, with [[strip]] and [[bus]] tables. config.toml
// names the active profile as `profile`; the mixer saves its changes straight
// into that profile's file.

/// The profile in use until another is made.
pub const DEFAULT_PROFILE: &str = "Default";
const PROFILE_KEY: &str = "profile";

pub fn profiles_dir() -> PathBuf {
    application_data_dir().join("profiles")
}

fn profile_path(name: &str) -> PathBuf {
    profiles_dir().join(format!("{name}.toml"))
}

/// The name of the active profile, from config.toml.
pub fn config_profile(config: &Config) -> String {
    config_string(config, PROFILE_KEY, DEFAULT_PROFILE)
}

/// Puts a profile name in its standard form: trimmed, with each run of spaces
/// reduced to one; letters, digits, spaces, hyphens, underscores and
/// parentheses only, at most 64 characters, and unlike every name in `taken`
/// ignoring case.
pub fn validate_profile_name(name: &str, taken: &[String]) -> Result<String, String> {
    let name = name.split(' ').filter(|word| !word.is_empty()).collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        return Err("Enter a name for the profile.".into());
    }
    if !name.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '(' | ')')) {
        return Err(
            "Profile names can contain only letters, digits, spaces, hyphens, underscores and parentheses.".into()
        );
    }
    if name.chars().count() > MAX_CABLE_NAME {
        return Err(format!("Profile names can be at most {MAX_CABLE_NAME} characters long."));
    }
    if taken.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
        return Err(format!("There is already a profile named \"{name}\"."));
    }
    Ok(name)
}

/// Every profile: the files in the profiles folder and the active one,
/// which has no file until another profile is switched to.
pub fn list_profiles(active: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(profiles_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let stem = name.strip_suffix(".toml")?;
            validate_profile_name(stem, &[]).ok().filter(|valid| valid == stem)
        })
        .collect();
    if !names.iter().any(|n| n.eq_ignore_ascii_case(active)) {
        names.push(active.to_string());
    }
    names.sort_by_key(|n| n.to_lowercase());
    names
}

/// Writes `mixer`'s strips and buses to a profile's file. A file that is not
/// valid TOML (being edited by hand, say) is not overwritten.
pub fn save_profile(name: &str, mixer: &SavedMixer) -> io::Result<()> {
    fs::create_dir_all(profiles_dir())?;
    let path = profile_path(name);
    let existing = fs::read(&path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()).unwrap_or_default();
    let updated = set_mixer_entries(&existing, mixer).map_err(|err| {
        io::Error::other(format!("{} is not valid TOML, so it was left as it is: {err}", path.display()))
    })?;
    fs::write(path, updated.trim_start())
}

/// Copies the profiles in the settings folder `from` to the one `to`, beside
/// config.toml, replacing those of the same name. Read and written rather
/// than copied, so the copies take the target folder's permissions. Returns
/// how many were copied.
pub fn copy_profiles(from: &std::path::Path, to: &std::path::Path) -> io::Result<usize> {
    let (source, target) = (from.join("profiles"), to.join("profiles"));
    let Ok(entries) = fs::read_dir(&source) else {
        return Ok(0);
    };
    fs::create_dir_all(&target)?;
    let mut copied = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "toml") && path.is_file() {
            fs::write(target.join(entry.file_name()), fs::read(&path)?)?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// The active profile's strips and buses, and how many hardware strips and
/// buses it has. A profile with no file yet, as on first start, is a new
/// configuration: five hardware inputs and outputs with default settings.
pub fn load_active_profile(name: &str) -> Result<(SavedMixer, (usize, usize)), String> {
    if !profile_path(name).exists() {
        let ports = DEFAULT_HARDWARE_PORTS as usize;
        return Ok((SavedMixer::default(), (ports, ports)));
    }
    load_profile(name)
}

/// A profile's strips and buses, and how many hardware strips and buses it has.
pub fn load_profile(name: &str) -> Result<(SavedMixer, (usize, usize)), String> {
    let content =
        fs::read_to_string(profile_path(name)).map_err(|_| format!("The profile \"{name}\" was not found."))?;
    let config = parse_config(&content).map_err(|err| format!("The profile \"{name}\" could not be read: {err}"))?;
    Ok((config_mixer(&config), hardware_counts(&config)))
}

pub fn delete_profile(name: &str) -> io::Result<()> {
    fs::remove_file(profile_path(name))
}

/// Records the active profile in config.toml, keeping everything else.
pub fn save_active_profile(name: &str) -> io::Result<()> {
    fs::create_dir_all(application_data_dir())?;
    let content = current_content();
    let mut document = parse_config(&content).map_err(io::Error::other)?.document;
    match document.get_mut(PROFILE_KEY).and_then(Item::as_value_mut) {
        Some(existing) => {
            let decor = existing.decor().clone();
            *existing = name.into();
            *existing.decor_mut() = decor;
        }
        None => {
            document.insert(PROFILE_KEY, value(name));
        }
    }
    fs::write(config_file_path(), keep_line_endings(&content, document.to_string()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that change LOCALAPPDATA.
    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Points LOCALAPPDATA at a fresh temporary folder for the test.
    pub(crate) fn temp_app_data(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("lemonmix-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // SAFETY: callers hold ENV_LOCK, so no other test reads the environment concurrently.
        unsafe { env::set_var("LOCALAPPDATA", &dir) };
        dir
    }

    fn cable(id: u32, serial: &str, name: &str) -> Cable {
        Cable { id, serial: serial.into(), name: name.into(), kind: CableKind::Loopback }
    }

    fn cables_of(content: &str) -> Vec<Cable> {
        config_cables(&parse_config(content).unwrap())
    }

    /// One [[cable]] table, as a user might write it.
    fn cable_table(id: &str, serial: &str, name: &str) -> String {
        format!("[[cable]]\nid = {id}\nserial = {serial}\nname = {name}\n\n")
    }

    #[test]
    fn reads_typed_settings() {
        let config = parse_config(
            "# comment\nlatency_ms = 30\nstall_test_ms = 250\ndiagnostics = true\nlisten = \"0.0.0.0:4000\"\n",
        )
        .unwrap();
        assert_eq!(config_int(&config, "latency_ms", 40), 30);
        assert_eq!(config_int(&config, "stall_test_ms", 1), 250);
        assert!(config_bool(&config, "diagnostics", false));
        assert_eq!(config_string(&config, "listen", DEFAULT_LISTEN), "0.0.0.0:4000");

        let config =
            parse_config("stall_test_ms = \"250\"\ndiagnostics = 1\nlisten = 3240\nlatency_ms = 1.5\n").unwrap();
        assert_eq!(config_int(&config, "stall_test_ms", 99), 99);
        assert!(!config_bool(&config, "diagnostics", false));
        assert_eq!(config_string(&config, "listen", DEFAULT_LISTEN), DEFAULT_LISTEN);
        assert_eq!(config_int(&config, "latency_ms", 40), 40);
        assert_eq!(config_string(&parse_config("listen = \"\"").unwrap(), "listen", DEFAULT_LISTEN), DEFAULT_LISTEN);
        assert_eq!(config_int(&Config::default(), "missing", 7), 7);
    }

    #[test]
    fn invalid_toml_is_reported_and_not_overwritten() {
        assert!(parse_config("listen = \n").is_err());
        assert!(parse_config("CABLES=3 x\n").is_err());
        assert!(set_cable_entries("[[cable]\n", &[Cable::numbered(1)]).is_err());
        assert!(set_mixer_entries("[[strip]\n", &SavedMixer::default()).is_err());
    }

    #[test]
    fn command_line_booleans_parse_as_expected() {
        for (text, expected) in [("1", true), ("t", true), ("TRUE", true), ("0", false), ("False", false)] {
            assert_eq!(parse_bool(text), Some(expected), "{text}");
        }
        assert_eq!(parse_bool("maybe"), None);
    }

    #[test]
    fn ensure_config_file_writes_defaults() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        temp_app_data("config");
        let logger = Logger::discard();
        ensure_config_file(&logger);
        let written = fs::read_to_string(config_file_path()).unwrap();
        assert_eq!(written, default_config_content());
        let config = parse_config(&written).unwrap();
        assert_eq!(config_string(&config, "listen", ""), DEFAULT_LISTEN);
        assert_eq!(config_int(&config, "latency_ms", 0), DEFAULT_LATENCY_MS);
        assert_eq!(config_int(&config, "buffer_ms", 0), DEFAULT_BUFFER_MS);
        assert_eq!(hardware_counts(&config), (DEFAULT_HARDWARE_PORTS as usize, DEFAULT_HARDWARE_PORTS as usize));
        assert_eq!(config_int(&config, "stall_test_ms", 1), 0);
        assert!(!config_bool(&config, "diagnostics", true));
        assert_eq!(config_cables(&config), []);

        fs::write(config_file_path(), "listen = \"x\"\n").unwrap();
        ensure_config_file(&logger);
        assert_eq!(fs::read_to_string(config_file_path()).unwrap(), "listen = \"x\"\n");

        // Saving the cables keeps the comments and the other settings, and the
        // mixer goes to the active profile's file.
        let mine = "# mine\nlisten = \"127.0.0.1:4000\" # my port\nlatency_ms = 30\n\n";
        fs::write(config_file_path(), format!("{mine}{}", cable_table("1", "\"VCABLE-001\"", "\"Virtual Cable 01\"")))
            .unwrap();
        let cables = [Cable::numbered(1), Cable { kind: CableKind::Output, ..cable(3, "8F3A2C1D", "Game") }];
        save_cables(&cables).unwrap();
        let mixer = SavedMixer {
            strips: vec![SavedStrip {
                id: "H1".into(),
                key: "a1b2c3d4e5f6".into(),
                label: "Mic".into(),
                params: engine::StripParams { gain_db: -3.5, eq: [1.0, 0.0, -2.0], ..Default::default() },
                routes: vec!["A1".into(), "B3".into()],
                duck_by: vec!["V3".into()],
                device: "default".into(),
                exclusive: false,
            }],
            buses: vec![SavedBus {
                id: "A1".into(),
                key: String::new(),
                label: String::new(),
                params: engine::BusParams { limiter: Some(-1.0), ..Default::default() },
                device: "{0.0.0.00000000}.{abc}".into(),
                exclusive: true,
            }],
        };
        save_profile("Default", &mixer).unwrap();
        let saved = fs::read_to_string(config_file_path()).unwrap();
        assert!(saved.starts_with(mine), "{saved:?}");
        assert!(
            saved.contains("[[cable]]\nid = 3\nserial = \"8F3A2C1D\"\nname = \"Game\"\nkind = \"output\"\n"),
            "{saved:?}"
        );
        assert!(!saved.contains("[[strip]]") && !saved.contains("strip ="), "{saved:?}");
        assert_eq!(config_cables(&parse_config(&saved).unwrap()), cables);
        let profile = fs::read_to_string(profile_path("Default")).unwrap();
        let profile_config = parse_config(&profile).unwrap();
        assert_eq!(config_mixer(&profile_config), mixer);
        assert_eq!(load_active_profile("Default").unwrap(), (mixer.clone(), (1, 1)));
        assert_eq!(load_active_profile("Nothing yet").unwrap(), (SavedMixer::default(), (5, 5)), "a new profile");
        // A profile file being edited by hand, and broken, is left alone.
        fs::write(profile_path("Default"), "[[strip]\n").unwrap();
        assert!(save_profile("Default", &mixer).is_err());
        assert_eq!(fs::read_to_string(profile_path("Default")).unwrap(), "[[strip]\n");
    }

    /// The tables are the count of hardware strips and buses: none is saved
    /// as an empty list, which reads back as none rather than a new file's
    /// defaults.
    #[test]
    fn hardware_counts_come_from_the_tables() {
        let content = "# mine\nlisten = \"x\"\n\n[[cable]]\nid = 1\n";
        let saved = set_mixer_entries(content, &SavedMixer::default()).unwrap();
        assert!(saved.starts_with("# mine\nlisten = \"x\"\n"), "{saved:?}");
        let config = parse_config(&saved).unwrap();
        assert_eq!(hardware_counts(&config), (0, 0), "{saved:?}");
        assert!(config.document.get("cable").is_some_and(|c| c.is_array_of_tables()), "{saved:?}");
        assert_eq!(hardware_counts(&parse_config("").unwrap()), (5, 5), "a new file");

        let strip = |id: &str| SavedStrip {
            id: id.into(),
            key: String::new(),
            label: String::new(),
            params: Default::default(),
            routes: Vec::new(),
            duck_by: Vec::new(),
            device: String::new(),
            exclusive: false,
        };
        let mixer = SavedMixer { strips: (1..=300).map(|n| strip(&format!("H{n}"))).collect(), buses: Vec::new() };
        let config = parse_config(&set_mixer_entries("", &mixer).unwrap()).unwrap();
        assert_eq!(hardware_counts(&config), (300, 0), "any number of them");

        let strips = "[[strip]]\nid = \"H300\"\n\n[[strip]]\nid = \"V300\"\n\n[[strip]]\nid = \"H01\"\n";
        let ids: Vec<String> = config_mixer(&parse_config(strips).unwrap()).strips.into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["H300"], "hardware strips have no maximum; cable IDs stop at {MAX_CABLE_ID}");
    }

    /// Ducking is read with its sources tidied and its values in range, and
    /// written only once a strip has sources or settings of its own.
    #[test]
    fn ducking_is_read_and_written_with_its_strip() {
        let content = "[[strip]]
id = \"H1\"
duck_by = [\"h2\", \"H1\", \"X9\", \"V3\", \"H2\"]
\n                       duck_amount_db = 20
duck_fade_up_ms = 1e9
";
        let strip = config_mixer(&parse_config(content).unwrap()).strips.remove(0);
        assert_eq!(strip.duck_by, ["H2", "V3"], "not itself, not twice, only strips");
        let d = strip.params.duck;
        assert_eq!((d.amount_db, d.fade_up_ms, d.threshold_db), (20.0, 10_000.0, -40.0), "defaults fill the rest");

        let plain = SavedStrip {
            id: "H1".into(),
            key: String::new(),
            label: String::new(),
            params: Default::default(),
            routes: Vec::new(),
            duck_by: Vec::new(),
            device: String::new(),
            exclusive: false,
        };
        let written = |strip: &SavedStrip| {
            set_mixer_entries("", &SavedMixer { strips: vec![strip.clone()], buses: Vec::new() }).unwrap()
        };
        assert!(!written(&plain).contains("duck"), "{}", written(&plain));
        let mut tuned = plain.clone();
        tuned.params.duck.amount_db = 6.0;
        let text = written(&tuned);
        assert!(text.contains("duck_by = []") && text.contains("duck_amount_db = 6.0"), "{text}");
        assert_eq!(config_mixer(&parse_config(&text).unwrap()).strips[0].params.duck.amount_db, 6.0);
    }

    /// The gate's and compressor's options round-trip, and are written only
    /// once changed.
    #[test]
    fn processing_options_are_read_and_written_with_their_strip() {
        let mut strip = SavedStrip {
            id: "H1".into(),
            key: String::new(),
            label: String::new(),
            params: Default::default(),
            routes: Vec::new(),
            duck_by: Vec::new(),
            device: String::new(),
            exclusive: false,
        };
        let round_trip = |strip: &SavedStrip| {
            let text = set_mixer_entries("", &SavedMixer { strips: vec![strip.clone()], buses: Vec::new() }).unwrap();
            (text.clone(), config_mixer(&parse_config(&text).unwrap()).strips.remove(0).params)
        };
        let (text, _) = round_trip(&strip);
        assert!(!text.contains("gate_") && !text.contains("comp_") && !text.contains("denoise"), "{text}");
        strip.params.denoise = 6.5;
        let (text, params) = round_trip(&strip);
        assert!(text.contains("denoise = 6.5"), "{text}");
        assert_eq!(params.denoise, 6.5);
        strip.params.denoise = 0.0;
        strip.params.gate_options = engine::GateOptions { range_db: 30.0, hold_ms: 200.0, ..Default::default() };
        strip.params.comp_options = engine::CompOptions {
            manual: true,
            threshold_db: -24.0,
            ratio: 6.0,
            makeup_db: Some(3.0),
            ..Default::default()
        };
        let (text, params) = round_trip(&strip);
        assert!(text.contains("comp_manual = true") && text.contains("gate_range_db = 30.0"), "{text}");
        assert_eq!((params.gate_options, params.comp_options), (strip.params.gate_options, strip.params.comp_options));
        // Automatic make-up is written as no make-up value.
        strip.params.comp_options.makeup_db = None;
        let (text, params) = round_trip(&strip);
        assert!(!text.contains("comp_makeup_db"), "{text}");
        assert_eq!(params.comp_options.makeup_db, None);
    }

    #[test]
    fn default_config_has_no_cables() {
        assert_eq!(cables_of(&default_config_content()), []);
        assert_eq!(cables_of(""), []);
    }

    #[test]
    fn reads_cables_in_file_order() {
        let content = [
            cable_table("4", "\"8f3a2c1d\"", "\" Discord \""),
            cable_table("2", "\"0000BEEF\"", "\"Game   audio  2\""),
            cable_table("4", "\"11111111\"", "\"Repeated ID\""),
            cable_table("5", "\"8F3A2C1D\"", "\"Repeated serial\""),
            cable_table("0", "\"22222222\"", "\"Zero\""),
            cable_table("256", "\"33333333\"", "\"Too big\""),
            cable_table("\"7\"", "\"44444444\"", "\"ID as text\""),
            cable_table("8", "\"bad serial!\"", "\"Bad serial\""),
            cable_table("9", "\"55555555\"", "\"Game=Audio, live\""),
            cable_table("10", "\"66666666\"", "\"\""),
            "[[cable]]\nid = 11\nname = \"No serial\"\n\n".to_string(),
            "[[cable]]\nid = 12\nserial = \"77777777\"\nname = \"Bad kind\"\nkind = \"both\"\n\n".to_string(),
            "[[cable]]\nid = 13\nserial = \"88888888\"\nname = \"Mic\"\nkind = \"Input\"\n".to_string(),
        ]
        .concat();
        assert_eq!(
            cables_of(&content),
            [
                cable(4, "8F3A2C1D", "Discord"),
                cable(2, "0000BEEF", "Game audio 2"),
                Cable { kind: CableKind::Input, ..cable(13, "88888888", "Mic") }
            ]
        );
        let many: String =
            (1..=40).map(|id| cable_table(&id.to_string(), &format!("\"S{id}\""), &format!("\"C{id}\""))).collect();
        assert_eq!(cables_of(&many).len(), MAX_CABLES);
    }

    #[test]
    fn repeated_names_in_the_config_are_skipped() {
        let content = [
            cable_table("1", "\"AAAA0001\"", "\"Stream Mix 2\""),
            cable_table("2", "\"AAAA0002\"", "\"  stream   MIX  2 \""),
            cable_table("3", "\"AAAA0003\"", "\"STREAM MIX 2\""),
            cable_table("4", "\"AAAA0004\"", "\"Stream Mix 3\""),
        ]
        .concat();
        assert_eq!(cables_of(&content), [cable(1, "AAAA0001", "Stream Mix 2"), cable(4, "AAAA0004", "Stream Mix 3")]);
    }

    #[test]
    fn set_cable_entries_replaces_all_cable_tables() {
        let settings = "# c\nlisten = \"x\" # kept\n\n";
        let content =
            format!("{settings}{}{}", cable_table("1", "\"A1\"", "\"A\""), cable_table("2", "\"B2\"", "\"B\""));
        let cables = [cable(2, "B2", "B"), cable(5, "E5", "E")];

        let updated = set_cable_entries(&content, &cables).unwrap();
        assert!(updated.starts_with(settings), "{updated:?}");
        assert_eq!(cables_of(&updated), cables);

        let none = set_cable_entries(&content, &[]).unwrap();
        assert!(none.starts_with(settings) && !none.contains("[[cable]]"), "removing the last cable: {none:?}");
        assert_eq!(set_cable_entries("", &[]).unwrap(), "");
        assert_eq!(cables_of(&set_cable_entries("", &cables).unwrap()), cables);

        let windows = set_cable_entries("listen = \"x\"\r\n", &cables).unwrap();
        assert!(!windows.replace("\r\n", "").contains('\n'), "{windows:?}");
        assert_eq!(cables_of(&windows), cables);
    }

    #[test]
    fn mixer_tables_are_validated() {
        let content = "[[strip]]\nid = \"h1\"\ngain_db = 99\npan = -0.5\neq = [20, 1]\nroutes = [\"a1\", \"A1\", \"C2\", \"b4\"]\n\n\
                       [[strip]]\nid = \"H1\"\nmute = true\n\n[[strip]]\nid = \"X1\"\n\n[[strip]]\nid = \"V300\"\n\n\
                       [[bus]]\nid = \"B4\"\nlimiter = 6\nmono = true\n\n[[bus]]\nid = \"H2\"\n";
        let mixer = config_mixer(&parse_config(content).unwrap());
        assert_eq!(mixer.strips.len(), 1, "repeated, unknown and out-of-range IDs are skipped");
        let strip = &mixer.strips[0];
        assert_eq!((strip.id.as_str(), strip.params.gain_db, strip.params.pan), ("H1", 12.0, -0.5));
        assert_eq!(strip.params.eq, [12.0, 1.0, 0.0]);
        assert_eq!(strip.routes, ["A1", "B4"]);
        assert_eq!(mixer.buses.len(), 1);
        assert_eq!((mixer.buses[0].params.limiter, mixer.buses[0].params.mono), (Some(0.0), true));
    }

    #[test]
    fn next_cable_id_takes_the_lowest_free_id() {
        assert_eq!(next_cable_id(&[]), Some(1));
        assert_eq!(next_cable_id(&[Cable::numbered(3), Cable::numbered(1)]), Some(2));
        let full: Vec<Cable> = (1..=MAX_CABLE_ID).map(Cable::numbered).collect();
        assert_eq!(next_cable_id(&full), None);
    }

    #[test]
    fn a_new_cable_is_saved_with_its_name_in_standard_form() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        temp_app_data("new-cable");
        let existing = [cable(1, "AAAA0001", "Game")];

        let added = new_cable("  Stream   Mix  2 ", CableKind::Input, &existing).unwrap();
        assert_eq!((added.id, added.name.as_str(), added.kind), (2, "Stream Mix 2", CableKind::Input));
        assert!(is_valid_serial(&added.serial) && added.serial != "AAAA0001");

        let cables = [existing[0].clone(), added.clone()];
        save_cables(&cables).unwrap();
        let saved = fs::read_to_string(config_file_path()).unwrap();
        let table =
            format!("[[cable]]\nid = 2\nserial = \"{}\"\nname = \"Stream Mix 2\"\nkind = \"input\"\n", added.serial);
        assert!(saved.contains(&table), "{saved:?}");
        assert_eq!(cables_of(&saved), cables, "the saved cables read back unchanged");
    }

    #[test]
    fn new_cables_are_refused_for_bad_names_or_when_full() {
        let existing = [cable(1, "AAAA0001", "Game")];
        let kind = CableKind::Loopback;
        assert_eq!(new_cable(" game ", kind, &existing), Err("There is already a cable called \"game\".".into()));
        assert!(new_cable("Game, live", kind, &existing).is_err());
        assert!(new_cable("   ", kind, &existing).is_err());
        let full: Vec<Cable> = (1..=MAX_CABLES as u32).map(Cable::numbered).collect();
        assert_eq!(
            new_cable("Another", kind, &full),
            Err(format!("A maximum of {MAX_CABLES} virtual cables is supported."))
        );
    }

    #[test]
    fn new_serials_are_valid_and_unique() {
        let mut cables = Vec::new();
        for id in 1..=MAX_CABLES as u32 {
            let serial = new_cable_serial(&cables);
            assert!(serial.len() == 8 && serial.bytes().all(|b| b.is_ascii_hexdigit()), "{serial}");
            assert!(is_valid_serial(&serial));
            cables.push(cable(id, &serial, "x"));
        }
        let mut serials: Vec<&str> = cables.iter().map(|c| c.serial.as_str()).collect();
        serials.sort();
        serials.dedup();
        assert_eq!(serials.len(), MAX_CABLES);
    }

    #[test]
    fn cable_names_are_checked() {
        let cables = [cable(1, "A1", "Game")];
        assert_eq!(validate_cable_name("  Music ", &cables), Ok("Music".into()));
        assert_eq!(validate_cable_name(" Stream   Mix  2 ", &cables), Ok("Stream Mix 2".into()));
        assert_eq!(validate_cable_name("Música", &cables), Ok("Música".into()), "letters beyond ASCII");
        assert!(validate_cable_name("", &cables).is_err());
        assert!(validate_cable_name("   ", &cables).is_err());
        assert!(validate_cable_name("game", &cables).is_err());
        assert!(validate_cable_name("GAME", &cables).is_err());
        assert_eq!(validate_cable_name("Game-audio_mix (2)", &cables), Ok("Game-audio_mix (2)".into()));
        for bad in ["Game, live", "Game/2", "Game\taudio", "Game [2]", "A=B", "A.B"] {
            assert!(validate_cable_name(bad, &cables).is_err(), "{bad:?}");
        }
        assert!(validate_cable_name(&"x".repeat(MAX_CABLE_NAME + 1), &cables).is_err());
        assert!(validate_cable_name(&"x".repeat(MAX_CABLE_NAME), &cables).is_ok());
    }

    #[test]
    fn names_differing_only_in_spacing_or_case_are_duplicates() {
        let cables = [cable(1, "A1", "Stream Mix 2"), cable(2, "B2", "Música")];
        for same in ["Stream Mix 2", "  Stream   Mix  2 ", "stream MIX 2", " STREAM  mix   2", "MÚSICA", " música "] {
            assert!(validate_cable_name(same, &cables).is_err(), "{same:?} was not a duplicate");
        }
        assert_eq!(
            validate_cable_name("  stream   mix 2 ", &cables),
            Err("There is already a cable called \"stream mix 2\".".into())
        );
        for different in ["Stream Mix 3", "StreamMix 2", "Stream Mix 2 (B)", "Stream-Mix 2"] {
            assert!(validate_cable_name(different, &cables).is_ok(), "{different:?} was taken for a duplicate");
        }
    }
}
