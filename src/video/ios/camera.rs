//! The camera: AVFoundation capture into the VideoToolbox encoder.
//!
//! An `AVCaptureSession` with the built-in wide-angle camera facing the way asked for and an
//! `AVCaptureVideoDataOutput` that hands NV12 `CVPixelBuffer`s to a delegate on a serial dispatch
//! queue. The delegate (an Objective-C class defined with objc2) paces the frames to the
//! configured rate, stamps each with the [`Rotation`] for the phone's orientation and the camera,
//! and queues it on the [`H264Encoder`], which is made at the first frame for the camera's real
//! frame size (and made again if a camera switch changes it). Late frames are discarded by
//! AVFoundation, and nothing in the callback blocks: the capture state sits behind a mutex that
//! the callback only `try_lock`s, and the engine thread takes it only after the queue has been
//! drained.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, NSObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};

use super::encoder::{EncoderOutput, H264Encoder};
use super::ffi::{
    AVCaptureDeviceTypeBuiltInWideAngleCamera, AVCaptureSessionPreset352x288,
    AVCaptureSessionPreset640x480, AVCaptureSessionPreset1280x720, AVCaptureSessionPreset1920x1080,
    AVLayerVideoGravityResizeAspectFill, AVMediaTypeVideo, CMSampleBufferGetImageBuffer,
    CMSampleBufferGetPresentationTimeStamp, CMSampleBufferRef, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, K_CVPIXEL_FORMAT_NV12, Owned, cf_dictionary, cf_i32, class,
    dispatch_queue_create, dispatch_release, dispatch_sync_f, kCVPixelBufferPixelFormatTypeKey,
};
use super::orientation::{DeviceOrientation, capture_rotation};
use super::settings::{
    FramePacer, SessionPreset, check_authorization, cmtime_to_duration, session_preset,
};
use crate::video::{
    Facing, FrameSender, Rotation, VideoConfig, VideoError, VideoSource, clamp_bitrate,
};

/// `AVCaptureDevicePosition`.
const POSITION_BACK: isize = 1;
const POSITION_FRONT: isize = 2;
/// `AVErrorApplicationIsNotAuthorizedToUseDevice`.
const NOT_AUTHORIZED_ERROR: isize = -11852;

/// What the engine and the app change while the camera runs, read by the capture callback.
#[derive(Debug)]
struct Controls {
    orientation: AtomicIsize,
    back: AtomicBool,
    force_keyframe: AtomicBool,
    bitrate: AtomicU32,
    bitrate_changed: AtomicBool,
}

impl Controls {
    fn facing(&self) -> Facing {
        if self.back.load(Ordering::Relaxed) {
            Facing::Back
        } else {
            Facing::Front
        }
    }
}

/// The capture callback's state for one run of the camera.
struct Capture {
    controls: Arc<Controls>,
    output: Arc<EncoderOutput>,
    encoder: Option<H264Encoder>,
    pacer: FramePacer,
    fps: u32,
    rotation: Rotation,
}

