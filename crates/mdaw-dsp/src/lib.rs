//! Deterministic built-in DSP: the Pocket synth, track compilation, the processing graph and
//! offline rendering. No device I/O; the same graph runs in the audio callback and offline.

pub mod compile;
pub mod external;
pub mod graph;
pub mod pocket;
pub mod render;

pub use compile::{Compiler, LIVE_KEY, sample_at, tick_at};
pub use external::{BlockTime, ExternalDevice, NoPlugins, PluginRef, PluginSource};
pub use graph::{Graph, MAX_BLOCK};
pub use render::{OfflineRender, render_offline, render_offline_with, render_with_block};
