//! The built-in instrument (`BuiltinKind::TwoOsc`, "Pocket" to users) on Tracktion's 4OSC.
//!
//! The model keeps its values in 4OSC's own units (`mdaw_model::two_osc`), so each one goes to
//! 4OSC unchanged in meaning; only the normalisation (4OSC's ranges and skews,
//! `tracktion_FourOscPlugin.cpp`) and a few unit changes (Hz to a MIDI note, % to a fraction)
//! are done here. Settings 4OSC keeps as properties of its state rather than as parameters (wave
//! shapes, filter type and slope, voice mode) are patched into the state.
//!
//! | Model                         | 4OSC                         | conversion                      |
//! |-------------------------------|------------------------------|---------------------------------|
//! | 0 OSC 1 Wave                  | state `waveShape1`           | Sine..Noise → 1..5              |
//! | 1, 2 OSC 1 Tune, Fine         | `tune1`, `fineTune1`         | same (st, ct)                   |
//! | 3 OSC 1 Level                 | `level1`                     | same (dB)                       |
//! | 4 OSC 1 PW                    | `pulseWidth1`                | % → fraction                    |
//! | 5 OSC 2 Wave                  | state `waveShape2`           | None..Noise → 0..5              |
//! | 6..9 OSC 2 Tune, Fine, Level, PW | `tune2` .. `pulseWidth2`  | as oscillator 1                 |
//! | 10 SUB On, 12 SUB Wave        | state `waveShape3`           | off → 0; Sine 1, Square 2       |
//! | 11 SUB Octave                 | `tune3`                      | -1 → -12 st, -2 → -24 st        |
//! | 13 SUB Level                  | `level3`                     | same (dB)                       |
//! | 14 FILTER Type                | state `filterType`           | Off, LP, HP, BP, Notch → 0..4   |
//! | 15 FILTER Freq                | `filterFreq`                 | Hz → MIDI note                  |
//! | 16 FILTER Res                 | `filterResonance`            | same (%)                        |
//! | 17 FILTER Slope               | state `filterSlope`          | 12, 24                          |
//! | 18 FILTER Key Trk             | `filterKey`                  | same (%)                        |
//! | 19 FILTER Env Amt             | `filterAmount`               | % → fraction                    |
//! | 20 FILTER Velocity            | `filterVelocity`             | same (%)                        |
//! | 21..24 FILTER ENV A, D, S, R  | `filterAttack` .. `filterRelease` | same (s, %)                |
//! | 25..28 AMP ENV A, D, S, R     | `ampAttack` .. `ampRelease`  | same (s, %)                     |
//! | 29 AMP ENV Velocity           | `ampVelocity`                | same (%)                        |
//! | 30 GLOBAL Mode                | state `voiceMode`            | Poly 2, Mono 0, Legato 1        |
//! | 31 GLOBAL Glide               | `legato`                     | same (ms)                       |
//! | 32 GLOBAL Level               | `masterLevel`                | same (dB)                       |
//!
//! Fixed: oscillator 3 (the sub) has fine tune 0 and pulse width 0.5; oscillator 4 stays off
//! (its default). 4OSC's LFOs and modulation envelopes are not routed anywhere (its modulation
//! matrix starts empty) and its effects start switched off, so they do not colour the sound.

use crate::apply::FourOscRange;
use mdaw_model::two_osc::{Values, id};

/// Where a model parameter goes in 4OSC.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Target {
    /// One 4OSC parameter, from this value alone, converted to 4OSC's units.
    Param {
        four_osc: &'static str,
        range: FourOscRange,
        convert: fn(f64) -> f64,
    },
    /// A property of 4OSC's state; see [`state_properties`].
    State,
}

fn same(v: f64) -> f64 {
    v
}

fn fraction(percent: f64) -> f64 {
    percent / 100.0
}

/// Hz to the MIDI note 4OSC's filter frequency is kept in.
pub(crate) fn hz_to_note(hz: f64) -> f64 {
    12.0 * (hz.max(1.0) / 440.0).log2() + 69.0
}

fn sub_tune(octave: f64) -> f64 {
    -12.0 * (octave.round() + 1.0)
}

const fn param(four_osc: &'static str, range: FourOscRange, convert: fn(f64) -> f64) -> Target {
    Target::Param {
        four_osc,
        range,
        convert,
    }
}

use crate::apply::four_osc_range as r;

