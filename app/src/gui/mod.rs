//! The window: a header with where the mixer runs and its status, the input
//! strips and output buses with their faders, meters and routing, and the
//! virtual devices (cables). The mixer runs in this process (on demand) or
//! in the Windows service, which the window then manages through its pipe.
//! Closing the window leaves the audio running: in the service, or here in
//! the notification area until Quit.

mod link;
mod shell;
mod widgets;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::{Duration, Instant};

use cables::logf;
use cables::win::wide;
use eframe::egui::{self, Align, Color32, Layout, RichText, Vec2};
use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MessageBoxW, SW_HIDE, SW_SHOW, SetForegroundWindow, ShowWindow,
};

use self::link::{Link, Mode, SharedState, lock, new_shared};
use self::shell::{call_service_within, run_job};
use crate::mixer::MixerHost;
use crate::platform::control;
use crate::platform::service::ServiceState;
use crate::platform::startup::{self, StartupState};
use crate::protocol::{
    AppView, BusView, CableView, DeviceView, Meter, MixerCommand, Request, StripParams, StripView, Tone,
};

const OK_COLOR: Color32 = Color32::from_rgb(64, 180, 80);
const PENDING_COLOR: Color32 = Color32::from_rgb(230, 160, 30);
const ERROR_COLOR: Color32 = Color32::from_rgb(220, 70, 60);
const LEMON: Color32 = Color32::from_rgb(247, 210, 62);
/// Ducking: the Duck button while a strip has sources, and its border while it
/// is being lowered.
const DUCK_COLOR: Color32 = Color32::from_rgb(150, 100, 230);
/// A strip counts as ducked once lowered by more than this (about 0.25 dB).
const DUCKED_BELOW: f32 = 0.97;

/// The colour of a strip's border: red while muted, purple while ducked,
/// yellow while soloed, and none otherwise. A muted strip is silent whatever
/// else applies; ducking is shown over solo as it changes as you listen.
fn strip_highlight(params: &StripParams, meter: &Meter) -> Option<Color32> {
    if params.mute {
        Some(ERROR_COLOR)
    } else if meter.duck < DUCKED_BELOW {
        Some(DUCK_COLOR)
    } else if params.solo {
        Some(PENDING_COLOR)
    } else {
        None
    }
}
/// How long a control the user just changed keeps its own value rather than
/// the mixer's, so a fader being dragged does not jump back to an older one.
const EDIT_HOLD: Duration = Duration::from_millis(600);

fn tone_color(tone: Tone) -> Color32 {
    match tone {
        Tone::Ok => OK_COLOR,
        Tone::Pending => PENDING_COLOR,
        Tone::Error => ERROR_COLOR,
    }
}

/// The window's handle, for the tray icon to show and hide it.
static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// Set by the tray's Quit, read by the window.
static QUIT: AtomicBool = AtomicBool::new(false);
/// Set when the window is shown from outside it (the tray, a second start), so
/// the next frame tells the window system too.
static SHOWN: AtomicBool = AtomicBool::new(false);

fn show_window() {
    let hwnd = WINDOW.load(Ordering::SeqCst);
    if hwnd != 0 {
        // SAFETY: shows and focuses this process's own window.
        unsafe {
            ShowWindow(hwnd as _, SW_SHOW);
            SetForegroundWindow(hwnd as _);
        }
    }
    SHOWN.store(true, Ordering::SeqCst);
    link::repaint();
}

fn hide_window() {
    let hwnd = WINDOW.load(Ordering::SeqCst);
    if hwnd != 0 {
        // SAFETY: hides this process's own window.
        unsafe { ShowWindow(hwnd as _, SW_HIDE) };
    }
}

pub fn run_gui(host: &Arc<MixerHost>, minimized: bool) {
    control::init(host);
    control::on_change(link::repaint);
    let logger = control::logger();
    // A running service has the mixer; otherwise this window runs it. One
    // still starting, as at sign-in, is given the time to.
    let state = crate::platform::service::service_state();
    let service = call_service_within(if state == ServiceState::Pending { 15 } else { 0 });
    let mode = if service.is_ok() { Mode::Service } else { Mode::OnDemand };
    let shared = new_shared(mode);
    match &service {
        Ok(reply) => {
            let mut s = lock(&shared);
            s.snapshot = reply.snapshot.clone();
            s.version = reply.version.clone();
            logf!(logger, "Managing the Windows service, version {}", reply.version);
        }
        Err(_) => logf!(logger, "Running on demand"),
    }
    // SAFETY: sets this process's taskbar identity with a NUL-terminated string.
    unsafe { SetCurrentProcessExplicitAppUserModelID(wide("LemonMix.LemonMix").as_ptr()) };
    // A second start asks for the window.
    startup::on_show_request(show_window);
    // Starting with Windows follows this copy wherever it now is.
    match startup::refresh_path() {
        Ok(true) => logf!(logger, "Start with Windows now opens this copy of Lemon Mix"),
        Ok(false) => {}
        Err(err) => logf!(logger, "Could not update the start-with-Windows entry: {err}"),
    }

    let icon = egui::IconData { rgba: crate::icon::lemon_rgba(64), width: 64, height: 64 };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Lemon Mix")
            .with_inner_size([1180.0, 880.0])
            .with_min_inner_size([720.0, 480.0])
            .with_icon(Arc::new(icon)),
        ..Default::default()
    };
    let result =
        eframe::run_native("Lemon Mix", options, Box::new(move |cc| Ok(Box::new(App::new(cc, shared, minimized)))));
    if let Err(err) = result {
        logf!(logger, "The window could not open: {err}");
        show_fatal_dialog(&format!(
            "The Lemon Mix window could not open:\n\n{err}\n\nDiagnostic log:\n{}",
            control::log_path()
        ));
    }
}

pub fn show_fatal_dialog(message: &str) {
    // SAFETY: a message box with NUL-terminated strings.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide(message).as_ptr(),
            wide("Lemon Mix - Startup Error").as_ptr(),
            MB_ICONERROR,
        );
    }
}

/// A question the window is asking.
enum Question {
    /// At startup: the service is installed but stopped.
    StartServiceAtStartup,
    InstallService,
    StartService,
    UninstallService,
    RemoveCable(CableView),
    /// A hardware strip, by ID and label.
    RemoveStrip(String, String),
    /// A hardware bus, by ID and label.
    RemoveBus(String, String),
    /// A profile, by name.
    DeleteProfile(String),
}

