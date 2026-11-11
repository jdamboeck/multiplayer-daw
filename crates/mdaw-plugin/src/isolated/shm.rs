//! The block of memory the DAW's audio thread and a helper's audio thread share: one block of
//! audio and its events, and two counters to hand it back and forth.
//!
//! The DAW writes the input and the events, raises `request` and wakes the helper. The helper
//! processes, writes the output over the input, sets `done` to the same number and wakes the
//! DAW. The DAW waits for that only until a deadline: a helper that is late, hung or dead costs
//! that block of its own sound. The wait itself is spent in the audio callback, though: with
//! several slow helpers the callback can run late and the whole output drop out.

use mdaw_dsp::{BlockTime, MAX_BLOCK};
use std::cell::UnsafeCell;
use std::fs::{File, OpenOptions};
use std::os::fd::FromRawFd;
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
    /// Frames into the block.
    pub offset: u32,
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
    /// Written by one side at a time, as the counters say whose turn it is.
    data: UnsafeCell<Data>,
}

/// The part of a [`Block`] the two sides take turns with.
#[repr(C)]
pub struct Data {
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
    /// The block's audio and events.
    ///
    /// # Safety
    ///
    /// Only the side whose turn it is may call this: the DAW while `done` equals `request`, until
    /// it raises `request`; the helper from seeing a new `request` until it raises `done`. Neither
    /// keeps the reference past its turn.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn data(&self) -> &mut Data {
        // SAFETY: as the caller promises, nobody else uses the data meanwhile.
        unsafe { &mut *self.data.get() }
    }
}

impl Data {
    pub fn time(&self) -> BlockTime {
        BlockTime {
            playing: self.playing != 0,
            tick: self.tick,
            seconds: self.seconds,
            bpm: self.bpm,
            deadline: None,
        }
    }

    pub fn set_time(&mut self, time: &BlockTime) {
        self.playing = time.playing as u32;
        self.tick = time.tick;
        self.seconds = time.seconds;
        self.bpm = time.bpm;
    }
}

/// A [`Block`] in memory both processes map.
pub struct Shared {
    map: memmap2::MmapMut,
}

// SAFETY: the mapping is plain memory; who writes what when is governed by the counters.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    /// Makes the block in anonymous memory and maps it. The file it returns is how a helper gets
    /// at it: no name anywhere, nothing left behind, nobody else can open it.
    pub fn create() -> std::io::Result<(Self, File)> {
        // SAFETY: a plain call with a valid name; the descriptor is owned from here.
        let fd = unsafe { libc::memfd_create(c"mdaw-block".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a descriptor just made, owned by nobody else.
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(std::mem::size_of::<Block>() as u64)?;
        Ok((Self::map(&file)?, file))
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
        // In memory for good, before the audio thread touches it: no page faults there. A
        // system that does not allow it gets the faults once, up front.
        // SAFETY: the mapping is live for the length given.
        if unsafe { libc::mlock(map.as_ptr().cast(), map.len()) } != 0 {
            let page = 4096;
            for at in (0..map.len()).step_by(page) {
                // SAFETY: in bounds of the mapping; a read is enough to fault the page in.
                unsafe { std::ptr::read_volatile(map.as_ptr().add(at)) };
            }
        }
        Ok(Self { map })
    }

    pub fn block(&self) -> &Block {
        // SAFETY: the mapping is as large as a `Block`, page-aligned, and every bit pattern of
        // it is a valid `Block` (atomics, integers, floats). Only the atomics are touched by
        // both sides at once; the rest is behind `Block::data`.
        unsafe { &*(self.map.as_ptr() as *const Block) }
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
