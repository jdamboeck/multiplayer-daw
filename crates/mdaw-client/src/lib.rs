//! The desktop client: an egui front end over a document replica and the audio engine.
//!
//! The UI reads the replica's view of the project and turns gestures into ops. It never changes
//! the document any other way, so the same code works against the in-process loopback (M1) and a
//! real server (M2 on).

mod app;
pub mod audio;
pub mod audio_old;
#[cfg(feature = "tracktion")]
pub mod audio_tracktion;
mod link;
pub mod live;
pub mod outbox;
pub mod plugins;
pub mod profile;
mod record;
mod session;
mod theme;
pub mod trace;
pub mod ui;

pub use app::{DawApp, Focus, Selection, Tab};
pub use audio::{Audio, DeviceStatus, ParamEntry, PluginEntry, TrackSound};
pub use audio_old::OldAudio;
pub use link::{Fetched, Fetcher, Upload, Waker, Who};
pub use session::{Displaced, Session, Status};