struct App {
    shared: SharedState,
    link: Link,
    /// The snapshot generation the local copies were taken from.
    generation: u64,
    strips: Vec<StripView>,
    buses: Vec<BusView>,
    cables: Vec<CableView>,
    capture_devices: Vec<DeviceView>,
    render_devices: Vec<DeviceView>,
    apps: Vec<AppView>,
    /// When the user last changed each strip or bus, by ID.
    edited: HashMap<String, Instant>,
    /// Meter levels as shown, with their fall-back.
    meter_display: HashMap<String, [f32; 2]>,
    last_frame: Instant,
    question: Option<Question>,
    new_cable_name: String,
    new_cable_kind: String,
    renaming: Option<(String, String)>,
    /// The profiles dialog, while it is open.
    profile_dialog: Option<ProfileDialog>,
    /// The strip whose ducking dialog is open, by its key: an ID shifts when
    /// an earlier strip is removed, and the dialog must stay on its strip.
    duck_dialog: Option<String>,
    /// The strip (by key) and processor whose options dialog is open.
    processing_dialog: Option<(String, Processor)>,
    /// Whether an edit hold was on at the last frame, to see it end.
    holding: bool,
    editing_label: Option<(String, String)>,
    _tray: Option<tray_icon::TrayIcon>,
    /// Started with Windows (`--minimized`): the first frame hides the window.
    start_hidden: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, shared: SharedState, start_hidden: bool) -> App {
        link::set_context(&cc.egui_ctx);
        let mut visuals = egui::Visuals::dark();
        visuals.selection.bg_fill = Color32::from_rgb(150, 120, 20);
        cc.egui_ctx.set_theme(egui::ThemePreference::Dark);
        cc.egui_ctx.set_visuals_of(egui::Theme::Dark, visuals);
        if let Some(hwnd) = window_handle(cc) {
            WINDOW.store(hwnd, Ordering::SeqCst);
            crate::platform::window::set_main_window(hwnd);
        }
        let tray = create_tray().inspect_err(|err| logf!(control::logger(), "No notification area icon: {err}")).ok();
        // Hidden only with an icon to bring it back from.
        let start_hidden = start_hidden && tray.is_some();
        let link = Link::start(&shared);
        let mut app = App {
            shared: shared.clone(),
            link,
            generation: u64::MAX,
            strips: Vec::new(),
            buses: Vec::new(),
            cables: Vec::new(),
            capture_devices: Vec::new(),
            render_devices: Vec::new(),
            apps: Vec::new(),
            edited: HashMap::new(),
            meter_display: HashMap::new(),
            last_frame: Instant::now(),
            question: None,
            new_cable_name: String::new(),
            new_cable_kind: "loopback".into(),
            renaming: None,
            profile_dialog: None,
            duck_dialog: None,
            processing_dialog: None,
            holding: false,
            editing_label: None,
            _tray: tray,
            start_hidden,
        };
        let (mode, state) = {
            let s = lock(&shared);
            (s.mode, s.service_state)
        };
        if mode == Mode::OnDemand {
            if state == ServiceState::Stopped {
                app.question = Some(Question::StartServiceAtStartup);
            } else {
                run_job(&shared, "Starting the mixer\u{2026}", |shared| {
                    shell::run_on_demand(shared);
                    Ok(())
                });
            }
        }
        app
    }

    /// The Options menu's "Start with Windows", read from the registry each
    /// time, so it shows a change made in Task Manager.
    fn startup_option(&mut self, ui: &mut egui::Ui, mode: Mode) {
        let state = startup::state();
        let mut on = state == StartupState::On;
        let toggle = ui
            .checkbox(&mut on, "Start with Windows")
            .on_hover_text("Opens Lemon Mix in the notification area when you sign in.");
        if toggle.changed()
            && let Err(err) = startup::set(on)
        {
            link::report_error(&self.shared, err);
        }
        let note = if state == StartupState::DisabledInTaskManager {
            Some("Turned off in Task Manager")
        } else if on && mode == Mode::OnDemand && !self.cables.is_empty() {
            // The cables are attached as administrator: without the service,
            // that is a prompt at every sign-in.
            Some(
                "Attaching the cables will ask for administrator
approval at each sign-in; the Windows service
attaches them without asking.",
            )
        } else {
            None
        };
        if let Some(note) = note {
            ui.label(RichText::new(note).small().color(Color32::GRAY));
        }
    }

    fn send(&self, request: Request) {
        self.link.send(request);
    }

    fn mixer(&mut self, id: &str, command: MixerCommand) {
        self.edited.insert(id.to_string(), Instant::now());
        self.send(Request::Mixer { command });
    }

    /// Takes the latest snapshot, keeping the values of controls the user is
    /// changing, and moves the meters. Returns whether a meter is still
    /// falling, so the window needs drawing again soon though nothing new
    /// came from the mixer.
    fn refresh(&mut self) -> bool {
        let now = Instant::now();
        let holding = self.edited.values().any(|t| now - *t < EDIT_HOLD);
        // When a hold ends, the mixer's values take over again, whether or
        // not a new snapshot came meanwhile.
        let hold_ended = self.holding && !holding;
        self.holding = holding;
        if lock(&self.shared).generation != self.generation || hold_ended {
            self.take_snapshot(now);
        }
        self.move_meters(now)
    }

    /// Meters rise at once and fall at about 24 dB a second; returns whether
    /// any is still falling. One fallen below the scale shows nothing.
    fn move_meters(&mut self, now: Instant) -> bool {
        /// -80 dB: well below the meters' scale.
        const SILENT: f32 = 1e-4;
        let elapsed = (now - self.last_frame).as_secs_f32().min(0.2);
        let fall = 10f32.powf(-24.0 * elapsed / 20.0);
        self.last_frame = now;
        let mut falling = false;
        for (id, peak) in
            self.strips.iter().map(|s| (&s.id, s.meter.peak)).chain(self.buses.iter().map(|b| (&b.id, b.meter.peak)))
        {
            let shown = self.meter_display.entry(id.clone()).or_insert([0.0; 2]);
            for channel in 0..2 {
                let fallen = shown[channel] * fall;
                shown[channel] = if peak[channel] >= fallen {
                    peak[channel]
                } else if fallen < SILENT {
                    0.0
                } else {
                    falling = true;
                    fallen
                };
            }
        }
        falling
    }

    fn take_snapshot(&mut self, now: Instant) {
        let s = lock(&self.shared);
        self.generation = s.generation;
        let snapshot = &s.snapshot;
        let held = |id: &str, edited: &HashMap<String, Instant>| edited.get(id).is_some_and(|t| now - *t < EDIT_HOLD);
        let mut strips = snapshot.strips.clone();
        for strip in &mut strips {
            if held(&strip.id, &self.edited)
                && let Some(local) = self.strips.iter().find(|s| s.id == strip.id)
            {
                strip.params = local.params.clone();
                strip.routes = local.routes.clone();
                strip.duck_by = local.duck_by.clone();
                strip.label = local.label.clone();
                if let (Some(port), Some(local_port)) = (&mut strip.port, &local.port) {
                    port.endpoint = local_port.endpoint.clone();
                    port.exclusive = local_port.exclusive;
                }
            }
        }
        let mut buses = snapshot.buses.clone();
        for bus in &mut buses {
            if held(&bus.id, &self.edited)
                && let Some(local) = self.buses.iter().find(|b| b.id == bus.id)
            {
                bus.params = local.params.clone();
                bus.label = local.label.clone();
                if let (Some(port), Some(local_port)) = (&mut bus.port, &local.port) {
                    port.endpoint = local_port.endpoint.clone();
                    port.exclusive = local_port.exclusive;
                }
            }
        }
        self.strips = strips;
        self.buses = buses;
        self.cables = snapshot.cables.clone();
        self.capture_devices = snapshot.capture_devices.clone();
        self.render_devices = snapshot.render_devices.clone();
        self.apps = snapshot.apps.clone();
    }
}

/// The window's Win32 handle.
fn window_handle(cc: &eframe::CreationContext<'_>) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

/// The notification area icon: a click or Show brings the window back,
/// Quit stops the mixer and exits.
fn create_tray() -> Result<tray_icon::TrayIcon, String> {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let menu = Menu::new();
    let show = MenuItem::new("Show Lemon Mix", true, None);
    let quit = MenuItem::new("Quit", true, None);
    menu.append_items(&[&show, &PredefinedMenuItem::separator(), &quit]).map_err(|e| e.to_string())?;
    let (show_id, quit_id) = (show.id().clone(), quit.id().clone());
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == show_id {
            show_window();
        } else if event.id == quit_id {
            QUIT.store(true, Ordering::SeqCst);
            show_window();
        }
    }));
    // A left click shows the window; the menu is on the right button.
    TrayIconEvent::set_event_handler(Some(|event: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
            show_window();
        }
    }));
    let icon = Icon::from_rgba(crate::icon::lemon_rgba(32), 32, 32).map_err(|e| e.to_string())?;
    TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Lemon Mix")
        .with_icon(icon)
        .build()
        .map_err(|e| e.to_string())
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let animating = self.refresh();
        // Started with Windows: hidden as soon as it first shows (eframe shows
        // every window after its first frame, before this command applies),
        // unless something is waiting for an answer.
        if std::mem::take(&mut self.start_hidden) && self.question.is_none() && lock(&self.shared).errors.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        // Shown from outside: the window system hears it too, or it would
        // still count the window as hidden.
        if SHOWN.swap(false, Ordering::SeqCst) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }
        let (mode, busy) = {
            let s = lock(&self.shared);
            (s.mode, s.busy.is_some())
        };

        // Closing the window: in the service the audio carries on anyway; on
        // demand the window goes to the notification area, unless it has no
        // icon there or the user chose Quit.
        if ctx.input(|i| i.viewport().close_requested())
            && !QUIT.load(Ordering::SeqCst)
            && mode == Mode::OnDemand
            && self._tray.is_some()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            hide_window();
        }
        if QUIT.load(Ordering::SeqCst) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        let modal = self.question.is_some()
            || !lock(&self.shared).errors.is_empty()
            || self.renaming.is_some()
            || self.profile_dialog.is_some();
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_enabled_ui(!modal, |ui| self.header(ui, mode, busy));
        });
        egui::SidePanel::right("devices").resizable(true).default_width(300.0).min_width(240.0).show(ctx, |ui| {
            ui.add_enabled_ui(!modal && !busy, |ui| self.cables_panel(ui));
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_enabled_ui(!modal && !busy, |ui| self.mixer_panel(ui));
        });
        self.dialogs(ctx);
        // The strip dialogs are greyed out while the window is (another
        // dialog open, or busy), like the strips behind them.
        let enabled = !modal && !busy;
        self.duck_dialog_ui(ctx, enabled);
        self.processing_dialog_ui(ctx, enabled);
        // Drawn again when the mixer's state changes (the link asks), when
        // the user acts (egui asks), and meanwhile only while something moves:
        // a falling meter, or an edit hold that will hand back to the mixer.
        if animating || busy {
            ctx.request_repaint_after(Duration::from_millis(33));
        } else if self.holding {
            ctx.request_repaint_after(EDIT_HOLD);
        }
    }
}

