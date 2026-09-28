//! Desktop video, for trying calls on a computer (feature `desktop`, macOS): the camera through
//! `nokhwa` (AVFoundation) encoded with [`SoftwareEncoder`](super::openh264::SoftwareEncoder),
//! and a window through `minifb` fed by [`SoftwareDecoder`](super::openh264::SoftwareDecoder).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::openh264::{I420Frame, SoftwareDecoder, SoftwareEncoder};
use super::{
    EncodedFrame, FrameSender, Rotation, SendOutcome, VideoConfig, VideoError, VideoSink,
    clamp_bitrate,
};

/// What the engine asks of a running camera, read by its capture thread before each frame.
#[derive(Debug)]
struct Controls {
    keyframe: AtomicBool,
    bitrate_bps: AtomicU32,
}

impl Controls {
    fn new(bitrate_bps: u32) -> Self {
        Self {
            keyframe: AtomicBool::new(false),
            bitrate_bps: AtomicU32::new(clamp_bitrate(bitrate_bps)),
        }
    }

    fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::Relaxed);
    }

    fn set_bitrate(&self, bps: u32) {
        self.bitrate_bps
            .store(clamp_bitrate(bps), Ordering::Relaxed);
    }
}

/// The camera thread's work once it has a picture: encode it as asked and send it, and show it
/// in the local preview. Kept apart from the camera so that it can be tested without one.
struct CapturePipeline {
    encoder: SoftwareEncoder,
    out: FrameSender,
    controls: Arc<Controls>,
    preview: FrameSlot,
}

impl CapturePipeline {
    fn new(
        config: VideoConfig,
        out: FrameSender,
        controls: Arc<Controls>,
        preview: FrameSlot,
    ) -> Result<Self, VideoError> {
        Ok(Self {
            encoder: SoftwareEncoder::new(config)?,
            out,
            controls,
            preview,
        })
    }

    fn process(
        &mut self,
        picture: I420Frame,
        timestamp: Duration,
    ) -> Result<SendOutcome, VideoError> {
        let bitrate = self.controls.bitrate_bps.load(Ordering::Relaxed);
        if bitrate != self.encoder.bitrate() {
            self.encoder.set_bitrate(bitrate);
        }
        if self.controls.keyframe.swap(false, Ordering::Relaxed) || self.out.keyframe_needed() {
            self.encoder.force_keyframe();
        }
        let outcome = match self.encoder.encode(&picture, timestamp)? {
            Some(frame) => self.out.try_send(frame),
            None => SendOutcome::Dropped,
        };
        // A desktop camera gives upright pictures.
        self.preview.publish(picture, Rotation::Deg0);
        Ok(outcome)
    }
}

/// A decoded or captured picture and how to turn it to show it upright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    pub frame: Arc<I420Frame>,
    pub rotation: Rotation,
}

/// The latest picture of a video, shared between the thread that makes it and the window that
/// shows it. Only the newest is kept: a window that falls behind skips pictures, it never
/// queues them.
#[derive(Debug, Clone, Default)]
pub struct FrameSlot {
    latest: Arc<Mutex<Option<Picture>>>,
}

impl FrameSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the picture.
    pub fn publish(&self, frame: I420Frame, rotation: Rotation) {
        *self.lock() = Some(Picture {
            frame: Arc::new(frame),
            rotation,
        });
    }

    /// The newest picture, if there is one.
    pub fn latest(&self) -> Option<Picture> {
        self.lock().clone()
    }

    /// Empties the slot: the window shows black.
    pub fn clear(&self) {
        *self.lock() = None;
    }

    /// A panic elsewhere cannot leave a picture half written (it is replaced whole), so a
    /// poisoned lock is still good to use.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Picture>> {
        self.latest.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What a [`WindowSink`] did so far. Counters only, never content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SinkStats {
    /// Frames pushed.
    pub received: u64,
    /// Frames decoded into a picture.
    pub decoded: u64,
    /// Delta frames dropped while waiting for a keyframe.
    pub dropped: u64,
    /// Frames the decoder rejected.
    pub errors: u64,
    /// The decoder is waiting for a keyframe: the sender should be asked for one (a PLI).
    pub keyframe_needed: bool,
}

