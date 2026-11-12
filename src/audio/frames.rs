use super::ring::{RingConsumer, RingProducer};
use super::{FRAME_SAMPLES, Frame};

pub struct CaptureFrames {
    consumer: RingConsumer,
}

impl CaptureFrames {
    pub fn new(consumer: RingConsumer) -> Self {
        Self { consumer }
    }

    pub fn read_frame(&mut self, frame: &mut Frame) -> bool {
        // Only this end reads, so what `len` reports cannot shrink before the pop.
        if self.consumer.len() < FRAME_SAMPLES {
            return false;
        }
        self.consumer.pop(frame) == FRAME_SAMPLES
    }

    pub fn frames_ready(&self) -> usize {
        self.consumer.len() / FRAME_SAMPLES
    }
}

pub struct PlayoutFrames {
    producer: RingProducer,
}

impl PlayoutFrames {
    pub fn new(producer: RingProducer) -> Self {
        Self { producer }
    }

    pub fn write_frame(&mut self, frame: &Frame) -> bool {
        // Only this end writes, so the free space can only grow.
        if self.producer.free_len() < FRAME_SAMPLES {
            return false;
        }
        self.producer.push(frame) == FRAME_SAMPLES
    }

    pub fn frames_free(&self) -> usize {
        self.producer.free_len() / FRAME_SAMPLES
    }

    pub fn samples_queued(&self) -> usize {
        self.producer.len()
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
    fn the_engine_sees_how_much_is_queued_for_the_speaker() {
        let (producer, mut consumer) = ring(4 * FRAME_SAMPLES);
        let mut frames = PlayoutFrames::new(producer);
        assert_eq!(frames.samples_queued(), 0);
        frames.write_frame(&numbered_frame(0));
        frames.write_frame(&numbered_frame(0));
        assert_eq!(frames.samples_queued(), 2 * FRAME_SAMPLES);
        consumer.pop(&mut [0; 300]);
        assert_eq!(frames.samples_queued(), 2 * FRAME_SAMPLES - 300);
    }

    #[test]
    fn a_frame_that_does_not_fit_is_not_written_at_all() {
        let (mut filler, consumer) = ring(2 * FRAME_SAMPLES);
        let fill = filler.free_len() - FRAME_SAMPLES + 1;
        assert_eq!(filler.push(&vec![1; fill]), fill);
        let mut frames = PlayoutFrames::new(filler);

        assert!(!frames.write_frame(&numbered_frame(0)));
        assert_eq!(consumer.len(), fill);
    }
}
