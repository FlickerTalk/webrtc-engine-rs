//! The device's end of the rings: whatever the callback delivers, in the engine's format.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::format::{Sample, StreamFormat};
use super::resample::Resampler;
use super::ring::{RingConsumer, RingProducer};
use super::{AudioError, SAMPLE_RATE};

/// A count that the audio thread bumps and any other thread can read.
#[derive(Debug, Clone, Default)]
pub struct Counter(Arc<AtomicU64>);

impl Counter {
    /// The current count.
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    pub(crate) fn add(&self, amount: u64) {
        self.0.fetch_add(amount, Ordering::Relaxed);
    }
}

/// Checks the format and builds the resampler it needs, if any. At the engine's own rate there
/// is none, so a device that already delivers 48 kHz mono i16 is copied bit for bit.
fn resampler_for(
    format: StreamFormat,
    from: u32,
    to: u32,
) -> Result<Option<Resampler>, AudioError> {
    if format.channels == 0 {
        return Err(AudioError::InvalidFormat {
            sample_rate: format.sample_rate,
            channels: format.channels,
        });
    }
    if from == to {
        return Ok(None);
    }
    Resampler::new(from, to).map(Some)
}

/// Turns a capture callback's buffers into 48 kHz mono i16 samples in a ring.
///
/// Everything it does on the audio thread is allocation-free and lock-free.
pub struct CaptureAdapter {
    channels: usize,
    resampler: Option<Resampler>,
    producer: RingProducer,
    dropped: Counter,
}

impl CaptureAdapter {
    /// An adapter for a device that captures in `format`, writing to `producer`.
    pub fn new(format: StreamFormat, producer: RingProducer) -> Result<Self, AudioError> {
        Ok(Self {
            channels: usize::from(format.channels),
            resampler: resampler_for(format, format.sample_rate, SAMPLE_RATE)?,
            producer,
            dropped: Counter::default(),
        })
    }

    /// Takes one callback's interleaved samples. Samples that find the ring full are dropped.
    pub fn push<S: Sample>(&mut self, interleaved: &[S]) {
        let Self {
            channels,
            resampler,
            producer,
            dropped,
        } = self;
        let mut lost = 0u64;
        let mut write = |level: f32| {
            if producer.push(&[i16::from_f32(level)]) == 0 {
                lost += 1;
            }
        };
        for device_frame in interleaved.chunks_exact(*channels) {
            let sum: f32 = device_frame.iter().map(|sample| sample.to_f32()).sum();
            let mono = sum / *channels as f32;
            match resampler {
                Some(resampler) => resampler.push(mono, &mut write),
                None => write(mono),
            }
        }
        if lost > 0 {
            dropped.add(lost);
        }
    }

    /// How many engine samples were dropped because the ring was full.
    pub fn dropped(&self) -> Counter {
        self.dropped.clone()
    }
}

/// Fills a playout callback's buffers from 48 kHz mono i16 samples in a ring.
///
/// Everything it does on the audio thread is allocation-free and lock-free.
pub struct PlayoutAdapter {
    channels: usize,
    resampler: Option<Resampler>,
    consumer: RingConsumer,
    underruns: Counter,
}

impl PlayoutAdapter {
    /// An adapter for a device that plays in `format`, reading from `consumer`.
    pub fn new(format: StreamFormat, consumer: RingConsumer) -> Result<Self, AudioError> {
        Ok(Self {
            channels: usize::from(format.channels),
            resampler: resampler_for(format, SAMPLE_RATE, format.sample_rate)?,
            consumer,
            underruns: Counter::default(),
        })
    }

    /// Fills one callback's interleaved buffer, with silence where the ring runs dry.
    pub fn fill<S: Sample>(&mut self, interleaved: &mut [S]) {
        let Self {
            channels,
            resampler,
            consumer,
            underruns,
        } = self;
        let mut starved = false;
        let mut read = || {
            let mut sample = [0i16];
            if consumer.pop(&mut sample) == 1 {
                sample[0].to_f32()
            } else {
                starved = true;
                0.0
            }
        };
        for device_frame in interleaved.chunks_exact_mut(*channels) {
            let level = match resampler {
                Some(resampler) => resampler.pull(&mut read),
                None => read(),
            };
            device_frame.fill(S::from_f32(level));
        }
        if starved {
            underruns.add(1);
        }
    }

