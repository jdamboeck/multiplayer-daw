//! What the DAW and a helper say to each other on the helper's stdin and stdout: frames of a
//! `u32` length and postcard. The helper points its own stdout at stderr first, so that what
//! plugins print cannot get in between.

use crate::{ParamInfo, PluginFormat, PluginInfo, Settings};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// Largest frame either side accepts: a plugin state can be large.
const MAX_FRAME: usize = 256 << 20;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    /// Makes the instance: from `file`, or the built-in test synth without one. The audio goes
    /// through the shared block at `shm`.
    Create {
        format: PluginFormat,
        file: Option<String>,
        id: String,
        settings: Settings,
        shm: String,
    },
    Params {
        limit: u32,
    },
    SetParam {
        id: u32,
        value: f64,
    },
    SaveState,
    LoadState(Vec<u8>),
    OpenEditor {
        title: String,
    },
    CloseEditor,
    Quit,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Reply {
    Scanned(Vec<PluginInfo>),
    Created {
        info: PluginInfo,
        has_editor: bool,
        param_count: u32,
    },
    Params(Vec<ParamInfo>, u32),
    State(Vec<u8>),
    /// Unasked: the helper says how it is whenever that changes.
    Status {
        changed: bool,
        editor_open: bool,
        param_count: u32,
    },
    Done,
    Error(String),
}

pub fn write<T: Serialize>(out: &mut impl Write, msg: &T) -> io::Result<()> {
    let bytes = postcard::to_stdvec(msg).map_err(io::Error::other)?;
    out.write_all(&(bytes.len() as u32).to_le_bytes())?;
    out.write_all(&bytes)?;
    out.flush()
}

pub fn read<T: DeserializeOwned>(input: &mut impl Read) -> io::Result<T> {
    let mut len = [0u8; 4];
    input.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::other("frame too large"));
    }
    let mut bytes = vec![0u8; len];
    input.read_exact(&mut bytes)?;
    postcard::from_bytes(&bytes).map_err(io::Error::other)
}
