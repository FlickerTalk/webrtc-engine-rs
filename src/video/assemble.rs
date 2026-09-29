//! RTP packets back into whole H.264 access units.
//!
//! Packets come as the network delivers them: late, out of order, twice or never.
//! [`FrameAssembler`] holds them until a frame is whole (every sequence number from its first
//! packet to the one with the marker bit) and hands frames out **in decoding order**: a delta
//! frame only follows the frame before it, since it cannot be decoded without it. A gap that
//! stays open longer than a retransmission takes ([`MAX_WAIT`]) gives up the frames behind it
//! and asks for a keyframe; so does a delta frame nobody can decode. A whole keyframe is always
//! taken, and everything older than it is dropped.
//!
//! Sans-IO: the caller stamps every packet with its arrival time and says what time it is.

use std::collections::BTreeMap;
use std::time::Duration;

use super::EncodedFrame;
use super::packet::{VIDEO_CLOCK_RATE, VideoPacket};

/// How long a gap in the sequence may stay open before the frames behind it are given up: time
/// for a NACK to bring the packet back on a mobile round trip.
pub const MAX_WAIT: Duration = Duration::from_millis(200);

/// The shortest time between two keyframe requests while waiting for a keyframe: long enough
/// for the other side to answer the first one, so one loss does not bring a burst of keyframes.
pub const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(500);

/// What the assembler has done so far. Counters only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssemblerStats {
    /// Whole frames handed out.
    pub frames: u64,
    /// Of them, keyframes.
    pub keyframes: u64,
    /// Frames given up: incomplete, undecodable or corrupt.
    pub dropped: u64,
    /// Keyframe requests made ([`FrameAssembler::wants_keyframe`] said yes).
    pub keyframe_requests: u64,
    /// Packets that came after their frame was handed out or given up.
    pub late: u64,
    /// Packets received twice.
    pub duplicates: u64,
}

/// Reassembles H.264 access units from RTP packets. See the [module docs](self).
#[derive(Debug)]
pub struct FrameAssembler {
    /// Packets waiting for their frame, by extended sequence number.
    packets: BTreeMap<i64, Held>,
    /// Highest extended sequence number seen: the reference to unwrap the next one.
    highest: Option<i64>,
    /// The sequence number that starts the next frame in decoding order: the one after the
    /// last frame handed out or given up.
    next: Option<i64>,
    /// Delta frames are useless until a keyframe comes. True at the start.
    need_keyframe: bool,
    /// A keyframe request is owed to the other side.
    request_pending: bool,
    last_request: Option<Duration>,
    /// The last RTP timestamp handed out, and its extended value.
    last_timestamp: Option<(u32, i64)>,
    first_timestamp: Option<i64>,
    /// The RTP timestamp of the last frame counted as dropped, so a frame dropped in pieces is
    /// counted once.
    last_dropped: Option<u32>,
    stats: AssemblerStats,
}

#[derive(Debug)]
struct Held {
    packet: VideoPacket,
    arrival: Duration,
}

/// Consecutive packets of one frame: the same timestamp, no sequence number missing, ending at
/// the first marker bit.
#[derive(Debug, Clone, Copy)]
struct Run {
    first: i64,
    last: i64,
    marker: bool,
}

const NAL_STAP_A: u8 = 24;
const NAL_FU_A: u8 = 28;

