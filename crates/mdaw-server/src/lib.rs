//! The project server. One process serves one project: it orders the clients' edits into a log,
//! stores the log, and sends every client every event. It also keeps what is shared but not stored:
//! who is there, and the transport with the clock it runs on.

mod clock;
mod hub;
mod sequencer;
mod server;
mod store;

pub use clock::Clock;
pub use hub::{Hub, Sink};
pub use sequencer::Sequencer;
pub use server::{Config, Server};
pub use store::Store;
