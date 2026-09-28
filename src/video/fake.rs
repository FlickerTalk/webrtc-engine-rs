//! A [`VideoSource`] and a [`VideoSink`] with no camera and no screen, for tests and demos.
//!
//! [`FakeSource`] sends tiny synthetic H.264 access units at the configured frame rate: real
//! NAL headers, an SPS and a PPS ahead of every IDR slice, sizes that follow the bitrate, and the
//! frame's number inside, so a test can tell frames apart ([`frame_index`]). [`FakeSink`] plays
//! the decoder: it takes nothing before a keyframe and fails on a delta frame that does not
//! follow the one before it, as a real decoder would show garbage.
//!
//! Both are driven on the Tokio clock, so tests can run them with the clock paused.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::{
    EncodedFrame, Facing, FrameSender, Rotation, SendOutcome, VideoConfig, VideoError, VideoSink,
    VideoSource,
};

/// The SPS a fake keyframe carries: Constrained Baseline, level 3.1.
pub const FAKE_SPS: &[u8] = &[0x67, 0x42, 0xE0, 0x1F, 0x8C, 0x8D, 0x40];
/// The PPS a fake keyframe carries.
pub const FAKE_PPS: &[u8] = &[0x68, 0xCE, 0x3C, 0x80];

/// The frame number a fake frame carries, if `data` is one.
pub fn frame_index(data: &[u8]) -> Option<u32> {
    use super::h264::{NAL_IDR, NAL_SLICE, nal_type, nal_units};
    let units = nal_units(data);
    let slice = units
        .iter()
        .find(|unit| matches!(nal_type(unit[0]), NAL_SLICE | NAL_IDR))?;
    let bytes = slice.get(1..1 + INDEX_BYTES)?;
    bytes.iter().try_fold(0u32, |index, &byte| {
        (byte & 0x80 != 0).then(|| (index << 7) | u32::from(byte & 0x7F))
    })
}

/// The frame number is written in 7-bit groups with the top bit set, so no byte is zero and no
/// start code can appear inside a frame.
const INDEX_BYTES: usize = 5;

fn index_bytes(index: u32) -> [u8; INDEX_BYTES] {
    std::array::from_fn(|byte| 0x80 | ((index >> (7 * (INDEX_BYTES - 1 - byte))) & 0x7F) as u8)
}

/// A fake access unit of about `size` bytes (three times that for a keyframe).
fn make_frame(index: u32, keyframe: bool, size: usize) -> Vec<u8> {
    const START_CODE: [u8; 4] = [0, 0, 0, 1];
    let mut data = Vec::with_capacity(size * 3);
    let size = if keyframe {
        for unit in [FAKE_SPS, FAKE_PPS] {
            data.extend_from_slice(&START_CODE);
            data.extend_from_slice(unit);
        }
        size * 3
    } else {
        size
    };
    data.extend_from_slice(&START_CODE);
    // nal_ref_idc 3 for an IDR slice, 2 for a reference P slice, as encoders mark them.
    data.push(if keyframe { 0x65 } else { 0x41 });
    data.extend_from_slice(&index_bytes(index));
    let filler = size.saturating_sub(data.len() + 1);
    data.extend((0..filler).map(|byte| (byte % 255) as u8 + 1));
    // The RBSP stop bit: a NAL unit never ends in a zero byte.
    data.push(0x80);
    data
}

/// What a [`FakeSource`] has been told and has done. Shared with the test through
/// [`FakeSource::probe`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceProbe {
    pub running: bool,
    pub facing: Option<Facing>,
    /// Frames the channel took.
    pub sent: u64,
    /// Frames the channel dropped.
    pub dropped: u64,
    pub keyframes: u64,
    pub keyframe_requests: u64,
    /// Every bitrate set, in order, clamped.
    pub bitrates: Vec<u32>,
}

/// A camera and encoder that make up their frames. See the [module docs](self).
pub struct FakeSource {
    shared: Arc<Mutex<SourceProbe>>,
    control: Arc<Mutex<Control>>,
    rotation: Rotation,
    /// When the first frame was captured: frame times count from it, across restarts.
    epoch: Option<Instant>,
    task: Option<JoinHandle<()>>,
}

#[derive(Debug, Default)]
struct Control {
    bitrate: u32,
    force_keyframe: bool,
    next_index: u32,
}

