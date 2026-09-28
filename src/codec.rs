//! Opus encoding and decoding of 20 ms mono frames at 48 kHz.

use std::ffi::c_int;
use std::fmt;
use std::ptr::NonNull;

/// Sample rate of every frame, in Hz.
pub const SAMPLE_RATE: u32 = 48_000;
/// Samples in one 20 ms mono frame at 48 kHz.
pub const FRAME_SAMPLES: usize = 960;
/// Target bitrate: what WebRTC uses for mono Opus voice; in-band FEC is paid from it.
pub const BITRATE: i32 = 32_000;
/// Packet loss the encoder plans its FEC for, in percent.
pub const EXPECTED_LOSS_PERCENT: i32 = 10;

/// Largest Opus packet for a single frame (RFC 6716, section 3.2.1).
const MAX_PACKET: usize = 1_275;
/// Longest frame an Opus packet can carry (120 ms), so a peer's packet never overflows.
const MAX_DECODED_SAMPLES: usize = 5_760;
const CHANNELS: c_int = 1;

/// Errors of the codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The frame did not have exactly [`FRAME_SAMPLES`] samples; holds the length given.
    FrameSize(usize),
    /// libopus returned this error code.
    Opus(i32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::FrameSize(len) => {
                write!(f, "frame has {len} samples, expected {FRAME_SAMPLES}")
            }
            Error::Opus(code) => write!(f, "libopus error {code}"),
        }
    }
}

impl std::error::Error for Error {}

/// Opus encoder tuned for voice calls.
pub struct Encoder {
    state: NonNull<ffi::OpusEncoder>,
}

// libopus state is plain heap memory with no thread affinity; `&mut self` guards all access.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new() -> Result<Encoder, Error> {
        let mut code = ffi::OPUS_OK;
        // SAFETY: valid arguments; `code` outlives the call.
        let raw = unsafe {
            ffi::opus_encoder_create(
                SAMPLE_RATE as i32,
                CHANNELS,
                ffi::OPUS_APPLICATION_VOIP,
                &mut code,
            )
        };
        let state = NonNull::new(raw).ok_or(Error::Opus(code))?;
        let mut encoder = Encoder { state };
        check(code)?;
        encoder.set(ffi::OPUS_SET_SIGNAL_REQUEST, ffi::OPUS_SIGNAL_VOICE)?;
        encoder.set(ffi::OPUS_SET_BITRATE_REQUEST, BITRATE)?;
        // In-band FEC (SILK LBRR) carries a coarse copy of the previous frame in each packet;
        // the expected loss tells the encoder how many bits to give it.
        encoder.set(ffi::OPUS_SET_INBAND_FEC_REQUEST, 1)?;
        encoder.set(
            ffi::OPUS_SET_PACKET_LOSS_PERC_REQUEST,
            EXPECTED_LOSS_PERCENT,
        )?;
        encoder.set(ffi::OPUS_SET_DTX_REQUEST, 0)?;
        Ok(encoder)
    }

    fn set(&mut self, request: c_int, value: i32) -> Result<(), Error> {
        // SAFETY: every request passed here is a setter taking one opus_int32.
        let code = unsafe { ffi::opus_encoder_ctl(self.state.as_ptr(), request, value) };
        check(code).map(drop)
    }

    /// Encodes one frame of exactly [`FRAME_SAMPLES`] samples into one Opus packet.
    pub fn encode(&mut self, frame: &[i16]) -> Result<Vec<u8>, Error> {
        if frame.len() != FRAME_SAMPLES {
            return Err(Error::FrameSize(frame.len()));
        }
        let mut packet = vec![0; MAX_PACKET];
        // SAFETY: `frame` holds FRAME_SAMPLES samples (checked above) and `packet` MAX_PACKET bytes.
        let len = unsafe {
            ffi::opus_encode(
                self.state.as_ptr(),
                frame.as_ptr(),
                FRAME_SAMPLES as c_int,
                packet.as_mut_ptr(),
                MAX_PACKET as i32,
            )
        };
        packet.truncate(check(len)?);
        Ok(packet)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_encoder_create and destroyed only here.
        unsafe { ffi::opus_encoder_destroy(self.state.as_ptr()) }
    }
}

/// Opus decoder producing 20 ms mono frames.
pub struct Decoder {
    state: NonNull<ffi::OpusDecoder>,
}

