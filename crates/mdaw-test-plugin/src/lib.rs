//! "mdaw Test Synth": a CLAP instrument small enough to reason about, for tests of the plugin
//! host. One sine voice per note with a short linear attack and release, a stereo output, two
//! parameters and a state that holds them. Its output depends only on its input, so two
//! instances with the same state render the same samples.

use clack_extensions::audio_ports::{
    AudioPortFlags, AudioPortInfo, AudioPortInfoWriter, AudioPortType, PluginAudioPorts, PluginAudioPortsImpl,
};
use clack_extensions::note_ports::{
    NoteDialect, NoteDialects, NotePortInfo, NotePortInfoWriter, PluginNotePorts, PluginNotePortsImpl,
};
use clack_extensions::params::{
    ParamDisplayWriter, ParamInfo, ParamInfoFlags, ParamInfoWriter, PluginAudioProcessorParams,
    PluginMainThreadParams, PluginParams,
};
use clack_extensions::state::{PluginState, PluginStateImpl};
use clack_plugin::events::Match;
use clack_plugin::events::spaces::CoreEventSpace;
use clack_plugin::prelude::*;
use clack_plugin::stream::{InputStream, OutputStream};
use std::ffi::CStr;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

pub const ID: &str = "org.mdaw.test-synth";
pub const NAME: &str = "mdaw Test Synth";
/// Parameter: output level, 0 to 1.
pub const LEVEL: u32 = 0;
/// Parameter: balance, -1 (left only) to 1 (right only).
pub const BALANCE: u32 = 1;
/// Parameter: how the plugin misbehaves on a note of [`CRASH_PITCH`], as a broken plugin would,
/// for tests of hosts that must survive that. 0, the default: not at all. 1: its processing
/// fails from then on. 2: it crashes the process. 3: it is slow. 4: it hangs.
pub const MISBEHAVE: u32 = 2;
pub const FAIL: f64 = 1.0;
pub const CRASH: f64 = 2.0;
/// Each block takes 50 ms from then on, as a plugin overloading the machine.
pub const SLOW: f64 = 3.0;
/// Its processing never returns, as a plugin caught in a loop.
pub const HANG: f64 = 4.0;
/// The note that sets off what [`MISBEHAVE`] asks for.
pub const CRASH_PITCH: u8 = 0;
const DEFAULT_LEVEL: f64 = 0.5;
const VOICES: usize = 16;
const ATTACK_SECONDS: f64 = 0.005;
const RELEASE_SECONDS: f64 = 0.05;
const STATE_MAGIC: &[u8; 4] = b"MDTS";

/// The entry to load the plugin in-process with.
pub type Entry = SinglePluginEntry<TestSynth>;

pub struct TestSynth;

/// What the main thread and the audio thread share: the parameter values.
pub struct Shared {
    level: AtomicU32,
    balance: AtomicU32,
    misbehave: AtomicU32,
}

impl Shared {
    fn get(&self, id: u32) -> Option<f64> {
        let cell = match id {
            LEVEL => &self.level,
            BALANCE => &self.balance,
            MISBEHAVE => &self.misbehave,
            _ => return None,
        };
        Some(f32::from_bits(cell.load(Relaxed)) as f64)
    }

    fn set(&self, id: u32, value: f64) {
        let (cell, value) = match id {
            LEVEL => (&self.level, value.clamp(0.0, 1.0)),
            BALANCE => (&self.balance, value.clamp(-1.0, 1.0)),
            MISBEHAVE => (&self.misbehave, value.round().clamp(0.0, 4.0)),
            _ => return,
        };
        cell.store((value as f32).to_bits(), Relaxed);
    }
}

impl PluginShared<'_> for Shared {}

pub struct MainThread<'a> {
    shared: &'a Shared,
}

impl<'a> PluginMainThread<'a, Shared> for MainThread<'a> {}

impl Plugin for TestSynth {
    type AudioProcessor<'a> = Processor<'a>;
    type Shared<'a> = Shared;
    type MainThread<'a> = MainThread<'a>;

    fn declare_extensions(builder: &mut PluginExtensions<Self>, _shared: Option<&Shared>) {
        builder
            .register::<PluginAudioPorts>()
            .register::<PluginNotePorts>()
            .register::<PluginParams>()
            .register::<PluginState>();
    }
}

