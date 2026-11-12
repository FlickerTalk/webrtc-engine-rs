pub mod adapter;
// Only the pure parts (format, callbacks) are tested on the host. `AaudioBackend` is Android only.
#[cfg(any(target_os = "android", test))]
pub mod android;
#[cfg(feature = "desktop")]
pub mod desktop;
pub mod format;
pub mod frames;
// The format and buffer handling is plain Rust, so the host tests build it too.
#[cfg(any(target_os = "ios", test))]
pub mod ios;
pub mod resample;
pub mod ring;

use std::fmt;

use crate::{FRAME_SAMPLES, Frame, SAMPLE_RATE};

pub use adapter::{CaptureAdapter, Counter, PlayoutAdapter};
pub use format::{Sample, StreamFormat};
pub use frames::{CaptureFrames, PlayoutFrames};
pub use ring::{RingConsumer, RingProducer};

/// Never raised from inside an audio callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioError {
    InvalidFormat { sample_rate: u32, channels: u16 },
    NoDevice,
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

pub struct DeviceIo {
    pub capture: RingProducer,
    pub playout: RingConsumer,
}

pub struct EngineIo {
    pub capture: CaptureFrames,
    pub playout: PlayoutFrames,
}

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

pub trait AudioBackend {
    fn start(&mut self, io: DeviceIo) -> Result<(), AudioError>;
    fn stop(&mut self) -> Result<(), AudioError>;
    /// Call it about every 100 ms from the owning thread, never from an audio callback. On Android
    /// a headset plugged in or out kills both streams and only this reopens them.
    fn maintain(&mut self) -> Result<(), AudioError> {
        Ok(())
    }
}

pub fn platform_backend() -> Result<Box<dyn AudioBackend + Send>, AudioError> {
    #[cfg(target_os = "ios")]
    {
        Ok(Box::new(ios::VoiceProcessingBackend::new()?))
    }
    #[cfg(target_os = "android")]
    {
        Ok(Box::new(android::AaudioBackend::new()?))
    }
    #[cfg(all(
        feature = "desktop",
        not(any(target_os = "ios", target_os = "android"))
    ))]
    {
        Ok(Box::new(desktop::DesktopBackend::new()))
    }
    #[cfg(not(any(target_os = "ios", target_os = "android", feature = "desktop")))]
    {
        Err(AudioError::NoDevice)
    }
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

    struct UpkeepBackend {
        maintained: Counter,
    }

    impl AudioBackend for UpkeepBackend {
        fn start(&mut self, _io: DeviceIo) -> Result<(), AudioError> {
            Ok(())
        }

        fn stop(&mut self) -> Result<(), AudioError> {
            Ok(())
        }

        fn maintain(&mut self) -> Result<(), AudioError> {
            self.maintained.add(1);
            Ok(())
        }
    }

    #[test]
    fn a_backend_with_no_upkeep_maintains_as_a_no_op() {
        let (device, mut engine) = audio_io(4);
        let mut loopback = LoopbackBackend::default();
        loopback.start(device).unwrap();

        assert_eq!(loopback.maintain(), Ok(()));

        assert!(engine.playout.write_frame(&numbered_frame(0)));
        loopback.run_callbacks(FRAME_SAMPLES);
        let mut frame = [0; FRAME_SAMPLES];
        assert!(engine.capture.read_frame(&mut frame));
        assert_eq!(frame, numbered_frame(0));
    }

    #[test]
    fn maintain_reaches_the_backend_behind_a_boxed_trait_object() {
        let maintained = Counter::default();
        let mut backend: Box<dyn AudioBackend + Send> = Box::new(UpkeepBackend {
            maintained: maintained.clone(),
        });
        let (device, _engine) = audio_io(4);
        backend.start(device).unwrap();

        for _ in 0..3 {
            backend.maintain().unwrap();
        }

        assert_eq!(maintained.get(), 3);
    }

    #[cfg(not(any(target_os = "ios", target_os = "android", feature = "desktop")))]
    #[test]
    fn without_a_platform_backend_there_is_no_device() {
        assert!(matches!(platform_backend(), Err(AudioError::NoDevice)));
    }

    #[cfg(all(
        feature = "desktop",
        not(any(target_os = "ios", target_os = "android"))
    ))]
    #[test]
    fn the_desktop_feature_gives_a_stopped_desktop_backend() {
        let mut backend = platform_backend().unwrap();
        assert_eq!(backend.maintain(), Ok(()));
        assert_eq!(backend.stop(), Ok(()));
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