impl App {
    fn header(&mut self, ui: &mut egui::Ui, mode: Mode, busy: bool) {
        let (snapshot_status, server_error, cable_count, notice, state, connected, version, busy_text) = {
            let s = lock(&self.shared);
            (
                s.snapshot.status.clone(),
                s.snapshot.server_error.clone(),
                s.snapshot.cables.len(),
                s.notice.clone(),
                s.service_state,
                s.connected,
                s.version.clone(),
                s.busy.clone(),
            )
        };
        ui.add_space(6.0);
        let server = if !server_error.is_empty() {
            (Tone::Error, "USB/IP server error \u{00b7} see the diagnostic log in the settings folder".to_string())
        } else {
            let devices = match cable_count {
                0 => "no virtual devices yet".to_string(),
                1 => "1 virtual device".to_string(),
                n => format!("{n} virtual devices"),
            };
            (Tone::Ok, format!("USB/IP server running \u{00b7} {devices}"))
        };
        // The first line carries the Options menu at its right end.
        ui.horizontal(|ui| {
            widgets::status_line(ui, server.0, &server.1, None);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    self.options_menu(ui, mode, state);
                    let profile = lock(&self.shared).snapshot.profile.clone();
                    if !profile.is_empty()
                        && ui
                            .button(format!("Profile: {profile}"))
                            .on_hover_text("Switch, create or delete profiles")
                            .clicked()
                    {
                        self.profile_dialog = Some(ProfileDialog::default());
                    }
                });
            });
        });
        let mode_line = match (busy_text, mode, state) {
            (Some(text), ..) => (Tone::Pending, text),
            (None, Mode::Service, _) if connected && version != crate::VERSION => {
                (Tone::Ok, format!("Running as a Windows service \u{00b7} the service is version {version}"))
            }
            (None, Mode::Service, _) if connected => {
                (Tone::Ok, "Running as a Windows service \u{00b7} audio keeps running after this window closes".into())
            }
            (None, Mode::Service, ServiceState::Running) => {
                (Tone::Error, "The Windows service is not answering \u{00b7} see its log in the settings folder".into())
            }
            (None, Mode::Service, _) => {
                (Tone::Error, "The Windows service is not running \u{00b7} start it from the Options menu".into())
            }
            (None, Mode::OnDemand, ServiceState::Running) => {
                (Tone::Error, "Running on demand, but the Windows service runs too \u{00b7} restart Lemon Mix".into())
            }
            (None, Mode::OnDemand, _) => (
                Tone::Ok,
                "Running on demand \u{00b7} audio keeps running from the notification area until Quit".into(),
            ),
        };
        widgets::status_line(ui, mode_line.0, &mode_line.1, None);
        let status = match notice {
            Some((line, covers)) if covers == snapshot_status => Some(line),
            _ => snapshot_status,
        };
        if let Some((tone, text)) = status {
            let retry = (tone == Tone::Error).then_some("Retry");
            if widgets::status_line(ui, tone, &text, retry) {
                self.send(Request::Sync);
            }
        }
        ui.add_space(4.0);
    }

    fn options_menu(&mut self, ui: &mut egui::Ui, mode: Mode, state: ServiceState) {
        let service_version = {
            let s = lock(&self.shared);
            (mode == Mode::Service && s.connected && s.version != crate::VERSION).then(|| s.version.clone())
        };
        ui.menu_button("Options", |ui| {
            ui.label(RichText::new(format!("Lemon Mix {}", crate::VERSION)).small().color(Color32::GRAY));
            if let Some(version) = service_version {
                ui.label(RichText::new(format!("Windows service {version}")).small().color(Color32::GRAY));
            }
            ui.separator();
            if ui.button("Sync with Windows Sound").clicked() {
                self.send(Request::Sync);
                ui.close_menu();
            }
            if ui.button("Refresh device list").clicked() {
                self.send(Request::Mixer { command: MixerCommand::RefreshDevices });
                ui.close_menu();
            }
            ui.separator();
            match state {
                ServiceState::NotInstalled => {
                    if ui.button("Install as Windows service\u{2026}").clicked() {
                        self.question = Some(Question::InstallService);
                        ui.close_menu();
                    }
                }
                ServiceState::Stopped if ui.button("Start Windows service").clicked() => {
                    self.question = Some(Question::StartService);
                    ui.close_menu();
                }
                _ => {}
            }
            if state != ServiceState::NotInstalled && ui.button("Uninstall Windows service\u{2026}").clicked() {
                self.question = Some(Question::UninstallService);
                ui.close_menu();
            }
            ui.separator();
            self.startup_option(ui, mode);
            ui.separator();
            if ui.button("Open settings folder").clicked() {
                shell::open_settings_folder(&self.shared);
                ui.close_menu();
            }
            if ui.button("Download and install driver").clicked() {
                shell::download_and_install_driver(&self.shared);
                ui.close_menu();
            }
            ui.separator();
            let quit = if mode == Mode::OnDemand { "Quit (stops the audio)" } else { "Close window" };
            if ui.button(quit).clicked() {
                QUIT.store(true, Ordering::SeqCst);
                ui.close_menu();
            }
        });
    }

    fn mixer_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            ui.label(RichText::new("INPUTS").small().strong().color(Color32::GRAY));
            ui.horizontal_top(|ui| {
                for index in 0..self.strips.len() {
                    let strip = &self.strips[index];
                    let highlight = strip_highlight(&strip.params, &strip.meter);
                    widgets::strip_frame(ui, highlight, |ui| self.strip_ui(ui, index));
                }
                if widgets::add_column(ui, "+ Input", "Add a hardware input strip") {
                    self.send(Request::Mixer { command: MixerCommand::AddStrip });
                }
            });
            ui.add_space(8.0);
            ui.separator();
            ui.label(RichText::new("OUTPUTS").small().strong().color(Color32::GRAY));
            ui.horizontal_top(|ui| {
                for index in 0..self.buses.len() {
                    let highlight = self.buses[index].params.mute.then_some(ERROR_COLOR);
                    widgets::strip_frame(ui, highlight, |ui| self.bus_ui(ui, index));
                }
                if widgets::add_column(ui, "+ Output", "Add a hardware output bus") {
                    self.send(Request::Mixer { command: MixerCommand::AddBus });
                }
            });
        });
    }

    /// The short name of a bus on routing buttons: A1..., and B1... for the
    /// virtual buses in order.
    fn bus_short_names(&self) -> Vec<(String, String, String)> {
        let mut virtual_index = 0;
        self.buses
            .iter()
            .map(|b| {
                let short = if b.id.starts_with('A') {
                    b.id.clone()
                } else {
                    virtual_index += 1;
                    format!("B{virtual_index}")
                };
                (b.id.clone(), short, b.label.clone())
            })
            .collect()
    }

    fn label_ui(&mut self, ui: &mut egui::Ui, id: &str, label: &str, editable: bool) {
        if let Some((editing, text)) = &mut self.editing_label
            && editing == id
        {
            let edit_id = egui::Id::new((id, "label"));
            let response =
                ui.add(egui::TextEdit::singleline(text).id(edit_id).desired_width(widgets::STRIP_WIDTH - 8.0));
            // Focus once, when editing starts; asking every frame would take
            // the keyboard back from wherever the user moves it.
            if ui.data(|d| d.get_temp::<bool>(edit_id).is_none()) {
                ui.data_mut(|d| d.insert_temp(edit_id, true));
                response.request_focus();
                // With the whole name selected, typing replaces it.
                let mut state = egui::text_edit::TextEditState::load(ui.ctx(), edit_id).unwrap_or_default();
                state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(text.chars().count()),
                )));
                state.store(ui.ctx(), edit_id);
            }
            let (enter, escape, pressed_elsewhere) = ui.input(|i| {
                let elsewhere =
                    i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|pos| !response.rect.contains(pos));
                (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape), elsewhere)
            });
            if escape {
                ui.data_mut(|d| d.remove::<bool>(edit_id));
                self.editing_label = None;
            } else if enter || pressed_elsewhere || response.lost_focus() {
                ui.data_mut(|d| d.remove::<bool>(edit_id));
                ui.memory_mut(|m| m.surrender_focus(edit_id));
                let (id, text) = self.editing_label.take().unwrap();
                self.mixer(&id, MixerCommand::Label { id: id.clone(), label: text.clone() });
                if let Some(s) = self.strips.iter_mut().find(|s| s.id == id) {
                    s.label = text.clone();
                }
                if let Some(b) = self.buses.iter_mut().find(|b| b.id == id) {
                    b.label = text;
                }
            }
            return;
        }
        let text = RichText::new(widgets::ellipsize(label, 16)).strong();
        let response = ui.add(egui::Label::new(text).sense(egui::Sense::click())).on_hover_text(label);
        if editable && response.double_clicked() {
            self.editing_label = Some((id.to_string(), label.to_string()));
        }
    }

    /// The small grey line under a strip's or bus's name, with a remove
    /// button for hardware ones. Returns whether it was clicked.
    fn kind_line(&self, ui: &mut egui::Ui, kind: &str, removable: bool, hint: &str) -> bool {
        ui.horizontal(|ui| {
            ui.label(RichText::new(kind).small().color(Color32::GRAY));
            removable
                && ui
                    .with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add(egui::Button::new(RichText::new("\u{2212}").small()).small())
                            .on_hover_text(hint)
                            .clicked()
                    })
                    .inner
        })
        .inner
    }

    fn strip_ui(&mut self, ui: &mut egui::Ui, index: usize) {
        let strip = self.strips[index].clone();
        let hardware = strip.port.is_some();
        ui.vertical(|ui| {
            ui.set_width(widgets::STRIP_WIDTH);
            self.label_ui(ui, &strip.id, &strip.label, hardware);
            let kind = if hardware { "Hardware input" } else { "Virtual input" };
            if self.kind_line(ui, kind, hardware, "Remove this input strip") {
                self.question = Some(Question::RemoveStrip(strip.id.clone(), strip.label.clone()));
            }
            if let Some(port) = &strip.port {
                let buses: Vec<(String, String)> = self
                    .bus_short_names()
                    .into_iter()
                    .zip(&self.buses)
                    .map(|((_, short, label), bus)| {
                        let name = if label == short { short } else { format!("{short} ({label})") };
                        (format!("{}{}", crate::mixer::BUS_SOURCE, bus.key), name)
                    })
                    .collect();
                if let Some((endpoint, exclusive)) = widgets::device_picker(
                    ui,
                    &strip.id,
                    port,
                    &self.capture_devices,
                    "Recording devices",
                    &buses,
                    &self.apps,
                ) {
                    if let Some(p) = &mut self.strips[index].port {
                        (p.endpoint, p.exclusive) = (endpoint.clone(), exclusive);
                    }
                    self.mixer(&strip.id, MixerCommand::Device { id: strip.id.clone(), endpoint, exclusive });
                }
            } else {
                ui.add_space(widgets::PICKER_HEIGHT);
            }

            let mut params = strip.params.clone();
            let meter = self.meter_display.get(&strip.id).copied().unwrap_or_default();
            let mut changed = widgets::fader(ui, &strip.id, &mut params.gain_db, meter, strip.meter.reduction);
            changed |= widgets::pan(ui, &strip.id, &mut params.pan);

            // Routing: one button per bus.
            let names = self.bus_short_names();
            let mut route_change = None;
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(3.0, 3.0);
                for (bus_id, short, label) in &names {
                    let on = strip.routes.contains(bus_id);
                    if widgets::toggle(ui, short, on, LEMON).on_hover_text(format!("Send to {label}")).clicked() {
                        route_change = Some((bus_id.clone(), !on));
                    }
                }
            });
            if let Some((bus, on)) = route_change {
                let routes = &mut self.strips[index].routes;
                routes.retain(|r| r != &bus);
                if on {
                    routes.push(bus.clone());
                }
                self.mixer(&strip.id, MixerCommand::Route { strip: strip.id.clone(), bus, on });
            }

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(3.0, 3.0);
                changed |= widgets::toggle(ui, "M", params.mute, ERROR_COLOR).on_hover_text("Mute").clicked() && {
                    params.mute = !params.mute;
                    true
                };
                changed |= widgets::toggle(ui, "S", params.solo, PENDING_COLOR).on_hover_text("Solo").clicked() && {
                    params.solo = !params.solo;
                    true
                };
                changed |=
                    widgets::toggle(ui, "Mono", params.mono, LEMON).on_hover_text("Sum left and right").clicked() && {
                        params.mono = !params.mono;
                        true
                    };
                let ducks = !strip.duck_by.is_empty();
                let hint = if ducks { "Ducking: lowered while other strips play" } else { "Set up ducking" };
                if widgets::toggle(ui, "Duck", ducks, DUCK_COLOR).on_hover_text(hint).clicked() {
                    self.duck_dialog = Some(strip.key.clone());
                }
            });
            ui.add_space(2.0);
            changed |= widgets::knob_row(ui, &strip.id, "Bass", &mut params.eq[0], -12.0..=12.0, "dB");
            changed |= widgets::knob_row(ui, &strip.id, "Mid", &mut params.eq[1], -12.0..=12.0, "dB");
            changed |= widgets::knob_row(ui, &strip.id, "Treble", &mut params.eq[2], -12.0..=12.0, "dB");
            // Noise suppression, then the gate that works best after it.
            changed |= widgets::knob_row(ui, &strip.id, "Denoise", &mut params.denoise, 0.0..=10.0, "");
            // The Gate and Comp labels open their options.
            let gate_changed = params.gate_options != engine::GateOptions::default();
            let (moved, open_gate) = widgets::knob_row_with_options(
                ui,
                &strip.id,
                "Gate",
                &mut params.gate,
                0.0..=10.0,
                "",
                Some(gate_changed),
            );
            changed |= moved;
            let comp_changed = params.comp_options != engine::CompOptions::default();
            let (moved, open_comp) = widgets::knob_row_with_options(
                ui,
                &strip.id,
                "Comp",
                &mut params.comp,
                0.0..=10.0,
                "",
                Some(comp_changed),
            );
            changed |= moved;
            if open_gate {
                self.processing_dialog = Some((strip.key.clone(), Processor::Gate));
            }
            if open_comp {
                self.processing_dialog = Some((strip.key.clone(), Processor::Compressor));
            }
            if let Some(port) = &strip.port {
                widgets::port_status(ui, port);
            }
            if changed {
                self.strips[index].params = params.clone();
                self.mixer(&strip.id, MixerCommand::Strip { id: strip.id.clone(), params });
            }
        });
    }

    fn bus_ui(&mut self, ui: &mut egui::Ui, index: usize) {
        let bus = self.buses[index].clone();
        let hardware = bus.port.is_some();
        let short = self.bus_short_names().get(index).map(|(_, s, _)| s.clone()).unwrap_or_default();
        ui.vertical(|ui| {
            ui.set_width(widgets::STRIP_WIDTH);
            self.label_ui(ui, &bus.id, &bus.label, hardware);
            let kind = if hardware {
                format!("{short} \u{00b7} hardware output")
            } else {
                format!("{short} \u{00b7} virtual output")
            };
            if self.kind_line(ui, &kind, hardware, "Remove this output bus") {
                self.question = Some(Question::RemoveBus(bus.id.clone(), bus.label.clone()));
            }
            if let Some(port) = &bus.port {
                if let Some((endpoint, exclusive)) =
                    widgets::device_picker(ui, &bus.id, port, &self.render_devices, "Playback devices", &[], &[])
                {
                    if let Some(p) = &mut self.buses[index].port {
                        (p.endpoint, p.exclusive) = (endpoint.clone(), exclusive);
                    }
                    self.mixer(&bus.id, MixerCommand::Device { id: bus.id.clone(), endpoint, exclusive });
                }
            } else {
                ui.add_space(widgets::PICKER_HEIGHT);
            }
            let mut params = bus.params.clone();
            let meter = self.meter_display.get(&bus.id).copied().unwrap_or_default();
            let mut changed = widgets::fader(ui, &bus.id, &mut params.gain_db, meter, bus.meter.reduction);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(3.0, 3.0);
                changed |= widgets::toggle(ui, "Mute", params.mute, ERROR_COLOR).clicked() && {
                    params.mute = !params.mute;
                    true
                };
                changed |= widgets::toggle(ui, "Mono", params.mono, LEMON).clicked() && {
                    params.mono = !params.mono;
                    true
                };
            });
            let mut limiter = params.limiter.is_some();
            if ui.checkbox(&mut limiter, "Limiter").on_hover_text("Keep peaks below the ceiling").changed() {
                params.limiter = limiter.then_some(-1.0);
                changed = true;
            }
            if let Some(ceiling) = &mut params.limiter {
                changed |= widgets::knob_row(ui, &bus.id, "Ceiling", ceiling, -20.0..=0.0, "dB");
            }
            if let Some(port) = &bus.port {
                widgets::port_status(ui, port);
            }
            if changed {
                self.buses[index].params = params.clone();
                self.mixer(&bus.id, MixerCommand::Bus { id: bus.id.clone(), params });
            }
        });
    }

    fn cables_panel(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.heading("Virtual devices");
        ui.label(
            RichText::new(
                "Each appears in Windows Sound. Inputs are playback devices that apps play into; \
                 outputs are recording devices that apps record from; loopback devices are both.",
            )
            .small()
            .color(Color32::GRAY),
        );
        ui.add_space(6.0);
        egui::ScrollArea::vertical().max_height(ui.available_height() - 120.0).show(ui, |ui| {
            if self.cables.is_empty() {
                ui.label(RichText::new("No virtual devices yet.").italics());
            }
            for cable in self.cables.clone() {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&cable.name).strong());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("Remove").clicked() {
                                self.question = Some(Question::RemoveCable(cable.clone()));
                            }
                            if ui.small_button("Rename").clicked() {
                                self.renaming = Some((cable.serial.clone(), cable.name.clone()));
                            }
                        });
                    });
                    let mut kind = cable.kind.clone();
                    egui::ComboBox::from_id_salt(("kind", &cable.serial))
                        .selected_text(kind_label(&kind))
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            for k in ["input", "output", "loopback"] {
                                ui.selectable_value(&mut kind, k.to_string(), kind_label(k));
                            }
                        });
                    if kind != cable.kind {
                        self.send(Request::SetCableKind { serial: cable.serial.clone(), kind });
                    }
                });
            }
        });
        ui.separator();
        ui.label(RichText::new("Add a virtual device").strong());
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_cable_name)
                    .hint_text("Name")
                    .desired_width(ui.available_width()),
            );
        });
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("new kind")
                .selected_text(kind_label(&self.new_cable_kind))
                .width(150.0)
                .show_ui(ui, |ui| {
                    for k in ["input", "output", "loopback"] {
                        ui.selectable_value(&mut self.new_cable_kind, k.to_string(), kind_label(k));
                    }
                });
            let can_add = !self.new_cable_name.trim().is_empty();
            if ui.add_enabled(can_add, egui::Button::new("Add")).clicked() {
                self.send(Request::AddCable {
                    name: self.new_cable_name.trim().to_string(),
                    kind: self.new_cable_kind.clone(),
                });
                self.new_cable_name.clear();
            }
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let error = lock(&self.shared).errors.first().cloned();
        if let Some(error) = error {
            widgets::dialog(ctx, "Lemon Mix", |ui| {
                ui.label(&error);
                ui.add_space(8.0);
                if ui.button("OK").clicked() {
                    lock(&self.shared).errors.remove(0);
                }
            });
            return;
        }
        if let Some((serial, name)) = &mut self.renaming {
            let mut done = None;
            widgets::dialog(ctx, "Rename virtual device", |ui| {
                ui.label("Windows Sound shows a device under the name it first had, so the device is added again under the new name.");
                let response = ui.add(egui::TextEdit::singleline(name).desired_width(300.0));
                let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.horizontal(|ui| {
                    if ui.button("Rename").clicked() || enter {
                        done = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        done = Some(false);
                    }
                });
            });
            match done {
                Some(true) => {
                    let request = Request::RenameCable { serial: serial.clone(), name: name.trim().to_string() };
                    self.renaming = None;
                    self.send(request);
                }
                Some(false) => self.renaming = None,
                None => {}
            }
            return;
        }
        if self.question.is_none() && self.profile_dialog.is_some() {
            self.profile_dialog_ui(ctx);
            return;
        }
        let Some(question) = &self.question else {
            return;
        };
        let (title, text, yes, no) = match question {
            Question::StartServiceAtStartup => (
                "Start Windows service",
                "The Lemon Mix Windows service is installed but not running.\n\n\
                 Start the service: it runs the mixer with its own settings, and keeps the audio running after this window closes.\n\n\
                 Run here: run the mixer in this window until you quit."
                    .to_string(),
                "Start the service",
                "Run here",
            ),
            Question::InstallService => (
                "Install Windows service",
                "Install Lemon Mix as a Windows service?\n\n\
                 The service starts with Windows and keeps the virtual devices and the mixer running without this window, \
                 with no administrator prompt each time. It gets a copy of your settings, and this window then manages it."
                    .to_string(),
                "Install",
                "Cancel",
            ),
            Question::StartService => (
                "Start Windows service",
                "Start the Lemon Mix Windows service?\n\nIt takes over from this window with its own settings, from when it was installed."
                    .to_string(),
                "Start",
                "Cancel",
            ),
            Question::UninstallService => (
                "Uninstall Windows service",
                "Uninstall the Lemon Mix Windows service?\n\n\
                 Its virtual devices leave Windows Sound. This window then runs the mixer itself, with the service's settings, \
                 and asks for administrator approval to add the devices to Windows Sound again."
                    .to_string(),
                "Uninstall",
                "Cancel",
            ),
            Question::RemoveCable(cable) => (
                "Remove virtual device",
                format!(
                    "Remove \"{}\"?\n\nIt will disappear from Windows Sound, and any app using it will lose that audio device.",
                    cable.name
                ),
                "Remove",
                "Cancel",
            ),
            Question::RemoveStrip(_, label) => (
                "Remove input strip",
                format!(
                    "Remove the input strip \"{label}\"?\n\nIts device and settings are forgotten, and the input strips after it move up a number."
                ),
                "Remove",
                "Cancel",
            ),
            Question::RemoveBus(_, label) => (
                "Remove output bus",
                format!(
                    "Remove the output bus \"{label}\"?\n\nIts device and settings are forgotten, every input stops sending to it, \
                     and the output buses after it move up a number."
                ),
                "Remove",
                "Cancel",
            ),
            Question::DeleteProfile(name) => (
                "Delete profile",
                format!("Delete the profile \"{name}\"?\n\nIts settings are lost."),
                "Delete",
                "Cancel",
            ),
        };
        let mut answer = None;
        widgets::dialog(ctx, title, |ui| {
            ui.label(text);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(yes).clicked() {
                    answer = Some(true);
                }
                if ui.button(no).clicked() {
                    answer = Some(false);
                }
            });
        });
        let Some(answer) = answer else {
            return;
        };
        let question = self.question.take().unwrap();
        let shared = &self.shared;
        match (question, answer) {
            (Question::StartServiceAtStartup, true) => {
                run_job(shared, "Starting the Windows service\u{2026}", |s| shell::switch_to_service(s, false))
            }
            (Question::StartServiceAtStartup, false) => run_job(shared, "Starting the mixer\u{2026}", |s| {
                shell::run_on_demand(s);
                Ok(())
            }),
            (Question::InstallService, true) => {
                run_job(shared, "Installing the Windows service\u{2026}", shell::install_service)
            }
            (Question::StartService, true) => {
                let running_here = lock(shared).mode == Mode::OnDemand;
                run_job(shared, "Starting the Windows service\u{2026}", move |s| {
                    shell::switch_to_service(s, running_here)
                })
            }
            (Question::UninstallService, true) => {
                run_job(shared, "Uninstalling the Windows service\u{2026}", shell::uninstall_service)
            }
            (Question::RemoveCable(cable), true) => self.send(Request::RemoveCable { serial: cable.serial }),
            (Question::RemoveStrip(id, _), true) => {
                self.send(Request::Mixer { command: MixerCommand::RemoveStrip { id } });
                self.edited.clear();
            }
            (Question::RemoveBus(id, _), true) => {
                self.send(Request::Mixer { command: MixerCommand::RemoveBus { id } });
                self.edited.clear();
            }
            (Question::DeleteProfile(name), true) => {
                self.send(Request::Mixer { command: MixerCommand::DeleteProfile { name } });
            }
            _ => {}
        }
    }
}

