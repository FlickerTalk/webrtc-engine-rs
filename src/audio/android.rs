//! Android backend on AAudio.

use super::AudioError;
use super::format::StreamFormat;

/// `aaudio_format_t` values (`<aaudio/AAudio.h>`).
const FORMAT_PCM_I16: i32 = 1;
const FORMAT_PCM_FLOAT: i32 = 2;

/// The sample type of an open stream's buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleKind {
    I16,
    F32,
}

/// The format AAudio actually granted, as the adapters need it. The engine asks for 48 kHz mono
/// i16, but AAudio may open something else; the adapters convert whatever it is.
fn granted_format(
    sample_rate: i32,
    channels: i32,
    format: i32,
) -> Result<(StreamFormat, SampleKind), AudioError> {
    let invalid = || AudioError::InvalidFormat {
        sample_rate: u32::try_from(sample_rate).unwrap_or(0),
        channels: u16::try_from(channels).unwrap_or(0),
    };
    let rate = u32::try_from(sample_rate)
        .ok()
        .filter(|&rate| rate > 0)
        .ok_or_else(invalid)?;
    let channels = u16::try_from(channels)
        .ok()
        .filter(|&channels| channels > 0)
        .ok_or_else(invalid)?;
    let kind = match format {
        FORMAT_PCM_I16 => SampleKind::I16,
        FORMAT_PCM_FLOAT => SampleKind::F32,
        other => {
            return Err(AudioError::Backend(format!(
                "AAudio opened sample format {other}, not i16 or float"
            )));
        }
    };
    let format = StreamFormat {
        sample_rate: rate,
        channels,
    };
    Ok((format, kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SAMPLE_RATE;

    #[test]
    fn the_engine_format_is_taken_as_it_is() {
        let granted = granted_format(48_000, 1, FORMAT_PCM_I16).unwrap();
        assert_eq!(
            granted,
            (
                StreamFormat {
                    sample_rate: SAMPLE_RATE,
                    channels: 1
                },
                SampleKind::I16
            )
        );
    }

    #[test]
    fn float_and_other_rates_and_channels_are_taken_for_the_adapters_to_convert() {
        let granted = granted_format(44_100, 2, FORMAT_PCM_FLOAT).unwrap();
        assert_eq!(
            granted,
            (
                StreamFormat {
                    sample_rate: 44_100,
                    channels: 2
                },
                SampleKind::F32
            )
        );
    }

    #[test]
    fn sample_types_the_adapters_cannot_read_are_rejected() {
        // Unspecified, 24-bit packed and 32-bit integer.
        for format in [0, 3, 4] {
            assert!(matches!(
                granted_format(48_000, 1, format),
                Err(AudioError::Backend(_))
            ));
        }
    }

    #[test]
    fn a_rate_or_channel_count_that_makes_no_sense_is_rejected() {
        for (rate, channels) in [
            (0, 1),
            (-48_000, 1),
            (48_000, 0),
            (48_000, -1),
            (48_000, 70_000),
        ] {
            assert!(matches!(
                granted_format(rate, channels, FORMAT_PCM_I16),
                Err(AudioError::InvalidFormat { .. })
            ));
        }
    }
}