impl Capture {
    /// Encodes one camera frame, if it is due.
    ///
    /// # Safety
    ///
    /// `sample` is a live `CMSampleBuffer` from the capture output.
    unsafe fn on_sample(&mut self, sample: CMSampleBufferRef) {
        if self.output.closed() || sample.is_null() {
            return;
        }
        // SAFETY: a live sample buffer; the image buffer is borrowed from it for this call.
        let (image, pts) = unsafe {
            (
                CMSampleBufferGetImageBuffer(sample),
                CMSampleBufferGetPresentationTimeStamp(sample),
            )
        };
        if image.is_null() || !pts.is_valid() {
            return;
        }
        let Some(timestamp) = cmtime_to_duration(pts.value, pts.timescale) else {
            return;
        };
        if !self.pacer.admit(timestamp) {
            return;
        }
        // SAFETY: a live pixel buffer.
        let size = unsafe { (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image)) };
        let controls = &self.controls;
        let mut new_encoder = false;
        if self.encoder.as_ref().map(H264Encoder::size) != Some(size) {
            self.encoder = None;
            let bitrate = controls.bitrate.load(Ordering::Relaxed);
            match H264Encoder::new(size.0, size.1, self.fps, bitrate, self.output.clone()) {
                Ok(encoder) => self.encoder = Some(encoder),
                Err(_) => {
                    self.output.count_error();
                    return;
                }
            }
            controls.bitrate_changed.store(false, Ordering::Relaxed);
            new_encoder = true;
        }
        let Some(encoder) = self.encoder.as_mut() else {
            return;
        };
        if controls.bitrate_changed.swap(false, Ordering::Relaxed) {
            // A failure leaves the previous bitrate; the error is not worth stopping for.
            let _ = encoder.set_bitrate(controls.bitrate.load(Ordering::Relaxed));
        }
        let orientation = DeviceOrientation::from_raw(controls.orientation.load(Ordering::Relaxed));
        if let Some(rotation) = capture_rotation(orientation, controls.facing()) {
            self.rotation = rotation;
        }
        let keyframe = controls.force_keyframe.swap(false, Ordering::Relaxed)
            || self.output.keyframe_needed()
            || new_encoder;
        // SAFETY: a live pixel buffer of the encoder's size (checked above). A failure is
        // counted by the encoder.
        let _ = unsafe { encoder.encode(image, pts, self.rotation, keyframe) };
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `CaptureDelegate` has no `Drop`.
    #[unsafe(super(NSObject))]
    #[ivars = Mutex<Option<Capture>>]
    struct CaptureDelegate;

    impl CaptureDelegate {
        /// `-[AVCaptureVideoDataOutputSampleBufferDelegate captureOutput:didOutputSampleBuffer:fromConnection:]`,
        /// on the capture queue.
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn capture_output(
            &self,
            _output: *mut AnyObject,
            sample: CMSampleBufferRef,
            _connection: *mut AnyObject,
        ) {
            // Never wait: the engine thread holds the lock only while the camera is stopped.
            let Ok(mut capture) = self.ivars().try_lock() else {
                return;
            };
            if let Some(capture) = capture.as_mut() {
                // SAFETY: AVFoundation hands a live sample buffer for the call.
                unsafe { capture.on_sample(sample) };
            }
        }
    }
);

impl CaptureDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(Mutex::new(None));
        // SAFETY: NSObject's designated initialiser.
        unsafe { msg_send![super(this), init] }
    }
}

/// The serial dispatch queue the capture callbacks run on.
struct Queue(*mut AnyObject);

impl Queue {
    fn new() -> Result<Self, VideoError> {
        // SAFETY: a NUL-terminated label; a null attribute makes a serial queue.
        let queue =
            unsafe { dispatch_queue_create(c"webrtc-engine.camera".as_ptr(), std::ptr::null()) };
        if queue.is_null() {
            return Err(VideoError::Backend(
                "dispatch_queue_create failed".to_owned(),
            ));
        }
        Ok(Self(queue))
    }

    /// Waits until every callback already queued has run.
    fn drain(&self) {
        unsafe extern "C" fn nothing(_: *mut c_void) {}
        // SAFETY: a live queue; the work function takes no context. Never called from the
        // queue itself (that would deadlock): only from the engine's thread.
        unsafe { dispatch_sync_f(self.0, std::ptr::null_mut(), nothing) };
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        // SAFETY: the queue was created by `Queue::new` and is released once.
        unsafe { dispatch_release(self.0) };
    }
}

// SAFETY: dispatch queues are thread-safe objects.
unsafe impl Send for Queue {}

/// The local camera with its hardware H.264 encoder.
///
/// See the [module docs](super) for how the app attaches the preview and what it must do about
/// the camera permission and the device orientation.
pub struct CameraSource {
    session: Owned,
    preview: Owned,
    delegate: Retained<CaptureDelegate>,
    queue: Queue,
    controls: Arc<Controls>,
    input: Option<Owned>,
    output: Option<Owned>,
    running: bool,
}

