//! The bar at the top: this client's own transport, position, tempo, undo and the output.

use egui::{Align, Button, DragValue, Layout, Rect, RichText, Sense, Ui, pos2, vec2};
use mdaw_model::{Op, TempoPoint, Tick};

use super::{MAX_DB, MIN_DB, db_to_gain, format_position, format_time, gain_to_db};
use crate::{DawApp, Status, theme};

#[derive(Default)]
pub struct TransportState {
    /// Output peaks with a falling decay, per channel.
    meter: [f32; 2],
}

impl TransportState {
    pub fn meter_active(&self) -> bool {
        self.meter.iter().any(|level| *level > 0.001)
    }
}

pub fn show(app: &mut DawApp, ui: &mut Ui) {
    let project = app.session.project().clone();
    let tempo = project.tempo_map();
    ui.add_space(3.0);
    ui.horizontal(|ui| {
        let playing = app.session.is_playing();
        let label = if playing { "Pause" } else { "Play" };
        if ui
            .add(Button::new(label).min_size(vec2(76.0, 0.0)))
            .on_hover_text("Space")
            .clicked()
        {
            app.toggle_play();
        }
        if ui
            .button("Stop")
            .on_hover_text("Stop. Press again to go back to the start.")
            .clicked()
        {
            app.stop();
        }
        super::live::record_button(app, ui);
        let hint = "Repeat the loop region. Shift-drag in the ruler to set it.";
        if ui
            .toggle_value(&mut app.loop_on, "Loop")
            .on_hover_text(hint)
            .changed()
        {
            app.apply_loop();
        }

        ui.separator();
        let tick = app.session.position();
        ui.label(
            RichText::new(format_position(tick))
                .monospace()
                .size(17.0)
                .color(theme::TEXT),
        )
        .on_hover_text("bar.beat.sixteenth");
        ui.label(
            RichText::new(format_time(tempo.tick_to_seconds(tick)))
                .monospace()
                .color(theme::TEXT_DIM),
        );

        ui.separator();
        let mut bpm = tempo.milli_bpm_at(Tick::ZERO) as f64 / 1000.0;
        let response = ui
            .add(
                DragValue::new(&mut bpm)
                    .range(20.0..=300.0)
                    .speed(0.1)
                    .max_decimals(1)
                    .suffix(" bpm"),
            )
            .on_hover_text("Tempo");
        if let Some(gesture) = app.gestures.track(&mut app.session, &response) {
            let existing = project
                .tempo
                .iter()
                .filter(|(_, p)| p.tick == Tick::ZERO)
                .map(|(id, _)| *id)
                .max();
            let id = existing.unwrap_or_else(|| app.session.ids().tempo());
            let point = TempoPoint {
                tick: Tick::ZERO,
                milli_bpm: (bpm * 1000.0).round() as u32,
            };
            app.session
                .edit_gesture(vec![Op::TempoPut { id, point }], gesture);
        }

        ui.separator();
        if ui
            .add_enabled(app.session.can_undo(), Button::new("Undo"))
            .on_hover_text("Ctrl+Z")
            .clicked()
        {
            app.session.undo();
        }
        if ui
            .add_enabled(app.session.can_redo(), Button::new("Redo"))
            .on_hover_text("Ctrl+Shift+Z")
            .clicked()
        {
            app.session.redo();
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let mut db = gain_to_db(app.session.engine().output_gain());
            let response = ui
                .add(
                    DragValue::new(&mut db)
                        .range(MIN_DB..=MAX_DB)
                        .speed(0.2)
                        .fixed_decimals(1)
                        .suffix(" dB"),
                )
                .on_hover_text(
                    "Your listening volume. It is yours alone: it changes nothing in the project, \
                     and nobody else hears it.",
                );
            if response.changed() {
                app.session.engine_mut().set_output_gain(db_to_gain(db));
            }
            meter(app, ui);
            ui.label(
                RichText::new(app.session.engine().description())
                    .color(theme::TEXT_DIM)
                    .small(),
            );
            connection(app, ui);
        });
    });
    ui.add_space(2.0);
    super::live::show(app, ui);
    ui.add_space(3.0);
}

/// Who is here, or what is wrong with the connection.
fn connection(app: &DawApp, ui: &mut Ui) {
    let session = &app.session;
    let waiting = match session.unconfirmed() {
        0 => String::new(),
        1 => ", 1 edit waiting".into(),
        n => format!(", {n} edits waiting"),
    };
    match session.status() {
        Status::Local => {}
        Status::Online => {
            let me = session.actor();
            let mut names: Vec<String> = Vec::new();
            for peer in session.peers() {
                names.push(if peer.actor == me {
                    format!("{} (you)", peer.name)
                } else {
                    peer.name.clone()
                });
            }
            let shown = names.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
            let more = match names.len().saturating_sub(4) {
                0 => String::new(),
                n => format!(" +{n}"),
            };
            ui.label(RichText::new(format!("Online: {shown}{more}")).color(theme::METER))
                .on_hover_text(names.join("\n"));
            ui.separator();
        }
        Status::Reconnecting => {
            ui.label(RichText::new(format!("Reconnecting{waiting}")).color(theme::WARN))
                .on_hover_text("The connection to the server is down. You can keep working; edits are sent when it is back.");
            ui.separator();
        }
        Status::Refused => {
            ui.label(RichText::new(format!("Disconnected{waiting}")).color(theme::METER_HOT))
                .on_hover_text(
                    "The server no longer accepts this client. Restart with a current join string.",
                );
            ui.separator();
        }
    }
}

/// A strip under the transport bar for things that happened behind the user's back.
pub fn notices(app: &mut DawApp, ui: &mut Ui) {
    let mut dismiss = None;
    for (index, text) in app.session.notices().iter().enumerate() {
        ui.horizontal(|ui| {
            if ui.small_button("Dismiss").clicked() {
                dismiss = Some(index);
            }
            ui.label(RichText::new(text).color(theme::WARN));
        });
    }
    if let Some(index) = dismiss {
        app.session.dismiss_notice(index);
    }
}

fn meter(app: &mut DawApp, ui: &mut Ui) {
    let peak = app.session.engine().take_peak();
    let levels = &mut app.transport.meter;
    for (level, peak) in levels.iter_mut().zip(peak) {
        *level = (*level * 0.86).max(peak);
    }
    let (rect, _) = ui.allocate_exact_size(vec2(96.0, 14.0), Sense::hover());
    let painter = ui.painter_at(rect);
    for (channel, level) in levels.iter().enumerate() {
        let top = rect.top() + 1.0 + channel as f32 * 7.0;
        let bar = Rect::from_min_max(pos2(rect.left(), top), pos2(rect.right(), top + 5.0));
        painter.rect_filled(bar, 1.0, theme::CANVAS);
        // -48 dB at the left edge, 0 dB at the right.
        let fill = ((gain_to_db(*level) + 48.0) / 48.0).clamp(0.0, 1.0);
        if fill > 0.0 {
            let color = if *level > 1.0 {
                theme::METER_HOT
            } else {
                theme::METER
            };
            let filled = Rect::from_min_max(bar.min, pos2(bar.left() + bar.width() * fill, bar.bottom()));
            painter.rect_filled(filled, 1.0, color);
        }
    }
}
