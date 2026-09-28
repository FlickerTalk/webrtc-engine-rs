//! Remote video on an `AVSampleBufferDisplayLayer`, which decodes H.264 in hardware itself.
//!
//! Each Annex-B access unit is taken apart: a new SPS/PPS pair makes a new
//! `CMVideoFormatDescription`, the slices go into a `CMBlockBuffer` in AVCC, and a ready
//! `CMSampleBuffer` marked `DisplayImmediately` is enqueued to the layer (the engine already
//! paces frames with its jitter buffer, so the layer needs no timebase). The frame's
//! [`Rotation`] becomes the layer's affine transform.
//!
//! Recovery: when the layer fails (`status == .failed`) or the system flushed it while the app was
//! in the background (`requiresFlushToResumeDecoding`), it is flushed and the [`DecodeGate`]
//! drops delta frames until a keyframe, asking the sender for one through
//! [`KeyframeRequests`]. The same happens for the first frames, after a stop, after a frame the
//! layer had no room for and after a frame that is not H.264.

use std::ffi::c_void;
use std::time::Duration;

use objc2::msg_send;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;

use super::ffi::{
    AVLayerVideoGravityResizeAspect, CFArrayGetCount, CFArrayGetValueAtIndex, CFDictionarySetValue,
    CGAffineTransform, CMBlockBufferCreateWithMemoryBlock, CMBlockBufferReplaceDataBytes,
    CMSampleBufferCreateReady, CMSampleBufferGetSampleAttachmentsArray, CMSampleTimingInfo, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets, CfOwned,
    K_CMBLOCK_BUFFER_ASSURE_MEMORY_NOW_FLAG, OpaqueFormatDescription, OpaqueSampleBuffer, Owned,
    backend, check, class, kCFBooleanTrue, kCMSampleAttachmentKey_DisplayImmediately,
};
use super::h264::parse_annexb;
use super::keyframes::{DecodeGate, KeyframeRequests};
use super::orientation::layer_transform;
use super::settings::{CMTIME_TIMESCALE, duration_to_cmtime};
use crate::video::{EncodedFrame, Rotation, VideoError, VideoSink};

/// `AVQueuedSampleBufferRenderingStatusFailed`.
const RENDERING_STATUS_FAILED: isize = 2;

/// The `CMVideoFormatDescription` for the SPS and PPS seen last, rebuilt when they change.
#[derive(Default)]
pub(crate) struct FormatCache {
    current: Option<(Vec<u8>, Vec<u8>, CfOwned<OpaqueFormatDescription>)>,
}

impl FormatCache {
    /// Makes `sps` and `pps` the current parameter sets; `true` if they changed.
    pub(crate) fn update(&mut self, sps: &[u8], pps: &[u8]) -> Result<bool, VideoError> {
        if let Some((current_sps, current_pps, _)) = &self.current
            && current_sps == sps
            && current_pps == pps
        {
            return Ok(false);
        }
        let pointers = [sps.as_ptr(), pps.as_ptr()];
        let sizes = [sps.len(), pps.len()];
        let mut description = std::ptr::null_mut();
        // SAFETY: two parameter sets, each pointer with its size; CoreMedia copies them.
        let status = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                std::ptr::null(),
                2,
                pointers.as_ptr(),
                sizes.as_ptr(),
                4,
                &mut description,
            )
        };
        check(
            status,
            "CMVideoFormatDescriptionCreateFromH264ParameterSets",
        )?;
        // SAFETY: a `Create` call hands over its reference.
        let description = unsafe { CfOwned::from_create(description) }
            .ok_or_else(|| backend("CMVideoFormatDescriptionCreateFromH264ParameterSets"))?;
        self.current = Some((sps.to_vec(), pps.to_vec(), description));
        Ok(true)
    }

    /// The current format description.
    pub(crate) fn current(&self) -> Option<&CfOwned<OpaqueFormatDescription>> {
        self.current.as_ref().map(|current| &current.2)
    }

    /// Forgets the parameter sets.
    pub(crate) fn clear(&mut self) {
        self.current = None;
    }
}