impl CameraSource {
    /// A source with its capture session and preview layer, not started.
    pub fn new() -> Result<Self, VideoError> {
        autoreleasepool(|_| {
            let session_class = class(c"AVCaptureSession")?;
            let preview_class = class(c"AVCaptureVideoPreviewLayer")?;
            // SAFETY: `+new` returns a new, owned session.
            let session: Retained<AnyObject> = unsafe { msg_send![session_class, new] };
            // SAFETY: `+layerWithSession:` takes a session; `videoGravity` one of the
            // framework's gravity strings.
            let preview: Option<Retained<AnyObject>> =
                unsafe { msg_send![preview_class, layerWithSession: &*session] };
            let preview = preview.ok_or_else(|| {
                VideoError::Backend("AVCaptureVideoPreviewLayer failed".to_owned())
            })?;
            // SAFETY: as above.
            unsafe {
                let gravity = AVLayerVideoGravityResizeAspectFill;
                let _: () = msg_send![&*preview, setVideoGravity: gravity];
            }
            Ok(Self {
                session: Owned::new(session),
                preview: Owned::new(preview),
                delegate: CaptureDelegate::new(),
                queue: Queue::new()?,
                controls: Arc::new(Controls {
                    orientation: AtomicIsize::new(DeviceOrientation::Portrait as isize),
                    back: AtomicBool::new(false),
                    force_keyframe: AtomicBool::new(false),
                    bitrate: AtomicU32::new(VideoConfig::default().bitrate_bps),
                    bitrate_changed: AtomicBool::new(false),
                }),
                input: None,
                output: None,
                running: false,
            })
        })
    }

    /// The local preview, an `AVCaptureVideoPreviewLayer`, for the app's Swift code to add to its
    /// view. Owned by the source and valid until it is dropped. It shows the front camera
    /// mirrored (AVFoundation's default); the frames sent are not.
    pub fn preview_layer(&self) -> *mut c_void {
        self.preview.as_raw()
    }

    /// The phone's orientation, from the app (see [`DeviceOrientation`]). Portrait until told.
    pub fn set_device_orientation(&self, orientation: DeviceOrientation) {
        self.controls
            .orientation
            .store(orientation as isize, Ordering::Relaxed);
    }

    /// Frames the encoder failed or dropped so far in this run.
    pub fn encode_errors(&self) -> u64 {
        self.delegate
            .ivars()
            .lock()
            .ok()
            .and_then(|capture| capture.as_ref().map(|capture| capture.output.errors()))
            .unwrap_or(0)
    }

    /// Hands captured frames to a new capture state encoding into `out`.
    fn arm(
        &mut self,
        config: VideoConfig,
        facing: Facing,
        out: FrameSender,
    ) -> Result<(), VideoError> {
        let controls = &self.controls;
        controls
            .back
            .store(facing == Facing::Back, Ordering::Relaxed);
        controls
            .bitrate
            .store(clamp_bitrate(config.bitrate_bps), Ordering::Relaxed);
        controls.bitrate_changed.store(false, Ordering::Relaxed);
        controls.force_keyframe.store(false, Ordering::Relaxed);
        let capture = Capture {
            controls: controls.clone(),
            output: EncoderOutput::new(out),
            encoder: None,
            pacer: FramePacer::new(config.fps),
            fps: config.fps.max(1),
            rotation: Rotation::Deg0,
        };
        let mut state = self
            .delegate
            .ivars()
            .lock()
            .map_err(|_| VideoError::Backend("capture state poisoned".to_owned()))?;
        *state = Some(capture);
        Ok(())
    }

