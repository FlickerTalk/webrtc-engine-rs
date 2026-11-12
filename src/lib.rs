use std::time::Duration;

pub mod audio;
pub mod call;
pub mod codec;
pub mod jitter;
pub mod netsim;
pub mod rtp;
pub mod video;

/// Hz, Opus' native rate.
pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_DURATION: Duration = Duration::from_millis(20);
pub const FRAME_SAMPLES: usize = 960;
pub type Frame = [i16; FRAME_SAMPLES];

const _: () =
    assert!(FRAME_SAMPLES as u128 * 1000 == SAMPLE_RATE as u128 * FRAME_DURATION.as_millis());
