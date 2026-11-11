//! MIDI input devices, through `midir`.

use crate::EngineError;
use midir::{Ignore, MidiInput, MidiInputConnection};

const CLIENT_NAME: &str = "mdaw";

/// What this program takes from a MIDI device. Every channel counts the same.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MidiMsg {
    On {
        pitch: u8,
        vel: u8,
    },
    Off {
        pitch: u8,
    },
    /// "All notes off" or "all sound off".
    AllOff,
}

impl MidiMsg {
    /// Reads one message; `None` for the ones this program has no use for.
    pub fn parse(bytes: &[u8]) -> Option<MidiMsg> {
        let (&status, data) = bytes.split_first()?;
        let byte = |i: usize| data.get(i).copied().filter(|b| *b < 0x80);
        match status & 0xF0 {
            0x90 => match (byte(0)?, byte(1)?) {
                (pitch, 0) => Some(MidiMsg::Off { pitch }),
                (pitch, vel) => Some(MidiMsg::On { pitch, vel }),
            },
            0x80 => Some(MidiMsg::Off { pitch: byte(0)? }),
            0xB0 if matches!(byte(0)?, 120 | 123) => Some(MidiMsg::AllOff),
            _ => None,
        }
    }
}

/// Whether two input names are the same device. ALSA ends names with the port's address, such
/// as `24:0`, which can change when the device is plugged in again.
pub fn same_device(a: &str, b: &str) -> bool {
    fn stable(name: &str) -> &str {
        match name.rsplit_once(' ') {
            Some((rest, address))
                if address
                    .split_once(':')
                    .is_some_and(|(c, p)| is_number(c) && is_number(p)) =>
            {
                rest
            }
            _ => name,
        }
    }
    fn is_number(s: &str) -> bool {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
    }
    stable(a) == stable(b)
}

/// The names of the MIDI inputs there are right now.
pub fn devices() -> Vec<String> {
    let Ok(input) = MidiInput::new(CLIENT_NAME) else {
        return Vec::new();
    };
    input
        .ports()
        .iter()
        .filter_map(|port| input.port_name(port).ok())
        .collect()
}

/// An open MIDI input. Closed when dropped.
pub struct MidiDevice {
    name: String,
    _connection: MidiInputConnection<()>,
}

impl MidiDevice {
    /// Opens the input called `name` (see [`same_device`]). `on_msg` is called on a thread of the MIDI system for
    /// every message, as it arrives.
    pub fn open(name: &str, mut on_msg: impl FnMut(MidiMsg) + Send + 'static) -> Result<Self, EngineError> {
        let mut input = MidiInput::new(CLIENT_NAME).map_err(crate::engine::err)?;
        input.ignore(Ignore::All);
        let port = input
            .ports()
            .into_iter()
            .find(|port| input.port_name(port).is_ok_and(|n| same_device(&n, name)))
            .ok_or_else(|| crate::engine::err(format!("no MIDI input called {name}")))?;
        let connection = input
            .connect(
                &port,
                "mdaw-in",
                move |_stamp, bytes, _| {
                    if let Some(msg) = MidiMsg::parse(bytes) {
                        on_msg(msg);
                    }
                },
                (),
            )
            .map_err(crate::engine::err)?;
        Ok(Self {
            name: name.to_string(),
            _connection: connection,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_messages_are_read_on_every_channel() {
        assert_eq!(
            MidiMsg::parse(&[0x90, 60, 100]),
            Some(MidiMsg::On { pitch: 60, vel: 100 })
        );
        assert_eq!(
            MidiMsg::parse(&[0x9F, 61, 1]),
            Some(MidiMsg::On { pitch: 61, vel: 1 })
        );
        // A start without velocity is an end, as many keyboards send it.
        assert_eq!(MidiMsg::parse(&[0x93, 60, 0]), Some(MidiMsg::Off { pitch: 60 }));
        assert_eq!(MidiMsg::parse(&[0x80, 60, 64]), Some(MidiMsg::Off { pitch: 60 }));
        assert_eq!(MidiMsg::parse(&[0xB0, 123, 0]), Some(MidiMsg::AllOff));
        assert_eq!(MidiMsg::parse(&[0xB0, 64, 127]), None);
        assert_eq!(MidiMsg::parse(&[0xF8]), None);
        assert_eq!(MidiMsg::parse(&[0x90, 60]), None);
        assert_eq!(MidiMsg::parse(&[0x90, 200, 100]), None);
        assert_eq!(MidiMsg::parse(&[]), None);
    }

    #[test]
    fn a_device_is_known_again_under_a_new_alsa_address() {
        let before = "Launchkey MK3 37:Launchkey MK3 37 LKMK3 MIDI In 24:0";
        assert!(same_device(
            before,
            "Launchkey MK3 37:Launchkey MK3 37 LKMK3 MIDI In 28:0"
        ));
        assert!(!same_device(
            before,
            "Launchkey MK3 37:Launchkey MK3 37 LKMK3 DAW In 24:1"
        ));
        assert!(same_device("Keystation", "Keystation"));
        assert!(!same_device("Keystation 1", "Keystation 2"));
    }
}
