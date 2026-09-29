//! H.264 access units into RTP payloads (RFC 6184, packetization mode 1).
//!
//! A NAL unit that fits the MTU travels alone; a bigger one is split into FU-A fragments; the
//! SPS and PPS of a keyframe travel together in a STAP-A ahead of the IDR slice. The splitting
//! itself is webrtc-rs' `H264Payloader`; this adds the sequence numbers, the 90 kHz timestamp,
//! the marker bit on the last packet of a frame and the rotation for the CVO extension.

use std::time::Duration;

use bytes::Bytes;
use rtc::rtp::codec::h264::H264Payloader;
use rtc::rtp::packetizer::Payloader;

use super::{EncodedFrame, Rotation};

/// The RTP clock of video, in Hz.
pub const VIDEO_CLOCK_RATE: u32 = 90_000;

/// The largest RTP payload sent: with the RTP header, SRTP and a TURN relay it still fits a
/// 1280-byte IPv6 minimum MTU. The same as webrtc-rs uses for its own tracks.
pub const MAX_PAYLOAD: usize = 1200;

/// One RTP packet of video, as the engine sends and receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoPacket {
    /// RTP sequence number: consecutive packets differ by one, wrapping at `u16::MAX`.
    pub sequence: u16,
    /// RTP timestamp at 90 kHz, the same for every packet of a frame.
    pub timestamp: u32,
    /// Set on the last packet of a frame.
    pub marker: bool,
    /// The RFC 6184 payload: a NAL unit, a STAP-A or an FU-A fragment.
    pub payload: Vec<u8>,
    /// The rotation in the packet's CVO header extension, if it has one.
    pub rotation: Option<Rotation>,
}

/// A capture time at 90 kHz, wrapping as RTP timestamps do.
pub fn rtp_timestamp(time: Duration) -> u32 {
    // Truncating to 32 bits is the wrap.
    (time.as_micros() * u128::from(VIDEO_CLOCK_RATE) / 1_000_000) as u32
}

/// Turns frames into numbered RTP packets.
#[derive(Debug)]
pub struct Packetizer {
    payloader: H264Payloader,
    sequence: u16,
    timestamp_offset: u32,
    mtu: usize,
}

impl Packetizer {
    /// Numbers the first packet `first_sequence` and adds `timestamp_offset` to every RTP
    /// timestamp. RFC 3550 wants both random, so a stream's numbers say nothing about when it
    /// started.
    pub fn new(first_sequence: u16, timestamp_offset: u32) -> Self {
        Self {
            payloader: H264Payloader::default(),
            sequence: first_sequence,
            timestamp_offset,
            mtu: MAX_PAYLOAD,
        }
    }

    /// Payloads of at most `mtu` bytes instead of [`MAX_PAYLOAD`].
    pub fn with_mtu(mut self, mtu: usize) -> Self {
        self.mtu = mtu;
        self
    }

