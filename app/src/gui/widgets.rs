//! The mixer's controls: faders with meters, routing toggles, small knobs
//! (drawn as sliders), device pickers and status lines.

use std::ops::RangeInclusive;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2};

use super::{ERROR_COLOR, OK_COLOR, PENDING_COLOR, tone_color};
use crate::protocol::{AppView, DeviceView, PortView, Tone};

pub const STRIP_WIDTH: f32 = 128.0;
/// The height of a device picker, kept free on strips without one so the
/// faders line up.
pub const PICKER_HEIGHT: f32 = 46.0;
const FADER_HEIGHT: f32 = 140.0;
const MIN_DB: f32 = -60.0;

/// One status line: a dot in the tone's colour, the text, and a link after
/// it. Returns whether the link was clicked.
pub fn status_line(ui: &mut egui::Ui, tone: Tone, text: &str, link: Option<&str>) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.5, tone_color(tone));
        ui.label(RichText::new(text).color(Color32::from_gray(190)));
        if let Some(link) = link {
            clicked = ui.link(link).clicked();
        }
    });
    clicked
}

/// A strip's or bus's frame, its border in `highlight` when given: the colour
/// of its mute or solo button while that is on.
pub fn strip_frame(ui: &mut egui::Ui, highlight: Option<Color32>, content: impl FnOnce(&mut egui::Ui)) {
    // Every frame has a border of the same width, so lighting one up moves nothing.
    let color = highlight.unwrap_or(ui.visuals().widgets.noninteractive.bg_stroke.color);
    egui::Frame::group(ui.style())
        .fill(Color32::from_gray(32))
        .inner_margin(egui::Margin::same(6))
        .stroke(Stroke::new(2.0_f32, color))
        .show(ui, content);
}

/// The column after the last strip or bus, with a button that adds one.
/// Returns whether it was clicked.
pub fn add_column(ui: &mut egui::Ui, text: &str, hint: &str) -> bool {
    ui.vertical(|ui| {
        ui.set_width(STRIP_WIDTH * 0.6);
        ui.add_space(24.0);
        ui.add_sized([STRIP_WIDTH * 0.6, 48.0], egui::Button::new(text)).on_hover_text(hint).clicked()
    })
    .inner
}

/// `text` shortened to `max` characters with an ellipsis.
pub fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}\u{2026}", text.chars().take(max - 1).collect::<String>())
    }
}

