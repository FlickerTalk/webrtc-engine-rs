//! The engine's end of the rings: whole 20 ms frames in and out.

use super::ring::{RingConsumer, RingProducer};
use super::{FRAME_SAMPLES, Frame};

/// Microphone audio for the engine, one whole frame at a time.
pub struct CaptureFrames {
    consumer: RingConsumer,
}

impl CaptureFrames {
    /// Reads from the ring that a capture adapter writes to.
    pub fn new(consumer: RingConsumer) -> Self {
        Self { consumer }
    }

    /// Fills `frame` and returns true if a whole frame was waiting; otherwise leaves the ring
    /// alone and returns false.
    pub fn read_frame(&mut self, frame: &mut Frame) -> bool {
        // Only this end reads, so what `len` reports cannot shrink before the pop.
        if self.consumer.len() < FRAME_SAMPLES {
            return false;
        }
        self.consumer.pop(frame) == FRAME_SAMPLES
    }

    /// How many whole frames are waiting.
    pub fn frames_ready(&self) -> usize {
        self.consumer.len() / FRAME_SAMPLES
    }
}

/// Speaker audio from the engine, one whole frame at a time.
pub struct PlayoutFrames {
    producer: RingProducer,
}

impl PlayoutFrames {
    /// Writes to the ring that a playout adapter reads from.
    pub fn new(producer: RingProducer) -> Self {
        Self { producer }
    }

    /// Queues `frame` and returns true, or returns false without writing anything if the ring
    /// has no room for a whole frame.
    pub fn write_frame(&mut self, frame: &Frame) -> bool {
        // Only this end writes, so the free space cannot shrink before the push.
        if self.producer.free_len() < FRAME_SAMPLES {
            return false;
        }
        self.producer.push(frame) == FRAME_SAMPLES
    }

    /// How many whole frames still fit.
    pub fn frames_free(&self) -> usize {
        self.producer.free_len() / FRAME_SAMPLES
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::ring::ring;

    fn numbered_frame(first: i16) -> Frame {
        let mut frame = [0; FRAME_SAMPLES];
        for (n, sample) in frame.iter_mut().enumerate() {
            *sample = first.wrapping_add(n as i16);
        }
        frame
    }

    #[test]
    fn a_frame_is_read_only_once_it_is_whole() {
        let (mut producer, consumer) = ring(4 * FRAME_SAMPLES);
        let mut frames = CaptureFrames::new(consumer);
        let samples = numbered_frame(0);
        let mut frame = [0; FRAME_SAMPLES];

        producer.push(&samples[..500]);
        assert!(!frames.read_frame(&mut frame));
        assert_eq!(frames.frames_ready(), 0);

        producer.push(&samples[500..]);
        producer.push(&[7; 100]);
        assert_eq!(frames.frames_ready(), 1);
        assert!(frames.read_frame(&mut frame));
        assert_eq!(frame, samples);
        assert!(!frames.read_frame(&mut frame));
    }

    #[test]
    fn frames_written_come_out_of_the_ring_in_order() {
        let (producer, mut consumer) = ring(2 * FRAME_SAMPLES);
        let mut frames = PlayoutFrames::new(producer);
        assert_eq!(frames.frames_free(), 2);

        assert!(frames.write_frame(&numbered_frame(0)));
        assert!(frames.write_frame(&numbered_frame(1000)));
        assert_eq!(frames.frames_free(), 0);

        let mut out = [0; 2 * FRAME_SAMPLES];
        assert_eq!(consumer.pop(&mut out), 2 * FRAME_SAMPLES);
        assert_eq!(out[..FRAME_SAMPLES], numbered_frame(0));
        assert_eq!(out[FRAME_SAMPLES..], numbered_frame(1000));
    }

    #[test]
    fn a_frame_that_does_not_fit_is_not_written_at_all() {
        let (mut filler, consumer) = ring(2 * FRAME_SAMPLES);
        // The ring's capacity is rounded up, so leave exactly one sample too few.
        let fill = filler.free_len() - FRAME_SAMPLES + 1;
        assert_eq!(filler.push(&vec![1; fill]), fill);
        let mut frames = PlayoutFrames::new(filler);

        assert!(!frames.write_frame(&numbered_frame(0)));
        assert_eq!(consumer.len(), fill);
    }
}