/// A ready `CMSampleBuffer` holding one AVCC access unit (four-byte lengths), marked to be shown
/// as soon as it is decoded.
pub(crate) fn sample_buffer(
    format: &CfOwned<OpaqueFormatDescription>,
    avcc: &[u8],
    pts: Duration,
) -> Result<CfOwned<OpaqueSampleBuffer>, VideoError> {
    if avcc.is_empty() {
        return Err(VideoError::Backend("empty access unit".to_owned()));
    }
    let mut block = std::ptr::null_mut();
    // SAFETY: CoreMedia allocates `avcc.len()` bytes itself (no memory block passed in).
    let status = unsafe {
        CMBlockBufferCreateWithMemoryBlock(
            std::ptr::null(),
            std::ptr::null_mut(),
            avcc.len(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            avcc.len(),
            K_CMBLOCK_BUFFER_ASSURE_MEMORY_NOW_FLAG,
            &mut block,
        )
    };
    check(status, "CMBlockBufferCreateWithMemoryBlock")?;
    // SAFETY: a `Create` call hands over its reference.
    let block = unsafe { CfOwned::from_create(block) }
        .ok_or_else(|| backend("CMBlockBufferCreateWithMemoryBlock"))?;
    // SAFETY: the block holds exactly `avcc.len()` bytes; `avcc` is that long.
    let status = unsafe {
        CMBlockBufferReplaceDataBytes(avcc.as_ptr().cast(), block.as_ptr(), 0, avcc.len())
    };
    check(status, "CMBlockBufferReplaceDataBytes")?;

    let timing = CMSampleTimingInfo {
        duration: CMTime::INVALID,
        presentation_time_stamp: CMTime::new(duration_to_cmtime(pts), CMTIME_TIMESCALE),
        decode_time_stamp: CMTime::INVALID,
    };
    let size = avcc.len();
    let mut sample = std::ptr::null_mut();
    // SAFETY: a live block buffer and format description, one sample with one timing entry and
    // one size entry.
    let status = unsafe {
        CMSampleBufferCreateReady(
            std::ptr::null(),
            block.as_ptr(),
            format.as_ptr(),
            1,
            1,
            &timing,
            1,
            &size,
            &mut sample,
        )
    };
    check(status, "CMSampleBufferCreateReady")?;
    // SAFETY: a `Create` call hands over its reference.
    let sample = unsafe { CfOwned::from_create(sample) }
        .ok_or_else(|| backend("CMSampleBufferCreateReady"))?;

    // SAFETY: a live sample buffer; asking for the array creates it with one mutable dictionary
    // per sample, owned by the buffer; the key and value are live constant CF objects.
    unsafe {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample.as_ptr(), 1);
        if attachments.is_null() || CFArrayGetCount(attachments) < 1 {
            return Err(backend("CMSampleBufferGetSampleAttachmentsArray"));
        }
        let dictionary = CFArrayGetValueAtIndex(attachments, 0).cast_mut();
        CFDictionarySetValue(
            dictionary,
            kCMSampleAttachmentKey_DisplayImmediately,
            kCFBooleanTrue,
        );
    }
    Ok(sample)
}

/// Remote video for the app's view: an `AVSampleBufferDisplayLayer` fed with H.264.
///
/// The layer decodes in hardware on its own; [`push`](VideoSink::push) only repackages and
/// enqueues, without blocking. See [`KeyframeRequests`] for how it asks for keyframes, and the
/// [module docs](super) for how the app attaches [`layer`](Self::layer).
pub struct DisplaySink {
    layer: Owned,
    requests: KeyframeRequests,
    gate: DecodeGate,
    formats: FormatCache,
    rotation: Rotation,
    running: bool,
}

