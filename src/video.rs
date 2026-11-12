pub mod assemble;
pub mod bitrate;
pub mod call;
pub mod fake;
pub mod h264;
pub mod packet;
pub mod rtp;

#[cfg(feature = "desktop")]
pub mod desktop;
#[cfg(feature = "openh264")]
pub mod openh264;

#[cfg(any(target_os = "ios", test))]
pub mod ios;

// Only the pure parts run on the host, in tests. The camera and the codecs are Android only.
#[cfg(any(target_os = "android", test))]
pub mod android;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// Annex-B. A keyframe starts with its SPS and PPS.
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Monotonic capture time. Only the differences are used.
    pub timestamp: Duration,
    pub rotation: Rotation,
}

/// The camera picks its closest mode, so the real size is the one in the SPS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoConfig {
    /// In sensor orientation (landscape), before any rotation.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
}

/// Below this 320×240 video breaks into blocks.
pub const MIN_BITRATE_BPS: u32 = 150_000;
pub const MAX_BITRATE_BPS: u32 = 2_500_000;

pub fn clamp_bitrate(bps: u32) -> u32 {
    bps.clamp(MIN_BITRATE_BPS, MAX_BITRATE_BPS)
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            width: 640,
            height: 480,
            fps: 30,
            bitrate_bps: 800_000,
        }
    }
}

/// Clockwise, applied by the receiver. Sources never rotate the pixels, it travels as CVO.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    Deg0,
    Deg90,
    Deg180,
    Deg270,
}

pub const CVO_URI: &str = "urn:3gpp:video-orientation";

impl Rotation {
    /// `0 0 0 0 C F R1 R0` with the camera and flip bits at zero, as libwebrtc sends it.
    pub fn to_cvo(self) -> u8 {
        match self {
            Self::Deg0 => 0,
            Self::Deg90 => 1,
            Self::Deg180 => 2,
            Self::Deg270 => 3,
        }
    }

    pub fn from_cvo(byte: u8) -> Self {
        match byte & 0b11 {
            0 => Self::Deg0,
            1 => Self::Deg90,
            2 => Self::Deg180,
            _ => Self::Deg270,
        }
    }

    pub fn degrees(self) -> u16 {
        u16::from(self.to_cvo()) * 90
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Facing {
    #[default]
    Front,
    Back,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoError {
    NoCamera,
    PermissionDenied,
    Unsupported,
    Backend(String),
}

impl fmt::Display for VideoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCamera => f.write_str("no camera"),
            Self::PermissionDenied => f.write_str("camera permission denied"),
            Self::Unsupported => f.write_str("video mode not supported"),
            Self::Backend(reason) => write!(f, "video backend: {reason}"),
        }
    }
}

impl std::error::Error for VideoError {}

pub const FRAME_CHANNEL_CAPACITY: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum SendOutcome {
    Sent,
    Dropped,
    /// The engine side is gone, the source should stop.
    Closed,
}

struct ChannelState {
    dropped: AtomicU64,
    keyframe_needed: AtomicBool,
}

/// Never blocks, so the camera thread can use it. After a drop, delta frames are dropped too
/// until a keyframe gets through.
#[derive(Clone)]
pub struct FrameSender {
    inner: SyncSender<EncodedFrame>,
    state: Arc<ChannelState>,
}

impl FrameSender {
    pub fn try_send(&self, frame: EncodedFrame) -> SendOutcome {
        let keyframe = frame.keyframe;
        if !keyframe && self.keyframe_needed() {
            self.state.dropped.fetch_add(1, Ordering::Relaxed);
            return SendOutcome::Dropped;
        }
        match self.inner.try_send(frame) {
            Ok(()) => {
                if keyframe {
                    self.state.keyframe_needed.store(false, Ordering::Relaxed);
                }
                SendOutcome::Sent
            }
            Err(TrySendError::Full(_)) => {
                self.state.dropped.fetch_add(1, Ordering::Relaxed);
                self.state.keyframe_needed.store(true, Ordering::Relaxed);
                SendOutcome::Dropped
            }
            Err(TrySendError::Disconnected(_)) => SendOutcome::Closed,
        }
    }