/// The device picker of a hardware strip or bus, with its exclusive-mode
/// switch. Returns the new choice when the user changes it.
/// A strip may also take the output of one of `buses`, each a choice (`bus:<key>`)
/// and its name, or capture one of `apps` (`app:<exe>`).
pub fn device_picker(
    ui: &mut egui::Ui,
    id: &str,
    port: &PortView,
    devices: &[DeviceView],
    heading: &str,
    buses: &[(String, String)],
    apps: &[AppView],
) -> Option<(String, bool)> {
    let mut endpoint = port.endpoint.clone();
    let mut exclusive = port.exclusive;
    let name_of = |endpoint: &str| -> String {
        match endpoint {
            "" => "No device".into(),
            devices::DEFAULT_ENDPOINT => "Default device".into(),
            id => devices
                .iter()
                .find(|d| d.id == id)
                .map(|d| d.name.clone())
                .or_else(|| buses.iter().find(|(choice, _)| choice == id).map(|(_, name)| format!("From {name}")))
                .or_else(|| id.strip_prefix(devices::APP_ENDPOINT).map(devices::app_display_name))
                .unwrap_or_else(|| "(not connected)".into()),
        }
    };
    let selected = name_of(&endpoint);
    egui::ComboBox::from_id_salt(("device", id))
        .selected_text(ellipsize(&selected, 18))
        .width(STRIP_WIDTH)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut endpoint, String::new(), "No device");
            let default = devices.iter().find(|d| d.is_default).map(|d| format!("Default device ({})", d.name));
            ui.selectable_value(
                &mut endpoint,
                devices::DEFAULT_ENDPOINT.to_string(),
                default.unwrap_or("Default device".into()),
            );
            ui.separator();
            if !devices.is_empty() {
                ui.label(RichText::new(heading).small().color(Color32::GRAY));
            }
            for device in devices {
                ui.selectable_value(&mut endpoint, device.id.clone(), &device.name);
            }
            if !buses.is_empty() {
                ui.separator();
                ui.label(RichText::new("Mixer outputs").small().color(Color32::GRAY));
                for (choice, name) in buses {
                    ui.selectable_value(&mut endpoint, choice.clone(), format!("From {name}"));
                }
            }
            if !apps.is_empty() || endpoint.starts_with(devices::APP_ENDPOINT) {
                ui.separator();
                ui.label(RichText::new("Applications").small().color(Color32::GRAY));
                for app in apps {
                    let text = if app.active { format!("{} \u{00b7} playing", app.name) } else { app.name.clone() };
                    ui.selectable_value(&mut endpoint, format!("{}{}", devices::APP_ENDPOINT, app.exe), text);
                }
                // The chosen application, while it is not running.
                if let Some(exe) = port.endpoint.strip_prefix(devices::APP_ENDPOINT)
                    && !apps.iter().any(|a| a.exe.eq_ignore_ascii_case(exe))
                {
                    let text = format!("{} \u{00b7} not running", devices::app_display_name(exe));
                    ui.selectable_value(&mut endpoint, port.endpoint.clone(), text);
                }
            }
        })
        .response
        .on_hover_text(selected);
    // Exclusive mode is for devices, not buses or applications.
    let device = !endpoint.starts_with(crate::mixer::BUS_SOURCE) && !endpoint.starts_with(devices::APP_ENDPOINT);
    if !device {
        exclusive = false;
    }
    ui.add_enabled_ui(!endpoint.is_empty() && device, |ui| {
        ui.checkbox(&mut exclusive, RichText::new("Exclusive").small())
            .on_hover_text("Use the device alone, at its lowest latency. Other apps cannot use it meanwhile.");
    });
    (endpoint != port.endpoint || exclusive != port.exclusive).then_some((endpoint, exclusive))
}

/// A fader with the strip's stereo meter beside it and the gain below.
/// Double-click resets it to 0 dB. Returns whether the gain changed.
pub fn fader(ui: &mut egui::Ui, id: &str, gain_db: &mut f32, peak: [f32; 2], reduction: f32) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        meter(ui, peak, reduction);
        ui.spacing_mut().slider_width = FADER_HEIGHT;
        let response =
            ui.add(egui::Slider::new(gain_db, MIN_DB..=12.0).vertical().show_value(false).trailing_fill(true));
        if response.double_clicked() {
            *gain_db = 0.0;
            changed = true;
        }
        changed |= response.changed();
        let _ = id;
        scale(ui);
    });
    let typed = NumberBox {
        id: egui::Id::new((id, "gain")),
        width: 64.0,
        range: MIN_DB..=12.0,
        speed: 0.1,
        format: &format_db,
        edit_text: &|db| if db <= MIN_DB { "-inf".into() } else { format!("{db:.1}") },
        parse: &parse_db,
        hint: "Click to type a level in dB, or drag",
    }
    .show(ui, gain_db);
    changed | typed
}

/// A value shown as text that can be typed over or dragged. Click it to type
/// a value; Enter, clicking anywhere else, or leaving the field applies it,
/// and Escape keeps the old one. Dragging moves it by `speed` a point.
///
/// The edit is kept in this widget's own state under a fixed ID, so it ends
/// however the field loses the keyboard, rather than relying on the text
/// field noticing.
pub struct NumberBox<'a> {
    pub id: egui::Id,
    pub width: f32,
    pub range: RangeInclusive<f32>,
    pub speed: f32,
    /// The value as shown.
    pub format: &'a dyn Fn(f32) -> String,
    /// The value as the field starts out when clicked.
    pub edit_text: &'a dyn Fn(f32) -> String,
    pub parse: &'a dyn Fn(&str) -> Option<f64>,
    pub hint: &'a str,
}