impl DisplaySink {
    /// A sink with its layer, not started.
    pub fn new() -> Result<Self, VideoError> {
        autoreleasepool(|_| {
            let layer_class = class(c"AVSampleBufferDisplayLayer")?;
            // SAFETY: `+new` on a layer class returns a new, owned layer.
            let layer: Retained<AnyObject> = unsafe { msg_send![layer_class, new] };
            // SAFETY: `videoGravity` takes one of the framework's gravity strings.
            unsafe {
                let gravity = AVLayerVideoGravityResizeAspect;
                let _: () = msg_send![&*layer, setVideoGravity: gravity];
            }
            Ok(Self {
                layer: Owned::new(layer),
                requests: KeyframeRequests::default(),
                gate: DecodeGate::new(),
                formats: FormatCache::default(),
                rotation: Rotation::Deg0,
                running: false,
            })
        })
    }

    /// The `AVSampleBufferDisplayLayer` (a `CALayer`), for the app's Swift code to add to its
    /// view. Owned by the sink and valid until it is dropped.
    pub fn layer(&self) -> *mut c_void {
        self.layer.as_raw()
    }

    /// Where the sink asks for keyframes; see [`KeyframeRequests`].
    pub fn keyframe_requests(&self) -> KeyframeRequests {
        self.requests.clone()
    }

    /// Whether the layer reports a decode failure it has not recovered from.
    pub(crate) fn layer_failed(&self) -> bool {
        // SAFETY: `status` is an `AVQueuedSampleBufferRenderingStatus` (NSInteger) property.
        let status: isize = unsafe { msg_send![&*self.layer, status] };
        status == RENDERING_STATUS_FAILED
    }

    fn needs_flush(&self) -> bool {
        // SAFETY: a BOOL property (iOS 14+).
        let flush: bool = unsafe { msg_send![&*self.layer, requiresFlushToResumeDecoding] };
        flush || self.layer_failed()
    }

    fn ready_for_more(&self) -> bool {
        // SAFETY: a BOOL property.
        unsafe { msg_send![&*self.layer, isReadyForMoreMediaData] }
    }

    /// The rotation the layer shows frames with now.
    ///
    /// For a quarter turn the app lays the layer out with its width and height swapped: it sets
    /// `bounds` to the view's size turned (and `position` to the view's centre), never `frame`,
    /// which is undefined under a transform.
    pub fn rotation(&self) -> Rotation {
        self.rotation
    }

    fn apply_rotation(&mut self, rotation: Rotation) -> Result<(), VideoError> {
        if rotation == self.rotation {
            return Ok(());
        }
        let [a, b, c, d] = layer_transform(rotation);
        let transform = CGAffineTransform {
            a,
            b,
            c,
            d,
            tx: 0.0,
            ty: 0.0,
        };
        let transaction = class(c"CATransaction")?;
        // SAFETY: an explicit transaction, committed at once and without animation, as Core
        // Animation requires for layer changes off the main thread.
        unsafe {
            let _: () = msg_send![transaction, begin];
            let _: () = msg_send![transaction, setDisableActions: true];
            let _: () = msg_send![&*self.layer, setAffineTransform: transform];
            let _: () = msg_send![transaction, commit];
        }
        self.rotation = rotation;
        Ok(())
    }

    fn decode(&mut self, frame: EncodedFrame) -> Result<(), VideoError> {
        let Ok(unit) = parse_annexb(&frame.data) else {
            self.gate.lost();
            return Ok(());
        };
        if self.needs_flush() {
            // SAFETY: `flush` takes no arguments.
            unsafe {
                let _: () = msg_send![&*self.layer, flush];
            }
            self.gate.lost();
        }
        if let (Some(sps), Some(pps)) = (unit.sps, unit.pps)
            && let Err(error) = self.formats.update(sps, pps)
        {
            self.formats.clear();
            self.gate.lost();
            return Err(error);
        }
        let decodable_keyframe = unit.keyframe && self.formats.current().is_some();
        let admission = self.gate.admit(decodable_keyframe, frame.timestamp);
        if admission.request_keyframe {
            self.requests.request();
        }
        if !admission.decode {
            return Ok(());
        }
        let Some(format) = self.formats.current() else {
            self.gate.lost();
            return Ok(());
        };
        if !self.ready_for_more() {
            // The frame is lost, and with it the references of the next delta frames.
            self.gate.lost();
            return Ok(());
        }
        let sample = match sample_buffer(format, &unit.avcc, frame.timestamp) {
            Ok(sample) => sample,
            Err(error) => {
                self.gate.lost();
                return Err(error);
            }
        };
        self.apply_rotation(frame.rotation)?;
        let sample = sample.as_ptr();
        // SAFETY: a live, ready sample buffer; the layer retains what it keeps.
        unsafe {
            let _: () = msg_send![&*self.layer, enqueueSampleBuffer: sample];
        }
        Ok(())
    }
}

