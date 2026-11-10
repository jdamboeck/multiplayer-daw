//! Pocket: the built-in polyphonic subtractive synth.
//!
//! Determinism rules. Every client must produce the same samples from the same notes and
//! parameters, so this file uses only `f32` with `+ - * /`, comparisons and the pure-Rust `libm`
//! functions. No `std` transcendental functions (they call the platform's C library), no SIMD
//! intrinsics, no `mul_add`. Any change to the sound needs a new `DSP_VERSION`.

use imbl::OrdMap;

/// Bumped whenever the output of the synth changes for the same input.
pub const DSP_VERSION: u16 = 1;
pub const VOICES: usize = 16;

/// How often per-voice filter coefficients are recomputed, in samples. Counted from each
/// voice's note-on, so it does not depend on the audio block size.
const CONTROL_INTERVAL: u32 = 16;

/// Envelope level below which a released voice is switched off (-80 dB).
const SILENCE: f32 = 1.0e-4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scale {
    Linear,
    /// The UI maps knob travel exponentially (times, frequencies).
    Log,
    /// Integer choices.
    Stepped,
}

#[derive(Clone, Copy, Debug)]
pub struct ParamInfo {
    pub id: u32,
    pub name: &'static str,
    pub unit: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub scale: Scale,
    /// Labels for stepped parameters, indexed by value.
    pub choices: &'static [&'static str],
}

