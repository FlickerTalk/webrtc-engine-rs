//! Desktop backend on `cpal`, for trying the engine on a computer.
//!
//! No echo cancellation here: use headphones.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample, SupportedStreamConfig, SupportedStreamConfigRange};

use super::format::{Sample, StreamFormat};
use super::{
    AudioBackend, AudioError, CaptureAdapter, Counter, DeviceIo, PlayoutAdapter, SAMPLE_RATE,
};

/// Picks the device configuration closest to the engine's format: the rate nearest to 48 kHz
/// first, since resampling costs more than mixing channels, then the fewest channels.
fn choose_config(
    ranges: impl IntoIterator<Item = SupportedStreamConfigRange>,
) -> Option<SupportedStreamConfig> {
    ranges
        .into_iter()
        .filter(|range| {
            matches!(range.sample_format(), SampleFormat::F32 | SampleFormat::I16)
                && range.channels() > 0
                && range.min_sample_rate() <= range.max_sample_rate()
        })
        .map(|range| {
            let rate = SAMPLE_RATE.clamp(range.min_sample_rate(), range.max_sample_rate());
            range.with_sample_rate(rate)
        })
        .min_by_key(|config| {
            (
                config.sample_rate().abs_diff(SAMPLE_RATE),
                config.channels(),
            )
        })
}

fn backend_error(error: impl std::fmt::Display) -> AudioError {
    AudioError::Backend(error.to_string())
}

fn stream_format(config: &SupportedStreamConfig) -> StreamFormat {
    StreamFormat {
        sample_rate: config.sample_rate(),
        channels: config.channels(),
    }
}

fn no_supported_config() -> AudioError {
    AudioError::Backend("the device offers no f32 or i16 configuration".to_owned())
}

fn build_input<S: Sample + SizedSample>(
    device: &cpal::Device,
    config: &SupportedStreamConfig,
    mut adapter: CaptureAdapter,
    errors: Counter,
) -> Result<cpal::Stream, AudioError> {
    device
        .build_input_stream(
            config.config(),
            move |data: &[S], _: &cpal::InputCallbackInfo| adapter.push(data),
            move |_| errors.add(1),
            None,
        )
        .map_err(backend_error)
}

fn build_output<S: Sample + SizedSample>(
    device: &cpal::Device,
    config: &SupportedStreamConfig,
    mut adapter: PlayoutAdapter,
    errors: Counter,
) -> Result<cpal::Stream, AudioError> {
    device
        .build_output_stream(
            config.config(),
            move |data: &mut [S], _: &cpal::OutputCallbackInfo| adapter.fill(data),
            move |_| errors.add(1),
            None,
        )
        .map_err(backend_error)
}

/// The default microphone and speaker of the computer.
#[derive(Default)]
pub struct DesktopBackend {
    streams: Option<(cpal::Stream, cpal::Stream)>,
    capture_dropped: Counter,
    playout_underruns: Counter,
    stream_errors: Counter,
}

impl DesktopBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Captured samples lost because the engine did not read in time, since the last start.
    pub fn capture_dropped(&self) -> Counter {
        self.capture_dropped.clone()
    }

    /// Speaker callbacks that played silence, since the last start.
    pub fn playout_underruns(&self) -> Counter {
        self.playout_underruns.clone()
    }

    /// Errors the OS reported on either stream, such as a device being unplugged.
    pub fn stream_errors(&self) -> Counter {
        self.stream_errors.clone()
    }
}

impl AudioBackend for DesktopBackend {
    fn start(&mut self, io: DeviceIo) -> Result<(), AudioError> {
        self.stop()?;
        let host = cpal::default_host();
        let input = host.default_input_device().ok_or(AudioError::NoDevice)?;
        let output = host.default_output_device().ok_or(AudioError::NoDevice)?;
        let input_config = choose_config(input.supported_input_configs().map_err(backend_error)?)
            .ok_or_else(no_supported_config)?;
        let output_config =
            choose_config(output.supported_output_configs().map_err(backend_error)?)
                .ok_or_else(no_supported_config)?;

        let capture = CaptureAdapter::new(stream_format(&input_config), io.capture)?;
        let playout = PlayoutAdapter::new(stream_format(&output_config), io.playout)?;
        self.capture_dropped = capture.dropped();
        self.playout_underruns = playout.underruns();

        let errors = self.stream_errors.clone();
        let input_stream = match input_config.sample_format() {
            SampleFormat::F32 => build_input::<f32>(&input, &input_config, capture, errors),
            SampleFormat::I16 => build_input::<i16>(&input, &input_config, capture, errors),
            _ => Err(no_supported_config()),
        }?;
        let errors = self.stream_errors.clone();
        let output_stream = match output_config.sample_format() {
            SampleFormat::F32 => build_output::<f32>(&output, &output_config, playout, errors),
            SampleFormat::I16 => build_output::<i16>(&output, &output_config, playout, errors),
            _ => Err(no_supported_config()),
        }?;

        input_stream.play().map_err(backend_error)?;
        output_stream.play().map_err(backend_error)?;
        self.streams = Some((input_stream, output_stream));
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AudioError> {
        // Dropping a cpal stream stops it and releases the device.
        self.streams = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::audio_io;
    use crate::{FRAME_SAMPLES, SAMPLE_RATE};
    use cpal::SupportedBufferSize;

    fn range(
        channels: u16,
        min: u32,
        max: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(channels, min, max, SupportedBufferSize::Unknown, format)
    }

    #[test]
    fn picks_48000_when_the_device_can_do_it() {
        let chosen = choose_config([
            range(1, 44_100, 44_100, SampleFormat::F32),
            range(1, 8_000, 96_000, SampleFormat::F32),
        ])
        .unwrap();
        assert_eq!(chosen.sample_rate(), SAMPLE_RATE);
    }

    #[test]
    fn otherwise_picks_the_rate_nearest_to_48000() {
        let chosen = choose_config([
            range(1, 8_000, 16_000, SampleFormat::F32),
            range(1, 44_100, 44_100, SampleFormat::F32),
            range(1, 96_000, 192_000, SampleFormat::F32),
        ])
        .unwrap();
        assert_eq!(chosen.sample_rate(), 44_100);
    }

    #[test]
    fn prefers_mono_to_stereo() {
        let chosen = choose_config([
            range(2, 48_000, 48_000, SampleFormat::F32),
            range(1, 48_000, 48_000, SampleFormat::I16),
        ])
        .unwrap();
        assert_eq!(chosen.channels(), 1);
    }

    #[test]
    fn only_formats_the_adapters_take_are_chosen() {
        assert!(choose_config([range(1, 48_000, 48_000, SampleFormat::I24)]).is_none());
        let chosen = choose_config([
            range(1, 48_000, 48_000, SampleFormat::U8),
            range(2, 44_100, 44_100, SampleFormat::I16),
        ])
        .unwrap();
        assert_eq!(chosen.sample_format(), SampleFormat::I16);
    }

    // Needs a microphone, a speaker and, on macOS, microphone permission for the terminal.
    #[test]
    #[ignore]
    fn captures_audio_from_the_default_microphone() {
        let (device, mut engine) = audio_io(16);
        let mut backend = DesktopBackend::new();
        backend.start(device).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        backend.stop().unwrap();

        let mut frame = [0; FRAME_SAMPLES];
        assert!(engine.capture.read_frame(&mut frame));
    }
}
