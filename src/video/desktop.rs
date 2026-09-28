//! Desktop video, for trying calls on a computer (feature `desktop`, macOS): the camera through
//! `nokhwa` (AVFoundation) encoded with [`SoftwareEncoder`](super::openh264::SoftwareEncoder),
//! and a window through `minifb` fed by [`SoftwareDecoder`](super::openh264::SoftwareDecoder).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use minifb::{Key, Window, WindowOptions};
use nokhwa::pixel_format::YuyvFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
    Resolution,
};
use nokhwa::{Camera, NokhwaError};

use super::openh264::{I420Frame, SoftwareDecoder, SoftwareEncoder};
use super::{
    EncodedFrame, Facing, FrameSender, Rotation, SendOutcome, VideoConfig, VideoError, VideoSink,
    VideoSource, clamp_bitrate,
};

/// What the engine asks of a running camera, read by its capture thread before each frame.
#[derive(Debug)]
struct Controls {
    keyframe: AtomicBool,
    bitrate_bps: AtomicU32,
    stop: AtomicBool,
    /// The camera to move to, or [`NO_SWITCH`].
    switch_to: AtomicUsize,
}

const NO_SWITCH: usize = usize::MAX;

impl Controls {
    fn new(bitrate_bps: u32) -> Self {
        Self {
            keyframe: AtomicBool::new(false),
            bitrate_bps: AtomicU32::new(clamp_bitrate(bitrate_bps)),
            stop: AtomicBool::new(false),
            switch_to: AtomicUsize::new(NO_SWITCH),
        }
    }

