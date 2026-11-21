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
use crate::protocol::{AppView, BusView, CableView, DeviceView, MixerCommand, Request, StripView, Tone};

const OK_COLOR: Color32 = Color32::from_rgb(64, 180, 80);
const PENDING_COLOR: Color32 = Color32::from_rgb(230, 160, 30);
const ERROR_COLOR: Color32 = Color32::from_rgb(220, 70, 60);
const LEMON: Color32 = Color32::from_rgb(247, 210, 62);
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

fn show_window() {
    let hwnd = WINDOW.load(Ordering::SeqCst);
    if hwnd != 0 {
        // SAFETY: shows and focuses this process's own window.
        unsafe {
            ShowWindow(hwnd as _, SW_SHOW);
            SetForegroundWindow(hwnd as _);
        }
    }
    link::repaint();
}

fn hide_window() {
    let hwnd = WINDOW.load(Ordering::SeqCst);
    if hwnd != 0 {
        // SAFETY: hides this process's own window.
        unsafe { ShowWindow(hwnd as _, SW_HIDE) };
    }
}

pub fn run_gui(host: &Arc<MixerHost>) {
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

    let icon = egui::IconData { rgba: crate::icon::lemon_rgba(64), width: 64, height: 64 };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Lemon Mix")
            .with_inner_size([1180.0, 880.0])
            .with_min_inner_size([720.0, 480.0])
            .with_icon(Arc::new(icon)),
        ..Default::default()
    };
    let result = eframe::run_native("Lemon Mix", options, Box::new(move |cc| Ok(Box::new(App::new(cc, shared)))));
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
    editing_label: Option<(String, String)>,
    _tray: Option<tray_icon::TrayIcon>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, shared: SharedState) -> App {
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
            editing_label: None,
            _tray: tray,
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

    fn send(&self, request: Request) {
        self.link.send(request);
    }

    fn mixer(&mut self, id: &str, command: MixerCommand) {
        self.edited.insert(id.to_string(), Instant::now());
        self.send(Request::Mixer { command });
    }

    /// Takes the latest snapshot, keeping the values of controls the user is
    /// changing.
    fn refresh(&mut self) {
        let s = lock(&self.shared);
        if s.generation == self.generation {
            return;
        }
        self.generation = s.generation;
        let snapshot = &s.snapshot;
        let now = Instant::now();
        let held = |id: &str, edited: &HashMap<String, Instant>| edited.get(id).is_some_and(|t| now - *t < EDIT_HOLD);
        let mut strips = snapshot.strips.clone();
        for strip in &mut strips {
            if held(&strip.id, &self.edited)
                && let Some(local) = self.strips.iter().find(|s| s.id == strip.id)
            {
                strip.params = local.params.clone();
                strip.routes = local.routes.clone();
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
        // Meters: a level rises at once and falls at about 24 dB a second.
        let elapsed = (now - self.last_frame).as_secs_f32().min(0.2);
        let fall = 10f32.powf(-24.0 * elapsed / 20.0);
        self.last_frame = now;
        for (id, peak) in
            self.strips.iter().map(|s| (&s.id, s.meter.peak)).chain(self.buses.iter().map(|b| (&b.id, b.meter.peak)))
        {
            let shown = self.meter_display.entry(id.clone()).or_insert([0.0; 2]);
            for channel in 0..2 {
                shown[channel] = peak[channel].max(shown[channel] * fall);
            }
        }
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
        self.refresh();
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

        let modal = self.question.is_some() || !lock(&self.shared).errors.is_empty() || self.renaming.is_some();
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
        ctx.request_repaint_after(Duration::from_millis(33));
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
                ui.add_enabled_ui(!busy, |ui| self.options_menu(ui, mode, state));
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
                    widgets::strip_frame(ui, |ui| self.strip_ui(ui, index));
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
                    widgets::strip_frame(ui, |ui| self.bus_ui(ui, index));
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
            });
            ui.add_space(2.0);
            changed |= widgets::knob_row(ui, &strip.id, "Bass", &mut params.eq[0], -12.0..=12.0, "dB");
            changed |= widgets::knob_row(ui, &strip.id, "Mid", &mut params.eq[1], -12.0..=12.0, "dB");
            changed |= widgets::knob_row(ui, &strip.id, "Treble", &mut params.eq[2], -12.0..=12.0, "dB");
            changed |= widgets::knob_row(ui, &strip.id, "Gate", &mut params.gate, 0.0..=10.0, "");
            changed |= widgets::knob_row(ui, &strip.id, "Comp", &mut params.comp, 0.0..=10.0, "");
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
            _ => {}
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
                last_frame: Instant::now(),
                question: None,
                new_cable_name: String::new(),
                new_cable_kind: "loopback".into(),
                renaming: None,
                editing_label: None,
                _tray: None,
            };
            Harness { ctx: egui::Context::default(), app, requests, snapshot, time: 0.0, sent: Vec::new() }
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            for request in self.requests.try_iter() {
                if let Request::Mixer { command: MixerCommand::Strip { params, .. } } = &request {
                    self.snapshot.strips[0].params = params.clone();
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