impl Default for FrameAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self {
            packets: BTreeMap::new(),
            highest: None,
            next: None,
            need_keyframe: true,
            request_pending: false,
            last_request: None,
            last_timestamp: None,
            first_timestamp: None,
            last_dropped: None,
            stats: AssemblerStats::default(),
        }
    }

    /// Takes a packet that arrived at `arrival`.
    pub fn push(&mut self, packet: VideoPacket, arrival: Duration) {
        let sequence = self.extend(packet.sequence);
        if self.next.is_some_and(|next| sequence < next) {
            self.stats.late += 1;
            return;
        }
        if self.packets.contains_key(&sequence) {
            self.stats.duplicates += 1;
            return;
        }
        self.packets.insert(sequence, Held { packet, arrival });
    }

    /// The next whole frame in decoding order, if there is one at `now`.
    pub fn pop(&mut self, now: Duration) -> Option<EncodedFrame> {
        loop {
            let runs = self.runs();
            let next_frame = runs
                .iter()
                .find(|run| !self.need_keyframe && Some(run.first) == self.next && run.marker);
            let keyframe = runs.iter().find(|run| {
                run.marker
                    && (self.starts_for_sure(run) || self.starts_with_parameter_sets(run))
                    && self.types_in(run).contains(&super::h264::NAL_IDR)
            });
            if let Some(&run) = next_frame.or(keyframe) {
                match self.hand_out(run) {
                    Some(frame) => return Some(frame),
                    None => continue,
                }
            }
            // Delta frames wait for a keyframe: one may still come before them, reordered. They
            // go when it does, or when they have waited too long.
            if self.need_keyframe && runs.iter().any(|run| self.is_delta(run)) {
                self.request_pending = true;
            }
            if self.give_up_stale(now) {
                self.ask_for_keyframe();
                continue;
            }
            return None;
        }
    }

    /// Whether to send a keyframe request (PLI) now. Says yes at most once every
    /// [`KEYFRAME_REQUEST_INTERVAL`] while a keyframe is owed.
    pub fn wants_keyframe(&mut self, now: Duration) -> bool {
        let due = self
            .last_request
            .is_none_or(|last| now.saturating_sub(last) >= KEYFRAME_REQUEST_INTERVAL);
        if self.request_pending && due {
            self.last_request = Some(now);
            self.stats.keyframe_requests += 1;
            true
        } else {
            false
        }
    }

    /// The decoder cannot go on: drop delta frames until a keyframe comes, and ask for one.
    pub fn request_keyframe(&mut self) {
        self.ask_for_keyframe();
    }

    pub fn stats(&self) -> AssemblerStats {
        self.stats
    }

    fn ask_for_keyframe(&mut self) {
        self.need_keyframe = true;
        self.request_pending = true;
    }

    fn extend(&mut self, sequence: u16) -> i64 {
        // Start far from zero, so a stream that begins by going backwards stays positive.
        let highest = self.highest.unwrap_or(i64::from(sequence) + (1 << 16));
        let extended = highest + i64::from(sequence.wrapping_sub(highest as u16) as i16);
        self.highest = Some(highest.max(extended));
        extended
    }

    fn runs(&self) -> Vec<Run> {
        let mut runs: Vec<Run> = Vec::new();
        let mut previous: Option<(i64, u32, bool)> = None;
        for (&sequence, held) in &self.packets {
            let packet = &held.packet;
            let continues = previous.is_some_and(|(last, timestamp, marker)| {
                last + 1 == sequence && timestamp == packet.timestamp && !marker
            });
            match runs.last_mut() {
                Some(run) if continues => {
                    run.last = sequence;
                    run.marker = packet.marker;
                }
                _ => runs.push(Run {
                    first: sequence,
                    last: sequence,
                    marker: packet.marker,
                }),
            }
            previous = Some((sequence, packet.timestamp, packet.marker));
        }
        runs
    }

    /// Whether nothing of the run's frame can come before its first packet: the packet before
    /// it belongs to another frame, or ended the last frame handed out.
    fn starts_for_sure(&self, run: &Run) -> bool {
        Some(run.first) == self.next || self.packets.contains_key(&(run.first - 1))
    }

    /// Whether the run opens with an SPS (maybe behind an access unit delimiter), as a keyframe
    /// does: then no packet of it came before, whatever was lost.
    fn starts_with_parameter_sets(&self, run: &Run) -> bool {
        use super::h264::{NAL_AUD, NAL_SPS};
        self.packets
            .get(&run.first)
            .and_then(|held| payload_types(&held.packet.payload).first().copied())
            .is_some_and(|first| first == NAL_SPS || first == NAL_AUD)
    }

    fn types_in(&self, run: &Run) -> Vec<u8> {
        self.packets
            .range(run.first..=run.last)
            .flat_map(|(_, held)| payload_types(&held.packet.payload))
            .collect()
    }

    /// Whether the run belongs to a delta frame, whole or not: it has a slice of a non-IDR
    /// picture, and a picture is either IDR or not.
    fn is_delta(&self, run: &Run) -> bool {
        use super::h264::{NAL_IDR, NAL_SLICE};
        let types = self.types_in(run);
        types.contains(&NAL_SLICE) && !types.contains(&NAL_IDR)
    }

    /// Hands out the frame in `run`, dropping every older packet. `None` if its payloads do not
    /// make an access unit: then it is dropped and a keyframe is owed.
    fn hand_out(&mut self, run: Run) -> Option<EncodedFrame> {
        self.discard(i64::MIN..=run.first - 1);
        let held: Vec<Held> = (run.first..=run.last)
            .filter_map(|sequence| self.packets.remove(&sequence))
            .collect();
        self.next = Some(run.last + 1);
        let packets: Vec<VideoPacket> = held.into_iter().map(|held| held.packet).collect();
        let Some(data) = depacketize(&packets) else {
            self.count_dropped(packets.first().map(|packet| packet.timestamp));
            self.ask_for_keyframe();
            return None;
        };
        let timestamp = packets.first().map_or(0, |packet| packet.timestamp);
        let keyframe = super::h264::is_keyframe(&data);
        if keyframe {
            self.need_keyframe = false;
            self.request_pending = false;
            self.stats.keyframes += 1;
        }
        self.stats.frames += 1;
        Some(EncodedFrame {
            data,
            keyframe,
            timestamp: self.capture_time(timestamp),
            rotation: packets
                .iter()
                .rev()
                .find_map(|packet| packet.rotation)
                .unwrap_or_default(),
        })
    }

    fn capture_time(&mut self, timestamp: u32) -> Duration {
        let extended = match self.last_timestamp {
            Some((last, extended)) => extended + i64::from(timestamp.wrapping_sub(last) as i32),
            None => i64::from(timestamp),
        };
        self.last_timestamp = Some((timestamp, extended));
        let first = *self.first_timestamp.get_or_insert(extended);
        let ticks = u64::try_from(extended - first).unwrap_or(0);
        Duration::from_micros(ticks * 1_000_000 / u64::from(VIDEO_CLOCK_RATE))
    }

    /// Gives up the packets that have waited [`MAX_WAIT`]: the gap in front of them is not
    /// closing. Whether any were.
    fn give_up_stale(&mut self, now: Duration) -> bool {
        let stale: Vec<i64> = self
            .packets
            .iter()
            .filter(|(_, held)| held.arrival + MAX_WAIT <= now)
            .map(|(&sequence, _)| sequence)
            .collect();
        let Some(&newest) = stale.last() else {
            return false;
        };
        for sequence in stale {
            if let Some(held) = self.packets.remove(&sequence) {
                self.count_dropped(Some(held.packet.timestamp));
            }
        }
        self.next = Some(self.next.map_or(newest + 1, |next| next.max(newest + 1)));
        true
    }

    fn discard(&mut self, range: std::ops::RangeInclusive<i64>) {
        let doomed: Vec<i64> = self
            .packets
            .range(range)
            .map(|(&sequence, _)| sequence)
            .collect();
        for sequence in doomed {
            if let Some(held) = self.packets.remove(&sequence) {
                self.count_dropped(Some(held.packet.timestamp));
            }
        }
    }

    fn count_dropped(&mut self, timestamp: Option<u32>) {
        if timestamp.is_some() && timestamp != self.last_dropped {
            self.stats.dropped += 1;
            self.last_dropped = timestamp;
        }
    }
}