// See the note on `Encoder`.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Decoder, Error> {
        let mut code = ffi::OPUS_OK;
        // SAFETY: valid arguments; `code` outlives the call.
        let raw = unsafe { ffi::opus_decoder_create(SAMPLE_RATE as i32, CHANNELS, &mut code) };
        let state = NonNull::new(raw).ok_or(Error::Opus(code))?;
        let decoder = Decoder { state };
        check(code)?;
        Ok(decoder)
    }

    /// Decodes one packet.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<i16>, Error> {
        self.run(packet, MAX_DECODED_SAMPLES, false)
    }

    /// Synthesises a frame for a packet that never arrived (packet loss concealment).
    pub fn conceal(&mut self) -> Result<Vec<i16>, Error> {
        self.run(&[], FRAME_SAMPLES, false)
    }

    /// Calls `opus_decode`; an empty `packet` means "lost". `max_samples` bounds the output
    /// and, for a lost frame, is the duration libopus must synthesise.
    fn run(&mut self, packet: &[u8], max_samples: usize, fec: bool) -> Result<Vec<i16>, Error> {
        let data = if packet.is_empty() {
            std::ptr::null()
        } else {
            packet.as_ptr()
        };
        let len = i32::try_from(packet.len()).map_err(|_| Error::Opus(ffi::OPUS_BAD_ARG))?;
        let mut pcm = vec![0; max_samples];
        // SAFETY: `data` is null or valid for `len` bytes; `pcm` holds `max_samples` samples.
        let samples = unsafe {
            ffi::opus_decode(
                self.state.as_ptr(),
                data,
                len,
                pcm.as_mut_ptr(),
                max_samples as c_int,
                c_int::from(fec),
            )
        };
        pcm.truncate(check(samples)?);
        Ok(pcm)
    }

    /// Rebuilds the lost frame before `next_packet` from the redundancy (in-band FEC) it carries.
    /// Call `decode(next_packet)` afterwards to get the next frame itself. Without FEC
    /// data in the packet, libopus falls back to concealment.
    pub fn recover(&mut self, next_packet: &[u8]) -> Result<Vec<i16>, Error> {
        self.run(next_packet, FRAME_SAMPLES, true)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_decoder_create and destroyed only here.
        unsafe { ffi::opus_decoder_destroy(self.state.as_ptr()) }
    }
}

/// Turns a libopus return value into a length, or its negative error code into an error.
fn check(ret: c_int) -> Result<usize, Error> {
    usize::try_from(ret).map_err(|_| Error::Opus(ret))
}

/// The part of the libopus C API we use (`include/opus.h`, `include/opus_defines.h`).
mod ffi {
    use std::ffi::c_int;

    pub const OPUS_OK: c_int = 0;
    pub const OPUS_BAD_ARG: c_int = -1;
    pub const OPUS_APPLICATION_VOIP: c_int = 2048;
    pub const OPUS_SIGNAL_VOICE: i32 = 3001;
    pub const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
    pub const OPUS_SET_INBAND_FEC_REQUEST: c_int = 4012;
    pub const OPUS_SET_PACKET_LOSS_PERC_REQUEST: c_int = 4014;
    pub const OPUS_SET_DTX_REQUEST: c_int = 4016;
    pub const OPUS_SET_SIGNAL_REQUEST: c_int = 4024;

    #[repr(C)]
    pub struct OpusEncoder {
        _opaque: [u8; 0],
    }

    #[repr(C)]
    pub struct OpusDecoder {
        _opaque: [u8; 0],
    }