    /// Drops the capture state, after its encoder has delivered every frame. Only once no
    /// callback can run (the output has no delegate, or the queue has been drained).
    fn disarm(&mut self) {
        let capture = match self.delegate.ivars().lock() {
            Ok(mut state) => state.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        drop(capture);
    }

    /// The built-in wide-angle camera facing `facing`, as a capture input.
    fn camera_input(facing: Facing) -> Result<Owned, VideoError> {
        let device_class = class(c"AVCaptureDevice")?;
        let input_class = class(c"AVCaptureDeviceInput")?;
        let position = match facing {
            Facing::Front => POSITION_FRONT,
            Facing::Back => POSITION_BACK,
        };
        // SAFETY: the framework's device type and media type strings and an
        // `AVCaptureDevicePosition`.
        let device: Option<Retained<AnyObject>> = unsafe {
            msg_send![
                device_class,
                defaultDeviceWithDeviceType: AVCaptureDeviceTypeBuiltInWideAngleCamera,
                mediaType: AVMediaTypeVideo,
                position: position
            ]
        };
        let device = device.ok_or(VideoError::NoCamera)?;
        let mut error: *mut AnyObject = std::ptr::null_mut();
        // SAFETY: a device and a place for an autoreleased `NSError`.
        let input: Option<Retained<AnyObject>> =
            unsafe { msg_send![input_class, deviceInputWithDevice: &*device, error: &mut error] };
        match input {
            Some(input) => Ok(Owned::new(input)),
            None if error.is_null() => Err(VideoError::Backend("camera input failed".to_owned())),
            None => {
                // SAFETY: a live `NSError` (autoreleased, inside the caller's pool).
                let code: isize = unsafe { msg_send![&*error, code] };
                if code == NOT_AUTHORIZED_ERROR {
                    Err(VideoError::PermissionDenied)
                } else {
                    Err(VideoError::Backend(format!("camera input failed: {code}")))
                }
            }
        }
    }

    /// The camera's `AVCaptureVideoDataOutput`, delivering NV12 to the delegate on the queue.
    fn video_output(&self) -> Result<Owned, VideoError> {
        let output_class = class(c"AVCaptureVideoDataOutput")?;
        // SAFETY: `+new` returns a new, owned output.
        let output: Retained<AnyObject> = unsafe { msg_send![output_class, new] };
        // SAFETY: reading a constant CFString.
        let format_key = unsafe { kCVPixelBufferPixelFormatTypeKey };
        let format = cf_i32(K_CVPIXEL_FORMAT_NV12 as i32)?;
        let settings = cf_dictionary(&[(format_key, format.as_ptr().cast_const())])?;
        let settings = settings.as_ptr().cast_const().cast::<AnyObject>();
        let delegate: &AnyObject = &self.delegate;
        // SAFETY: `videoSettings` takes an NSDictionary (a CFDictionary is toll-free bridged),
        // the delegate implements the delegate method, and the queue is serial and live.
        unsafe {
            let _: () = msg_send![&*output, setVideoSettings: settings];
            let _: () = msg_send![&*output, setAlwaysDiscardsLateVideoFrames: true];
            let _: () = msg_send![&*output, setSampleBufferDelegate: delegate, queue: self.queue.0];
        }
        Ok(Owned::new(output))
    }

    fn configure(&mut self, config: VideoConfig, facing: Facing) -> Result<(), VideoError> {
        let device_class = class(c"AVCaptureDevice")?;
        // SAFETY: the framework's media type string.
        let status: isize =
            unsafe { msg_send![device_class, authorizationStatusForMediaType: AVMediaTypeVideo] };
        check_authorization(status)?;
        let input = Self::camera_input(facing)?;
        let output = self.video_output()?;
        let session = &*self.session;
        // SAFETY: the framework's preset strings.
        let preset = unsafe {
            match session_preset(config.width, config.height) {
                SessionPreset::Cif352x288 => AVCaptureSessionPreset352x288,
                SessionPreset::Vga640x480 => AVCaptureSessionPreset640x480,
                SessionPreset::Hd1280x720 => AVCaptureSessionPreset1280x720,
                SessionPreset::Hd1920x1080 => AVCaptureSessionPreset1920x1080,
            }
        };
        // SAFETY: session configuration inside begin/commit, each object added only after the
        // session said it can take it (adding one it cannot take would raise).
        let added = unsafe {
            let _: () = msg_send![session, beginConfiguration];
            let can_preset: bool = msg_send![session, canSetSessionPreset: preset];
            if can_preset {
                let _: () = msg_send![session, setSessionPreset: preset];
            }
            let can_input: bool = msg_send![session, canAddInput: &*input];
            let can_output: bool = can_input && {
                let _: () = msg_send![session, addInput: &*input];
                msg_send![session, canAddOutput: &*output]
            };
            if can_output {
                let _: () = msg_send![session, addOutput: &*output];
            } else if can_input {
                let _: () = msg_send![session, removeInput: &*input];
            }
            let _: () = msg_send![session, commitConfiguration];
            can_output
        };
        if !added {
            return Err(VideoError::Unsupported);
        }
        self.input = Some(input);
        self.output = Some(output);
        Ok(())
    }

    /// Takes the camera's input and output out of the session, with the callbacks stopped.
    fn unconfigure(&mut self) {
        let session = &*self.session;
        // SAFETY: session configuration inside begin/commit; the output loses its delegate so
        // no callback is queued after this.
        unsafe {
            let _: () = msg_send![session, beginConfiguration];
            if let Some(output) = self.output.take() {
                let none: *mut AnyObject = std::ptr::null_mut();
                let _: () = msg_send![&*output, setSampleBufferDelegate: none, queue: none];
                let _: () = msg_send![session, removeOutput: &*output];
            }
            if let Some(input) = self.input.take() {
                let _: () = msg_send![session, removeInput: &*input];
            }
            let _: () = msg_send![session, commitConfiguration];
        }
        self.queue.drain();
        self.disarm();
    }
}

impl VideoSource for CameraSource {
    fn start(
        &mut self,
        config: VideoConfig,
        facing: Facing,
        out: FrameSender,
    ) -> Result<(), VideoError> {
        self.stop()?;
        autoreleasepool(|_| {
            self.arm(config, facing, out)?;
            if let Err(error) = self.configure(config, facing) {
                self.unconfigure();
                return Err(error);
            }
            let session = &*self.session;
            // SAFETY: `startRunning` blocks until the camera runs (or fails); `isRunning` is a
            // BOOL property.
            let running: bool = unsafe {
                let _: () = msg_send![session, startRunning];
                msg_send![session, isRunning]
            };
            if !running {
                self.unconfigure();
                return Err(VideoError::Backend(
                    "capture session did not start".to_owned(),
                ));
            }
            self.running = true;
            Ok(())
        })
    }

