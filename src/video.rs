//! Video: the contract between the platform camera and display code and the engine.
//!
//! The pixels never enter the common core. Each platform captures **and** encodes in one place,
//! with its hardware encoder, and decodes **and** renders in another, so frames stay in native
//! buffers (zero copy, less battery). The engine only moves encoded H.264 access units:
//!
//! ```text
//! camera + hardware encoder (VideoSource) -> FrameSender -> FrameReceiver -> engine -> RTP
//! RTP -> engine -> VideoSink::push -> hardware decoder + display (VideoSink)
//! ```
//!
//! # Rules for every source
//!
//! - **H.264 Constrained Baseline** (`profile-level-id=42e01f`, `packetization-mode=1`), the
//!   profile every browser and hardware decoder takes.
//! - One [`EncodedFrame`] is one whole **access unit** in **Annex-B** (NAL units behind
//!   `00 00 00 01` or `00 00 01` start codes). AVCC (length-prefixed) output, as VideoToolbox and
//!   MediaCodec give it, is converted by the source before it sends the frame.
//! - **Every keyframe carries its SPS and PPS in-band**, ahead of the IDR slice, so a decoder can
//!   start from any keyframe (a late joiner, a lost packet, a camera switch that changes size).
//! - Frames are **not rotated**: the source encodes what the sensor gives and reports the
//!   orientation in [`EncodedFrame::rotation`] (see [`Rotation`]).
//! - Frames are **not mirrored**, not even from the front camera: mirroring is for the local
//!   preview only, which the platform draws itself.
//! - Before encoding each frame the source checks [`FrameSender::keyframe_needed`] and, if set,
//!   forces a keyframe (the same as [`VideoSource::request_keyframe`]).
//!
//! # The engine side
//!
//! [`call::VideoCall`] runs a source and a sink against the network: RTP packetisation
//! ([`packet`]), reassembly ([`assemble`]), keyframe requests and bitrate control ([`bitrate`]),
//! over webrtc-rs tracks ([`rtp`]) or the simulated link of [`crate::netsim`]. Besides the frames,
//! it polls two things of the platform code, both default methods that a platform may leave out:
//!
//! - [`VideoSink::keyframe_needed`]: the decoder cannot go on without a keyframe. The call sends
//!   the other side a PLI, at most once every
//!   [`KEYFRAME_REQUEST_INTERVAL`](assemble::KEYFRAME_REQUEST_INTERVAL).
//! - [`VideoSource::lost`]: the camera stopped for good (another app took it). The call starts
//!   the source again, at most once every
//!   [`CAMERA_RESTART_INTERVAL`](call::CAMERA_RESTART_INTERVAL), and says so in its stats
//!   (`camera_lost`, `camera_restarts`).
//!
//! # Platform attach points
//!
//! The app's UI code only ever sees native view objects; it never gets pixels.
//! [`platform_source`] and [`platform_sink`] give the camera and the display of the platform
//! the crate is built for, as their concrete types ([`PlatformSource`], [`PlatformSink`]), so the
//! app can reach their views before boxing them for a [`call::VideoCall`].
//!
//! - **iOS** — `video::ios::{CameraSource, DisplaySink}` (AVFoundation + VideoToolbox).
//!   `CameraSource` exposes its `AVCaptureVideoPreviewLayer` (local preview) and `DisplaySink`
//!   its `AVSampleBufferDisplayLayer` (remote video) as a raw `*mut c_void` to a `CALayer`. The
//!   pointer stays owned by the Rust object and is valid until that object is dropped; the app's
//!   Swift code adds it as a sublayer of its view (on the main thread) and removes it before
//!   dropping the object. Rotation is applied as the layer's transform.
//! - **Android** — `video::android::{CameraSource, DisplaySink}` (Camera2 + MediaCodec, loaded
//!   with `dlopen`; capture needs Android 8.0). The app's Kotlin code hands a `Surface` over JNI,
//!   turned into an `ANativeWindow` with `ANativeWindow_fromSurface`: `DisplaySink::set_surface`
//!   renders the remote video there (the decoder outputs straight to it) and
//!   `CameraSource::set_preview_surface` shows the local preview. Passing `None` detaches the
//!   window: the app must do it in `surfaceDestroyed`. Frames that arrive without a surface are
//!   decoded and dropped, so the decoder keeps its references.
//! - **Desktop** — `video::desktop::{CameraSource, WindowSink, VideoWindow}` (feature
//!   `desktop`): the camera through nokhwa, encoded with OpenH264, and a minifb window, for
//!   trying calls on a computer (`examples/video_demo.rs`).
//! - **Software** — `video::openh264` (feature `openh264`): an H.264 encoder and decoder
//!   (OpenH264, built from source) for tests and the desktop.
//!
//! The platform code shares [`h264`] (start code, NAL unit types, Annex-B splitting, keyframe
//! detection); what only one platform needs stays with it (AVCC for CoreMedia on iOS, AVCC and
//! the SPS reader for MediaCodec on Android).

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

