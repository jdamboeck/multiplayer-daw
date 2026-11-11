//! The arrangement: track headers on the left, clips on a timeline on the right.

use std::sync::Arc;

use egui::{
    Align2, Button, Color32, CursorIcon, DragValue, FontId, Painter, Pos2, Rect, Sense, Stroke, StrokeKind,
    TextEdit, Ui, UiBuilder, pos2, vec2,
};
use mdaw_model::{
    Clip, ClipId, ClipProp, EntityRef, MixProp, Op, PPQ, Project, ShellProp, TICKS_PER_BAR, Tick, Track,
    TrackId,
};

use super::{
    MAX_DB, MIN_DB, TimeAxis, db_to_gain, gain_to_db, paint_grid, paint_playhead, paint_ruler, snap_ceil,
    snap_floor, snap_round,
};
use crate::{DawApp, Focus, theme};

pub const HEADER_W: f32 = 232.0;
pub const RULER_H: f32 = 26.0;
pub const ROW_H: f32 = 64.0;
/// Width of the zone at a clip's right edge that resizes it.
const EDGE: f32 = 7.0;

pub struct ArrangeState {
    pub px_per_beat: f32,
    pub scroll_x: f32,
    pub scroll_y: f32,
    drag: Drag,
    /// Tick the ruler last sent to the engine, so a held pointer does not flood it.
    last_seek: Option<Tick>,
    /// Where things were in the last frame.
    pub layout: Option<ArrangeLayout>,
}

