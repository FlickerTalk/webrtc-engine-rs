//! Single-producer single-consumer ring of samples between an audio callback and the engine.
//!
//! Lock-free and allocation-free after construction, so both ends are safe to use on a
//! real-time audio thread. Slots are `AtomicI16` accessed with relaxed ordering, which compiles
//! to plain loads and stores, so the ring needs no `unsafe`; the release/acquire pair on the
//! positions is what hands samples from one thread to the other.

use std::sync::Arc;
use std::sync::atomic::{AtomicI16, AtomicUsize, Ordering};

/// Creates a ring that holds at least `capacity` samples (rounded up to a power of two).
pub fn ring(capacity: usize) -> (RingProducer, RingConsumer) {
    // A power of two keeps `position & mask` right even when the positions wrap around usize.
    let capacity = capacity.max(1).next_power_of_two();
    let shared = Arc::new(Shared {
        slots: (0..capacity).map(|_| AtomicI16::new(0)).collect(),
        mask: capacity - 1,
        read: AtomicUsize::new(0),
        write: AtomicUsize::new(0),
    });
    (
        RingProducer {
            shared: Arc::clone(&shared),
        },
        RingConsumer { shared },
    )
}

struct Shared {
    slots: Box<[AtomicI16]>,
    mask: usize,
    /// Total samples ever read; only the consumer stores it.
    read: AtomicUsize,
    /// Total samples ever written; only the producer stores it.
    write: AtomicUsize,
}

impl Shared {
    fn capacity(&self) -> usize {
        self.slots.len()
    }

    // Each end owns one position, so it never moves under its owner and the difference cannot
    // underflow.
    fn len(&self) -> usize {
        let write = self.write.load(Ordering::Acquire);
        let read = self.read.load(Ordering::Acquire);
        write.wrapping_sub(read)
    }
}

/// The writing end.
pub struct RingProducer {
    shared: Arc<Shared>,
}

impl RingProducer {
    /// Writes as many of `samples` as fit and returns how many were written.
    pub fn push(&mut self, samples: &[i16]) -> usize {
        let shared = &*self.shared;
        let write = shared.write.load(Ordering::Relaxed);
        let read = shared.read.load(Ordering::Acquire);
        let free = shared.capacity() - write.wrapping_sub(read);
        let count = free.min(samples.len());
        for (offset, &sample) in samples[..count].iter().enumerate() {
            let slot = write.wrapping_add(offset) & shared.mask;
            shared.slots[slot].store(sample, Ordering::Relaxed);
        }
        shared
            .write
            .store(write.wrapping_add(count), Ordering::Release);
        count
    }

    /// How many samples can be written right now.
    pub fn free_len(&self) -> usize {
        self.shared.capacity() - self.shared.len()
    }

    /// How many samples are written and not read yet.
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    /// Whether the reader has taken everything written.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The reading end.
pub struct RingConsumer {
    shared: Arc<Shared>,
}

impl RingConsumer {
    /// Reads up to `out.len()` samples and returns how many were read.
    pub fn pop(&mut self, out: &mut [i16]) -> usize {
        let shared = &*self.shared;
        let read = shared.read.load(Ordering::Relaxed);
        let write = shared.write.load(Ordering::Acquire);
        let count = write.wrapping_sub(read).min(out.len());
        for (offset, sample) in out[..count].iter_mut().enumerate() {
            let slot = read.wrapping_add(offset) & shared.mask;
            *sample = shared.slots[slot].load(Ordering::Relaxed);
        }
        shared
            .read
            .store(read.wrapping_add(count), Ordering::Release);
        count
    }

    /// How many samples are waiting to be read.
    pub fn len(&self) -> usize {
        self.shared.len()
    }

    /// Whether there is nothing to read.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_come_out_in_the_order_they_went_in() {
        let (mut producer, mut consumer) = ring(8);
        assert_eq!(producer.push(&[1, 2, 3]), 3);
        let mut out = [0; 3];
        assert_eq!(consumer.pop(&mut out), 3);
        assert_eq!(out, [1, 2, 3]);
    }

    #[test]
    fn a_full_ring_takes_only_what_fits() {
        let (mut producer, mut consumer) = ring(4);
        assert_eq!(producer.push(&[1, 2, 3, 4, 5, 6]), 4);
        assert_eq!(producer.push(&[7]), 0);
        let mut out = [0; 8];
        assert_eq!(consumer.pop(&mut out), 4);
        assert_eq!(out[..4], [1, 2, 3, 4]);
    }

    #[test]
    fn capacity_is_rounded_up_to_a_power_of_two() {
        let (producer, _consumer) = ring(5);
        assert_eq!(producer.free_len(), 8);
    }

    #[test]
    fn fill_level_is_seen_from_both_ends() {
        let (mut producer, mut consumer) = ring(8);
        assert!(consumer.is_empty());
        producer.push(&[1, 2, 3]);
        assert_eq!(consumer.len(), 3);
        assert_eq!(producer.free_len(), 5);
        consumer.pop(&mut [0; 2]);
        assert_eq!(consumer.len(), 1);
        assert_eq!(producer.free_len(), 7);
    }

    // The writer paces itself on what the reader has not taken yet.
    #[test]
    fn the_producer_sees_what_is_still_unread() {
        let (mut producer, mut consumer) = ring(8);
        assert_eq!(producer.len(), 0);
        producer.push(&[1, 2, 3]);
        assert_eq!(producer.len(), 3);
        consumer.pop(&mut [0; 2]);
        assert_eq!(producer.len(), 1);
    }

    #[test]
    fn keeps_order_across_the_wrap_around() {
        let (mut producer, mut consumer) = ring(4);
        let mut next = 0i16;
        let mut expected = 0i16;
        for _ in 0..100 {
            let chunk = [next, next + 1, next + 2];
            assert_eq!(producer.push(&chunk), 3);
            next += 3;
            let mut out = [0; 3];
            assert_eq!(consumer.pop(&mut out), 3);
            for sample in out {
                assert_eq!(sample, expected);
                expected += 1;
            }
        }
    }

    // Two real threads with uneven chunk sizes: every sample must arrive once, in order.
    #[test]
    fn survives_a_producer_and_a_consumer_on_two_threads() {
        const TOTAL: usize = 2_000_000;
        let (mut producer, mut consumer) = ring(64);

        let writer = std::thread::spawn(move || {
            let mut sent = 0usize;
            let mut chunk = [0i16; 37];
            let mut size = 1;
            while sent < TOTAL {
                let len = size.min(TOTAL - sent);
                for (offset, sample) in chunk[..len].iter_mut().enumerate() {
                    *sample = (sent + offset) as u16 as i16;
                }
                let written = producer.push(&chunk[..len]);
                sent += written;
                if written == 0 {
                    std::thread::yield_now();
                }
                size = size % chunk.len() + 1;
            }
        });

        let mut received = 0usize;
        let mut out = [0i16; 23];
        let mut size = 1;
        while received < TOTAL {
            let read = consumer.pop(&mut out[..size]);
            for &sample in &out[..read] {
                assert_eq!(sample, received as u16 as i16, "sample {received}");
                received += 1;
            }
            if read == 0 {
                std::thread::yield_now();
            }
            size = size % out.len() + 1;
        }
        writer.join().unwrap();
        assert!(consumer.is_empty());
    }
}
