//! Which way up the phone is, and what that means for the frames sent and shown.

use crate::video::{Facing, Rotation};

/// The phone's physical orientation: `UIDeviceOrientation`, with the same raw values.
///
/// The app's Swift code passes it to
/// [`CameraSource::set_device_orientation`](super::CameraSource::set_device_orientation) from its
/// `UIDevice.orientationDidChangeNotification` observer (UIKit is main-thread only, so the engine
/// does not read `UIDevice` itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeviceOrientation {
    #[default]
    Unknown,
    /// Upright, home button or bar at the bottom.
    Portrait,
    PortraitUpsideDown,
    /// On its side, the top of the phone to the left.
    LandscapeLeft,
    /// On its side, the top of the phone to the right.
    LandscapeRight,
    FaceUp,
    FaceDown,
}

impl DeviceOrientation {
    /// From `UIDeviceOrientation.rawValue`; anything unknown is [`DeviceOrientation::Unknown`].
    pub fn from_raw(raw: isize) -> Self {
        match raw {
            1 => Self::Portrait,
            2 => Self::PortraitUpsideDown,
            3 => Self::LandscapeLeft,
            4 => Self::LandscapeRight,
            5 => Self::FaceUp,
            6 => Self::FaceDown,
            _ => Self::Unknown,
        }
    }
}

/// The [`Rotation`] a receiver applies to a frame captured with the phone held in `orientation`
/// by the camera facing `facing`, or `None` when the orientation says nothing about it (flat on
/// a table, unknown) and the last one stays.
///
/// The sensor's native orientation is landscape with the top of the phone to the left for the
/// back camera, and the front camera is mounted the other way round. The pixels are neither
/// rotated nor mirrored (the same table as libwebrtc's `RTCCameraVideoCapturer`).
pub(crate) fn capture_rotation(orientation: DeviceOrientation, facing: Facing) -> Option<Rotation> {
    let front = facing == Facing::Front;
    match orientation {
        DeviceOrientation::Portrait => Some(Rotation::Deg90),
        DeviceOrientation::PortraitUpsideDown => Some(Rotation::Deg270),
        DeviceOrientation::LandscapeLeft if front => Some(Rotation::Deg180),
        DeviceOrientation::LandscapeLeft => Some(Rotation::Deg0),
        DeviceOrientation::LandscapeRight if front => Some(Rotation::Deg0),
        DeviceOrientation::LandscapeRight => Some(Rotation::Deg180),
        DeviceOrientation::FaceUp | DeviceOrientation::FaceDown | DeviceOrientation::Unknown => {
            None
        }
    }
}

/// The affine transform `(a, b, c, d)` of a layer showing a frame turned clockwise by
/// `rotation`, in UIKit's coordinates (y down, so a positive angle turns clockwise on screen).
/// Exact at the quarter turns, with no rounding noise.
pub(crate) fn layer_transform(rotation: Rotation) -> [f64; 4] {
    // (cos θ, sin θ, -sin θ, cos θ), written out so the zeros are exact.
    match rotation {
        Rotation::Deg0 => [1.0, 0.0, 0.0, 1.0],
        Rotation::Deg90 => [0.0, 1.0, -1.0, 0.0],
        Rotation::Deg180 => [-1.0, 0.0, 0.0, -1.0],
        Rotation::Deg270 => [0.0, -1.0, 1.0, 0.0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_values_are_uikits() {
        let all = [
            (0, DeviceOrientation::Unknown),
            (1, DeviceOrientation::Portrait),
            (2, DeviceOrientation::PortraitUpsideDown),
            (3, DeviceOrientation::LandscapeLeft),
            (4, DeviceOrientation::LandscapeRight),
            (5, DeviceOrientation::FaceUp),
            (6, DeviceOrientation::FaceDown),
        ];
        for (raw, orientation) in all {
            assert_eq!(DeviceOrientation::from_raw(raw), orientation);
        }
        assert_eq!(DeviceOrientation::from_raw(7), DeviceOrientation::Unknown);
        assert_eq!(DeviceOrientation::from_raw(-1), DeviceOrientation::Unknown);
    }

    #[test]
    fn upright_phones_send_frames_turned_a_quarter() {
        for facing in [Facing::Front, Facing::Back] {
            assert_eq!(
                capture_rotation(DeviceOrientation::Portrait, facing),
                Some(Rotation::Deg90)
            );
            assert_eq!(
                capture_rotation(DeviceOrientation::PortraitUpsideDown, facing),
                Some(Rotation::Deg270)
            );
        }
    }

    #[test]
    fn in_landscape_the_two_cameras_are_upside_down_from_each_other() {
        use DeviceOrientation::{LandscapeLeft, LandscapeRight};
        assert_eq!(
            capture_rotation(LandscapeLeft, Facing::Back),
            Some(Rotation::Deg0)
        );
        assert_eq!(
            capture_rotation(LandscapeLeft, Facing::Front),
            Some(Rotation::Deg180)
        );
        assert_eq!(
            capture_rotation(LandscapeRight, Facing::Back),
            Some(Rotation::Deg180)
        );
        assert_eq!(
            capture_rotation(LandscapeRight, Facing::Front),
            Some(Rotation::Deg0)
        );
    }

    #[test]
    fn flat_or_unknown_keeps_the_last_rotation() {
        use DeviceOrientation::{FaceDown, FaceUp, Unknown};
        for orientation in [FaceUp, FaceDown, Unknown] {
            assert_eq!(capture_rotation(orientation, Facing::Front), None);
        }
    }

    #[test]
    fn layer_transforms_turn_clockwise_by_exact_quarters() {
        assert_eq!(layer_transform(Rotation::Deg0), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(layer_transform(Rotation::Deg90), [0.0, 1.0, -1.0, 0.0]);
        assert_eq!(layer_transform(Rotation::Deg180), [-1.0, 0.0, 0.0, -1.0]);
        assert_eq!(layer_transform(Rotation::Deg270), [0.0, -1.0, 1.0, 0.0]);
    }
}