impl Default for ArrangeState {
    fn default() -> Self {
        Self {
            px_per_beat: 24.0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            drag: Drag::None,
            last_seek: None,
            layout: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ArrangeLayout {
    pub ruler: Rect,
    pub lanes: Rect,
    pub headers: Rect,
    pub axis: TimeAxis,
    pub scroll_y: f32,
}

impl ArrangeLayout {
    pub fn row_top(&self, row: usize) -> f32 {
        self.lanes.top() - self.scroll_y + row as f32 * ROW_H
    }

    /// The middle of a row at a tick.
    pub fn pos(&self, row: usize, tick: Tick) -> Pos2 {
        pos2(self.axis.x(tick), self.row_top(row) + ROW_H / 2.0)
    }

    pub fn row_at(&self, y: f32) -> Option<usize> {
        let row = ((y - self.lanes.top() + self.scroll_y) / ROW_H).floor();
        (row >= 0.0).then_some(row as usize)
    }
}

#[derive(Clone, Copy, Debug)]
enum Drag {
    None,
    New {
        track: TrackId,
        anchor: Tick,
        start: Tick,
        end: Tick,
    },
    Move {
        track: TrackId,
        clip: ClipId,
        origin: f64,
        from: Tick,
        start: Tick,
    },
    Resize {
        track: TrackId,
        clip: ClipId,
        length: Tick,
    },
    Loop {
        anchor: Tick,
    },
}

enum Hit {
    Clip {
        track: TrackId,
        clip: ClipId,
        edge: bool,
    },
    Lane {
        track: TrackId,
    },
    Nothing,
}

type Tracks = Vec<(TrackId, Arc<Track>)>;

pub fn show(app: &mut DawApp, ui: &mut Ui) {
    let project = app.session.project().clone();
    let tracks: Tracks = project
        .ordered_tracks()
        .map(|(id, track)| (id, track.clone()))
        .collect();

    let full = ui.available_rect_before_wrap();
    ui.allocate_rect(full, Sense::hover());
    let split = pos2(full.left() + HEADER_W, full.top() + RULER_H);
    let corner = Rect::from_min_max(full.min, split);
    let ruler = Rect::from_min_max(pos2(split.x, full.top()), pos2(full.right(), split.y));
    let headers = Rect::from_min_max(pos2(full.left(), split.y), pos2(split.x, full.bottom()));
    let lanes = Rect::from_min_max(split, full.max);

    scroll_and_zoom(app, ui, full, lanes, tracks.len());
    let state = &app.arrange;
    let axis = TimeAxis {
        x0: lanes.left() - state.scroll_x,
        px_per_tick: state.px_per_beat / PPQ as f32,
    };
    let layout = ArrangeLayout {
        ruler,
        lanes,
        headers,
        axis,
        scroll_y: state.scroll_y,
    };
    app.arrange.layout = Some(layout);
    let snap = if app.arrange.px_per_beat >= 14.0 {
        PPQ
    } else {
        TICKS_PER_BAR
    };

    ui.painter_at(full).rect_filled(full, 0.0, theme::CANVAS);
    interact_ruler(app, ui, &layout, snap);
    interact_lanes(app, ui, &layout, &tracks, snap);

    paint_lanes(app, &ui.painter_at(lanes), &layout, &tracks);
    let loop_range = app.loop_on.then_some((app.loop_range.start, app.loop_range.end));
    paint_ruler(&ui.painter_at(ruler), ruler, &axis, loop_range);
    let playhead = axis.x_f(app.session.engine().position());
    paint_playhead(&ui.painter_at(ruler.union(lanes)), playhead, ruler, lanes);

    ui.painter_at(headers).rect_filled(headers, 0.0, theme::CANVAS);
    for (row, (id, track)) in tracks.iter().enumerate() {
        let top = layout.row_top(row);
        let rect = Rect::from_min_max(pos2(headers.left(), top), pos2(headers.right(), top + ROW_H));
        if rect.intersects(headers) {
            header(app, ui, rect, headers, *id, track);
        }
    }
    let painter = ui.painter_at(full);
    painter.rect_filled(corner, 0.0, theme::RULER);
    painter.hline(
        corner.x_range(),
        corner.bottom(),
        Stroke::new(1.0, Color32::BLACK),
    );
    painter.vline(split.x, full.y_range(), Stroke::new(1.0, Color32::BLACK));
    if ui
        .put(corner.shrink2(vec2(8.0, 3.0)), Button::new("+ Track").small())
        .clicked()
    {
        app.add_track();
    }
    let _ = &project;
}

fn scroll_and_zoom(app: &mut DawApp, ui: &Ui, full: Rect, lanes: Rect, rows: usize) {
    let state = &mut app.arrange;
    if ui.rect_contains_pointer(full) {
        let (scroll, zoom, pointer) =
            ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta(), i.pointer.hover_pos()));
        if zoom != 1.0 {
            let pivot = pointer.map_or(lanes.left(), |p| p.x.max(lanes.left())) - lanes.left();
            let beats = (state.scroll_x + pivot) / state.px_per_beat;
            state.px_per_beat = (state.px_per_beat * zoom).clamp(3.0, 400.0);
            state.scroll_x = beats * state.px_per_beat - pivot;
        }
        state.scroll_x -= scroll.x;
        state.scroll_y -= scroll.y;
    }
    let content = rows as f32 * ROW_H + ROW_H;
    state.scroll_x = state.scroll_x.max(0.0);
    state.scroll_y = state.scroll_y.clamp(0.0, (content - lanes.height()).max(0.0));
}

fn interact_ruler(app: &mut DawApp, ui: &Ui, layout: &ArrangeLayout, snap: i64) {
    let response = ui.interact(
        layout.ruler,
        egui::Id::unique("arrange-ruler"),
        Sense::click_and_drag(),
    );
    let (pressed, shift, free) =
        ui.input(|i| (i.pointer.primary_pressed(), i.modifiers.shift, i.modifiers.alt));
    if !response.is_pointer_button_down_on() {
        if matches!(app.arrange.drag, Drag::Loop { .. }) {
            app.arrange.drag = Drag::None;
        }
        app.arrange.last_seek = None;
        return;
    }
    let Some(pos) = response.interact_pointer_pos() else {
        return;
    };
    let tick = layout.axis.tick(pos.x).max(0.0);
    let tick = if free {
        Tick(tick as i64)
    } else {
        snap_round(tick, snap)
    };
    if pressed && shift {
        app.arrange.drag = Drag::Loop { anchor: tick };
    }
    if let Drag::Loop { anchor } = app.arrange.drag {
        if tick != anchor {
            app.loop_range = mdaw_model::TickRange::new(anchor.min(tick), anchor.max(tick));
            app.loop_on = true;
            app.apply_loop();
        }
    } else if app.arrange.last_seek != Some(tick) {
        app.arrange.last_seek = Some(tick);
        app.session.seek(tick);
    }
}

fn hit(layout: &ArrangeLayout, tracks: &Tracks, pos: Pos2) -> Hit {
    let Some((track, data)) = layout.row_at(pos.y).and_then(|row| tracks.get(row)) else {
        return Hit::Nothing;
    };
    let mut found = None;
    for (id, clip) in &data.content.clips {
        let (left, right) = (layout.axis.x(clip.start), layout.axis.x(clip.start + clip.length));
        if pos.x >= left && pos.x < right {
            found = Some((*id, pos.x >= right - EDGE && right - left > 2.0 * EDGE));
        }
    }
    match found {
        Some((clip, edge)) => Hit::Clip {
            track: *track,
            clip,
            edge,
        },
        None => Hit::Lane { track: *track },
    }
}

fn clip_of(tracks: &Tracks, track: TrackId, clip: ClipId) -> Option<&Arc<Clip>> {
    tracks
        .iter()
        .find(|(id, _)| *id == track)?
        .1
        .content
        .clips
        .get(&clip)
}

fn interact_lanes(app: &mut DawApp, ui: &Ui, layout: &ArrangeLayout, tracks: &Tracks, snap: i64) {
    let response = ui.interact(
        layout.lanes,
        egui::Id::unique("arrange-lanes"),
        Sense::click_and_drag(),
    );
    let axis = layout.axis;
    let origin = ui.input(|i| i.pointer.press_origin());

    if let Some(pos) = response.hover_pos()
        && let Hit::Clip { track, clip, edge } = hit(layout, tracks, pos)
    {
        if app.session.locked_by(&EntityRef::Clip(track, clip)).is_some() {
            ui.ctx().set_cursor_icon(CursorIcon::NotAllowed);
        } else if edge {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }
    }
    if matches!(app.arrange.drag, Drag::Resize { .. }) {
        ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
    }

    if response.drag_started()
        && let Some(origin) = origin
    {
        app.sel.focus = Focus::Arrange;
        let tick = axis.tick(origin.x).max(0.0);
        app.arrange.drag = match hit(layout, tracks, origin) {
            Hit::Clip { track, clip, edge } => match clip_of(tracks, track, clip) {
                Some(data) => {
                    if app.sel.clip != Some((track, clip)) {
                        app.sel.notes.clear();
                    }
                    app.sel.track = Some(track);
                    app.sel.clip = Some((track, clip));
                    if app.session.locked_by(&EntityRef::Clip(track, clip)).is_some() {
                        // Someone else is moving it. Wait for them rather than fight over it.
                        Drag::None
                    } else if edge {
                        Drag::Resize {
                            track,
                            clip,
                            length: data.length,
                        }
                    } else {
                        Drag::Move {
                            track,
                            clip,
                            origin: tick,
                            from: data.start,
                            start: data.start,
                        }
                    }
                }
                None => Drag::None,
            },
            Hit::Lane { track } => {
                app.sel.track = Some(track);
                let anchor = snap_floor(tick, snap);
                Drag::New {
                    track,
                    anchor,
                    start: anchor,
                    end: anchor + Tick(snap),
                }
            }
            Hit::Nothing => Drag::None,
        };
    }

    if response.dragged()
        && let Some(pos) = response.interact_pointer_pos()
    {
        let tick = axis.tick(pos.x).max(0.0);
        match &mut app.arrange.drag {
            Drag::New {
                anchor, start, end, ..
            } => {
                if tick >= anchor.0 as f64 {
                    (*start, *end) = (*anchor, snap_ceil(tick, snap).max(*anchor + Tick(snap)));
                } else {
                    (*start, *end) = (snap_floor(tick, snap), *anchor + Tick(snap));
                }
            }
            Drag::Move {
                origin, from, start, ..
            } => {
                *start = (*from + snap_round(tick - *origin, snap)).max(Tick::ZERO);
            }
            Drag::Resize { track, clip, length } => {
                if let Some(data) = clip_of(tracks, *track, *clip) {
                    let end = snap_round(tick, snap).max(snap_floor(data.start.0 as f64, snap) + Tick(snap));
                    *length = (end - data.start).max(Tick(1));
                }
            }
            Drag::None | Drag::Loop { .. } => {}
        }
    }

    if let Drag::Move { track, clip, .. } | Drag::Resize { track, clip, .. } = app.arrange.drag {
        app.editing = Some(EntityRef::Clip(track, clip));
    }

    if response.drag_stopped() {
        match std::mem::replace(&mut app.arrange.drag, Drag::None) {
            Drag::New {
                track, start, end, ..
            } => {
                let id = app.session.ids().clip();
                let clip = Clip {
                    start,
                    length: end - start,
                    name: String::new(),
                    notes: Default::default(),
                };
                if app.session.edit(vec![Op::ClipAdd { track, id, clip }]) {
                    app.sel.notes.clear();
                    app.sel.clip = Some((track, id));
                }
            }
            Drag::Move {
                track, clip, start, ..
            } => {
                app.session.edit(vec![Op::ClipSet {
                    track,
                    id: clip,
                    prop: ClipProp::Start(start),
                }]);
            }
            Drag::Resize { track, clip, length } => {
                app.session.edit(vec![Op::ClipSet {
                    track,
                    id: clip,
                    prop: ClipProp::Length(length),
                }]);
            }
            Drag::None | Drag::Loop { .. } => {}
        }
    } else if response.clicked()
        && let Some(pos) = response.interact_pointer_pos()
    {
        app.sel.focus = Focus::Arrange;
        let double = response.double_clicked();
        match hit(layout, tracks, pos) {
            Hit::Clip { track, clip, .. } if double => app.open_clip(track, clip),
            Hit::Clip { track, clip, .. } => {
                if app.sel.clip != Some((track, clip)) {
                    app.sel.notes.clear();
                }
                app.sel.track = Some(track);
                app.sel.clip = Some((track, clip));
            }
            Hit::Lane { track } if double => {
                let start = snap_floor(axis.tick(pos.x).max(0.0), TICKS_PER_BAR);
                let id = app.session.ids().clip();
                let clip = Clip {
                    start,
                    length: Tick::bars(1),
                    name: String::new(),
                    notes: Default::default(),
                };
                if app.session.edit(vec![Op::ClipAdd { track, id, clip }]) {
                    app.open_clip(track, id);
                }
            }
            Hit::Lane { track } => {
                app.sel.track = Some(track);
                app.sel.clip = None;
                app.sel.notes.clear();
            }
            Hit::Nothing => {}
        }
    }
}

fn paint_lanes(app: &DawApp, painter: &Painter, layout: &ArrangeLayout, tracks: &Tracks) {
    let lanes = layout.lanes;
    for (row, (id, _)) in tracks.iter().enumerate() {
        let top = layout.row_top(row);
        let rect = Rect::from_min_max(pos2(lanes.left(), top), pos2(lanes.right(), top + ROW_H));
        let fill = if app.sel.track == Some(*id) {
            theme::LANE_SELECTED
        } else if row % 2 == 0 {
            theme::LANE
        } else {
            theme::LANE_ALT
        };
        painter.rect_filled(rect, 0.0, fill);
    }
    let bottom = layout.row_top(tracks.len()).min(lanes.bottom());
    let body = Rect::from_min_max(lanes.min, pos2(lanes.right(), bottom));
    paint_grid(painter, body, &layout.axis, 12.0);
    for row in 1..=tracks.len() {
        painter.hline(
            lanes.x_range(),
            layout.row_top(row),
            Stroke::new(1.0, Color32::BLACK),
        );
    }

    for (row, (track_id, track)) in tracks.iter().enumerate() {
        let top = layout.row_top(row);
        if top > lanes.bottom() || top + ROW_H < lanes.top() {
            continue;
        }
        let color = theme::rgb(track.shell.color);
        let color = if track.content.mute {
            theme::mix(color, theme::LANE, 0.6)
        } else {
            color
        };
        // Someone else's checkout: shown as it was, and not to be touched.
        let locked = track
            .checkout
            .filter(|c| c.user != app.session.user())
            .map(|c| app.session.user_name(c.user));
        let color = if locked.is_some() {
            theme::mix(color, theme::LANE, 0.5)
        } else {
            color
        };
        if let Some(owner) = &locked {
            let stale = if app.session.is_stale(*track_id) {
                ", not heard from lately"
            } else {
                ""
            };
            let band = Rect::from_min_max(pos2(lanes.left(), top), pos2(lanes.right(), top + ROW_H));
            let text = format!("checked out by {owner}{stale}");
            painter.with_clip_rect(band.intersect(painter.clip_rect())).text(
                pos2(lanes.left() + 8.0, top + ROW_H - 4.0),
                Align2::LEFT_BOTTOM,
                text,
                FontId::proportional(11.0),
                theme::WARN,
            );
        }
        for (clip_id, clip) in &track.content.clips {
            let (mut start, mut length) = (clip.start, clip.length);
            match app.arrange.drag {
                Drag::Move {
                    track,
                    clip,
                    start: moved,
                    ..
                } if (track, clip) == (*track_id, *clip_id) => start = moved,
                Drag::Resize {
                    track,
                    clip,
                    length: resized,
                } if (track, clip) == (*track_id, *clip_id) => length = resized,
                _ => {}
            }
            let rect = Rect::from_min_max(
                pos2(layout.axis.x(start), top + 2.0),
                pos2(layout.axis.x(start + length), top + ROW_H - 2.0),
            );
            if rect.right() >= lanes.left() && rect.left() <= lanes.right() {
                let selected = app.sel.clip == Some((*track_id, *clip_id));
                match app.session.locked_by(&EntityRef::Clip(*track_id, *clip_id)) {
                    Some(holder) => {
                        let dimmed = theme::mix(color, theme::LANE, 0.65);
                        paint_clip(painter, rect, clip, length, dimmed, selected);
                        paint_holder(painter, rect, holder);
                    }
                    None => paint_clip(painter, rect, clip, length, color, selected),
                }
            }
        }
        if let Drag::New {
            track, start, end, ..
        } = app.arrange.drag
            && track == *track_id
        {
            let rect = Rect::from_min_max(
                pos2(layout.axis.x(start), top + 2.0),
                pos2(layout.axis.x(end), top + ROW_H - 2.0),
            );
            painter.rect_filled(rect, 4.0, color.gamma_multiply(0.3));
            painter.rect_stroke(rect, 4.0, Stroke::new(1.0, color), StrokeKind::Inside);
        }
    }
}

fn paint_clip(painter: &Painter, rect: Rect, clip: &Clip, length: Tick, color: Color32, selected: bool) {
    painter.rect_filled(rect, 4.0, theme::mix(color, theme::LANE, 0.62));
    let inner = rect.shrink2(vec2(2.0, 5.0));
    let notes = painter.with_clip_rect(rect.intersect(painter.clip_rect()));
    if !clip.notes.is_empty() && length.0 > 0 {
        let low = clip
            .notes
            .values()
            .map(|n| n.pitch)
            .min()
            .unwrap_or(60)
            .saturating_sub(2) as f32;
        let high = (clip.notes.values().map(|n| n.pitch).max().unwrap_or(60) + 2).min(127) as f32;
        let span = (high - low).max(12.0);
        for note in clip.notes.values().filter(|n| n.start < length) {
            let x0 = inner.left() + inner.width() * note.start.0 as f32 / length.0 as f32;
            let x1 =
                inner.left() + inner.width() * (note.start + note.dur).min(length).0 as f32 / length.0 as f32;
            let y = inner.bottom() - inner.height() * (note.pitch as f32 - low) / span;
            let bar = Rect::from_min_max(pos2(x0, y - 1.0), pos2(x1.max(x0 + 1.5), y + 1.0));
            notes.rect_filled(bar, 0.0, color);
        }
    }
    if !clip.name.is_empty() && rect.width() > 30.0 {
        let at = rect.left_top() + vec2(5.0, 3.0);
        notes.text(
            at,
            Align2::LEFT_TOP,
            &clip.name,
            FontId::proportional(11.0),
            theme::TEXT,
        );
    }
    let stroke = if selected {
        Stroke::new(1.5, theme::SELECTION)
    } else {
        Stroke::new(1.0, color)
    };
    painter.rect_stroke(rect, 4.0, stroke, StrokeKind::Inside);
}

/// Marks a clip as being moved by someone else.
fn paint_holder(painter: &Painter, rect: Rect, holder: &str) {
    let painter = painter.with_clip_rect(rect.intersect(painter.clip_rect()));
    let font = FontId::proportional(11.0);
    let galley = painter.layout_no_wrap(holder.to_string(), font, Color32::BLACK);
    let tag = Rect::from_min_size(
        rect.left_bottom() - vec2(0.0, galley.size().y + 4.0),
        galley.size() + vec2(10.0, 4.0),
    );
    painter.rect_filled(tag, 4.0, theme::WARN);
    painter.galley(tag.min + vec2(5.0, 2.0), galley, Color32::BLACK);
}

fn header(app: &mut DawApp, ui: &mut Ui, rect: Rect, clip: Rect, id: TrackId, track: &Track) {
    let selected = app.sel.track == Some(id);
    let builder = UiBuilder::new().id_salt(("track", id.0.0)).max_rect(rect);
    ui.scope_builder(builder, |ui| {
        ui.set_clip_rect(rect.intersect(clip));
        let background = ui.interact(rect, ui.scope_id().with("row"), Sense::click());
        let painter = ui.painter().clone();
        painter.rect_filled(
            rect,
            0.0,
            if selected {
                theme::HEADER_SELECTED
            } else {
                theme::HEADER
            },
        );
        let stripe = Rect::from_min_max(rect.min, pos2(rect.left() + 5.0, rect.bottom()));
        painter.rect_filled(stripe, 0.0, theme::rgb(track.shell.color));
        painter.hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, Color32::BLACK));

        // Name: double-click to rename.
        let name = Rect::from_min_size(rect.min + vec2(14.0, 7.0), vec2(rect.width() - 156.0, 20.0));
        let renaming = matches!(&app.rename, Some((track, ..)) if *track == id);
        if renaming {
            let Some((_, text, focused)) = &mut app.rename else {
                return;
            };
            let response = ui.put(name, TextEdit::singleline(text).desired_width(name.width()));
            if !*focused {
                response.request_focus();
                *focused = true;
            } else if response.lost_focus() {
                let cancelled = ui.input(|i| i.key_pressed(egui::Key::Escape));
                if let Some((_, text, _)) = app.rename.take()
                    && !cancelled
                    && !text.trim().is_empty()
                {
                    let prop = ShellProp::Name(mdaw_model::fit_name(text.trim(), ""));
                    app.session.edit(vec![Op::TrackSetShell { id, prop }]);
                }
            }
        } else {
            let text = painter.with_clip_rect(name.intersect(painter.clip_rect()));
            let color = if track.content.mute {
                theme::TEXT_DIM
            } else {
                theme::TEXT
            };
            let written = text.text(
                name.left_center(),
                Align2::LEFT_CENTER,
                &track.shell.name,
                FontId::proportional(14.0),
                color,
            );
            // Who else plays on the track right now.
            let players = app.session.players(id);
            if !players.is_empty() {
                text.text(
                    pos2(written.right() + 8.0, name.center().y),
                    Align2::LEFT_CENTER,
                    format!("{} playing", players.join(", ")),
                    FontId::proportional(12.0),
                    theme::METER,
                );
            }
        }

        // Mute and delete.
        let button = |n: f32| {
            Rect::from_min_size(
                pos2(rect.right() - 8.0 - 26.0 * n, rect.top() + 7.0),
                vec2(22.0, 20.0),
            )
        };
        if ui
            .put(button(2.0), Button::new("M").selected(track.content.mute))
            .on_hover_text("Mute")
            .clicked()
        {
            let prop = MixProp::Mute(!track.content.mute);
            app.session.edit(vec![Op::TrackSetMix { track: id, prop }]);
        }
        if ui
            .put(button(1.0), Button::new("×"))
            .on_hover_text("Delete the track")
            .clicked()
        {
            app.remove_track(id);
        }
        let left_of_mute = button(2.0).left();
        let slot = Rect::from_min_max(
            pos2(left_of_mute - 78.0, rect.top() + 7.0),
            pos2(left_of_mute - 4.0, rect.top() + 27.0),
        );
        checkout_button(app, ui, slot, id);

        // Volume and pan.
        let line = rect.top() + 35.0;
        let mut db = gain_to_db(track.content.gain);
        let volume = DragValue::new(&mut db)
            .range(MIN_DB..=MAX_DB)
            .speed(0.2)
            .fixed_decimals(1)
            .suffix(" dB");
        let response = ui.put(
            Rect::from_min_size(pos2(rect.left() + 14.0, line), vec2(82.0, 20.0)),
            volume,
        );
        let response = response.on_hover_text("Volume");
        if let Some(gesture) = app.gestures.track(&mut app.session, &response) {
            let prop = MixProp::Gain(db_to_gain(db));
            app.session
                .edit_gesture(vec![Op::TrackSetMix { track: id, prop }], gesture);
        }
        let mut pan = track.content.pan;
        let widget = DragValue::new(&mut pan)
            .range(-1.0..=1.0)
            .speed(0.01)
            .fixed_decimals(2)
            .prefix("pan ");
        let response = ui.put(
            Rect::from_min_size(pos2(rect.left() + 104.0, line), vec2(82.0, 20.0)),
            widget,
        );
        let response = response.on_hover_text("Pan: -1 is left, 1 is right");
        if let Some(gesture) = app.gestures.track(&mut app.session, &response) {
            let prop = MixProp::Pan(pan);
            app.session
                .edit_gesture(vec![Op::TrackSetMix { track: id, prop }], gesture);
        }

        if background.double_clicked() {
            app.rename = Some((id, track.shell.name.clone(), false));
        } else if background.clicked() {
            app.sel.track = Some(id);
            app.sel.focus = Focus::Arrange;
        }
    });
}

