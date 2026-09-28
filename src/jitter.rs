//! The jitter buffer: packets come from the network out of order and at uneven times; the speaker needs them in order,
//! one every 20 ms.

use std::collections::BTreeMap;

pub struct JitterBuffer {
    packets: BTreeMap<u16, Vec<u8>>,
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self {
            packets: BTreeMap::new(),
        }
    }

    pub fn push(&mut self, sequence: u16, payload: Vec<u8>) {
        self.packets.insert(sequence, payload);
    }

    pub fn pop(&mut self) -> Option<Vec<u8>> {
        self.packets.pop_first().map(|(_, payload)| payload)
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
}