impl Default for FakeSource {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeSource {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(SourceProbe::default())),
            control: Arc::new(Mutex::new(Control::default())),
            rotation: Rotation::Deg0,
            epoch: None,
            task: None,
        }
    }

    /// Frames carry `rotation`.
    pub fn with_rotation(mut self, rotation: Rotation) -> Self {
        self.rotation = rotation;
        self
    }

    /// A live view of what the source has done.
    pub fn probe(&self) -> Arc<Mutex<SourceProbe>> {
        self.shared.clone()
    }
}

impl VideoSource for FakeSource {
    fn start(
        &mut self,
        config: VideoConfig,
        facing: Facing,
        out: FrameSender,
    ) -> Result<(), VideoError> {
        let runtime = Handle::try_current()
            .map_err(|_| VideoError::Backend("the fake source needs a Tokio runtime".to_owned()))?;
        self.stop()?;
        {
            let mut control = lock(&self.control);
            control.bitrate = super::clamp_bitrate(config.bitrate_bps);
            control.force_keyframe = true;
        }
        {
            let mut probe = lock(&self.shared);
            probe.running = true;
            probe.facing = Some(facing);
        }
        let epoch = *self.epoch.get_or_insert_with(Instant::now);
        let period = Duration::from_secs(1) / config.fps.max(1);
        let fps = config.fps.max(1) as usize;
        let rotation = self.rotation;
        let control = self.control.clone();
        let shared = self.shared.clone();
        self.task = Some(runtime.spawn(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let (index, keyframe, bitrate) = {
                    let mut control = lock(&control);
                    let index = control.next_index;
                    control.next_index = index.wrapping_add(1);
                    let keyframe = control.force_keyframe || out.keyframe_needed();
                    control.force_keyframe = false;
                    (index, keyframe, control.bitrate)
                };
                let size = (bitrate as usize / 8 / fps).max(16);
                let frame = EncodedFrame {
                    data: make_frame(index, keyframe, size),
                    keyframe,
                    timestamp: epoch.elapsed(),
                    rotation,
                };
                let outcome = out.try_send(frame);
                let mut probe = lock(&shared);
                match outcome {
                    SendOutcome::Sent => {
                        probe.sent += 1;
                        probe.keyframes += u64::from(keyframe);
                    }
                    SendOutcome::Dropped => probe.dropped += 1,
                    SendOutcome::Closed => {
                        probe.running = false;
                        break;
                    }
                }
            }
        }));
        Ok(())
    }

    fn stop(&mut self) -> Result<(), VideoError> {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        lock(&self.shared).running = false;
        Ok(())
    }

    fn request_keyframe(&mut self) {
        lock(&self.control).force_keyframe = true;
        lock(&self.shared).keyframe_requests += 1;
    }

    fn set_bitrate(&mut self, bps: u32) {
        let bps = super::clamp_bitrate(bps);
        lock(&self.control).bitrate = bps;
        lock(&self.shared).bitrates.push(bps);
    }

    fn switch_camera(&mut self, facing: Facing) -> Result<(), VideoError> {
        lock(&self.control).force_keyframe = true;
        lock(&self.shared).facing = Some(facing);
        Ok(())
    }
}

impl Drop for FakeSource {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// What a [`FakeSink`] has been given. Shared with the test through [`FakeSink::probe`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SinkProbe {
    pub running: bool,
    /// Frames it decoded, in order.
    pub shown: Vec<EncodedFrame>,
    /// Frames it dropped waiting for a keyframe.
    pub waiting: u64,
    /// Delta frames that did not follow the frame before them: garbage on a real screen.
    pub broken: u64,
}

/// A decoder and display that remember what they were given. See the [module docs](self).
pub struct FakeSink {
    shared: Arc<Mutex<SinkProbe>>,
    /// The number of the last frame decoded; `None` while waiting for a keyframe.
    last: Option<u32>,
}

impl Default for FakeSink {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeSink {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(SinkProbe::default())),
            last: None,
        }
    }

    /// A live view of what the sink has been given.
    pub fn probe(&self) -> Arc<Mutex<SinkProbe>> {
        self.shared.clone()
    }
}

impl VideoSink for FakeSink {
    fn start(&mut self) -> Result<(), VideoError> {
        self.last = None;
        lock(&self.shared).running = true;
        Ok(())
    }

