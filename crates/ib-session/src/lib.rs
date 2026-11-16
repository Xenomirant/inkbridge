//! Session logic shared by the host and clients.
//!
//! - [`handshake`]: pairing and hello exchange on the control stream;
//! - [`pairing_store`]: the host's list of paired tablets;
//! - [`pen_stream`]: turns unreliable pen datagrams plus reliable transitions
//!   into a clean, ordered sample stream that can never leave the pen stuck.

pub mod handshake;
pub mod pairing_store;
pub mod pen_stream;

pub use handshake::{HostSettings, SessionError, client_handshake, host_handshake};
pub use pairing_store::{PairedDevice, PairingStore};
pub use pen_stream::PenStream;
