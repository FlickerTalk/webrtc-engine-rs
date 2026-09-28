//! iOS backend on the `VoiceProcessingIO` audio unit.

use std::ffi::c_void;

use super::{AudioError, SAMPLE_RATE, StreamFormat};

// The few AudioToolbox / CoreAudioTypes declarations this backend needs, copied from the iOS SDK
// headers (`AUComponent.h`, `AudioUnitProperties.h`, `CoreAudioBaseTypes.h`). The layout tests
// below pin their sizes.

/// A Core Audio result code: zero is success.
pub(crate) type OSStatus = i32;

const K_AUDIO_FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1 << 0;
const K_AUDIO_FORMAT_FLAG_IS_BIG_ENDIAN: u32 = 1 << 1;
const K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER: u32 = 1 << 2;
const K_AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 1 << 3;
const K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED: u32 = 1 << 5;

/// `AudioStreamBasicDescription`: the layout of a PCM stream.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct AudioStreamBasicDescription {
    pub sample_rate: f64,
    pub format_id: u32,
    pub format_flags: u32,
    pub bytes_per_packet: u32,
    pub frames_per_packet: u32,
    pub bytes_per_frame: u32,
    pub channels_per_frame: u32,
    pub bits_per_channel: u32,
    pub reserved: u32,
}

/// `AudioBuffer`: one buffer of samples, owned by whoever filled in `data`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct AudioBuffer {
    pub number_channels: u32,
    pub data_byte_size: u32,
    pub data: *mut c_void,
}

/// `AudioBufferList`: in C a variable-length array; the callbacks here see one buffer, since
/// the formats they accept are interleaved.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct AudioBufferList {
    pub number_buffers: u32,
    pub buffers: [AudioBuffer; 1],
}

/// The sample type of a stream the adapters can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleKind {
    I16,
    F32,
}

impl SampleKind {
    /// Bytes in one sample.
    pub(crate) fn bytes(self) -> u32 {
        match self {
            Self::I16 => 2,
            Self::F32 => 4,
        }
    }
}

/// The stream this backend asks the unit for, on both sides: 48 kHz mono, packed and
/// native-endian, in `kind` samples.
pub(crate) fn requested_format(kind: SampleKind) -> AudioStreamBasicDescription {
    let flags = match kind {
        SampleKind::I16 => K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER,
        SampleKind::F32 => K_AUDIO_FORMAT_FLAG_IS_FLOAT,
    };
    let bytes = kind.bytes();
    AudioStreamBasicDescription {
        sample_rate: f64::from(SAMPLE_RATE),
        format_id: K_AUDIO_FORMAT_LINEAR_PCM,
        format_flags: flags | K_AUDIO_FORMAT_FLAG_IS_PACKED,
        bytes_per_packet: bytes,
        frames_per_packet: 1,
        bytes_per_frame: bytes,
        channels_per_frame: 1,
        bits_per_channel: 8 * bytes,
        reserved: 0,
    }
}

/// A stream the adapters can take, as the unit reports it after initialising.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeviceFormat {
    pub stream: StreamFormat,
    pub kind: SampleKind,
}

/// Reads a stream description back into something the adapters take: linear PCM in
/// native-endian i16 or f32, interleaved (or mono), at any whole sample rate.
pub(crate) fn device_format(
    description: &AudioStreamBasicDescription,
) -> Result<DeviceFormat, AudioError> {
    let invalid = || AudioError::InvalidFormat {
        // `as` saturates, and NaN becomes 0: only for the message.
        sample_rate: description.sample_rate as u32,
        channels: u16::try_from(description.channels_per_frame).unwrap_or(u16::MAX),
    };
    let flags = description.format_flags;
    let kind = match (
        flags & (K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER),
        description.bits_per_channel,
    ) {
        (K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER, 16) => SampleKind::I16,
        (K_AUDIO_FORMAT_FLAG_IS_FLOAT, 32) => SampleKind::F32,
        _ => return Err(invalid()),
    };
    let channels = u16::try_from(description.channels_per_frame).map_err(|_| invalid())?;
    let rate = description.sample_rate;
    let whole_rate = rate.is_finite() && rate >= 1.0 && rate <= f64::from(u32::MAX);
    let interleaved = flags & K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED == 0 || channels == 1;
    let packed = description.bytes_per_frame == u32::from(channels) * kind.bytes();
    if description.format_id != K_AUDIO_FORMAT_LINEAR_PCM
        || flags & K_AUDIO_FORMAT_FLAG_IS_BIG_ENDIAN != 0
        || channels == 0
        || !whole_rate
        || rate.fract() != 0.0
        || !interleaved
        || !packed
    {
        return Err(invalid());
    }
    Ok(DeviceFormat {
        stream: StreamFormat {
            sample_rate: rate as u32,
            channels,
        },
        kind,
    })
}

