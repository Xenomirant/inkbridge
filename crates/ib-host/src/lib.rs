//! Inkbridge desktop host.
//!
//! Composition root: wires transport, session and pen injection together.
//! The pen path runs on its own high-priority OS thread ([`pen_thread`]), fed
//! through a channel, so async scheduling never delays injection.

pub mod clock;
pub mod pairing_page;
pub mod pen_thread;
pub mod platform;
pub mod scroll;
pub mod server;
pub mod storage;
pub mod touch;
pub mod video;

pub use server::{Host, HostConfig};