    /// Check it before encoding each frame and force a keyframe while it is set.
    pub fn keyframe_needed(&self) -> bool {
        self.state.keyframe_needed.load(Ordering::Relaxed)
    }

    pub fn dropped(&self) -> u64 {
        self.state.dropped.load(Ordering::Relaxed)
    }
}

pub struct FrameReceiver {
    inner: Receiver<EncodedFrame>,
    state: Arc<ChannelState>,
}

impl FrameReceiver {
    pub fn try_recv(&self) -> Result<EncodedFrame, TryRecvError> {
        self.inner.try_recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<EncodedFrame, RecvTimeoutError> {
        self.inner.recv_timeout(timeout)
    }

    pub fn dropped(&self) -> u64 {
        self.state.dropped.load(Ordering::Relaxed)
    }
}

pub fn frame_channel(capacity: usize) -> (FrameSender, FrameReceiver) {
    let (sender, receiver) = mpsc::sync_channel(capacity.max(1));
    let state = Arc::new(ChannelState {
        dropped: AtomicU64::new(0),
        keyframe_needed: AtomicBool::new(true),
    });
    (
        FrameSender {
            inner: sender,
            state: state.clone(),
        },
        FrameReceiver {
            inner: receiver,
            state,
        },
    )
}

/// Captures and encodes on its own thread. Each frame is one Annex-B access unit, Constrained
/// Baseline, SPS and PPS before every IDR, neither rotated nor mirrored.
pub trait VideoSource: Send {
    fn start(
        &mut self,
        config: VideoConfig,
        facing: Facing,
        out: FrameSender,
    ) -> Result<(), VideoError>;
    fn stop(&mut self) -> Result<(), VideoError>;
    fn request_keyframe(&mut self);
    /// Clamp with [`clamp_bitrate`] and apply it without restarting the encoder.
    fn set_bitrate(&mut self, bps: u32);
    /// The first frame from the new camera must be a keyframe.
    fn switch_camera(&mut self, facing: Facing) -> Result<(), VideoError>;
    /// True once the camera stopped for good (another app took it). The engine restarts the source.
    fn lost(&self) -> bool {
        false
    }
}

pub trait VideoSink: Send {
    /// Frames before the first keyframe are dropped.
    fn start(&mut self) -> Result<(), VideoError>;
    /// Must not block. The sink drops the frames it cannot keep up with.
    fn push(&mut self, frame: EncodedFrame) -> Result<(), VideoError>;
    fn stop(&mut self) -> Result<(), VideoError>;
    /// Polled after every push. While it is true the engine asks the other side for a keyframe.
    fn keyframe_needed(&mut self) -> bool {
        false
    }
}

/// Has no values, so on a build without a video platform both constructors return `Unsupported`.
#[derive(Debug)]
pub enum NoPlatform {}

impl VideoSource for NoPlatform {
    fn start(&mut self, _: VideoConfig, _: Facing, _: FrameSender) -> Result<(), VideoError> {
        match *self {}
    }
    fn stop(&mut self) -> Result<(), VideoError> {
        match *self {}
    }
    fn request_keyframe(&mut self) {
        match *self {}
    }
    fn set_bitrate(&mut self, _: u32) {
        match *self {}
    }
    fn switch_camera(&mut self, _: Facing) -> Result<(), VideoError> {
        match *self {}
    }
}

impl VideoSink for NoPlatform {
    fn start(&mut self) -> Result<(), VideoError> {
        match *self {}
    }
    fn push(&mut self, _: EncodedFrame) -> Result<(), VideoError> {
        match *self {}
    }
    fn stop(&mut self) -> Result<(), VideoError> {
        match *self {}
    }
}

#[cfg(target_os = "ios")]
pub type PlatformSource = ios::CameraSource;
#[cfg(target_os = "android")]
pub type PlatformSource = android::CameraSource;
#[cfg(all(
    feature = "desktop",
    not(any(target_os = "ios", target_os = "android"))
))]
pub type PlatformSource = desktop::CameraSource;
#[cfg(not(any(target_os = "ios", target_os = "android", feature = "desktop")))]
pub type PlatformSource = NoPlatform;

