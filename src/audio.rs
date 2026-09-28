//! Device audio I/O: the bridge between the platform's audio callbacks and the engine.
//!
//! The engine works in one format only: 48 kHz, mono, i16 PCM, frames of 20 ms.

pub mod format;
pub mod frames;
pub mod resample;
pub mod ring;

use std::fmt;

/// The engine's sample rate.
pub const SAMPLE_RATE: u32 = 48_000;
/// Samples in one 20 ms frame at [`SAMPLE_RATE`], mono.
pub const FRAME_SAMPLES: usize = 960;
/// One 20 ms frame of 48 kHz mono i16 PCM.
pub type Frame = [i16; FRAME_SAMPLES];

/// Why audio could not be set up. Never raised from inside an audio callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioError {
    /// A sample rate or channel count the engine cannot work with.
    InvalidFormat { sample_rate: u32, channels: u16 },
    /// The platform has no input or output device to use.
    NoDevice,
    /// The platform audio API failed.
    Backend(String),
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFormat {
                sample_rate,
                channels,
            } => write!(f, "invalid audio format: {sample_rate} Hz, {channels} channels"),
            Self::NoDevice => write!(f, "no audio device"),
            Self::Backend(reason) => write!(f, "audio backend: {reason}"),
        }
    }
}

impl std::error::Error for AudioError {}

#[cfg(test)]
mod tests {}
