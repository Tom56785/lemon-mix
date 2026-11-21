//! The mixer's controls: faders with meters, routing toggles, small knobs
//! (drawn as sliders), device pickers and status lines.

use std::ops::RangeInclusive;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2};

use super::{ERROR_COLOR, OK_COLOR, PENDING_COLOR, tone_color};
use crate::protocol::{DeviceView, PortView, Tone};

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

/// A strip's or bus's frame.
pub fn strip_frame(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).fill(Color32::from_gray(32)).inner_margin(egui::Margin::same(6)).show(ui, content);
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
pub fn device_picker(ui: &mut egui::Ui, id: &str, port: &PortView, devices: &[DeviceView]) -> Option<(String, bool)> {
    let mut endpoint = port.endpoint.clone();
    let mut exclusive = port.exclusive;
    let name_of = |endpoint: &str| -> String {
        match endpoint {
            "" => "No device".into(),
            devices::DEFAULT_ENDPOINT => "Default device".into(),
            id => {
                devices.iter().find(|d| d.id == id).map(|d| d.name.clone()).unwrap_or_else(|| "(not connected)".into())
            }
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
            for device in devices {
                ui.selectable_value(&mut endpoint, device.id.clone(), &device.name);
            }
        })
        .response
        .on_hover_text(selected);
    ui.add_enabled_ui(!endpoint.is_empty(), |ui| {
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
    let text = if *gain_db <= MIN_DB { "-inf dB".to_string() } else { format!("{:+.1} dB", gain_db) };
    ui.label(RichText::new(text).monospace());
    changed
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
    let _ = id;
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new("L").small());
        ui.spacing_mut().slider_width = STRIP_WIDTH - 36.0;
        let response = ui
            .add(egui::Slider::new(pan, -1.0..=1.0).show_value(false))
            .on_hover_text("Balance (double-click to centre)");
        if response.double_clicked() {
            *pan = 0.0;
            changed = true;
        }
        changed |= response.changed();
        ui.label(RichText::new("R").small());
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
pub fn knob_row(ui: &mut egui::Ui, label: &str, value: &mut f32, range: RangeInclusive<f32>, unit: &str) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.add_sized([38.0, 14.0], egui::Label::new(RichText::new(label).small()));
        ui.spacing_mut().slider_width = STRIP_WIDTH - 82.0;
        let default = if *range.start() < 0.0 && *range.end() > 0.0 { 0.0 } else { *range.start() };
        let default = if label == "Ceiling" { -1.0 } else { default };
        let response = ui.add(egui::Slider::new(value, range).show_value(false));
        if response.double_clicked() {
            *value = default;
            changed = true;
        }
        changed |= response.changed();
        let text = if unit.is_empty() { format!("{value:.1}") } else { format!("{value:+.0}") };
        ui.label(RichText::new(text).small().monospace());
    });
    changed
}

/// A hardware port's stream: its rate and drift correction, or why it is
/// not running.
pub fn port_status(ui: &mut egui::Ui, port: &PortView) {
    let (color, text, hover) = match port.state.as_str() {
        "running" => (
            OK_COLOR,
            format!("{:.1} kHz \u{00b7} {:+.0} ppm", port.rate as f64 / 1000.0, port.correction_ppm),
            format!(
                "{}\nBuffer {:.1} ms, drift correction {:+.1} ppm\nUnderruns {}, overruns {}",
                port.device_name, port.fill_ms, port.correction_ppm, port.underruns, port.overruns
            ),
        ),
        "opening" => (PENDING_COLOR, "Opening\u{2026}".into(), String::new()),
        "failed" => (ERROR_COLOR, "Unavailable".into(), port.error.clone()),
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
}
