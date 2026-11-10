//! Client audio engine: owns the output device and the real-time thread, and keeps the graph
//! there in step with the document.

pub mod alloc_guard;
mod engine;
mod rt;

pub use engine::{Engine, EngineError, Stats};
