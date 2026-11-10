//! Deterministic built-in DSP: the Pocket synth, track compilation, the processing graph and
//! offline rendering. No device I/O; the same graph runs in the audio callback and offline.

pub mod compile;
pub mod graph;
pub mod pocket;
pub mod render;

pub use compile::{Compiler, LIVE_KEY, sample_at, tick_at};
pub use graph::{Graph, MAX_BLOCK};
pub use render::{render_offline, render_with_block};