/// One row of an options dialog: its label, with a hint, and its controls.
fn option_row(ui: &mut egui::Ui, label: &str, hint: &str, add: impl FnOnce(&mut egui::Ui) -> bool) -> bool {
    ui.horizontal(|ui| {
        ui.add_sized([90.0, 18.0], egui::Label::new(label)).on_hover_text(hint);
        add(ui)
    })
    .inner
}

/// A number box in an options dialog, kept apart by the strip and `key`.
fn option_number(
    ui: &mut egui::Ui,
    strip: &str,
    key: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    speed: f32,
    unit: &str,
) -> bool {
    let decimals = if speed < 1.0 { 1 } else { 0 };
    let format = |v: f32| format!("{v:.decimals$} {unit}");
    widgets::NumberBox {
        id: egui::Id::new((strip, key)),
        width: 76.0,
        range,
        speed,
        format: &format,
        edit_text: &|v| format!("{v:.decimals$}"),
        parse: &widgets::parse_number,
        hint: "Click to type a value, or drag",
    }
    .show(ui, value)
}

/// A slider and a number box for one setting of an options dialog.
fn option_slider(
    ui: &mut egui::Ui,
    strip: &str,
    key: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    speed: f32,
    unit: &str,
) -> bool {
    let slider = ui.add(egui::Slider::new(value, range.clone()).show_value(false));
    let typed = option_number(ui, strip, key, value, range, speed, unit);
    slider.changed() | typed
}

