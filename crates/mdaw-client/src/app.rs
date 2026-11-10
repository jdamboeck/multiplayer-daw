//! Application state and the frame's top level.

use std::collections::BTreeSet;

use egui::{Key, Modifiers};
use mdaw_model::{ClipId, EntityRef, NoteId, Op, Tick, TickRange, TrackId};

use crate::session::{Session, new_track};
use crate::ui::{arrange, changelog, checkout, devices, piano_roll, transport};

/// The view that keyboard commands such as Delete act on: the one clicked last.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Focus {
    #[default]
    Arrange,
    Notes,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Tab {
    #[default]
    Notes,
    Device,
    Changelog,
}

#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub track: Option<TrackId>,
    pub clip: Option<(TrackId, ClipId)>,
    /// Notes of the selected clip.
    pub notes: BTreeSet<NoteId>,
    pub focus: Focus,
}

pub struct DawApp {
    pub session: Session,
    pub sel: Selection,
    pub tab: Tab,
    pub loop_on: bool,
    pub loop_range: TickRange,
    pub arrange: arrange::ArrangeState,
    pub roll: piano_roll::RollState,
    pub(crate) transport: transport::TransportState,
    pub(crate) gestures: crate::ui::Gestures,
    pub(crate) rename: Option<(TrackId, String, bool)>,
    /// What the user is dragging in this frame; the views set it, the session shares it.
    pub(crate) editing: Option<EntityRef>,
    /// The commit dialog, while open.
    pub commit: Option<checkout::CommitDialog>,
    /// A track whose checkout by someone else the user asked to end, awaiting confirmation.
    pub confirm_force: Option<TrackId>,
}

impl DawApp {
    pub fn new(session: Session) -> Self {
        let track = session.project().ordered_tracks().next().map(|(id, _)| id);
        Self {
            session,
            sel: Selection {
                track,
                ..Selection::default()
            },
            tab: Tab::default(),
            loop_on: false,
            loop_range: TickRange::new(Tick::ZERO, Tick::bars(4)),
            arrange: arrange::ArrangeState::default(),
            roll: piano_roll::RollState::default(),
            transport: transport::TransportState::default(),
            gestures: crate::ui::Gestures::default(),
            rename: None,
            editing: None,
            commit: None,
            confirm_force: None,
        }
    }

    /// One-time look and feel. Call when the egui context exists.
    pub fn setup(ctx: &egui::Context) {
        ctx.set_theme(egui::Theme::Dark);
        ctx.all_styles_mut(|style| {
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.interaction.selectable_labels = false;
        });
    }

    /// Runs one frame.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.session.frame();
        // The loop belongs to the transport, which others may have moved.
        match self.session.loop_range() {
            Some(range) => (self.loop_on, self.loop_range) = (true, range),
            None => self.loop_on = false,
        }
        self.drop_stale_selection();
        self.shortcuts(ui.ctx());
        piano_roll::release_preview(self, ui.ctx());

