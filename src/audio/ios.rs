//! iOS backend on the `VoiceProcessingIO` audio unit.

use std::ffi::c_void;

use super::{AudioError, CaptureAdapter, Counter, RingProducer, SAMPLE_RATE, StreamFormat};
use std::mem::size_of;

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

/// Microphone samples in the unit's sample type, allocated once when the unit starts.
enum SampleBuffer {
    I16(Box<[i16]>),
    F32(Box<[f32]>),
}

impl SampleBuffer {
    fn new(kind: SampleKind, samples: usize) -> Self {
        match kind {
            SampleKind::I16 => Self::I16(vec![0; samples].into_boxed_slice()),
            SampleKind::F32 => Self::F32(vec![0.0; samples].into_boxed_slice()),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::I16(samples) => samples.len(),
            Self::F32(samples) => samples.len(),
        }
    }

    fn sample_bytes(&self) -> usize {
        match self {
            Self::I16(_) => size_of::<i16>(),
            Self::F32(_) => size_of::<f32>(),
        }
    }

    fn as_mut_ptr(&mut self) -> *mut c_void {
        match self {
            Self::I16(samples) => samples.as_mut_ptr().cast(),
            Self::F32(samples) => samples.as_mut_ptr().cast(),
        }
    }

    /// Hands the first `count` samples to `adapter`.
    fn push_to(&self, adapter: &mut CaptureAdapter, count: usize) {
        match self {
            Self::I16(samples) => adapter.push(&samples[..count.min(samples.len())]),
            Self::F32(samples) => adapter.push(&samples[..count.min(samples.len())]),
        }
    }
}

/// The capture half of the unit's input callback: renders the microphone into a pre-allocated
/// buffer and hands it to a [`CaptureAdapter`].
pub(crate) struct Capture {
    adapter: CaptureAdapter,
    buffer: SampleBuffer,
    channels: u32,
    errors: Counter,
}

impl Capture {
    /// A capture for `format` that can take callbacks of up to `max_frames` frames.
    pub(crate) fn new(
        format: DeviceFormat,
        max_frames: u32,
        producer: RingProducer,
    ) -> Result<Self, AudioError> {
        let channels = u32::from(format.stream.channels);
        // The list tells the unit the buffer's size in bytes as a u32.
        let samples = max_frames
            .checked_mul(channels)
            .filter(|samples| samples.checked_mul(format.kind.bytes()).is_some())
            .ok_or_else(|| {
                AudioError::Backend(format!("{max_frames} frames per callback is too many"))
            })?;
        Ok(Self {
            adapter: CaptureAdapter::new(format.stream, producer)?,
            buffer: SampleBuffer::new(format.kind, samples as usize),
            channels,
            errors: Counter::default(),
        })
    }

    /// How many callbacks failed to render or were larger than the buffer.
    pub(crate) fn errors(&self) -> Counter {
        self.errors.clone()
    }

    /// How many engine samples were dropped because the ring was full.
    pub(crate) fn dropped(&self) -> Counter {
        self.adapter.dropped()
    }

    /// Runs on the audio thread for every input callback: `render` (`AudioUnitRender` on the
    /// unit) fills `frames` frames into the buffer the list points at, and they go to the ring.
    /// No allocation, no lock, no system call besides `render` itself.
    pub(crate) fn process(
        &mut self,
        frames: u32,
        render: impl FnOnce(&mut AudioBufferList) -> OSStatus,
    ) {
        let samples = (frames as usize).saturating_mul(self.channels as usize);
        if samples > self.buffer.len() {
            self.errors.add(1);
            return;
        }
        let sample_bytes = self.buffer.sample_bytes();
        let data = self.buffer.as_mut_ptr();
        let mut list = AudioBufferList {
            number_buffers: 1,
            buffers: [AudioBuffer {
                number_channels: self.channels,
                // At most the buffer's size, which `new` checked fits in a u32.
                data_byte_size: (samples * sample_bytes) as u32,
                data,
            }],
        };
        if render(&mut list) != 0 || list.buffers[0].data != data {
            self.errors.add(1);
            return;
        }
        let rendered = (list.buffers[0].data_byte_size as usize / sample_bytes).min(samples);
        self.buffer.push_to(&mut self.adapter, rendered);
    }
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
    use crate::audio::RingConsumer;
    use crate::audio::ring::ring;
    use std::mem::size_of_val;

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

    fn drain(consumer: &mut RingConsumer) -> Vec<i16> {
        let mut all = vec![0; consumer.len()];
        consumer.pop(&mut all);
        all
    }

    /// What a render callback sees of the list it is handed.
    #[derive(Debug, PartialEq)]
    struct Seen {
        buffers: u32,
        channels: u32,
        bytes: u32,
    }

