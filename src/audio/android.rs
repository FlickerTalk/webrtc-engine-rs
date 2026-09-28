//! Android backend on AAudio.

use std::ffi::c_void;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};

use super::format::StreamFormat;
use super::{AudioError, CaptureAdapter, Counter, PlayoutAdapter};

/// `aaudio_format_t` values (`<aaudio/AAudio.h>`).
const FORMAT_PCM_I16: i32 = 1;
const FORMAT_PCM_FLOAT: i32 = 2;
/// `aaudio_data_callback_result_t`: keep calling.
const CALLBACK_RESULT_CONTINUE: i32 = 0;

/// `AAudioStream`, opaque.
#[repr(C)]
struct AAudioStream {
    _private: [u8; 0],
}

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

/// What a stream's data callback does with each buffer, in the format the stream opened with.
enum Processor {
    Capture {
        adapter: CaptureAdapter,
        kind: SampleKind,
        channels: usize,
    },
    Playout {
        adapter: PlayoutAdapter,
        kind: SampleKind,
        channels: usize,
    },
}

impl Processor {
    /// Hands one callback buffer to the adapter.
    ///
    /// # Safety
    ///
    /// `audio` is null or points to `frames` frames of interleaved samples of this processor's
    /// kind and channel count, valid and not aliased for the duration of the call.
    unsafe fn process(&mut self, audio: *mut c_void, frames: i32) {
        let Ok(frames) = usize::try_from(frames) else {
            return;
        };
        if audio.is_null() || frames == 0 {
            return;
        }
        match self {
            Self::Capture {
                adapter,
                kind,
                channels,
            } => {
                let len = frames.saturating_mul(*channels);
                // SAFETY: the caller guarantees `len` samples of `kind` behind `audio`.
                match kind {
                    SampleKind::I16 => {
                        adapter.push(unsafe { slice::from_raw_parts(audio.cast::<i16>(), len) })
                    }
                    SampleKind::F32 => {
                        adapter.push(unsafe { slice::from_raw_parts(audio.cast::<f32>(), len) })
                    }
                }
            }
            Self::Playout {
                adapter,
                kind,
                channels,
            } => {
                let len = frames.saturating_mul(*channels);
                // SAFETY: as above, and nobody else reads or writes the buffer during the call.
                match kind {
                    SampleKind::I16 => {
                        adapter.fill(unsafe { slice::from_raw_parts_mut(audio.cast::<i16>(), len) })
                    }
                    SampleKind::F32 => {
                        adapter.fill(unsafe { slice::from_raw_parts_mut(audio.cast::<f32>(), len) })
                    }
                }
            }
        }
    }
}

/// The data callback's state. AAudio holds a raw pointer to it from opening the stream until
/// closing it; it stays empty until the stream's format is known and it gets a [`Processor`].
#[derive(Default)]
struct CallbackSlot(Option<Processor>);

/// What the error callbacks of both streams report to the backend. Atomics only: the error
/// callback runs on an AAudio thread and must not block.
#[derive(Default)]
struct StreamHealth {
    errors: Counter,
    restart_needed: AtomicBool,
}

impl StreamHealth {
    /// Notes an error: AAudio only reports errors that end the stream, such as a disconnect.
    fn report(&self) {
        self.errors.add(1);
        self.restart_needed.store(true, Ordering::Release);
    }

    /// Whether a stream died since the last call, clearing the flag.
    fn take_restart(&self) -> bool {
        self.restart_needed.swap(false, Ordering::AcqRel)
    }
}

/// `AAudioStream_dataCallback`. Runs on AAudio's real-time thread: no locks, no allocation.
///
/// # Safety
///
/// `user_data` is null or the [`CallbackSlot`] registered with this stream, used by nobody
/// else while the stream runs; `audio` holds `frames` frames in the stream's format.
unsafe extern "C" fn on_data(
    _stream: *mut AAudioStream,
    user_data: *mut c_void,
    audio: *mut c_void,
    frames: i32,
) -> i32 {
    // SAFETY: the caller guarantees `user_data` is null or our slot, borrowed by nobody else.
    if let Some(CallbackSlot(Some(processor))) =
        unsafe { user_data.cast::<CallbackSlot>().as_mut() }
    {
        // SAFETY: AAudio hands over `frames` frames in the format the processor was made for.
        unsafe { processor.process(audio, frames) };
    }
    CALLBACK_RESULT_CONTINUE
}

