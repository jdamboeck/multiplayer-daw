//! The audio-thread half of a CLAP instance, behind `mdaw_dsp::ExternalDevice`.

use super::handlers::MdawHost;
use clack_host::events::event_types::TransportFlags;
use clack_host::events::event_types::{NoteOffEvent, NoteOnEvent, ParamValueEvent, TransportEvent};
use clack_host::events::spaces::CoreEventSpace;
use clack_host::events::{EventFlags, EventHeader, Pckn};
use clack_host::prelude::*;
use clack_host::utils::{BeatTime, Cookie, SecondsTime};
use mdaw_dsp::{BlockTime, ExternalDevice};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::Sender;

/// Notes one processor keeps track of at once; more are not started.
const NOTES: usize = 128;
/// Events one block can carry from the host.
const EVENTS: u32 = 512;

/// A parameter change from the main thread.
#[derive(Clone, Copy)]
pub struct ParamChange {
    pub id: ClapId,
    pub cookie: Cookie,
    pub value: f64,
}

/// The audio ports of one direction, with buffers for their channels.
pub struct Ports {
    ports: AudioPorts,
    /// `[port][channel][frame]`
    buffers: Vec<Vec<Vec<f32>>>,
    /// The main port, if any.
    main: Option<usize>,
}

impl Ports {
    pub fn new(channels: &[u32], main: Option<usize>, max_block: usize) -> Self {
        Self {
            ports: AudioPorts::with_capacity(channels.iter().sum::<u32>() as usize, channels.len()),
            buffers: channels
                .iter()
                .map(|&n| vec![vec![0.0; max_block]; n as usize])
                .collect(),
            main,
        }
    }
}

pub struct ClapProcessor {
    processor: Option<PluginAudioProcessor<MdawHost>>,
    /// Where the stopped processor goes once this is dropped: back to its instance.
    back: Sender<StoppedPluginAudioProcessor<MdawHost>>,
    inputs: Ports,
    outputs: Ports,
    events_in: EventBuffer,
    events_out: EventBuffer,
    params: rtrb::Consumer<ParamChange>,
    /// Sounding notes: the graph's key, the pitch, and the note id given to the plugin.
    notes: [Option<(u64, u8, u32)>; NOTES],
    next_note_id: u32,
    steady_time: u64,
    max_block: usize,
    /// The plugin failed to process; it stays silent.
    failed: bool,
}

impl ClapProcessor {
    pub fn new(
        processor: PluginAudioProcessor<MdawHost>,
        back: Sender<StoppedPluginAudioProcessor<MdawHost>>,
        inputs: Ports,
        outputs: Ports,
        params: rtrb::Consumer<ParamChange>,
        max_block: usize,
    ) -> Self {
        Self {
            processor: Some(processor),
            back,
            inputs,
            outputs,
            events_in: EventBuffer::with_capacity(EVENTS as usize),
            events_out: EventBuffer::with_capacity(EVENTS as usize),
            params,
            notes: [None; NOTES],
            next_note_id: 0,
            steady_time: 0,
            max_block,
            failed: false,
        }
    }

    fn end(&mut self, slot: usize) {
        if let Some((_, pitch, id)) = self.notes[slot].take()
            && self.events_in.len() < EVENTS
        {
            let pckn = Pckn::new(0u16, 0u16, pitch as u16, id);
            self.events_in.push(&NoteOffEvent::new(0, pckn, 0.0));
        }
    }

    fn transport(time: &BlockTime) -> TransportEvent {
        let beats = time.tick / mdaw_model::PPQ as f64;
        let bar = (beats / 4.0).floor();
        let mut flags = TransportFlags::HAS_TEMPO
            | TransportFlags::HAS_BEATS_TIMELINE
            | TransportFlags::HAS_SECONDS_TIMELINE
            | TransportFlags::HAS_TIME_SIGNATURE;
        if time.playing {
            flags |= TransportFlags::IS_PLAYING;
        }
        TransportEvent {
            header: EventHeader::new_core(0, EventFlags::empty()),
            flags,
            song_pos_beats: BeatTime::from_float(beats),
            song_pos_seconds: SecondsTime::from_float(time.seconds),
            tempo: time.bpm,
            tempo_inc: 0.0,
            loop_start_beats: BeatTime::from_int(0),
            loop_end_beats: BeatTime::from_int(0),
            loop_start_seconds: SecondsTime::from_int(0),
            loop_end_seconds: SecondsTime::from_int(0),
            bar_start: BeatTime::from_float(bar * 4.0),
            bar_number: bar as i32,
            time_signature_numerator: 4,
            time_signature_denominator: 4,
        }
    }

