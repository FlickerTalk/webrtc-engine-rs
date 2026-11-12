use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Playout {
    Frame(Vec<u8>),
    Missing,
    /// Nothing is due yet. No sequence was used up, but mid-call the decoder still conceals.
    Waiting,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub received: u64,
    pub played: u64,
    pub concealed: u64,
    pub late: u64,
    pub duplicate: u64,
    pub discarded: u64,
    pub depth: usize,
    /// In 20 ms frames.
    pub target: usize,
    /// RFC 3550 interarrival jitter. Only `push_at` feeds it.
    pub jitter: Duration,
}

const INITIAL_TARGET: usize = 2;

/// 1 s of audio. Further away it is a restart or a long outage, not reordering.
const MAX_JUMP: i64 = 50;

const MIN_TARGET: usize = 1;
const MAX_TARGET: usize = 10;

const SHRINK_AFTER: usize = 50;

/// RFC 3550 jitter is a mean deviation. Three of them cover most delay peaks.
const JITTER_MARGIN: f64 = 3.0;

const CAPACITY: usize = 50;

const FRAME_MS: f64 = 20.0;

#[derive(Default)]
struct JitterEstimate {
    jitter_ms: f64,
    /// Only differences matter, so the two clocks need no common origin.
    last_transit_ms: Option<f64>,
}

impl JitterEstimate {
    fn update(&mut self, extended: i64, arrival: Duration) {
        let transit_ms = arrival.as_secs_f64() * 1000.0 - extended as f64 * FRAME_MS;
        if let Some(last) = self.last_transit_ms {
            let difference = (transit_ms - last).abs();
            self.jitter_ms += (difference - self.jitter_ms) / 16.0;
        }
        self.last_transit_ms = Some(transit_ms);
    }
}

