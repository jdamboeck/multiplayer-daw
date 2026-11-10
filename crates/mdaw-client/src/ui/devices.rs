//! The device panel of the selected track.

use egui::{
    Align, Align2, Color32, ComboBox, FontId, Layout, Response, RichText, Sense, Shape, Stroke, Ui, pos2,
    vec2,
};
use mdaw_dsp::pocket::{self, ParamInfo, Scale};
use mdaw_model::{BuiltinKind, Device, DeviceKind, Op, TrackId};

use crate::{DawApp, theme};

/// Parameters by section, as indices into `pocket::PARAMS`.
const SECTIONS: [(&str, &[usize]); 5] = [
    ("Oscillators", &[0, 1, 2, 3, 4, 5, 6]),
    ("Amp envelope", &[7, 8, 9, 10]),
    ("Filter", &[11, 12, 13, 18]),
    ("Filter envelope", &[14, 15, 16, 17]),
    ("Output", &[19]),
];

pub fn toolbar(app: &mut DawApp, ui: &mut Ui) {
    let name = app
        .sel
        .track
        .and_then(|id| app.session.project().track(id))
        .map(|t| t.shell.name.clone());
    match name {
        Some(name) => ui.label(name),
        None => ui.label(RichText::new("No track selected").color(theme::TEXT_DIM)),
    };
}

pub fn show(app: &mut DawApp, ui: &mut Ui) {
    let project = app.session.project().clone();
    let Some((track_id, track)) = app.sel.track.and_then(|id| Some((id, project.track(id)?))) else {
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("Select a track to see its devices.").color(theme::TEXT_DIM));
        });
        return;
    };
    egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
        ui.add_space(4.0);
        for device in &track.content.devices {
            ui.push_id(device.id.0.0, |ui| match &device.kind {
                DeviceKind::Builtin {
                    kind: BuiltinKind::Pocket,
                    ..
                } => pocket_panel(app, ui, track_id, device),
                DeviceKind::Clap { plugin_id, .. } => {
                    ui.label(format!("CLAP plugin {plugin_id}"));
                }
            });
        }
    });
}

fn pocket_panel(app: &mut DawApp, ui: &mut Ui, track: TrackId, device: &Device) {
    // Two rows of sections, so the panel fits a window of ordinary width.
    const ROWS: [&[usize]; 2] = [&[0, 4], &[1, 2, 3]];
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.set_width(64.0);
            ui.label(RichText::new("Pocket").strong().size(15.0));
            ui.label(RichText::new("Synth").small().color(theme::TEXT_DIM));
            let mut on = !device.bypass;
            if ui.checkbox(&mut on, "On").changed() {
                let bypass = !on;
                app.session.edit(vec![Op::DeviceSetBypass {
                    track,
                    id: device.id,
                    bypass,
                }]);
            }
        });
        ui.separator();
        ui.vertical(|ui| {
            for row in ROWS {
                // A fixed height, because a separator takes all the height it is offered.
                let size = vec2(ui.available_width(), 98.0);
                ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Min), |ui| {
                    for (position, &section) in row.iter().enumerate() {
                        if position > 0 {
                            ui.separator();
                        }
                        let (title, params) = SECTIONS[section];
                        ui.vertical(|ui| {
                            ui.label(RichText::new(title).small().color(theme::TEXT_DIM));
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 2.0;
                                for &index in params {
                                    param(app, ui, track, device, &pocket::PARAMS[index]);
                                }
                            });
                        });
                    }
                });
                ui.add_space(4.0);
            }
        });
    });
}

fn param(app: &mut DawApp, ui: &mut Ui, track: TrackId, device: &Device, info: &ParamInfo) {
    let stored = device.params.get(&info.id).map_or(info.default, |v| *v as f32);
    let value = if stored.is_finite() {
        stored.clamp(info.min, info.max)
    } else {
        info.default
    };
    let set = |value: Option<f32>| Op::DeviceSetParam {
        track,
        id: device.id,
        param: info.id,
        value: value.map(f64::from),
    };
    if info.scale == Scale::Stepped {
        ui.vertical(|ui| {
            ui.add_space(6.0);
            let label = ui.label(RichText::new(info.name).size(11.0).color(theme::TEXT_DIM));
            let mut choice = value.round() as usize;
            let current = info.choices.get(choice).copied().unwrap_or("?");
            let combo = ComboBox::from_id_salt(info.id)
                .selected_text(current)
                .width(76.0)
                .show_ui(ui, |ui| {
                    for (index, name) in info.choices.iter().enumerate() {
                        if ui.selectable_value(&mut choice, index, *name).changed() {
                            app.session.edit(vec![set(Some(index as f32))]);
                        }
                    }
                });
            combo.response.labelled_by(label.id);
        });
        return;
    }
    let (response, change) = knob(ui, info, value);
    match change {
        Some(Change::Reset) => {
            app.session.edit(vec![set(None)]);
        }
        Some(Change::Set(value)) => {
            if let Some(gesture) = app.gestures.track(&mut app.session, &response) {
                app.session.edit_gesture(vec![set(Some(value))], gesture);
            }
        }
        None => {
            // Keeps the gesture tracker informed about where a drag begins.
            app.gestures.track(&mut app.session, &response);
        }
    }
}

