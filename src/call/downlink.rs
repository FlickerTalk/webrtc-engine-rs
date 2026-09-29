//! The receive side of a call: packets from the network in, 20 ms frames for the speaker out.
//!
//! The speaker sets the pace: [`Downlink::pump`] only takes a frame from the jitter buffer
//! when the playout ring runs low, so the device's clock, not a timer, decides how fast frames
//! are played.

use std::time::Duration;

use crate::audio::PlayoutFrames;
use crate::codec::{self, Decoder};
use crate::jitter::{JitterBuffer, Playout};
use crate::rtp::AudioPacket;
use crate::{FRAME_SAMPLES, Frame};

/// Counters of the receive side and the jitter buffer's state. No audio content, no
/// identifiers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReceiveStats {
    /// Packets handed to [`Downlink::receive`].
    pub received: u64,
    /// Frames decoded from their own packet.
    pub decoded: u64,
    /// Lost frames rebuilt from the in-band FEC of the packet after them.
    pub recovered: u64,
    /// Frames made up by the decoder's concealment (PLC), for a loss without FEC to use or
    /// because the buffer ran dry.
    pub concealed: u64,
    /// Of the concealed frames, those played because the jitter buffer ran dry mid-call.
    pub underruns: u64,
    /// Silent frames played before the first packet was due.
    pub silence: u64,
    /// Packets the decoder refused, or that did not hold 20 ms; concealed instead.
    pub decode_errors: u64,
    /// Packets that came after their turn.
    pub late: u64,
    /// Packets that came twice.
    pub duplicate: u64,
    /// Packets thrown away unplayed by the jitter buffer.
    pub discarded: u64,
    /// Packets in the jitter buffer now.
    pub depth: usize,
    /// Depth the jitter buffer aims for, in 20 ms frames.
    pub target: usize,
    /// Interarrival jitter measured by the jitter buffer.
    pub jitter: Duration,
}

/// Feeds the speaker from the jitter buffer, decoding, recovering or concealing each frame.
pub struct Downlink {
    jitter: JitterBuffer,
    decoder: Decoder,
    playout: PlayoutFrames,
    queue_samples: usize,
    started: bool,
    stats: ReceiveStats,
}

impl Downlink {
    /// Plays into `playout`, keeping about `queue_frames` frames queued for the speaker.
    pub fn new(playout: PlayoutFrames, queue_frames: usize) -> Result<Self, codec::Error> {
        Ok(Self {
            jitter: JitterBuffer::new(),
            decoder: Decoder::new()?,
            playout,
            queue_samples: queue_frames.max(1) * FRAME_SAMPLES,
            started: false,
            stats: ReceiveStats::default(),
        })
    }

    /// Takes a packet that arrived at `arrival`, on the caller's monotonic clock.
    pub fn receive(&mut self, packet: AudioPacket, arrival: Duration) {
        self.jitter
            .push_at(packet.sequence, packet.payload, arrival);
    }

    /// Tops up the speaker's queue; returns how many frames it wrote.
    ///
    /// Call it often (every few milliseconds): each call takes from the jitter buffer only
    /// the frames the speaker has made room for.
    pub fn pump(&mut self) -> usize {
        let mut written = 0;
        while self.playout.samples_queued() < self.queue_samples && self.playout.frames_free() > 0 {
            let frame = self.next_frame();
            if !self.playout.write_frame(&frame) {
                break;
            }
            written += 1;
        }
        written
    }

    fn next_frame(&mut self) -> Frame {
        match self.jitter.playout() {
            Playout::Frame(payload) => {
                self.started = true;
                match self.decode(&payload) {
                    Some(frame) => {
                        self.stats.decoded += 1;
                        frame
                    }
                    None => {
                        self.stats.decode_errors += 1;
                        self.conceal()
                    }
                }
            }
            Playout::Missing => {
                // Without FEC in that packet libopus falls back to concealment by itself.
                let rebuilt = self
                    .jitter
                    .peek_next()
                    .and_then(|next| self.decoder.recover(next).ok())
                    .and_then(whole_frame);
                match rebuilt {
                    Some(frame) => {
                        self.stats.recovered += 1;
                        frame
                    }
                    None => self.conceal(),
                }
            }
            Playout::Waiting if self.started => {
                self.stats.underruns += 1;
                self.conceal()
            }
            Playout::Waiting => {
                self.stats.silence += 1;
                [0; FRAME_SAMPLES]
            }
        }
    }