impl VideoSink for DisplaySink {
    fn start(&mut self) -> Result<(), VideoError> {
        self.gate = DecodeGate::new();
        self.running = true;
        Ok(())
    }

    fn push(&mut self, frame: EncodedFrame) -> Result<(), VideoError> {
        if !self.running {
            return Ok(());
        }
        autoreleasepool(|_| self.decode(frame))
    }

    fn stop(&mut self) -> Result<(), VideoError> {
        if !self.running {
            return Ok(());
        }
        self.running = false;
        // SAFETY: `flushAndRemoveImage` takes no arguments.
        unsafe {
            let _: () = msg_send![&*self.layer, flushAndRemoveImage];
        }
        self.gate.lost();
        self.formats.clear();
        Ok(())
    }

    /// [`KeyframeRequests::take`]: an event, cleared by the poll. The sink spaces its requests
    /// out already.
    fn keyframe_needed(&mut self) -> bool {
        self.requests.take()
    }
}

impl Drop for DisplaySink {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::video::ios::encoder::tests::encode_synthetic;
    use crate::video::ios::ffi::*;
    use crate::video::ios::h264::parse_annexb;

    /// What a `VTDecompressionSession` made of the frames.
    #[derive(Default)]
    struct Decoded {
        images: AtomicUsize,
        failures: AtomicUsize,
        wrong_size: AtomicUsize,
    }

