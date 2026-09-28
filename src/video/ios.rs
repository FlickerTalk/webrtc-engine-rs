//! iOS video: camera capture with AVFoundation and hardware H.264 with VideoToolbox.

#[cfg(target_os = "ios")]
mod encoder;
#[cfg(target_os = "ios")]
mod ffi;
mod h264;
mod keyframes;
mod orientation;
mod settings;

pub use keyframes::KeyframeRequests;
pub use orientation::DeviceOrientation;

#[cfg(test)]
mod tests {}
