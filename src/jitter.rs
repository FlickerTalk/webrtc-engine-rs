//! The jitter buffer: packets come from the network out of order and at uneven times; the speaker needs them in order,
//! one every 20 ms.

use std::collections::BTreeMap;

/// What the playout clock gets every 20 ms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Playout {
    /// The next frame, in order.
    Frame(Vec<u8>),
    /// The next frame was lost: the decoder conceals it.
    Missing,
    /// Nothing is due yet: still filling, or the buffer ran dry.
    Waiting,
}

/// Counters since the buffer was created, plus the current depth.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// Packets pushed, whatever became of them.
    pub received: u64,
    /// Frames handed to the decoder.
    pub played: u64,
    /// `Missing` frames the decoder had to conceal.
    pub concealed: u64,
    /// Packets dropped because their turn had already been played or concealed.
    pub late: u64,
    /// Packets dropped because the same sequence was already buffered.
    pub duplicate: u64,
    /// Packets received but thrown away unplayed: over capacity, flushed by a resync, or a stray
    /// far packet.
    pub discarded: u64,
    /// Packets stored now.
    pub depth: usize,
    /// Depth the playout aims for, in 20 ms frames.
    pub target: usize,
}

/// Playout depth before the first frame, in 20 ms frames.
const INITIAL_TARGET: usize = 2;

/// A sequence this far from the highest one (1 s of audio) is not reordering but a restart
/// or a long outage.
const MAX_JUMP: i64 = 50;

/// Most packets kept (1 s of audio): far above the 10-frame maximum depth, so it only bites
/// when nobody drains the buffer.
const CAPACITY: usize = 50;

pub struct JitterBuffer {
    /// Keyed by extended sequence, so the order survives the 16-bit wrap.
    packets: BTreeMap<i64, Vec<u8>>,
    /// Extended sequence the playout clock takes next; `None` until the first playout.
    next: Option<i64>,
    /// Highest extended sequence received: the reference for unwrapping the next one.
    highest: Option<i64>,
    playing: bool,
    target: usize,
    counts: Stats,
    /// A far packet held until the next sequence confirms the jump.
    suspect: Option<(u16, Vec<u8>)>,
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self {
            packets: BTreeMap::new(),
            next: None,
            highest: None,
            playing: false,
            target: INITIAL_TARGET,
            counts: Stats::default(),
            suspect: None,
        }
    }

    pub fn push(&mut self, sequence: u16, payload: Vec<u8>) {
        self.counts.received += 1;
        let far = self
            .highest
            .is_some_and(|highest| distance(highest, sequence).abs() > MAX_JUMP);
        if far {
            match self.suspect.take() {
                Some((held_sequence, held)) if sequence == held_sequence.wrapping_add(1) => {
                    self.resync();
                    self.store(held_sequence, held);
                }
                stray => {
                    if stray.is_some() {
                        self.counts.discarded += 1;
                    }
                    self.suspect = Some((sequence, payload));
                    return;
                }
            }
        } else if self.suspect.take().is_some() {
            self.counts.discarded += 1;
        }
        self.store(sequence, payload);
    }

    fn store(&mut self, sequence: u16, payload: Vec<u8>) {
        let extended = self.extend(sequence);
        if self.next.is_some_and(|next| extended < next) {
            self.counts.late += 1;
            return;
        }
        if self.packets.contains_key(&extended) {
            self.counts.duplicate += 1;
            return;
        }
        self.packets.insert(extended, payload);
        if self.packets.len() > CAPACITY
            && let Some((oldest, _)) = self.packets.pop_first()
        {
            self.counts.discarded += 1;
            // Its turn is gone: a copy arriving later is late, not a hole to conceal.
            self.next = self.next.map(|next| next.max(oldest + 1));
        }
    }

    /// Starts over from the next packet: what is buffered belongs to the old stream.
    fn resync(&mut self) {
        self.counts.discarded += self.packets.len() as u64;
        self.packets.clear();
        self.next = None;
        self.highest = None;
        self.playing = false;
    }

    fn extend(&mut self, sequence: u16) -> i64 {
        let extended = match self.highest {
            None => i64::from(sequence),
            Some(highest) => highest + distance(highest, sequence),
        };
        self.highest = Some(
            self.highest
                .map_or(extended, |highest| highest.max(extended)),
        );
        extended
    }

    pub fn stats(&self) -> Stats {
        Stats {
            depth: self.packets.len(),
            target: self.target,
            ..self.counts.clone()
        }
    }

    /// Called by the playout clock every 20 ms.
    pub fn playout(&mut self) -> Playout {
        if !self.playing {
            let Some((&first, _)) = self.packets.first_key_value() else {
                return Playout::Waiting;
            };
            if self.packets.len() < self.target {
                return Playout::Waiting;
            }
            self.playing = true;
            self.next = Some(first);
        }
        let Some(next) = self.next else {
            return Playout::Waiting;
        };
        if let Some(payload) = self.packets.remove(&next) {
            self.next = Some(next + 1);
            self.counts.played += 1;
            return Playout::Frame(payload);
        }
        if self.packets.is_empty() {
            // Keep `next`: the packet may still arrive, late but in time to be played.
            self.playing = false;
            return Playout::Waiting;
        }
        self.next = Some(next + 1);
        self.counts.concealed += 1;
        Playout::Missing
    }

    /// `playout` for callers that only want frames: a missing frame and waiting both give `None`.
    pub fn pop(&mut self) -> Option<Vec<u8>> {
        match self.playout() {
            Playout::Frame(payload) => Some(payload),
            Playout::Missing | Playout::Waiting => None,
        }
    }
}