/// A row with a slider over the usual values and a number box that takes the
/// whole range, for durations that are mostly short but may be long.
#[allow(clippy::too_many_arguments)]
fn option_slider_row(
    ui: &mut egui::Ui,
    label: &str,
    hint: &str,
    (strip, key): (&str, &str),
    value: &mut f32,
    usual: std::ops::RangeInclusive<f32>,
    range: std::ops::RangeInclusive<f32>,
    unit: &str,
) -> bool {
    option_row(ui, label, hint, |ui| {
        // A longer value typed in shows the slider at its end, unchanged.
        let slider = ui.add(egui::Slider::new(value, usual).show_value(false).clamping(egui::SliderClamping::Never));
        let typed = option_number(ui, strip, key, value, range, 5.0, unit);
        slider.changed() | typed
    })
}

/// The processor an options dialog is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Processor {
    Gate,
    Compressor,
}

impl App {
    /// A strip's noise gate or compressor options. Not modal, like the
    /// ducking dialog; changes apply at once.
    fn processing_dialog_ui(&mut self, ctx: &egui::Context, enabled: bool) {
        let Some((key, processor)) = self.processing_dialog.clone() else {
            return;
        };
        let Some(index) = self.strips.iter().position(|s| s.key == key) else {
            self.processing_dialog = None;
            return;
        };
        // Commands name the strip by its ID as it is now.
        let id = self.strips[index].id.clone();
        let strip = self.strips[index].clone();
        let mut params = strip.params.clone();
        let mut changed = false;
        let mut open = true;
        let title = match processor {
            Processor::Gate => format!("Noise gate: {}", strip.label),
            Processor::Compressor => format!("Compressor: {}", strip.label),
        };
        egui::Window::new(title)
            .id(egui::Id::new("processing dialog"))
            .enabled(enabled)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_pos(ctx.screen_rect().center() - Vec2::new(170.0, 150.0))
            .show(ctx, |ui| {
                ui.set_width(330.0);
                let id = id.as_str();
                let grey = |ui: &mut egui::Ui, text: String| {
                    ui.label(RichText::new(text).small().color(Color32::GRAY));
                };
                match processor {
                    Processor::Gate => {
                        let g = &mut params.gate_options;
                        if params.gate > 0.0 {
                            let threshold = engine::dsp::gate_threshold_db(params.gate);
                            grey(ui, format!("Threshold {threshold:.0} dBFS, set by the strip's Gate knob."));
                        } else {
                            grey(ui, "The gate is off: turn up the strip's Gate knob to set its threshold.".into());
                        }
                        ui.add_space(4.0);
                        changed |= option_row(ui, "Range", "How far the gate closes below the threshold", |ui| {
                            option_slider(ui, id, "gate range", &mut g.range_db, engine::params::GATE_RANGE_DB, 0.2, "dB")
                        });
                        changed |= option_row(ui, "Attack", "How fast it opens once the level passes the threshold", |ui| {
                            option_number(ui, id, "gate attack", &mut g.attack_ms, engine::params::GATE_ATTACK_MS, 0.1, "ms")
                        });
                        changed |= option_row(ui, "Hold", "How long it stays open after the level drops", |ui| {
                            option_number(ui, id, "gate hold", &mut g.hold_ms, engine::params::GATE_HOLD_MS, 5.0, "ms")
                        });
                        changed |= option_row(ui, "Release", "How fast it closes after the hold", |ui| {
                            option_number(ui, id, "gate release", &mut g.release_ms, engine::params::GATE_RELEASE_MS, 5.0, "ms")
                        });
                        if ui.button("Reset to defaults").clicked() && *g != engine::GateOptions::default() {
                            *g = engine::GateOptions::default();
                            changed = true;
                        }
                    }
                    Processor::Compressor => {
                        let c = &mut params.comp_options;
                        if ui
                            .checkbox(&mut c.manual, "Set the threshold and ratio here")
                            .on_hover_text("The strip's Comp knob then only turns the compressor on and off")
                            .changed()
                        {
                            changed = true;
                        }
                        if c.manual {
                            changed |= option_row(ui, "Threshold", "The level above which it reduces the gain", |ui| {
                                let range = engine::params::COMP_THRESHOLD_DB;
                                option_slider(ui, id, "comp threshold", &mut c.threshold_db, range, 0.2, "dBFS")
                            });
                            changed |= option_row(ui, "Ratio", "How much it reduces: 4 means 4 dB over becomes 1 dB", |ui| {
                                option_slider(ui, id, "comp ratio", &mut c.ratio, engine::params::COMP_RATIO, 0.05, ": 1")
                            });
                            if params.comp <= 0.0 {
                                grey(ui, "The compressor is off: turn up the strip's Comp knob to turn it on.".into());
                            }
                        } else {
                            let (threshold, ratio) = engine::dsp::comp_curve(params.comp);
                            grey(
                                ui,
                                if params.comp > 0.0 {
                                    format!("Threshold {threshold:.1} dBFS and ratio {ratio:.1}:1, set by the strip's Comp knob.")
                                } else {
                                    "The compressor is off: the strip's Comp knob turns it on and sets its curve.".into()
                                },
                            );
                        }
                        ui.add_space(4.0);
                        changed |= option_row(ui, "Attack", "How fast it reduces the gain once the level passes the threshold", |ui| {
                            option_number(ui, id, "comp attack", &mut c.attack_ms, engine::params::COMP_ATTACK_MS, 0.1, "ms")
                        });
                        changed |= option_row(ui, "Release", "How fast the gain recovers", |ui| {
                            option_number(ui, id, "comp release", &mut c.release_ms, engine::params::COMP_RELEASE_MS, 5.0, "ms")
                        });
                        changed |= option_row(ui, "Knee", "How gradually it starts around the threshold; 0 is a hard knee", |ui| {
                            option_number(ui, id, "comp knee", &mut c.knee_db, engine::params::COMP_KNEE_DB, 0.1, "dB")
                        });
                        changed |= option_row(ui, "Make-up", "Gain added back after compression", |ui| {
                            let mut automatic = c.makeup_db.is_none();
                            let mut changed = false;
                            if ui.checkbox(&mut automatic, "Automatic").changed() {
                                c.makeup_db = if automatic { None } else { Some(0.0) };
                                changed = true;
                            }
                            if let Some(makeup) = &mut c.makeup_db {
                                changed |= option_number(ui, id, "comp makeup", makeup, engine::params::COMP_MAKEUP_DB, 0.1, "dB");
                            }
                            changed
                        });
                        if ui.button("Reset to defaults").clicked() && *c != engine::CompOptions::default() {
                            *c = engine::CompOptions::default();
                            changed = true;
                        }
                    }
                }
                ui.add_space(4.0);
                let reduction = 20.0 * strip.meter.reduction.max(1e-6).log10();
                let text = if reduction < -0.1 {
                    format!("Gain reduction now (gate and compressor): {reduction:.1} dB")
                } else {
                    "No gain reduction now".to_string()
                };
                ui.label(RichText::new(text).color(if reduction < -0.1 { Color32::from_rgb(90, 160, 230) } else { Color32::GRAY }));
            });
        if changed {
            self.strips[index].params = params.clone();
            self.mixer(&id, MixerCommand::Strip { id: id.clone(), params });
        }
        if !open {
            self.processing_dialog = None;
        }
    }