impl DefaultPluginFactory for TestSynth {
    fn get_descriptor() -> PluginDescriptor {
        use clack_plugin::plugin::features::*;
        PluginDescriptor::new(ID, NAME)
            .with_vendor("mdaw")
            .with_version("1.0.0")
            .with_features([INSTRUMENT, SYNTHESIZER, STEREO])
    }

    fn new_shared(_host: HostSharedHandle<'_>) -> Result<Shared, PluginError> {
        Ok(Shared {
            level: AtomicU32::new((DEFAULT_LEVEL as f32).to_bits()),
            balance: AtomicU32::new(0.0f32.to_bits()),
            misbehave: AtomicU32::new(0.0f32.to_bits()),
        })
    }

    fn new_main_thread<'a>(
        _host: HostMainThreadHandle<'a>,
        shared: &'a Shared,
    ) -> Result<MainThread<'a>, PluginError> {
        Ok(MainThread { shared })
    }
}

#[derive(Clone, Copy)]
struct Voice {
    key: i16,
    note_id: i32,
    channel: i16,
    phase: f64,
    step: f64,
    vel: f64,
    /// 0 to 1 while attacking and sustaining; falls to 0 after release.
    env: f64,
    released: bool,
}

pub struct Processor<'a> {
    shared: &'a Shared,
    /// Told to be slow, and the note came.
    slow: bool,
    sample_rate: f64,
    voices: [Option<Voice>; VOICES],
}

impl<'a> PluginAudioProcessor<'a, Shared, MainThread<'a>> for Processor<'a> {
    fn activate(
        _host: HostAudioProcessorHandle<'a>,
        _main_thread: &MainThread<'a>,
        shared: &'a Shared,
        config: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        Ok(Self {
            shared,
            slow: false,
            sample_rate: config.sample_rate,
            voices: [None; VOICES],
        })
    }

    fn process(
        &mut self,
        _process: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        let mut port = audio
            .output_port(0)
            .ok_or(PluginError::Message("no output port"))?;
        let mut channels = port
            .channels()?
            .into_f32()
            .ok_or(PluginError::Message("expected 32-bit output"))?;
        let frames = channels.frames_count() as usize;
        let mut left = take_channel(&mut channels, 0);
        let mut right = take_channel(&mut channels, 1);
        if let Some(left) = left.as_deref_mut() {
            left.fill(0.0);
        }
        if let Some(right) = right.as_deref_mut() {
            right.fill(0.0);
        }
        let mut done = 0;
        for event in events.input {
            let at = (event.header().time() as usize).min(frames);
            if at > done {
                self.render(left.as_deref_mut(), right.as_deref_mut(), done, at);
                done = at;
            }
            match event.as_core_event() {
                Some(CoreEventSpace::NoteOn(e)) => {
                    if e.key() == Match::Specific(CRASH_PITCH as u16) {
                        match self.shared.get(MISBEHAVE).unwrap_or(0.0) {
                            m if m >= HANG => loop {
                                std::thread::sleep(std::time::Duration::from_secs(1));
                            },
                            m if m >= SLOW => self.slow = true,
                            m if m >= CRASH => std::process::abort(),
                            m if m >= FAIL => return Err(PluginError::Message("told to fail")),
                            _ => {}
                        }
                    }
                    self.start(e.key(), e.note_id(), e.channel(), e.velocity())
                }
                Some(CoreEventSpace::NoteOff(e)) => self.stop(e.key(), e.note_id(), e.channel()),
                Some(CoreEventSpace::ParamValue(e)) => {
                    if let Some(id) = e.param_id() {
                        self.shared.set(id.get(), e.value());
                    }
                }
                _ => {}
            }
        }
        if done < frames {
            self.render(left, right, done, frames);
        }
        if self.slow {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Ok(ProcessStatus::ContinueIfNotQuiet)
    }
}

/// The channel buffer at `index`, borrowed for the rest of the block.
fn take_channel<'b>(
    channels: &mut clack_plugin::process::audio::OutputChannels<'b, f32>,
    index: u32,
) -> Option<&'b mut [f32]> {
    let channel = channels.channel_mut(index)?;
    // SAFETY: the two channels of a port are distinct buffers, and each index is taken once per
    // block, so the slices never overlap; they live as long as the port's buffers.
    Some(unsafe { std::slice::from_raw_parts_mut(channel.as_mut_ptr(), channel.len()) })
}

