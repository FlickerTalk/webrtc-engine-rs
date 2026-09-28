//! iOS video: camera capture with AVFoundation and hardware H.264 with VideoToolbox.

mod h264;
mod keyframes;
mod orientation;
mod settings;

pub use keyframes::KeyframeRequests;
pub use orientation::DeviceOrientation;

#[cfg(test)]
mod tests {}
