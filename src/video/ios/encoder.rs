use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::ffi::{
    CMBlockBufferCopyDataBytes, CMBlockBufferGetDataLength, CMFormatDescriptionRef,
    CMSampleBufferDataIsReady, CMSampleBufferGetDataBuffer, CMSampleBufferGetFormatDescription,
    CMSampleBufferGetPresentationTimeStamp, CMSampleBufferRef, CMTime,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex, CVPixelBufferRef, CfOwned,
    K_CMVIDEO_CODEC_TYPE_H264, K_VTENCODE_INFO_FRAME_DROPPED, OSStatus, OpaqueCompressionSession,
    VTCompressionSessionCompleteFrames, VTCompressionSessionCreate,
    VTCompressionSessionEncodeFrame, VTCompressionSessionInvalidate,
    VTCompressionSessionPrepareToEncodeFrames, VTSessionSetProperty, backend, cf_array,
    cf_dictionary, cf_f64, cf_i32, check, kCFBooleanTrue,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_DataRateLimits, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_Baseline_AutoLevel, kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
};
use super::h264::{avcc_nals, contains_idr, write_annexb};
use super::settings::{
    KEYFRAME_INTERVAL, cmtime_to_duration, data_rate_limit, keyframe_interval_frames,
};
use crate::video::{EncodedFrame, FrameSender, Rotation, SendOutcome, VideoError, clamp_bitrate};

pub(crate) struct EncoderOutput {
    sender: FrameSender,
    errors: AtomicU64,
    closed: AtomicBool,
}

impl EncoderOutput {
    pub(crate) fn new(sender: FrameSender) -> Arc<Self> {
        Arc::new(Self {
            sender,
            errors: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        })
    }

    pub(crate) fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    pub(crate) fn closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub(crate) fn keyframe_needed(&self) -> bool {
        self.sender.keyframe_needed()
    }

    pub(crate) fn count_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// # Safety
    ///
    /// `sample` is a live `CMSampleBuffer` from the compression session.
    unsafe fn deliver(&self, sample: CMSampleBufferRef, rotation: Rotation) {
        // SAFETY: a live sample buffer, per the caller.
        match unsafe { read_sample(sample) } {
            Some((data, keyframe, timestamp)) => {
                let frame = EncodedFrame {
                    data,
                    keyframe,
                    timestamp,
                    rotation,
                };
                if self.sender.try_send(frame) == SendOutcome::Closed {
                    self.closed.store(true, Ordering::Relaxed);
                }
            }
            None => self.count_error(),
        }
    }
}

/// # Safety
///
/// `sample` is a live `CMSampleBuffer` holding H.264 in AVCC.
unsafe fn read_sample(sample: CMSampleBufferRef) -> Option<(Vec<u8>, bool, std::time::Duration)> {
    // SAFETY: `sample` is live; the block buffer and format description it returns are borrowed
    // from it and outlive this function, which holds no reference past it.
    unsafe {
        if CMSampleBufferDataIsReady(sample) == 0 {
            return None;
        }
        let pts = CMSampleBufferGetPresentationTimeStamp(sample);
        if !pts.is_valid() {
            return None;
        }
        let timestamp = cmtime_to_duration(pts.value, pts.timescale)?;
        let block = CMSampleBufferGetDataBuffer(sample);
        let format = CMSampleBufferGetFormatDescription(sample);
        if block.is_null() || format.is_null() {
            return None;
        }
        let length = CMBlockBufferGetDataLength(block);
        let mut avcc = vec![0u8; length];
        let copied = CMBlockBufferCopyDataBytes(block, 0, length, avcc.as_mut_ptr().cast());
        if copied != 0 {
            return None;
        }
        let (count, length_size) = parameter_set_info(format)?;
        let nals = avcc_nals(&avcc, length_size).ok()?;
        let keyframe = contains_idr(&nals);
        let mut parameter_sets = Vec::new();
        if keyframe {
            for index in 0..count {
                parameter_sets.push(parameter_set(format, index)?);
            }
        }
        Some((write_annexb(&nals, &parameter_sets), keyframe, timestamp))
    }
}

