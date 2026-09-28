//! A call's audio pipeline: microphone → Opus → network, and network → jitter buffer → Opus →
//! speaker.

pub mod uplink;

pub use uplink::{SendStats, Uplink};