#[derive(Debug, Default)]
struct SinkCounters {
    received: AtomicU64,
    decoded: AtomicU64,
    dropped: AtomicU64,
    errors: AtomicU64,
    keyframe_needed: AtomicBool,
}

/// Reads a [`WindowSink`]'s counters from any thread, also once the sink is boxed.
#[derive(Debug, Clone)]
pub struct SinkMonitor {
    counters: Arc<SinkCounters>,
}

impl SinkMonitor {
    pub fn stats(&self) -> SinkStats {
        let counters = &self.counters;
        SinkStats {
            received: counters.received.load(Ordering::Relaxed),
            decoded: counters.decoded.load(Ordering::Relaxed),
            dropped: counters.dropped.load(Ordering::Relaxed),
            errors: counters.errors.load(Ordering::Relaxed),
            keyframe_needed: counters.keyframe_needed.load(Ordering::Relaxed),
        }
    }
}

/// A [`VideoSink`] that decodes with OpenH264 and hands the pictures to a window through a
/// [`FrameSlot`]. It decodes in [`VideoSink::push`], on the caller's thread (a few milliseconds
/// for VGA); the window draws on the main thread (see [`VideoWindow`]).
pub struct WindowSink {
    slot: FrameSlot,
    decoder: Option<SoftwareDecoder>,
    counters: Arc<SinkCounters>,
}

impl WindowSink {
    /// A sink that shows its pictures in `slot`.
    pub fn new(slot: FrameSlot) -> Self {
        Self {
            slot,
            decoder: None,
            counters: Arc::default(),
        }
    }

    pub fn monitor(&self) -> SinkMonitor {
        SinkMonitor {
            counters: self.counters.clone(),
        }
    }
}

impl VideoSink for WindowSink {
    fn start(&mut self) -> Result<(), VideoError> {
        let decoder = SoftwareDecoder::new()?;
        self.counters
            .keyframe_needed
            .store(decoder.needs_keyframe(), Ordering::Relaxed);
        self.decoder = Some(decoder);
        Ok(())
    }