    fn run(&mut self, frames: usize, time: &BlockTime) -> bool {
        let Some(processor) = self.processor.as_mut() else {
            return false;
        };
        let inputs = self
            .inputs
            .ports
            .with_input_buffers(self.inputs.buffers.iter_mut().map(|port| {
                AudioPortBuffer {
                    latency: 0,
                    channels: AudioPortBufferType::f32_input_only(
                        port.iter_mut()
                            .map(|ch| InputChannel::variable(&mut ch[..frames])),
                    ),
                }
            }));
        let mut outputs = self
            .outputs
            .ports
            .with_output_buffers(self.outputs.buffers.iter_mut().map(|port| AudioPortBuffer {
                latency: 0,
                channels: AudioPortBufferType::f32_output_only(port.iter_mut().map(|ch| &mut ch[..frames])),
            }));
        let Ok(started) = processor.ensure_processing_started() else {
            return false;
        };
        let transport = Self::transport(time);
        let result = started.process(
            &inputs,
            &mut outputs,
            &self.events_in.as_input(),
            &mut self.events_out.as_output(),
            Some(self.steady_time),
            Some(&transport),
        );
        self.steady_time += frames as u64;
        result.is_ok()
    }
}

impl ExternalDevice for ClapProcessor {
    fn note_on(&mut self, key: u64, pitch: u8, vel: u8) {
        if let Some(slot) = self.notes.iter().position(|n| n.is_some_and(|n| n.0 == key)) {
            self.end(slot);
        }
        let Some(slot) = self.notes.iter().position(Option::is_none) else {
            return;
        };
        if self.events_in.len() >= EVENTS {
            return;
        }
        let id = self.next_note_id;
        self.next_note_id = self.next_note_id.wrapping_add(1) & 0x7fff_ffff;
        self.notes[slot] = Some((key, pitch, id));
        let pckn = Pckn::new(0u16, 0u16, pitch.min(127) as u16, id);
        self.events_in
            .push(&NoteOnEvent::new(0, pckn, vel.clamp(1, 127) as f64 / 127.0));
    }

    fn note_off(&mut self, key: u64) {
        if let Some(slot) = self.notes.iter().position(|n| n.is_some_and(|n| n.0 == key)) {
            self.end(slot);
        }
    }

    fn release_where(&mut self, filter: &dyn Fn(u64) -> bool) {
        for slot in 0..NOTES {
            if self.notes[slot].is_some_and(|n| filter(n.0)) {
                self.end(slot);
            }
        }
    }

    fn process(&mut self, left: &mut [f32], right: &mut [f32], time: &BlockTime) {
        // Plugin code is not ours to hold to the rules of the audio thread.
        let _foreign = mdaw_engine::alloc_guard::leave();
        let frames = left.len().min(self.max_block);
        while let Ok(change) = self.params.pop() {
            if self.events_in.len() >= EVENTS {
                break;
            }
            let event = ParamValueEvent::new(0, change.id, Pckn::match_all(), change.value);
            // SAFETY: the cookie came from this plugin's `get_info` for this parameter, and the
            // host never asks it to rescan its parameters.
            self.events_in.push(&unsafe { event.with_cookie(change.cookie) });
        }
        if let Some(main) = self.inputs.main {
            let port = &mut self.inputs.buffers[main];
            match port.len() {
                0 => {}
                1 => {
                    for ((d, l), r) in port[0][..frames].iter_mut().zip(left.iter()).zip(right.iter()) {
                        *d = (l + r) * 0.5;
                    }
                }
                _ => {
                    port[0][..frames].copy_from_slice(&left[..frames]);
                    port[1][..frames].copy_from_slice(&right[..frames]);
                }
            }
        }
        let ok = !self.failed && self.run(frames, time);
        self.events_in.clear();
        let mut touched = false;
        for event in self.events_out.iter() {
            if let Some(CoreEventSpace::ParamValue(_)) = event.as_core_event() {
                touched = true;
            }
        }
        self.events_out.clear();
        if touched && let Some(processor) = &self.processor {
            processor.access_shared_handler(|shared| shared.changed.store(true, Relaxed));
        }
        if !ok {
            if !self.failed
                && let Some(processor) = &self.processor
            {
                processor.access_shared_handler(|shared| shared.failed.store(true, Relaxed));
            }
            self.failed = true;
            if self.inputs.main.is_some() {
                left[..frames].fill(0.0);
                right[..frames].fill(0.0);
            }
            return;
        }
        let Some(port) = self.outputs.main.and_then(|m| self.outputs.buffers.get(m)) else {
            return;
        };
        let Some(out_left) = port.first() else {
            return;
        };
        let out_right = port.get(1).unwrap_or(out_left);
        let effect = self.inputs.main.is_some();
        for i in 0..frames {
            let (l, r) = (out_left[i], out_right[i]);
            // A plugin's bad numbers stop here.
            let (l, r) = (
                if l.is_finite() { l } else { 0.0 },
                if r.is_finite() { r } else { 0.0 },
            );
            if effect {
                left[i] = l;
                right[i] = r;
            } else {
                left[i] += l;
                right[i] += r;
            }
        }
    }

    fn retire(&mut self) {
        let _foreign = mdaw_engine::alloc_guard::leave();
        if let Some(processor) = self.processor.as_mut() {
            processor.ensure_processing_stopped();
        }
    }
}

impl Drop for ClapProcessor {
    fn drop(&mut self) {
        if let Some(processor) = self.processor.take() {
            let _ = self.back.send(processor.into_stopped());
        }
    }
}
