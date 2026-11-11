//! Client audio engine: owns the output device and the real-time thread, and keeps the graph
//! there in step with the document.

pub mod alloc_guard;
pub mod clock;
pub(crate) mod engine;
pub mod flac;
mod rt;

pub mod midi;

pub use engine::{Engine, EngineError, LiveInput, Stats};