    /// Decodes `frame` and shows it. An error means the frame could not be decoded: ask the
    /// sender for a keyframe. Delta frames until then are dropped (`Ok`).
    fn push(&mut self, frame: EncodedFrame) -> Result<(), VideoError> {
        let decoder = self
            .decoder
            .as_mut()
            .ok_or_else(|| VideoError::Backend("the window sink is not running".to_owned()))?;
        let counters = &self.counters;
        counters.received.fetch_add(1, Ordering::Relaxed);
        let result = decoder.decode(&frame.data);
        counters
            .keyframe_needed
            .store(decoder.needs_keyframe(), Ordering::Relaxed);
        match result {
            Ok(Some(picture)) => {
                counters.decoded.fetch_add(1, Ordering::Relaxed);
                self.slot.publish(picture, frame.rotation);
                Ok(())
            }
            Ok(None) => {
                counters.dropped.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(error) => {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                Err(error)
            }
        }
    }

    fn stop(&mut self) -> Result<(), VideoError> {
        self.decoder = None;
        self.slot.clear();
        Ok(())
    }
}

/// Repacks a packed 4:2:2 picture in `yuvs` order (Y0 Cb Y1 Cr, video range: what AVFoundation
/// gives for YUYV) into I420, averaging the chroma of each pair of rows. `stride` is the bytes
/// in a row, padding included.
pub fn yuyv_to_i420(
    width: u32,
    height: u32,
    stride: usize,
    data: &[u8],
) -> Result<I420Frame, VideoError> {
    let (columns, rows) = (width as usize, height as usize);
    let row_bytes = columns * 2;
    let needed = rows
        .saturating_sub(1)
        .checked_mul(stride)
        .and_then(|bytes| bytes.checked_add(row_bytes));
    if stride < row_bytes || needed.is_none_or(|needed| data.len() < needed) || rows % 2 != 0 {
        return Err(VideoError::Unsupported);
    }
    let row = |index: usize| &data[index * stride..index * stride + row_bytes];
    let mut y = Vec::with_capacity(columns * rows);
    for index in 0..rows {
        y.extend(row(index).iter().step_by(2));
    }
    let chroma = (columns / 2) * (rows / 2);
    let (mut u, mut v) = (Vec::with_capacity(chroma), Vec::with_capacity(chroma));
    for index in (0..rows).step_by(2) {
        let (top, bottom) = (row(index), row(index + 1));
        for (a, b) in top.chunks_exact(4).zip(bottom.chunks_exact(4)) {
            let average = |i: usize| ((u16::from(a[i]) + u16::from(b[i]) + 1) / 2) as u8;
            u.push(average(1));
            v.push(average(3));
        }
    }
    I420Frame::from_planes(width, height, y, u, v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::frame_channel;

    /// `count` encoded frames of the test pattern, with a keyframe forced at each of `keyframes`.
    fn encoded(count: u32, keyframes: &[u32]) -> Vec<EncodedFrame> {
        let mut encoder = SoftwareEncoder::new(VideoConfig {
            width: 320,
            height: 240,
            ..VideoConfig::default()
        })
        .unwrap();
        (0..count)
            .map(|index| {
                if keyframes.contains(&index) {
                    encoder.force_keyframe();
                }
                let picture = I420Frame::test_pattern(320, 240, index).unwrap();
                let at = Duration::from_millis(u64::from(index) * 33);
                encoder.encode(&picture, at).unwrap().unwrap()
            })
            .collect()
    }

    fn pattern(index: u32) -> I420Frame {
        I420Frame::test_pattern(320, 240, index).unwrap()
    }

    fn at(index: u32) -> Duration {
        Duration::from_millis(u64::from(index) * 33)
    }

    fn pipeline(capacity: usize) -> (CapturePipeline, crate::video::FrameReceiver, FrameSlot) {
        let (out, frames) = frame_channel(capacity);
        let preview = FrameSlot::new();
        let config = VideoConfig {
            width: 320,
            height: 240,
            ..VideoConfig::default()
        };
        let controls = Arc::new(Controls::new(config.bitrate_bps));
        let pipeline = CapturePipeline::new(config, out, controls, preview.clone()).unwrap();
        (pipeline, frames, preview)
    }

    #[test]
    fn the_camera_sends_a_keyframe_first_and_shows_its_preview() {
        let (mut pipeline, frames, preview) = pipeline(3);
        assert_eq!(pipeline.process(pattern(0), at(0)), Ok(SendOutcome::Sent));
        let first = frames.try_recv().unwrap();
        assert!(first.keyframe);
        assert_eq!(first.timestamp, at(0));
        assert_eq!(preview.latest().unwrap().frame.as_ref(), &pattern(0));

        assert_eq!(pipeline.process(pattern(1), at(1)), Ok(SendOutcome::Sent));
        assert!(!frames.try_recv().unwrap().keyframe);
    }

    #[test]
    fn a_requested_keyframe_is_the_next_frame() {
        let (mut pipeline, frames, _) = pipeline(3);
        for index in 0..2 {
            let _ = pipeline.process(pattern(index), at(index));
            frames.try_recv().unwrap();
        }
        pipeline.controls.request_keyframe();
        let _ = pipeline.process(pattern(2), at(2));
        assert!(frames.try_recv().unwrap().keyframe);
        let _ = pipeline.process(pattern(3), at(3));
        assert!(!frames.try_recv().unwrap().keyframe, "asked for once");
    }

    #[test]
    fn a_keyframe_owed_by_the_channel_is_the_next_frame() {
        let (mut pipeline, frames, _) = pipeline(1);
        assert_eq!(pipeline.process(pattern(0), at(0)), Ok(SendOutcome::Sent));
        assert_eq!(
            pipeline.process(pattern(1), at(1)),
            Ok(SendOutcome::Dropped)
        );
        frames.try_recv().unwrap();
        assert_eq!(pipeline.process(pattern(2), at(2)), Ok(SendOutcome::Sent));
        assert!(frames.try_recv().unwrap().keyframe);
    }

    #[test]
    fn a_new_bitrate_reaches_the_encoder() {
        let (mut pipeline, _frames, _) = pipeline(3);
        let _ = pipeline.process(pattern(0), at(0));
        pipeline.controls.set_bitrate(300_000);
        let _ = pipeline.process(pattern(1), at(1));
        assert_eq!(pipeline.encoder.bitrate(), 300_000);
    }

    #[test]
    fn the_camera_learns_when_the_engine_is_gone() {
        let (mut pipeline, frames, _) = pipeline(3);
        drop(frames);
        assert_eq!(pipeline.process(pattern(0), at(0)), Ok(SendOutcome::Closed));
    }

    #[test]
    fn a_started_sink_shows_each_decoded_frame_in_its_slot() {
        let slot = FrameSlot::new();
        let mut sink = WindowSink::new(slot.clone());
        let monitor = sink.monitor();
        sink.start().unwrap();
        assert_eq!(slot.latest(), None);

        let mut frames = encoded(3, &[]);
        frames[2].rotation = Rotation::Deg90;
        for frame in frames {
            sink.push(frame).unwrap();
        }
        let picture = slot.latest().unwrap();
        assert_eq!((picture.frame.width(), picture.frame.height()), (320, 240));
        assert_eq!(picture.rotation, Rotation::Deg90);
        assert_eq!(
            monitor.stats(),
            SinkStats {
                received: 3,
                decoded: 3,
                ..SinkStats::default()
            }
        );

        sink.stop().unwrap();
        assert_eq!(slot.latest(), None, "stopping clears the window");
    }

    #[test]
    fn a_sink_that_is_not_running_refuses_frames() {
        let mut sink = WindowSink::new(FrameSlot::new());
        let frame = encoded(1, &[]).remove(0);
        assert!(sink.push(frame.clone()).is_err());
        sink.start().unwrap();
        sink.stop().unwrap();
        sink.stop().unwrap();
        assert!(sink.push(frame).is_err());
    }

    #[test]
    fn after_a_lost_frame_the_sink_waits_for_a_keyframe_and_says_so() {
        let frames = encoded(10, &[8]);
        let slot = FrameSlot::new();
        let mut sink = WindowSink::new(slot.clone());
        let monitor = sink.monitor();
        sink.start().unwrap();
        for frame in &frames[..5] {
            sink.push(frame.clone()).unwrap();
        }
        // Frame 5 is lost.
        assert!(sink.push(frames[6].clone()).is_err());
        assert!(monitor.stats().keyframe_needed);
        sink.push(frames[7].clone()).unwrap();
        assert_eq!(
            monitor.stats(),
            SinkStats {
                received: 7,
                decoded: 5,
                dropped: 1,
                errors: 1,
                keyframe_needed: true,
            }
        );
        sink.push(frames[8].clone()).unwrap();
        sink.push(frames[9].clone()).unwrap();
        let stats = monitor.stats();
        assert_eq!(stats.decoded, 7);
        assert!(!stats.keyframe_needed);
    }

    #[test]
    fn yuyv_is_repacked_into_i420() {
        // 4×2 with two bytes of padding at the end of each row.
        #[rustfmt::skip]
        let yuyv = [
            10, 100, 20, 200,  30, 110, 40, 210,  0, 0,
            50, 102, 60, 202,  70, 112, 80, 212,  0, 0,
        ];
        let frame = yuyv_to_i420(4, 2, 10, &yuyv).unwrap();
        assert_eq!(frame.y(), [10, 20, 30, 40, 50, 60, 70, 80]);
        assert_eq!(frame.u(), [101, 111]);
        assert_eq!(frame.v(), [201, 211]);
    }

    #[test]
    fn a_short_yuyv_buffer_is_rejected() {
        assert_eq!(
            yuyv_to_i420(4, 2, 8, &[0; 15]),
            Err(VideoError::Unsupported)
        );
        assert_eq!(
            yuyv_to_i420(4, 2, 6, &[0; 16]),
            Err(VideoError::Unsupported)
        );
    }
}
