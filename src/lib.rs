//! Media engine for WebRTC calls, on top of webrtc-rs.
//!
//! Every module works in one audio format: 48 kHz, mono, i16 PCM, in frames of 20 ms.

use std::time::Duration;

pub mod audio;
pub mod codec;
pub mod jitter;
pub mod netsim;
pub mod rtp;

/// The engine's sample rate, in Hz: Opus' native rate, so nothing is resampled between the
/// codec and the rings.
pub const SAMPLE_RATE: u32 = 48_000;
/// Duration of one frame: one Opus packet, one RTP packet, one jitter buffer slot.
pub const FRAME_DURATION: Duration = Duration::from_millis(20);
/// Samples in one 20 ms frame at [`SAMPLE_RATE`], mono.
pub const FRAME_SAMPLES: usize = 960;
/// One 20 ms frame of 48 kHz mono i16 PCM.
pub type Frame = [i16; FRAME_SAMPLES];

const _: () =
    assert!(FRAME_SAMPLES as u128 * 1000 == SAMPLE_RATE as u128 * FRAME_DURATION.as_millis());