    /// A strip's ducking: the strips that lower it while they play, by how
    /// much, and (under Timing) how fast. Not modal, so the mixer can be used
    /// while it is open; changes apply at once.
    fn duck_dialog_ui(&mut self, ctx: &egui::Context, enabled: bool) {
        let Some(key) = self.duck_dialog.clone() else {
            return;
        };
        let Some(index) = self.strips.iter().position(|s| s.key == key) else {
            // The strip went (a removed device, say).
            self.duck_dialog = None;
            return;
        };
        // Commands name the strip by its ID as it is now.
        let id = self.strips[index].id.clone();
        let strip = self.strips[index].clone();
        let others: Vec<(String, String)> =
            self.strips.iter().filter(|s| s.id != id).map(|s| (s.id.clone(), s.label.clone())).collect();
        let mut open = true;
        let mut sources = strip.duck_by.clone();
        let mut params = strip.params.clone();
        let mut sources_changed = false;
        let mut params_changed = false;
        egui::Window::new(format!("Ducking: {}", strip.label))
            .id(egui::Id::new("duck dialog"))
            .enabled(enabled)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_pos(ctx.screen_rect().center() - Vec2::new(170.0, 160.0))
            .show(ctx, |ui| {
                ui.set_width(320.0);
                ui.label(RichText::new("Lower this strip while any of these play:").strong());
                if others.is_empty() {
                    ui.label(RichText::new("There are no other strips.").italics());
                }
                egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                    for (other, label) in &others {
                        let mut on = sources.contains(other);
                        let text = if label == other { other.clone() } else { format!("{label} ({other})") };
                        if ui.checkbox(&mut on, text).changed() {
                            sources.retain(|s| s != other);
                            if on {
                                sources.push(other.clone());
                            }
                            sources_changed = true;
                        }
                    }
                });
                ui.add_space(6.0);
                let duck = &mut params.duck;
                let id = id.as_str();
                params_changed |=
                    option_row(ui, "Lower by", "How far the strip goes down while a source plays", |ui| {
                        option_slider(
                            ui,
                            id,
                            "duck amount",
                            &mut duck.amount_db,
                            0.0..=engine::params::MAX_DUCK_DB,
                            0.2,
                            "dB",
                        )
                    });
                params_changed |= option_row(
                    ui,
                    "Sensitivity",
                    "How loud a source must be to count as playing, in dBFS: lower hears quieter sources",
                    |ui| {
                        let range = engine::params::MIN_DUCK_THRESHOLD_DB..=0.0;
                        option_slider(ui, id, "duck threshold", &mut duck.threshold_db, range, 0.2, "dBFS")
                    },
                );
                // How long each change takes, end to end.
                params_changed |= option_slider_row(
                    ui,
                    "Fade down",
                    "How long it takes to go down to the lowered level once a source plays; 0 is at once",
                    (id, "duck fade down"),
                    &mut duck.fade_down_ms,
                    0.0..=1000.0,
                    engine::params::DUCK_FADE_DOWN_MS,
                    "ms",
                );
                params_changed |= option_slider_row(
                    ui,
                    "Hold",
                    "How long it stays down after the sources go quiet, so it does not come up between words",
                    (id, "duck hold"),
                    &mut duck.hold_ms,
                    0.0..=2000.0,
                    engine::params::DUCK_HOLD_MS,
                    "ms",
                );
                params_changed |= option_slider_row(
                    ui,
                    "Fade up",
                    "How long it takes to come back up after the hold; 0 is at once",
                    (id, "duck fade up"),
                    &mut duck.fade_up_ms,
                    0.0..=3000.0,
                    engine::params::DUCK_FADE_UP_MS,
                    "ms",
                );
                ui.add_space(4.0);
                let (color, status) = if strip.meter.duck < DUCKED_BELOW {
                    (DUCK_COLOR, format!("Ducking now: {:.1} dB", 20.0 * strip.meter.duck.max(1e-6).log10()))
                } else if sources.is_empty() {
                    (Color32::GRAY, "Off: choose the strips that duck this one".to_string())
                } else {
                    (Color32::GRAY, "Not ducking now".to_string())
                };
                ui.label(RichText::new(status).color(color));
            });
        if sources_changed {
            self.strips[index].duck_by = sources.clone();
            self.mixer(&id, MixerCommand::Duck { strip: id.clone(), sources });
        }
        if params_changed {
            self.strips[index].params = params.clone();
            self.mixer(&id, MixerCommand::Strip { id: id.clone(), params });
        }
        if !open {
            self.duck_dialog = None;
        }
    }
}

