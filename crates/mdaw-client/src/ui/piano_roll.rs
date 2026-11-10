//! The note editor for the selected clip.

use egui::{
    Align2, Color32, ComboBox, CursorIcon, FontId, Id, Painter, Pos2, Rect, RichText, Sense, Stroke,
    StrokeKind, Ui, pos2,
};
use mdaw_model::{Clip, ClipId, Note, NoteId, NoteProp, Op, PPQ, TICKS_PER_BAR, Tick, TrackId};
use mdaw_sync::Gesture;

use super::{TimeAxis, paint_grid, paint_playhead, paint_ruler, snap_ceil, snap_floor, snap_round};
use crate::{DawApp, Focus, theme};

pub const KEYS_W: f32 = 56.0;
pub const NOTE_H: f32 = 13.0;
pub const VEL_H: f32 = 58.0;
pub const RULER_H: f32 = 18.0;
/// Engine key of the note that sounds while the pointer is held in the editor.
pub const PREVIEW_KEY: u64 = 1;
const EDGE: f32 = 6.0;
const MIN_NOTE: i64 = PPQ / 32;
const SNAPS: [(i64, &str); 5] = [
    (PPQ, "1/4"),
    (PPQ / 2, "1/8"),
    (PPQ / 4, "1/16"),
    (PPQ / 8, "1/32"),
    (1, "Off"),
];

pub struct RollState {
    pub px_per_beat: f32,
    pub scroll_x: f32,
    pub scroll_y: f32,
    /// Grid for new and moved notes, in ticks. 1 means no snapping.
    pub snap: i64,
    /// Length and velocity for the next note added with a click.
    pub last_dur: Tick,
    pub velocity: u8,
    drag: Drag,
    preview: Option<(TrackId, u8)>,
    vel_gesture: Option<Gesture>,
    shown: Option<ClipId>,
    last_seek: Option<Tick>,
    /// Where things were in the last frame; `None` while no clip is open.
    pub layout: Option<RollLayout>,
}