    unsafe extern "C" fn decoded(
        decoded: *mut c_void,
        _frame: *mut c_void,
        status: OSStatus,
        _flags: u32,
        image: CVPixelBufferRef,
        _pts: CMTime,
        _duration: CMTime,
    ) {
        // SAFETY: the refcon is the `Decoded` the test keeps alive until the session is gone.
        let decoded = unsafe { &*decoded.cast::<Decoded>() };
        if status != 0 || image.is_null() {
            decoded.failures.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // SAFETY: a live image from the decoder.
        let size = unsafe { (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image)) };
        if size != (640, 480) {
            decoded.wrong_size.fetch_add(1, Ordering::Relaxed);
        }
        decoded.images.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    #[ignore = "needs VideoToolbox: run in the iOS simulator"]
    fn encoded_frames_become_sample_buffers_that_decode() {
        let frames = encode_synthetic(640, 480, 30);
        let mut formats = FormatCache::default();
        let first = parse_annexb(&frames[0].data).unwrap();
        assert!(
            formats
                .update(first.sps.unwrap(), first.pps.unwrap())
                .unwrap()
        );
        assert!(
            !formats
                .update(first.sps.unwrap(), first.pps.unwrap())
                .unwrap()
        );
        let format = formats.current().unwrap();
        // SAFETY: a live format description.
        let dimensions = unsafe { CMVideoFormatDescriptionGetDimensions(format.as_ptr()) };
        assert_eq!((dimensions.width, dimensions.height), (640, 480));

        let results = Decoded::default();
        let record = VTDecompressionOutputCallbackRecord {
            callback: Some(decoded),
            ref_con: (&raw const results).cast_mut().cast(),
        };
        let mut session = std::ptr::null_mut();
        // SAFETY: a live format description and callback record; `results` outlives the
        // session, which is invalidated below.
        let status = unsafe {
            VTDecompressionSessionCreate(
                std::ptr::null(),
                format.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                &record,
                &mut session,
            )
        };
        assert_eq!(status, 0, "VTDecompressionSessionCreate");
        for frame in &frames {
            let unit = parse_annexb(&frame.data).unwrap();
            let sample = sample_buffer(format, &unit.avcc, frame.timestamp).unwrap();
            // SAFETY: a live session and sample buffer; synchronous decoding (no flags).
            let status = unsafe {
                VTDecompressionSessionDecodeFrame(
                    session,
                    sample.as_ptr(),
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(status, 0, "VTDecompressionSessionDecodeFrame");
        }
        // SAFETY: a live session, torn down once, then released.
        unsafe {
            VTDecompressionSessionWaitForAsynchronousFrames(session);
            VTDecompressionSessionInvalidate(session);
            CFRelease(session.cast_const().cast());
        }
        assert_eq!(results.failures.load(Ordering::Relaxed), 0);
        assert_eq!(results.wrong_size.load(Ordering::Relaxed), 0);
        assert_eq!(results.images.load(Ordering::Relaxed), 30);
    }

    #[test]
    #[ignore = "needs AVFoundation: run in the iOS simulator"]
    fn the_display_layer_takes_a_whole_stream_without_asking_for_keyframes() {
        let frames = encode_synthetic(640, 480, 30);
        let mut sink = DisplaySink::new().unwrap();
        assert!(!sink.layer().is_null());
        let requests = sink.keyframe_requests();
        sink.start().unwrap();
        for frame in frames {
            sink.push(frame).unwrap();
        }
        assert_eq!(requests.total(), 0);
        assert!(!sink.layer_failed());
        // SAFETY: an NSInteger property; 1 is `AVQueuedSampleBufferRenderingStatusRendering`.
        let status: isize = unsafe { objc2::msg_send![&*sink.layer, status] };
        assert_eq!(status, 1);
        assert_eq!(sink.rotation(), Rotation::Deg90);
        sink.stop().unwrap();
    }

    #[test]
    #[ignore = "needs AVFoundation: run in the iOS simulator"]
    fn delta_frames_before_a_keyframe_are_dropped_and_ask_for_one() {
        let frames = encode_synthetic(640, 480, 30);
        let mut sink = DisplaySink::new().unwrap();
        let requests = sink.keyframe_requests();
        // Before `start` frames are ignored.
        sink.push(frames[1].clone()).unwrap();
        assert!(!requests.take());

        sink.start().unwrap();
        for frame in &frames[1..4] {
            sink.push(frame.clone()).unwrap();
        }
        assert!(requests.take());
        assert_eq!(requests.total(), 1, "spaced out by the retry interval");

        sink.push(frames[0].clone()).unwrap();
        sink.push(frames[4].clone()).unwrap();
        assert!(!requests.take());

        // After a stop the decoder has no references: a delta frame asks again.
        sink.stop().unwrap();
        sink.start().unwrap();
        sink.push(frames[5].clone()).unwrap();
        assert!(requests.take());
    }

    #[test]
    #[ignore = "needs AVFoundation: run in the iOS simulator"]
    fn the_engine_takes_the_keyframe_request_through_the_contract() {
        let frames = encode_synthetic(640, 480, 3);
        let mut sink = DisplaySink::new().unwrap();
        let sink: &mut dyn VideoSink = &mut sink;
        sink.start().unwrap();
        // A delta frame before any keyframe: the sink raises a request, taken once.
        sink.push(frames[1].clone()).unwrap();
        assert!(sink.keyframe_needed());
        assert!(!sink.keyframe_needed(), "an event: taken by the first poll");
        sink.push(frames[0].clone()).unwrap();
        assert!(!sink.keyframe_needed());
    }

    #[test]
    #[ignore = "needs AVFoundation: run in the iOS simulator"]
    fn a_frame_that_is_not_h264_waits_for_a_keyframe() {
        let frames = encode_synthetic(640, 480, 3);
        let mut sink = DisplaySink::new().unwrap();
        let requests = sink.keyframe_requests();
        sink.start().unwrap();
        sink.push(frames[0].clone()).unwrap();
        let mut garbage = frames[1].clone();
        garbage.data = vec![1, 2, 3];
        sink.push(garbage).unwrap();
        assert!(!requests.take());
        sink.push(frames[2].clone()).unwrap();
        assert!(requests.take());
    }
}
