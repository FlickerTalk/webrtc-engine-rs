//! Sample formats that device callbacks deliver, and the device stream layout.

/// A device sample type. Conversions go through `f32` in the range -1.0..=1.0.
pub trait Sample: Copy + Send + 'static {
    /// The same level as an `f32` in -1.0..=1.0.
    fn to_f32(self) -> f32;
    /// The nearest sample for `value`, clipped to the type's range.
    fn from_f32(value: f32) -> Self;
}

// Scaling by 32768 both ways makes the i16 -> f32 -> i16 trip exact.
const I16_SCALE: f32 = 32768.0;

impl Sample for i16 {
    fn to_f32(self) -> f32 {
        f32::from(self) / I16_SCALE
    }

    fn from_f32(value: f32) -> Self {
        // `as` saturates, and NaN becomes 0.
        (value * I16_SCALE).round() as i16
    }
}

impl Sample for f32 {
    fn to_f32(self) -> f32 {
        self
    }

    fn from_f32(value: f32) -> Self {
        value.clamp(-1.0, 1.0)
    }
}

/// The layout of a device stream: interleaved samples at some rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i16_survives_a_trip_through_f32() {
        for sample in [i16::MIN, -12345, -1, 0, 1, 12345, i16::MAX] {
            assert_eq!(i16::from_f32(sample.to_f32()), sample);
        }
    }

    #[test]
    fn i16_full_scale_maps_to_one() {
        assert_eq!(i16::MIN.to_f32(), -1.0);
        assert!((i16::MAX.to_f32() - 1.0).abs() < 1e-4);
    }

    #[test]
    fn out_of_range_levels_are_clipped() {
        assert_eq!(i16::from_f32(1.5), i16::MAX);
        assert_eq!(i16::from_f32(-1.5), i16::MIN);
        assert_eq!(f32::from_f32(1.5), 1.0);
        assert_eq!(f32::from_f32(-1.5), -1.0);
    }

    #[test]
    fn f32_levels_pass_through() {
        assert_eq!(0.25f32.to_f32(), 0.25);
        assert_eq!(f32::from_f32(-0.5), -0.5);
    }
}
