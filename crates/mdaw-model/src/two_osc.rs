//! The parameters of the built-in instrument on the Tracktion engine (`BuiltinKind::TwoOsc`,
//! shown to users as "Pocket"): two oscillators and a switchable sub oscillator, a filter with
//! its envelope, an amp envelope and global settings. The engine plays it with Tracktion's 4OSC.
//!
//! This table is part of the document format: a device's `params` holds plain values by the
//! ids below, in 4OSC's own units (Hz, dB, semitones, cents, %, seconds, milliseconds), choice
//! indices, or 0/1 for switches. Ids are never reused or renumbered; a new parameter gets a new
//! id. A missing entry means the default.

use crate::op::Invalid;
use imbl::OrdMap;

/// The `dsp_version` a new device of this kind carries.
pub const DSP_VERSION: u16 = 1;

/// How a knob's travel maps to a continuous value.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Curve {
    Linear,
    /// `travel = proportion ^ skew`, as JUCE's `NormalisableRange` (and so 4OSC) skews.
    Skew(f64),
    /// Exponential between `min` and `max` (both above 0).
    Log,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    /// A value between `min` and `max`; with `whole`, whole numbers only.
    Continuous {
        min: f64,
        max: f64,
        curve: Curve,
        whole: bool,
    },
    /// An index into the labels.
    Choice(&'static [&'static str]),
    /// 0 off, 1 on.
    Toggle,
}

/// The sections of the instrument, in the order they are shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    Osc1,
    Osc2,
    Sub,
    Filter,
    FilterEnv,
    AmpEnv,
    Global,
}