/// # Safety
///
/// `format` is a live H.264 format description.
unsafe fn parameter_set_info(format: CMFormatDescriptionRef) -> Option<(usize, usize)> {
    let mut count = 0usize;
    let mut length_size = 0i32;
    // SAFETY: a live description; the pointers are valid places for the outputs asked for.
    let status = unsafe {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            format,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut count,
            &mut length_size,
        )
    };
    (status == 0).then_some((count, usize::try_from(length_size).ok()?))
}

/// # Safety
///
/// `format` is a live H.264 format description, and outlives the returned slice.
unsafe fn parameter_set<'a>(format: CMFormatDescriptionRef, index: usize) -> Option<&'a [u8]> {
    let mut pointer = std::ptr::null();
    let mut size = 0usize;
    // SAFETY: a live description; the pointers are valid places for the outputs asked for.
    let status = unsafe {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            format,
            index,
            &mut pointer,
            &mut size,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if status != 0 || pointer.is_null() || size == 0 {
        return None;
    }
    // SAFETY: CoreMedia points at `size` bytes it owns while `format` lives (the caller's
    // lifetime).
    Some(unsafe { std::slice::from_raw_parts(pointer, size) })
}

/// The frame's refcon carries its [`Rotation`] as a CVO byte.
///
/// # Safety
///
/// `output` is the [`EncoderOutput`] the session was created with (kept alive by its
/// [`H264Encoder`] until the session is invalidated), and `sample` is null or a live sample.
unsafe extern "C" fn encoded(
    output: *mut c_void,
    frame: *mut c_void,
    status: OSStatus,
    flags: u32,
    sample: super::ffi::CMSampleBufferRef,
) {
    if output.is_null() {
        return;
    }
    // SAFETY: see above: the pointer is a live `EncoderOutput`.
    let output = unsafe { &*output.cast::<EncoderOutput>() };
    if status != 0 || sample.is_null() || flags & K_VTENCODE_INFO_FRAME_DROPPED != 0 {
        output.count_error();
        return;
    }
    let rotation = Rotation::from_cvo(frame as usize as u8);
    // SAFETY: a live sample buffer from the session.
    unsafe { output.deliver(sample, rotation) };
}

pub(crate) struct H264Encoder {
    session: CfOwned<OpaqueCompressionSession>,
    output: Arc<EncoderOutput>,
    force_keyframe: CfOwned<c_void>,
    width: usize,
    height: usize,
}

impl H264Encoder {
    pub(crate) fn new(
        width: usize,
        height: usize,
        fps: u32,
        bitrate_bps: u32,
        output: Arc<EncoderOutput>,
    ) -> Result<Self, VideoError> {
        let (Ok(w), Ok(h)) = (i32::try_from(width), i32::try_from(height)) else {
            return Err(VideoError::Unsupported);
        };
        // SAFETY: reading constant CF objects the frameworks export.
        let (force_key, yes) = unsafe { (kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue) };
        let force_keyframe = cf_dictionary(&[(force_key, yes)])?;
        let mut session = std::ptr::null_mut();
        // SAFETY: valid arguments; the refcon is the `EncoderOutput` this encoder keeps alive
        // (in `self.output`) until `Drop` has invalidated the session.
        let status = unsafe {
            VTCompressionSessionCreate(
                std::ptr::null(),
                w,
                h,
                K_CMVIDEO_CODEC_TYPE_H264,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                Some(encoded),
                Arc::as_ptr(&output).cast_mut().cast(),
                &mut session,
            )
        };
        check(status, "VTCompressionSessionCreate")?;
        // SAFETY: a `Create` call hands over its reference.
        let session = unsafe { CfOwned::from_create(session) }
            .ok_or_else(|| backend("VTCompressionSessionCreate"))?;
        let mut encoder = Self {
            session,
            output,
            force_keyframe,
            width,
            height,
        };
        encoder.configure(fps)?;
        encoder.set_bitrate(bitrate_bps)?;
        // SAFETY: a live, configured session.
        check(
            unsafe { VTCompressionSessionPrepareToEncodeFrames(encoder.session.as_ptr()) },
            "VTCompressionSessionPrepareToEncodeFrames",
        )?;
        Ok(encoder)
    }

    fn set(&self, key: *const c_void, value: *const c_void, name: &str) -> Result<(), VideoError> {
        // SAFETY: a live session, a property key and a CF value of the type it takes.
        check(
            unsafe { VTSessionSetProperty(self.session.as_ptr().cast(), key, value) },
            name,
        )
    }

    fn configure(&self, fps: u32) -> Result<(), VideoError> {
        // SAFETY: reading constant CF objects the frameworks export.
        let (real_time, profile, reordering, interval, interval_duration, frame_rate, yes, no) = unsafe {
            (
                kVTCompressionPropertyKey_RealTime,
                kVTCompressionPropertyKey_ProfileLevel,
                kVTCompressionPropertyKey_AllowFrameReordering,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                kVTCompressionPropertyKey_ExpectedFrameRate,
                kCFBooleanTrue,
                super::ffi::kCFBooleanFalse,
            )
        };
        self.set(real_time, yes, "RealTime")?;
        // SAFETY: as above.
        let (constrained, baseline) = unsafe {
            (
                kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
                kVTProfileLevel_H264_Baseline_AutoLevel,
            )
        };
        // Plain Baseline as a fallback, which VideoToolbox writes without B-frames anyway.
        self.set(profile, constrained, "ProfileLevel")
            .or_else(|_| self.set(profile, baseline, "ProfileLevel"))?;
        self.set(reordering, no, "AllowFrameReordering")?;
        let frames = i32::try_from(keyframe_interval_frames(fps)).unwrap_or(i32::MAX);
        self.set(interval, cf_i32(frames)?.as_ptr(), "MaxKeyFrameInterval")?;
        let seconds = cf_f64(KEYFRAME_INTERVAL.as_secs_f64())?;
        self.set(
            interval_duration,
            seconds.as_ptr(),
            "MaxKeyFrameIntervalDuration",
        )?;
        let rate = cf_i32(i32::try_from(fps.max(1)).unwrap_or(i32::MAX))?;
        self.set(frame_rate, rate.as_ptr(), "ExpectedFrameRate")
    }

    pub(crate) fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    pub(crate) fn set_bitrate(&mut self, bps: u32) -> Result<(), VideoError> {
        let bps = clamp_bitrate(bps);
        // SAFETY: reading constant CFStrings the framework exports.
        let (average_key, limits_key) = unsafe {
            (
                kVTCompressionPropertyKey_AverageBitRate,
                kVTCompressionPropertyKey_DataRateLimits,
            )
        };
        let average = cf_i32(i32::try_from(bps).unwrap_or(i32::MAX))?;
        self.set(average_key, average.as_ptr(), "AverageBitRate")?;
        let (bytes, seconds) = data_rate_limit(bps);
        let bytes = cf_i32(i32::try_from(bytes).unwrap_or(i32::MAX))?;
        let seconds = cf_f64(seconds)?;
        let limits = cf_array(&[bytes.as_ptr().cast_const(), seconds.as_ptr().cast_const()])?;
        self.set(limits_key, limits.as_ptr(), "DataRateLimits")
    }

    /// # Safety
    ///
    /// `image` is a live `CVPixelBuffer` of this encoder's size.
    pub(crate) unsafe fn encode(
        &mut self,
        image: CVPixelBufferRef,
        pts: CMTime,
        rotation: Rotation,
        keyframe: bool,
    ) -> Result<(), VideoError> {
        let properties = if keyframe {
            self.force_keyframe.as_ptr().cast_const()
        } else {
            std::ptr::null()
        };
        let mut flags = 0u32;
        // SAFETY: a live session and pixel buffer (per the caller); the refcon is a plain
        // number, never dereferenced.
        let status = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session.as_ptr(),
                image,
                pts,
                CMTime::INVALID,
                properties,
                usize::from(rotation.to_cvo()) as *mut c_void,
                &mut flags,
            )
        };
        if status != 0 {
            self.output.count_error();
        }
        check(status, "VTCompressionSessionEncodeFrame")
    }

    #[cfg(test)]
    pub(crate) fn flush(&mut self) -> Result<(), VideoError> {
        check(
            unsafe { VTCompressionSessionCompleteFrames(self.session.as_ptr(), CMTime::INVALID) },
            "VTCompressionSessionCompleteFrames",
        )
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        // SAFETY: a live session. Completing first means no callback is still running when the
        // session goes and, after it, `self.output` (the callback's refcon).
        unsafe {
            VTCompressionSessionCompleteFrames(self.session.as_ptr(), CMTime::INVALID);
            VTCompressionSessionInvalidate(self.session.as_ptr());
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use super::*;
    use crate::video::ios::ffi::*;
    use crate::video::ios::h264::{NAL_IDR, NAL_PPS, NAL_SPS, START_CODE, annexb_nals, nal_type};
    use crate::video::{EncodedFrame, FrameReceiver, frame_channel};

    pub(crate) fn synthetic_frame(
        width: usize,
        height: usize,
        index: usize,
    ) -> CfOwned<OpaquePixelBuffer> {
        let empty = cf_dictionary(&[]).unwrap();
        let format = cf_i32(K_CVPIXEL_FORMAT_NV12 as i32).unwrap();
        let (surface_key, format_key) = unsafe {
            (
                kCVPixelBufferIOSurfacePropertiesKey,
                kCVPixelBufferPixelFormatTypeKey,
            )
        };
        let attributes = cf_dictionary(&[
            (surface_key, empty.as_ptr().cast_const()),
            (format_key, format.as_ptr().cast_const()),
        ])
        .unwrap();
        let mut buffer = std::ptr::null_mut();
        let status = unsafe {
            CVPixelBufferCreate(
                std::ptr::null(),
                width,
                height,
                K_CVPIXEL_FORMAT_NV12,
                attributes.as_ptr().cast_const(),
                &mut buffer,
            )
        };
        assert_eq!(status, 0, "CVPixelBufferCreate");
        let buffer = unsafe { CfOwned::from_create(buffer) }.unwrap();
        unsafe {
            assert_eq!(CVPixelBufferLockBaseAddress(buffer.as_ptr(), 0), 0);
            for plane in 0..2 {
                let base = CVPixelBufferGetBaseAddressOfPlane(buffer.as_ptr(), plane).cast::<u8>();
                let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer.as_ptr(), plane);
                let rows = CVPixelBufferGetHeightOfPlane(buffer.as_ptr(), plane);
                for row in 0..rows {
                    let line = std::slice::from_raw_parts_mut(base.add(row * stride), stride);
                    for (column, pixel) in line.iter_mut().enumerate() {
                        *pixel = if plane == 0 {
                            ((row + column + index * 4) % 256) as u8
                        } else {
                            128
                        };
                    }
                }
            }
            assert_eq!(CVPixelBufferUnlockBaseAddress(buffer.as_ptr(), 0), 0);
        }
        buffer
    }

    pub(crate) fn encode_synthetic(width: usize, height: usize, count: usize) -> Vec<EncodedFrame> {
        let (sender, receiver) = frame_channel(count + 1);
        let output = EncoderOutput::new(sender);
        let mut encoder =
            H264Encoder::new(width, height, 30, 800_000, output.clone()).expect("encoder");
        for index in 0..count {
            let frame = synthetic_frame(width, height, index);
            let pts = CMTime::new(index as i64 * 1_000_000 / 30, 1_000_000);
            let keyframe = output.keyframe_needed();
            unsafe { encoder.encode(frame.as_ptr(), pts, Rotation::Deg90, keyframe) }
                .expect("encode");
        }
        encoder.flush().expect("flush");
        assert_eq!(output.errors(), 0);
        drain(&receiver)
    }

    fn drain(receiver: &FrameReceiver) -> Vec<EncodedFrame> {
        std::iter::from_fn(|| receiver.try_recv().ok()).collect()
    }

    #[test]
    #[ignore = "needs VideoToolbox: run in the iOS simulator"]
    fn thirty_synthetic_frames_encode_to_annexb_with_parameter_sets_on_the_keyframe() {
        let frames = encode_synthetic(640, 480, 30);
        assert_eq!(frames.len(), 30);

        let first = &frames[0];
        assert!(first.keyframe);
        let types: Vec<_> = annexb_nals(&first.data)
            .into_iter()
            .filter_map(nal_type)
            .collect();
        assert_eq!(&types[..2], &[NAL_SPS, NAL_PPS], "{types:?}");
        assert!(types.contains(&NAL_IDR));
        assert!(first.data.starts_with(&START_CODE));

        let sps = annexb_nals(&first.data)[0];
        assert_eq!(sps[1], 0x42, "profile_idc");
        assert_ne!(sps[2] & 0x40, 0, "constraint_set1_flag");

        for (index, frame) in frames.iter().enumerate() {
            assert_eq!(frame.rotation, Rotation::Deg90);
            assert_eq!(
                frame.timestamp,
                Duration::from_micros(index as u64 * 1_000_000 / 30)
            );
            assert_eq!(frame.keyframe, index == 0, "frame {index}");
            let types: Vec<_> = annexb_nals(&frame.data)
                .into_iter()
                .filter_map(nal_type)
                .collect();
            assert_eq!(frame.keyframe, types.contains(&NAL_SPS), "frame {index}");
        }
    }

    #[test]
    #[ignore = "needs VideoToolbox: run in the iOS simulator"]
    fn a_forced_keyframe_carries_its_parameter_sets_and_bitrates_change_live() {
        let (sender, receiver) = frame_channel(16);
        let output = EncoderOutput::new(sender);
        let mut encoder = H264Encoder::new(320, 240, 30, 300_000, output.clone()).unwrap();
        for index in 0..6 {
            if index == 3 {
                encoder.set_bitrate(1_000_000).unwrap();
            }
            let frame = synthetic_frame(320, 240, index);
            let pts = CMTime::new(index as i64, 30);
            unsafe { encoder.encode(frame.as_ptr(), pts, Rotation::Deg0, index == 4) }.unwrap();
        }
        encoder.flush().unwrap();
        let keyframes: Vec<_> = drain(&receiver)
            .iter()
            .map(|frame| {
                let has_sps = annexb_nals(&frame.data)
                    .into_iter()
                    .any(|nal| nal_type(nal) == Some(NAL_SPS));
                assert_eq!(frame.keyframe, has_sps);
                frame.keyframe
            })
            .collect();
        assert_eq!(keyframes, [true, false, false, false, true, false]);
    }

    #[test]
    #[ignore = "needs VideoToolbox: run in the iOS simulator"]
    fn a_closed_channel_is_noticed() {
        let (sender, receiver) = frame_channel(1);
        drop(receiver);
        let output = EncoderOutput::new(sender);
        let mut encoder = H264Encoder::new(320, 240, 30, 300_000, output.clone()).unwrap();
        let frame = synthetic_frame(320, 240, 0);
        unsafe { encoder.encode(frame.as_ptr(), CMTime::new(0, 30), Rotation::Deg0, true) }
            .unwrap();
        encoder.flush().unwrap();
        assert!(output.closed());
    }
}
