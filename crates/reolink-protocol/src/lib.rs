//! Baichuan wire protocol for Reolink cameras/NVRs: binary framing, XML
//! payloads, and encryption (BCEncrypt/AES), built from scratch against the
//! observed wire format. No transport or session logic lives here — see
//! `reolink-core` for the P2P/UDP transport and the client/session API built
//! on top of this crate.

pub mod bc;
pub mod bcudp;
pub mod bcmedia;
pub mod crypto;
mod error;

pub use error::{Error, Result};