impl Group {
    pub const ALL: [Group; 7] = [
        Group::Osc1,
        Group::Osc2,
        Group::Sub,
        Group::Filter,
        Group::FilterEnv,
        Group::AmpEnv,
        Group::Global,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Group::Osc1 => "OSC 1",
            Group::Osc2 => "OSC 2",
            Group::Sub => "SUB",
            Group::Filter => "FILTER",
            Group::FilterEnv => "FILTER ENV",
            Group::AmpEnv => "AMP ENV",
            Group::Global => "GLOBAL",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Param {
    pub id: u32,
    pub group: Group,
    pub name: &'static str,
    /// "Hz", "dB", "st", "ct", "%", "s", "ms", or "" for choices and switches.
    pub unit: &'static str,
    pub kind: Kind,
    pub default: f64,
}

impl Param {
    pub fn min(&self) -> f64 {
        match self.kind {
            Kind::Continuous { min, .. } => min,
            Kind::Choice(_) | Kind::Toggle => 0.0,
        }
    }

    pub fn max(&self) -> f64 {
        match self.kind {
            Kind::Continuous { max, .. } => max,
            Kind::Choice(labels) => (labels.len() - 1) as f64,
            Kind::Toggle => 1.0,
        }
    }

    /// Whether `value` is one this parameter can hold.
    pub fn allows(&self, value: f64) -> bool {
        let whole = match self.kind {
            Kind::Continuous { whole, .. } => whole,
            Kind::Choice(_) | Kind::Toggle => true,
        };
        value.is_finite() && value >= self.min() && value <= self.max() && (!whole || value.fract() == 0.0)
    }

    /// The nearest value the parameter can hold (the default for one that is not a number).
    pub fn sanitize(&self, value: f64) -> f64 {
        if !value.is_finite() {
            return self.default;
        }
        let value = value.clamp(self.min(), self.max());
        match self.kind {
            Kind::Continuous { whole: false, .. } => value,
            _ => value.round(),
        }
    }

    /// Knob travel 0..1 for a value.
    pub fn to_travel(&self, value: f64) -> f64 {
        let (min, max) = (self.min(), self.max());
        let value = value.clamp(min, max);
        let curve = match self.kind {
            Kind::Continuous { curve, .. } => curve,
            Kind::Choice(_) | Kind::Toggle => Curve::Linear,
        };
        let travel = match curve {
            Curve::Linear => (value - min) / (max - min),
            Curve::Skew(skew) => ((value - min) / (max - min)).powf(skew),
            Curve::Log => (value / min).ln() / (max / min).ln(),
        };
        travel.clamp(0.0, 1.0)
    }

    /// The value at a knob travel 0..1, as the parameter can hold it.
    pub fn from_travel(&self, travel: f64) -> f64 {
        let (min, max) = (self.min(), self.max());
        let travel = travel.clamp(0.0, 1.0);
        let curve = match self.kind {
            Kind::Continuous { curve, .. } => curve,
            Kind::Choice(_) | Kind::Toggle => Curve::Linear,
        };
        let value = match curve {
            Curve::Linear => min + (max - min) * travel,
            Curve::Skew(skew) => min + (max - min) * travel.powf(1.0 / skew),
            Curve::Log => min * (max / min).powf(travel),
        };
        self.sanitize(value)
    }
}

/// Parameter ids. Part of the format: never changed or reused.
pub mod id {
    pub const OSC1_WAVE: u32 = 0;
    pub const OSC1_TUNE: u32 = 1;
    pub const OSC1_FINE: u32 = 2;
    pub const OSC1_LEVEL: u32 = 3;
    pub const OSC1_PW: u32 = 4;
    pub const OSC2_WAVE: u32 = 5;
    pub const OSC2_TUNE: u32 = 6;
    pub const OSC2_FINE: u32 = 7;
    pub const OSC2_LEVEL: u32 = 8;
    pub const OSC2_PW: u32 = 9;
    pub const SUB_ON: u32 = 10;
    pub const SUB_OCTAVE: u32 = 11;
    pub const SUB_WAVE: u32 = 12;
    pub const SUB_LEVEL: u32 = 13;
    pub const FILTER_TYPE: u32 = 14;
    pub const FILTER_FREQ: u32 = 15;
    pub const FILTER_RES: u32 = 16;
    pub const FILTER_SLOPE: u32 = 17;
    pub const FILTER_KEY: u32 = 18;
    pub const FILTER_ENV_AMOUNT: u32 = 19;
    pub const FILTER_VELOCITY: u32 = 20;
    pub const FILTER_ATTACK: u32 = 21;
    pub const FILTER_DECAY: u32 = 22;
    pub const FILTER_SUSTAIN: u32 = 23;
    pub const FILTER_RELEASE: u32 = 24;
    pub const AMP_ATTACK: u32 = 25;
    pub const AMP_DECAY: u32 = 26;
    pub const AMP_SUSTAIN: u32 = 27;
    pub const AMP_RELEASE: u32 = 28;
    pub const AMP_VELOCITY: u32 = 29;
    pub const MODE: u32 = 30;
    pub const GLIDE: u32 = 31;
    pub const LEVEL: u32 = 32;
    pub const COUNT: usize = 33;
}

/// Choice labels. The index is the stored value.
pub const OSC1_WAVES: &[&str] = &["Sine", "Square", "Saw", "Triangle", "Noise"];
pub const OSC2_WAVES: &[&str] = &["None", "Sine", "Square", "Saw", "Triangle", "Noise"];
pub const SUB_OCTAVES: &[&str] = &["-1", "-2"];
pub const SUB_WAVES: &[&str] = &["Sine", "Square"];
pub const FILTER_TYPES: &[&str] = &["Off", "LP", "HP", "BP", "Notch"];
pub const FILTER_SLOPES: &[&str] = &["12 dB", "24 dB"];
pub const MODES: &[&str] = &["Poly", "Mono", "Legato"];

/// 4OSC's filter frequency range: MIDI notes 0 to 135.076232, in Hz.
pub const FREQ_MIN: f64 = 8.175_798_915_643_707;
pub const FREQ_MAX: f64 = 20_000.0;

const fn cont(
    id: u32,
    group: Group,
    name: &'static str,
    unit: &'static str,
    (min, max): (f64, f64),
    curve: Curve,
    default: f64,
) -> Param {
    Param {
        id,
        group,
        name,
        unit,
        kind: Kind::Continuous {
            min,
            max,
            curve,
            whole: false,
        },
        default,
    }
}

const fn tune(id: u32, group: Group) -> Param {
    Param {
        id,
        group,
        name: "Tune",
        unit: "st",
        kind: Kind::Continuous {
            min: -36.0,
            max: 36.0,
            curve: Curve::Linear,
            whole: true,
        },
        default: 0.0,
    }
}

const fn choice(
    id: u32,
    group: Group,
    name: &'static str,
    labels: &'static [&'static str],
    default: f64,
) -> Param {
    Param {
        id,
        group,
        name,
        unit: "",
        kind: Kind::Choice(labels),
        default,
    }
}

const LEVEL: (f64, f64) = (-100.0, 0.0);
const PERCENT: (f64, f64) = (0.0, 100.0);
const BIPOLAR: (f64, f64) = (-100.0, 100.0);
const PW: (f64, f64) = (1.0, 99.0);
const AMP_TIME: (f64, f64) = (0.001, 60.0);
const FILTER_TIME: (f64, f64) = (0.0, 60.0);
/// 4OSC's own skews: levels expand the top, times the bottom of the range.
const LEVEL_CURVE: Curve = Curve::Skew(4.0);
const TIME_CURVE: Curve = Curve::Skew(0.2);

use Group::*;

/// The parameter table, indexed by id.
pub const PARAMS: [Param; id::COUNT] = [
    choice(id::OSC1_WAVE, Osc1, "Wave", OSC1_WAVES, 2.0),
    tune(id::OSC1_TUNE, Osc1),
    cont(id::OSC1_FINE, Osc1, "Fine", "ct", BIPOLAR, Curve::Linear, 0.0),
    cont(id::OSC1_LEVEL, Osc1, "Level", "dB", LEVEL, LEVEL_CURVE, -6.0),
    cont(id::OSC1_PW, Osc1, "PW", "%", PW, Curve::Linear, 50.0),
    choice(id::OSC2_WAVE, Osc2, "Wave", OSC2_WAVES, 3.0),
    tune(id::OSC2_TUNE, Osc2),
    cont(id::OSC2_FINE, Osc2, "Fine", "ct", BIPOLAR, Curve::Linear, 7.0),
    cont(id::OSC2_LEVEL, Osc2, "Level", "dB", LEVEL, LEVEL_CURVE, -6.0),
    cont(id::OSC2_PW, Osc2, "PW", "%", PW, Curve::Linear, 50.0),
    Param {
        id: id::SUB_ON,
        group: Sub,
        name: "On",
        unit: "",
        kind: Kind::Toggle,
        default: 0.0,
    },
    choice(id::SUB_OCTAVE, Sub, "Octave", SUB_OCTAVES, 0.0),
    choice(id::SUB_WAVE, Sub, "Wave", SUB_WAVES, 0.0),
    cont(id::SUB_LEVEL, Sub, "Level", "dB", LEVEL, LEVEL_CURVE, -6.0),
    choice(id::FILTER_TYPE, Filter, "Type", FILTER_TYPES, 1.0),
    cont(
        id::FILTER_FREQ,
        Filter,
        "Freq",
        "Hz",
        (FREQ_MIN, FREQ_MAX),
        Curve::Log,
        2500.0,
    ),
    cont(id::FILTER_RES, Filter, "Res", "%", PERCENT, Curve::Linear, 15.0),
    choice(id::FILTER_SLOPE, Filter, "Slope", FILTER_SLOPES, 0.0),
    cont(
        id::FILTER_KEY,
        Filter,
        "Key Trk",
        "%",
        PERCENT,
        Curve::Linear,
        50.0,
    ),
    cont(
        id::FILTER_ENV_AMOUNT,
        Filter,
        "Env Amt",
        "%",
        BIPOLAR,
        Curve::Linear,
        30.0,
    ),
    cont(
        id::FILTER_VELOCITY,
        Filter,
        "Velocity",
        "%",
        PERCENT,
        Curve::Linear,
        0.0,
    ),
    cont(
        id::FILTER_ATTACK,
        FilterEnv,
        "A",
        "s",
        FILTER_TIME,
        TIME_CURVE,
        0.005,
    ),
    cont(
        id::FILTER_DECAY,
        FilterEnv,
        "D",
        "s",
        FILTER_TIME,
        TIME_CURVE,
        0.3,
    ),
    cont(
        id::FILTER_SUSTAIN,
        FilterEnv,
        "S",
        "%",
        PERCENT,
        Curve::Linear,
        30.0,
    ),
    cont(
        id::FILTER_RELEASE,
        FilterEnv,
        "R",
        "s",
        FILTER_TIME,
        TIME_CURVE,
        0.3,
    ),
    cont(id::AMP_ATTACK, AmpEnv, "A", "s", AMP_TIME, TIME_CURVE, 0.005),
    cont(id::AMP_DECAY, AmpEnv, "D", "s", AMP_TIME, TIME_CURVE, 0.25),
    cont(id::AMP_SUSTAIN, AmpEnv, "S", "%", PERCENT, Curve::Linear, 70.0),
    cont(id::AMP_RELEASE, AmpEnv, "R", "s", AMP_TIME, TIME_CURVE, 0.3),
    cont(
        id::AMP_VELOCITY,
        AmpEnv,
        "Velocity",
        "%",
        PERCENT,
        Curve::Linear,
        100.0,
    ),
    choice(id::MODE, Global, "Mode", MODES, 0.0),
    cont(id::GLIDE, Global, "Glide", "ms", (0.0, 500.0), Curve::Linear, 0.0),
    cont(id::LEVEL, Global, "Level", "dB", LEVEL, LEVEL_CURVE, -3.0),
];

/// The parameter with this id.
pub fn param(id: u32) -> Option<&'static Param> {
    PARAMS.get(id as usize)
}