/// Turns the result of the Core Audio function `call` into a `Result`.
pub(crate) fn check(status: OSStatus, call: &'static str) -> Result<(), AudioError> {
    if status == 0 {
        return Ok(());
    }
    // Many Core Audio errors are four ASCII characters packed into the number, like '!pri'.
    let code = status.to_be_bytes();
    let reason = if code.iter().all(|byte| byte.is_ascii_graphic()) {
        let text: String = code.iter().map(|&byte| char::from(byte)).collect();
        format!("{call} failed: OSStatus {status} ('{text}')")
    } else {
        format!("{call} failed: OSStatus {status}")
    };
    Err(AudioError::Backend(reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn the_c_types_have_the_sdk_layout() {
        assert_eq!(size_of::<AudioStreamBasicDescription>(), 40);
        assert_eq!(size_of::<AudioBuffer>(), 16);
        assert_eq!(size_of::<AudioBufferList>(), 24);
    }

    #[test]
    fn asks_for_48000_mono_i16() {
        assert_eq!(
            requested_format(SampleKind::I16),
            AudioStreamBasicDescription {
                sample_rate: f64::from(SAMPLE_RATE),
                format_id: K_AUDIO_FORMAT_LINEAR_PCM,
                format_flags: K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER | K_AUDIO_FORMAT_FLAG_IS_PACKED,
                bytes_per_packet: 2,
                frames_per_packet: 1,
                bytes_per_frame: 2,
                channels_per_frame: 1,
                bits_per_channel: 16,
                reserved: 0,
            }
        );
    }

    #[test]
    fn or_48000_mono_f32() {
        let format = requested_format(SampleKind::F32);
        assert_eq!(
            format.format_flags,
            K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED
        );
        assert_eq!(format.sample_rate, f64::from(SAMPLE_RATE));
        assert_eq!(format.channels_per_frame, 1);
        assert_eq!(format.bits_per_channel, 32);
        assert_eq!(format.bytes_per_frame, 4);
        assert_eq!(format.bytes_per_packet, 4);
        assert_eq!(format.frames_per_packet, 1);
    }

    fn format(sample_rate: u32, channels: u16, kind: SampleKind) -> DeviceFormat {
        DeviceFormat {
            stream: StreamFormat {
                sample_rate,
                channels,
            },
            kind,
        }
    }

    #[test]
    fn what_was_asked_for_reads_back_as_is() {
        assert_eq!(
            device_format(&requested_format(SampleKind::I16)),
            Ok(format(SAMPLE_RATE, 1, SampleKind::I16))
        );
        assert_eq!(
            device_format(&requested_format(SampleKind::F32)),
            Ok(format(SAMPLE_RATE, 1, SampleKind::F32))
        );
    }

    #[test]
    fn another_rate_and_interleaved_stereo_are_taken() {
        let mut description = requested_format(SampleKind::F32);
        description.sample_rate = 44_100.0;
        description.channels_per_frame = 2;
        description.bytes_per_frame = 8;
        description.bytes_per_packet = 8;
        assert_eq!(
            device_format(&description),
            Ok(format(44_100, 2, SampleKind::F32))
        );
    }

    #[test]
    fn mono_marked_non_interleaved_is_the_same_stream() {
        let mut description = requested_format(SampleKind::I16);
        description.format_flags |= K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED;
        assert_eq!(
            device_format(&description),
            Ok(format(SAMPLE_RATE, 1, SampleKind::I16))
        );
    }

    #[test]
    fn streams_the_adapters_cannot_take_are_rejected() {
        let i16_mono = requested_format(SampleKind::I16);
        let rejected = [
            // Non-interleaved stereo comes as two buffers.
            AudioStreamBasicDescription {
                format_flags: i16_mono.format_flags | K_AUDIO_FORMAT_FLAG_IS_NON_INTERLEAVED,
                channels_per_frame: 2,
                ..i16_mono
            },
            AudioStreamBasicDescription {
                format_flags: i16_mono.format_flags | K_AUDIO_FORMAT_FLAG_IS_BIG_ENDIAN,
                ..i16_mono
            },
            // 8.24 fixed point, the old canonical unit format.
            AudioStreamBasicDescription {
                bits_per_channel: 32,
                bytes_per_frame: 4,
                bytes_per_packet: 4,
                ..i16_mono
            },
            AudioStreamBasicDescription {
                format_id: u32::from_be_bytes(*b"aac "),
                ..i16_mono
            },
            AudioStreamBasicDescription {
                format_flags: K_AUDIO_FORMAT_FLAG_IS_PACKED,
                ..i16_mono
            },
            // Padded frames: the buffer arithmetic assumes packed samples.
            AudioStreamBasicDescription {
                bytes_per_frame: 4,
                bytes_per_packet: 4,
                ..i16_mono
            },
            AudioStreamBasicDescription {
                channels_per_frame: 70_000,
                bytes_per_frame: 140_000,
                bytes_per_packet: 140_000,
                ..i16_mono
            },
            AudioStreamBasicDescription {
                channels_per_frame: 0,
                ..i16_mono
            },
            AudioStreamBasicDescription {
                sample_rate: 0.0,
                ..i16_mono
            },
            AudioStreamBasicDescription {
                sample_rate: 44_100.5,
                ..i16_mono
            },
            AudioStreamBasicDescription {
                sample_rate: f64::NAN,
                ..i16_mono
            },
        ];
        for description in rejected {
            assert!(
                device_format(&description).is_err(),
                "accepted {description:?}"
            );
        }
    }

    #[test]
    fn a_zero_status_is_success() {
        assert_eq!(check(0, "AudioUnitInitialize"), Ok(()));
    }

    #[test]
    fn a_failing_status_names_the_call_and_the_code() {
        let error = check(-10868, "AudioUnitSetProperty").unwrap_err();
        assert_eq!(
            error,
            AudioError::Backend("AudioUnitSetProperty failed: OSStatus -10868".to_owned())
        );
    }

    #[test]
    fn a_four_char_status_is_also_shown_as_text() {
        // '!pri': the session refused to activate or to record (AVAudioSession.ErrorCode).
        let error = check(0x2170_7269, "AudioOutputUnitStart").unwrap_err();
        assert_eq!(
            error,
            AudioError::Backend(
                "AudioOutputUnitStart failed: OSStatus 561017449 ('!pri')".to_owned()
            )
        );
    }
}
