//! Playing live: the computer keyboard as a piano, the MIDI input, and the strip that shows
//! where the notes go.

use std::collections::HashMap;

use egui::{Button, ComboBox, Event, Key, RichText, Ui};
use mdaw_model::TrackId;

use crate::live::MidiWatch;
use crate::{DawApp, theme};
use mdaw_engine::midi;

/// The keys that play, with their semitone above the octave's C: two rows like a piano's
/// white and black keys.
const PIANO: [(Key, u8); 17] = [
    (Key::A, 0),
    (Key::W, 1),
    (Key::S, 2),
    (Key::E, 3),
    (Key::D, 4),
    (Key::F, 5),
    (Key::T, 6),
    (Key::G, 7),
    (Key::Y, 8),
    (Key::H, 9),
    (Key::U, 10),
    (Key::J, 11),
    (Key::K, 12),
    (Key::O, 13),
    (Key::L, 14),
    (Key::P, 15),
    (Key::Semicolon, 16),
];
/// Velocity of notes from the computer keyboard.
const KEY_VEL: u8 = 100;
const MIN_OCTAVE: i8 = -1;
const MAX_OCTAVE: i8 = 8;

pub struct LiveState {
    /// The computer keyboard plays notes.
    pub keys_on: bool,
    /// The octave the A key plays the C of; C4 is middle C, MIDI 60.
    pub octave: i8,
    /// Keys held down, with the pitch each started.
    down: HashMap<Key, u8>,
    /// Watches the MIDI inputs, once enabled.
    pub midi: Option<MidiWatch>,
}

impl Default for LiveState {
    fn default() -> Self {
        Self {
            keys_on: true,
            octave: 4,
            down: HashMap::new(),
            midi: None,
        }
    }
}

/// The track live notes go to: the selected one, or else the first.
pub fn target(app: &DawApp) -> Option<TrackId> {
    let project = app.session.project();
    app.sel
        .track
        .filter(|id| project.track(*id).is_some())
        .or_else(|| project.ordered_tracks().next().map(|(id, _)| id))
}

/// Plays the computer keyboard. Runs every frame, before the shortcuts.
pub fn keyboard(app: &mut DawApp, ctx: &egui::Context) {
    let live = app.session.live().clone();
    let events = ctx.input(|i| i.events.clone());
    let typing = ctx.egui_wants_keyboard_input();
    for event in events {
        match event {
            Event::WindowFocused(false) => {
                for (_, pitch) in app.live.down.drain() {
                    live.release(pitch);
                }
            }
            Event::Key {
                key, pressed: false, ..
            } => {
                if let Some(pitch) = app.live.down.remove(&key) {
                    live.release(pitch);
                }
            }
            Event::Key {
                key,
                pressed: true,
                repeat: false,
                modifiers,
                ..
            } if app.live.keys_on && !typing && !modifiers.any() => {
                if key == Key::Z {
                    app.live.octave = (app.live.octave - 1).max(MIN_OCTAVE);
                } else if key == Key::X {
                    app.live.octave = (app.live.octave + 1).min(MAX_OCTAVE);
                } else if let Some(&(_, semitone)) = PIANO.iter().find(|(k, _)| *k == key) {
                    let pitch = (app.live.octave as i16 + 1) * 12 + semitone as i16;
                    if let Ok(pitch) = u8::try_from(pitch)
                        && pitch < 128
                        && !app.live.down.contains_key(&key)
                    {
                        app.live.down.insert(key, pitch);
                        live.press(pitch, KEY_VEL);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The strip under the transport bar.
pub fn show(app: &mut DawApp, ui: &mut Ui) {
    ui.horizontal(|ui| {
        let hint = "The computer keyboard plays the track: A S D F G H J K are white keys, \
                    W E T Y U black ones. Z and X change the octave.";
        if ui
            .toggle_value(&mut app.live.keys_on, "Keys")
            .on_hover_text(hint)
            .changed()
            && !app.live.keys_on
        {
            let live = app.session.live().clone();
            for (_, pitch) in app.live.down.drain() {
                live.release(pitch);
            }
        }
        ui.label(RichText::new(format!("Octave C{}", app.live.octave)).color(theme::TEXT_DIM))
            .on_hover_text("Z: down, X: up");

        ui.separator();
        midi_input(app, ui);

        ui.separator();
        let project = app.session.project();
        match target(app).and_then(|id| project.track(id)) {
            Some(track) => {
                let text = format!("Playing into {}", track.shell.name);
                ui.label(RichText::new(text).color(theme::TEXT_DIM))
                    .on_hover_text("Select another track to play into it.");
            }
            None => {
                ui.label(RichText::new("No track to play into").color(theme::TEXT_DIM));
            }
        }
        if app.session.is_recording() && app.session.is_playing() {
            ui.separator();
            let text = match app.session.take_len() {
                1 => "Recording: 1 note".to_string(),
                n => format!("Recording: {n} notes"),
            };
            ui.label(RichText::new(text).color(theme::METER_HOT));
        }
    });
}

fn midi_input(app: &mut DawApp, ui: &mut Ui) {
    let Some(midi) = &app.live.midi else {
        ui.label(RichText::new("No MIDI input").color(theme::TEXT_DIM));
        return;
    };
    let wanted = midi.wanted();
    let open = midi.open();
    let shown = match (&wanted, &open) {
        (None, _) => "None".to_string(),
        (Some(name), Some(open)) if midi::same_device(name, open) => open.clone(),
        (Some(name), _) => format!("{name} (not connected)"),
    };
    let mut pick = None;
    ComboBox::from_label("MIDI in")
        .selected_text(shown)
        .width(200.0)
        .show_ui(ui, |ui| {
            if ui.selectable_label(wanted.is_none(), "None").clicked() {
                pick = Some(None);
            }
            for name in midi.devices() {
                let chosen = wanted.as_deref().is_some_and(|w| midi::same_device(w, &name));
                if ui.selectable_label(chosen, &name).clicked() {
                    pick = Some(Some(name));
                }
            }
        });
    if let Some(pick) = pick {
        midi.set_wanted(pick);
    }
}

/// The record button for the transport bar.
pub fn record_button(app: &mut DawApp, ui: &mut Ui) {
    let on = app.session.is_recording();
    let mut button = Button::new(RichText::new("Rec").color(if on { theme::TEXT } else { theme::METER_HOT }));
    if on {
        button = button.fill(theme::REC);
    }
    let hint = "Record what you play while the transport runs. It becomes one edit when the \
                transport stops: a new clip, or notes in the clip it falls into.";
    if ui.add(button).on_hover_text(hint).clicked() {
        app.session.set_recording(!on);
    }
}
