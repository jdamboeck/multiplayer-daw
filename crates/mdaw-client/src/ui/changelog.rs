//! The changelog: checkouts and commits with their samples, edits that lost against someone
//! else's concurrent edit with a way back, and forks of checkouts that were ended.

use egui::{Button, RichText, Ui};
use mdaw_model::{ClipProp, EntityRef, MixProp, NoteProp, Op, Project, PropKey, ShellProp};
use mdaw_proto::{ReleaseReason, Superseded};

use super::checkout::sample_button;
use super::{format_position, gain_to_db};
use crate::session::{Displaced, LogEntry, Logged};
use crate::{DawApp, theme};

pub fn toolbar(app: &mut DawApp, ui: &mut Ui) {
    let count = app.session.displaced().len() + app.session.log().len();
    let text = match count {
        0 => "Nothing yet".to_string(),
        1 => "1 entry".to_string(),
        n => format!("{n} entries"),
    };
    ui.label(RichText::new(text).color(theme::TEXT_DIM));
}

/// One line of the changelog.
enum Row {
    Log(LogEntry),
    Displaced(Displaced),
}

impl Row {
    fn seq(&self) -> u64 {
        match self {
            Row::Log(entry) => entry.seq,
            Row::Displaced(entry) => entry.seq,
        }
    }
}

pub fn show(app: &mut DawApp, ui: &mut Ui) {
    let forks = app.session.forks().to_vec();
    if app.session.displaced().is_empty() && app.session.log().is_empty() && forks.is_empty() {
        ui.centered_and_justified(|ui| {
            let hint = "Commits of checked-out tracks are listed here, with samples to listen to.\n\
                        When two people change the same thing at the same moment, the last change wins;\n\
                        the one that lost is listed here too and can be brought back.";
            ui.label(RichText::new(hint).color(theme::TEXT_DIM));
        });
        return;
    }
    let project = app.session.project().clone();
    let mut rows: Vec<Row> = app.session.log().iter().cloned().map(Row::Log).collect();
    rows.extend(app.session.displaced().iter().cloned().map(Row::Displaced));
    // Newest first; within one event, in the order recorded.
    rows.reverse();
    rows.sort_by_key(|row| std::cmp::Reverse(row.seq()));

    let mut restore = None;
    let mut play = None;
    let mut fork_action = None;
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        ui.add_space(4.0);
        for (index, fork) in forks.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui
                    .button("Restore as new track")
                    .on_hover_text("Adds the track as you had made it in your checkout")
                    .clicked()
                {
                    fork_action = Some((index, true));
                }
                if ui.button("Discard").clicked() {
                    fork_action = Some((index, false));
                }
                let text = format!(
                    "Your changes to \"{}\", kept when your checkout was ended",
                    fork.shell.name
                );
                ui.label(RichText::new(text).color(theme::WARN));
            });
        }
        if !forks.is_empty() {
            ui.separator();
        }
        let mut last_seq = None;
        for row in &rows {
            // Samples belong to an event; show them with its first line only.
            let first = last_seq != Some(row.seq());
            last_seq = Some(row.seq());
            ui.horizontal_wrapped(|ui| {
                match row {
                    Row::Displaced(entry) => {
                        let possible = app.session.can_restore(&entry.lost);
                        let hint = if possible {
                            "Make this change again, as your own edit."
                        } else {
                            "Nothing to restore: the project already has this value, or what it applied to is gone."
                        };
                        if ui
                            .add_enabled(possible, Button::new("Restore"))
                            .on_hover_text(hint)
                            .on_disabled_hover_text(hint)
                            .clicked()
                        {
                            restore = Some(entry.lost.clone());
                        }
                        ui.label(describe(app, &project, entry));
                    }
                    Row::Log(entry) => {
                        ui.label(describe_log(app, &project, entry));
                    }
                }
                if first {
                    for sample in crate::checkout::distinct(app.session.samples(row.seq())) {
                        if sample_button(app, ui, &sample).clicked() {
                            play = Some(sample);
                        }
                    }
                }
            });
        }
    });
    if let Some(lost) = restore {
        app.session.restore(&lost);
    }
    if let Some(sample) = play {
        app.session.play_sample(&sample);
    }
    match fork_action {
        Some((index, true)) => {
            app.session.restore_fork(index);
        }
        Some((index, false)) => app.session.discard_fork(index),
        None => {}
    }
}