/// Check out, commit or free a track, depending on who has it.
fn checkout_button(app: &mut DawApp, ui: &mut Ui, rect: Rect, id: TrackId) {
    let session = &app.session;
    if session.linked().is_none() {
        // Alone there is nobody to keep out.
        return;
    }
    match session.checkout_owner(id) {
        None => {
            let enabled = session.can_check_out(id);
            let hint = "Check out: work on this track alone. The others hear it as it is now and \
                        cannot change it until you commit.";
            let response = ui.add_enabled_ui(enabled, |ui| {
                ui.put(
                    rect,
                    Button::new("Check out")
                        .small()
                        .wrap_mode(egui::TextWrapMode::Extend),
                )
            });
            if response
                .inner
                .on_hover_text(hint)
                .on_disabled_hover_text("Not connected to the server")
                .clicked()
            {
                app.session.check_out(id);
            }
        }
        Some(owner) if owner == session.user() => {
            let hint = "Your checkout: commit your changes, or drop them";
            let button = Button::new("Commit…")
                .small()
                .wrap_mode(egui::TextWrapMode::Extend)
                .fill(theme::ACCENT_DIM);
            if ui.put(rect, button).on_hover_text(hint).clicked() {
                app.open_commit(id);
            }
        }
        Some(owner) => {
            let name = session.user_name(owner);
            let stale = session.is_stale(id);
            let hint = if stale {
                format!(
                    "{name} has this track checked out, but has not been heard from for a while. Click to end the checkout."
                )
            } else {
                format!("{name} has this track checked out. Click to end the checkout.")
            };
            let label = if stale { "Stale" } else { "Locked" };
            if ui
                .put(rect, Button::new(label).small().fill(theme::WARN_DIM))
                .on_hover_text(hint)
                .clicked()
            {
                app.confirm_force = Some(id);
            }
        }
    }
}

/// Row index of a track, for tests and tools that drive the view by position.
pub fn row_of(project: &Project, track: TrackId) -> Option<usize> {
    project.ordered_tracks().position(|(id, _)| id == track)
}
