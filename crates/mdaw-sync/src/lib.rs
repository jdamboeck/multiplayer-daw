//! The client's copy of a project. Pure state machines: transactions and events in, state out.

mod loopback;
mod replica;

pub use loopback::Loopback;
pub use replica::{Gesture, Replica, SyncError};