impl Default for RollState {
    fn default() -> Self {
        Self {
            px_per_beat: 88.0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            snap: PPQ / 4,
            last_dur: Tick(PPQ / 4),
            velocity: 100,
            drag: Drag::None,
            preview: None,
            vel_gesture: None,
            shown: None,
            last_seek: None,
            layout: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RollLayout {
    pub ruler: Rect,
    pub grid: Rect,
    pub keys: Rect,
    pub velocity: Rect,
    /// Maps absolute ticks; notes are at `clip_start + note.start`.
    pub axis: TimeAxis,
    pub clip_start: Tick,
    /// Screen y of the top of the row of pitch 127.
    pub top: f32,
}

impl RollLayout {
    /// Top of a pitch's row.
    pub fn y(&self, pitch: u8) -> f32 {
        self.top + (127 - pitch.min(127)) as f32 * NOTE_H
    }

    pub fn pitch_at(&self, y: f32) -> Option<u8> {
        let row = ((y - self.top) / NOTE_H).floor();
        (0.0..128.0).contains(&row).then(|| 127 - row as u8)
    }

    /// The middle of a pitch's row at a tick counted from the clip start.
    pub fn pos(&self, tick: Tick, pitch: u8) -> Pos2 {
        pos2(self.axis.x(self.clip_start + tick), self.y(pitch) + NOTE_H / 2.0)
    }

    /// Ticks from the clip start at a screen x.
    fn rel(&self, x: f32) -> f64 {
        self.axis.tick(x) - self.clip_start.0 as f64
    }

    fn note_rect(&self, note: &Note) -> Rect {
        let left = self.axis.x(self.clip_start + note.start);
        let right = self.axis.x(self.clip_start + note.start + note.dur);
        let top = self.y(note.pitch);
        Rect::from_min_max(
            pos2(left, top + 1.0),
            pos2(right.max(left + 2.0), top + NOTE_H - 1.0),
        )
    }
}

#[derive(Clone, Copy, Debug)]
enum Drag {
    None,
    New {
        start: Tick,
        dur: Tick,
        pitch: u8,
    },
    /// Moves the selected notes by `dt` ticks and `dp` semitones.
    Move {
        origin: f64,
        origin_pitch: u8,
        grabbed_pitch: u8,
        dt: i64,
        dp: i32,
    },
    /// Lengthens the selected notes by `dd` ticks.
    Resize {
        origin: f64,
        dd: i64,
    },
}

pub fn toolbar(app: &mut DawApp, ui: &mut Ui) {
    let project = app.session.project();
    let open = app.sel.clip.and_then(|(track, clip)| {
        let track = project.track(track)?;
        Some((track.shell.name.clone(), track.content.clips.get(&clip)?.start))
    });
    match open {
        Some((name, start)) => ui.label(format!("{name}, clip at bar {}", start.0 / TICKS_PER_BAR + 1)),
        None => ui.label(RichText::new("No clip open").color(theme::TEXT_DIM)),
    };
    ui.separator();
    let label = ui.label("Snap");
    let current = SNAPS
        .iter()
        .find(|(ticks, _)| *ticks == app.roll.snap)
        .map_or("?", |(_, name)| name);
    let combo = ComboBox::from_id_salt("roll-snap")
        .selected_text(current)
        .width(64.0)
        .show_ui(ui, |ui| {
            for (ticks, name) in SNAPS {
                ui.selectable_value(&mut app.roll.snap, ticks, name);
            }
        });
    combo.response.labelled_by(label.id);
}

pub fn show(app: &mut DawApp, ui: &mut Ui) {
    let project = app.session.project().clone();
    let open = app.sel.clip.and_then(|(track_id, clip_id)| {
        let track = project.track(track_id)?;
        Some((track_id, clip_id, track, track.content.clips.get(&clip_id)?))
    });
    let Some((track_id, clip_id, track, clip)) = open else {
        app.roll.layout = None;
        app.roll.shown = None;
        ui.centered_and_justified(|ui| {
            ui.label(RichText::new("Double-click a clip to edit its notes.").color(theme::TEXT_DIM));
        });
        return;
    };
    let color = theme::rgb(track.shell.color);

    let full = ui.available_rect_before_wrap();
    ui.allocate_rect(full, Sense::hover());
    let left = full.left() + KEYS_W;
    let ruler = Rect::from_min_max(pos2(left, full.top()), pos2(full.right(), full.top() + RULER_H));
    let velocity = Rect::from_min_max(pos2(left, full.bottom() - VEL_H), full.max);
    let grid = Rect::from_min_max(ruler.left_bottom(), velocity.right_top());
    let keys = Rect::from_min_max(pos2(full.left(), grid.top()), grid.left_bottom());

    let state = &mut app.roll;
    if state.shown != Some(clip_id) {
        state.shown = Some(clip_id);
        state.scroll_x = 0.0;
        let centre = if clip.notes.is_empty() {
            64.0
        } else {
            clip.notes.values().map(|n| n.pitch as f32).sum::<f32>() / clip.notes.len() as f32
        };
        state.scroll_y = (127.5 - centre) * NOTE_H - grid.height() / 2.0;
    }
    if ui.rect_contains_pointer(full) {
        let (scroll, zoom, pointer) =
            ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta(), i.pointer.hover_pos()));
        if zoom != 1.0 {
            let pivot = pointer.map_or(grid.left(), |p| p.x.max(grid.left())) - grid.left();
            let beats = (state.scroll_x + pivot) / state.px_per_beat;
            state.px_per_beat = (state.px_per_beat * zoom).clamp(12.0, 800.0);
            state.scroll_x = beats * state.px_per_beat - pivot;
        }
        state.scroll_x -= scroll.x;
        state.scroll_y -= scroll.y;
    }
    state.scroll_x = state.scroll_x.max(0.0);
    state.scroll_y = state
        .scroll_y
        .clamp(0.0, (128.0 * NOTE_H - grid.height()).max(0.0));
    let px_per_tick = state.px_per_beat / PPQ as f32;
    let axis = TimeAxis {
        x0: grid.left() - state.scroll_x - clip.start.0 as f32 * px_per_tick,
        px_per_tick,
    };
    let layout = RollLayout {
        ruler,
        grid,
        keys,
        velocity,
        axis,
        clip_start: clip.start,
        top: grid.top() - state.scroll_y,
    };
    state.layout = Some(layout);

    interact_ruler(app, ui, &layout);
    interact_keys(app, ui, &layout, track_id);
    interact_grid(app, ui, &layout, track_id, clip_id, clip);
    interact_velocity(app, ui, &layout, track_id, clip_id, clip);

    ui.painter_at(full).rect_filled(full, 0.0, theme::CANVAS);
    paint_rows(&ui.painter_at(grid), &layout);
    paint_grid(&ui.painter_at(grid), grid, &axis, 9.0);
    paint_notes(app, &ui.painter_at(grid), &layout, clip, color);
    let outside = ui.painter_at(grid.union(velocity));
    let end = axis.x(clip.start + clip.length).max(grid.left());
    let after = Rect::from_min_max(pos2(end, grid.top()), velocity.max);
    outside.rect_filled(after, 0.0, theme::OUTSIDE);
    outside.vline(end, after.y_range(), Stroke::new(1.0, color));
    paint_velocity(app, &ui.painter_at(velocity), &layout, clip, color);

    let loop_range = app.loop_on.then_some((app.loop_range.start, app.loop_range.end));
    paint_ruler(&ui.painter_at(ruler), ruler, &axis, loop_range);
    let body = grid.union(velocity);
    paint_playhead(
        &ui.painter_at(ruler.union(body)),
        axis.x_f(app.session.engine().position()),
        ruler,
        body,
    );
    paint_keys(app, &ui.painter_at(keys), &layout);

    let painter = ui.painter_at(full);
    let label = pos2(full.left() + 6.0, velocity.top() + 6.0);
    painter.text(
        label,
        Align2::LEFT_TOP,
        "Velocity",
        FontId::proportional(10.0),
        theme::TEXT_DIM,
    );
    painter.vline(grid.left(), full.y_range(), Stroke::new(1.0, Color32::BLACK));
    painter.hline(full.x_range(), velocity.top(), Stroke::new(1.0, Color32::BLACK));
}

/// Stops the preview note once the pointer is up. Runs every frame, whatever is on screen.
pub fn release_preview(app: &mut DawApp, ctx: &egui::Context) {
    if app.roll.preview.is_some() && !ctx.input(|i| i.pointer.primary_down()) {
        stop_preview(app);
    }
}

fn start_preview(app: &mut DawApp, track: TrackId, pitch: u8, vel: u8) {
    if app.roll.preview != Some((track, pitch)) {
        stop_preview(app);
        app.session.engine_mut().note_on(track, PREVIEW_KEY, pitch, vel);
        app.roll.preview = Some((track, pitch));
    }
}

fn stop_preview(app: &mut DawApp) {
    if let Some((track, _)) = app.roll.preview.take() {
        app.session.engine_mut().note_off(track, PREVIEW_KEY);
    }
}

fn interact_ruler(app: &mut DawApp, ui: &Ui, layout: &RollLayout) {
    let response = ui.interact(layout.ruler, Id::unique("roll-ruler"), Sense::click_and_drag());
    if !response.is_pointer_button_down_on() {
        app.roll.last_seek = None;
        return;
    }
    let Some(pos) = response.interact_pointer_pos() else {
        return;
    };
    let tick = snap_round(layout.axis.tick(pos.x).max(0.0), app.roll.snap);
    if app.roll.last_seek != Some(tick) {
        app.roll.last_seek = Some(tick);
        app.session.engine_mut().seek(tick);
    }
}

fn interact_keys(app: &mut DawApp, ui: &Ui, layout: &RollLayout, track: TrackId) {
    let response = ui.interact(layout.keys, Id::unique("roll-keys"), Sense::click_and_drag());
    if response.is_pointer_button_down_on()
        && let Some(pitch) = response
            .interact_pointer_pos()
            .and_then(|pos| layout.pitch_at(pos.y))
    {
        start_preview(app, track, pitch, app.roll.velocity);
    }
}

/// The note under a point, and whether the point is on its resize edge.
fn note_at(layout: &RollLayout, clip: &Clip, pos: Pos2) -> Option<(NoteId, Note, bool)> {
    let mut found = None;
    for (id, note) in &clip.notes {
        let rect = layout.note_rect(note);
        if layout.pitch_at(pos.y) == Some(note.pitch) && pos.x >= rect.left() && pos.x < rect.right() {
            found = Some((
                *id,
                *note,
                pos.x >= rect.right() - EDGE && rect.width() > 2.0 * EDGE,
            ));
        }
    }
    found
}

fn interact_grid(
    app: &mut DawApp,
    ui: &Ui,
    layout: &RollLayout,
    track: TrackId,
    clip_id: ClipId,
    clip: &Clip,
) {
    let response = ui.interact(layout.grid, Id::unique("roll-grid"), Sense::click_and_drag());
    let (pressed, shift, origin) = ui.input(|i| {
        (
            i.pointer.primary_pressed(),
            i.modifiers.shift,
            i.pointer.press_origin(),
        )
    });
    let snap = app.roll.snap;

    if let Some(pos) = response.hover_pos()
        && matches!(note_at(layout, clip, pos), Some((_, _, true)))
    {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
    }
    if matches!(app.roll.drag, Drag::Resize { .. }) {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
    }

    // Pointer down: select and sound the note under it.
    if pressed
        && response.is_pointer_button_down_on()
        && let Some(pos) = response.interact_pointer_pos()
    {
        app.sel.focus = Focus::Notes;
        match note_at(layout, clip, pos) {
            Some((id, note, _)) => {
                if !app.sel.notes.contains(&id) {
                    if !shift {
                        app.sel.notes.clear();
                    }
                    app.sel.notes.insert(id);
                }
                start_preview(app, track, note.pitch, note.vel);
            }
            None => {
                if let Some(pitch) = layout.pitch_at(pos.y) {
                    start_preview(app, track, pitch, app.roll.velocity);
                }
            }
        }
    }

    if response.drag_started()
        && let Some(origin) = origin
    {
        let tick = layout.rel(origin.x);
        app.roll.drag = match (note_at(layout, clip, origin), layout.pitch_at(origin.y)) {
            (Some((_, _, true)), _) => Drag::Resize { origin: tick, dd: 0 },
            (Some((_, note, false)), Some(pitch)) => Drag::Move {
                origin: tick,
                origin_pitch: pitch,
                grabbed_pitch: note.pitch,
                dt: 0,
                dp: 0,
            },
            (None, Some(pitch)) if tick >= 0.0 && tick < clip.length.0 as f64 => {
                app.sel.notes.clear();
                let start = snap_floor(tick, snap);
                Drag::New {
                    start,
                    dur: Tick(snap.max(MIN_NOTE)),
                    pitch,
                }
            }
            _ => Drag::None,
        };
    }

    if response.dragged()
        && let Some(pos) = response.interact_pointer_pos()
    {
        let tick = layout.rel(pos.x);
        let selected = || {
            clip.notes
                .iter()
                .filter(|(id, _)| app.sel.notes.contains(id))
                .map(|(_, n)| *n)
        };
        let mut sound = None;
        match &mut app.roll.drag {
            Drag::New { start, dur, .. } => {
                *dur = (snap_ceil(tick, snap) - *start).max(Tick(snap.max(MIN_NOTE)));
            }
            Drag::Move {
                origin,
                origin_pitch,
                grabbed_pitch,
                dt,
                dp,
            } => {
                let earliest = selected().map(|n| n.start.0).min().unwrap_or(0);
                let lowest = selected().map(|n| n.pitch as i32).min().unwrap_or(0);
                let highest = selected().map(|n| n.pitch as i32).max().unwrap_or(127);
                *dt = snap_round(tick - *origin, snap).0.max(-earliest);
                let pitch = layout.pitch_at(pos.y).unwrap_or(*origin_pitch) as i32;
                let moved = (pitch - *origin_pitch as i32).clamp(-lowest, 127 - highest);
                if moved != *dp {
                    *dp = moved;
                    sound = Some((*grabbed_pitch as i32 + moved) as u8);
                }
            }
            Drag::Resize { origin, dd } => *dd = snap_round(tick - *origin, snap).0,
            Drag::None => {}
        }
        if let Some(pitch) = sound {
            start_preview(app, track, pitch, app.roll.velocity);
        }
    }

    if response.drag_stopped() {
        let mut ops = Vec::new();
        match std::mem::replace(&mut app.roll.drag, Drag::None) {
            Drag::New { start, dur, pitch } => {
                let id = app.session.ids().note();
                let note = Note {
                    start,
                    dur,
                    pitch,
                    vel: app.roll.velocity,
                };
                ops.push(Op::NoteAdd {
                    track,
                    clip: clip_id,
                    id,
                    note,
                });
                app.sel.notes = [id].into();
                app.roll.last_dur = dur;
            }
            Drag::Move { dt, dp, .. } => {
                for (&id, note) in clip.notes.iter().filter(|(id, _)| app.sel.notes.contains(id)) {
                    let moved = moved_note(note, dt, dp);
                    if moved.start != note.start {
                        let prop = NoteProp::Start(moved.start);
                        ops.push(Op::NoteSet {
                            track,
                            clip: clip_id,
                            id,
                            prop,
                        });
                    }
                    if moved.pitch != note.pitch {
                        let prop = NoteProp::Pitch(moved.pitch);
                        ops.push(Op::NoteSet {
                            track,
                            clip: clip_id,
                            id,
                            prop,
                        });
                    }
                }
            }
            Drag::Resize { dd, .. } => {
                for (&id, note) in clip.notes.iter().filter(|(id, _)| app.sel.notes.contains(id)) {
                    let dur = resized_dur(note, dd);
                    if dur != note.dur {
                        ops.push(Op::NoteSet {
                            track,
                            clip: clip_id,
                            id,
                            prop: NoteProp::Dur(dur),
                        });
                        app.roll.last_dur = dur;
                    }
                }
            }
            Drag::None => {}
        }
        app.session.edit(ops);
    } else if response.clicked()
        && let Some(pos) = response.interact_pointer_pos()
    {
        match (note_at(layout, clip, pos), layout.pitch_at(pos.y)) {
            (Some((id, ..)), _) => {
                if !shift {
                    app.sel.notes = [id].into();
                }
            }
            (None, Some(pitch)) => {
                let tick = layout.rel(pos.x);
                if tick >= 0.0 && tick < clip.length.0 as f64 {
                    let id = app.session.ids().note();
                    let note = Note {
                        start: snap_floor(tick, snap),
                        dur: app.roll.last_dur,
                        pitch,
                        vel: app.roll.velocity,
                    };
                    if app.session.edit(vec![Op::NoteAdd {
                        track,
                        clip: clip_id,
                        id,
                        note,
                    }]) {
                        app.sel.notes = [id].into();
                    }
                }
            }
            (None, None) => {}
        }
    } else if response.secondary_clicked()
        && let Some(pos) = response.interact_pointer_pos()
        && let Some((id, ..)) = note_at(layout, clip, pos)
    {
        app.session.edit(vec![Op::NoteRemove {
            track,
            clip: clip_id,
            id,
        }]);
    }
}

fn moved_note(note: &Note, dt: i64, dp: i32) -> Note {
    Note {
        start: Tick((note.start.0 + dt).max(0)),
        pitch: (note.pitch as i32 + dp).clamp(0, 127) as u8,
        ..*note
    }
}

fn resized_dur(note: &Note, dd: i64) -> Tick {
    Tick((note.dur.0 + dd).max(MIN_NOTE.min(note.dur.0)))
}

fn interact_velocity(
    app: &mut DawApp,
    ui: &Ui,
    layout: &RollLayout,
    track: TrackId,
    clip_id: ClipId,
    clip: &Clip,
) {
    let response = ui.interact(
        layout.velocity,
        Id::unique("roll-velocity"),
        Sense::click_and_drag(),
    );
    if !response.is_pointer_button_down_on() {
        app.roll.vel_gesture = None;
        return;
    }
    let Some(pos) = response.interact_pointer_pos() else {
        return;
    };
    app.sel.focus = Focus::Notes;
    let gesture = match app.roll.vel_gesture {
        Some(gesture) => gesture,
        None => *app.roll.vel_gesture.insert(app.session.new_gesture()),
    };
    let rect = layout.velocity;
    let level = (rect.bottom() - pos.y) / (rect.height() - 6.0);
    let vel = (1.0 + level.clamp(0.0, 1.0) * 126.0).round() as u8;
    let eligible = |id: &NoteId| app.sel.notes.is_empty() || app.sel.notes.contains(id);
    let tick = layout.rel(pos.x);
    let near = |n: &Note| (layout.axis.x(layout.clip_start + n.start) - pos.x).abs() <= 5.0;
    let under = |n: &Note| tick >= n.start.0 as f64 && tick < (n.start + n.dur).0 as f64;
    let mut hits: Vec<NoteId> = clip
        .notes
        .iter()
        .filter(|(id, n)| eligible(id) && near(n))
        .map(|(id, _)| *id)
        .collect();
    if hits.is_empty() {
        hits = clip
            .notes
            .iter()
            .filter(|(id, n)| eligible(id) && under(n))
            .map(|(id, _)| *id)
            .collect();
    }
    let ops: Vec<Op> = hits
        .into_iter()
        .map(|id| Op::NoteSet {
            track,
            clip: clip_id,
            id,
            prop: NoteProp::Vel(vel),
        })
        .collect();
    if !ops.is_empty() {
        app.roll.velocity = vel;
        app.session.edit_gesture(ops, gesture);
    }
}

fn is_black(pitch: u8) -> bool {
    matches!(pitch % 12, 1 | 3 | 6 | 8 | 10)
}

fn visible_pitches(layout: &RollLayout) -> impl Iterator<Item = u8> {
    let rect = layout.grid;
    (0..=127u8).filter(move |p| layout.y(*p) < rect.bottom() && layout.y(*p) + NOTE_H > rect.top())
}

fn paint_rows(painter: &Painter, layout: &RollLayout) {
    let grid = layout.grid;
    for pitch in visible_pitches(layout) {
        let top = layout.y(pitch);
        let row = Rect::from_min_max(pos2(grid.left(), top), pos2(grid.right(), top + NOTE_H));
        painter.rect_filled(
            row,
            0.0,
            if is_black(pitch) {
                theme::ROW_BLACK
            } else {
                theme::LANE
            },
        );
        if pitch % 12 == 0 {
            painter.hline(grid.x_range(), top + NOTE_H, Stroke::new(1.0, theme::GRID_BEAT));
        }
    }
}

fn paint_notes(app: &DawApp, painter: &Painter, layout: &RollLayout, clip: &Clip, color: Color32) {
    for (id, note) in &clip.notes {
        let selected = app.sel.notes.contains(id);
        let shown = match app.roll.drag {
            Drag::Move { dt, dp, .. } if selected => moved_note(note, dt, dp),
            Drag::Resize { dd, .. } if selected => Note {
                dur: resized_dur(note, dd),
                ..*note
            },
            _ => *note,
        };
        let rect = layout.note_rect(&shown);
        if !rect.intersects(layout.grid) {
            continue;
        }
        painter.rect_filled(
            rect,
            2.0,
            theme::mix(theme::CANVAS, color, 0.4 + 0.6 * shown.vel as f32 / 127.0),
        );
        let stroke = if selected {
            Stroke::new(1.5, theme::SELECTION)
        } else {
            Stroke::new(1.0, Color32::BLACK)
        };
        painter.rect_stroke(rect, 2.0, stroke, StrokeKind::Inside);
    }
    if let Drag::New { start, dur, pitch } = app.roll.drag {
        let rect = layout.note_rect(&Note {
            start,
            dur,
            pitch,
            vel: app.roll.velocity,
        });
        painter.rect_filled(rect, 2.0, color.gamma_multiply(0.5));
        painter.rect_stroke(rect, 2.0, Stroke::new(1.0, theme::SELECTION), StrokeKind::Inside);
    }
}

fn paint_velocity(app: &DawApp, painter: &Painter, layout: &RollLayout, clip: &Clip, color: Color32) {
    let rect = layout.velocity;
    painter.rect_filled(rect, 0.0, theme::RULER);
    for (id, note) in &clip.notes {
        let x = layout.axis.x(layout.clip_start + note.start);
        let top = rect.bottom() - (rect.height() - 6.0) * note.vel as f32 / 127.0;
        let selected = app.sel.notes.contains(id);
        let color = if selected { theme::SELECTION } else { color };
        painter.vline(x + 1.0, top..=rect.bottom(), Stroke::new(2.0, color));
        painter.rect_filled(
            Rect::from_center_size(pos2(x + 1.0, top), egui::vec2(6.0, 6.0)),
            1.0,
            color,
        );
    }
}

fn paint_keys(app: &DawApp, painter: &Painter, layout: &RollLayout) {
    let keys = layout.keys;
    painter.rect_filled(keys, 0.0, theme::KEY_WHITE);
    let sounding = app.roll.preview.map(|(_, pitch)| pitch);
    for pitch in visible_pitches(layout) {
        let top = layout.y(pitch);
        let row = Rect::from_min_max(pos2(keys.left(), top), pos2(keys.right(), top + NOTE_H));
        if sounding == Some(pitch) {
            painter.rect_filled(row, 0.0, theme::LOOP);
        } else if is_black(pitch) {
            let key = Rect::from_min_max(row.min, pos2(row.left() + KEYS_W * 0.62, row.bottom()));
            painter.rect_filled(key, 0.0, theme::KEY_BLACK);
        }
        if matches!(pitch % 12, 0 | 5) {
            painter.hline(keys.x_range(), top + NOTE_H, Stroke::new(1.0, theme::TEXT_DIM));
        }
        if pitch % 12 == 0 {
            let at = pos2(keys.right() - 4.0, top + NOTE_H / 2.0);
            let name = format!("C{}", pitch as i32 / 12 - 1);
            painter.text(
                at,
                Align2::RIGHT_CENTER,
                name,
                FontId::proportional(10.0),
                Color32::BLACK,
            );
        }
    }
}
