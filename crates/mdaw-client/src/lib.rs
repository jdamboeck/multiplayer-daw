//! The desktop client: an egui front end over a document replica and the audio engine.
//!
//! The UI reads the replica's view of the project and turns gestures into ops. It never changes
//! the document any other way, so the same code works against the in-process loopback (M1) and a
//! real server (M2 on).

mod app;
mod link;
pub mod profile;
mod session;
mod theme;
pub mod ui;

pub use app::{DawApp, Focus, Selection, Tab};
pub use link::{Waker, Who};
pub use session::{Displaced, Session, Status, Timing};