/// The profiles dialog's own state: the new profile being named.
#[derive(Default)]
struct ProfileDialog {
    name: String,
    from_defaults: bool,
}

impl App {
    /// The profiles: switch to one, delete one, or make a new one.
    fn profile_dialog_ui(&mut self, ctx: &egui::Context) {
        let (active, profiles) = {
            let s = lock(&self.shared);
            (s.snapshot.profile.clone(), s.snapshot.profiles.clone())
        };
        let mut close = false;
        let mut send = None;
        let mut delete = None;
        let Some(dialog) = &mut self.profile_dialog else {
            return;
        };
        widgets::dialog(ctx, "Profiles", |ui| {
            ui.label(
                RichText::new(
                    "A profile holds the settings of every strip and bus. \
                     Switching saves the current settings to the active profile, then takes up the other's.",
                )
                .small()
                .color(Color32::GRAY),
            );
            ui.add_space(6.0);
            egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                for name in &profiles {
                    ui.horizontal(|ui| {
                        let is_active = *name == active;
                        let text = if is_active { RichText::new(name).strong() } else { RichText::new(name) };
                        ui.label(text);
                        if is_active {
                            ui.label(RichText::new("active").small().color(OK_COLOR));
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.add_enabled(!is_active, egui::Button::new("Delete").small()).clicked() {
                                delete = Some(name.clone());
                            }
                            if ui.add_enabled(!is_active, egui::Button::new("Switch").small()).clicked() {
                                send = Some(MixerCommand::SwitchProfile { name: name.clone() });
                            }
                        });
                    });
                }
            });
            ui.separator();
            ui.label(RichText::new("New profile").strong());
            let response = ui.add(egui::TextEdit::singleline(&mut dialog.name).hint_text("Name").desired_width(300.0));
            ui.horizontal(|ui| {
                ui.radio_value(&mut dialog.from_defaults, false, "From the current settings");
                ui.radio_value(&mut dialog.from_defaults, true, "From the defaults");
            });
            let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.horizontal(|ui| {
                let can_create = !dialog.name.trim().is_empty();
                if ui.add_enabled(can_create, egui::Button::new("Create and switch")).clicked() || (enter && can_create)
                {
                    send = Some(MixerCommand::CreateProfile {
                        name: dialog.name.trim().to_string(),
                        from_defaults: dialog.from_defaults,
                    });
                    dialog.name.clear();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });
        });
        if let Some(command) = send {
            // Every strip and bus may change: the mixer's values win.
            self.edited.clear();
            self.send(Request::Mixer { command });
        }
        if let Some(name) = delete {
            self.question = Some(Question::DeleteProfile(name));
        }
        if close || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.profile_dialog = None;
        }
    }
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "input" => "Input (playback device)",
        "output" => "Output (recording device)",
        _ => "Loopback (both)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PortView, Snapshot, StripParams};

    fn key(key: egui::Key, pressed: bool) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: Default::default() }
    }

    fn click(pos: egui::Pos2) -> Vec<egui::Event> {
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        vec![egui::Event::PointerMoved(pos), button(true), button(false)]
    }

    /// The window as it runs: the mixer's state arrives every frame, and the
    /// mixer applies what the window sends.
    struct Harness {
        ctx: egui::Context,
        app: App,
        requests: std::sync::mpsc::Receiver<Request>,
        snapshot: Snapshot,
        time: f64,
        /// Every request the window sent.
        sent: Vec<Request>,
    }

    impl Harness {
        fn new() -> Harness {
            let shared = new_shared(Mode::OnDemand);
            let (link, requests) = Link::detached();
            let strip = StripView {
                id: "H1".into(),
                key: "k1".into(),
                label: "Hardware 1".into(),
                params: StripParams::default(),
                routes: vec!["A1".into()],
                duck_by: Vec::new(),
                meter: Default::default(),
                port: Some(PortView::default()),
            };
            let snapshot = Snapshot { strips: vec![strip], ..Default::default() };
            let app = App {
                shared,
                link,
                generation: u64::MAX,
                strips: Vec::new(),
                buses: Vec::new(),
                cables: Vec::new(),
                capture_devices: Vec::new(),
                render_devices: Vec::new(),
                apps: Vec::new(),
                edited: HashMap::new(),
                meter_display: HashMap::new(),
                holding: false,
                last_frame: Instant::now(),
                question: None,
                new_cable_name: String::new(),
                new_cable_kind: "loopback".into(),
                renaming: None,
                profile_dialog: None,
                duck_dialog: None,
                processing_dialog: None,
                editing_label: None,
                _tray: None,
                start_hidden: false,
            };
            Harness { ctx: egui::Context::default(), app, requests, snapshot, time: 0.0, sent: Vec::new() }
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            for request in self.requests.try_iter() {
                match &request {
                    Request::Mixer { command: MixerCommand::Strip { id, params } } => {
                        if let Some(s) = self.snapshot.strips.iter_mut().find(|s| &s.id == id) {
                            s.params = params.clone();
                        }
                    }
                    Request::Mixer { command: MixerCommand::Duck { strip, sources } } => {
                        if let Some(s) = self.snapshot.strips.iter_mut().find(|s| &s.id == strip) {
                            s.duck_by = sources.clone();
                        }
                    }
                    _ => {}
                }
                self.sent.push(request);
            }
            {
                let mut s = lock(&self.app.shared);
                s.snapshot = self.snapshot.clone();
                s.generation += 1;
            }
            let raw = egui::RawInput {
                events,
                time: Some(self.time),
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(900.0, 700.0))),
                ..Default::default()
            };
            self.time += 0.1;
            let app = &mut self.app;
            let _ = self.ctx.run(raw, |ctx| {
                app.refresh();
                egui::CentralPanel::default().show(ctx, |ui| app.mixer_panel(ui));
                app.dialogs(ctx);
                app.duck_dialog_ui(ctx, true);
                app.processing_dialog_ui(ctx, true);
            });
        }

        fn focused(&self) -> bool {
            self.ctx.memory(|m| m.focused().is_some())
        }

        /// Clicks each point of a grid until one starts editing a value.
        fn find_editable(&mut self, xs: std::ops::Range<i32>, ys: std::ops::Range<i32>) -> egui::Pos2 {
            for y in ys.step_by(3) {
                for x in xs.clone().step_by(5) {
                    let pos = egui::Pos2::new(x as f32, y as f32);
                    self.frame(click(pos));
                    self.frame(vec![]);
                    if self.focused() {
                        return pos;
                    }
                }
            }
            panic!("nothing became editable");
        }
    }

    #[test]
    fn a_typed_level_is_applied_with_enter() {
        let mut h = Harness::new();
        h.frame(vec![]);
        h.frame(vec![]);
        h.find_editable(5..80, 230..300);
        h.frame(vec![egui::Event::Key {
            key: egui::Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL,
        }]);
        // The pointer rests on the box long enough for its tooltip.
        for _ in 0..10 {
            h.frame(vec![]);
        }
        h.frame(vec![egui::Event::Text("-6".into())]);
        h.frame(vec![key(egui::Key::Enter, true)]);
        h.frame(vec![key(egui::Key::Enter, false)]);
        for _ in 0..3 {
            h.frame(vec![]);
        }
        assert!(!h.focused(), "Enter ends the edit");
        assert_eq!(h.snapshot.strips[0].params.gain_db, -6.0, "the mixer got the level");
    }

    #[test]
    fn the_profile_dialog_lists_profiles_and_creates_one() {
        let mut h = Harness::new();
        h.snapshot.profile = "Default".into();
        h.snapshot.profiles = vec!["Default".into(), "Gaming".into()];
        h.app.profile_dialog = Some(ProfileDialog { name: "Streaming".into(), from_defaults: true });
        h.frame(vec![]);
        h.frame(vec![]);
        // Enter in the name field creates the profile.
        let field = h.find_editable(200..700, 100..600);
        eprintln!("name field at {field:?}");
        h.frame(vec![key(egui::Key::Enter, true)]);
        h.frame(vec![]);
        assert!(
            h.sent.iter().any(|r| matches!(r, Request::Mixer { command: MixerCommand::CreateProfile { name, from_defaults: true } } if name == "Streaming")),
            "{:?}",
            h.sent
        );
        h.frame(vec![key(egui::Key::Escape, true)]);
        assert!(h.app.profile_dialog.is_none(), "Escape closes it");
    }

    /// A meter falls away after the sound stops, asking for frames only while
    /// it falls; with nothing moving, the window asks for none.
    #[test]
    fn meters_ask_for_frames_only_while_they_fall() {
        let mut h = Harness::new();
        h.snapshot.strips[0].meter.peak = [1.0; 2];
        h.frame(vec![]);
        let id = h.snapshot.strips[0].id.clone();
        assert_eq!(h.app.meter_display[&id], [1.0; 2]);
        // The sound stops: one new snapshot, then nothing more from the mixer.
        {
            let mut s = lock(&h.app.shared);
            s.snapshot.strips[0].meter.peak = [0.0; 2];
            s.generation += 1;
        }
        let mut frames = 0;
        loop {
            // A frame every 33 ms, as the window asks for while a meter falls.
            h.app.last_frame -= Duration::from_millis(33);
            frames += 1;
            if !h.app.refresh() {
                break;
            }
            assert!(frames < 200, "the meter never settles");
        }
        // 80 dB at 24 dB a second: about 3.3 s, or 100 frames.
        assert!((95..=105).contains(&frames), "{frames} frames");
        assert_eq!(h.app.meter_display[&id], [0.0; 2]);
        h.app.last_frame -= Duration::from_millis(33);
        assert!(!h.app.refresh(), "nothing moves: no more frames");
    }

    #[test]
    fn borders_show_mute_then_ducking_then_solo() {
        let quiet = Meter::default();
        let ducked = Meter { duck: 0.25, ..Default::default() };
        let params = |mute, solo| StripParams { mute, solo, ..Default::default() };
        assert_eq!(strip_highlight(&params(false, false), &quiet), None);
        assert_eq!(strip_highlight(&params(false, false), &ducked), Some(DUCK_COLOR), "purple while ducked");
        assert_eq!(strip_highlight(&params(false, true), &quiet), Some(PENDING_COLOR));
        assert_eq!(strip_highlight(&params(false, true), &ducked), Some(DUCK_COLOR));
        assert_eq!(strip_highlight(&params(true, true), &ducked), Some(ERROR_COLOR), "muted wins");
        let barely = Meter { duck: 0.99, ..Default::default() };
        assert_eq!(strip_highlight(&params(false, false), &barely), None, "a fraction of a dB is not shown");
    }

    /// The ducking dialog sends the sources ticked and the amount typed, and
    /// closes when its strip goes.
    #[test]
    fn the_ducking_dialog_sets_sources_and_amount() {
        let mut h = Harness::new();
        let mut mic = h.snapshot.strips[0].clone();
        (mic.id, mic.key, mic.label) = ("H2".into(), "k2".into(), "Mic".into());
        h.snapshot.strips.push(mic);
        h.frame(vec![]);
        h.app.duck_dialog = Some("k1".into());
        h.frame(vec![]);
        h.frame(vec![]);
        // Tick "Mic (H2)": the list is at the top of the dialog.
        let ducked_by_mic = |sent: &[Request]| {
            sent.iter().any(|r| matches!(r, Request::Mixer { command: MixerCommand::Duck { strip, sources } } if strip == "H1" && sources == &["H2".to_string()]))
        };
        'scan: for y in (150..360).step_by(4) {
            for x in (240..520).step_by(8) {
                h.frame(click(egui::Pos2::new(x as f32, y as f32)));
                h.frame(vec![]);
                if ducked_by_mic(&h.sent) {
                    break 'scan;
                }
            }
        }
        assert!(ducked_by_mic(&h.sent), "{:?}", h.sent);
        assert_eq!(h.app.strips[0].duck_by, ["H2"], "shown at once");
        // Type an amount into the first number box.
        h.sent.clear();
        h.find_editable(300..580, 200..460); // inside the dialog
        h.frame(vec![egui::Event::Text("20".into())]);
        h.frame(vec![key(egui::Key::Enter, true)]);
        h.frame(vec![]);
        let amount = h.sent.iter().find_map(|r| match r {
            Request::Mixer { command: MixerCommand::Strip { id, params } } if id == "H1" => Some(params.duck.amount_db),
            _ => None,
        });
        assert_eq!(amount, Some(20.0), "{:?}", h.sent);
        // An earlier strip goes, so this one's ID shifts: the dialog stays
        // with it, by key, and its changes go to the new ID.
        let mut shifted = h.snapshot.strips.clone();
        shifted.retain(|s| s.id != "H2");
        let mut extra = shifted[0].clone();
        (extra.id, extra.key, extra.label) = ("H0".into(), "k0".into(), "Earlier".into());
        shifted.insert(0, extra);
        h.snapshot.strips = shifted;
        h.frame(vec![]);
        assert_eq!(h.app.duck_dialog.as_deref(), Some("k1"), "still on its strip");

        // The strip goes: so does the dialog.
        h.snapshot.strips.retain(|s| s.id != "H1");
        h.frame(vec![]);
        h.frame(vec![]);
        assert!(h.app.duck_dialog.is_none());
    }

    /// The Gate label opens the gate's options; a typed range is sent. The
    /// compressor's manual curve is sent when ticked.
    #[test]
    fn the_processing_dialogs_set_their_options() {
        let mut h = Harness::new();
        h.frame(vec![]);
        h.frame(vec![]);
        'scan: for y in (250..480).step_by(3) {
            for x in (5..60).step_by(5) {
                h.frame(click(egui::Pos2::new(x as f32, y as f32)));
                h.frame(vec![]);
                if h.app.processing_dialog.is_some() {
                    break 'scan;
                }
            }
        }
        assert_eq!(h.app.processing_dialog, Some(("k1".into(), Processor::Gate)), "the Gate label opened it");
        h.sent.clear();
        // The Range box, first in the dialog, below its title bar (whose close
        // button is at the top right).
        h.find_editable(300..590, 230..460);
        h.frame(vec![egui::Event::Text("30".into())]);
        h.frame(vec![key(egui::Key::Enter, true)]);
        h.frame(vec![]);
        let range = h.sent.iter().find_map(|r| match r {
            Request::Mixer { command: MixerCommand::Strip { params, .. } } => Some(params.gate_options.range_db),
            _ => None,
        });
        assert_eq!(range, Some(30.0), "{:?}", h.sent);

        h.app.processing_dialog = Some(("k1".into(), Processor::Compressor));
        h.sent.clear();
        h.frame(vec![]);
        h.frame(vec![]);
        let manual = |sent: &[Request]| {
            sent.iter().any(|r| matches!(r, Request::Mixer { command: MixerCommand::Strip { params, .. } } if params.comp_options.manual))
        };
        'tick: for y in (226..300).step_by(3) {
            for x in (290..590).step_by(8) {
                h.frame(click(egui::Pos2::new(x as f32, y as f32)));
                h.frame(vec![]);
                if manual(&h.sent) {
                    break 'tick;
                }
            }
        }
        assert!(manual(&h.sent), "{:?}", h.sent);
        assert!(h.app.strips[0].params.comp_options.manual, "shown at once");
    }

    #[test]
    fn escape_keeps_the_old_level() {
        let mut h = Harness::new();
        h.frame(vec![]);
        h.find_editable(5..80, 230..300);
        h.frame(vec![egui::Event::Text("9".into())]);
        h.frame(vec![key(egui::Key::Escape, true)]);
        h.frame(vec![]);
        h.frame(vec![]);
        assert!(!h.focused());
        assert_eq!(h.snapshot.strips[0].params.gain_db, 0.0);
    }

    #[test]
    fn a_new_name_is_kept_by_clicking_away() {
        let mut h = Harness::new();
        h.frame(vec![]);
        // As after double-clicking the name.
        h.app.editing_label = Some(("H1".into(), "Hardware 1".into()));
        h.frame(vec![]);
        h.frame(vec![]);
        assert!(h.focused(), "the name field takes the keyboard");
        h.frame(vec![egui::Event::Text("Mic".into())]);
        h.frame(click(egui::Pos2::new(600.0, 650.0)));
        for _ in 0..3 {
            h.frame(vec![]);
        }
        assert!(!h.focused(), "clicking away ends the edit");
        assert!(h.app.editing_label.is_none());
        h.frame(vec![]);
        let sent = &h.sent;
        assert!(
            sent.iter()
                .any(|r| matches!(r, Request::Mixer { command: MixerCommand::Label { label, .. } } if label == "Mic")),
            "{sent:?}"
        );
    }

    #[test]
    fn a_typed_knob_value_is_applied_by_clicking_away() {
        let mut h = Harness::new();
        h.frame(vec![]);
        h.frame(vec![]);
        // The knob boxes are at the right of the strip, below the fader.
        let at = h.find_editable(95..140, 330..420);
        eprintln!("editing at {at:?}");
        h.frame(vec![egui::Event::Key {
            key: egui::Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL,
        }]);
        h.frame(vec![egui::Event::Text("4".into())]);
        for _ in 0..10 {
            h.frame(vec![]);
        }
        h.frame(click(egui::Pos2::new(600.0, 650.0)));
        for _ in 0..3 {
            h.frame(vec![]);
        }
        assert!(!h.focused(), "clicking away ends the edit");
        let p = &h.snapshot.strips[0].params;
        assert!(p.eq.contains(&4.0) || p.gate == 4.0 || p.comp == 4.0, "{p:?}");
    }
}