/// Checks a value for a parameter, as `Op::validate` and `Op::fits` do.
pub fn check(id: u32, value: f64) -> Result<(), Invalid> {
    match param(id) {
        None => Err(Invalid("the built-in instrument has no such parameter")),
        Some(p) if p.allows(value) => Ok(()),
        Some(_) => Err(Invalid("parameter value out of range")),
    }
}

/// All parameter values of a device, defaults filled in, each sanitized.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Values(pub [f64; id::COUNT]);

impl Default for Values {
    fn default() -> Self {
        let mut values = [0.0; id::COUNT];
        for (value, p) in values.iter_mut().zip(PARAMS.iter()) {
            *value = p.default;
        }
        Self(values)
    }
}

impl Values {
    /// From a device's parameter map; unknown ids are ignored.
    pub fn from_map(map: &OrdMap<u32, f64>) -> Self {
        let mut values = Self::default();
        for (value, p) in values.0.iter_mut().zip(PARAMS.iter()) {
            if let Some(v) = map.get(&p.id) {
                *value = p.sanitize(*v);
            }
        }
        values
    }

    pub fn get(&self, id: u32) -> f64 {
        self.0[id as usize]
    }

    /// A choice or switch as its index.
    pub fn index(&self, id: u32) -> usize {
        self.get(id) as usize
    }

