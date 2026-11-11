//! The views, and the helpers they share.

pub mod arrange;
pub mod changelog;
pub mod checkout;
pub mod devices;
pub mod live;
pub mod piano_roll;
pub mod transport;

use egui::{Align2, Color32, FontId, Painter, Rect, Stroke, pos2};
use mdaw_model::{PPQ, TICKS_PER_BAR, Tick};

use crate::theme;

/// Maps absolute ticks to screen x.
#[derive(Clone, Copy, Debug)]
pub struct TimeAxis {
    /// Screen x of tick 0.
    pub x0: f32,
    pub px_per_tick: f32,
}

impl TimeAxis {
    pub fn x(&self, tick: Tick) -> f32 {
        self.x0 + tick.0 as f32 * self.px_per_tick
    }

    pub fn x_f(&self, tick: f64) -> f32 {
        self.x0 + (tick * self.px_per_tick as f64) as f32
    }

    pub fn tick(&self, x: f32) -> f64 {
        ((x - self.x0) / self.px_per_tick) as f64
    }
}

pub fn snap_floor(tick: f64, grid: i64) -> Tick {
    Tick((tick / grid as f64).floor() as i64 * grid)
}

pub fn snap_ceil(tick: f64, grid: i64) -> Tick {
    Tick((tick / grid as f64).ceil() as i64 * grid)
}

pub fn snap_round(tick: f64, grid: i64) -> Tick {
    Tick((tick / grid as f64).round() as i64 * grid)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GridLine {
    /// A bar line, with the zero-based bar index.
    Bar(i64),
    Beat,
    Sub,
}

/// The grid lines between two screen x positions, no closer together than `min_px`.
pub fn grid_lines(axis: &TimeAxis, left: f32, right: f32, min_px: f32) -> Vec<(f32, GridLine)> {
    const STEPS: [i64; 5] = [PPQ / 8, PPQ / 4, PPQ / 2, PPQ, TICKS_PER_BAR];
    let mut step = STEPS
        .iter()
        .copied()
        .find(|s| *s as f32 * axis.px_per_tick >= min_px);
    let mut bars = TICKS_PER_BAR;
    while step.is_none() {
        bars *= 2;
        if bars as f32 * axis.px_per_tick >= min_px {
            step = Some(bars);
        }
    }
    let step = step.unwrap_or(TICKS_PER_BAR);
    let mut tick = snap_floor(axis.tick(left).max(0.0), step).0;
    let mut lines = Vec::new();
    while axis.x(Tick(tick)) <= right {
        let x = axis.x(Tick(tick));
        if x >= left {
            let line = if tick % TICKS_PER_BAR == 0 {
                GridLine::Bar(tick / TICKS_PER_BAR)
            } else if tick % PPQ == 0 {
                GridLine::Beat
            } else {
                GridLine::Sub
            };
            lines.push((x, line));
        }
        tick += step;
    }
    lines
}

/// Vertical grid lines over `rect`.
pub fn paint_grid(painter: &Painter, rect: Rect, axis: &TimeAxis, min_px: f32) {
    for (x, line) in grid_lines(axis, rect.left(), rect.right(), min_px) {
        let color = match line {
            GridLine::Bar(_) => theme::GRID_BAR,
            GridLine::Beat => theme::GRID_BEAT,
            GridLine::Sub => theme::GRID_SUB,
        };
        painter.vline(x, rect.y_range(), Stroke::new(1.0, color));
    }
}

/// A ruler with bar numbers, the loop region and tick marks.
pub fn paint_ruler(painter: &Painter, rect: Rect, axis: &TimeAxis, loop_range: Option<(Tick, Tick)>) {
    painter.rect_filled(rect, 0.0, theme::RULER);
    if let Some((start, end)) = loop_range {
        let region = Rect::from_min_max(pos2(axis.x(start), rect.top()), pos2(axis.x(end), rect.bottom()));
        painter.rect_filled(region, 0.0, theme::LOOP.gamma_multiply(0.35));
        painter.hline(region.x_range(), rect.top() + 1.0, Stroke::new(2.0, theme::LOOP));
    }
    let bar_px = TICKS_PER_BAR as f32 * axis.px_per_tick;
    let mut label_every = 1;
    while (label_every as f32) * bar_px < 44.0 {
        label_every *= 2;
    }
    for (x, line) in grid_lines(axis, rect.left(), rect.right(), 8.0) {
        match line {
            GridLine::Bar(bar) => {
                painter.vline(x, rect.y_range(), Stroke::new(1.0, theme::GRID_BAR));
                if bar % label_every == 0 {
                    let at = pos2(x + 4.0, rect.top() + 3.0);
                    painter.text(
                        at,
                        Align2::LEFT_TOP,
                        bar + 1,
                        FontId::proportional(11.0),
                        theme::TEXT_DIM,
                    );
                }
            }
            GridLine::Beat => {
                painter.vline(
                    x,
                    (rect.bottom() - 6.0)..=rect.bottom(),
                    Stroke::new(1.0, theme::GRID_BAR),
                );
            }
            GridLine::Sub => {}
        }
    }
    painter.hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, Color32::BLACK));
}