impl Processor<'_> {
    fn start(&mut self, key: Match<u16>, note_id: Match<u32>, channel: Match<u16>, velocity: f64) {
        let key = match key {
            Match::Specific(key) => key as i16,
            Match::All => return,
        };
        let note_id = match note_id {
            Match::Specific(id) => id as i32,
            Match::All => -1,
        };
        let channel = match channel {
            Match::Specific(c) => c as i16,
            Match::All => 0,
        };
        let freq = 440.0 * 2f64.powf((key as f64 - 69.0) / 12.0);
        let voice = Voice {
            key,
            note_id,
            channel,
            phase: 0.0,
            step: freq / self.sample_rate,
            vel: velocity.clamp(0.0, 1.0),
            env: 0.0,
            released: false,
        };
        // A free slot, or else the one in release the longest, or else the first.
        let slot = self
            .voices
            .iter()
            .position(Option::is_none)
            .or_else(|| self.voices.iter().position(|v| v.is_some_and(|v| v.released)))
            .unwrap_or(0);
        self.voices[slot] = Some(voice);
    }

    fn stop(&mut self, key: Match<u16>, note_id: Match<u32>, channel: Match<u16>) {
        for voice in self.voices.iter_mut().flatten() {
            let key_ok = match key {
                Match::Specific(k) => voice.key == k as i16,
                Match::All => true,
            };
            let id_ok = match note_id {
                Match::Specific(id) => voice.note_id == id as i32 || voice.note_id < 0,
                Match::All => true,
            };
            let channel_ok = match channel {
                Match::Specific(c) => voice.channel == c as i16,
                Match::All => true,
            };
            if key_ok && id_ok && channel_ok {
                voice.released = true;
            }
        }
    }

    fn render(
        &mut self,
        mut left: Option<&mut [f32]>,
        mut right: Option<&mut [f32]>,
        from: usize,
        to: usize,
    ) {
        let level = self.shared.get(LEVEL).unwrap_or(DEFAULT_LEVEL);
        let balance = self.shared.get(BALANCE).unwrap_or(0.0);
        let (gain_left, gain_right) = ((1.0 - balance).min(1.0), (1.0 + balance).min(1.0));
        let attack = 1.0 / (ATTACK_SECONDS * self.sample_rate);
        let release = 1.0 / (RELEASE_SECONDS * self.sample_rate);
        for slot in self.voices.iter_mut() {
            let Some(voice) = slot else { continue };
            for i in from..to {
                if voice.released {
                    voice.env -= release;
                    if voice.env <= 0.0 {
                        *slot = None;
                        break;
                    }
                } else if voice.env < 1.0 {
                    voice.env = (voice.env + attack).min(1.0);
                }
                let sample =
                    (voice.phase * std::f64::consts::TAU).sin() * voice.env * voice.vel * level * 0.25;
                voice.phase = (voice.phase + voice.step).fract();
                if let Some(left) = left.as_deref_mut() {
                    left[i] += (sample * gain_left) as f32;
                }
                if let Some(right) = right.as_deref_mut() {
                    right[i] += (sample * gain_right) as f32;
                }
            }
        }
    }
}

impl PluginAudioProcessorParams for Processor<'_> {
    fn flush(&mut self, input: &InputEvents, _output: &mut OutputEvents) {
        for event in input {
            if let Some(CoreEventSpace::ParamValue(e)) = event.as_core_event()
                && let Some(id) = e.param_id()
            {
                self.shared.set(id.get(), e.value());
            }
        }
    }
}

