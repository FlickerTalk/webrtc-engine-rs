//! The CoreFoundation, CoreMedia, CoreVideo, VideoToolbox and libdispatch declarations the iOS
//! video code needs, copied by hand from the iOS SDK headers (`CMTime.h`, `CMSampleBuffer.h`,
//! `CMFormatDescription.h`, `CMBlockBuffer.h`, `CVPixelBuffer.h`, `VTCompressionSession.h`,
//! `VTCompressionProperties.h`, `VTDecompressionSession.h`, `CFDictionary.h`, `queue.h`). The
//! compile-time asserts at the bottom pin the struct layouts.
//!
//! The opaque `CM*Ref` types get an Objective-C encoding so they can travel through `msg_send!`
//! (`enqueueSampleBuffer:`, the capture delegate) with objc2's debug encoding checks passing.

use std::ffi::{c_char, c_long, c_void};
use std::mem::size_of;

use objc2::encode::{Encode, Encoding, RefEncode};
use objc2::runtime::AnyObject;

use crate::video::VideoError;

/// A CoreMedia / VideoToolbox result: zero is success.
pub(crate) type OSStatus = i32;
/// A CoreVideo result: zero is success.
pub(crate) type CVReturn = i32;
pub(crate) type CFTypeRef = *const c_void;
pub(crate) type CFAllocatorRef = *const c_void;
pub(crate) type CFStringRef = *const c_void;
pub(crate) type CFDictionaryRef = *const c_void;
pub(crate) type CFMutableDictionaryRef = *mut c_void;
pub(crate) type CFArrayRef = *const c_void;
pub(crate) type CFNumberRef = *const c_void;
pub(crate) type CFBooleanRef = *const c_void;
pub(crate) type CFIndex = c_long;
pub(crate) type CMItemCount = c_long;

macro_rules! opaque {
    ($(#[$doc:meta])* $name:ident, $objc:literal) => {
        $(#[$doc])*
        #[repr(C)]
        pub(crate) struct $name {
            _private: [u8; 0],
        }

        // SAFETY: the same encoding as the SDK's `struct $objc *`.
        unsafe impl RefEncode for $name {
            const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct($objc, &[]));
        }
    };
}

opaque!(
    /// What a `CMSampleBufferRef` points at.
    OpaqueSampleBuffer,
    "opaqueCMSampleBuffer"
);
opaque!(
    /// What a `CMFormatDescriptionRef` points at.
    OpaqueFormatDescription,
    "opaqueCMFormatDescription"
);
opaque!(
    /// What a `CMBlockBufferRef` points at.
    OpaqueBlockBuffer,
    "OpaqueCMBlockBuffer"
);
opaque!(
    /// What a `CVPixelBufferRef` (a `CVImageBufferRef`) points at.
    OpaquePixelBuffer,
    "__CVBuffer"
);
opaque!(
    /// What a `VTCompressionSessionRef` points at.
    OpaqueCompressionSession,
    "OpaqueVTCompressionSession"
);
opaque!(
    /// What a `VTDecompressionSessionRef` points at.
    OpaqueDecompressionSession,
    "OpaqueVTDecompressionSession"
);

pub(crate) type CMSampleBufferRef = *mut OpaqueSampleBuffer;
pub(crate) type CMFormatDescriptionRef = *mut OpaqueFormatDescription;
pub(crate) type CMBlockBufferRef = *mut OpaqueBlockBuffer;
pub(crate) type CVPixelBufferRef = *mut OpaquePixelBuffer;
pub(crate) type VTCompressionSessionRef = *mut OpaqueCompressionSession;
pub(crate) type VTDecompressionSessionRef = *mut OpaqueDecompressionSession;

/// `CMTime`: `value / timescale` seconds.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CMTime {
    pub value: i64,
    pub timescale: i32,
    pub flags: u32,
    pub epoch: i64,
}

/// `kCMTimeFlags_Valid`.
pub(crate) const K_CMTIME_FLAGS_VALID: u32 = 1;