/// The playhead line, with a marker where it crosses the ruler.
pub fn paint_playhead(painter: &Painter, x: f32, ruler: Rect, body: Rect) {
    if x < ruler.left() || x > ruler.right() {
        return;
    }
    painter.vline(x, ruler.top()..=body.bottom(), Stroke::new(1.0, theme::PLAYHEAD));
    let top = ruler.bottom() - 8.0;
    let marker = vec![pos2(x - 5.0, top), pos2(x + 5.0, top), pos2(x, ruler.bottom())];
    painter.add(egui::Shape::convex_polygon(marker, theme::PLAYHEAD, Stroke::NONE));
}

/// "bar.beat.sixteenth", all counted from one.
pub fn format_position(tick: f64) -> String {
    let tick = tick.max(0.0) as i64;
    let bar = tick / TICKS_PER_BAR + 1;
    let beat = tick % TICKS_PER_BAR / PPQ + 1;
    let sixteenth = tick % PPQ / (PPQ / 4) + 1;
    format!("{bar:3}.{beat}.{sixteenth}")
}

pub fn format_time(seconds: f64) -> String {
    let seconds = seconds.max(0.0);
    let minutes = (seconds / 60.0) as u64;
    format!("{minutes}:{:06.3}", seconds - minutes as f64 * 60.0)
}

pub fn gain_to_db(gain: f32) -> f32 {
    if gain <= 0.001 {
        MIN_DB
    } else {
        20.0 * gain.log10()
    }
}

pub fn db_to_gain(db: f32) -> f32 {
    if db <= MIN_DB {
        0.0
    } else {
        10f32.powf(db / 20.0).min(MAX_GAIN)
    }
}

/// At and below this the fader is fully closed.
pub const MIN_DB: f32 = -60.0;
pub const MAX_DB: f32 = 12.0;
const MAX_GAIN: f32 = 3.981; // +12 dB, inside the model's limit of 4.

/// Groups the changes of one continuous interaction with a control into one undo step.
#[derive(Default)]
pub struct Gestures {
    widget: Option<egui::Id>,
    current: Option<mdaw_sync::Gesture>,
}

impl Gestures {
    /// Call with the control's response every frame. Returns the gesture to submit under when
    /// the control changed in this frame.
    pub fn track(
        &mut self,
        session: &mut crate::Session,
        response: &egui::Response,
    ) -> Option<mdaw_sync::Gesture> {
        let starts = response.drag_started() || response.gained_focus();
        let other_widget = self.widget != Some(response.id);
        if starts || (response.changed() && (other_widget || self.current.is_none())) {
            self.widget = Some(response.id);
            self.current = Some(session.new_gesture());
        }
        if response.changed() { self.current } else { None }
    }
}
