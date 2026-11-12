use super::AudioError;

/// Allocation-free, so it can run on the audio thread. Use one per stream and direction, and
/// only `push` or only `pull` on it.
pub struct Resampler {
    from: u64,
    to: u64,
    /// In 1/`to` of an input sample, between `previous` and `current`.
    phase: u64,
    previous: f32,
    current: f32,
}

impl Resampler {
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
            // One input sample ahead, so the first input is taken before the first output.
            phase: u64::from(to),
            previous: 0.0,
            current: 0.0,
        })
    }

    pub fn push(&mut self, input: f32, mut emit: impl FnMut(f32)) {
        self.advance(input);
        while self.phase < self.to {
            emit(self.output());
        }
    }

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

    fn frequency_of(signal: &[f32], rate: u32) -> f32 {
        let body = &signal[100..];
        let crossings = body
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
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
        assert_eq!(output[1..], input[..3]);
    }
}
