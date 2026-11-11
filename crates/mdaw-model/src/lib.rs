//! Document model, IDs, ops and deterministic apply for the multiplayer DAW.
//!
//! Everything here is pure: no I/O, no clocks, no randomness. Client and server link the same
//! code, so a replica that applies the same ops in the same order reaches the same document.

mod apply;
mod diff;
mod doc;
mod ids;
mod op;
mod time;

pub use apply::{apply, apply_all};
pub use diff::{Affected, affected_ranges, listening_range};
pub use doc::{
    BuiltinKind, Checkout, Clip, Device, DeviceKind, Note, Project, Stem, Track, TrackContent, TrackShell,
};
pub use ids::{ActorId, BlobHash, ClipId, DeviceId, EntityId, IdGen, NoteId, TempoId, TrackId, UserId};
pub use op::{
    ClipProp, EntityRef, Invalid, MAX_NAME_BYTES, MixProp, NoteProp, Op, PropKey, Scope, ShellProp, fit_name,
};
pub use time::{
    DEFAULT_MILLI_BPM, MAX_MILLI_BPM, MAX_TICK, MIN_MILLI_BPM, PPQ, TICKS_PER_BAR, TempoMap, TempoPoint,
    Tick, TickRange,
};