/// Signed distance from `highest` to `sequence`: as in RFC 3550, the one within half the
/// range (32768) is the right one, whichever side of the wrap it lands on.
fn distance(highest: i64, sequence: u16) -> i64 {
    // Truncating to u16 then i16 is the modular distance, by design.
    i64::from(sequence.wrapping_sub(highest as u16) as i16)
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The network may swap two packets: they come back in teh order they were sent.
    #[test]
    fn gives_packets_back_in_order() {
        let mut buffer = JitterBuffer::new();
        buffer.push(2, vec![2]);
        buffer.push(1, vec![1]);
        assert_eq!(buffer.pop(), Some(vec![1]));
        assert_eq!(buffer.pop(), Some(vec![2]));
    }

    // Playout starts only once the target depth (two frames by default) is buffered.
    #[test]
    fn waits_until_the_target_depth_is_buffered() {
        let mut buffer = JitterBuffer::new();
        assert_eq!(buffer.playout(), Playout::Waiting);
        buffer.push(10, vec![10]);
        assert_eq!(buffer.playout(), Playout::Waiting);
        buffer.push(11, vec![11]);
        assert_eq!(buffer.playout(), Playout::Frame(vec![10]));
        assert_eq!(buffer.playout(), Playout::Frame(vec![11]));
    }

    // A hole with later packets behind it is a loss: the decoder must conceal it, not wait.
    #[test]
    fn reports_a_lost_packet_as_missing() {
        let mut buffer = JitterBuffer::new();
        buffer.push(1, vec![1]);
        buffer.push(3, vec![3]);
        assert_eq!(buffer.playout(), Playout::Frame(vec![1]));
        assert_eq!(buffer.playout(), Playout::Missing);
        assert_eq!(buffer.playout(), Playout::Frame(vec![3]));
    }

    // Running dry is not a loss yet: the next packet may only be late, so it waits and refills.
    #[test]
    fn waits_and_refills_when_the_buffer_runs_dry() {
        let mut buffer = JitterBuffer::new();
        buffer.push(1, vec![1]);
        buffer.push(2, vec![2]);
        assert_eq!(buffer.playout(), Playout::Frame(vec![1]));
        assert_eq!(buffer.playout(), Playout::Frame(vec![2]));
        assert_eq!(buffer.playout(), Playout::Waiting);
        buffer.push(3, vec![3]);
        assert_eq!(buffer.playout(), Playout::Waiting);
        buffer.push(4, vec![4]);
        assert_eq!(buffer.playout(), Playout::Frame(vec![3]));
    }

    // Sequence numbers wrap after 65535: 0 then comes after 65535, not before it.
    #[test]
    fn orders_packets_across_the_sequence_wrap() {
        let mut buffer = JitterBuffer::new();
        buffer.push(0, vec![0]);
        buffer.push(65535, vec![255]);
        buffer.push(1, vec![1]);
        assert_eq!(buffer.pop(), Some(vec![255]));
        assert_eq!(buffer.pop(), Some(vec![0]));
        assert_eq!(buffer.pop(), Some(vec![1]));
    }

    // A packet the network delivered twice plays once.
    #[test]
    fn drops_duplicates() {
        let mut buffer = JitterBuffer::new();
        buffer.push(1, vec![1]);
        buffer.push(1, vec![9]);
        buffer.push(2, vec![2]);
        assert_eq!(buffer.pop(), Some(vec![1]));
        assert_eq!(buffer.pop(), Some(vec![2]));
        let stats = buffer.stats();
        assert_eq!(stats.received, 3);
        assert_eq!(stats.duplicate, 1);
        assert_eq!(stats.played, 2);
    }

    // Once its turn has been played or concealed, a packet is useless: playing it later would
    // put the audio out of order.
    #[test]
    fn drops_packets_whose_turn_has_passed() {
        let mut buffer = JitterBuffer::new();
        buffer.push(1, vec![1]);
        buffer.push(3, vec![3]);
        assert_eq!(buffer.playout(), Playout::Frame(vec![1]));
        assert_eq!(buffer.playout(), Playout::Missing);
        buffer.push(2, vec![2]);
        buffer.push(1, vec![1]);
        assert_eq!(buffer.playout(), Playout::Frame(vec![3]));
        assert_eq!(buffer.playout(), Playout::Waiting);
        let stats = buffer.stats();
        assert_eq!(stats.late, 2);
        assert_eq!(stats.concealed, 1);
        assert_eq!(stats.played, 2);
        assert_eq!(stats.depth, 0);
    }

    #[test]
    fn stats_show_the_current_depth_and_target() {
        let mut buffer = JitterBuffer::new();
        buffer.push(1, vec![1]);
        buffer.push(2, vec![2]);
        let stats = buffer.stats();
        assert_eq!(stats.depth, 2);
        assert_eq!(stats.target, 2);
    }

    // A sender that restarts jumps to an unrelated sequence: start over from it instead of
    // concealing every sequence in between.
    #[test]
    fn resynchronises_on_a_far_jump_forward() {
        let mut buffer = JitterBuffer::new();
        buffer.push(1, vec![1]);
        buffer.push(2, vec![2]);
        buffer.push(3, vec![3]);
        assert_eq!(buffer.playout(), Playout::Frame(vec![1]));
        buffer.push(5000, vec![50]);
        buffer.push(5001, vec![51]);
        assert_eq!(buffer.playout(), Playout::Frame(vec![50]));
        assert_eq!(buffer.playout(), Playout::Frame(vec![51]));
        let stats = buffer.stats();
        assert_eq!(stats.concealed, 0);
        assert_eq!(stats.discarded, 2);
    }

    // The same for a restart that lands behind what was played: it is not a late packet.
    #[test]
    fn resynchronises_on_a_far_jump_backward() {
        let mut buffer = JitterBuffer::new();
        buffer.push(40000, vec![40]);
        buffer.push(40001, vec![41]);
        assert_eq!(buffer.playout(), Playout::Frame(vec![40]));
        assert_eq!(buffer.playout(), Playout::Frame(vec![41]));
        buffer.push(5, vec![5]);
        buffer.push(6, vec![6]);
        assert_eq!(buffer.playout(), Playout::Frame(vec![5]));
        assert_eq!(buffer.playout(), Playout::Frame(vec![6]));
        assert_eq!(buffer.stats().late, 0);
    }

    // One stray far packet is not a restart: the jump counts only when the next sequence
    // confirms it, as in RFC 3550 A.1.
    #[test]
    fn ignores_a_single_stray_far_packet() {
        let mut buffer = JitterBuffer::new();
        buffer.push(1, vec![1]);
        buffer.push(2, vec![2]);
        assert_eq!(buffer.playout(), Playout::Frame(vec![1]));
        buffer.push(30000, vec![30]);
        buffer.push(3, vec![3]);
        assert_eq!(buffer.playout(), Playout::Frame(vec![2]));
        assert_eq!(buffer.playout(), Playout::Frame(vec![3]));
        assert_eq!(buffer.playout(), Playout::Waiting);
        assert_eq!(buffer.stats().discarded, 1);
    }

    // Memory is bounded: when full, the oldest packet goes, and playout moves on to what is
    // left instead of concealing the dropped ones.
    #[test]
    fn keeps_at_most_capacity_packets() {
        let mut buffer = JitterBuffer::new();
        buffer.push(1, vec![1]);
        buffer.push(2, vec![2]);
        assert_eq!(buffer.playout(), Playout::Frame(vec![1]));
        for sequence in 3..=60 {
            buffer.push(sequence, vec![sequence as u8]);
        }
        let stats = buffer.stats();
        assert_eq!(stats.depth, CAPACITY);
        assert_eq!(stats.discarded, 9);
        assert_eq!(buffer.playout(), Playout::Frame(vec![11]));
        assert_eq!(buffer.stats().concealed, 0);
    }
}