impl NumberBox<'_> {
    /// Returns whether the value changed.
    pub fn show(self, ui: &mut egui::Ui, value: &mut f32) -> bool {
        let size = Vec2::new(self.width, ui.spacing().interact_size.y);
        let edit_id = self.id.with("edit");
        let clamp = |v: f32| v.clamp(*self.range.start(), *self.range.end());
        if let Some(mut text) = ui.data(|d| d.get_temp::<String>(self.id)) {
            let response = ui.add_sized(
                size,
                egui::TextEdit::singleline(&mut text).id(edit_id).horizontal_align(egui::Align::Center),
            );
            let (enter, escape, pressed_elsewhere) = ui.input(|i| {
                let pressed_elsewhere =
                    i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|pos| !response.rect.contains(pos));
                (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape), pressed_elsewhere)
            });
            if enter || escape || pressed_elsewhere || response.lost_focus() {
                ui.data_mut(|d| d.remove::<String>(self.id));
                ui.memory_mut(|m| m.surrender_focus(edit_id));
                let typed = if escape { None } else { (self.parse)(&text).map(|v| clamp(v as f32)) };
                return match typed {
                    Some(v) if v != *value => {
                        *value = v;
                        true
                    }
                    _ => false,
                };
            }
            ui.data_mut(|d| d.insert_temp(self.id, text));
            return false;
        }
        let response = ui
            .add_sized(size, egui::Button::new((self.format)(*value)).sense(Sense::click_and_drag()))
            .on_hover_text(self.hint);
        if response.clicked() {
            let text = (self.edit_text)(*value);
            // Start with the whole value selected, so typing replaces it.
            let mut state = egui::text_edit::TextEditState::load(ui.ctx(), edit_id).unwrap_or_default();
            state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                egui::text::CCursor::new(text.chars().count()),
            )));
            state.store(ui.ctx(), edit_id);
            ui.data_mut(|d| d.insert_temp(self.id, text));
            ui.memory_mut(|m| m.request_focus(edit_id));
            ui.ctx().request_repaint();
            return false;
        }
        if response.dragged() {
            let delta = response.drag_delta();
            let dragged = clamp(*value + (delta.x - delta.y) * self.speed);
            if dragged != *value {
                *value = dragged;
                return true;
            }
        }
        false
    }
}

/// A fader level as shown: `-6.0 dB`, `+3.0 dB`, or `-inf dB` at the bottom.
pub fn format_db(db: f32) -> String {
    if db <= MIN_DB { "-inf dB".to_string() } else { format!("{db:+.1} dB") }
}

/// A level as typed: `-6`, `-6 dB`, `+3.5db`, or `-inf`. Values out of
/// range are brought into it by the control.
pub fn parse_db(text: &str) -> Option<f64> {
    let text = text.trim().trim_end_matches(['d', 'D', 'b', 'B']).trim();
    match text.to_ascii_lowercase().as_str() {
        "-inf" | "inf" | "-∞" => Some(MIN_DB as f64),
        _ => parse_number(text),
    }
}

/// A number as typed, with or without a trailing `dB`.
pub fn parse_number(text: &str) -> Option<f64> {
    let text = text.trim().trim_end_matches(['d', 'D', 'b', 'B']).trim();
    text.trim_start_matches('+').parse::<f64>().ok().filter(|v| v.is_finite())
}

/// A balance as shown: `C`, `L40` or `R25` (percent of the way to one side).
pub fn format_pan(percent: f64) -> String {
    match percent.round() as i32 {
        0 => "C".into(),
        n if n < 0 => format!("L{}", -n),
        n => format!("R{n}"),
    }
}

/// A balance as typed: `C`, `L40`, `R25`, or a number from -100 (left) to 100.
pub fn parse_pan(text: &str) -> Option<f64> {
    let text = text.trim().trim_end_matches('%').trim().to_ascii_uppercase();
    let number = |s: &str| s.trim().parse::<f64>().ok().filter(|v| v.is_finite());
    match text.chars().next()? {
        'C' if text.len() == 1 => Some(0.0),
        'L' => number(&text[1..]).map(|n| -n.abs()),
        'R' => number(&text[1..]).map(f64::abs),
        _ => number(&text),
    }
}

/// Maps a level to the meter's height, -60 dBFS to 0 dBFS.
fn level_fraction(level: f32) -> f32 {
    if level <= 0.0 {
        return 0.0;
    }
    ((20.0 * level.log10() - MIN_DB) / -MIN_DB).clamp(0.0, 1.0)
}