    fn push(&mut self, frame: EncodedFrame) -> Result<(), VideoError> {
        let index = frame_index(&frame.data);
        let mut probe = lock(&self.shared);
        if !frame.keyframe {
            let Some(last) = self.last else {
                probe.waiting += 1;
                return Ok(());
            };
            if index != Some(last.wrapping_add(1)) {
                probe.broken += 1;
                self.last = None;
                return Err(VideoError::Backend("reference frame missing".to_owned()));
            }
        }
        // A frame without a number still decodes; the next delta frame will not follow it.
        self.last = index;
        probe.shown.push(frame);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), VideoError> {
        lock(&self.shared).running = false;
        Ok(())
    }
}

fn lock<T>(shared: &Mutex<T>) -> MutexGuard<'_, T> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::h264::{NAL_IDR, NAL_PPS, NAL_SLICE, NAL_SPS, nal_type, nal_units};
    use crate::video::{FrameReceiver, frame_channel};

    const CONFIG: VideoConfig = VideoConfig {
        width: 320,
        height: 240,
        fps: 10,
        bitrate_bps: 240_000,
    };

    fn drain(receiver: &FrameReceiver) -> Vec<EncodedFrame> {
        std::iter::from_fn(|| receiver.try_recv().ok()).collect()
    }

    fn types(frame: &EncodedFrame) -> Vec<u8> {
        nal_units(&frame.data)
            .iter()
            .map(|unit| nal_type(unit[0]))
            .collect()
    }

    async fn run_for(duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    #[tokio::test(start_paused = true)]
    async fn sends_frames_at_the_frame_rate_starting_with_a_keyframe() {
        let mut source = FakeSource::new().with_rotation(Rotation::Deg90);
        let probe = source.probe();
        let (sender, receiver) = frame_channel(100);
        source.start(CONFIG, Facing::Front, sender).unwrap();
        run_for(Duration::from_millis(1_050)).await;
        let frames = drain(&receiver);

        assert!((10..=11).contains(&frames.len()), "{}", frames.len());
        assert!(frames[0].keyframe);
        assert_eq!(types(&frames[0]), vec![NAL_SPS, NAL_PPS, NAL_IDR]);
        assert!(frames[1..].iter().all(|frame| !frame.keyframe));
        assert_eq!(types(&frames[1]), vec![NAL_SLICE]);
        for (index, frame) in frames.iter().enumerate() {
            assert_eq!(frame_index(&frame.data), Some(index as u32));
            assert_eq!(frame.rotation, Rotation::Deg90);
            assert_eq!(crate::video::h264::is_keyframe(&frame.data), frame.keyframe);
        }
        let step = frames[2].timestamp - frames[1].timestamp;
        assert_eq!(step, Duration::from_millis(100));
        let probe = lock(&probe).clone();
        assert!(probe.running);
        assert_eq!(probe.facing, Some(Facing::Front));
        assert_eq!(probe.sent, frames.len() as u64);
        assert_eq!(probe.keyframes, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn frame_sizes_follow_the_bitrate() {
        let mut source = FakeSource::new();
        let (sender, receiver) = frame_channel(100);
        source.start(CONFIG, Facing::Front, sender).unwrap();
        run_for(Duration::from_millis(250)).await;
        let before = drain(&receiver);
        // 240 kbit/s at 10 fps: 3000 bytes a frame; keyframes three times that.
        assert!(
            (2_900..=3_100).contains(&before[1].data.len()),
            "{}",
            before[1].data.len()
        );
        assert!(before[0].data.len() > 8_000);

        source.set_bitrate(480_000);
        run_for(Duration::from_millis(200)).await;
        let after = drain(&receiver);
        assert!(
            (5_900..=6_100).contains(&after[0].data.len()),
            "{}",
            after[0].data.len()
        );
        assert_eq!(lock(&source.probe()).bitrates, vec![480_000]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_keyframe_request_makes_the_next_frame_a_keyframe() {
        let mut source = FakeSource::new();
        let probe = source.probe();
        let (sender, receiver) = frame_channel(100);
        source.start(CONFIG, Facing::Front, sender).unwrap();
        run_for(Duration::from_millis(250)).await;
        drain(&receiver);

        source.request_keyframe();
        run_for(Duration::from_millis(100)).await;
        let frames = drain(&receiver);
        assert!(frames[0].keyframe);
        assert_eq!(lock(&probe).keyframe_requests, 1);
        assert_eq!(lock(&probe).keyframes, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_channel_owing_a_keyframe_gets_one() {
        let mut source = FakeSource::new();
        let (sender, receiver) = frame_channel(1);
        source.start(CONFIG, Facing::Front, sender).unwrap();
        // Nobody reads: the second frame does not fit, and a keyframe is owed.
        run_for(Duration::from_millis(250)).await;
        assert!(receiver.try_recv().is_ok_and(|frame| frame.keyframe));
        run_for(Duration::from_millis(100)).await;
        assert!(receiver.try_recv().is_ok_and(|frame| frame.keyframe));
        assert!(lock(&source.probe()).dropped >= 1);
    }

    #[tokio::test(start_paused = true)]
    async fn switching_camera_sends_a_keyframe_from_the_other_one() {
        let mut source = FakeSource::new();
        let (sender, receiver) = frame_channel(100);
        source.start(CONFIG, Facing::Front, sender).unwrap();
        run_for(Duration::from_millis(250)).await;
        drain(&receiver);

        source.switch_camera(Facing::Back).unwrap();
        run_for(Duration::from_millis(100)).await;
        assert!(drain(&receiver)[0].keyframe);
        assert_eq!(lock(&source.probe()).facing, Some(Facing::Back));
    }

    #[tokio::test(start_paused = true)]
    async fn a_stopped_source_sends_nothing() {
        let mut source = FakeSource::new();
        let (sender, receiver) = frame_channel(100);
        source.start(CONFIG, Facing::Front, sender).unwrap();
        run_for(Duration::from_millis(250)).await;
        source.stop().unwrap();
        drain(&receiver);
        run_for(Duration::from_millis(500)).await;
        assert!(drain(&receiver).is_empty());
        assert!(!lock(&source.probe()).running);
        source.stop().unwrap();
    }

    #[test]
    fn starting_outside_a_runtime_fails() {
        let mut source = FakeSource::new();
        let (sender, _receiver) = frame_channel(1);
        assert!(matches!(
            source.start(CONFIG, Facing::Front, sender),
            Err(VideoError::Backend(_))
        ));
    }

    fn fake_frame(index: u32, keyframe: bool) -> EncodedFrame {
        let mut data = Vec::new();
        if keyframe {
            for unit in [FAKE_SPS, FAKE_PPS] {
                data.extend_from_slice(&[0, 0, 0, 1]);
                data.extend_from_slice(unit);
            }
        }
        data.extend_from_slice(&[0, 0, 0, 1, if keyframe { 0x65 } else { 0x41 }]);
        data.extend_from_slice(&index_bytes(index));
        data.push(0x80);
        EncodedFrame {
            data,
            keyframe,
            timestamp: Duration::from_millis(u64::from(index) * 100),
            rotation: Rotation::Deg0,
        }
    }

    #[test]
    fn reads_the_index_of_a_fake_frame() {
        assert_eq!(frame_index(&fake_frame(0, true).data), Some(0));
        assert_eq!(frame_index(&fake_frame(300_000, false).data), Some(300_000));
        assert_eq!(frame_index(&[0, 0, 0, 1, 0x41]), None);
        assert_eq!(frame_index(&[]), None);
    }

    #[test]
    fn the_sink_shows_nothing_before_a_keyframe_then_every_frame_in_order() {
        let mut sink = FakeSink::new();
        let probe = sink.probe();
        sink.start().unwrap();
        sink.push(fake_frame(0, false)).unwrap();
        for index in 1..4 {
            sink.push(fake_frame(index, index == 1)).unwrap();
        }
        let probe = lock(&probe).clone();
        assert!(probe.running);
        assert_eq!(probe.waiting, 1);
        let shown: Vec<_> = probe
            .shown
            .iter()
            .filter_map(|f| frame_index(&f.data))
            .collect();
        assert_eq!(shown, vec![1, 2, 3]);
        assert_eq!(probe.broken, 0);
    }

    #[test]
    fn the_sink_fails_on_a_missing_reference_and_waits_for_a_keyframe() {
        let mut sink = FakeSink::new();
        let probe = sink.probe();
        sink.start().unwrap();
        sink.push(fake_frame(0, true)).unwrap();
        assert!(sink.push(fake_frame(2, false)).is_err());
        sink.push(fake_frame(3, false)).unwrap();
        sink.push(fake_frame(4, true)).unwrap();
        sink.push(fake_frame(5, false)).unwrap();
        sink.stop().unwrap();

        let probe = lock(&probe).clone();
        assert!(!probe.running);
        assert_eq!(probe.broken, 1);
        assert_eq!(probe.waiting, 1);
        let shown: Vec<_> = probe
            .shown
            .iter()
            .filter_map(|f| frame_index(&f.data))
            .collect();
        assert_eq!(shown, vec![0, 4, 5]);
    }
}