    /// How many callbacks ran out of samples and played silence.
    pub fn underruns(&self) -> Counter {
        self.underruns.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::ring::ring;
    use crate::{FRAME_SAMPLES, SAMPLE_RATE};
    use std::f32::consts::TAU;

    const ENGINE_MONO: StreamFormat = StreamFormat {
        sample_rate: SAMPLE_RATE,
        channels: 1,
    };

    fn stereo(sample_rate: u32) -> StreamFormat {
        StreamFormat {
            sample_rate,
            channels: 2,
        }
    }

    fn drain(consumer: &mut RingConsumer) -> Vec<i16> {
        let mut all = vec![0; consumer.len()];
        consumer.pop(&mut all);
        all
    }

    fn frequency_of(signal: &[f32], rate: u32) -> f32 {
        let body = &signal[100..];
        let crossings = body
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        crossings as f32 * rate as f32 / body.len() as f32
    }

    #[test]
    fn zero_channels_is_rejected() {
        let format = StreamFormat {
            sample_rate: SAMPLE_RATE,
            channels: 0,
        };
        let (producer, consumer) = ring(16);
        assert!(CaptureAdapter::new(format, producer).is_err());
        assert!(PlayoutAdapter::new(format, consumer).is_err());
    }

    #[test]
    fn capture_in_the_engine_format_is_copied_exactly_whatever_the_buffer_sizes() {
        let (producer, mut consumer) = ring(4 * FRAME_SAMPLES);
        let mut adapter = CaptureAdapter::new(ENGINE_MONO, producer).unwrap();
        let input: Vec<i16> = (0..FRAME_SAMPLES as i16).map(|n| n * 30).collect();

        for chunk in [&input[..100], &input[100..433], &input[433..]] {
            adapter.push(chunk);
        }
        assert_eq!(drain(&mut consumer), input);
    }

    #[test]
    fn capture_mixes_stereo_down_to_mono() {
        let (producer, mut consumer) = ring(16);
        let mut adapter = CaptureAdapter::new(stereo(SAMPLE_RATE), producer).unwrap();
        adapter.push(&[0.5f32, 0.0, -0.25, -0.25]);
        assert_eq!(drain(&mut consumer), [8192, -8192]);
    }

    #[test]
    fn capture_at_44100_stereo_gives_48000_mono_with_the_same_tone() {
        let (producer, mut consumer) = ring(64 * FRAME_SAMPLES);
        let mut adapter = CaptureAdapter::new(stereo(44_100), producer).unwrap();
        let tone: Vec<f32> = (0..44_100)
            .flat_map(|n| {
                let level = 0.5 * (TAU * 1000.0 * n as f32 / 44_100.0).sin();
                [level, level]
            })
            .collect();

        for chunk in tone.chunks(2 * 441) {
            adapter.push(chunk);
        }
        let captured: Vec<f32> = drain(&mut consumer).iter().map(|s| s.to_f32()).collect();
        assert_eq!(captured.len(), 50 * FRAME_SAMPLES);
        let frequency = frequency_of(&captured, SAMPLE_RATE);
        assert!((frequency - 1000.0).abs() < 5.0, "{frequency} Hz");
    }

    #[test]
    fn capture_counts_what_a_full_ring_drops() {
        let (producer, mut consumer) = ring(8);
        let mut adapter = CaptureAdapter::new(ENGINE_MONO, producer).unwrap();
        let dropped = adapter.dropped();
        adapter.push(&[1i16; 11]);
        assert_eq!(dropped.get(), 3);
        assert_eq!(drain(&mut consumer).len(), 8);
    }

    #[test]
    fn playout_in_the_engine_format_is_copied_exactly() {
        let (mut producer, consumer) = ring(4 * FRAME_SAMPLES);
        let mut adapter = PlayoutAdapter::new(ENGINE_MONO, consumer).unwrap();
        let frame: Vec<i16> = (0..FRAME_SAMPLES as i16).map(|n| n * 30).collect();
        producer.push(&frame);

        let mut played = vec![0i16; FRAME_SAMPLES];
        let (first, rest) = played.split_at_mut(333);
        adapter.fill(first);
        adapter.fill(rest);
        assert_eq!(played, frame);
        assert_eq!(adapter.underruns().get(), 0);
    }

    #[test]
    fn playout_copies_mono_to_every_channel() {
        let (mut producer, consumer) = ring(16);
        let mut adapter = PlayoutAdapter::new(stereo(SAMPLE_RATE), consumer).unwrap();
        producer.push(&[16384, -8192]);
        let mut played = [0.0f32; 4];
        adapter.fill(&mut played);
        assert_eq!(played, [0.5, 0.5, -0.25, -0.25]);
    }

    #[test]
    fn playout_plays_silence_and_counts_an_underrun_when_the_ring_runs_dry() {
        let (mut producer, consumer) = ring(16);
        let mut adapter = PlayoutAdapter::new(ENGINE_MONO, consumer).unwrap();
        let underruns = adapter.underruns();
        producer.push(&[100, 200]);

        let mut played = [7i16; 5];
        adapter.fill(&mut played);
        assert_eq!(played, [100, 200, 0, 0, 0]);
        assert_eq!(underruns.get(), 1);

        adapter.fill(&mut played);
        assert_eq!(played, [0; 5]);
        assert_eq!(underruns.get(), 2);

        producer.push(&[1, 2, 3, 4, 5]);
        adapter.fill(&mut played);
        assert_eq!(played, [1, 2, 3, 4, 5]);
        assert_eq!(underruns.get(), 2);
    }

    #[test]
    fn playout_at_44100_keeps_the_tone() {
        let (mut producer, consumer) = ring(64 * FRAME_SAMPLES);
        let mut adapter = PlayoutAdapter::new(stereo(44_100), consumer).unwrap();
        let tone: Vec<i16> = (0..SAMPLE_RATE)
            .map(|n| i16::from_f32(0.5 * (TAU * 1000.0 * n as f32 / SAMPLE_RATE as f32).sin()))
            .collect();
        producer.push(&tone);

        let mut played = vec![0.0f32; 2 * 44_000];
        for chunk in played.chunks_mut(2 * 512) {
            adapter.fill(chunk);
        }
        let left: Vec<f32> = played.iter().step_by(2).copied().collect();
        let frequency = frequency_of(&left, 44_100);
        assert!((frequency - 1000.0).abs() < 5.0, "{frequency} Hz");
        assert_eq!(adapter.underruns().get(), 0);
    }
}