/// Two bars, left and right, green to -12 dBFS, yellow to -3 dBFS, red above;
/// a gain-reduction bar beside them when processing reduces the level.
fn meter(ui: &mut egui::Ui, peak: [f32; 2], reduction: f32) {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(22.0, FADER_HEIGHT), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, Color32::from_gray(18));
    let bar_width = 7.0;
    for (channel, level) in peak.iter().enumerate() {
        let x = rect.left() + 2.0 + channel as f32 * (bar_width + 1.0);
        let full = Rect::from_min_max(Pos2::new(x, rect.top() + 2.0), Pos2::new(x + bar_width, rect.bottom() - 2.0));
        let height = full.height() * level_fraction(*level);
        // Colour segments by level.
        for (from_db, to_db, color) in
            [(-60.0, -12.0, OK_COLOR), (-12.0, -3.0, PENDING_COLOR), (-3.0, 0.0, ERROR_COLOR)]
        {
            let from = full.height() * ((from_db - MIN_DB) / -MIN_DB);
            let to = (full.height() * ((to_db - MIN_DB) / -MIN_DB)).min(height);
            if to > from {
                let segment = Rect::from_min_max(
                    Pos2::new(full.left(), full.bottom() - to),
                    Pos2::new(full.right(), full.bottom() - from),
                );
                painter.rect_filled(segment, 0.0, color);
            }
        }
    }
    if reduction < 0.999 {
        let x = rect.right() - 4.0;
        let depth = (-20.0 * reduction.max(1e-3).log10() / 24.0).clamp(0.0, 1.0) * (rect.height() - 4.0);
        painter.rect_filled(
            Rect::from_min_max(Pos2::new(x, rect.top() + 2.0), Pos2::new(x + 3.0, rect.top() + 2.0 + depth)),
            0.0,
            Color32::from_rgb(90, 160, 230),
        );
    }
    let db = |l: f32| if l > 0.0 { format!("{:.1}", 20.0 * l.log10()) } else { "-inf".into() };
    response.on_hover_text(format!("Peak L {} dBFS, R {} dBFS", db(peak[0]), db(peak[1])));
}

/// dB marks beside a fader.
fn scale(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(22.0, FADER_HEIGHT), Sense::hover());
    let painter = ui.painter();
    // The slider maps -60..+12 dB linearly over its length, inset by its knob.
    let inset = 8.0;
    for db in [12.0, 0.0, -12.0, -24.0, -36.0, -48.0, -60.0f32] {
        let t = (db - MIN_DB) / (12.0 - MIN_DB);
        let y = rect.bottom() - inset - t * (rect.height() - 2.0 * inset);
        painter.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.left() + 4.0, y)],
            Stroke::new(1.0_f32, Color32::GRAY),
        );
        painter.text(
            Pos2::new(rect.left() + 6.0, y),
            Align2::LEFT_CENTER,
            format!("{db:.0}"),
            FontId::proportional(9.0),
            Color32::GRAY,
        );
    }
}

/// The balance control; double-click centres it.
pub fn pan(ui: &mut egui::Ui, id: &str, pan: &mut f32) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().slider_width = STRIP_WIDTH - 44.0;
        let response = ui
            .add(egui::Slider::new(pan, -1.0..=1.0).show_value(false))
            .on_hover_text("Balance (double-click to centre)");
        if response.double_clicked() {
            *pan = 0.0;
            changed = true;
        }
        changed |= response.changed();
        let mut percent = *pan * 100.0;
        let typed = NumberBox {
            id: egui::Id::new((id, "pan")),
            width: 36.0,
            range: -100.0..=100.0,
            speed: 1.0,
            format: &|p| format_pan(p as f64),
            edit_text: &|p| format_pan(p as f64),
            parse: &parse_pan,
            hint: "Click to type a balance: C, L40, R25",
        }
        .show(ui, &mut percent);
        if typed {
            *pan = percent / 100.0;
            changed = true;
        }
    });
    changed
}