    unsafe extern "C" {
        pub fn opus_encoder_create(
            fs: i32,
            channels: c_int,
            application: c_int,
            error: *mut c_int,
        ) -> *mut OpusEncoder;
        pub fn opus_encode(
            st: *mut OpusEncoder,
            pcm: *const i16,
            frame_size: c_int,
            data: *mut u8,
            max_data_bytes: i32,
        ) -> i32;
        pub fn opus_encoder_ctl(st: *mut OpusEncoder, request: c_int, ...) -> c_int;
        pub fn opus_encoder_destroy(st: *mut OpusEncoder);

        pub fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int)
        -> *mut OpusDecoder;
        pub fn opus_decode(
            st: *mut OpusDecoder,
            data: *const u8,
            len: i32,
            pcm: *mut i16,
            frame_size: c_int,
            decode_fec: c_int,
        ) -> c_int;
        pub fn opus_decoder_destroy(st: *mut OpusDecoder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    /// Two tones, so the signal is not periodic within the lag search window
    /// and the alignment is unambiguous.
    fn test_signal(frames: usize) -> Vec<i16> {
        (0..frames * FRAME_SAMPLES)
            .map(|n| {
                let t = n as f64 / f64::from(SAMPLE_RATE);
                let s = 0.25 * (TAU * 300.0 * t).sin() + 0.15 * (TAU * 1_250.0 * t).sin();
                (s * f64::from(i16::MAX)) as i16
            })
            .collect()
    }

    fn encode_all(signal: &[i16]) -> Vec<Vec<u8>> {
        let mut encoder = Encoder::new().unwrap();
        signal
            .chunks(FRAME_SAMPLES)
            .map(|frame| encoder.encode(frame).unwrap())
            .collect()
    }

    fn decode_all(packets: &[Vec<u8>]) -> Vec<i16> {
        let mut decoder = Decoder::new().unwrap();
        packets
            .iter()
            .flat_map(|packet| {
                let pcm = decoder.decode(packet).unwrap();
                assert_eq!(pcm.len(), FRAME_SAMPLES);
                pcm
            })
            .collect()
    }

    fn energy(samples: &[i16]) -> f64 {
        samples.iter().map(|&s| f64::from(s).powi(2)).sum()
    }

    /// Highest normalised cross-correlation of `decoded` against `original`
    /// delayed by 0..=max_lag samples.
    fn best_correlation(original: &[i16], decoded: &[i16], max_lag: usize) -> f64 {
        let window = original.len() - max_lag;
        let reference = &original[..window];
        (0..=max_lag)
            .map(|lag| {
                let shifted = &decoded[lag..lag + window];
                let dot: f64 = reference
                    .iter()
                    .zip(shifted)
                    .map(|(&a, &b)| f64::from(a) * f64::from(b))
                    .sum();
                dot / (energy(reference) * energy(shifted)).sqrt()
            })
            .fold(f64::MIN, f64::max)
    }

    #[test]
    fn round_trip_keeps_the_waveform_and_its_energy() {
        const FRAMES: usize = 50;
        const WARM_UP: usize = 5 * FRAME_SAMPLES;
        let signal = test_signal(FRAMES);

        let decoded = decode_all(&encode_all(&signal));
        assert_eq!(decoded.len(), signal.len());

        // Skip the encoder's start-up; the codec delay is found by the lag search.
        let original = &signal[WARM_UP..];
        let decoded = &decoded[WARM_UP..];
        let correlation = best_correlation(original, decoded, FRAME_SAMPLES);
        assert!(correlation > 0.9, "correlation {correlation}");

        let rms_ratio = (energy(decoded) / energy(original)).sqrt();
        assert!((0.7..1.4).contains(&rms_ratio), "rms ratio {rms_ratio}");
    }

    #[test]
    fn encode_rejects_a_frame_that_is_not_20_ms() {
        let mut encoder = Encoder::new().unwrap();
        for len in [FRAME_SAMPLES + 1, 2 * FRAME_SAMPLES, FRAME_SAMPLES - 1, 0] {
            let frame = vec![0; len];
            assert_eq!(encoder.encode(&frame), Err(Error::FrameSize(len)));
        }
    }

    #[test]
    fn packets_stay_within_the_target_bitrate() {
        const FRAMES: usize = 50;
        const FRAMES_PER_SECOND: usize = SAMPLE_RATE as usize / FRAME_SAMPLES;
        let nominal_bytes = BITRATE as usize / 8 / FRAMES_PER_SECOND;

        // Skip the first packets: the encoder is still adapting.
        let packets = encode_all(&test_signal(FRAMES)).split_off(5);
        let total: usize = packets.iter().map(Vec::len).sum();
        let mean_bitrate = total * 8 * FRAMES_PER_SECOND / packets.len();

        // VBR moves bits between frames, but the average must honour the target and no
        // single packet may take more than a few frames' worth of budget.
        let target = BITRATE as usize;
        assert!(
            (target / 4..=target * 5 / 4).contains(&mean_bitrate),
            "mean bitrate {mean_bitrate}"
        );
        let largest = packets.iter().map(Vec::len).max().unwrap();
        assert!(
            largest <= 3 * nominal_bytes,
            "largest packet {largest} bytes"
        );
    }

    #[test]
    fn conceal_fills_a_lost_frame_with_960_samples() {
        let packets = encode_all(&test_signal(10));
        let mut decoder = Decoder::new().unwrap();
        for packet in &packets {
            decoder.decode(packet).unwrap();
        }

        let concealed = decoder.conceal().unwrap();

        assert_eq!(concealed.len(), FRAME_SAMPLES);
        // It continues the voice rather than dropping to silence.
        assert!(energy(&concealed) > 0.0);
    }

    fn squared_error(a: &[i16], b: &[i16]) -> f64 {
        a.iter()
            .zip(b)
            .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
            .sum()
    }

    #[test]
    fn recover_rebuilds_a_lost_frame_from_the_next_packets_fec() {
        const LOST: usize = 20;
        let packets = encode_all(&test_signal(LOST + 2));
        let frame =
            |pcm: &[i16], index: usize| pcm[index * FRAME_SAMPLES..][..FRAME_SAMPLES].to_vec();
        // What the listener hears with no loss: the codec delay is the same on every path.
        let expected = frame(&decode_all(&packets), LOST);

        let mut with_fec = Decoder::new().unwrap();
        let mut with_plc = Decoder::new().unwrap();
        for packet in &packets[..LOST] {
            with_fec.decode(packet).unwrap();
            with_plc.decode(packet).unwrap();
        }
        let recovered = with_fec.recover(&packets[LOST + 1]).unwrap();
        let concealed = with_plc.conceal().unwrap();

        assert_eq!(recovered.len(), FRAME_SAMPLES);
        let fec_error = squared_error(&recovered, &expected);
        let silence_error = energy(&expected);
        let plc_error = squared_error(&concealed, &expected);
        assert!(
            fec_error < silence_error,
            "fec {fec_error} vs silence {silence_error}"
        );
        // Beating plain concealment shows the redundancy was actually decoded.
        assert!(fec_error < plc_error, "fec {fec_error} vs plc {plc_error}");
    }
}