// The pure parts (Annex-B, rotation, formats) are tested on the host; the camera and the codecs
// are Android only.
#[cfg(any(target_os = "android", test))]
pub mod android;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::time::Duration;

/// One encoded H.264 access unit, as a [`VideoSource`] sends it and a [`VideoSink`] takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// One access unit in Annex-B. A keyframe starts with its SPS and PPS.
    pub data: Vec<u8>,
    /// An IDR frame: decodable on its own.
    pub keyframe: bool,
    /// Capture time on a monotonic clock. Only differences matter: the RTP timestamp (90 kHz) is
    /// derived from them, and a sink uses them to pace the display.
    pub timestamp: Duration,
    /// How the receiver must rotate the frame to show it upright.
    pub rotation: Rotation,
}

/// What a [`VideoSource`] is asked to capture and encode. The camera picks its closest mode; the
/// real size travels in the SPS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoConfig {
    /// Width in sensor orientation (landscape), before any [`Rotation`].
    pub width: u32,
    /// Height in sensor orientation.
    pub height: u32,
    /// Frames per second.
    pub fps: u32,
    /// Target encoder bitrate, within [`MIN_BITRATE_BPS`]..=[`MAX_BITRATE_BPS`].
    pub bitrate_bps: u32,
}

/// The lowest bitrate a source is set to: below it 320×240 video breaks into blocks.
pub const MIN_BITRATE_BPS: u32 = 150_000;
/// The highest bitrate a source is set to: enough for 720p at 30 fps.
pub const MAX_BITRATE_BPS: u32 = 2_500_000;

/// Keeps a bitrate inside [`MIN_BITRATE_BPS`]..=[`MAX_BITRATE_BPS`]. Sources apply it in
/// [`VideoSource::set_bitrate`].
pub fn clamp_bitrate(bps: u32) -> u32 {
    bps.clamp(MIN_BITRATE_BPS, MAX_BITRATE_BPS)
}

impl Default for VideoConfig {
    /// 640×480 at 30 fps and 800 kbit/s: VGA, fine on a phone screen and on a mobile uplink.
    fn default() -> Self {
        Self {
            width: 640,
            height: 480,
            fps: 30,
            bitrate_bps: 800_000,
        }
    }
}

/// How much the receiver rotates a frame, clockwise, to show it upright.
///
/// **Contract**: the source reports it per frame and leaves the pixels as the sensor gave them;
/// on the network it travels in the RTP header extension *Coordination of Video Orientation*
/// ([`CVO_URI`], 3GPP TS 26.114), as browsers send and read it; the sink applies it when it
/// renders (layer or surface transform). Rotating the pixels would cost an extra pass on every
/// frame and a new keyframe on every turn of the phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    Deg0,
    Deg90,
    Deg180,
    Deg270,
}

