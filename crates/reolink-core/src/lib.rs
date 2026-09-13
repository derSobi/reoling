//! P2P/UDP transport and client/session API for Reolink cameras/NVRs,
//! built on the wire-format primitives in `reolink-protocol`. This is the
//! layer both `apps/reoling-linux` and the future Windows app consume:
//! resolve a UID, connect (direct or via relay), log in, and stream video —
//! with no GUI-toolkit dependency of its own.

pub mod transport;
pub mod client;

pub use reolink_protocol::{Error, Result};
pub use client::{DeviceInfoSummary, ReolinkClient, VideoFrame};