/// The depth adapts to `push_at` arrival times only. With `push` it stays at 2 frames.
pub struct JitterBuffer {
    packets: BTreeMap<i64, Vec<u8>>,
    next: Option<i64>,
    highest: Option<i64>,
    playing: bool,
    target: usize,
    counts: Stats,
    suspect: Option<(u16, Vec<u8>)>,
    estimate: JitterEstimate,
    desired: usize,
    calm: usize,
    above: usize,
    holds: usize,
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
            estimate: JitterEstimate::default(),
            desired: INITIAL_TARGET,
            calm: 0,
            above: 0,
            holds: 0,
        }
    }

    /// `arrival` can be on any monotonic clock of the caller's.
    pub fn push_at(&mut self, sequence: u16, payload: Vec<u8>, arrival: Duration) {
        self.receive(sequence, payload, Some(arrival));
    }

    pub fn push(&mut self, sequence: u16, payload: Vec<u8>) {
        self.receive(sequence, payload, None);
    }

    fn receive(&mut self, sequence: u16, payload: Vec<u8>, arrival: Option<Duration>) {
        self.counts.received += 1;
        let far = self
            .highest
            .is_some_and(|highest| distance(highest, sequence).abs() > MAX_JUMP);
        if far {
            match self.suspect.take() {
                Some((held_sequence, held)) if sequence == held_sequence.wrapping_add(1) => {
                    self.resync();
                    self.store(held_sequence, held, None);
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
        self.store(sequence, payload, arrival);
    }

    fn store(&mut self, sequence: u16, payload: Vec<u8>, arrival: Option<Duration>) {
        let extended = self.extend(sequence);
        if self.packets.contains_key(&extended) {
            self.counts.duplicate += 1;
            return;
        }
        // Late packets count too, being late is what jitter measures.
        if let Some(arrival) = arrival {
            self.estimate.update(extended, arrival);
            self.desired = desired_depth(self.estimate.jitter_ms);
            if self.desired > self.target {
                if self.playing {
                    self.holds += self.desired - self.target;
                }
                self.target = self.desired;
                self.calm = 0;
            }
        }
        if self.next.is_some_and(|next| extended < next) {
            self.counts.late += 1;
            return;
        }
        self.packets.insert(extended, payload);
        if self.packets.len() > CAPACITY {
            self.drop_oldest();
        }
    }

    fn drop_oldest(&mut self) {
        if let Some((oldest, _)) = self.packets.pop_first() {
            self.counts.discarded += 1;
            self.next = self.next.map(|next| next.max(oldest + 1));
        }
    }

    fn resync(&mut self) {
        self.counts.discarded += self.packets.len() as u64;
        self.packets.clear();
        self.next = None;
        self.highest = None;
        self.playing = false;
        self.holds = 0;
        self.estimate.last_transit_ms = None;
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
            jitter: Duration::try_from_secs_f64(self.estimate.jitter_ms / 1000.0)
                .unwrap_or_default(),
            ..self.counts.clone()
        }
    }

    /// Call it every 20 ms.
    pub fn playout(&mut self) -> Playout {
        self.shrink_when_calm();
        let playout = self.take_next();
        if self.playing {
            self.drain_above_target();
        } else {
            self.above = 0;
        }
        playout
    }

    fn take_next(&mut self) -> Playout {
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
        // A steady but late stream never refills to the new target by itself, so hold the playout.
        if self.holds > 0 {
            if self.packets.len() >= self.target {
                self.holds = 0;
            } else {
                self.holds -= 1;
                return Playout::Waiting;
            }
        }
        if let Some(payload) = self.packets.remove(&next) {
            self.next = Some(next + 1);
            self.counts.played += 1;
            return Playout::Frame(payload);
        }
        if self.packets.is_empty() {
            // Keep `next`, the packet may still arrive in time to be played.
            self.playing = false;
            self.holds = 0;
            return Playout::Waiting;
        }
        self.next = Some(next + 1);
        self.counts.concealed += 1;
        Playout::Missing
    }

    fn shrink_when_calm(&mut self) {
        if self.desired >= self.target {
            self.calm = 0;
            return;
        }
        self.calm += 1;
        if self.calm >= SHRINK_AFTER {
            self.target -= 1;
            self.calm = 0;
        }
    }

    fn drain_above_target(&mut self) {
        if self.packets.len() <= self.target {
            self.above = 0;
            return;
        }
        self.above += 1;
        if self.above >= SHRINK_AFTER {
            self.above = 0;
            self.drop_oldest();
        }
    }

    /// After a `Missing` this is the packet right after the lost one, whose FEC can rebuild it.
    pub fn peek_next(&self) -> Option<&[u8]> {
        let next = self.next?;
        self.packets.get(&next).map(Vec::as_slice)
    }

    pub fn pop(&mut self) -> Option<Vec<u8>> {
        match self.playout() {
            Playout::Frame(payload) => Some(payload),
            Playout::Missing | Playout::Waiting => None,
        }
    }
}

fn desired_depth(jitter_ms: f64) -> usize {
    let frames = (JITTER_MARGIN * jitter_ms / FRAME_MS).ceil() as usize;
    frames.clamp(MIN_TARGET, MAX_TARGET)
}

fn distance(highest: i64, sequence: u16) -> i64 {
    // The truncation is on purpose, it gives the modular distance.
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

    #[test]
    fn gives_packets_back_in_order() {
        let mut buffer = JitterBuffer::new();
        buffer.push(2, vec![2]);
        buffer.push(1, vec![1]);
        assert_eq!(buffer.pop(), Some(vec![1]));
        assert_eq!(buffer.pop(), Some(vec![2]));
    }

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

    #[test]
    fn reports_a_lost_packet_as_missing() {
        let mut buffer = JitterBuffer::new();
        buffer.push(1, vec![1]);
        buffer.push(3, vec![3]);
        assert_eq!(buffer.playout(), Playout::Frame(vec![1]));
        assert_eq!(buffer.playout(), Playout::Missing);
        assert_eq!(buffer.playout(), Playout::Frame(vec![3]));
    }

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

    #[test]
    fn peeks_at_the_packet_after_a_lost_one() {
        let mut buffer = JitterBuffer::new();
        buffer.push(1, vec![1]);
        buffer.push(3, vec![3]);
        buffer.push(4, vec![4]);
        assert_eq!(buffer.peek_next(), None);
        assert_eq!(buffer.playout(), Playout::Frame(vec![1]));
        assert_eq!(buffer.playout(), Playout::Missing);
        assert_eq!(buffer.peek_next(), Some(&[3][..]));
        assert_eq!(buffer.playout(), Playout::Frame(vec![3]));
        assert_eq!(buffer.peek_next(), Some(&[4][..]));
    }

    #[test]
    fn peeks_at_nothing_when_the_next_packet_is_missing_too() {
        let mut buffer = JitterBuffer::new();
        buffer.push(1, vec![1]);
        buffer.push(4, vec![4]);
        assert_eq!(buffer.playout(), Playout::Frame(vec![1]));
        assert_eq!(buffer.playout(), Playout::Missing);
        assert_eq!(buffer.peek_next(), None);
    }

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

    fn at(milliseconds: u64) -> Duration {
        Duration::from_millis(milliseconds)
    }

    fn assert_jitter_ms(buffer: &JitterBuffer, expected: f64) {
        let jitter = buffer.stats().jitter.as_secs_f64() * 1000.0;
        assert!(
            (jitter - expected).abs() < 1e-6,
            "jitter {jitter} ms, expected {expected} ms"
        );
    }

    #[test]
    fn measures_rfc_3550_interarrival_jitter() {
        let mut buffer = JitterBuffer::new();
        buffer.push_at(0, vec![0], at(1000));
        buffer.push_at(1, vec![1], at(1020));
        buffer.push_at(2, vec![2], at(1040));
        assert_jitter_ms(&buffer, 0.0);
        buffer.push_at(3, vec![3], at(1070));
        assert_jitter_ms(&buffer, 0.625);
        buffer.push_at(4, vec![4], at(1080));
        assert_jitter_ms(&buffer, 1.2109375);
    }

    #[test]
    fn a_restart_is_not_jitter() {
        let mut buffer = JitterBuffer::new();
        buffer.push_at(1, vec![1], at(1000));
        buffer.push_at(2, vec![2], at(1020));
        buffer.push_at(5000, vec![50], at(1040));
        buffer.push_at(5001, vec![51], at(1060));
        buffer.push_at(5002, vec![52], at(1080));
        assert_jitter_ms(&buffer, 0.0);
    }

    fn every_other_delayed(sequences: std::ops::Range<u16>, delay_ms: u64) -> Vec<(u16, Duration)> {
        let mut arrivals: Vec<(u16, Duration)> = sequences
            .map(|sequence| {
                let delay = if sequence % 2 == 1 { delay_ms } else { 0 };
                (sequence, at(1000 + u64::from(sequence) * 20 + delay))
            })
            .collect();
        arrivals.sort_by_key(|&(_, arrival)| arrival);
        arrivals
    }

    #[test]
    fn grows_the_target_fast_when_jitter_rises() {
        let mut buffer = JitterBuffer::new();
        for (sequence, arrival) in every_other_delayed(0..20, 60) {
            buffer.push_at(sequence, vec![], arrival);
        }
        let target = buffer.stats().target;
        assert!((4..=MAX_TARGET).contains(&target), "target {target}");
    }

    #[test]
    fn never_grows_beyond_ten_frames() {
        let mut buffer = JitterBuffer::new();
        for (sequence, arrival) in every_other_delayed(0..40, 1000) {
            buffer.push_at(sequence, vec![], arrival);
        }
        assert_eq!(buffer.stats().target, MAX_TARGET);
    }

    #[test]
    fn shrinks_the_target_slowly_when_the_network_calms() {
        let mut buffer = JitterBuffer::new();
        for (sequence, arrival) in every_other_delayed(0..40, 200) {
            buffer.push_at(sequence, vec![], arrival);
        }
        let mut target = buffer.stats().target;
        assert_eq!(target, MAX_TARGET);
        let mut ticks_since_change = 0;
        for sequence in 40..1000u16 {
            buffer.push_at(sequence, vec![], at(1000 + u64::from(sequence) * 20 + 200));
            buffer.playout();
            ticks_since_change += 1;
            let now = buffer.stats().target;
            if now != target {
                assert_eq!(now, target - 1);
                assert!(
                    ticks_since_change >= SHRINK_AFTER,
                    "shrank after {ticks_since_change} ticks"
                );
                ticks_since_change = 0;
                target = now;
            }
        }
        assert_eq!(target, MIN_TARGET);
    }

    #[test]
    fn drains_depth_above_the_target_slowly() {
        let mut buffer = JitterBuffer::new();
        for sequence in 0..12u16 {
            buffer.push(sequence, vec![]);
        }
        let mut depth = buffer.stats().depth;
        let mut ticks_since_drop = 0;
        for sequence in 12..600u16 {
            buffer.push(sequence, vec![]);
            buffer.playout();
            ticks_since_drop += 1;
            let now = buffer.stats().depth;
            if now < depth {
                assert_eq!(now, depth - 1);
                assert!(
                    ticks_since_drop >= SHRINK_AFTER,
                    "dropped after {ticks_since_drop} ticks"
                );
                ticks_since_drop = 0;
                depth = now;
            }
        }
        let stats = buffer.stats();
        assert_eq!(stats.depth, stats.target);
        assert_eq!(stats.discarded, 10);
        assert_eq!(stats.concealed, 0);
    }

    fn run(buffer: &mut JitterBuffer, arrivals: &[(u16, Duration)], ticks: u64) -> Vec<Playout> {
        let mut pending = arrivals.iter().peekable();
        (0..ticks)
            .map(|tick| {
                let now = at(1000 + tick * 20);
                while let Some(&&(sequence, arrival)) = pending.peek() {
                    if arrival > now {
                        break;
                    }
                    buffer.push_at(sequence, vec![], arrival);
                    pending.next();
                }
                buffer.playout()
            })
            .collect()
    }

    #[test]
    fn stops_concealing_once_the_depth_has_grown() {
        let mut buffer = JitterBuffer::new();
        let arrivals = every_other_delayed(0..500, 60);
        let playouts = run(&mut buffer, &arrivals, 520);
        let missing_after_first_second = playouts[50..]
            .iter()
            .filter(|playout| **playout == Playout::Missing)
            .count();
        assert_eq!(missing_after_first_second, 0);
    }
}
