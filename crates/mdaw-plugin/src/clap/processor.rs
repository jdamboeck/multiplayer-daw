//! The audio-thread half of a CLAP instance, behind `mdaw_dsp::ExternalDevice`.

use super::handlers::MdawHost;
use clack_host::events::event_types::TransportFlags;
use clack_host::events::event_types::{NoteOffEvent, NoteOnEvent, ParamValueEvent, TransportEvent};
use clack_host::events::spaces::CoreEventSpace;
use clack_host::events::{EventFlags, EventHeader, Pckn};
use clack_host::prelude::*;
use clack_host::utils::{BeatTime, Cookie, SecondsTime};
use mdaw_dsp::{BlockTime, ExternalDevice};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::Sender;

/// Notes one processor keeps track of at once; more are not started.
const NOTES: usize = 128;
/// Events one block can carry from the host.
const EVENTS: u32 = 512;
/// Note events waiting for the next block, at most.
const PENDING: usize = 384;
/// Places among them only note ends may take: a note that cannot start is a wrong note, one
/// that cannot end is stuck.
const KEPT_FOR_ENDS: usize = 128;

/// A note start or end waiting for the next block.
#[derive(Clone, Copy, Default)]
struct Pending {
    offset: u32,
    on: bool,
    pitch: u8,
    vel: u8,
    id: u32,
}

/// A parameter change from the main thread.
#[derive(Clone, Copy)]
pub struct ParamChange {
    pub id: ClapId,
    pub cookie: Cookie,
    pub value: f64,
    /// Which reading of the plugin's parameters the cookie is from.
    pub generation: u32,
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
    /// The reading of the plugin's parameters that is current: cookies from an earlier one are
    /// no longer the plugin's.
    generation: Arc<AtomicU32>,
    /// Sounding notes: the graph's key, the pitch, and the note id given to the plugin.
    notes: [Option<(u64, u8, u32)>; NOTES],
    /// Note events for the next block, in the order of their offsets.
    pending: [Pending; PENDING],
    pending_len: usize,
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
        generation: Arc<AtomicU32>,
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
            generation,
            notes: [None; NOTES],
            pending: [Pending::default(); PENDING],
            pending_len: 0,
            next_note_id: 0,
            steady_time: 0,
            max_block,
            failed: false,
        }
    }

    fn queue(&mut self, event: Pending) -> bool {
        let room = if event.on {
            PENDING - KEPT_FOR_ENDS
        } else {
            PENDING
        };
        if self.pending_len >= room {
            return false;
        }
        self.pending[self.pending_len] = event;
        self.pending_len += 1;
        true
    }

    /// Ends the note in `slot`. It counts as ended only once its end is queued.
    fn end(&mut self, slot: usize, offset: u32) {
        let Some((_, pitch, id)) = self.notes[slot] else {
            return;
        };
        let event = Pending {
            offset,
            on: false,
            pitch,
            vel: 0,
            id,
        };
        if self.queue(event) {
            self.notes[slot] = None;
        }
    }

    /// Puts the waiting notes into the block's events, in time order: a stable sort that does
    /// not allocate, over a few events.
    fn push_pending(&mut self, frames: usize) {
        let pending = &mut self.pending[..self.pending_len];
        for i in 1..pending.len() {
            let mut j = i;
            while j > 0 && pending[j - 1].offset > pending[j].offset {
                pending.swap(j - 1, j);
                j -= 1;
            }
        }
        let last = frames.saturating_sub(1) as u32;
        for event in &self.pending[..self.pending_len] {
            if self.events_in.len() >= EVENTS {
                break;
            }
            let time = event.offset.min(last);
            let pckn = Pckn::new(0u16, 0u16, event.pitch as u16, event.id);
            if event.on {
                self.events_in
                    .push(&NoteOnEvent::new(time, pckn, event.vel as f64 / 127.0));
            } else {
                self.events_in.push(&NoteOffEvent::new(time, pckn, 0.0));
            }
        }
        self.pending_len = 0;
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
    fn note_on(&mut self, offset: u32, key: u64, pitch: u8, vel: u8) {
        if let Some(slot) = self.notes.iter().position(|n| n.is_some_and(|n| n.0 == key)) {
            self.end(slot, offset);
        }
        let Some(slot) = self.notes.iter().position(Option::is_none) else {
            return;
        };
        let id = self.next_note_id;
        let event = Pending {
            offset,
            on: true,
            pitch: pitch.min(127),
            vel: vel.clamp(1, 127),
            id,
        };
        if self.queue(event) {
            self.next_note_id = self.next_note_id.wrapping_add(1) & 0x7fff_ffff;
            self.notes[slot] = Some((key, pitch, id));
        }
    }

    fn note_off(&mut self, offset: u32, key: u64) {
        if let Some(slot) = self.notes.iter().position(|n| n.is_some_and(|n| n.0 == key)) {
            self.end(slot, offset);
        }
    }

    fn release_where(&mut self, filter: &dyn Fn(u64) -> bool) {
        for slot in 0..NOTES {
            if self.notes[slot].is_some_and(|n| filter(n.0)) {
                self.end(slot, 0);
            }
        }
    }

    fn process(&mut self, left: &mut [f32], right: &mut [f32], time: &BlockTime) {
        // Plugin code is not ours to hold to the rules of the audio thread.
        let _foreign = mdaw_engine::alloc_guard::leave();
        let frames = left.len().min(self.max_block);
        // Parameters at the block's start, then the notes in time order.
        self.events_in.clear();
        let generation = self.generation.load(Relaxed);
        while let Ok(change) = self.params.pop() {
            if self.events_in.len() >= EVENTS {
                break;
            }
            if change.generation != generation {
                // Made before the plugin changed its parameters: its cookie may be gone.
                continue;
            }
            let event = ParamValueEvent::new(0, change.id, Pckn::match_all(), change.value);
            // SAFETY: the cookie came from this plugin's `get_info` for this parameter, in the
            // reading of its parameters that is still current.
            self.events_in.push(&unsafe { event.with_cookie(change.cookie) });
        }
        self.push_pending(frames);
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