impl CMTime {
    /// A valid time of `value / timescale` seconds.
    pub(crate) fn new(value: i64, timescale: i32) -> Self {
        Self {
            value,
            timescale,
            flags: K_CMTIME_FLAGS_VALID,
            epoch: 0,
        }
    }

    /// `kCMTimeInvalid`.
    pub(crate) const INVALID: Self = Self {
        value: 0,
        timescale: 0,
        flags: 0,
        epoch: 0,
    };

    /// Whether `kCMTimeFlags_Valid` is set.
    pub(crate) fn is_valid(self) -> bool {
        self.flags & K_CMTIME_FLAGS_VALID != 0
    }
}

/// `CMSampleTimingInfo`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct CMSampleTimingInfo {
    pub duration: CMTime,
    pub presentation_time_stamp: CMTime,
    pub decode_time_stamp: CMTime,
}

/// `CMVideoDimensions`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CMVideoDimensions {
    pub width: i32,
    pub height: i32,
}

/// `CGAffineTransform` (64-bit, so `CGFloat` is `f64`), for `-[CALayer setAffineTransform:]`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CGAffineTransform {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub tx: f64,
    pub ty: f64,
}

// SAFETY: the SDK's encoding of `CGAffineTransform` on 64-bit targets.
unsafe impl Encode for CGAffineTransform {
    const ENCODING: Encoding = Encoding::Struct(
        "CGAffineTransform",
        &[
            f64::ENCODING,
            f64::ENCODING,
            f64::ENCODING,
            f64::ENCODING,
            f64::ENCODING,
            f64::ENCODING,
        ],
    );
}

/// `VTCompressionOutputCallback`.
pub(crate) type VTCompressionOutputCallback = unsafe extern "C" fn(
    output_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    sample_buffer: CMSampleBufferRef,
);

/// `VTDecompressionOutputCallback`.
pub(crate) type VTDecompressionOutputCallback = unsafe extern "C" fn(
    output_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OSStatus,
    info_flags: u32,
    image_buffer: CVPixelBufferRef,
    presentation_time_stamp: CMTime,
    presentation_duration: CMTime,
);

/// `VTDecompressionOutputCallbackRecord`.
#[repr(C)]
pub(crate) struct VTDecompressionOutputCallbackRecord {
    pub callback: Option<VTDecompressionOutputCallback>,
    pub ref_con: *mut c_void,
}

/// A function `dispatch_sync_f` runs.
pub(crate) type DispatchFunction = unsafe extern "C" fn(context: *mut c_void);