    /// The RTP packets of one frame, the last one with the marker bit and the frame's rotation.
    /// A frame with no NAL unit gives no packet.
    pub fn packetize(&mut self, frame: &EncodedFrame) -> Vec<VideoPacket> {
        let data = Bytes::copy_from_slice(&frame.data);
        // It only fails on its own bugs; a frame it cannot split is a frame not sent.
        let payloads = self.payloader.payload(self.mtu, &data).unwrap_or_default();
        let timestamp = self
            .timestamp_offset
            .wrapping_add(rtp_timestamp(frame.timestamp));
        let last = payloads.len().saturating_sub(1);
        payloads
            .into_iter()
            .enumerate()
            .map(|(index, payload)| {
                let sequence = self.sequence;
                self.sequence = self.sequence.wrapping_add(1);
                VideoPacket {
                    sequence,
                    timestamp,
                    marker: index == last,
                    payload: payload.to_vec(),
                    rotation: (index == last).then_some(frame.rotation),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::h264::{NAL_IDR, NAL_PPS, NAL_SPS, nal_type};

    const SPS: &[u8] = &[0x67, 0x42, 0xE0, 0x1F, 0x8C, 0x8D];
    const PPS: &[u8] = &[0x68, 0xCE, 0x3C, 0x80];

    fn annex_b(units: &[&[u8]]) -> Vec<u8> {
        units
            .iter()
            .flat_map(|unit| [&[0, 0, 0, 1][..], unit].concat())
            .collect()
    }

    /// A NAL unit of `len` bytes after its `header`, with no zero bytes in it.
    fn unit(header: u8, len: usize) -> Vec<u8> {
        std::iter::once(header)
            .chain((0..len).map(|i| (i % 255) as u8 + 1))
            .collect()
    }

    fn frame(data: Vec<u8>, keyframe: bool, millis: u64) -> EncodedFrame {
        EncodedFrame {
            data,
            keyframe,
            timestamp: Duration::from_millis(millis),
            rotation: Rotation::Deg0,
        }
    }

    #[test]
    fn capture_times_become_90_khz_timestamps() {
        assert_eq!(rtp_timestamp(Duration::ZERO), 0);
        assert_eq!(rtp_timestamp(Duration::from_millis(33)), 2_970);
        assert_eq!(rtp_timestamp(Duration::from_secs(1)), 90_000);
        // 2^32 ticks at 90 kHz is about 13 hours: then the timestamp wraps.
        let wrap = Duration::from_micros((1u64 << 32) * 1_000_000 / 90_000 + 100);
        assert!(rtp_timestamp(wrap) < 90_000);
    }

    #[test]
    fn a_small_delta_frame_is_one_packet_with_the_marker() {
        let mut packetizer = Packetizer::new(100, 1_000);
        let slice = unit(0x41, 50);
        let packets = packetizer.packetize(&frame(annex_b(&[&slice]), false, 33));

        assert_eq!(
            packets,
            vec![VideoPacket {
                sequence: 100,
                timestamp: 1_000 + 2_970,
                marker: true,
                payload: slice,
                rotation: Some(Rotation::Deg0),
            }]
        );
    }

    #[test]
    fn a_keyframe_sends_its_sps_and_pps_in_a_stap_a_ahead_of_the_idr() {
        let mut packetizer = Packetizer::new(0, 0);
        let idr = unit(0x65, 200);
        let packets = packetizer.packetize(&frame(annex_b(&[SPS, PPS, &idr]), true, 0));

        assert_eq!(packets.len(), 2, "{packets:?}");
        let stap_a = &packets[0].payload;
        assert_eq!(nal_type(stap_a[0]), 24, "a STAP-A");
        assert_eq!(
            usize::from(u16::from_be_bytes([stap_a[1], stap_a[2]])),
            SPS.len()
        );
        assert_eq!(nal_type(stap_a[3]), NAL_SPS);
        assert_eq!(nal_type(stap_a[3 + SPS.len() + 2]), NAL_PPS);
        assert!(!packets[0].marker);
        assert_eq!(packets[0].rotation, None);
        assert_eq!(packets[1].payload, idr);
        assert!(packets[1].marker);
    }

    #[test]
    fn a_big_nal_unit_is_split_into_fu_a_fragments_within_the_mtu() {
        let mut packetizer = Packetizer::new(0, 0).with_mtu(100);
        let idr = unit(0x65, 450);
        let packets = packetizer.packetize(&frame(annex_b(&[&idr]), true, 0));

        assert_eq!(
            packets.len(),
            5,
            "450 bytes after the header, 98 per fragment"
        );
        for (index, packet) in packets.iter().enumerate() {
            assert!(packet.payload.len() <= 100);
            assert_eq!(nal_type(packet.payload[0]), 28, "an FU-A");
            assert_eq!(nal_type(packet.payload[1]), NAL_IDR);
            assert_eq!(packet.payload[1] & 0x80 != 0, index == 0, "start bit");
            assert_eq!(packet.payload[1] & 0x40 != 0, index == 4, "end bit");
            assert_eq!(packet.marker, index == 4);
            assert_eq!(packet.sequence, index as u16);
        }
        let rebuilt: Vec<u8> = packets
            .iter()
            .flat_map(|packet| packet.payload[2..].to_vec())
            .collect();
        assert_eq!(rebuilt, idr[1..]);
    }

    #[test]
    fn sequence_numbers_run_on_across_frames_and_wrap() {
        let mut packetizer = Packetizer::new(u16::MAX - 1, 0);
        let slice = unit(0x41, 10);
        let sequences: Vec<u16> = (0..3)
            .flat_map(|index| packetizer.packetize(&frame(annex_b(&[&slice]), false, index * 33)))
            .map(|packet| packet.sequence)
            .collect();
        assert_eq!(sequences, vec![u16::MAX - 1, u16::MAX, 0]);
    }

    #[test]
    fn the_rotation_rides_on_the_last_packet_of_the_frame() {
        let mut packetizer = Packetizer::new(0, 0).with_mtu(100);
        let mut rotated = frame(annex_b(&[&unit(0x41, 300)]), false, 0);
        rotated.rotation = Rotation::Deg90;
        let packets = packetizer.packetize(&rotated);

        let rotations: Vec<_> = packets.iter().map(|packet| packet.rotation).collect();
        assert_eq!(rotations, vec![None, None, None, Some(Rotation::Deg90)]);
    }

    #[test]
    fn a_frame_without_nal_units_sends_nothing() {
        let mut packetizer = Packetizer::new(7, 0);
        assert!(
            packetizer
                .packetize(&frame(Vec::new(), false, 0))
                .is_empty()
        );
        let packets = packetizer.packetize(&frame(annex_b(&[&unit(0x41, 5)]), false, 0));
        assert_eq!(packets[0].sequence, 7, "no number was used up");
    }
}