    fn stop(&mut self) -> Result<(), VideoError> {
        if !self.running {
            return Ok(());
        }
        self.running = false;
        autoreleasepool(|_| {
            // SAFETY: `stopRunning` blocks until the session has stopped.
            unsafe {
                let _: () = msg_send![&*self.session, stopRunning];
            }
            self.unconfigure();
        });
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.controls.force_keyframe.store(true, Ordering::Relaxed);
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.controls
            .bitrate
            .store(clamp_bitrate(bps), Ordering::Relaxed);
        self.controls.bitrate_changed.store(true, Ordering::Relaxed);
    }

    fn switch_camera(&mut self, facing: Facing) -> Result<(), VideoError> {
        if !self.running {
            return Ok(());
        }
        autoreleasepool(|_| {
            let input = Self::camera_input(facing)?;
            let session = &*self.session;
            // SAFETY: session configuration inside begin/commit; the old input goes back if the
            // new one cannot be added.
            let switched = unsafe {
                let _: () = msg_send![session, beginConfiguration];
                if let Some(old) = &self.input {
                    let _: () = msg_send![session, removeInput: &**old];
                }
                let can_add: bool = msg_send![session, canAddInput: &*input];
                if can_add {
                    let _: () = msg_send![session, addInput: &*input];
                } else if let Some(old) = &self.input {
                    let _: () = msg_send![session, addInput: &**old];
                }
                let _: () = msg_send![session, commitConfiguration];
                can_add
            };
            if !switched {
                return Err(VideoError::Unsupported);
            }
            self.input = Some(input);
            self.controls
                .back
                .store(facing == Facing::Back, Ordering::Relaxed);
            self.controls.force_keyframe.store(true, Ordering::Relaxed);
            Ok(())
        })
    }
}

impl Drop for CameraSource {
    fn drop(&mut self) {
        let _ = self.stop();
        self.disarm();
    }
}

#[cfg(test)]
mod tests {
    use objc2::msg_send;

