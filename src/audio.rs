//! Device audio I/O: the bridge between the platform's audio callbacks and the engine.
//!
//! The engine works in one format only: 48 kHz, mono, i16 PCM, frames of 20 ms.
//!
//! ```text
//! mic callback -> CaptureAdapter -> ring -> CaptureFrames -> engine
//! engine -> PlayoutFrames -> ring -> PlayoutAdapter -> speaker callback
//! ```
//!
//! A platform backend owns the callbacks and the adapters; the engine owns the frame ends.

pub mod adapter;
#[cfg(feature = "desktop")]
pub mod desktop;
pub mod format;
pub mod frames;
pub mod resample;
pub mod ring;

use std::fmt;

use crate::{FRAME_SAMPLES, Frame, SAMPLE_RATE};

pub use adapter::{CaptureAdapter, Counter, PlayoutAdapter};
pub use format::{Sample, StreamFormat};
pub use frames::{CaptureFrames, PlayoutFrames};
pub use ring::{RingConsumer, RingProducer};

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
            } => write!(
                f,
                "invalid audio format: {sample_rate} Hz, {channels} channels"
            ),
            Self::NoDevice => write!(f, "no audio device"),
            Self::Backend(reason) => write!(f, "audio backend: {reason}"),
        }
    }
}

impl std::error::Error for AudioError {}

/// The device ends of the two rings, handed to a backend when it starts.
pub struct DeviceIo {
    /// Where the capture callback writes, through a [`CaptureAdapter`].
    pub capture: RingProducer,
    /// Where the playout callback reads, through a [`PlayoutAdapter`].
    pub playout: RingConsumer,
}

/// The engine ends of the two rings.
pub struct EngineIo {
    pub capture: CaptureFrames,
    pub playout: PlayoutFrames,
}

/// Creates a capture ring and a playout ring, each holding at least `frames` frames.
pub fn audio_io(frames: usize) -> (DeviceIo, EngineIo) {
    let samples = frames.saturating_mul(FRAME_SAMPLES);
    let (capture_producer, capture_consumer) = ring::ring(samples);
    let (playout_producer, playout_consumer) = ring::ring(samples);
    (
        DeviceIo {
            capture: capture_producer,
            playout: playout_consumer,
        },
        EngineIo {
            capture: CaptureFrames::new(capture_consumer),
            playout: PlayoutFrames::new(playout_producer),
        },
    )
}

/// A platform's audio devices: the iOS VoiceProcessingIO unit, Android AAudio, or `cpal` on
/// the desktop.
///
/// A backend opens the microphone and the speaker, learns the formats the OS gives it, wraps
/// `io` in a [`CaptureAdapter`] and a [`PlayoutAdapter`] and calls them from its callbacks.
pub trait AudioBackend {
    /// Opens the devices and starts moving audio between them and `io`.
    fn start(&mut self, io: DeviceIo) -> Result<(), AudioError>;
    /// Stops the devices and lets them go. Stopping a backend that is not running does nothing.
    fn stop(&mut self) -> Result<(), AudioError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered_frame(first: i16) -> Frame {
        let mut frame = [0; FRAME_SAMPLES];
        for (n, sample) in frame.iter_mut().enumerate() {
            *sample = first.wrapping_add(n as i16);
        }
        frame
    }

    /// Stands in for a platform: its "callbacks" run when the test says so, and the speaker
    /// output goes straight back into the microphone.
    #[derive(Default)]
    struct LoopbackBackend {
        running: Option<(CaptureAdapter, PlayoutAdapter)>,
    }

    impl LoopbackBackend {
        fn run_callbacks(&mut self, samples: usize) {
            if let Some((capture, playout)) = &mut self.running {
                let mut buffer = vec![0i16; samples];
                playout.fill(&mut buffer);
                capture.push(&buffer);
            }
        }
    }

    impl AudioBackend for LoopbackBackend {
        fn start(&mut self, io: DeviceIo) -> Result<(), AudioError> {
            let format = StreamFormat {
                sample_rate: SAMPLE_RATE,
                channels: 1,
            };
            self.running = Some((
                CaptureAdapter::new(format, io.capture)?,
                PlayoutAdapter::new(format, io.playout)?,
            ));
            Ok(())
        }

        fn stop(&mut self) -> Result<(), AudioError> {
            self.running = None;
            Ok(())
        }
    }

    #[test]
    fn both_rings_hold_the_frames_asked_for() {
        let (_device, engine) = audio_io(4);
        assert!(engine.playout.frames_free() >= 4);
    }

    #[test]
    fn a_backend_moves_frames_between_the_engine_and_its_callbacks() {
        let (device, mut engine) = audio_io(4);
        let mut loopback = LoopbackBackend::default();
        let backend: &mut dyn AudioBackend = &mut loopback;
        backend.start(device).unwrap();

        assert!(engine.playout.write_frame(&numbered_frame(0)));
        assert!(engine.playout.write_frame(&numbered_frame(5000)));

        // Callbacks rarely line up with frames.
        for size in [256, 700, 1004] {
            loopback.run_callbacks(size);
        }

        let mut frame = [0; FRAME_SAMPLES];
        assert!(engine.capture.read_frame(&mut frame));
        assert_eq!(frame, numbered_frame(0));
        assert!(engine.capture.read_frame(&mut frame));
        assert_eq!(frame, numbered_frame(5000));
        assert!(!engine.capture.read_frame(&mut frame));

        loopback.stop().unwrap();
        loopback.stop().unwrap();
    }
}