/// `kCMVideoCodecType_H264`.
pub(crate) const K_CMVIDEO_CODEC_TYPE_H264: u32 = u32::from_be_bytes(*b"avc1");
/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`: NV12, video range.
pub(crate) const K_CVPIXEL_FORMAT_NV12: u32 = u32::from_be_bytes(*b"420v");
/// `kCFNumberSInt32Type`.
pub(crate) const K_CFNUMBER_SINT32_TYPE: CFIndex = 3;
/// `kCFNumberFloat64Type`.
pub(crate) const K_CFNUMBER_FLOAT64_TYPE: CFIndex = 6;
/// `kVTEncodeInfo_FrameDropped`.
pub(crate) const K_VTENCODE_INFO_FRAME_DROPPED: u32 = 1 << 1;
/// `kCMBlockBufferAssureMemoryNowFlag`.
pub(crate) const K_CMBLOCK_BUFFER_ASSURE_MEMORY_NOW_FLAG: u32 = 1 << 0;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub(crate) static kCFBooleanTrue: CFBooleanRef;
    pub(crate) static kCFBooleanFalse: CFBooleanRef;
    pub(crate) static kCFTypeDictionaryKeyCallBacks: c_void;
    pub(crate) static kCFTypeDictionaryValueCallBacks: c_void;
    pub(crate) static kCFTypeArrayCallBacks: c_void;

    pub(crate) fn CFRelease(object: CFTypeRef);
    pub(crate) fn CFRetain(object: CFTypeRef) -> CFTypeRef;
    pub(crate) fn CFNumberCreate(
        allocator: CFAllocatorRef,
        number_type: CFIndex,
        value: *const c_void,
    ) -> CFNumberRef;
    pub(crate) fn CFDictionaryCreate(
        allocator: CFAllocatorRef,
        keys: *const *const c_void,
        values: *const *const c_void,
        count: CFIndex,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
    pub(crate) fn CFDictionarySetValue(
        dictionary: CFMutableDictionaryRef,
        key: *const c_void,
        value: *const c_void,
    );
    pub(crate) fn CFArrayCreate(
        allocator: CFAllocatorRef,
        values: *const *const c_void,
        count: CFIndex,
        callbacks: *const c_void,
    ) -> CFArrayRef;
    pub(crate) fn CFArrayGetCount(array: CFArrayRef) -> CFIndex;
    pub(crate) fn CFArrayGetValueAtIndex(array: CFArrayRef, index: CFIndex) -> *const c_void;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    pub(crate) static kCMSampleAttachmentKey_DisplayImmediately: CFStringRef;

    pub(crate) fn CMSampleBufferGetImageBuffer(buffer: CMSampleBufferRef) -> CVPixelBufferRef;
    pub(crate) fn CMSampleBufferGetDataBuffer(buffer: CMSampleBufferRef) -> CMBlockBufferRef;
    pub(crate) fn CMSampleBufferGetFormatDescription(
        buffer: CMSampleBufferRef,
    ) -> CMFormatDescriptionRef;
    pub(crate) fn CMSampleBufferGetPresentationTimeStamp(buffer: CMSampleBufferRef) -> CMTime;
    pub(crate) fn CMSampleBufferDataIsReady(buffer: CMSampleBufferRef) -> u8;
    pub(crate) fn CMSampleBufferGetSampleAttachmentsArray(
        buffer: CMSampleBufferRef,
        create_if_necessary: u8,
    ) -> CFArrayRef;
    pub(crate) fn CMSampleBufferCreateReady(
        allocator: CFAllocatorRef,
        data_buffer: CMBlockBufferRef,
        format_description: CMFormatDescriptionRef,
        num_samples: CMItemCount,
        num_sample_timing_entries: CMItemCount,
        sample_timing_array: *const CMSampleTimingInfo,
        num_sample_size_entries: CMItemCount,
        sample_size_array: *const usize,
        sample_buffer_out: *mut CMSampleBufferRef,
    ) -> OSStatus;
    pub(crate) fn CMBlockBufferGetDataLength(buffer: CMBlockBufferRef) -> usize;
    pub(crate) fn CMBlockBufferCopyDataBytes(
        source: CMBlockBufferRef,
        offset: usize,
        length: usize,
        destination: *mut c_void,
    ) -> OSStatus;
    pub(crate) fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: CFAllocatorRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFAllocatorRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    pub(crate) fn CMBlockBufferReplaceDataBytes(
        source: *const c_void,
        destination: CMBlockBufferRef,
        offset: usize,
        length: usize,
    ) -> OSStatus;
    pub(crate) fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        description: CMFormatDescriptionRef,
        index: usize,
        parameter_set_out: *mut *const u8,
        parameter_set_size_out: *mut usize,
        parameter_set_count_out: *mut usize,
        nal_unit_header_length_out: *mut i32,
    ) -> OSStatus;
    pub(crate) fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFAllocatorRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: i32,
        format_description_out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    pub(crate) fn CMVideoFormatDescriptionGetDimensions(
        description: CMFormatDescriptionRef,
    ) -> CMVideoDimensions;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    pub(crate) static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    pub(crate) static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;

    pub(crate) fn CVPixelBufferGetWidth(buffer: CVPixelBufferRef) -> usize;
    pub(crate) fn CVPixelBufferGetHeight(buffer: CVPixelBufferRef) -> usize;
    pub(crate) fn CVPixelBufferGetPixelFormatType(buffer: CVPixelBufferRef) -> u32;
    pub(crate) fn CVPixelBufferCreate(
        allocator: CFAllocatorRef,
        width: usize,
        height: usize,
        pixel_format_type: u32,
        attributes: CFDictionaryRef,
        pixel_buffer_out: *mut CVPixelBufferRef,
    ) -> CVReturn;
    pub(crate) fn CVPixelBufferLockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> CVReturn;
    pub(crate) fn CVPixelBufferUnlockBaseAddress(buffer: CVPixelBufferRef, flags: u64) -> CVReturn;
    pub(crate) fn CVPixelBufferGetBaseAddressOfPlane(
        buffer: CVPixelBufferRef,
        plane: usize,
    ) -> *mut c_void;
    pub(crate) fn CVPixelBufferGetBytesPerRowOfPlane(
        buffer: CVPixelBufferRef,
        plane: usize,
    ) -> usize;
    pub(crate) fn CVPixelBufferGetHeightOfPlane(buffer: CVPixelBufferRef, plane: usize) -> usize;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    pub(crate) static kVTCompressionPropertyKey_RealTime: CFStringRef;
    pub(crate) static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
    pub(crate) static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
    pub(crate) static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
    pub(crate) static kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: CFStringRef;
    pub(crate) static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
    pub(crate) static kVTCompressionPropertyKey_DataRateLimits: CFStringRef;
    pub(crate) static kVTCompressionPropertyKey_ExpectedFrameRate: CFStringRef;
    pub(crate) static kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel: CFStringRef;
    pub(crate) static kVTProfileLevel_H264_Baseline_AutoLevel: CFStringRef;
    pub(crate) static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;

    pub(crate) fn VTSessionSetProperty(
        session: *mut c_void,
        key: CFStringRef,
        value: CFTypeRef,
    ) -> OSStatus;
    pub(crate) fn VTCompressionSessionCreate(
        allocator: CFAllocatorRef,
        width: i32,
        height: i32,
        codec_type: u32,
        encoder_specification: CFDictionaryRef,
        source_image_buffer_attributes: CFDictionaryRef,
        compressed_data_allocator: CFAllocatorRef,
        output_callback: Option<VTCompressionOutputCallback>,
        output_callback_ref_con: *mut c_void,
        compression_session_out: *mut VTCompressionSessionRef,
    ) -> OSStatus;
    pub(crate) fn VTCompressionSessionPrepareToEncodeFrames(
        session: VTCompressionSessionRef,
    ) -> OSStatus;
    pub(crate) fn VTCompressionSessionEncodeFrame(
        session: VTCompressionSessionRef,
        image_buffer: CVPixelBufferRef,
        presentation_time_stamp: CMTime,
        duration: CMTime,
        frame_properties: CFDictionaryRef,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    pub(crate) fn VTCompressionSessionCompleteFrames(
        session: VTCompressionSessionRef,
        complete_until_presentation_time_stamp: CMTime,
    ) -> OSStatus;
    pub(crate) fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);

    pub(crate) fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        video_format_description: CMFormatDescriptionRef,
        video_decoder_specification: CFDictionaryRef,
        destination_image_buffer_attributes: CFDictionaryRef,
        output_callback: *const VTDecompressionOutputCallbackRecord,
        decompression_session_out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    pub(crate) fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample_buffer: CMSampleBufferRef,
        decode_flags: u32,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OSStatus;
    pub(crate) fn VTDecompressionSessionWaitForAsynchronousFrames(
        session: VTDecompressionSessionRef,
    ) -> OSStatus;
    pub(crate) fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
}

// AVFoundation and QuartzCore are reached through the Objective-C runtime (objc2); these empty
// blocks make sure the frameworks are linked so their classes can be looked up by name.
#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {
    pub(crate) static AVMediaTypeVideo: *const AnyObject;
    pub(crate) static AVCaptureDeviceTypeBuiltInWideAngleCamera: *const AnyObject;
    pub(crate) static AVCaptureSessionPreset352x288: *const AnyObject;
    pub(crate) static AVCaptureSessionPreset640x480: *const AnyObject;
    pub(crate) static AVCaptureSessionPreset1280x720: *const AnyObject;
    pub(crate) static AVCaptureSessionPreset1920x1080: *const AnyObject;
    pub(crate) static AVLayerVideoGravityResizeAspect: *const AnyObject;
    pub(crate) static AVLayerVideoGravityResizeAspectFill: *const AnyObject;
}

#[link(name = "QuartzCore", kind = "framework")]
unsafe extern "C" {}

// libdispatch, part of libSystem.
unsafe extern "C" {
    /// Returns a `dispatch_queue_t`, an Objective-C object under the Objective-C runtime.
    pub(crate) fn dispatch_queue_create(
        label: *const c_char,
        attr: *const c_void,
    ) -> *mut AnyObject;
    pub(crate) fn dispatch_sync_f(
        queue: *mut AnyObject,
        context: *mut c_void,
        work: DispatchFunction,
    );
    pub(crate) fn dispatch_release(object: *mut AnyObject);
}

/// A CoreFoundation object this code owns one reference to, released on drop.
#[derive(Debug)]
pub(crate) struct CfOwned<T>(*mut T);

impl<T> CfOwned<T> {
    /// Takes over a reference from a `Create`/`Copy` call; `None` if `ptr` is null.
    ///
    /// # Safety
    ///
    /// `ptr` is null or a CoreFoundation object with a reference the caller hands over.
    pub(crate) unsafe fn from_create(ptr: *mut T) -> Option<Self> {
        (!ptr.is_null()).then_some(Self(ptr))
    }

    /// Takes a new reference on a borrowed object; `None` if `ptr` is null.
    ///
    /// # Safety
    ///
    /// `ptr` is null or a live CoreFoundation object.
    pub(crate) unsafe fn retain(ptr: *mut T) -> Option<Self> {
        if ptr.is_null() {
            return None;
        }
        // SAFETY: a live object, per the caller.
        unsafe { CFRetain(ptr.cast_const().cast()) };
        Some(Self(ptr))
    }

    pub(crate) fn as_ptr(&self) -> *mut T {
        self.0
    }
}

impl<T> Drop for CfOwned<T> {
    fn drop(&mut self) {
        // SAFETY: `self` owns one reference to a live object (see the constructors).
        unsafe { CFRelease(self.0.cast_const().cast()) };
    }
}

// SAFETY: the CoreFoundation objects kept in a `CfOwned` (numbers, dictionaries, format
// descriptions, sample and pixel buffers, sessions) are reference counted atomically and are
// either immutable or documented as usable from any thread; the owners in this crate never use
// one from two threads at once.
unsafe impl<T> Send for CfOwned<T> {}
// SAFETY: as above; shared references only read immutable objects.
unsafe impl<T> Sync for CfOwned<T> {}

/// A `CFNumber` holding a 32-bit integer.
pub(crate) fn cf_i32(value: i32) -> Result<CfOwned<c_void>, VideoError> {
    // SAFETY: `value` is a live i32, the type the number type names.
    let number = unsafe {
        CFNumberCreate(
            std::ptr::null(),
            K_CFNUMBER_SINT32_TYPE,
            (&raw const value).cast(),
        )
    };
    // SAFETY: a `Create` call hands over its reference.
    unsafe { CfOwned::from_create(number.cast_mut()) }.ok_or_else(|| backend("CFNumberCreate"))
}

/// A `CFNumber` holding a double.
pub(crate) fn cf_f64(value: f64) -> Result<CfOwned<c_void>, VideoError> {
    // SAFETY: `value` is a live f64, the type the number type names.
    let number = unsafe {
        CFNumberCreate(
            std::ptr::null(),
            K_CFNUMBER_FLOAT64_TYPE,
            (&raw const value).cast(),
        )
    };
    // SAFETY: a `Create` call hands over its reference.
    unsafe { CfOwned::from_create(number.cast_mut()) }.ok_or_else(|| backend("CFNumberCreate"))
}

/// An immutable `CFDictionary` of CF objects, which retains its keys and values.
pub(crate) fn cf_dictionary(
    pairs: &[(*const c_void, *const c_void)],
) -> Result<CfOwned<c_void>, VideoError> {
    let keys: Vec<*const c_void> = pairs.iter().map(|pair| pair.0).collect();
    let values: Vec<*const c_void> = pairs.iter().map(|pair| pair.1).collect();
    // SAFETY: `keys` and `values` hold `pairs.len()` CF objects each, the callbacks are the
    // standard CF ones, which retain them.
    let dictionary = unsafe {
        CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            pairs.len() as CFIndex,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    };
    // SAFETY: a `Create` call hands over its reference.
    unsafe { CfOwned::from_create(dictionary.cast_mut()) }
        .ok_or_else(|| backend("CFDictionaryCreate"))
}

/// An immutable `CFArray` of CF objects, which retains them.
pub(crate) fn cf_array(values: &[*const c_void]) -> Result<CfOwned<c_void>, VideoError> {
    // SAFETY: `values` holds CF objects; the standard callbacks retain them.
    let array = unsafe {
        CFArrayCreate(
            std::ptr::null(),
            values.as_ptr(),
            values.len() as CFIndex,
            &raw const kCFTypeArrayCallBacks,
        )
    };
    // SAFETY: a `Create` call hands over its reference.
    unsafe { CfOwned::from_create(array.cast_mut()) }.ok_or_else(|| backend("CFArrayCreate"))
}

/// A failed platform call, named for [`VideoError::Backend`].
pub(crate) fn backend(call: &str) -> VideoError {
    VideoError::Backend(format!("{call} failed"))
}

/// `Ok` for a zero status, else the call and the code as [`VideoError::Backend`].
pub(crate) fn check(status: OSStatus, call: &str) -> Result<(), VideoError> {
    if status == 0 {
        Ok(())
    } else {
        Err(VideoError::Backend(format!(
            "{call} failed: OSStatus {status}"
        )))
    }
}

const _: () = assert!(size_of::<CMTime>() == 24);
const _: () = assert!(size_of::<CMSampleTimingInfo>() == 72);
const _: () = assert!(size_of::<CMVideoDimensions>() == 8);
const _: () = assert!(size_of::<CGAffineTransform>() == 48);
const _: () = assert!(size_of::<VTDecompressionOutputCallbackRecord>() == 16);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_objects_are_created_and_released() {
        let number = cf_i32(800_000).unwrap();
        let seconds = cf_f64(1.0).unwrap();
        let array =
            cf_array(&[number.as_ptr().cast_const(), seconds.as_ptr().cast_const()]).unwrap();
        // SAFETY: a live array.
        assert_eq!(unsafe { CFArrayGetCount(array.as_ptr().cast_const()) }, 2);
        // SAFETY: the key is a live CFString and the value a live CFBoolean.
        let key = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
        let dictionary = cf_dictionary(&[(key, unsafe { kCFBooleanTrue })]).unwrap();
        assert!(!dictionary.as_ptr().is_null());
    }

    #[test]
    fn statuses_name_the_call() {
        assert_eq!(check(0, "X"), Ok(()));
        assert_eq!(
            check(-12902, "VTSessionSetProperty"),
            Err(VideoError::Backend(
                "VTSessionSetProperty failed: OSStatus -12902".to_owned()
            ))
        );
    }

    #[test]
    fn times_are_valid_only_with_the_flag() {
        assert!(CMTime::new(1, 30).is_valid());
        assert!(!CMTime::INVALID.is_valid());
    }
}
