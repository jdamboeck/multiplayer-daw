//! The commit dialog, and the question before ending someone else's checkout.

use egui::{Align2, Button, Context, RichText, TextEdit, Ui, Window};
use mdaw_proto::{Sample, SampleKind};

use crate::session::CommitState;
use crate::{DawApp, theme};

#[derive(Clone, Debug, Default)]
pub struct CommitDialog {
    pub summary: String,
    /// "Drop changes" was pressed once and waits to be pressed again.
    pub drop_armed: bool,
}

pub fn dialogs(app: &mut DawApp, ctx: &Context) {
    commit_dialog(app, ctx);
    force_dialog(app, ctx);
}

fn commit_dialog(app: &mut DawApp, ctx: &Context) {
    if app.commit.is_none() {
        return;
    }
    let Some(state) = app.session.commit_state() else {
        // Committed, or turned down: either way the session has said so.
        app.commit = None;
        return;
    };
    enum Phase {
        Rendering(mdaw_model::TrackId),
        Ready(Box<crate::checkout::CommitPlan>),
        Sending(mdaw_model::TrackId),
    }
    let phase = match state {
        CommitState::Rendering { track } => Phase::Rendering(track),
        CommitState::Ready { plan } => Phase::Ready(Box::new(plan.clone())),
        CommitState::Sending { track } => Phase::Sending(track),
    };
    let track = match &phase {
        Phase::Rendering(t) | Phase::Sending(t) => *t,
        Phase::Ready(plan) => plan.track,
    };
    let name = app
        .session
        .project()
        .track(track)
        .map(|t| t.shell.name.clone())
        .unwrap_or_default();

    let mut close = false;
    Window::new(format!("Commit \"{name}\""))
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_width(420.0);
            match &phase {
                Phase::Rendering(_) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Rendering the samples…");
                    });
                    ui.add_space(8.0);
                    if ui.button("Keep working").clicked() {
                        app.session.cancel_commit();
                        close = true;
                    }
                }
                Phase::Sending(_) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Uploading the samples and committing…");
                    });
                }
                Phase::Ready(plan) => {
                    let dialog = app.commit.as_mut().expect("checked above");
                    ui.label("What did you change?");
                    ui.add(
                        TextEdit::multiline(&mut dialog.summary)
                            .desired_rows(2)
                            .desired_width(f32::INFINITY)
                            .hint_text("A line for the changelog"),
                    );
                    ui.add_space(6.0);
                    let rendered = plan.to_listen();
                    if rendered.is_empty() {
                        ui.label(RichText::new("The track is silent: there is nothing to listen to.").color(theme::TEXT_DIM));
                    } else {
                        ui.label("Listen before you commit:");
                        ui.horizontal_wrapped(|ui| {
                            for r in rendered {
                                if sample_button(app, ui, &r.sample).clicked() {
                                    app.session.play_rendered(r);
                                }
                            }
                        });
                    }
                    ui.add_space(6.0);
                    let summary = app.commit.as_ref().map(|d| d.summary.clone()).unwrap_or_default();
                    if plan.conflict {
                        ui.label(
                            RichText::new(
                                "The tempo under this track changed since you checked it out. \
                                 \"Old tempo\" plays your work as you made it, \"Current tempo\" as \
                                 the project would play it now.",
                            )
                            .color(theme::WARN),
                        );
                        ui.horizontal(|ui| {
                            if ui.button("Commit as is").clicked() {
                                app.session.finish_commit(&summary, false);
                            }
                            if ui
                                .button("Commit and restore the old tempo")
                                .on_hover_text("Puts the tempo back to what it was when you checked out, in the same step")
                                .clicked()
                            {
                                app.session.finish_commit(&summary, true);
                            }
                        });
                    } else if ui.button("Commit").clicked() {
                        app.session.finish_commit(&summary, false);
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("Keep working").clicked() {
                            app.session.stop_sample();
                            app.session.cancel_commit();
                            close = true;
                        }
                        let armed = app.commit.as_ref().is_some_and(|d| d.drop_armed);
                        let label = if armed { "Really drop them?" } else { "Drop changes" };
                        let button = Button::new(RichText::new(label).color(theme::WARN));
                        if ui
                            .add(button)
                            .on_hover_text("End the checkout and throw away what you changed in it")
                            .clicked()
                        {
                            if armed {
                                app.session.stop_sample();
                                app.session.cancel_commit();
                                app.session.release(track);
                                close = true;
                            } else if let Some(dialog) = &mut app.commit {
                                dialog.drop_armed = true;
                            }
                        }
                    });
                }
            }
        });
    if close {
        app.commit = None;
    }
}

fn force_dialog(app: &mut DawApp, ctx: &Context) {
    let Some(track) = app.confirm_force else {
        return;
    };
    let session = &app.session;
    let (Some(owner), Some(name)) = (
        session.checkout_owner(track),
        session.project().track(track).map(|t| t.shell.name.clone()),
    ) else {
        app.confirm_force = None;
        return;
    };
    let owner = session.user_name(owner);
    let mut answer = None;
    Window::new("End the checkout?")
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_width(380.0);
            ui.label(format!(
                "{owner} has \"{name}\" checked out. Ending the checkout makes the track editable \
                 for everyone again, as it was before. {owner}'s changes are not lost: they stay on \
                 their machine as a fork they can bring back as a new track."
            ));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .button(RichText::new("End the checkout").color(theme::WARN))
                    .clicked()
                {
                    answer = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(false);
                }
            });
        });
    match answer {
        Some(true) => {
            app.session.force_release(track);
            app.confirm_force = None;
        }
        Some(false) => app.confirm_force = None,
        None => {}
    }
}

/// What a sample is, in a few words.
pub fn kind_label(kind: SampleKind) -> &'static str {
    match kind {
        SampleKind::Solo => "Track alone",
        SampleKind::InContext => "With the others",
        SampleKind::Before => "As it was",
        SampleKind::ProjectBase => "Old tempo",
        SampleKind::ProjectCurrent => "Current tempo",
    }
}

/// A button that plays or stops a sample and shows how far it got. The caller starts it.
pub fn sample_button(app: &DawApp, ui: &mut Ui, sample: &Sample) -> egui::Response {
    let length = sample.ms as f64 / 1000.0;
    let label = match app.session.sample_position(sample.blob) {
        Some(at) => format!(
            "Stop: {} {:.1}/{:.1} s",
            kind_label(sample.kind),
            at.min(length),
            length
        ),
        None => format!("Play: {} {:.1} s", kind_label(sample.kind), length),
    };
    ui.add(Button::new(label).small())
}