/// One line about a checkout, commit or release.
fn describe_log(app: &DawApp, project: &Project, entry: &LogEntry) -> String {
    let who = app.session.user_name(entry.by);
    let track = |id: &mdaw_model::TrackId| match project.track(*id) {
        Some(track) => format!("\"{}\"", track.shell.name),
        None => "a removed track".into(),
    };
    match &entry.what {
        Logged::Checkout { track: id } => format!("{who} checked out {}", track(id)),
        Logged::Commit {
            track: id, summary, ..
        } => {
            if summary.is_empty() {
                format!("{who} committed {}", track(id))
            } else {
                format!("{who} committed {}: {summary}", track(id))
            }
        }
        Logged::Release {
            track: id,
            reason: ReleaseReason::Abandoned,
        } => format!("{who} dropped the checkout of {}", track(id)),
        Logged::Release {
            track: id,
            reason: ReleaseReason::Forced { owner },
        } => {
            let whose = if *owner == app.session.user() {
                "your".to_string()
            } else {
                format!("{}'s", app.session.user_name(*owner))
            };
            format!("{who} ended {whose} checkout of {}", track(id))
        }
    }
}

/// One line saying whose edit was lost, what it was, and to whom.
pub fn describe(app: &DawApp, project: &Project, entry: &Displaced) -> String {
    let lost = &entry.lost;
    let loser = app.session.user_name(lost.lost_user);
    let what = what(project, lost);
    if lost.lost_seq == entry.seq {
        // The edit itself arrived too late: its target had been removed.
        format!("{loser}: {what}, lost because it had been removed")
    } else {
        let winner = app.session.user_name(entry.by);
        format!("{loser}: {what}, overwritten by {winner}")
    }
}

fn what(project: &Project, lost: &Superseded) -> String {
    let entity = entity_name(project, &lost.entity);
    if let Some(value) = value(&lost.lost) {
        return format!("{value} for {entity}");
    }
    match lost.prop {
        PropKey::Existence if lost.lost.is_removal() => format!("removing {entity}"),
        PropKey::Existence => format!("adding {entity}"),
        PropKey::Order => format!("moving {entity}"),
        PropKey::Param(_) => format!("a setting of {entity}"),
        other => format!("{} of {entity}", format!("{other:?}").to_lowercase()),
    }
}

fn entity_name(project: &Project, entity: &EntityRef) -> String {
    let on = |what: &str| match entity.track().and_then(|id| project.track(id)) {
        Some(track) => format!("{what} on \"{}\"", track.shell.name),
        None => format!("{what} on a removed track"),
    };
    match entity {
        EntityRef::Project => "the project".into(),
        EntityRef::Tempo(_) => "the tempo".into(),
        EntityRef::Track(id) => match project.track(*id) {
            Some(track) => format!("track \"{}\"", track.shell.name),
            None => "a removed track".into(),
        },
        EntityRef::Clip(..) => on("a clip"),
        EntityRef::Note(..) => on("a note"),
        EntityRef::Device(..) => on("a device"),
    }
}

/// The value an op wanted to set, where it reads well.
fn value(op: &Op) -> Option<String> {
    let db = |gain: f32| format!("{:.1} dB", gain_to_db(gain));
    let at = |tick: mdaw_model::Tick| format_position(tick.0 as f64).trim().to_string();
    Some(match op {
        Op::ProjectSetName { name } => format!("name \"{name}\""),
        Op::MasterSetGain { gain } => format!("volume {}", db(*gain)),
        Op::TempoPut { point, .. } => format!("{:.1} bpm", point.milli_bpm as f64 / 1000.0),
        Op::TrackSetShell {
            prop: ShellProp::Name(name),
            ..
        } => format!("name \"{name}\""),
        Op::TrackSetMix { prop, .. } => match prop {
            MixProp::Gain(gain) => format!("volume {}", db(*gain)),
            MixProp::Pan(pan) => format!("pan {pan:+.2}"),
            MixProp::Mute(true) => "mute on".into(),
            MixProp::Mute(false) => "mute off".into(),
        },
        Op::ClipSet { prop, .. } => match prop {
            ClipProp::Start(tick) => format!("start {}", at(*tick)),
            ClipProp::Length(length) => format!(
                "length {:.2} bars",
                length.0 as f64 / mdaw_model::TICKS_PER_BAR as f64
            ),
            ClipProp::Name(name) => format!("name \"{name}\""),
        },
        Op::NoteSet { prop, .. } => match prop {
            NoteProp::Start(tick) => format!("start {}", at(*tick)),
            NoteProp::Dur(dur) => format!("length {:.2} beats", dur.0 as f64 / mdaw_model::PPQ as f64),
            NoteProp::Pitch(pitch) => format!("pitch {}", pitch_name(*pitch)),
            NoteProp::Vel(vel) => format!("velocity {vel}"),
        },
        _ => return None,
    })
}

fn pitch_name(pitch: u8) -> String {
    const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    format!("{}{}", NAMES[pitch as usize % 12], pitch as i32 / 12 - 1)
}