const fn lin(id: u32, name: &'static str, unit: &'static str, min: f32, max: f32, default: f32) -> ParamInfo {
    ParamInfo {
        id,
        name,
        unit,
        min,
        max,
        default,
        scale: Scale::Linear,
        choices: &[],
    }
}
const fn log(id: u32, name: &'static str, unit: &'static str, min: f32, max: f32, default: f32) -> ParamInfo {
    ParamInfo {
        id,
        name,
        unit,
        min,
        max,
        default,
        scale: Scale::Log,
        choices: &[],
    }
}
const fn choice(id: u32, name: &'static str, choices: &'static [&'static str], default: f32) -> ParamInfo {
    ParamInfo {
        id,
        name,
        unit: "",
        min: 0.0,
        max: (choices.len() - 1) as f32,
        default,
        scale: Scale::Stepped,
        choices,
    }
}

const WAVES: &[&str] = &["Saw", "Pulse", "Sine"];

pub mod param {
    pub const OSC1_WAVE: usize = 0;
    pub const OSC2_WAVE: usize = 1;
    pub const OSC2_SEMI: usize = 2;
    pub const OSC2_FINE: usize = 3;
    pub const OSC_MIX: usize = 4;
    pub const PULSE_WIDTH: usize = 5;
    pub const NOISE: usize = 6;
    pub const AMP_ATTACK: usize = 7;
    pub const AMP_DECAY: usize = 8;
    pub const AMP_SUSTAIN: usize = 9;
    pub const AMP_RELEASE: usize = 10;
    pub const CUTOFF: usize = 11;
    pub const RESONANCE: usize = 12;
    pub const FILTER_ENV: usize = 13;
    pub const FILTER_ATTACK: usize = 14;
    pub const FILTER_DECAY: usize = 15;
    pub const FILTER_SUSTAIN: usize = 16;
    pub const FILTER_RELEASE: usize = 17;
    pub const KEY_TRACK: usize = 18;
    pub const VOLUME: usize = 19;
    pub const COUNT: usize = 20;
}

/// Parameter table. The index is the parameter ID used in `Device::params`; IDs are never reused.
pub const PARAMS: [ParamInfo; param::COUNT] = [
    choice(0, "Osc 1", WAVES, 0.0),
    choice(1, "Osc 2", WAVES, 0.0),
    lin(2, "Osc 2 semi", "st", -24.0, 24.0, 0.0),
    lin(3, "Osc 2 fine", "ct", -100.0, 100.0, 7.0),
    lin(4, "Osc mix", "", 0.0, 1.0, 0.5),
    lin(5, "Pulse width", "", 0.05, 0.95, 0.5),
    lin(6, "Noise", "", 0.0, 1.0, 0.0),
    log(7, "Attack", "s", 0.001, 5.0, 0.005),
    log(8, "Decay", "s", 0.001, 5.0, 0.25),
    lin(9, "Sustain", "", 0.0, 1.0, 0.7),
    log(10, "Release", "s", 0.001, 5.0, 0.3),
    log(11, "Cutoff", "Hz", 20.0, 20_000.0, 2_500.0),
    lin(12, "Resonance", "", 0.0, 1.0, 0.2),
    lin(13, "Filter env", "oct", -5.0, 5.0, 2.5),
    log(14, "F attack", "s", 0.001, 5.0, 0.005),
    log(15, "F decay", "s", 0.001, 5.0, 0.3),
    lin(16, "F sustain", "", 0.0, 1.0, 0.3),
    log(17, "F release", "s", 0.001, 5.0, 0.3),
    lin(18, "Key track", "", 0.0, 1.0, 0.5),
    lin(19, "Volume", "", 0.0, 1.0, 0.7),
];

/// Parameter values in natural units, clamped to their ranges.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PocketParams(pub [f32; param::COUNT]);

impl Default for PocketParams {
    fn default() -> Self {
        let mut values = [0.0; param::COUNT];
        for (value, info) in values.iter_mut().zip(PARAMS.iter()) {
            *value = info.default;
        }
        Self(values)
    }
}

impl PocketParams {
    /// Reads a device's parameter map. Missing or unknown entries mean "default".
    pub fn from_map(map: &OrdMap<u32, f64>) -> Self {
        let mut params = Self::default();
        for (value, info) in params.0.iter_mut().zip(PARAMS.iter()) {
            if let Some(v) = map.get(&info.id) {
                *value = sanitize(*v as f32, info);
            }
        }
        params
    }
}

fn sanitize(v: f32, info: &ParamInfo) -> f32 {
    if !v.is_finite() {
        return info.default;
    }
    let v = v.clamp(info.min, info.max);
    // Not `round()`: that is a libm call on some targets. Values are small and non-negative here.
    if info.scale == Scale::Stepped {
        (v + 0.5) as i32 as f32
    } else {
        v
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

/// Rates shared by all voices; derived from the time parameters.
#[derive(Clone, Copy, Default, Debug)]
struct EnvRates {
    attack_step: f32,
    decay_coef: f32,
    sustain: f32,
    release_coef: f32,
}

impl EnvRates {
    fn new(attack: f32, decay: f32, sustain: f32, release: f32, sample_rate: f32) -> Self {
        // ln(1000): the exponential stages fall by 60 dB within the nominal time.
        const LN_1000: f32 = 6.907_755_4;
        Self {
            attack_step: 1.0 / (attack * sample_rate),
            decay_coef: libm::expf(-LN_1000 / (decay * sample_rate)),
            sustain,
            release_coef: libm::expf(-LN_1000 / (release * sample_rate)),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Env {
    stage: Stage,
    level: f32,
}

impl Env {
    const IDLE: Env = Env {
        stage: Stage::Idle,
        level: 0.0,
    };

    #[inline]
    fn tick(&mut self, rates: &EnvRates) -> f32 {
        match self.stage {
            Stage::Idle => {}
            Stage::Attack => {
                self.level += rates.attack_step;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                self.level = rates.sustain + (self.level - rates.sustain) * rates.decay_coef;
                if self.level - rates.sustain < SILENCE {
                    self.level = rates.sustain;
                    self.stage = Stage::Sustain;
                }
            }
            Stage::Sustain => self.level = rates.sustain,
            Stage::Release => {
                self.level *= rates.release_coef;
                if self.level < SILENCE {
                    self.level = 0.0;
                    self.stage = Stage::Idle;
                }
            }
        }
        self.level
    }
}

#[derive(Clone, Copy, Debug)]
struct Voice {
    /// Identifies the note for note-off. Unique among sounding notes.
    key: u64,
    /// Order of note-ons; the lowest is the oldest voice.
    age: u64,
    pitch: f32,
    gain: f32,
    phase: [f32; 2],
    inc: [f32; 2],
    amp: Env,
    filter: Env,
    // State-variable filter: integrator states and coefficients.
    ic1: f32,
    ic2: f32,
    a1: f32,
    a2: f32,
    a3: f32,
    /// Samples since note-on.
    counter: u32,
    rng: u32,
}

impl Voice {
    const OFF: Voice = Voice {
        key: 0,
        age: 0,
        pitch: 60.0,
        gain: 0.0,
        phase: [0.0; 2],
        inc: [0.0; 2],
        amp: Env::IDLE,
        filter: Env::IDLE,
        ic1: 0.0,
        ic2: 0.0,
        a1: 0.0,
        a2: 0.0,
        a3: 0.0,
        counter: 0,
        rng: 1,
    };

    fn active(&self) -> bool {
        self.amp.stage != Stage::Idle
    }

    fn held(&self) -> bool {
        self.active() && self.amp.stage != Stage::Release
    }
}

/// One Pocket instance: 16 voices, mono output.
#[derive(Clone, Debug)]
pub struct Pocket {
    sample_rate: f32,
    params: PocketParams,
    amp_rates: EnvRates,
    filter_rates: EnvRates,
    voices: [Voice; VOICES],
    note_counter: u64,
}

impl Pocket {
    pub fn new(sample_rate: u32, params: PocketParams) -> Self {
        let mut synth = Self {
            sample_rate: sample_rate as f32,
            params,
            amp_rates: EnvRates::default(),
            filter_rates: EnvRates::default(),
            voices: [Voice::OFF; VOICES],
            note_counter: 0,
        };
        synth.derive();
        synth
    }

    pub fn params(&self) -> &PocketParams {
        &self.params
    }

    /// Takes effect at the next processed sample.
    pub fn set_params(&mut self, params: PocketParams) {
        if params == self.params {
            return;
        }
        self.params = params;
        self.derive();
        for i in 0..VOICES {
            if self.voices[i].active() {
                self.voices[i].inc = self.increments(self.voices[i].pitch);
            }
        }
    }

    fn derive(&mut self) {
        use param::*;
        let p = &self.params.0;
        let sr = self.sample_rate;
        self.amp_rates = EnvRates::new(p[AMP_ATTACK], p[AMP_DECAY], p[AMP_SUSTAIN], p[AMP_RELEASE], sr);
        self.filter_rates = EnvRates::new(
            p[FILTER_ATTACK],
            p[FILTER_DECAY],
            p[FILTER_SUSTAIN],
            p[FILTER_RELEASE],
            sr,
        );
    }

    fn increments(&self, pitch: f32) -> [f32; 2] {
        let p = &self.params.0;
        let detune = p[param::OSC2_SEMI] + p[param::OSC2_FINE] / 100.0;
        let limit = 0.45;
        [
            (note_hz(pitch) / self.sample_rate).min(limit),
            (note_hz(pitch + detune) / self.sample_rate).min(limit),
        ]
    }

    /// Starts a note. `key` is what `note_off` refers to; `vel` is 1..=127.
    pub fn note_on(&mut self, key: u64, pitch: u8, vel: u8) {
        let index = self.pick_voice();
        self.note_counter += 1;
        let pitch = pitch.min(127) as f32;
        let vel = vel.clamp(1, 127) as f32 / 127.0;
        // Seeded from the note itself, so a note sounds the same wherever rendering starts.
        let mut rng = seed(key, pitch as u32);
        let phase2 = unit(&mut rng);
        let previous_level = self.voices[index].amp.level;
        self.voices[index] = Voice {
            key,
            age: self.note_counter,
            pitch,
            gain: 0.2 + 0.8 * vel * vel,
            phase: [0.0, phase2],
            inc: self.increments(pitch),
            // A stolen voice ramps up from where it was instead of jumping to zero.
            amp: Env {
                stage: Stage::Attack,
                level: previous_level,
            },
            filter: Env {
                stage: Stage::Attack,
                level: 0.0,
            },
            rng,
            ..Voice::OFF
        };
    }

    /// Releases the held note with this key. Unknown keys are ignored.
    pub fn note_off(&mut self, key: u64) {
        for voice in &mut self.voices {
            if voice.key == key && voice.held() {
                voice.amp.stage = Stage::Release;
                voice.filter.stage = Stage::Release;
            }
        }
    }

    /// Releases every held note whose key satisfies `filter`.
    pub fn release_where(&mut self, filter: impl Fn(u64) -> bool) {
        for voice in &mut self.voices {
            if voice.held() && filter(voice.key) {
                voice.amp.stage = Stage::Release;
                voice.filter.stage = Stage::Release;
            }
        }
    }

    /// Silences everything at once.
    pub fn reset(&mut self) {
        self.voices = [Voice::OFF; VOICES];
    }

    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|v| v.active()).count()
    }

    /// First free voice, else the oldest released one, else the oldest.
    fn pick_voice(&self) -> usize {
        if let Some(free) = self.voices.iter().position(|v| !v.active()) {
            return free;
        }
        let oldest = |held: bool| {
            self.voices
                .iter()
                .enumerate()
                .filter(|(_, v)| v.held() == held)
                .min_by_key(|(_, v)| v.age)
                .map(|(i, _)| i)
        };
        oldest(false).or_else(|| oldest(true)).unwrap_or(0)
    }

    /// Adds the synth's output to `out`.
    pub fn process(&mut self, out: &mut [f32]) {
        use param::*;
        let p = self.params.0;
        let waves = [p[OSC1_WAVE] as u8, p[OSC2_WAVE] as u8];
        let mix = [1.0 - p[OSC_MIX], p[OSC_MIX]];
        let width = p[PULSE_WIDTH];
        let noise = p[NOISE];
        // Headroom for chords: four full-velocity voices reach full scale.
        let volume = p[VOLUME] * 0.25;
        let resonance = 2.0 - 1.9 * p[RESONANCE];
        let nyquist_guard = 0.45 * self.sample_rate;
        let pi_over_sr = core::f32::consts::PI / self.sample_rate;
        let (amp_rates, filter_rates) = (self.amp_rates, self.filter_rates);

        for voice in &mut self.voices {
            if !voice.active() {
                continue;
            }
            let key_octaves = p[KEY_TRACK] * (voice.pitch - 60.0) / 12.0;
            for sample in out.iter_mut() {
                let amp = voice.amp.tick(&amp_rates);
                let filter_env = voice.filter.tick(&filter_rates);
                if voice.amp.stage == Stage::Idle {
                    break;
                }

                if voice.counter % CONTROL_INTERVAL == 0 {
                    let octaves = p[FILTER_ENV] * filter_env + key_octaves;
                    let hz = (p[CUTOFF] * libm::exp2f(octaves)).clamp(20.0, nyquist_guard);
                    let g = libm::tanf(pi_over_sr * hz);
                    voice.a1 = 1.0 / (1.0 + g * (g + resonance));
                    voice.a2 = g * voice.a1;
                    voice.a3 = g * voice.a2;
                    // Keep the integrators out of the denormal range.
                    if voice.ic1.abs() < 1.0e-20 {
                        voice.ic1 = 0.0;
                    }
                    if voice.ic2.abs() < 1.0e-20 {
                        voice.ic2 = 0.0;
                    }
                }
                voice.counter = voice.counter.wrapping_add(1);

                let mut x = 0.0;
                for osc in 0..2 {
                    x += mix[osc] * oscillator(waves[osc], voice.phase[osc], voice.inc[osc], width);
                    voice.phase[osc] += voice.inc[osc];
                    if voice.phase[osc] >= 1.0 {
                        voice.phase[osc] -= 1.0;
                    }
                }
                if noise > 0.0 {
                    x += noise * (2.0 * unit(&mut voice.rng) - 1.0);
                }

                // Zero-delay-feedback state-variable filter, low-pass output.
                let v3 = x - voice.ic2;
                let v1 = voice.a1 * voice.ic1 + voice.a2 * v3;
                let v2 = voice.ic2 + voice.a2 * voice.ic1 + voice.a3 * v3;
                voice.ic1 = 2.0 * v1 - voice.ic1;
                voice.ic2 = 2.0 * v2 - voice.ic2;

                *sample += v2 * amp * voice.gain * volume;
            }
        }
    }
}

fn note_hz(pitch: f32) -> f32 {
    440.0 * libm::exp2f((pitch - 69.0) / 12.0)
}

#[inline]
fn oscillator(wave: u8, phase: f32, inc: f32, width: f32) -> f32 {
    match wave {
        0 => saw(phase, inc),
        1 => {
            let mut shifted = phase + 1.0 - width;
            if shifted >= 1.0 {
                shifted -= 1.0;
            }
            saw(phase, inc) - saw(shifted, inc) - (2.0 * width - 1.0)
        }
        _ => libm::sinf(core::f32::consts::TAU * phase),
    }
}

/// Band-limited sawtooth (polyBLEP).
#[inline]
fn saw(phase: f32, inc: f32) -> f32 {
    2.0 * phase - 1.0 - poly_blep(phase, inc)
}

#[inline]
fn poly_blep(t: f32, dt: f32) -> f32 {
    if t < dt {
        let x = t / dt;
        x + x - x * x - 1.0
    } else if t > 1.0 - dt {
        let x = (t - 1.0) / dt;
        x * x + x + x + 1.0
    } else {
        0.0
    }
}

fn seed(key: u64, pitch: u32) -> u32 {
    // SplitMix64 finaliser.
    let mut z = key ^ ((pitch as u64) << 56) ^ 0x9e37_79b9_7f4a_7c15;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    (z as u32) | 1
}

/// xorshift32 mapped to `[0, 1)`. Uses 24 bits, which convert to `f32` exactly.
#[inline]
fn unit(state: &mut u32) -> f32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    (x >> 8) as f32 * (1.0 / 16_777_216.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(synth: &mut Pocket, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames];
        synth.process(&mut out);
        out
    }

    #[test]
    fn parameter_ids_match_their_index() {
        for (index, info) in PARAMS.iter().enumerate() {
            assert_eq!(info.id as usize, index);
            assert!(
                info.min <= info.default && info.default <= info.max,
                "{}",
                info.name
            );
        }
    }

    #[test]
    fn a_note_sounds_and_then_ends() {
        let mut synth = Pocket::new(48_000, PocketParams::default());
        assert!(render(&mut synth, 64).iter().all(|s| *s == 0.0));
        synth.note_on(1, 60, 100);
        let held = render(&mut synth, 4800);
        let peak = held.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.02 && peak < 1.0, "peak {peak}");
        synth.note_off(1);
        render(&mut synth, 48_000);
        assert_eq!(synth.active_voices(), 0);
        assert!(render(&mut synth, 64).iter().all(|s| *s == 0.0));
    }

    #[test]
    fn output_does_not_depend_on_block_size() {
        let play = |block: usize| {
            let mut synth = Pocket::new(48_000, PocketParams::default());
            let mut out = vec![0.0f32; 6000];
            let mut done = 0;
            synth.note_on(1, 48, 127);
            synth.note_on(2, 55, 90);
            while done < out.len() {
                let n = block.min(out.len() - done);
                synth.process(&mut out[done..done + n]);
                done += n;
            }
            out
        };
        let reference = play(6000);
        for block in [1, 7, 64, 500] {
            assert!(play(block) == reference, "block size {block} changed the output");
        }
    }

    #[test]
    fn the_oldest_voice_is_stolen() {
        let mut synth = Pocket::new(48_000, PocketParams::default());
        for key in 0..VOICES as u64 {
            synth.note_on(key, 40 + key as u8, 100);
            render(&mut synth, 16);
        }
        assert_eq!(synth.active_voices(), VOICES);
        synth.note_on(99, 80, 100);
        assert_eq!(synth.active_voices(), VOICES);
        assert!(
            synth.voices.iter().all(|v| v.key != 0),
            "voice with key 0 should be gone"
        );
        assert!(synth.voices.iter().any(|v| v.key == 99));

        // A released voice is taken before a held one.
        synth.note_off(5);
        render(&mut synth, 16);
        synth.note_on(100, 81, 100);
        assert!(synth.voices.iter().all(|v| v.key != 5));
        assert!(synth.voices.iter().any(|v| v.key == 1));
    }

    #[test]
    fn stays_finite_at_extreme_settings() {
        let mut params = PocketParams::default();
        params.0[param::RESONANCE] = 1.0;
        params.0[param::CUTOFF] = 20_000.0;
        params.0[param::FILTER_ENV] = 5.0;
        params.0[param::NOISE] = 1.0;
        params.0[param::OSC1_WAVE] = 1.0;
        params.0[param::PULSE_WIDTH] = 0.05;
        let mut synth = Pocket::new(44_100, params);
        for key in 0..40u64 {
            synth.note_on(key, (key * 3) as u8 + 5, 127);
            let out = render(&mut synth, 2000);
            assert!(out.iter().all(|s| s.is_finite() && s.abs() < 16.0));
        }
    }
}