enum Change {
    Set(f32),
    Reset,
}

fn to_norm(info: &ParamInfo, value: f32) -> f32 {
    let norm = match info.scale {
        Scale::Log => (value / info.min).ln() / (info.max / info.min).ln(),
        Scale::Linear | Scale::Stepped => (value - info.min) / (info.max - info.min),
    };
    norm.clamp(0.0, 1.0)
}

fn from_norm(info: &ParamInfo, norm: f32) -> f32 {
    let value = match info.scale {
        Scale::Log => info.min * (info.max / info.min).powf(norm),
        Scale::Linear | Scale::Stepped => info.min + (info.max - info.min) * norm,
    };
    // Whole steps for semitones and cents.
    let value = if matches!(info.unit, "st" | "ct") {
        value.round()
    } else {
        value
    };
    value.clamp(info.min, info.max)
}

fn format_value(info: &ParamInfo, value: f32) -> String {
    match info.unit {
        "s" if value < 1.0 => format!("{:.0} ms", value * 1000.0),
        "s" => format!("{value:.2} s"),
        "Hz" if value >= 1000.0 => format!("{:.2} kHz", value / 1000.0),
        "Hz" => format!("{value:.0} Hz"),
        "st" | "ct" => format!("{value:+.0} {}", info.unit),
        "oct" => format!("{value:+.1} oct"),
        _ => format!("{:.0} %", value * 100.0),
    }
}

/// A rotary control. Drag up and down to turn it; double-click to reset.
fn knob(ui: &mut Ui, info: &ParamInfo, value: f32) -> (Response, Option<Change>) {
    let (rect, mut response) = ui.allocate_exact_size(vec2(62.0, 76.0), Sense::click_and_drag());
    let mut change = None;
    let mut norm = to_norm(info, value);

    // The drag accumulates in knob travel, not in the value, so rounding cannot stall it.
    if response.drag_started() {
        ui.data_mut(|data| data.insert_temp(response.id, norm));
    }
    if response.dragged() {
        let fine = ui.input(|i| i.modifiers.shift);
        let delta = -response.drag_delta().y / if fine { 1400.0 } else { 180.0 };
        let travel = ui.data_mut(|data| {
            let travel = data.get_temp_mut_or(response.id, norm);
            *travel = (*travel + delta).clamp(0.0, 1.0);
            *travel
        });
        let turned = from_norm(info, travel);
        if turned != value {
            norm = to_norm(info, turned);
            change = Some(Change::Set(turned));
            response.mark_changed();
        }
    }
    if response.double_clicked() {
        norm = to_norm(info, info.default);
        change = Some(Change::Reset);
    }

    let shown = match change {
        Some(Change::Set(value)) => value,
        Some(Change::Reset) => info.default,
        None => value,
    };
    response.widget_info(|| egui::WidgetInfo::slider(true, f64::from(shown), info.name));
    let painter = ui.painter_at(rect);
    let name = pos2(rect.center().x, rect.top() + 2.0);
    painter.text(
        name,
        Align2::CENTER_TOP,
        info.name,
        FontId::proportional(11.0),
        theme::TEXT_DIM,
    );
    let centre = pos2(rect.center().x, rect.top() + 36.0);
    let radius = 15.0;
    // 270 degrees of travel, with the gap at the bottom.
    let angle = |norm: f32| (135.0 + 270.0 * norm).to_radians();
    let arc = |to: f32| -> Vec<egui::Pos2> {
        let steps = (to * 32.0).ceil().max(1.0) as usize;
        (0..=steps)
            .map(|step| {
                let a = angle(to * step as f32 / steps as f32);
                centre + radius * vec2(a.cos(), a.sin())
            })
            .collect()
    };
    let active = response.dragged() || response.hovered();
    painter.add(Shape::line(arc(1.0), Stroke::new(3.0, theme::GRID_BAR)));
    if norm > 0.0 {
        painter.add(Shape::line(arc(norm), Stroke::new(3.0, theme::LOOP)));
    }
    let a = angle(norm);
    let direction = vec2(a.cos(), a.sin());
    let pointer = if active { Color32::WHITE } else { theme::TEXT };
    painter.line_segment(
        [centre + direction * 4.0, centre + direction * (radius - 3.0)],
        Stroke::new(2.0, pointer),
    );
    let value_at = pos2(rect.center().x, rect.bottom() - 3.0);
    let text = format_value(info, shown);
    painter.text(
        value_at,
        Align2::CENTER_BOTTOM,
        text,
        FontId::proportional(11.0),
        theme::TEXT,
    );

    let hint = format!(
        "{}: drag up or down. Hold Shift for fine steps. Double-click to reset.",
        info.name
    );
    (response.on_hover_text(hint), change)
}
