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
        shared.write.store(write.wrapping_add(count), Ordering::Release);
        count
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
        shared.read.store(read.wrapping_add(count), Ordering::Release);
        count
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
}
