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

/// Playout depth before the first frame, in 20 ms frames.
const INITIAL_TARGET: usize = 2;

pub struct JitterBuffer {
    packets: BTreeMap<u16, Vec<u8>>,
    /// Sequence the playout clock takes next; `None` until the first playout.
    next: Option<u16>,
    playing: bool,
    target: usize,
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self {
            packets: BTreeMap::new(),
            next: None,
            playing: false,
            target: INITIAL_TARGET,
        }
    }

    pub fn push(&mut self, sequence: u16, payload: Vec<u8>) {
        self.packets.insert(sequence, payload);
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
            self.next = Some(next.wrapping_add(1));
            return Playout::Frame(payload);
        }
        if self.packets.is_empty() {
            // Keep `next`: the packet may still arrive, late but in time to be played.
            self.playing = false;
            return Playout::Waiting;
        }
        self.next = Some(next.wrapping_add(1));
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
}