/// A small toggle button, filled with `color` when on.
pub fn toggle(ui: &mut egui::Ui, text: &str, on: bool, color: Color32) -> egui::Response {
    let label = RichText::new(text).small().strong().color(if on { Color32::BLACK } else { Color32::from_gray(200) });
    let button =
        egui::Button::new(label).fill(if on { color } else { Color32::from_gray(55) }).min_size(Vec2::new(26.0, 18.0));
    ui.add(button)
}

/// A labelled small slider for a processing setting; double-click resets it.
/// `id` is the strip's or bus's, to keep each row's typing apart.
pub fn knob_row(
    ui: &mut egui::Ui,
    id: &str,
    label: &str,
    value: &mut f32,
    range: RangeInclusive<f32>,
    unit: &str,
) -> bool {
    knob_row_with_options(ui, id, label, value, range, unit, None).0
}

/// A knob row whose label opens more options: `options` is whether they are
/// changed from their defaults, which shows the label in yellow. Returns
/// whether the value changed and whether the label was clicked.
pub fn knob_row_with_options(
    ui: &mut egui::Ui,
    id: &str,
    label: &str,
    value: &mut f32,
    range: RangeInclusive<f32>,
    unit: &str,
    options: Option<bool>,
) -> (bool, bool) {
    let mut changed = false;
    let mut clicked = false;
    ui.horizontal(|ui| {
        match options {
            None => {
                ui.add_sized([38.0, 14.0], egui::Label::new(RichText::new(label).small()));
            }
            Some(changed_options) => {
                let text = RichText::new(label).small();
                let text = if changed_options { text.color(super::LEMON) } else { text.underline() };
                let hint = format!("{label} options{}", if changed_options { " (changed)" } else { "" });
                clicked =
                    ui.add_sized([38.0, 14.0], egui::Button::new(text).frame(false)).on_hover_text(hint).clicked();
            }
        }
        ui.spacing_mut().slider_width = STRIP_WIDTH - 88.0;
        let default = if *range.start() < 0.0 && *range.end() > 0.0 { 0.0 } else { *range.start() };
        let default = if label == "Ceiling" { -1.0 } else { default };
        let response = ui.add(egui::Slider::new(value, range.clone()).show_value(false));
        if response.double_clicked() {
            *value = default;
            changed = true;
        }
        changed |= response.changed();
        let decibels = !unit.is_empty();
        let format = move |v: f32| if decibels { format!("{v:+.1}") } else { format!("{v:.1}") };
        changed |= NumberBox {
            id: egui::Id::new((id, label)),
            width: 38.0,
            range,
            speed: 0.1,
            format: &format,
            edit_text: &format,
            parse: &parse_number,
            hint: if decibels {
                "Click to type a value in dB, or drag"
            } else {
                "Click to type a value, 0 (off) to 10"
            },
        }
        .show(ui, value);
    });
    (changed, clicked)
}

/// A hardware port's stream: its rate and drift correction, or why it is
/// not running.
pub fn port_status(ui: &mut egui::Ui, port: &PortView) {
    let (color, text, hover) = match port.state.as_str() {
        "running" if port.endpoint.starts_with(devices::APP_ENDPOINT) => (
            OK_COLOR,
            format!("Capturing {}", port.device_name),
            format!("Buffer {:.1} ms, underruns {}, overruns {}", port.fill_ms, port.underruns, port.overruns),
        ),
        "running" => (
            OK_COLOR,
            format!("{:.1} kHz \u{00b7} {:+.0} ppm", port.rate as f64 / 1000.0, port.correction_ppm),
            format!(
                "{}\nBuffer {:.1} ms, drift correction {:+.1} ppm\nUnderruns {}, overruns {}",
                port.device_name, port.fill_ms, port.correction_ppm, port.underruns, port.overruns
            ),
        ),
        "opening" => (PENDING_COLOR, "Opening\u{2026}".into(), String::new()),
        "failed" if port.endpoint.starts_with(devices::APP_ENDPOINT) => {
            let name = devices::app_display_name(&port.endpoint[devices::APP_ENDPOINT.len()..]);
            (PENDING_COLOR, format!("Waiting for {name}"), port.error.clone())
        }
        "failed" => (ERROR_COLOR, "Unavailable".into(), port.error.clone()),
        "bus" => (
            Color32::GRAY,
            format!("From {}", port.device_name),
            "This strip takes that bus's output, 1 ms later, with no virtual device in between.".into(),
        ),
        _ => (Color32::GRAY, "No device".into(), String::new()),
    };
    let response = ui.label(RichText::new(text).small().color(color));
    if !hover.is_empty() {
        response.on_hover_text(hover);
    }
}