/// The URI of the CVO RTP header extension, which carries a [`Rotation`].
pub const CVO_URI: &str = "urn:3gpp:video-orientation";

impl Rotation {
    /// The CVO byte for this rotation: `0 0 0 0 C F R1 R0`, with camera (C) and flip (F) left at
    /// zero, as libwebrtc sends it.
    pub fn to_cvo(self) -> u8 {
        match self {
            Self::Deg0 => 0,
            Self::Deg90 => 1,
            Self::Deg180 => 2,
            Self::Deg270 => 3,
        }
    }

    /// The rotation in a CVO byte; the camera and flip bits are ignored.
    pub fn from_cvo(byte: u8) -> Self {
        match byte & 0b11 {
            0 => Self::Deg0,
            1 => Self::Deg90,
            2 => Self::Deg180,
            _ => Self::Deg270,
        }
    }

    /// The rotation in degrees, clockwise.
    pub fn degrees(self) -> u16 {
        u16::from(self.to_cvo()) * 90
    }
}

/// Which camera a source captures from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Facing {
    /// The selfie camera: the default for a video call.
    #[default]
    Front,
    Back,
}

/// Why video could not be set up or went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoError {
    /// The device has no camera, or none facing the way asked for.
    NoCamera,
    /// The user has not allowed the camera.
    PermissionDenied,
    /// The platform cannot do what was asked (a size, a frame rate, H.264 in hardware).
    Unsupported,
    /// The platform video API failed.
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

/// Frames a source's channel holds by default: about 100 ms at 30 fps. More is only delay.
pub const FRAME_CHANNEL_CAPACITY: usize = 3;

/// What happened to a frame given to [`FrameSender::try_send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum SendOutcome {
    /// Queued for the engine.
    Sent,
    /// Dropped: the channel was full, or it was a delta frame while a keyframe is owed.
    Dropped,
    /// The engine side is gone: the source should stop.
    Closed,
}

struct ChannelState {
    dropped: AtomicU64,
    keyframe_needed: AtomicBool,
}

/// The source's end of a frame channel: bounded and never blocking, for the camera's thread.
///
/// A dropped frame breaks the chain of references the following delta frames decode against,
/// so after any drop the channel owes a keyframe: [`FrameSender::keyframe_needed`] turns on,
/// delta frames are dropped too, and the first keyframe that gets through turns it off. It starts
/// on, so the engine's first frame is a keyframe.
#[derive(Clone)]
pub struct FrameSender {
    inner: SyncSender<EncodedFrame>,
    state: Arc<ChannelState>,
}

impl FrameSender {
    /// Queues `frame` without blocking. See [`SendOutcome`].
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

    /// Whether the source must make its next frame a keyframe.
    pub fn keyframe_needed(&self) -> bool {
        self.state.keyframe_needed.load(Ordering::Relaxed)
    }

    /// Frames dropped so far.
    pub fn dropped(&self) -> u64 {
        self.state.dropped.load(Ordering::Relaxed)
    }
}

/// The engine's end of a frame channel.
pub struct FrameReceiver {
    inner: Receiver<EncodedFrame>,
    state: Arc<ChannelState>,
}

impl FrameReceiver {
    /// The next frame, if one is waiting. `Disconnected` once every sender is gone.
    pub fn try_recv(&self) -> Result<EncodedFrame, TryRecvError> {
        self.inner.try_recv()
    }

    /// The next frame, waiting at most `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<EncodedFrame, RecvTimeoutError> {
        self.inner.recv_timeout(timeout)
    }

    /// Frames the sender has dropped so far.
    pub fn dropped(&self) -> u64 {
        self.state.dropped.load(Ordering::Relaxed)
    }
}

/// A frame channel holding at most `capacity` frames (at least one).
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