    use super::*;
    use crate::video::ios::encoder::tests::synthetic_frame;
    use crate::video::ios::ffi::*;
    use crate::video::ios::h264::{NAL_SPS, annexb_nals, nal_type, parse_annexb};
    use crate::video::{EncodedFrame, Rotation, frame_channel};

    #[link(name = "CoreMedia", kind = "framework")]
    unsafe extern "C" {
        fn CMVideoFormatDescriptionCreateForImageBuffer(
            allocator: CFAllocatorRef,
            image_buffer: CVPixelBufferRef,
            format_description_out: *mut CMFormatDescriptionRef,
        ) -> OSStatus;
        fn CMSampleBufferCreateReadyWithImageBuffer(
            allocator: CFAllocatorRef,
            image_buffer: CVPixelBufferRef,
            format_description: CMFormatDescriptionRef,
            sample_timing: *const CMSampleTimingInfo,
            sample_buffer_out: *mut CMSampleBufferRef,
        ) -> OSStatus;
    }

    /// What the camera would hand the delegate: a synthetic frame captured at `millis`.
    fn camera_sample(width: usize, height: usize, millis: i64) -> CfOwned<OpaqueSampleBuffer> {
        let image = synthetic_frame(width, height, millis as usize);
        let mut format = std::ptr::null_mut();
        let mut sample = std::ptr::null_mut();
        let timing = CMSampleTimingInfo {
            duration: CMTime::INVALID,
            presentation_time_stamp: CMTime::new(millis, 1000),
            decode_time_stamp: CMTime::INVALID,
        };
        // SAFETY: a live pixel buffer and places for the outputs; both are `Create` calls whose
        // references are taken over just below.
        unsafe {
            assert_eq!(
                CMVideoFormatDescriptionCreateForImageBuffer(
                    std::ptr::null(),
                    image.as_ptr(),
                    &mut format
                ),
                0
            );
            let format = CfOwned::from_create(format).unwrap();
            assert_eq!(
                CMSampleBufferCreateReadyWithImageBuffer(
                    std::ptr::null(),
                    image.as_ptr(),
                    format.as_ptr(),
                    &timing,
                    &mut sample,
                ),
                0
            );
            CfOwned::from_create(sample).unwrap()
        }
    }

    /// Hands `sample` to the source's delegate the way AVFoundation does, through the
    /// Objective-C method.
    fn deliver(source: &CameraSource, sample: &CfOwned<OpaqueSampleBuffer>) {
        let sample = sample.as_ptr();
        let none: *mut objc2::runtime::AnyObject = std::ptr::null_mut();
        // SAFETY: the delegate method's own signature; the output and connection are unused.
        unsafe {
            let _: () = msg_send![
                &*source.delegate,
                captureOutput: none,
                didOutputSampleBuffer: sample,
                fromConnection: none
            ];
        }
    }

    fn sps_of(frame: &EncodedFrame) -> Option<Vec<u8>> {
        parse_annexb(&frame.data).unwrap().sps.map(<[u8]>::to_vec)
    }

