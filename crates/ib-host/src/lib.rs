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
#[cfg(all(target_os = "linux", feature = "wayland", feature = "vaapi"))]
pub mod selftest;
pub mod server;
pub mod storage;
pub mod touch;
pub mod usb;
pub mod video;
#[cfg(all(target_os = "linux", feature = "wayland"))]
pub mod wayland;

pub use server::{Host, HostConfig};