#[cfg(target_os = "ios")]
pub type PlatformSink = ios::DisplaySink;
#[cfg(target_os = "android")]
pub type PlatformSink = android::DisplaySink;
#[cfg(all(
    feature = "desktop",
    not(any(target_os = "ios", target_os = "android"))
))]
pub type PlatformSink = desktop::WindowSink;
#[cfg(not(any(target_os = "ios", target_os = "android", feature = "desktop")))]
pub type PlatformSink = NoPlatform;

pub fn platform_source() -> Result<PlatformSource, VideoError> {
    #[cfg(target_os = "ios")]
    {
        ios::CameraSource::new()
    }
    #[cfg(target_os = "android")]
    {
        Ok(android::CameraSource::new())
    }
    #[cfg(all(
        feature = "desktop",
        not(any(target_os = "ios", target_os = "android"))
    ))]
    {
        Ok(desktop::CameraSource::new())
    }
    #[cfg(not(any(target_os = "ios", target_os = "android", feature = "desktop")))]
    {
        Err(VideoError::Unsupported)
    }
}

pub fn platform_sink() -> Result<PlatformSink, VideoError> {
    #[cfg(target_os = "ios")]
    {
        ios::DisplaySink::new()
    }
    #[cfg(target_os = "android")]
    {
        Ok(android::DisplaySink::new())
    }
    #[cfg(all(
        feature = "desktop",
        not(any(target_os = "ios", target_os = "android"))
    ))]
    {
        Ok(desktop::WindowSink::new(desktop::FrameSlot::new()))
    }
    #[cfg(not(any(target_os = "ios", target_os = "android", feature = "desktop")))]
    {
        Err(VideoError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(keyframe: bool, millis: u64) -> EncodedFrame {
        EncodedFrame {
            data: vec![0, 0, 0, 1, if keyframe { 0x65 } else { 0x41 }],
            keyframe,
            timestamp: Duration::from_millis(millis),
            rotation: Rotation::Deg0,
        }
    }

    #[test]
    fn the_default_config_is_vga_at_30_fps_and_800_kbits() {
        assert_eq!(
            VideoConfig::default(),
            VideoConfig {
                width: 640,
                height: 480,
                fps: 30,
                bitrate_bps: 800_000,
            }
        );
        assert!((MIN_BITRATE_BPS..=MAX_BITRATE_BPS).contains(&VideoConfig::default().bitrate_bps));
    }

    #[test]
    fn bitrates_are_clamped_to_the_supported_range() {
        assert_eq!(clamp_bitrate(0), MIN_BITRATE_BPS);
        assert_eq!(clamp_bitrate(500_000), 500_000);
        assert_eq!(clamp_bitrate(u32::MAX), MAX_BITRATE_BPS);
    }

    #[test]
    fn rotations_map_to_and_from_the_cvo_byte() {
        let all = [
            (Rotation::Deg0, 0, 0),
            (Rotation::Deg90, 1, 90),
            (Rotation::Deg180, 2, 180),
            (Rotation::Deg270, 3, 270),
        ];
        for (rotation, byte, degrees) in all {
            assert_eq!(rotation.to_cvo(), byte);
            assert_eq!(Rotation::from_cvo(byte), rotation);
            assert_eq!(rotation.degrees(), degrees);
        }
        assert_eq!(Rotation::from_cvo(0b0000_1101), Rotation::Deg90);
    }

    #[test]
    fn the_first_frame_must_be_a_keyframe() {
        let (sender, receiver) = frame_channel(2);
        assert!(sender.keyframe_needed());

        assert_eq!(sender.try_send(frame(false, 0)), SendOutcome::Dropped);
        assert_eq!(sender.try_send(frame(true, 33)), SendOutcome::Sent);
        assert!(!sender.keyframe_needed());
        assert_eq!(receiver.try_recv(), Ok(frame(true, 33)));
        assert_eq!(receiver.dropped(), 1);
    }

    #[test]
    fn a_full_channel_drops_the_frame_and_asks_for_a_keyframe() {
        let (sender, receiver) = frame_channel(2);
        assert_eq!(sender.try_send(frame(true, 0)), SendOutcome::Sent);
        assert_eq!(sender.try_send(frame(false, 33)), SendOutcome::Sent);
        assert_eq!(sender.try_send(frame(false, 66)), SendOutcome::Dropped);
        assert!(sender.keyframe_needed());
        assert_eq!(sender.dropped(), 1);

        assert_eq!(receiver.try_recv(), Ok(frame(true, 0)));
        assert_eq!(sender.try_send(frame(false, 100)), SendOutcome::Dropped);
        assert_eq!(sender.dropped(), 2);

        assert_eq!(sender.try_send(frame(true, 133)), SendOutcome::Sent);
        assert!(!sender.keyframe_needed());
        assert_eq!(receiver.try_recv(), Ok(frame(false, 33)));
        assert_eq!(receiver.try_recv(), Ok(frame(true, 133)));
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn a_keyframe_that_does_not_fit_leaves_the_keyframe_owed() {
        let (sender, _receiver) = frame_channel(1);
        assert_eq!(sender.try_send(frame(true, 0)), SendOutcome::Sent);
        assert_eq!(sender.try_send(frame(true, 33)), SendOutcome::Dropped);
        assert!(sender.keyframe_needed());
    }

    #[test]
    fn a_sender_knows_when_the_engine_is_gone() {
        let (sender, receiver) = frame_channel(2);
        drop(receiver);
        assert_eq!(sender.try_send(frame(true, 0)), SendOutcome::Closed);
    }

    struct Minimal;

    impl VideoSource for Minimal {
        fn start(&mut self, _: VideoConfig, _: Facing, _: FrameSender) -> Result<(), VideoError> {
            Ok(())
        }
        fn stop(&mut self) -> Result<(), VideoError> {
            Ok(())
        }
        fn request_keyframe(&mut self) {}
        fn set_bitrate(&mut self, _: u32) {}
        fn switch_camera(&mut self, _: Facing) -> Result<(), VideoError> {
            Ok(())
        }
    }

    impl VideoSink for Minimal {
        fn start(&mut self) -> Result<(), VideoError> {
            Ok(())
        }
        fn push(&mut self, _: EncodedFrame) -> Result<(), VideoError> {
            Ok(())
        }
        fn stop(&mut self) -> Result<(), VideoError> {
            Ok(())
        }
    }

    #[test]
    fn a_source_that_cannot_tell_is_never_lost() {
        let source: Box<dyn VideoSource> = Box::new(Minimal);
        assert!(!source.lost());
    }

    #[test]
    fn a_sink_that_cannot_tell_never_asks_for_a_keyframe_by_polling() {
        let mut sink: Box<dyn VideoSink> = Box::new(Minimal);
        assert!(!sink.keyframe_needed());
    }

    #[cfg(not(any(target_os = "ios", target_os = "android", feature = "desktop")))]
    #[test]
    fn without_a_video_platform_there_is_no_camera_nor_display() {
        assert!(matches!(platform_source(), Err(VideoError::Unsupported)));
        assert!(matches!(platform_sink(), Err(VideoError::Unsupported)));
    }

    #[cfg(all(
        feature = "desktop",
        not(any(target_os = "ios", target_os = "android"))
    ))]
    #[test]
    fn the_desktop_feature_gives_the_desktop_camera_and_window_stopped() {
        let mut source: desktop::CameraSource = platform_source().unwrap();
        assert!(source.preview().latest().is_none());
        assert_eq!(source.stop(), Ok(()));
        let sink: desktop::WindowSink = platform_sink().unwrap();
        let slot = sink.slot();
        let mut boxed: Box<dyn VideoSink> = Box::new(sink);
        assert_eq!(boxed.stop(), Ok(()));
        assert!(slot.latest().is_none());
    }

    #[test]
    fn errors_say_what_went_wrong() {
        assert_eq!(VideoError::NoCamera.to_string(), "no camera");
        assert_eq!(
            VideoError::PermissionDenied.to_string(),
            "camera permission denied"
        );
        assert_eq!(
            VideoError::Unsupported.to_string(),
            "video mode not supported"
        );
        assert_eq!(
            VideoError::Backend("encoder failed".to_owned()).to_string(),
            "video backend: encoder failed"
        );
    }
}