    #[test]
    #[ignore = "needs AVFoundation: run in the iOS simulator"]
    fn a_new_source_has_a_preview_layer_and_can_move_threads() {
        fn assert_send<T: Send>() {}
        assert_send::<CameraSource>();
        let source = CameraSource::new().unwrap();
        assert!(!source.preview_layer().is_null());
    }

    #[test]
    #[ignore = "needs AVFoundation: run in the iOS simulator"]
    fn without_a_camera_or_its_permission_start_fails_cleanly() {
        let mut source = CameraSource::new().unwrap();
        let (sender, _receiver) = frame_channel(3);
        let result = source.start(VideoConfig::default(), Facing::Front, sender);
        assert!(
            matches!(
                result,
                Err(VideoError::NoCamera | VideoError::PermissionDenied)
            ),
            "{result:?}"
        );
        // Everything else is harmless on a source that is not running.
        source.request_keyframe();
        source.set_bitrate(1_000_000);
        source.set_device_orientation(DeviceOrientation::Portrait);
        assert_eq!(source.switch_camera(Facing::Back), Ok(()));
        assert_eq!(source.stop(), Ok(()));
    }

    #[test]
    #[ignore = "needs VideoToolbox: run in the iOS simulator"]
    fn captured_frames_are_encoded_with_the_phone_orientation() {
        let mut source = CameraSource::new().unwrap();
        let (sender, receiver) = frame_channel(32);
        let config = VideoConfig {
            fps: 15,
            ..VideoConfig::default()
        };
        source.set_device_orientation(DeviceOrientation::LandscapeLeft);
        source.arm(config, Facing::Back, sender).unwrap();

        // A 30 fps camera for a 15 fps call: every other frame is encoded.
        for millis in [0, 33, 67, 100] {
            deliver(&source, &camera_sample(640, 480, millis));
        }
        source.set_device_orientation(DeviceOrientation::Portrait);
        source.request_keyframe();
        source.set_bitrate(300_000);
        deliver(&source, &camera_sample(640, 480, 133));
        source.set_device_orientation(DeviceOrientation::FaceUp);
        deliver(&source, &camera_sample(640, 480, 200));
        // Another camera, another size: a new encoder, a keyframe with the new SPS.
        deliver(&source, &camera_sample(320, 240, 267));
        source.disarm();

        let frames: Vec<EncodedFrame> = std::iter::from_fn(|| receiver.try_recv().ok()).collect();
        let summary: Vec<(u128, bool, Rotation)> = frames
            .iter()
            .map(|frame| (frame.timestamp.as_millis(), frame.keyframe, frame.rotation))
            .collect();
        assert_eq!(
            summary,
            [
                (0, true, Rotation::Deg0),
                (67, false, Rotation::Deg0),
                (133, true, Rotation::Deg90),
                (200, false, Rotation::Deg90),
                (267, true, Rotation::Deg90),
            ]
        );
        for frame in &frames {
            let has_sps = annexb_nals(&frame.data)
                .into_iter()
                .any(|nal| nal_type(nal) == Some(NAL_SPS));
            assert_eq!(frame.keyframe, has_sps);
        }
        assert_eq!(sps_of(&frames[0]), sps_of(&frames[2]));
        assert_ne!(sps_of(&frames[0]), sps_of(&frames[4]));
    }

    #[test]
    #[ignore = "needs VideoToolbox: run in the iOS simulator"]
    fn the_front_camera_in_landscape_is_turned_half_way() {
        let mut source = CameraSource::new().unwrap();
        let (sender, receiver) = frame_channel(4);
        source.set_device_orientation(DeviceOrientation::LandscapeLeft);
        source
            .arm(VideoConfig::default(), Facing::Front, sender)
            .unwrap();
        deliver(&source, &camera_sample(320, 240, 0));
        source.disarm();
        let frame = receiver.try_recv().unwrap();
        assert_eq!(frame.rotation, Rotation::Deg180);
    }
}