/// A dialog in the middle of the window.
pub fn dialog(ctx: &egui::Context, title: &str, content: impl FnOnce(&mut egui::Ui)) {
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .max_width(460.0)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, content);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_scale() {
        assert_eq!(level_fraction(0.0), 0.0);
        assert_eq!(level_fraction(1.0), 1.0);
        assert!((level_fraction(0.001) - 0.0).abs() < 1e-6, "-60 dBFS is the bottom");
        assert!((level_fraction(0.0316) - 0.5).abs() < 0.01, "-30 dBFS is half way");
        assert_eq!(ellipsize("Headset Microphone", 8), "Headset\u{2026}");
        assert_eq!(ellipsize("Mic", 8), "Mic");
    }

    #[test]
    fn typed_values_are_read_as_people_write_them() {
        assert_eq!(parse_db("-6"), Some(-6.0));
        assert_eq!(parse_db(" +3.5 dB "), Some(3.5));
        assert_eq!(parse_db("-inf"), Some(-60.0));
        assert_eq!(parse_db("loud"), None);
        assert_eq!(parse_db("NaN"), None);
        assert_eq!(format_db(-60.0), "-inf dB");
        assert_eq!(format_db(2.0), "+2.0 dB");
        assert_eq!(parse_pan("c"), Some(0.0));
        assert_eq!(parse_pan("L40"), Some(-40.0));
        assert_eq!(parse_pan("r 25%"), Some(25.0));
        assert_eq!(parse_pan("-10"), Some(-10.0));
        assert_eq!(parse_pan("left"), None);
        assert_eq!([format_pan(0.2), format_pan(-40.0), format_pan(25.0)], ["C", "L40", "R25"]);
        assert_eq!(parse_number("4.5"), Some(4.5));
    }
}

#[cfg(test)]
mod editing_tests {
    use super::*;

    fn run(ctx: &egui::Context, events: Vec<egui::Event>, gain: &mut f32) -> bool {
        let raw = egui::RawInput {
            events,
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
            ..Default::default()
        };
        let mut changed = false;
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        strip_frame(ui, None, |ui| {
                            ui.vertical(|ui| {
                                ui.set_width(STRIP_WIDTH);
                                changed |= fader(ui, "H1", gain, [0.0; 2], 1.0);
                            });
                        });
                    });
                });
            });
        });
        changed
    }

    #[test]
    fn typing_a_level_applies_it() {
        let ctx = egui::Context::default();
        let mut gain = 0.0;
        run(&ctx, vec![], &mut gain);
        // The level box is under the fader: find it by scanning for where a click starts editing.
        let mut found = None;
        'scan: for y in (150..260).step_by(4) {
            for x in (10..120).step_by(6) {
                let pos = Pos2::new(x as f32, y as f32);
                let click = vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: Default::default(),
                    },
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: Default::default(),
                    },
                ];
                let before = gain;
                run(&ctx, click, &mut gain);
                run(&ctx, vec![], &mut gain);
                gain = before;
                if ctx.memory(|m| m.focused().is_some()) {
                    found = Some(pos);
                    break 'scan;
                }
            }
        }
        let pos = found.expect("a click focuses the level box");
        eprintln!("level box at {pos:?}");
        run(
            &ctx,
            vec![egui::Event::Key {
                key: egui::Key::A,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::CTRL,
            }],
            &mut gain,
        );
        run(&ctx, vec![egui::Event::Text("-6".into())], &mut gain);
        let mut changed = false;
        for events in [
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Default::default(),
            }],
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: Default::default(),
            }],
            vec![],
            vec![],
        ] {
            changed |= run(&ctx, events, &mut gain);
        }
        assert!(ctx.memory(|m| m.focused().is_none()), "Enter ends the edit");
        assert!(changed, "the change is reported");
        assert_eq!(gain, -6.0);
    }
}