        egui::Panel::top("transport").show(ui, |ui| transport::show(self, ui));
        if !self.session.notices().is_empty() {
            egui::Panel::top("notices").show(ui, |ui| transport::notices(self, ui));
        }
        egui::Panel::bottom("editor")
            .resizable(true)
            .default_size(330.0)
            .size_range(160.0..=560.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tab, Tab::Notes, "Notes");
                    ui.selectable_value(&mut self.tab, Tab::Device, "Device");
                    ui.selectable_value(&mut self.tab, Tab::Changelog, "Changelog");
                    ui.separator();
                    match self.tab {
                        Tab::Notes => piano_roll::toolbar(self, ui),
                        Tab::Device => devices::toolbar(self, ui),
                        Tab::Changelog => changelog::toolbar(self, ui),
                    }
                });
                match self.tab {
                    Tab::Notes => piano_roll::show(self, ui),
                    Tab::Device => devices::show(self, ui),
                    Tab::Changelog => changelog::show(self, ui),
                }
            });
        egui::CentralPanel::no_frame().show(ui, |ui| arrange::show(self, ui));
        checkout::dialogs(self, ui.ctx());

        let editing = self.editing.take();
        self.session.set_editing(editing);
        if self.session.is_playing() || self.transport.meter_active() {
            ui.ctx().request_repaint();
        } else if let Some(wait) = self.session.next_frame_in() {
            ui.ctx().request_repaint_after(wait);
        }
    }

    pub fn toggle_play(&mut self) {
        if self.session.is_playing() {
            self.session.pause()
        } else {
            self.session.play()
        }
    }

    /// Stops. When already stopped, goes back to the start.
    pub fn stop(&mut self) {
        if self.session.is_playing() {
            self.session.pause()
        } else {
            self.session.seek(Tick::ZERO)
        }
    }

    pub fn apply_loop(&mut self) {
        let range = (self.loop_on && !self.loop_range.is_empty()).then_some(self.loop_range);
        self.session.set_loop(range);
    }

    /// Opens the commit dialog for this user's checkout of `track`, which stops edits to it
    /// while the samples render.
    pub fn open_commit(&mut self, track: TrackId) {
        if self.session.commit_state().is_none() && !self.session.start_commit(track) {
            return;
        }
        if self.commit.is_none() {
            self.commit = Some(checkout::CommitDialog::default());
        }
    }

    pub fn add_track(&mut self) {
        let project = self.session.project();
        let (count, last) = (project.track_order.len(), project.track_order.last().copied());
        let op = new_track(self.session.ids(), count, last);
        let Op::TrackAdd { id, .. } = &op else { return };
        let id = *id;
        if self.session.edit(vec![op]) {
            self.sel.track = Some(id);
        }
    }

    pub fn remove_track(&mut self, id: TrackId) {
        self.session.engine_mut().note_off(id, piano_roll::PREVIEW_KEY);
        self.session.edit(vec![Op::TrackRemove { id }]);
    }

    /// Opens a clip in the note editor.
    pub fn open_clip(&mut self, track: TrackId, clip: ClipId) {
        if self.sel.clip != Some((track, clip)) {
            self.sel.notes.clear();
        }
        self.sel.track = Some(track);
        self.sel.clip = Some((track, clip));
        self.tab = Tab::Notes;
    }

    pub fn delete_selection(&mut self) {
        let Some((track, clip)) = self.sel.clip else {
            return;
        };
        match self.sel.focus {
            Focus::Notes => {
                let ops = self
                    .sel
                    .notes
                    .iter()
                    .map(|&id| Op::NoteRemove { track, clip, id })
                    .collect();
                self.session.edit(ops);
                self.sel.notes.clear();
            }
            Focus::Arrange => {
                self.session.edit(vec![Op::ClipRemove { track, id: clip }]);
            }
        }
    }

    /// Undo, redo and remote edits can remove what was selected.
    fn drop_stale_selection(&mut self) {
        let project = self.session.project();
        if self.sel.track.is_some_and(|id| project.track(id).is_none()) {
            self.sel.track = project.ordered_tracks().next().map(|(id, _)| id);
        }
        let clip = self
            .sel
            .clip
            .and_then(|(track, clip)| project.track(track)?.content.clips.get(&clip));
        match clip {
            Some(clip) => self.sel.notes.retain(|id| clip.notes.contains_key(id)),
            None => {
                self.sel.clip = None;
                self.sel.notes.clear();
            }
        }
        if self
            .rename
            .as_ref()
            .is_some_and(|(id, ..)| project.track(*id).is_none())
        {
            self.rename = None;
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let pressed = |modifiers: Modifiers, key: Key| ctx.input_mut(|i| i.consume_key(modifiers, key));
        // The more specific shortcut first: Ctrl+Z also matches Ctrl+Shift+Z.
        if pressed(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) || pressed(Modifiers::COMMAND, Key::Y) {
            self.session.redo();
        } else if pressed(Modifiers::COMMAND, Key::Z) {
            self.session.undo();
        }
        if pressed(Modifiers::NONE, Key::Space) {
            self.toggle_play();
        }
        if pressed(Modifiers::NONE, Key::Home) {
            self.session.seek(Tick::ZERO);
        }
        if pressed(Modifiers::NONE, Key::Delete) || pressed(Modifiers::NONE, Key::Backspace) {
            self.delete_selection();
        }
        if pressed(Modifiers::NONE, Key::Escape) {
            self.sel.notes.clear();
        }
        if self.sel.focus == Focus::Notes && pressed(Modifiers::COMMAND, Key::A) {
            self.select_all_notes();
        }
    }

    fn select_all_notes(&mut self) {
        let Some((track, clip)) = self.sel.clip else {
            return;
        };
        if let Some(clip) = self
            .session
            .project()
            .track(track)
            .and_then(|t| t.content.clips.get(&clip))
        {
            self.sel.notes = clip.notes.keys().copied().collect();
        }
    }
}
