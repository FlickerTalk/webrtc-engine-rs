//! A call's audio pipeline: microphone → Opus → network, and network → jitter buffer → Opus →
//! speaker.

pub mod downlink;
pub mod uplink;

pub use downlink::{Downlink, ReceiveStats};
pub use uplink::{SendStats, Uplink};
