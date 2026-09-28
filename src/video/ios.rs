//! iOS video: camera capture with AVFoundation and hardware H.264 with VideoToolbox.

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

#[cfg(test)]
mod tests {}
