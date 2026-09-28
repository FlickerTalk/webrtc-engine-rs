//! Sample rate conversion between a device and the engine's 48 kHz.
//!
//! Linear interpolation with an exact integer phase: no allocation, no drift over a long call,
//! and cheap enough for any audio thread. It is a fallback, since the mobile backends ask the OS
//! for 48 kHz and the OS converts with its own filters; for voice it is good enough.

use super::AudioError;

/// Converts a mono stream from one sample rate to another, one sample at a time.
///
/// Use one instance per stream and per direction, and only one of `push` or `pull` on it.
pub struct Resampler {
    from: u64,
    to: u64,
    /// Position of the next output between `previous` and `current`, in units of 1/`to` of an
    /// input sample.
    phase: u64,
    previous: f32,
    current: f32,
}

impl Resampler {
    /// A resampler from `from` Hz to `to` Hz.
    pub fn new(from: u32, to: u32) -> Result<Self, AudioError> {
        if from == 0 || to == 0 {
            return Err(AudioError::InvalidFormat {
                sample_rate: from.min(to),
                channels: 1,
            });
        }
        Ok(Self {
            from: u64::from(from),
            to: u64::from(to),
            // One whole input sample ahead: the first input is taken before the first output.
            phase: u64::from(to),
            previous: 0.0,
            current: 0.0,
        })
    }

    /// Feeds one input sample and hands every output sample it completes to `emit`.
    pub fn push(&mut self, input: f32, mut emit: impl FnMut(f32)) {
        self.advance(input);
        while self.phase < self.to {
            emit(self.output());
        }
    }

    /// Produces one output sample, taking as many input samples from `source` as it needs.
    pub fn pull(&mut self, mut source: impl FnMut() -> f32) -> f32 {
        while self.phase >= self.to {
            self.advance(source());
        }
        self.output()
    }

    fn advance(&mut self, input: f32) {
        self.phase -= self.to;
        self.previous = self.current;
        self.current = input;
    }

    fn output(&mut self) -> f32 {
        let fraction = (self.phase as f64 / self.to as f64) as f32;
        self.phase += self.from;
        self.previous + (self.current - self.previous) * fraction
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn sine(frequency: f32, rate: u32, seconds: f32) -> Vec<f32> {
        let count = (rate as f32 * seconds) as usize;
        (0..count)
            .map(|n| 0.8 * (TAU * frequency * n as f32 / rate as f32).sin())
            .collect()
    }

    /// Frequency from the upward zero crossings, skipping the start-up of the resampler.
    fn frequency_of(signal: &[f32], rate: u32) -> f32 {
        let body = &signal[100..];
        let crossings = body.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        crossings as f32 * rate as f32 / body.len() as f32
    }

    fn push_all(resampler: &mut Resampler, input: &[f32]) -> Vec<f32> {
        let mut output = Vec::new();
        for &sample in input {
            resampler.push(sample, |out| output.push(out));
        }
        output
    }

    #[test]
    fn a_zero_rate_is_rejected() {
        assert!(Resampler::new(0, 48_000).is_err());
        assert!(Resampler::new(48_000, 0).is_err());
    }

    #[test]
    fn pushing_one_second_gives_one_second_at_the_new_rate() {
        let mut resampler = Resampler::new(44_100, 48_000).unwrap();
        let output = push_all(&mut resampler, &sine(440.0, 44_100, 1.0));
        assert_eq!(output.len(), 48_000);
    }

    #[test]
    fn a_sine_keeps_its_frequency_going_up_from_44100() {
        let mut resampler = Resampler::new(44_100, 48_000).unwrap();
        let output = push_all(&mut resampler, &sine(1000.0, 44_100, 1.0));
        let frequency = frequency_of(&output, 48_000);
        assert!((frequency - 1000.0).abs() < 5.0, "{frequency} Hz");
    }

    #[test]
    fn a_sine_keeps_its_frequency_going_up_from_16000() {
        let mut resampler = Resampler::new(16_000, 48_000).unwrap();
        let output = push_all(&mut resampler, &sine(700.0, 16_000, 1.0));
        let frequency = frequency_of(&output, 48_000);
        assert!((frequency - 700.0).abs() < 5.0, "{frequency} Hz");
    }

    #[test]
    fn a_sine_keeps_its_frequency_and_level_when_pulled_down_to_44100() {
        let input = sine(1000.0, 48_000, 1.0);
        let mut samples = input.iter().copied();
        let mut resampler = Resampler::new(48_000, 44_100).unwrap();
        let output: Vec<f32> = (0..44_000)
            .map(|_| resampler.pull(|| samples.next().unwrap_or(0.0)))
            .collect();
        let frequency = frequency_of(&output, 44_100);
        assert!((frequency - 1000.0).abs() < 5.0, "{frequency} Hz");
        let peak = output.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        assert!((peak - 0.8).abs() < 0.02, "peak {peak}");
    }

    #[test]
    fn the_same_rate_passes_samples_through() {
        let mut resampler = Resampler::new(48_000, 48_000).unwrap();
        let input = [0.1, 0.2, 0.3, 0.4];
        let output = push_all(&mut resampler, &input);
        assert_eq!(output.len(), 4);
        // One sample of delay: the interpolation starts from silence.
        assert_eq!(output[1..], input[..3]);
    }
}
