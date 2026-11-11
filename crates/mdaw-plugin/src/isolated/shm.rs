//! The block of memory the DAW's audio thread and a helper's audio thread share: one block of
//! audio and its events, and two counters to hand it back and forth.
//!
//! The DAW writes the input and the events, raises `request` and wakes the helper. The helper
//! processes, writes the output over the input, sets `done` to the same number and wakes the
//! DAW. The DAW waits for that only until a deadline: a helper that is late, hung or dead costs
//! that block of its own sound. The wait itself is spent in the audio callback, though: with
//! several slow helpers the callback can run late and the whole output drop out.

use mdaw_dsp::{BlockTime, MAX_BLOCK};
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// Events one block can carry.
pub const EVENTS: usize = 256;

pub const NOTE_ON: u32 = 1;
pub const NOTE_OFF: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Event {
    pub kind: u32,
    pub pitch: u32,
    pub vel: u32,
    pub _pad: u32,
    pub key: u64,
}

#[repr(C)]
pub struct Block {
    /// The number of the block the DAW asks for.
    pub request: AtomicU32,
    /// The number of the block the helper finished.
    pub done: AtomicU32,
    /// Set by the DAW: the helper's audio thread is to end.
    pub quit: AtomicU32,
    pub frames: u32,
    pub events_len: u32,
    pub playing: u32,
    pub tick: f64,
    pub seconds: f64,
    pub bpm: f64,
    pub events: [Event; EVENTS],
    pub left: [f32; MAX_BLOCK],
    pub right: [f32; MAX_BLOCK],
}

impl Block {
    pub fn time(&self) -> BlockTime {
        BlockTime {
            playing: self.playing != 0,
            tick: self.tick,
            seconds: self.seconds,
            bpm: self.bpm,
        }
    }

    pub fn set_time(&mut self, time: &BlockTime) {
        self.playing = time.playing as u32;
        self.tick = time.tick;
        self.seconds = time.seconds;
        self.bpm = time.bpm;
    }
}

/// A [`Block`] in a file both processes map.
pub struct Shared {
    map: memmap2::MmapMut,
}

// SAFETY: the mapping is plain memory; who writes what when is governed by the counters.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    /// Makes the file at `path` and maps it.
    pub fn create(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        file.set_len(std::mem::size_of::<Block>() as u64)?;
        Self::map(&file)
    }

    /// Maps the file a DAW made.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        if file.metadata()?.len() < std::mem::size_of::<Block>() as u64 {
            return Err(std::io::Error::other("the shared block is too small"));
        }
        Self::map(&file)
    }

    fn map(file: &std::fs::File) -> std::io::Result<Self> {
        // SAFETY: the file is ours and the helper's alone, and both treat it as a `Block`.
        let map = unsafe {
            memmap2::MmapOptions::new()
                .len(std::mem::size_of::<Block>())
                .map_mut(file)?
        };
        Ok(Self { map })
    }

    #[allow(clippy::mut_from_ref)]
    pub fn block(&self) -> &mut Block {
        // SAFETY: the mapping is as large as a `Block`, page-aligned, and every bit pattern of
        // it is a valid `Block` (atomics, integers, floats). The two sides take turns as the
        // counters say; the atomics are the only fields both touch at once.
        unsafe { &mut *(self.map.as_ptr() as *mut Block) }
    }
}

/// Waits while `atom` holds `expected`, at most `timeout`. Works across processes.
pub fn wait(atom: &AtomicU32, expected: u32, timeout: Duration) {
    if atom.load(Ordering::Acquire) != expected {
        return;
    }
    let spec = libc::timespec {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_nsec: timeout.subsec_nanos() as libc::c_long,
    };
    // SAFETY: a futex wait on a live 32-bit atomic, shared between processes (hence not the
    // private variant), with a relative timeout.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            atom.as_ptr(),
            libc::FUTEX_WAIT,
            expected,
            &spec as *const libc::timespec,
            std::ptr::null::<u32>(),
            0,
        );
    }
}

/// Wakes whoever waits on `atom`.
pub fn wake(atom: &AtomicU32) {
    // SAFETY: a futex wake on a live 32-bit atomic.
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            atom.as_ptr(),
            libc::FUTEX_WAKE,
            i32::MAX,
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<u32>(),
            0,
        );
    }
}