/// A platform camera with its hardware H.264 encoder.
///
/// It captures and encodes on its own thread and pushes each [`EncodedFrame`] into the
/// [`FrameSender`] it was started with, following the rules in the [module docs](self). The
/// methods are called from the engine, never from inside a camera callback.
pub trait VideoSource: Send {
    /// Opens the camera facing `facing` and starts sending frames to `out`.
    fn start(
        &mut self,
        config: VideoConfig,
        facing: Facing,
        out: FrameSender,
    ) -> Result<(), VideoError>;
    /// Stops the camera and the encoder. Stopping a source that is not running does nothing.
    fn stop(&mut self) -> Result<(), VideoError>;
    /// Makes the next encoded frame a keyframe (the other side lost one, or asked with a PLI).
    fn request_keyframe(&mut self);
    /// Changes the encoder's target bitrate, clamped with [`clamp_bitrate`], without restarting.
    fn set_bitrate(&mut self, bps: u32);
    /// Moves to the other camera while running; the first frame from it is a keyframe.
    fn switch_camera(&mut self, facing: Facing) -> Result<(), VideoError>;
    /// Whether the camera stopped for good while running (another app took it, the device or
    /// the encoder failed): no more frames will come until the source is started again. The
    /// engine checks it now and then and restarts the source. A source that cannot tell keeps
    /// the default, `false`.
    fn lost(&self) -> bool {
        false
    }
}

/// A platform hardware H.264 decoder with its display.
pub trait VideoSink: Send {
    /// Sets up the decoder. Frames before the first keyframe are dropped.
    fn start(&mut self) -> Result<(), VideoError>;
    /// Hands over one frame. It must not block for long: the sink decodes and renders on its
    /// own, and drops frames it cannot keep up with.
    fn push(&mut self, frame: EncodedFrame) -> Result<(), VideoError>;
    /// Stops decoding and clears the display. Stopping a sink that is not running does nothing.
    fn stop(&mut self) -> Result<(), VideoError>;
    /// Whether the decoder needs a keyframe to go on (none yet, a lost frame, a decoder reset).
    /// The engine polls it after every [`VideoSink::push`] and now and then without frames, and
    /// asks the other side for a keyframe (a PLI, rate limited) while it says `true`. It may be
    /// a level (true until a keyframe is decoded) or an event (cleared by the call). A sink that
    /// reports this through [`VideoSink::push`] errors only keeps the default, `false`.
    fn keyframe_needed(&mut self) -> bool {
        false
    }
}

/// Neither a camera nor a display: what [`PlatformSource`] and [`PlatformSink`] are on a build
/// with no video platform. It has no values, so [`platform_source`] and [`platform_sink`] can
/// only return [`VideoError::Unsupported`] there.
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

/// The camera of the platform this is built for: `ios::CameraSource` on iOS,
/// `android::CameraSource` on Android, `desktop::CameraSource` elsewhere with the `desktop`
/// feature, [`NoPlatform`] without any of them.
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

/// The display of the platform this is built for: `ios::DisplaySink` on iOS,
/// `android::DisplaySink` on Android, `desktop::WindowSink` elsewhere with the `desktop`
/// feature, [`NoPlatform`] without any of them.
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

/// The platform's camera, created stopped, as [`crate::audio::platform_backend`] gives the
/// audio device. The concrete type, so the app can reach what only it has (the preview layer on
/// iOS, the preview surface on Android) before boxing it for a
/// [`VideoCall`](call::VideoCall). [`VideoError::Unsupported`] where there is no video platform.
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

/// The platform's display, created stopped. The concrete type, so the app can reach its view
/// (the layer on iOS, `set_surface` on Android, the picture slot on the desktop).
/// [`VideoError::Unsupported`] where there is no video platform.
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
        // Back camera and flip bits set: the rotation is still read.
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

        // Room again, but a delta frame would reference the dropped one.
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

    /// A source and a sink that implement only what the contract requires.
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
        // Nothing opens a camera or a window until `start` or `VideoWindow::open`.
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