    /// Whether a parameter makes a difference with the other settings as they are (the
    /// controls that do not are shown greyed out). Pulse width only acts on a square wave;
    /// oscillator 2 not at all with no wave; the sub's settings only when it is on; the filter's
    /// only with a filter; its envelope (and velocity, which scales the envelope) only with an
    /// envelope amount; glide only in legato mode.
    pub fn active(&self, param: u32) -> bool {
        let osc2 = self.index(id::OSC2_WAVE) != 0;
        let filter = self.index(id::FILTER_TYPE) != 0;
        let envelope = filter && self.get(id::FILTER_ENV_AMOUNT) != 0.0;
        match param {
            id::OSC1_PW => self.index(id::OSC1_WAVE) == 1,
            id::OSC2_TUNE | id::OSC2_FINE | id::OSC2_LEVEL => osc2,
            id::OSC2_PW => self.index(id::OSC2_WAVE) == 2,
            id::SUB_OCTAVE | id::SUB_WAVE | id::SUB_LEVEL => self.get(id::SUB_ON) != 0.0,
            id::FILTER_FREQ | id::FILTER_RES | id::FILTER_SLOPE | id::FILTER_KEY | id::FILTER_ENV_AMOUNT => {
                filter
            }
            id::FILTER_VELOCITY
            | id::FILTER_ATTACK
            | id::FILTER_DECAY
            | id::FILTER_SUSTAIN
            | id::FILTER_RELEASE => envelope,
            id::GLIDE => self.index(id::MODE) == 2,
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_indexed_by_id_and_defaults_are_allowed() {
        for (index, p) in PARAMS.iter().enumerate() {
            assert_eq!(p.id as usize, index, "{}", p.name);
            assert!(p.allows(p.default), "{} default {}", p.name, p.default);
            assert!(p.min() < p.max());
        }
    }

    #[test]
    fn travel_round_trips() {
        for p in &PARAMS {
            for travel in [0.0, 0.1, 0.25, 0.5, 0.8, 1.0] {
                let value = p.from_travel(travel);
                assert!(p.allows(value), "{} at {travel}: {value}", p.name);
                let back = p.from_travel(p.to_travel(value));
                assert!((back - value).abs() <= 1e-9 * value.abs().max(1.0), "{}", p.name);
            }
            assert!(
                (p.from_travel(0.0) - p.min()).abs() < 1e-9 && (p.from_travel(1.0) - p.max()).abs() < 1e-6
            );
        }
    }

    #[test]
    fn checks_refuse_what_the_table_does_not_hold() {
        assert!(check(id::FILTER_FREQ, 2500.0).is_ok());
        assert!(check(id::FILTER_FREQ, 5.0).is_err());
        assert!(check(id::FILTER_FREQ, 30_000.0).is_err());
        assert!(check(id::OSC1_WAVE, 4.0).is_ok());
        assert!(check(id::OSC1_WAVE, 5.0).is_err());
        assert!(check(id::OSC1_WAVE, 1.5).is_err());
        assert!(check(id::OSC1_TUNE, 0.5).is_err());
        assert!(check(id::SUB_ON, 1.0).is_ok());
        assert!(check(id::SUB_ON, 2.0).is_err());
        assert!(check(id::LEVEL, f64::NAN).is_err());
        assert!(check(id::COUNT as u32, 0.0).is_err());
    }

    #[test]
    fn ops_with_values_out_of_range_are_refused() {
        use crate::{ActorId, BuiltinKind, Device, DeviceKind, IdGen, Op, Project, TrackContent, TrackShell};
        let mut ids = IdGen::new(ActorId(1));
        let (track, device) = (ids.track(), ids.device());
        let kind = DeviceKind::Builtin {
            kind: BuiltinKind::TwoOsc,
            dsp_version: DSP_VERSION,
        };
        let mut synth = Device::new(device, kind);
        synth.params.insert(id::FILTER_FREQ, 2.0);
        let add = |synth: Device| Op::DeviceAdd {
            track,
            device: synth,
            after: None,
        };
        assert!(add(synth.clone()).validate().is_err());
        synth.params.insert(id::FILTER_FREQ, 200.0);
        assert!(add(synth.clone()).validate().is_ok());

        let mut project = Project::default();
        let mut content = TrackContent::default();
        content.devices.push_back(synth);
        crate::apply(
            &mut project,
            &Op::TrackAdd {
                id: track,
                shell: TrackShell {
                    name: "t".into(),
                    color: 0,
                },
                content,
                after: None,
            },
        );
        let set = |param, value| Op::DeviceSetParam {
            track,
            id: device,
            param,
            value,
        };
        assert!(set(id::SUB_ON, Some(1.0)).fits(&project).is_ok());
        assert!(set(id::SUB_ON, Some(3.0)).fits(&project).is_err());
        assert!(set(99, Some(0.0)).fits(&project).is_err());
        assert!(
            set(99, None).fits(&project).is_ok(),
            "back to the default is always allowed"
        );
    }

    #[test]
    fn inactive_controls() {
        let mut values = Values::default();
        assert!(!values.active(id::OSC1_PW) && !values.active(id::SUB_LEVEL) && !values.active(id::GLIDE));
        assert!(values.active(id::FILTER_ATTACK) && values.active(id::OSC2_FINE));
        values.0[id::OSC2_WAVE as usize] = 0.0;
        values.0[id::FILTER_ENV_AMOUNT as usize] = 0.0;
        values.0[id::SUB_ON as usize] = 1.0;
        assert!(!values.active(id::OSC2_FINE) && !values.active(id::FILTER_ATTACK));
        assert!(values.active(id::SUB_LEVEL) && values.active(id::FILTER_FREQ));
        let mut map = OrdMap::new();
        map.insert(id::FILTER_FREQ, 1e9);
        map.insert(id::MODE, 1.7);
        let v = Values::from_map(&map);
        assert_eq!(v.get(id::FILTER_FREQ), FREQ_MAX);
        assert_eq!(v.index(id::MODE), 2);
    }
}
