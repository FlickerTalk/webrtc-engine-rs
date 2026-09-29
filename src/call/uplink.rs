//! The send side of a call: microphone frames in, Opus packets out.

use crate::audio::CaptureFrames;
use crate::codec::{self, Encoder};
use crate::{FRAME_SAMPLES, Frame};

/// Counters of the send side. No audio content, no identifiers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SendStats {
    /// Frames encoded into packets, muted ones included.
    pub encoded: u64,
    /// Of those, frames sent as silence because the microphone was muted.
    pub muted: u64,
    /// Frames the encoder refused; they are skipped.
    pub encode_errors: u64,
}

/// Turns each captured 20 ms frame into one Opus packet.
pub struct Uplink {
    capture: CaptureFrames,
    encoder: Encoder,
    frame: Frame,
    muted: bool,
    stats: SendStats,
}

impl Uplink {
    pub fn new(capture: CaptureFrames) -> Result<Self, codec::Error> {
        Ok(Self {
            capture,
            encoder: Encoder::new()?,
            frame: [0; FRAME_SAMPLES],
            muted: false,
            stats: SendStats::default(),
        })
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }

    /// The packet for the next captured frame, or `None` until a whole frame is waiting.
    pub fn next_packet(&mut self) -> Option<Vec<u8>> {
        while self.capture.read_frame(&mut self.frame) {
            if self.muted {
                // Silence, not nothing: a gap in the packets would read as loss on the far side.
                self.frame.fill(0);
                self.stats.muted += 1;
            }
            match self.encoder.encode(&self.frame) {
                Ok(packet) => {
                    self.stats.encoded += 1;
                    return Some(packet);
                }
                Err(_) => self.stats.encode_errors += 1,
            }
        }
        None
    }

    pub fn stats(&self) -> SendStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{RingProducer, audio_io};
    use crate::codec::Decoder;
    use std::f64::consts::TAU;

    fn tone(samples: usize) -> Vec<i16> {
        (0..samples)
            .map(|n| (12_000.0 * (TAU * 440.0 * n as f64 / 48_000.0).sin()) as i16)
            .collect()
    }

    fn uplink() -> (RingProducer, Uplink) {
        let (device, engine) = audio_io(8);
        let uplink = Uplink::new(engine.capture).unwrap();
        (device.capture, uplink)
    }

    fn energy(samples: &[i16]) -> f64 {
        samples.iter().map(|&s| f64::from(s).powi(2)).sum()
    }

    fn rms(samples: &[i16]) -> f64 {
        (energy(samples) / samples.len() as f64).sqrt()
    }

    #[test]
    fn encodes_nothing_until_a_whole_frame_is_captured() {
        let (mut microphone, mut uplink) = uplink();
        let signal = tone(FRAME_SAMPLES);
        microphone.push(&signal[..500]);
        assert_eq!(uplink.next_packet(), None);
        microphone.push(&signal[500..]);
        assert!(
            uplink
                .next_packet()
                .is_some_and(|packet| !packet.is_empty())
        );
        assert_eq!(uplink.next_packet(), None);
    }

    #[test]
    fn sends_one_packet_per_captured_frame() {
        let (mut microphone, mut uplink) = uplink();
        microphone.push(&tone(3 * FRAME_SAMPLES));
        let packets: Vec<Vec<u8>> = std::iter::from_fn(|| uplink.next_packet()).collect();
        assert_eq!(packets.len(), 3);
        assert_eq!(uplink.stats().encoded, 3);

        let mut decoder = Decoder::new().unwrap();
        let decoded: Vec<i16> = packets
            .iter()
            .flat_map(|packet| decoder.decode(packet).unwrap())
            .collect();
        assert_eq!(decoded.len(), 3 * FRAME_SAMPLES);
        assert!(energy(&decoded[2 * FRAME_SAMPLES..]) > 0.0);
    }

    // Muting still sends a packet every 20 ms, so the far side's jitter buffer keeps its
    // timing, but the packets carry silence instead of the microphone.
    #[test]
    fn muted_sends_silence_instead_of_the_microphone() {
        let (mut microphone, mut uplink) = uplink();
        uplink.set_muted(true);
        assert!(uplink.is_muted());
        microphone.push(&tone(4 * FRAME_SAMPLES));
        let packets: Vec<Vec<u8>> = std::iter::from_fn(|| uplink.next_packet()).collect();
        assert_eq!(packets.len(), 4);
        let stats = uplink.stats();
        assert_eq!(stats.encoded, 4);
        assert_eq!(stats.muted, 4);

        // Opus decodes silence to a few LSB of noise, not to exact zeros: far below -60 dBFS.
        let mut decoder = Decoder::new().unwrap();
        for packet in &packets {
            let rms = rms(&decoder.decode(packet).unwrap());
            assert!(rms < 30.0, "muted frame at rms {rms}");
        }

        uplink.set_muted(false);
        microphone.push(&tone(2 * FRAME_SAMPLES));
        let loud: Vec<i16> = std::iter::from_fn(|| uplink.next_packet())
            .flat_map(|packet| decoder.decode(&packet).unwrap())
            .collect();
        assert!(rms(&loud) > 1_000.0, "unmuted rms {}", rms(&loud));
    }
}