    fn take_switch(&self) -> Option<usize> {
        match self.switch_to.swap(NO_SWITCH, Ordering::Relaxed) {
            NO_SWITCH => None,
            next => Some(next),
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

/// The camera after `current` among `count`, wrapping around; `None` with fewer than two.
fn next_camera(current: usize, count: usize) -> Option<usize> {
    (count >= 2).then(|| (current + 1) % count)
}

/// The Mac's camera as a [`VideoSource`]: nokhwa (AVFoundation) captures YUYV, which is
/// repacked into I420, encoded with OpenH264 and sent, all on a thread of its own.
///
/// Desktop cameras do not face a way: [`Facing`] is ignored, and
/// [`VideoSource::switch_camera`] moves to the next camera, if there is one. The first time, macOS
/// asks the user for the camera permission (for the terminal, when run from one).
pub struct CameraSource {
    preview: FrameSlot,
    /// Which camera, as a position in the list AVFoundation gives.
    camera: usize,
    running: Option<Running>,
}

struct Running {
    controls: Arc<Controls>,
    thread: JoinHandle<Result<(), VideoError>>,
}

/// How long `start` waits for the camera to open, and for the user to answer the permission
/// prompt.
const OPEN_TIMEOUT: Duration = Duration::from_secs(60);

impl CameraSource {
    pub fn new() -> Self {
        Self {
            preview: FrameSlot::new(),
            camera: 0,
            running: None,
        }
    }

    /// The local preview: each captured picture, as the sensor gives it (not mirrored).
    pub fn preview(&self) -> FrameSlot {
        self.preview.clone()
    }
}

impl Default for CameraSource {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CameraSource {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl VideoSource for CameraSource {
    fn start(
        &mut self,
        config: VideoConfig,
        _facing: Facing,
        out: FrameSender,
    ) -> Result<(), VideoError> {
        self.stop()?;
        ensure_permission()?;
        let cameras = cameras()?;
        let index = cameras
            .get(self.camera)
            .or(cameras.first())
            .cloned()
            .ok_or(VideoError::NoCamera)?;
        let controls = Arc::new(Controls::new(config.bitrate_bps));
        let pipeline = CapturePipeline::new(config, out, controls.clone(), self.preview.clone())?;
        let (opened, open_result) = mpsc::channel();
        let thread_controls = controls.clone();
        let thread = thread::Builder::new()
            .name("camera".to_owned())
            .spawn(move || capture(index, config, pipeline, &thread_controls, opened))
            .map_err(|error| VideoError::Backend(format!("camera thread: {error}")))?;
        let running = Running { controls, thread };
        match open_result.recv_timeout(OPEN_TIMEOUT) {
            Ok(Ok(())) => {
                self.running = Some(running);
                Ok(())
            }
            Ok(Err(error)) => {
                let _ = running.thread.join();
                Err(error)
            }
            Err(_) => {
                // Still opening: tell the thread to give up once it can; don't wait for it.
                running.controls.stop.store(true, Ordering::Relaxed);
                Err(VideoError::Backend("the camera did not open".to_owned()))
            }
        }
    }

    fn stop(&mut self) -> Result<(), VideoError> {
        let Some(running) = self.running.take() else {
            return Ok(());
        };
        running.controls.stop.store(true, Ordering::Relaxed);
        self.preview.clear();
        running
            .thread
            .join()
            .map_err(|_| VideoError::Backend("the camera thread panicked".to_owned()))?
    }

    fn request_keyframe(&mut self) {
        if let Some(running) = &self.running {
            running.controls.request_keyframe();
        }
    }

    fn set_bitrate(&mut self, bps: u32) {
        if let Some(running) = &self.running {
            running.controls.set_bitrate(bps);
        }
    }

    fn switch_camera(&mut self, _facing: Facing) -> Result<(), VideoError> {
        let next = next_camera(self.camera, cameras()?.len()).ok_or(VideoError::Unsupported)?;
        self.camera = next;
        if let Some(running) = &self.running {
            running.controls.switch_to.store(next, Ordering::Relaxed);
        }
        Ok(())
    }
}

/// Asks for the camera permission if the user has not given it yet, and waits for the answer.
fn ensure_permission() -> Result<(), VideoError> {
    if nokhwa::nokhwa_check() {
        return Ok(());
    }
    let (answer, answered) = mpsc::channel();
    nokhwa::nokhwa_initialize(move |granted| {
        let _ = answer.send(granted);
    });
    match answered.recv_timeout(OPEN_TIMEOUT) {
        Ok(true) => Ok(()),
        _ => Err(VideoError::PermissionDenied),
    }
}

/// The cameras AVFoundation sees, in its order.
fn cameras() -> Result<Vec<CameraIndex>, VideoError> {
    let cameras = nokhwa::query(ApiBackend::AVFoundation).map_err(camera_error)?;
    Ok(cameras.iter().map(|info| info.index().clone()).collect())
}

fn open_camera(index: CameraIndex, config: VideoConfig) -> Result<Camera, VideoError> {
    let wanted = CameraFormat::new(
        Resolution::new(config.width, config.height),
        FrameFormat::YUYV,
        config.fps,
    );
    let format = RequestedFormat::new::<YuyvFormat>(RequestedFormatType::Closest(wanted));
    let mut camera =
        Camera::with_backend(index, format, ApiBackend::AVFoundation).map_err(camera_error)?;
    camera.open_stream().map_err(camera_error)?;
    Ok(camera)
}

/// The camera thread: opens the camera, says how that went on `opened`, then captures until
/// told to stop or the engine is gone.
fn capture(
    index: CameraIndex,
    config: VideoConfig,
    mut pipeline: CapturePipeline,
    controls: &Controls,
    opened: mpsc::Sender<Result<(), VideoError>>,
) -> Result<(), VideoError> {
    let mut camera = match open_camera(index, config) {
        Ok(camera) => {
            let _ = opened.send(Ok(()));
            camera
        }
        Err(error) => {
            let _ = opened.send(Err(error.clone()));
            return Err(error);
        }
    };
    let clock = Instant::now();
    let result = loop {
        if controls.stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        if let Some(next) = controls.take_switch() {
            let _ = camera.stop_stream();
            let index = match cameras()?.get(next) {
                Some(index) => index.clone(),
                None => break Err(VideoError::NoCamera),
            };
            camera = open_camera(index, config)?;
            // The receiver's decoder cannot go on from the other camera's pictures.
            controls.request_keyframe();
        }
        let buffer = match camera.frame() {
            Ok(buffer) => buffer,
            Err(error) => break Err(camera_error(error)),
        };
        let timestamp = clock.elapsed();
        let (width, height) = (buffer.resolution().width(), buffer.resolution().height());
        let data = buffer.buffer();
        let stride = data.len() / (height.max(1) as usize);
        let picture = match yuyv_to_i420(width, height, stride, data) {
            Ok(picture) => picture,
            Err(error) => break Err(error),
        };
        match pipeline.process(picture, timestamp) {
            Ok(SendOutcome::Closed) => break Ok(()),
            Ok(SendOutcome::Sent | SendOutcome::Dropped) => {}
            Err(error) => break Err(error),
        }
    };
    let _ = camera.stop_stream();
    result
}

fn camera_error(error: NokhwaError) -> VideoError {
    VideoError::Backend(format!("camera: {error}"))
}

/// A desktop window showing the remote video with the local preview in a corner.
///
/// **Main thread only.** On macOS, AppKit windows must be created and updated on the process'
/// main thread, so the window is not `Send`: open it in `main` and call [`VideoWindow::show`]
/// from `main`'s loop; the camera, the network and the decoder run elsewhere and hand over
/// their pictures through [`FrameSlot`]s.
pub struct VideoWindow {
    window: Window,
    canvas: Vec<u32>,
}

impl VideoWindow {
    /// Opens a resizable window of `width × height` pixels.
    pub fn open(title: &str, width: usize, height: usize) -> Result<Self, VideoError> {
        let options = WindowOptions {
            resize: true,
            ..WindowOptions::default()
        };
        let mut window = Window::new(title, width, height, options)
            .map_err(|error| VideoError::Backend(format!("window: {error}")))?;
        window.set_target_fps(60);
        Ok(Self {
            window,
            canvas: Vec::new(),
        })
    }

    /// Draws the newest pictures and handles the window's events. `false` once the user has
    /// closed the window or pressed Escape.
    pub fn show(
        &mut self,
        remote: &FrameSlot,
        preview: Option<&FrameSlot>,
    ) -> Result<bool, VideoError> {
        if !self.window.is_open() || self.window.is_key_down(Key::Escape) {
            return Ok(false);
        }
        let (width, height) = self.window.get_size();
        let (width, height) = (width.max(1), height.max(1));
        self.canvas.resize(width * height, 0);
        let preview = preview.and_then(FrameSlot::latest);
        compose(
            &mut self.canvas,
            width,
            height,
            remote.latest().as_ref(),
            preview.as_ref(),
        );
        self.window
            .update_with_buffer(&self.canvas, width, height)
            .map_err(|error| VideoError::Backend(format!("window: {error}")))?;
        Ok(true)
    }
}

/// Draws a window's contents into `canvas` (`width × height`, 0RGB): `remote` fitted to the
/// window and turned upright, `preview` mirrored in the bottom-right corner, black elsewhere.
pub fn compose(
    canvas: &mut [u32],
    width: usize,
    height: usize,
    remote: Option<&Picture>,
    preview: Option<&Picture>,
) {
    canvas.fill(0);
    if canvas.len() < width * height {
        return;
    }
    if let Some(remote) = remote {
        draw_fitted(canvas, width, (0, 0, width, height), remote, false);
    }
    if let Some(preview) = preview {
        let (box_width, box_height) = (width / 4, height / 4);
        let margin = width / 40;
        let area = (
            width.saturating_sub(box_width + margin),
            height.saturating_sub(box_height + margin),
            box_width,
            box_height,
        );
        draw_fitted(canvas, width, area, preview, true);
    }
}

/// Draws `picture` upright (and `mirrored`, if asked) as large as it fits in `area`
/// (`x, y, width, height`), centred, with nearest-neighbour scaling.
fn draw_fitted(
    canvas: &mut [u32],
    stride: usize,
    area: (usize, usize, usize, usize),
    picture: &Picture,
    mirrored: bool,
) {
    let (area_x, area_y, area_width, area_height) = area;
    let frame = &picture.frame;
    let (source_width, source_height) = (frame.width() as usize, frame.height() as usize);
    let sideways = matches!(picture.rotation, Rotation::Deg90 | Rotation::Deg270);
    // The picture's size once upright.
    let (upright_width, upright_height) = if sideways {
        (source_height, source_width)
    } else {
        (source_width, source_height)
    };
    if upright_width == 0 || upright_height == 0 {
        return;
    }
    let (width, height) = if upright_width * area_height <= area_width * upright_height {
        (upright_width * area_height / upright_height, area_height)
    } else {
        (area_width, upright_height * area_width / upright_width)
    };
    let (left, top) = (
        area_x + (area_width - width) / 2,
        area_y + (area_height - height) / 2,
    );
    for row in 0..height {
        let v = row * upright_height / height;
        for column in 0..width {
            let mut u = column * upright_width / width;
            if mirrored {
                u = upright_width - 1 - u;
            }
            let (x, y) = match picture.rotation {
                Rotation::Deg0 => (u, v),
                Rotation::Deg90 => (v, source_height - 1 - u),
                Rotation::Deg180 => (source_width - 1 - u, source_height - 1 - v),
                Rotation::Deg270 => (source_width - 1 - v, u),
            };
            let [r, g, b] = frame.rgb_at(x as u32, y as u32).unwrap_or_default();
            if let Some(pixel) = canvas.get_mut((top + row) * stride + left + column) {
                *pixel = u32::from_be_bytes([0, r, g, b]);
            }
        }
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
            let average = |i: usize| (u16::from(a[i]) + u16::from(b[i])).div_ceil(2) as u8;
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
    fn switching_goes_to_the_next_camera_and_wraps_around() {
        assert_eq!(next_camera(0, 2), Some(1));
        assert_eq!(next_camera(1, 2), Some(0));
        assert_eq!(next_camera(1, 3), Some(2));
        assert_eq!(next_camera(0, 1), None);
        assert_eq!(next_camera(0, 0), None);
    }

    /// Needs a camera and the camera permission for the terminal: run it by hand.
    #[test]
    #[ignore = "needs a camera"]
    fn captures_encoded_frames_from_the_camera() {
        let (out, frames) = frame_channel(crate::video::FRAME_CHANNEL_CAPACITY);
        let mut camera = CameraSource::new();
        let preview = camera.preview();
        camera
            .start(VideoConfig::default(), Facing::Front, out)
            .unwrap();
        let first = frames.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(first.keyframe);
        let mut received = 1;
        let until = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < until {
            if frames.recv_timeout(Duration::from_millis(200)).is_ok() {
                received += 1;
            }
        }
        camera.request_keyframe();
        camera.set_bitrate(300_000);
        assert!(received > 20, "{received} frames in 2 s");
        assert!(preview.latest().is_some());
        match camera.switch_camera(Facing::Back) {
            Ok(()) | Err(VideoError::Unsupported) => {}
            Err(error) => panic!("switching: {error}"),
        }
        camera.stop().unwrap();
        camera.stop().unwrap();
    }

    const RED: u32 = 0xff_00_00;
    const BLUE: u32 = 0x00_00_ff;

    /// A picture with its left half red and its right half blue.
    fn halves(width: u32, height: u32, rotation: Rotation) -> Picture {
        let mut rgb = Vec::new();
        for _ in 0..height {
            for x in 0..width {
                let colour = if x < width / 2 { RED } else { BLUE };
                rgb.extend_from_slice(&colour.to_be_bytes()[1..]);
            }
        }
        Picture {
            frame: Arc::new(I420Frame::from_rgb(width, height, &rgb).unwrap()),
            rotation,
        }
    }

    /// The canvas as rows of 'R', 'B', '.' (black) and '?' (anything else).
    fn draw(
        width: usize,
        height: usize,
        remote: Option<&Picture>,
        preview: Option<&Picture>,
    ) -> Vec<String> {
        let mut canvas = vec![0x12_34_56; width * height];
        compose(&mut canvas, width, height, remote, preview);
        let near = |pixel: u32, colour: u32| {
            pixel
                .to_be_bytes()
                .iter()
                .zip(colour.to_be_bytes())
                .all(|(a, b)| a.abs_diff(b) <= 8)
        };
        canvas
            .chunks(width)
            .map(|row| {
                row.iter()
                    .map(|&pixel| match pixel {
                        pixel if near(pixel, RED) => 'R',
                        pixel if near(pixel, BLUE) => 'B',
                        0 => '.',
                        _ => '?',
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn an_empty_window_is_black() {
        assert_eq!(draw(4, 2, None, None), ["....", "...."]);
    }

    #[test]
    fn the_remote_picture_is_scaled_to_fit_and_keeps_its_shape() {
        let picture = halves(4, 2, Rotation::Deg0);
        assert_eq!(draw(8, 4, Some(&picture), None), ["RRRRBBBB"; 4]);
        assert_eq!(
            draw(8, 8, Some(&picture), None),
            [
                "........", "........", "RRRRBBBB", "RRRRBBBB", "RRRRBBBB", "RRRRBBBB", "........",
                "........"
            ]
        );
    }

    #[test]
    fn the_remote_picture_is_turned_upright() {
        // Turned clockwise, the left half ends up on top.
        assert_eq!(
            draw(2, 4, Some(&halves(4, 2, Rotation::Deg90)), None),
            ["RR", "RR", "BB", "BB"]
        );
        assert_eq!(
            draw(4, 2, Some(&halves(4, 2, Rotation::Deg180)), None),
            ["BBRR", "BBRR"]
        );
        assert_eq!(
            draw(2, 4, Some(&halves(4, 2, Rotation::Deg270)), None),
            ["BB", "BB", "RR", "RR"]
        );
    }

    #[test]
    fn the_preview_is_mirrored_in_the_bottom_right_corner() {
        let preview = halves(4, 4, Rotation::Deg0);
        let rows = draw(16, 16, None, Some(&preview));
        for row in &rows[..12] {
            assert_eq!(row, "................");
        }
        for row in &rows[12..] {
            assert_eq!(row, "............BBRR", "mirrored, as in a mirror");
        }
    }

    /// Needs a screen and the main thread, which libtest does not give: on macOS AppKit refuses
    /// the window, so there check it with the `video_demo` example instead.
    #[test]
    #[ignore = "needs a screen and the main thread"]
    fn shows_the_test_pattern_in_a_window() {
        let remote = FrameSlot::new();
        let preview = FrameSlot::new();
        let mut window = VideoWindow::open("webrtc-engine test", 640, 480).unwrap();
        for index in 0..90 {
            remote.publish(
                I420Frame::test_pattern(640, 480, index).unwrap(),
                Rotation::Deg0,
            );
            preview.publish(
                I420Frame::test_pattern(320, 240, index).unwrap(),
                Rotation::Deg0,
            );
            if !window.show(&remote, Some(&preview)).unwrap() {
                break;
            }
        }
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