    fn decode(&mut self, payload: &[u8]) -> Option<Frame> {
        // libopus reads an empty packet as "lost", which is not what the sender meant.
        if payload.is_empty() {
            return None;
        }
        self.decoder.decode(payload).ok().and_then(whole_frame)
    }

    fn conceal(&mut self) -> Frame {
        self.stats.concealed += 1;
        self.decoder
            .conceal()
            .ok()
            .and_then(whole_frame)
            .unwrap_or([0; FRAME_SAMPLES])
    }

    pub fn stats(&self) -> ReceiveStats {
        let jitter = self.jitter.stats();
        ReceiveStats {
            received: jitter.received,
            late: jitter.late,
            duplicate: jitter.duplicate,
            discarded: jitter.discarded,
            depth: jitter.depth,
            target: jitter.target,
            jitter: jitter.jitter,
            ..self.stats.clone()
        }
    }
}

/// Only 20 ms frames fit the pipeline; a sender using another duration is not supported.
fn whole_frame(samples: Vec<i16>) -> Option<Frame> {
    samples.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{RingConsumer, audio_io};
    use crate::codec::Encoder;
    use std::f64::consts::TAU;

    const QUEUE: usize = 2;

    fn downlink() -> (RingConsumer, Downlink) {
        let (device, engine) = audio_io(8);
        (
            device.playout,
            Downlink::new(engine.playout, QUEUE).unwrap(),
        )
    }

    /// Opus packets of a 440 Hz tone, numbered from 0.
    fn packets(count: usize) -> Vec<AudioPacket> {
        let mut encoder = Encoder::new().unwrap();
        (0..count)
            .map(|index| {
                let frame: Vec<i16> = (0..FRAME_SAMPLES)
                    .map(|n| {
                        let t = (index * FRAME_SAMPLES + n) as f64 / 48_000.0;
                        (12_000.0 * (TAU * 440.0 * t).sin()) as i16
                    })
                    .collect();
                AudioPacket {
                    sequence: index as u16,
                    timestamp: (index * FRAME_SAMPLES) as u32,
                    payload: encoder.encode(&frame).unwrap(),
                }
            })
            .collect()
    }

    fn at(frame: usize) -> Duration {
        Duration::from_millis(20 * frame as u64)
    }

    /// The speaker plays one frame, then the engine tops the queue up again.
    fn play_one(speaker: &mut RingConsumer, downlink: &mut Downlink) -> Frame {
        let mut frame = [0; FRAME_SAMPLES];
        assert_eq!(
            speaker.pop(&mut frame),
            FRAME_SAMPLES,
            "the speaker underran"
        );
        downlink.pump();
        frame
    }

    fn rms(samples: &[i16]) -> f64 {
        let energy: f64 = samples.iter().map(|&s| f64::from(s).powi(2)).sum();
        (energy / samples.len() as f64).sqrt()
    }

    #[test]
    fn plays_silence_until_the_first_frame_is_due() {
        let (mut speaker, mut downlink) = downlink();
        assert_eq!(downlink.pump(), QUEUE);
        let frame = play_one(&mut speaker, &mut downlink);
        assert_eq!(frame, [0; FRAME_SAMPLES]);
        let stats = downlink.stats();
        assert_eq!(stats.silence, 3);
        assert_eq!(stats.concealed, 0);
        assert_eq!(stats.underruns, 0);
    }

    // Only a short queue for the speaker: whatever sits there is delay the jitter buffer
    // cannot adapt.
    #[test]
    fn keeps_only_the_queue_asked_for() {
        let (mut speaker, mut downlink) = downlink();
        assert_eq!(downlink.pump(), QUEUE);
        assert_eq!(downlink.pump(), 0);
        speaker.pop(&mut [0; 300]);
        assert_eq!(downlink.pump(), 1);
        assert_eq!(downlink.pump(), 0);
    }

    #[test]
    fn plays_the_decoded_frames() {
        let (mut speaker, mut downlink) = downlink();
        downlink.pump();
        for (index, packet) in packets(10).into_iter().enumerate() {
            downlink.receive(packet, at(index));
        }
        let played: Vec<i16> = (0..10)
            .flat_map(|_| play_one(&mut speaker, &mut downlink))
            .collect();
        assert!(rms(&played[5 * FRAME_SAMPLES..]) > 3_000.0);
        let stats = downlink.stats();
        assert_eq!(stats.received, 10);
        assert_eq!(stats.decoded, 10);
        assert_eq!(stats.concealed + stats.recovered + stats.underruns, 0);
        assert_eq!(stats.depth, 0);
        assert_eq!(stats.target, 2);
    }

    // The packet after a lost one carries a coarse copy of it (in-band FEC).
    #[test]
    fn rebuilds_a_lost_frame_from_the_next_packets_fec() {
        let (mut speaker, mut downlink) = downlink();
        for packet in packets(8) {
            if packet.sequence != 4 {
                downlink.receive(packet, Duration::ZERO);
            }
        }
        downlink.pump();
        let played: Vec<i16> = (0..6)
            .flat_map(|_| play_one(&mut speaker, &mut downlink))
            .collect();
        let stats = downlink.stats();
        assert_eq!(stats.recovered, 1);
        assert_eq!(stats.concealed, 0);
        assert_eq!(stats.decoded, 7);
        // The rebuilt frame is voice, not a hole.
        let rebuilt = 4;
        assert!(rms(&played[rebuilt * FRAME_SAMPLES..][..FRAME_SAMPLES]) > 3_000.0);
    }

    // Two losses in a row: the first has no FEC to use (its next packet is lost too), the
    // second does.
    #[test]
    fn conceals_a_loss_when_the_next_packet_is_lost_too() {
        let (mut speaker, mut downlink) = downlink();
        for packet in packets(8) {
            if packet.sequence != 3 && packet.sequence != 4 {
                downlink.receive(packet, Duration::ZERO);
            }
        }
        downlink.pump();
        for _ in 0..6 {
            play_one(&mut speaker, &mut downlink);
        }
        let stats = downlink.stats();
        assert_eq!(stats.concealed, 1);
        assert_eq!(stats.recovered, 1);
        assert_eq!(stats.underruns, 0);
    }

    // Once the call has started, a dry buffer is concealed rather than cut to silence.
    #[test]
    fn conceals_when_the_buffer_runs_dry_mid_call() {
        let (mut speaker, mut downlink) = downlink();
        for packet in packets(4) {
            downlink.receive(packet, Duration::ZERO);
        }
        downlink.pump();
        let played: Vec<i16> = (0..8)
            .flat_map(|_| play_one(&mut speaker, &mut downlink))
            .collect();
        let stats = downlink.stats();
        assert_eq!(stats.decoded, 4);
        assert!(stats.underruns >= 3, "{} underruns", stats.underruns);
        assert_eq!(stats.concealed, stats.underruns);
        // The first concealed frame carries the voice on.
        let first_concealed = 4;
        assert!(rms(&played[first_concealed * FRAME_SAMPLES..][..FRAME_SAMPLES]) > 1_000.0);
    }

    // A packet that is not 20 ms of Opus is concealed, never played as it is.
    #[test]
    fn conceals_a_packet_the_decoder_cannot_use() {
        let (mut speaker, mut downlink) = downlink();
        let mut packets = packets(6);
        packets[3].payload.clear();
        for packet in packets {
            downlink.receive(packet, Duration::ZERO);
        }
        downlink.pump();
        for _ in 0..4 {
            play_one(&mut speaker, &mut downlink);
        }
        let stats = downlink.stats();
        assert_eq!(stats.decode_errors, 1);
        assert_eq!(stats.decoded, 5);
        assert_eq!(stats.concealed, 1);
    }
}
