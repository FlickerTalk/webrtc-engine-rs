use crate::video::{Facing, Rotation};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeviceOrientation {
    #[default]
    Unknown = 0,
    Portrait = 1,
    PortraitUpsideDown = 2,
    /// On its side, top of the phone to the left.
    LandscapeLeft = 3,
    /// On its side, top of the phone to the right.
    LandscapeRight = 4,
    FaceUp = 5,
    FaceDown = 6,
}

impl DeviceOrientation {
    /// From `UIDeviceOrientation.rawValue`.
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

/// The back sensor is landscape with the phone's top to the left, and the front one is mounted
/// the other way round. Same table as libwebrtc's `RTCCameraVideoCapturer`.
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

/// UIKit coordinates, y down, so a positive angle turns clockwise on screen.
pub(crate) fn layer_transform(rotation: Rotation) -> [f64; 4] {
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