impl PluginMainThreadParams for MainThread<'_> {
    fn count(&self) -> u32 {
        3
    }

    fn get_info(&self, index: u32, info: &mut ParamInfoWriter) {
        let (id, name, min, max, default): (u32, &[u8], f64, f64, f64) = match index {
            0 => (LEVEL, b"Level", 0.0, 1.0, DEFAULT_LEVEL),
            1 => (BALANCE, b"Balance", -1.0, 1.0, 0.0),
            2 => (MISBEHAVE, b"Misbehave on lowest note", 0.0, 4.0, 0.0),
            _ => return,
        };
        let flags = if id == MISBEHAVE {
            ParamInfoFlags::IS_STEPPED
        } else {
            ParamInfoFlags::IS_AUTOMATABLE
        };
        info.set(&ParamInfo {
            id: ClapId::new(id),
            flags,
            cookie: Default::default(),
            name,
            module: b"",
            min_value: min,
            max_value: max,
            default_value: default,
        });
    }

    fn get_value(&self, id: ClapId) -> Option<f64> {
        self.shared.get(id.get())
    }

    fn value_to_text(&self, id: ClapId, value: f64, writer: &mut ParamDisplayWriter) -> std::fmt::Result {
        match id.get() {
            LEVEL => write!(writer, "{:.0} %", value * 100.0),
            _ => write!(writer, "{value:+.2}"),
        }
    }

    fn text_to_value(&self, _id: ClapId, text: &CStr) -> Option<f64> {
        text.to_str()
            .ok()?
            .trim()
            .trim_end_matches('%')
            .trim()
            .parse()
            .ok()
    }

    fn flush(&self, input: &InputEvents, _output: &mut OutputEvents) {
        for event in input {
            if let Some(CoreEventSpace::ParamValue(e)) = event.as_core_event()
                && let Some(id) = e.param_id()
            {
                self.shared.set(id.get(), e.value());
            }
        }
    }
}

impl PluginStateImpl for MainThread<'_> {
    fn save(&self, output: &mut OutputStream) -> Result<(), PluginError> {
        let level = self.shared.get(LEVEL).unwrap_or(DEFAULT_LEVEL) as f32;
        let balance = self.shared.get(BALANCE).unwrap_or(0.0) as f32;
        output.write_all(STATE_MAGIC)?;
        output.write_all(&level.to_le_bytes())?;
        output.write_all(&balance.to_le_bytes())?;
        let misbehave = self.shared.get(MISBEHAVE).unwrap_or(0.0) as f32;
        output.write_all(&misbehave.to_le_bytes())?;
        Ok(())
    }

    fn load(&self, input: &mut InputStream) -> Result<(), PluginError> {
        let mut bytes = [0u8; 12];
        input.read_exact(&mut bytes)?;
        if &bytes[..4] != STATE_MAGIC {
            return Err(PluginError::Message("not a state of this plugin"));
        }
        let float = |at: usize| f32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes")) as f64;
        self.shared.set(LEVEL, float(4));
        self.shared.set(BALANCE, float(8));
        // Older states end here.
        let mut more = [0u8; 4];
        let misbehave = match input.read_exact(&mut more) {
            Ok(()) => f32::from_le_bytes(more) as f64,
            Err(_) => 0.0,
        };
        self.shared.set(MISBEHAVE, misbehave);
        Ok(())
    }
}

impl PluginAudioPortsImpl for MainThread<'_> {
    fn count(&self, is_input: bool) -> u32 {
        if is_input { 0 } else { 1 }
    }

    fn get(&self, index: u32, is_input: bool, writer: &mut AudioPortInfoWriter) {
        if !is_input && index == 0 {
            writer.set(&AudioPortInfo {
                id: ClapId::new(0),
                name: b"Output",
                channel_count: 2,
                flags: AudioPortFlags::IS_MAIN,
                port_type: Some(AudioPortType::STEREO),
                in_place_pair: None,
            });
        }
    }
}

impl PluginNotePortsImpl for MainThread<'_> {
    fn count(&self, is_input: bool) -> u32 {
        if is_input { 1 } else { 0 }
    }

    fn get(&self, index: u32, is_input: bool, writer: &mut NotePortInfoWriter) {
        if is_input && index == 0 {
            writer.set(&NotePortInfo {
                id: ClapId::new(0),
                name: b"Notes",
                supported_dialects: NoteDialects::CLAP | NoteDialects::MIDI,
                preferred_dialect: Some(NoteDialect::Clap),
            });
        }
    }
}

clack_export_entry!(SinglePluginEntry<TestSynth>);