/// `AAudioStream_errorCallback`. Must not stop or close the stream (AAudio forbids it from this
/// thread): it only raises the flag the owner polls.
///
/// # Safety
///
/// `user_data` is null or points to a [`StreamHealth`] that outlives the stream.
unsafe extern "C" fn on_error(_stream: *mut AAudioStream, user_data: *mut c_void, _error: i32) {
    // SAFETY: the caller guarantees `user_data` is null or a live `StreamHealth`; it is only
    // read through a shared reference, and it only holds atomics.
    if let Some(health) = unsafe { user_data.cast::<StreamHealth>().as_ref() } {
        health.report();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SAMPLE_RATE;
    use crate::audio::ring::{RingConsumer, ring};
    use std::ptr;

    const ENGINE_MONO: StreamFormat = StreamFormat {
        sample_rate: SAMPLE_RATE,
        channels: 1,
    };

    fn drain(consumer: &mut RingConsumer) -> Vec<i16> {
        let mut all = vec![0; consumer.len()];
        consumer.pop(&mut all);
        all
    }

    fn capture_slot(format: StreamFormat, kind: SampleKind) -> (CallbackSlot, RingConsumer) {
        let (producer, consumer) = ring(64);
        let processor = Processor::Capture {
            adapter: CaptureAdapter::new(format, producer).unwrap(),
            kind,
            channels: usize::from(format.channels),
        };
        (CallbackSlot(Some(processor)), consumer)
    }

    fn playout_slot(format: StreamFormat, kind: SampleKind, queued: &[i16]) -> CallbackSlot {
        let (mut producer, consumer) = ring(64);
        producer.push(queued);
        let processor = Processor::Playout {
            adapter: PlayoutAdapter::new(format, consumer).unwrap(),
            kind,
            channels: usize::from(format.channels),
        };
        CallbackSlot(Some(processor))
    }

    /// Calls the data callback as AAudio would, with `slot` as user data.
    fn run_callback<S>(slot: &mut CallbackSlot, buffer: &mut [S], frames: i32) -> i32 {
        // SAFETY: the slot and the buffer outlive the call, and `frames` fits the buffer.
        unsafe {
            on_data(
                ptr::null_mut(),
                ptr::from_mut(slot).cast(),
                buffer.as_mut_ptr().cast(),
                frames,
            )
        }
    }

    #[test]
    fn the_capture_callback_copies_i16_mono_into_the_ring() {
        let (mut slot, mut consumer) = capture_slot(ENGINE_MONO, SampleKind::I16);
        let mut buffer = [10i16, -20, 30];
        assert_eq!(
            run_callback(&mut slot, &mut buffer, 3),
            CALLBACK_RESULT_CONTINUE
        );
        assert_eq!(drain(&mut consumer), [10, -20, 30]);
    }

    #[test]
    fn the_capture_callback_reads_float_stereo_frames() {
        let stereo = StreamFormat {
            sample_rate: SAMPLE_RATE,
            channels: 2,
        };
        let (mut slot, mut consumer) = capture_slot(stereo, SampleKind::F32);
        let mut buffer = [0.5f32, 0.0, -0.25, -0.25];
        run_callback(&mut slot, &mut buffer, 2);
        assert_eq!(drain(&mut consumer), [8192, -8192]);
    }

    #[test]
    fn the_playout_callback_fills_i16_with_silence_after_the_ring_runs_dry() {
        let mut slot = playout_slot(ENGINE_MONO, SampleKind::I16, &[1, 2]);
        let mut buffer = [7i16; 4];
        assert_eq!(
            run_callback(&mut slot, &mut buffer, 4),
            CALLBACK_RESULT_CONTINUE
        );
        assert_eq!(buffer, [1, 2, 0, 0]);
    }

    #[test]
    fn the_playout_callback_fills_float_stereo_frames() {
        let stereo = StreamFormat {
            sample_rate: SAMPLE_RATE,
            channels: 2,
        };
        let mut slot = playout_slot(stereo, SampleKind::F32, &[16384, -8192]);
        let mut buffer = [9.0f32; 4];
        run_callback(&mut slot, &mut buffer, 2);
        assert_eq!(buffer, [0.5, 0.5, -0.25, -0.25]);
    }

    #[test]
    fn the_callback_only_touches_the_frames_it_is_given() {
        let mut slot = playout_slot(ENGINE_MONO, SampleKind::I16, &[1, 2, 3]);
        let mut buffer = [7i16; 3];
        run_callback(&mut slot, &mut buffer, 2);
        assert_eq!(buffer, [1, 2, 7]);
    }

    #[test]
    fn the_callback_ignores_missing_or_empty_buffers() {
        let mut slot = playout_slot(ENGINE_MONO, SampleKind::I16, &[1, 2]);
        let mut buffer = [7i16; 2];
        for frames in [0, -5] {
            assert_eq!(
                run_callback(&mut slot, &mut buffer, frames),
                CALLBACK_RESULT_CONTINUE
            );
        }
        // SAFETY: a null buffer and a null slot are what the callback must survive.
        let results = unsafe {
            [
                on_data(
                    ptr::null_mut(),
                    ptr::from_mut(&mut slot).cast(),
                    ptr::null_mut(),
                    2,
                ),
                on_data(
                    ptr::null_mut(),
                    ptr::null_mut(),
                    buffer.as_mut_ptr().cast(),
                    2,
                ),
            ]
        };
        assert_eq!(results, [CALLBACK_RESULT_CONTINUE; 2]);
        assert_eq!(buffer, [7, 7]);

        // Nothing was consumed: the queued samples are still there.
        run_callback(&mut slot, &mut buffer, 2);
        assert_eq!(buffer, [1, 2]);
    }

    #[test]
    fn an_empty_slot_leaves_the_buffer_alone() {
        let mut slot = CallbackSlot::default();
        let mut buffer = [7i16; 2];
        assert_eq!(
            run_callback(&mut slot, &mut buffer, 2),
            CALLBACK_RESULT_CONTINUE
        );
        assert_eq!(buffer, [7, 7]);
    }

    #[test]
    fn the_error_callback_counts_the_error_and_asks_once_for_a_restart() {
        let health = StreamHealth::default();
        let errors = health.errors.clone();
        assert!(!health.take_restart());

        // SAFETY: `health` outlives both calls; a null one must be survived.
        unsafe {
            on_error(
                ptr::null_mut(),
                ptr::from_ref(&health).cast_mut().cast(),
                -899,
            );
            on_error(ptr::null_mut(), ptr::null_mut(), -899);
        }
        assert_eq!(errors.get(), 1);
        assert!(health.take_restart());
        assert!(!health.take_restart());
    }

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