/// The NAL unit types a payload carries: its own, the ones aggregated in a STAP-A, or the
/// fragmented one of an FU-A.
fn payload_types(payload: &[u8]) -> Vec<u8> {
    use super::h264::nal_type;
    let Some(&header) = payload.first() else {
        return Vec::new();
    };
    match nal_type(header) {
        NAL_STAP_A => stap_a_units(payload)
            .unwrap_or_default()
            .iter()
            .filter_map(|unit| unit.first().map(|&byte| nal_type(byte)))
            .collect(),
        NAL_FU_A => payload
            .get(1)
            .map(|&byte| nal_type(byte))
            .into_iter()
            .collect(),
        other => vec![other],
    }
}

/// The NAL units in a STAP-A payload, or `None` if a length runs past its end.
fn stap_a_units(payload: &[u8]) -> Option<Vec<&[u8]>> {
    let mut units = Vec::new();
    let mut rest = payload.get(1..)?;
    while !rest.is_empty() {
        let (size, tail) = rest.split_first_chunk::<2>()?;
        let size = usize::from(u16::from_be_bytes(*size));
        if size == 0 || tail.len() < size {
            return None;
        }
        units.push(&tail[..size]);
        rest = &tail[size..];
    }
    Some(units)
}

/// The Annex-B access unit in a frame's payloads, or `None` if they do not make one.
fn depacketize(packets: &[VideoPacket]) -> Option<Vec<u8>> {
    use super::h264::nal_type;
    const START_CODE: [u8; 4] = [0, 0, 0, 1];
    let mut data = Vec::new();
    let mut fragmented = false;
    for packet in packets {
        let payload = &packet.payload;
        let &header = payload.first()?;
        match nal_type(header) {
            1..=23 if !fragmented => {
                data.extend_from_slice(&START_CODE);
                data.extend_from_slice(payload);
            }
            NAL_STAP_A if !fragmented => {
                for unit in stap_a_units(payload)? {
                    data.extend_from_slice(&START_CODE);
                    data.extend_from_slice(unit);
                }
            }
            NAL_FU_A => {
                let &fu_header = payload.get(1)?;
                let start = fu_header & 0x80 != 0;
                let end = fu_header & 0x40 != 0;
                if start == fragmented {
                    // A start inside a fragmented unit, or a middle without its start.
                    return None;
                }
                if start {
                    data.extend_from_slice(&START_CODE);
                    data.push((header & 0xE0) | nal_type(fu_header));
                }
                data.extend_from_slice(&payload[2..]);
                fragmented = !end;
            }
            _ => return None,
        }
    }
    (!fragmented && !data.is_empty()).then_some(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::Rotation;
    use crate::video::packet::Packetizer;

    const SPS: &[u8] = &[0x67, 0x42, 0xE0, 0x1F, 0x8C, 0x8D];
    const PPS: &[u8] = &[0x68, 0xCE, 0x3C, 0x80];
    const FRAME: Duration = Duration::from_millis(33);

    fn annex_b(units: &[&[u8]]) -> Vec<u8> {
        units
            .iter()
            .flat_map(|unit| [&[0, 0, 0, 1][..], unit].concat())
            .collect()
    }

    /// A NAL unit with `header`, then `len` bytes that tell frame `index` apart, none zero.
    fn unit(header: u8, index: usize, len: usize) -> Vec<u8> {
        std::iter::once(header)
            .chain((0..len).map(|i| ((index * 31 + i) % 255) as u8 + 1))
            .collect()
    }

    fn keyframe(index: usize, len: usize) -> EncodedFrame {
        EncodedFrame {
            data: annex_b(&[SPS, PPS, &unit(0x65, index, len)]),
            keyframe: true,
            timestamp: FRAME * index as u32,
            rotation: Rotation::Deg0,
        }
    }

    fn delta(index: usize, len: usize) -> EncodedFrame {
        EncodedFrame {
            data: annex_b(&[&unit(0x41, index, len)]),
            keyframe: false,
            timestamp: FRAME * index as u32,
            rotation: Rotation::Deg0,
        }
    }

    /// Frames as numbered packets, 100-byte payloads at most.
    fn packets(frames: &[EncodedFrame]) -> Vec<Vec<VideoPacket>> {
        let mut packetizer = Packetizer::new(65_530, 1_000).with_mtu(100);
        frames
            .iter()
            .map(|frame| packetizer.packetize(frame))
            .collect()
    }

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    fn push_all(assembler: &mut FrameAssembler, packets: &[VideoPacket], arrival: Duration) {
        for packet in packets {
            assembler.push(packet.clone(), arrival);
        }
    }

    fn pop_all(assembler: &mut FrameAssembler, now: Duration) -> Vec<EncodedFrame> {
        std::iter::from_fn(|| assembler.pop(now)).collect()
    }

    fn without_time(frames: &[EncodedFrame]) -> Vec<(Vec<u8>, bool)> {
        frames
            .iter()
            .map(|frame| (frame.data.clone(), frame.keyframe))
            .collect()
    }

    #[test]
    fn small_frames_come_back_whole() {
        let sent = [keyframe(0, 50), delta(1, 40), delta(2, 30)];
        let mut assembler = FrameAssembler::new();
        for (index, frame) in packets(&sent).iter().enumerate() {
            push_all(&mut assembler, frame, FRAME * index as u32);
        }
        let got = pop_all(&mut assembler, ms(100));
        assert_eq!(without_time(&got), without_time(&sent));
        assert_eq!(assembler.stats().frames, 3);
        assert_eq!(assembler.stats().keyframes, 1);
        assert!(!assembler.wants_keyframe(ms(100)));
    }

    #[test]
    fn big_frames_split_into_fu_a_come_back_whole() {
        let sent = [keyframe(0, 1_000), delta(1, 450)];
        let sent_packets = packets(&sent);
        assert!(sent_packets[0].len() > 5, "{:?}", sent_packets[0].len());
        let mut assembler = FrameAssembler::new();
        push_all(&mut assembler, &sent_packets.concat(), ms(0));
        assert_eq!(
            without_time(&pop_all(&mut assembler, ms(0))),
            without_time(&sent)
        );
    }

    #[test]
    fn sps_and_pps_come_back_ahead_of_the_idr() {
        let sent = keyframe(0, 20);
        let mut assembler = FrameAssembler::new();
        push_all(
            &mut assembler,
            &packets(std::slice::from_ref(&sent))[0],
            ms(0),
        );
        let got = assembler.pop(ms(0)).expect("the keyframe");
        assert_eq!(got.data, sent.data);
        assert!(got.keyframe);
    }

    #[test]
    fn frames_come_back_in_order_from_reordered_packets() {
        let sent = [keyframe(0, 300), delta(1, 250), delta(2, 250)];
        let mut all = packets(&sent).concat();
        // A fixed shuffle: every packet moves.
        all.reverse();
        all.swap(1, 4);
        let mut assembler = FrameAssembler::new();
        let mut got = Vec::new();
        for packet in all {
            assembler.push(packet, ms(10));
            got.extend(pop_all(&mut assembler, ms(10)));
        }
        assert_eq!(without_time(&got), without_time(&sent));
    }

    #[test]
    fn a_frame_waits_for_its_late_packet() {
        let sent = [keyframe(0, 20), delta(1, 250), delta(2, 20)];
        let sent_packets = packets(&sent);
        let mut assembler = FrameAssembler::new();
        push_all(&mut assembler, &sent_packets[0], ms(0));
        let (late, early) = sent_packets[1].split_first().expect("packets");
        push_all(&mut assembler, early, ms(33));
        push_all(&mut assembler, &sent_packets[2], ms(66));

        assert_eq!(
            pop_all(&mut assembler, ms(100)).len(),
            1,
            "only the keyframe"
        );
        assert!(!assembler.wants_keyframe(ms(100)));

        assembler.push(late.clone(), ms(150));
        let got = pop_all(&mut assembler, ms(150));
        assert_eq!(without_time(&got), without_time(&sent[1..]));
        assert_eq!(assembler.stats().dropped, 0);
    }

    #[test]
    fn a_lost_fragment_drops_the_frame_and_the_ones_after_it_and_asks_for_a_keyframe() {
        let sent = [
            keyframe(0, 20),
            delta(1, 250),
            delta(2, 20),
            delta(3, 20),
            keyframe(4, 20),
        ];
        let mut sent_packets = packets(&sent);
        sent_packets[1].remove(1);
        let mut assembler = FrameAssembler::new();
        for (index, frame) in sent_packets[..4].iter().enumerate() {
            push_all(&mut assembler, frame, FRAME * index as u32);
        }
        assert_eq!(pop_all(&mut assembler, ms(100)).len(), 1);
        assert!(
            !assembler.wants_keyframe(ms(100)),
            "still in time for a NACK"
        );

        // The gap has been open since the first packet behind it arrived, at 33 ms.
        assert!(pop_all(&mut assembler, ms(33) + MAX_WAIT).is_empty());
        assert!(assembler.wants_keyframe(ms(33) + MAX_WAIT));

        push_all(&mut assembler, &sent_packets[4], ms(300));
        let got = pop_all(&mut assembler, ms(300));
        assert_eq!(without_time(&got), without_time(&sent[4..]));
        assert_eq!(assembler.stats().frames, 2);
        assert_eq!(assembler.stats().dropped, 3);
        assert_eq!(assembler.stats().keyframe_requests, 1);
    }

    #[test]
    fn a_whole_keyframe_does_not_wait_for_an_open_gap() {
        let sent = [
            keyframe(0, 20),
            delta(1, 250),
            delta(2, 20),
            keyframe(3, 300),
        ];
        let mut sent_packets = packets(&sent);
        sent_packets[1].remove(0);
        let mut assembler = FrameAssembler::new();
        push_all(&mut assembler, &sent_packets.concat(), ms(0));

        let got = pop_all(&mut assembler, ms(0));
        assert_eq!(
            without_time(&got),
            without_time(&[sent[0].clone(), sent[3].clone()])
        );
        assert_eq!(assembler.stats().dropped, 2);
        assert!(!assembler.wants_keyframe(ms(0)));
    }

    #[test]
    fn keyframe_requests_repeat_at_most_every_interval_until_a_keyframe_comes() {
        let sent = [delta(0, 20), delta(1, 20), keyframe(2, 20)];
        let sent_packets = packets(&sent);
        let mut assembler = FrameAssembler::new();

        // The stream starts with delta frames: nothing can decode them.
        push_all(&mut assembler, &sent_packets[0], ms(0));
        assert_eq!(assembler.pop(ms(0)), None);
        assert!(assembler.wants_keyframe(ms(0)));
        assert!(!assembler.wants_keyframe(ms(10)));

        push_all(&mut assembler, &sent_packets[1], ms(33));
        assert_eq!(assembler.pop(ms(33)), None);
        assert!(!assembler.wants_keyframe(KEYFRAME_REQUEST_INTERVAL - ms(1)));
        assert!(assembler.wants_keyframe(KEYFRAME_REQUEST_INTERVAL));

        push_all(&mut assembler, &sent_packets[2], ms(600));
        assert!(assembler.pop(ms(600)).is_some_and(|frame| frame.keyframe));
        assert!(!assembler.wants_keyframe(ms(2_000)));
        assert_eq!(assembler.stats().dropped, 2);
        assert_eq!(assembler.stats().keyframe_requests, 2);
    }

    #[test]
    fn the_decoder_asking_for_a_keyframe_drops_delta_frames_until_one_comes() {
        let sent = [keyframe(0, 20), delta(1, 20), delta(2, 20), keyframe(3, 20)];
        let sent_packets = packets(&sent);
        let mut assembler = FrameAssembler::new();
        push_all(&mut assembler, &sent_packets[0], ms(0));
        assert!(assembler.pop(ms(0)).is_some());

        assembler.request_keyframe();
        assert!(assembler.wants_keyframe(ms(1)));
        push_all(&mut assembler, &sent_packets[1..3].concat(), ms(33));
        assert_eq!(assembler.pop(ms(33)), None);
        push_all(&mut assembler, &sent_packets[3], ms(100));
        assert!(assembler.pop(ms(100)).is_some_and(|frame| frame.keyframe));
        assert_eq!(assembler.stats().dropped, 2);
    }

    #[test]
    fn capture_time_differences_and_rotation_come_back() {
        let mut sent = [keyframe(0, 20), delta(1, 20), delta(3, 20)];
        sent[2].rotation = Rotation::Deg270;
        let mut assembler = FrameAssembler::new();
        push_all(&mut assembler, &packets(&sent).concat(), ms(0));
        let got = pop_all(&mut assembler, ms(0));

        let times: Vec<Duration> = got
            .iter()
            .map(|frame| frame.timestamp - got[0].timestamp)
            .collect();
        assert_eq!(times, vec![Duration::ZERO, FRAME, FRAME * 3]);
        let rotations: Vec<Rotation> = got.iter().map(|frame| frame.rotation).collect();
        assert_eq!(
            rotations,
            vec![Rotation::Deg0, Rotation::Deg0, Rotation::Deg270]
        );
    }

    #[test]
    fn a_frame_without_rotation_is_upright() {
        let mut sent_packets = packets(&[keyframe(0, 20)]);
        for packet in &mut sent_packets[0] {
            packet.rotation = None;
        }
        let mut assembler = FrameAssembler::new();
        push_all(&mut assembler, &sent_packets[0], ms(0));
        assert_eq!(
            assembler.pop(ms(0)).map(|frame| frame.rotation),
            Some(Rotation::Deg0)
        );
    }

    #[test]
    fn duplicates_and_late_packets_are_counted_and_ignored() {
        let sent = [keyframe(0, 20), delta(1, 20)];
        let sent_packets = packets(&sent);
        let mut assembler = FrameAssembler::new();
        push_all(&mut assembler, &sent_packets[0], ms(0));
        push_all(&mut assembler, &sent_packets[0], ms(1));
        assert_eq!(assembler.stats().duplicates, sent_packets[0].len() as u64);
        assert!(assembler.pop(ms(1)).is_some());

        push_all(&mut assembler, &sent_packets[0], ms(2));
        assert_eq!(assembler.stats().late, sent_packets[0].len() as u64);
        push_all(&mut assembler, &sent_packets[1], ms(33));
        assert_eq!(pop_all(&mut assembler, ms(33)).len(), 1);
        assert_eq!(assembler.stats().frames, 2);
    }

    #[test]
    fn a_corrupt_payload_drops_the_frame_and_asks_for_a_keyframe() {
        let sent = [keyframe(0, 20), delta(1, 20), keyframe(2, 20)];
        let mut sent_packets = packets(&sent);
        // NAL type 30 is not a payload WebRTC uses.
        sent_packets[1][0].payload[0] = 0x5E;
        let mut assembler = FrameAssembler::new();
        push_all(&mut assembler, &sent_packets[..2].concat(), ms(0));
        assert_eq!(pop_all(&mut assembler, ms(0)).len(), 1);
        assert!(assembler.wants_keyframe(ms(0)));
        assert_eq!(assembler.stats().dropped, 1);

        push_all(&mut assembler, &sent_packets[2], ms(66));
        assert!(assembler.pop(ms(66)).is_some_and(|frame| frame.keyframe));
    }

    #[test]
    fn the_clock_rate_is_90_khz() {
        assert_eq!(VIDEO_CLOCK_RATE, 90_000);
    }

    #[test]
    fn nothing_comes_out_of_an_empty_assembler() {
        let mut assembler = FrameAssembler::new();
        assert_eq!(assembler.pop(ms(10_000)), None);
        assert!(!assembler.wants_keyframe(ms(10_000)));
    }
}
