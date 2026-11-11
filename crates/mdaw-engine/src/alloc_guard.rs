//! Catches memory allocation on the audio thread.
//!
//! Binaries and tests opt in with
//! `#[global_allocator] static A: mdaw_engine::alloc_guard::GuardedAlloc = GuardedAlloc;`.
//! The audio callback marks itself with [`enter`]; any allocation or free while the mark is set
//! is counted, and aborts the process if [`set_abort`] asked for that. Without the allocator
//! installed, `enter` costs one thread-local write and nothing is checked.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

thread_local! {
    static IN_REALTIME: Cell<bool> = const { Cell::new(false) };
}

static VIOLATIONS: AtomicUsize = AtomicUsize::new(0);
static ABORT: AtomicBool = AtomicBool::new(false);

pub struct GuardedAlloc;

fn check() {
    // `try_with`: the allocator also runs while a thread is being torn down.
    if IN_REALTIME.try_with(Cell::get).unwrap_or(false) {
        VIOLATIONS.fetch_add(1, Relaxed);
        if ABORT.load(Relaxed) {
            use std::io::Write;
            let _ = std::io::stderr().write_all(b"mdaw: memory allocation on the audio thread\n");
            std::process::abort();
        }
    }
}

// SAFETY: defers to the system allocator; `check` only reads a thread-local flag and atomics.
unsafe impl GlobalAlloc for GuardedAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        check();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        check();
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        check();
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        check();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Marks the current thread as running real-time code until the returned value is dropped.
pub fn enter() -> Realtime {
    Realtime {
        was: IN_REALTIME.with(|flag| flag.replace(true)),
    }
}

/// Lifts the mark for code that is not this program's to police, such as a plugin's `process`,
/// until the returned value is dropped.
pub fn leave() -> Realtime {
    Realtime {
        was: IN_REALTIME.with(|flag| flag.replace(false)),
    }
}

pub struct Realtime {
    was: bool,
}

impl Drop for Realtime {
    fn drop(&mut self) {
        let was = self.was;
        let _ = IN_REALTIME.try_with(|flag| flag.set(was));
    }
}

/// Allocations and frees seen on real-time threads so far.
pub fn violations() -> usize {
    VIOLATIONS.load(Relaxed)
}

/// Abort the process on the first violation instead of only counting it.
pub fn set_abort(abort: bool) {
    ABORT.store(abort, Relaxed);
}
