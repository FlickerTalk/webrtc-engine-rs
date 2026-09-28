//! iOS video: camera capture with AVFoundation, hardware H.264 with VideoToolbox, and remote
//! video on an `AVSampleBufferDisplayLayer` (iOS only; the pure parts are tested on the host).
//!
//! - [`CameraSource`] ([`VideoSource`](super::VideoSource)): `AVCaptureSession` with the
//!   built-in wide-angle camera (front or back) → `AVCaptureVideoDataOutput` (NV12, serial
//!   queue) → `VTCompressionSession` (Constrained Baseline, real time, no B-frames, a keyframe at
//!   least every 2 s, average bitrate with a 1.5× per-second hard limit) → Annex-B with SPS/PPS in
//!   front of every IDR → [`FrameSender`](super::FrameSender).
//! - [`DisplaySink`] ([`VideoSink`](super::VideoSink)): Annex-B → `CMVideoFormatDescription`
//!   from the SPS/PPS → AVCC `CMSampleBuffer` marked `DisplayImmediately` → enqueued to the
//!   layer, which decodes in hardware.
//!
//! # Contract with the app (Swift)
//!
//! **Layers.** `CameraSource::preview_layer` (an `AVCaptureVideoPreviewLayer`, local preview,
//! the front camera mirrored) and `DisplaySink::layer` (an `AVSampleBufferDisplayLayer`, remote
//! video) are `CALayer`s owned by the Rust object, valid until it is dropped. The Swift code takes
//! them with `Unmanaged<CALayer>.fromOpaque(ptr).takeUnretainedValue()`, adds them as sublayers
//! of its views **on the main thread**, lays them out in `layoutSubviews`, and removes them before
//! the Rust object is dropped. The remote layer is rotated with its transform (see
//! `DisplaySink::rotation`): for a quarter turn the app sets `bounds` to the view's size with
//! width and height swapped and `position` to the view's centre, never `frame`.
//!
//! **Camera permission.** The app declares `NSCameraUsageDescription` and asks with
//! `AVCaptureDevice.requestAccess(for: .video)` before starting a video call. This code never
//! asks: [`start`](super::VideoSource::start) returns
//! [`VideoError::PermissionDenied`](super::VideoError::PermissionDenied) unless access is
//! already granted (not yet asked counts as denied).
//!
//! **Orientation.** UIKit is main-thread only, so the source does not read `UIDevice`: the app
//! calls `CameraSource::set_device_orientation` from its
//! `UIDevice.orientationDidChangeNotification` observer (after
//! `beginGeneratingDeviceOrientationNotifications()`), with
//! [`DeviceOrientation::from_raw`]`(UIDevice.current.orientation.rawValue)`. It starts as
//! portrait. The frames keep the sensor's pixels; the rotation travels in each
//! [`EncodedFrame`](super::EncodedFrame).
//!
//! **Keyframes from the display side.** `DisplaySink::keyframe_requests` gives a
//! [`KeyframeRequests`] the pipeline polls to send an RTCP PLI (see there).
//!
//! **Audio session.** Nothing here touches `AVAudioSession`: it belongs to the app and CallKit.
//!
//! **Background and locked screen.** iOS stops the camera when the app leaves the foreground (no
//! frames arrive; the session resumes by itself). The display layer is flushed by the system in
//! the background; the next [`push`](super::VideoSink::push) notices, flushes and asks for a
//! keyframe. Neither has been tried on a phone yet.
//!
//! # FFI
//!
//! VideoToolbox, CoreMedia, CoreVideo, CoreFoundation and libdispatch are C APIs, declared by
//! hand in `ffi.rs` like the audio backend's AudioToolbox. AVFoundation and Core Animation are
//! Objective-C and go through `objc2` alone (the runtime crate, an iOS-only dependency): it
//! defines the capture delegate class, manages retain counts and checks every message's type
//! encoding in debug builds. The generated framework crates (`objc2-av-foundation` and its
//! family) would bring half a dozen crates and dozens of feature flags for some thirty messages.

#[cfg(target_os = "ios")]
mod camera;
#[cfg(target_os = "ios")]
mod display;
#[cfg(target_os = "ios")]
mod encoder;
#[cfg(target_os = "ios")]
mod ffi;
mod h264;
mod keyframes;
mod orientation;
mod settings;

#[cfg(target_os = "ios")]
pub use camera::CameraSource;
#[cfg(target_os = "ios")]
pub use display::DisplaySink;
pub use keyframes::KeyframeRequests;
pub use orientation::DeviceOrientation;