/// Where each model parameter goes; `None` for an id the instrument does not have.
pub(crate) fn target(param_id: u32) -> Option<Target> {
    Some(match param_id {
        id::OSC1_TUNE => param("tune1", r::TUNE, same),
        id::OSC1_FINE => param("fineTune1", r::FINE_TUNE, same),
        id::OSC1_LEVEL => param("level1", r::LEVEL_DB, same),
        id::OSC1_PW => param("pulseWidth1", r::PULSE_WIDTH, fraction),
        id::OSC2_TUNE => param("tune2", r::TUNE, same),
        id::OSC2_FINE => param("fineTune2", r::FINE_TUNE, same),
        id::OSC2_LEVEL => param("level2", r::LEVEL_DB, same),
        id::OSC2_PW => param("pulseWidth2", r::PULSE_WIDTH, fraction),
        id::SUB_OCTAVE => param("tune3", r::TUNE, sub_tune),
        id::SUB_LEVEL => param("level3", r::LEVEL_DB, same),
        id::FILTER_FREQ => param("filterFreq", r::FILTER_NOTE, hz_to_note),
        id::FILTER_RES => param("filterResonance", r::PERCENT, same),
        id::FILTER_KEY => param("filterKey", r::PERCENT, same),
        id::FILTER_ENV_AMOUNT => param("filterAmount", r::FILTER_AMOUNT, fraction),
        id::FILTER_VELOCITY => param("filterVelocity", r::PERCENT, same),
        id::FILTER_ATTACK => param("filterAttack", r::FILTER_TIME, same),
        id::FILTER_DECAY => param("filterDecay", r::FILTER_TIME, same),
        id::FILTER_SUSTAIN => param("filterSustain", r::PERCENT, same),
        id::FILTER_RELEASE => param("filterRelease", r::FILTER_TIME, same),
        id::AMP_ATTACK => param("ampAttack", r::AMP_TIME, same),
        id::AMP_DECAY => param("ampDecay", r::AMP_TIME, same),
        id::AMP_SUSTAIN => param("ampSustain", r::PERCENT, same),
        id::AMP_RELEASE => param("ampRelease", r::AMP_TIME, same),
        id::AMP_VELOCITY => param("ampVelocity", r::PERCENT, same),
        id::GLIDE => param("legato", r::GLIDE, same),
        id::LEVEL => param("masterLevel", r::LEVEL_DB, same),
        id::OSC1_WAVE
        | id::OSC2_WAVE
        | id::SUB_ON
        | id::SUB_WAVE
        | id::FILTER_TYPE
        | id::FILTER_SLOPE
        | id::MODE => Target::State,
        _ => return None,
    })
}

/// 4OSC parameters that do not follow any model parameter: (id, range, plain value).
pub(crate) const FIXED_PARAMS: &[(&str, FourOscRange, f64)] = &[
    ("fineTune3", r::FINE_TUNE, 0.0),
    ("pulseWidth3", r::PULSE_WIDTH, 0.5),
];

/// The properties of 4OSC's state the instrument sets, from all of its values.
pub(crate) fn state_properties(values: &Values) -> [(&'static str, i32); 7] {
    let sub = if values.get(id::SUB_ON) != 0.0 {
        values.index(id::SUB_WAVE) as i32 + 1
    } else {
        0
    };
    let mode = match values.index(id::MODE) {
        1 => 0, // mono
        2 => 1, // legato
        _ => 2, // poly
    };
    [
        // tracktion::Oscillator::Waves: none 0, sine 1, square 2, saw 3, triangle 4, noise 5.
        ("waveShape1", values.index(id::OSC1_WAVE) as i32 + 1),
        ("waveShape2", values.index(id::OSC2_WAVE) as i32),
        ("waveShape3", sub),
        ("waveShape4", 0),
        ("filterType", values.index(id::FILTER_TYPE) as i32),
        (
            "filterSlope",
            if values.index(id::FILTER_SLOPE) == 1 {
                24
            } else {
                12
            },
        ),
        ("voiceMode", mode),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdaw_model::two_osc::{Kind, PARAMS};

    #[test]
    fn every_parameter_has_a_place_in_four_osc() {
        for p in &PARAMS {
            let t = target(p.id).unwrap_or_else(|| panic!("{} has no target", p.name));
            let continuous = matches!(p.kind, Kind::Continuous { .. });
            assert_eq!(
                continuous,
                matches!(t, Target::Param { .. }) && p.id != id::SUB_OCTAVE,
                "{}: continuous values go to parameters, settings to the state",
                p.name
            );
        }
        assert!(target(id::COUNT as u32).is_none());
    }

    #[test]
    fn defaults_in_four_osc_units() {
        let values = Values::default();
        let plain = |model: u32| match target(model) {
            Some(Target::Param { convert, .. }) => convert(values.get(model)),
            _ => panic!("not a parameter"),
        };
        assert!((plain(id::FILTER_FREQ) - (12.0 * (2500.0f64 / 440.0).log2() + 69.0)).abs() < 1e-9);
        assert!((hz_to_note(mdaw_model::two_osc::FREQ_MIN)).abs() < 1e-9);
        assert!((hz_to_note(mdaw_model::two_osc::FREQ_MAX) - 135.076_232).abs() < 1e-5);
        assert_eq!(plain(id::OSC1_PW), 0.5);
        assert_eq!(plain(id::SUB_OCTAVE), -12.0);
        assert_eq!(plain(id::FILTER_ENV_AMOUNT), 0.3);
        assert_eq!(
            state_properties(&values),
            [
                ("waveShape1", 3),
                ("waveShape2", 3),
                ("waveShape3", 0),
                ("waveShape4", 0),
                ("filterType", 1),
                ("filterSlope", 12),
                ("voiceMode", 2),
            ]
        );
        let mut on = values;
        on.0[id::SUB_ON as usize] = 1.0;
        on.0[id::SUB_WAVE as usize] = 1.0;
        on.0[id::MODE as usize] = 2.0;
        let props = state_properties(&on);
        assert_eq!(props[2], ("waveShape3", 2));
        assert_eq!(props[6], ("voiceMode", 1));
    }
}