    fn seen(list: &AudioBufferList) -> Seen {
        Seen {
            buffers: list.number_buffers,
            channels: list.buffers[0].number_channels,
            bytes: list.buffers[0].data_byte_size,
        }
    }

    /// Stands in for `AudioUnitRender`: writes `samples` where the list points.
    fn write<S: Copy>(list: &mut AudioBufferList, samples: &[S]) {
        let buffer = &mut list.buffers[0];
        assert!(size_of_val(samples) <= buffer.data_byte_size as usize);
        // SAFETY: the list points at a buffer of `data_byte_size` bytes of `S`, checked above.
        let target =
            unsafe { std::slice::from_raw_parts_mut(buffer.data.cast::<S>(), samples.len()) };
        target.copy_from_slice(samples);
    }

    #[test]
    fn rendered_i16_samples_reach_the_ring_exactly() {
        let (producer, mut consumer) = ring(4096);
        let mut capture =
            Capture::new(format(SAMPLE_RATE, 1, SampleKind::I16), 512, producer).unwrap();
        let samples: Vec<i16> = (0..480).map(|n| n * 50 - 12000).collect();

        let mut seen_by_render = None;
        capture.process(480, |list| {
            seen_by_render = Some(seen(list));
            write(list, &samples);
            0
        });

        assert_eq!(
            seen_by_render,
            Some(Seen {
                buffers: 1,
                channels: 1,
                bytes: 960
            })
        );
        assert_eq!(drain(&mut consumer), samples);
        assert_eq!(capture.errors().get(), 0);
    }

    #[test]
    fn rendered_f32_stereo_is_mixed_to_mono_i16() {
        let (producer, mut consumer) = ring(4096);
        let mut capture =
            Capture::new(format(SAMPLE_RATE, 2, SampleKind::F32), 512, producer).unwrap();

        let mut seen_by_render = None;
        capture.process(2, |list| {
            seen_by_render = Some(seen(list));
            write(list, &[0.5f32, 0.5, -0.25, -0.25]);
            0
        });

        assert_eq!(
            seen_by_render,
            Some(Seen {
                buffers: 1,
                channels: 2,
                bytes: 16
            })
        );
        assert_eq!(drain(&mut consumer), [16384, -8192]);
    }

    #[test]
    fn a_failed_render_pushes_nothing_and_is_counted() {
        let (producer, mut consumer) = ring(4096);
        let mut capture =
            Capture::new(format(SAMPLE_RATE, 1, SampleKind::I16), 512, producer).unwrap();
        capture.process(256, |list| {
            write(list, &[1i16; 256]);
            -10863
        });
        assert!(drain(&mut consumer).is_empty());
        assert_eq!(capture.errors().get(), 1);
    }

    #[test]
    fn a_callback_larger_than_the_buffer_is_skipped_and_counted() {
        let (producer, mut consumer) = ring(4096);
        let mut capture =
            Capture::new(format(SAMPLE_RATE, 1, SampleKind::I16), 512, producer).unwrap();
        let mut rendered = false;
        capture.process(513, |_| {
            rendered = true;
            0
        });
        assert!(!rendered);
        assert!(drain(&mut consumer).is_empty());
        assert_eq!(capture.errors().get(), 1);

        capture.process(512, |list| {
            write(list, &[7i16; 512]);
            0
        });
        assert_eq!(drain(&mut consumer), [7; 512]);
    }

    #[test]
    fn a_render_that_points_the_list_elsewhere_is_counted_and_ignored() {
        let (producer, mut consumer) = ring(4096);
        let mut capture =
            Capture::new(format(SAMPLE_RATE, 1, SampleKind::I16), 512, producer).unwrap();
        let mut elsewhere = [5i16; 16];
        capture.process(16, |list| {
            list.buffers[0].data = elsewhere.as_mut_ptr().cast();
            0
        });
        assert!(drain(&mut consumer).is_empty());
        assert_eq!(capture.errors().get(), 1);
    }

    #[test]
    fn samples_that_find_the_ring_full_are_counted_as_dropped() {
        let (producer, _consumer) = ring(8);
        let mut capture =
            Capture::new(format(SAMPLE_RATE, 1, SampleKind::I16), 512, producer).unwrap();
        let dropped = capture.dropped();
        capture.process(10, |list| {
            write(list, &[1i16; 10]);
            0
        });
        assert_eq!(dropped.get(), 2);
    }

    #[test]
    fn only_the_bytes_the_render_reports_are_pushed() {
        let (producer, mut consumer) = ring(4096);
        let mut capture =
            Capture::new(format(SAMPLE_RATE, 1, SampleKind::I16), 512, producer).unwrap();
        capture.process(4, |list| {
            write(list, &[1i16, 2, 3, 4]);
            list.buffers[0].data_byte_size = 4;
            0
        });
        assert_eq!(drain(&mut consumer), [1, 2]);
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
