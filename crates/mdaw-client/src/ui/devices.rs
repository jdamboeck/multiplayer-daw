//! The device panel of the selected track.

use egui::{
    Align, Align2, Color32, ComboBox, FontId, Layout, Response, RichText, Sense, Shape, Stroke, Ui, pos2,
    vec2,
};
use mdaw_dsp::pocket::{self, ParamInfo, Scale};
use mdaw_model::{BuiltinKind, Device, DeviceKind, Op, PluginFormat, TrackId};

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
        add_plugin(app, ui, track_id);
        ui.separator();
        for device in &track.content.devices {
            ui.push_id(device.id.0.0, |ui| match &device.kind {
                DeviceKind::Builtin {
                    kind: BuiltinKind::Pocket,
                    ..
                } => pocket_panel(app, ui, track_id, device),
                DeviceKind::Plugin { format, id, .. } => plugin_panel(app, ui, track_id, device, *format, id),
            });
            ui.separator();
        }
    });
}

/// Parameters shown in the panel; the plugin's own window has them all.
const SHOWN_PARAMS: usize = 48;

fn plugin_panel(
    app: &mut DawApp,
    ui: &mut Ui,
    track: TrackId,
    device: &Device,
    format: PluginFormat,
    id: &str,
) {
    let mine =
        app.session.checkout_owner(track) == Some(app.session.user()) || app.session.linked().is_none();
    let name = app
        .session
        .plugin_name(format, id)
        .unwrap_or_else(|| id.to_string());
    let running = app.session.plugin_running(track, device.id);
    ui.horizontal(|ui| {
        ui.label(RichText::new(&name).strong().size(15.0));
        ui.label(RichText::new(format.name()).small().color(theme::TEXT_DIM));
        if running && app.session.plugin_has_editor(track, device.id) {
            let open = app.session.plugin_editor_open(track, device.id);
            let label = if open { "Close window" } else { "Open window" };
            if ui
                .button(label)
                .on_hover_text("The plugin's own window, with all of its controls")
                .clicked()
            {
                app.session.toggle_plugin_editor(track, device.id);
            }
        }
        let mut on = !device.bypass;
        if ui.add_enabled(mine, egui::Checkbox::new(&mut on, "On")).changed() {
            app.session.edit(vec![Op::DeviceSetBypass {
                track,
                id: device.id,
                bypass: !on,
            }]);
        }
        if ui
            .add_enabled(mine, egui::Button::new("Remove"))
            .on_hover_text("Take the plugin off the track")
            .clicked()
        {
            app.session.edit(vec![Op::DeviceRemove { track, id: device.id }]);
        }
    });
    if app.session.plugin_crashed(track, device.id) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "{name} crashed. The rest of mdaw goes on; the track plays its stem, if it has one."
                ))
                .color(theme::METER_HOT),
            );
            if ui
                .button("Restart")
                .on_hover_text("Start the plugin again, with the settings it had shortly before")
                .clicked()
            {
                app.session.restart_plugin(track, device.id);
            }
        });
        return;
    }
    if !running {
        let why = if app.session.plugin_name(format, id).is_none() {
            format!("{name} is not installed here. The track plays its stem: the sound of its last commit.")
        } else {
            "Starting the plugin…".to_string()
        };
        ui.label(RichText::new(why).color(theme::TEXT_DIM));
        return;
    }
    if !mine {
        ui.label(
            RichText::new("Check out the track to change the plugin's settings.").color(theme::TEXT_DIM),
        );
    }
    let (params, count) = app
        .session
        .plugin_params(track, device.id, SHOWN_PARAMS)
        .unwrap_or_default();
    egui::Grid::new(("params", device.id.0.0))
        .num_columns(4)
        .spacing(vec2(12.0, 4.0))
        .show(ui, |ui| {
            for (index, param) in params.iter().enumerate() {
                let mut value = param.value;
                let slider = egui::Slider::new(&mut value, param.min..=param.max)
                    .show_value(true)
                    .step_by(if param.stepped { 1.0 } else { 0.0 })
                    .text(&param.name);
                if ui.add_enabled(mine, slider).changed() {
                    app.session.set_plugin_param(track, device.id, param.id, value);
                }
                if index % 2 == 1 {
                    ui.end_row();
                }
            }
        });
    if count > params.len() {
        let more = format!("And {} more in the plugin's window.", count - params.len());
        ui.label(RichText::new(more).color(theme::TEXT_DIM));
    }
}

/// The track's instrument, and effects to add after it.
fn add_plugin(app: &mut DawApp, ui: &mut Ui, track: TrackId) {
    if !app.session.plugins_enabled() {
        ui.label(RichText::new("Plugins are switched off in this window.").color(theme::TEXT_DIM));
        return;
    }
    let mine =
        app.session.checkout_owner(track) == Some(app.session.user()) || app.session.linked().is_none();
    let catalog = app.session.plugin_catalog();
    let label = |plugin: &mdaw_plugin::PluginInfo| {
        if plugin.vendor.is_empty() {
            format!("{} ({})", plugin.name, plugin.format.name())
        } else {
            format!("{} - {} ({})", plugin.name, plugin.vendor, plugin.format.name())
        }
    };
    let (instruments, effects): (Vec<_>, Vec<_>) = catalog.iter().partition(|p| p.instrument);
    enum Pick {
        Pocket,
        Plugin(mdaw_plugin::PluginInfo),
    }
    let mut pick = None;
    ui.horizontal(|ui| {
        ui.add_enabled_ui(mine, |ui| {
            ui.menu_button("Instrument…", |ui| {
                ui.label(RichText::new("A track has one instrument; this one takes its place.").small());
                if ui.button("Pocket (built in)").clicked() {
                    pick = Some(Pick::Pocket);
                    ui.close();
                }
                for plugin in &instruments {
                    if ui.button(label(plugin)).on_hover_text(&plugin.location).clicked() {
                        pick = Some(Pick::Plugin((*plugin).clone()));
                        ui.close();
                    }
                }
            });
            if !effects.is_empty() {
                ui.menu_button("Add effect…", |ui| {
                    for plugin in &effects {
                        if ui.button(label(plugin)).on_hover_text(&plugin.location).clicked() {
                            pick = Some(Pick::Plugin((*plugin).clone()));
                            ui.close();
                        }
                    }
                });
            }
        });
        if !mine {
            // Said out loud: a greyed-out button does not tell why.
            let why = match app.session.checkout_owner(track) {
                Some(_) => "The track is checked out by someone else.",
                None => "Plugins go onto a track while you have it checked out: click \"Check out\" first.",
            };
            ui.label(RichText::new(why).color(theme::TEXT_DIM));
        }
    });
    match pick {
        Some(Pick::Pocket) => {
            app.session.use_pocket(track);
        }
        Some(Pick::Plugin(plugin)) => {
            app.session.add_plugin(track, &plugin);
        }
        None => {}
    }
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
