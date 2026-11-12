use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::assemble::{AssemblerStats, FrameAssembler, KEYFRAME_REQUEST_INTERVAL};
use super::bitrate::BitrateController;
use super::packet::VideoPacket;
use super::{
    EncodedFrame, FRAME_CHANNEL_CAPACITY, Facing, FrameSender, VideoConfig, VideoError, VideoSink,
    VideoSource, frame_channel,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Feedback {
    /// PLI or FIR.
    KeyframeRequest,
    /// `fraction_lost` is 0 to 1, since the previous report.
    Report {
        fraction_lost: f64,
        round_trip: Option<Duration>,
    },
    /// The most the receiver thinks it can take.
    Remb { bps: u64 },
}

pub trait FrameSink: Send + 'static {
    type Error: Send;
    fn send(
        &mut self,
        frame: &EncodedFrame,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

pub trait FeedbackSource: Send + 'static {
    fn recv(&mut self) -> impl Future<Output = Option<Feedback>> + Send;
}

/// Whole frames, in decoding order.
pub trait FrameSource: Send + 'static {
    type Error: Send;
    /// Must be cancel-safe. [`VideoCall`] gives up waiting now and then to read the stats.
    fn recv(&mut self) -> impl Future<Output = Result<Option<EncodedFrame>, Self::Error>> + Send;
    fn request_keyframe(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send;
    fn stats(&self) -> AssemblerStats {
        AssemblerStats::default()
    }
}

pub trait VideoPacketSource: Send + 'static {
    type Error: Send;
    fn recv(&mut self) -> impl Future<Output = Result<Option<VideoPacket>, Self::Error>> + Send;
    fn request_keyframe(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

const STATS_REFRESH: Duration = Duration::from_millis(250);

pub const CAMERA_RESTART_INTERVAL: Duration = Duration::from_secs(1);

pub const REMOTE_POLL: Duration = Duration::from_millis(20);

pub struct RemoteVideo<P> {
    packets: P,
    assembler: FrameAssembler,
    clock: Instant,
}

impl<P: VideoPacketSource> RemoteVideo<P> {
    pub fn new(packets: P) -> Self {
        Self {
            packets,
            assembler: FrameAssembler::new(),
            clock: Instant::now(),
        }
    }

    /// Cancel-safe if the packet source's `recv` is. A keyframe request cut off this way is lost.
    pub async fn recv(&mut self) -> Result<Option<EncodedFrame>, P::Error> {
        loop {
            let now = self.clock.elapsed();
            if let Some(frame) = self.assembler.pop(now) {
                return Ok(Some(frame));
            }
            if self.assembler.wants_keyframe(now) {
                self.packets.request_keyframe().await?;
            }
            // A gap only closes with time, so wake up without packets too.
            match tokio::time::timeout(REMOTE_POLL, self.packets.recv()).await {
                Ok(Ok(Some(packet))) => self.assembler.push(packet, self.clock.elapsed()),
                Ok(Ok(None)) => return Ok(None),
                Ok(Err(error)) => return Err(error),
                Err(_) => {}
            }
        }
    }

    pub async fn request_keyframe(&mut self) -> Result<(), P::Error> {
        self.assembler.request_keyframe();
        if self.assembler.wants_keyframe(self.clock.elapsed()) {
            self.packets.request_keyframe().await?;
        }
        Ok(())
    }

    pub fn stats(&self) -> AssemblerStats {
        self.assembler.stats()
    }

    pub fn packets(&self) -> &P {
        &self.packets
    }
}

impl<P: VideoPacketSource> FrameSource for RemoteVideo<P> {
    type Error = P::Error;
    fn recv(&mut self) -> impl Future<Output = Result<Option<EncodedFrame>, P::Error>> + Send {
        RemoteVideo::recv(self)
    }
    fn request_keyframe(&mut self) -> impl Future<Output = Result<(), P::Error>> + Send {
        RemoteVideo::request_keyframe(self)
    }
    fn stats(&self) -> AssemblerStats {
        RemoteVideo::stats(self)
    }
}

pub struct VideoTransport<S, F, R> {
    pub frames: S,
    pub feedback: F,
    pub remote: R,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoCallConfig {
    pub video: VideoConfig,
    pub facing: Facing,
    /// The source's frame channel has no wake-up, so it is polled this often.
    pub poll_interval: Duration,
}

impl Default for VideoCallConfig {
    fn default() -> Self {
        Self {
            video: VideoConfig::default(),
            facing: Facing::Front,
            poll_interval: Duration::from_millis(5),
        }
    }
}

/// No picture content and no identifiers in here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VideoStats {
    pub frames_sent: u64,
    pub keyframes_sent: u64,
    pub send_errors: u64,
    pub source_dropped: u64,
    pub keyframe_requests_received: u64,
    pub bitrate_bps: u32,
    pub frames_received: u64,
    pub sink_errors: u64,
    pub sink_keyframe_requests: u64,
    pub camera_lost: bool,
    pub camera_restarts: u64,
    pub remote: AssemblerStats,
    pub receive_errors: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoCallError {
    Video(VideoError),
    NoRuntime,
}

impl fmt::Display for VideoCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Video(error) => write!(f, "{error}"),
            Self::NoRuntime => f.write_str("a video call needs a Tokio runtime"),
        }
    }
}

impl std::error::Error for VideoCallError {}

impl From<VideoError> for VideoCallError {
    fn from(error: VideoError) -> Self {
        Self::Video(error)
    }
}

struct Shared {
    source: Mutex<Box<dyn VideoSource>>,
    sink: Mutex<Box<dyn VideoSink>>,
    state: Mutex<State>,
    stats: Mutex<VideoStats>,
}

impl Shared {
    fn update(&self, change: impl FnOnce(&mut VideoStats)) {
        change(&mut lock(&self.stats));
    }

    fn paused(&self) -> bool {
        lock(&self.state).paused
    }
}

struct State {
    paused: bool,
    facing: Facing,
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct VideoCall {
    shared: Arc<Shared>,
    config: VideoCallConfig,
    frames: FrameSender,
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    stopped: bool,
}

impl VideoCall {
    /// Must be called inside a Tokio runtime.
    pub fn start<S: FrameSink, F: FeedbackSource, R: FrameSource>(
        source: Box<dyn VideoSource>,
        sink: Box<dyn VideoSink>,
        transport: VideoTransport<S, F, R>,
        config: VideoCallConfig,
    ) -> Result<VideoCall, VideoCallError> {
        let runtime = Handle::try_current().map_err(|_| VideoCallError::NoRuntime)?;
        let (mut source, mut sink) = (source, sink);
        let (frames, incoming) = frame_channel(FRAME_CHANNEL_CAPACITY);
        sink.start()?;
        if let Err(error) = source.start(config.video, config.facing, frames.clone()) {
            let _ = sink.stop();
            return Err(error.into());
        }
        let controller = BitrateController::new(config.video.bitrate_bps);
        let shared = Arc::new(Shared {
            source: Mutex::new(source),
            sink: Mutex::new(sink),
            state: Mutex::new(State {
                paused: false,
                facing: config.facing,
            }),
            stats: Mutex::new(VideoStats {
                bitrate_bps: controller.target(),
                ..VideoStats::default()
            }),
        });
        let (stop, stopped) = watch::channel(false);
        let tasks = vec![
            runtime.spawn(send_task(
                incoming,
                transport.frames,
                CameraWatch {
                    shared: shared.clone(),
                    frames: frames.clone(),
                    video: config.video,
                    last_restart: None,
                },
                config.poll_interval,
                stopped.clone(),
            )),
            runtime.spawn(feedback_task(
                transport.feedback,
                controller,
                shared.clone(),
                stopped.clone(),
            )),
            runtime.spawn(receive_task(transport.remote, shared.clone(), stopped)),
        ];
        Ok(VideoCall {
            shared,
            config,
            frames,
            stop,
            tasks,
            stopped: false,
        })
    }

    /// The other side keeps the last frame. Tell it the camera is off in your own signalling.
    pub fn pause(&self) -> Result<(), VideoError> {
        // Set first, so frames the camera still had in flight are not sent either.
        lock(&self.shared.state).paused = true;
        lock(&self.shared.source).stop()
    }

    pub fn resume(&self) -> Result<(), VideoError> {
        let mut state = lock(&self.shared.state);
        if !state.paused {
            return Ok(());
        }
        let video = VideoConfig {
            bitrate_bps: self.stats().bitrate_bps,
            ..self.config.video
        };
        let mut source = lock(&self.shared.source);
        source.start(video, state.facing, self.frames.clone())?;
        source.request_keyframe();
        state.paused = false;
        Ok(())
    }

    pub fn is_paused(&self) -> bool {
        lock(&self.shared.state).paused
    }

    pub fn switch_camera(&self, facing: Facing) -> Result<(), VideoError> {
        let mut state = lock(&self.shared.state);
        if !state.paused {
            lock(&self.shared.source).switch_camera(facing)?;
        }
        state.facing = facing;
        Ok(())
    }

    pub fn facing(&self) -> Facing {
        lock(&self.shared.state).facing
    }

    pub fn stats(&self) -> VideoStats {
        lock(&self.shared.stats).clone()
    }

    pub async fn stop(mut self) -> VideoStats {
        let _ = self.stop.send(true);
        for task in std::mem::take(&mut self.tasks) {
            let _ = task.await;
        }
        self.stop_devices();
        self.stats()
    }

    fn stop_devices(&mut self) {
        if !self.stopped {
            self.stopped = true;
            let _ = lock(&self.shared.source).stop();
            let _ = lock(&self.shared.sink).stop();
        }
    }
}

impl Drop for VideoCall {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        self.stop_devices();
    }
}

async fn stopping(stopped: &mut watch::Receiver<bool>) {
    let _ = stopped.wait_for(|stop| *stop).await;
}

struct CameraWatch {
    shared: Arc<Shared>,
    frames: FrameSender,
    video: VideoConfig,
    last_restart: Option<Instant>,
}

impl CameraWatch {
    fn check(&mut self) -> bool {
        // Lock order is state, then source, as in `VideoCall::resume`.
        let state = lock(&self.shared.state);
        if state.paused {
            return false;
        }
        let mut source = lock(&self.shared.source);
        if !source.lost() {
            return false;
        }
        if self
            .last_restart
            .is_some_and(|at| at.elapsed() < CAMERA_RESTART_INTERVAL)
        {
            return true;
        }
        self.last_restart = Some(Instant::now());
        let video = VideoConfig {
            bitrate_bps: lock(&self.shared.stats).bitrate_bps,
            ..self.video
        };
        let _ = source.stop();
        if source
            .start(video, state.facing, self.frames.clone())
            .is_err()
        {
            return true;
        }
        source.request_keyframe();
        self.shared.update(|stats| stats.camera_restarts += 1);
        source.lost()
    }
}

async fn send_task<S: FrameSink>(
    incoming: super::FrameReceiver,
    mut out: S,
    mut camera: CameraWatch,
    poll_interval: Duration,
    mut stopped: watch::Receiver<bool>,
) {
    let shared = camera.shared.clone();
    let mut ticker = tokio::time::interval(poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let (mut sent, mut keyframes, mut errors) = (0, 0, 0);
    let work = async move {
        loop {
            ticker.tick().await;
            // The call holds a sender, so this never sees a disconnect.
            while let Ok(frame) = incoming.try_recv() {
                if shared.paused() {
                    continue;
                }
                match out.send(&frame).await {
                    Ok(()) => {
                        sent += 1;
                        keyframes += u64::from(frame.keyframe);
                    }
                    Err(_) => errors += 1,
                }
            }
            let lost = camera.check();
            shared.update(|stats| {
                stats.frames_sent = sent;
                stats.keyframes_sent = keyframes;
                stats.send_errors = errors;
                stats.source_dropped = incoming.dropped();
                stats.camera_lost = lost;
            });
        }
    };
    tokio::select! {
        () = stopping(&mut stopped) => {}
        () = work => {}
    }
}

async fn feedback_task<F: FeedbackSource>(
    mut feedback: F,
    mut controller: BitrateController,
    shared: Arc<Shared>,
    mut stopped: watch::Receiver<bool>,
) {
    let clock = Instant::now();
    let work = async {
        while let Some(told) = feedback.recv().await {
            let before = controller.target();
            let after = match told {
                Feedback::KeyframeRequest => {
                    lock(&shared.source).request_keyframe();
                    shared.update(|stats| stats.keyframe_requests_received += 1);
                    continue;
                }
                Feedback::Report {
                    fraction_lost,
                    round_trip,
                } => controller.on_report(fraction_lost, round_trip, clock.elapsed()),
                Feedback::Remb { bps } => controller.on_remb(bps),
            };
            if after != before {
                lock(&shared.source).set_bitrate(after);
                shared.update(|stats| stats.bitrate_bps = after);
            }
        }
    };
    tokio::select! {
        () = stopping(&mut stopped) => {}
        () = work => {}
    }
}

async fn receive_task<R: FrameSource>(
    mut remote: R,
    shared: Arc<Shared>,
    mut stopped: watch::Receiver<bool>,
) {
    let mut last_request: Option<Instant> = None;
    let work = async {
        loop {
            let received = tokio::time::timeout(STATS_REFRESH, remote.recv()).await;
            let assembled = remote.stats();
            shared.update(|stats| stats.remote = assembled);
            let asked = match received {
                Err(_) => Ok(()),
                Ok(Ok(Some(frame))) => {
                    let shown = lock(&shared.sink).push(frame);
                    match shown {
                        Ok(()) => {
                            shared.update(|stats| stats.frames_received += 1);
                            Ok(())
                        }
                        Err(_) => {
                            shared.update(|stats| stats.sink_errors += 1);
                            last_request = Some(Instant::now());
                            remote.request_keyframe().await
                        }
                    }
                }
                Ok(Ok(None)) => break,
                Ok(Err(_)) => {
                    shared.update(|stats| stats.receive_errors += 1);
                    break;
                }
            };
            let asked = match asked {
                Ok(()) => sink_keyframe_request(&mut remote, &shared, &mut last_request).await,
                failed => failed,
            };
            if asked.is_err() {
                shared.update(|stats| stats.receive_errors += 1);
                break;
            }
        }
    };
    tokio::select! {
        () = stopping(&mut stopped) => {}
        () = work => {}
    }
}

/// The sink is not polled inside the interval, so a one-shot request waits until it can be sent.
async fn sink_keyframe_request<R: FrameSource>(
    remote: &mut R,
    shared: &Shared,
    last_request: &mut Option<Instant>,
) -> Result<(), R::Error> {
    if last_request.is_some_and(|at| at.elapsed() < KEYFRAME_REQUEST_INTERVAL) {
        return Ok(());
    }
    if !lock(&shared.sink).keyframe_needed() {
        return Ok(());
    }
    *last_request = Some(Instant::now());
    shared.update(|stats| stats.sink_keyframe_requests += 1);
    remote.request_keyframe().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::assemble::MAX_WAIT;
    use crate::video::fake::{FakeSink, FakeSource, SinkProbe, SourceProbe, frame_index};
    use crate::video::packet::Packetizer;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use tokio::sync::mpsc;

    const CONFIG: VideoCallConfig = VideoCallConfig {
        video: VideoConfig {
            width: 320,
            height: 240,
            fps: 30,
            bitrate_bps: 300_000,
        },
        facing: Facing::Front,
        poll_interval: Duration::from_millis(5),
    };

    struct WireOut(mpsc::UnboundedSender<EncodedFrame>);

    impl FrameSink for WireOut {
        type Error = ();
        async fn send(&mut self, frame: &EncodedFrame) -> Result<(), ()> {
            self.0.send(frame.clone()).map_err(drop)
        }
    }

    struct WireIn {
        frames: mpsc::UnboundedReceiver<EncodedFrame>,
        keyframe_requests: mpsc::UnboundedSender<()>,
    }

    impl FrameSource for WireIn {
        type Error = ();
        async fn recv(&mut self) -> Result<Option<EncodedFrame>, ()> {
            Ok(self.frames.recv().await)
        }
        async fn request_keyframe(&mut self) -> Result<(), ()> {
            self.keyframe_requests.send(()).map_err(drop)
        }
    }

    struct Told(mpsc::UnboundedReceiver<Feedback>);

    impl FeedbackSource for Told {
        async fn recv(&mut self) -> Option<Feedback> {
            self.0.recv().await
        }
    }

    struct Ends {
        feedback: mpsc::UnboundedSender<Feedback>,
        keyframe_requests: mpsc::UnboundedReceiver<()>,
        inject: mpsc::UnboundedSender<EncodedFrame>,
    }

    fn looped(
        source: FakeSource,
        sink: FakeSink,
    ) -> (
        VideoCall,
        Ends,
        Arc<Mutex<SourceProbe>>,
        Arc<Mutex<SinkProbe>>,
    ) {
        let (source_probe, sink_probe) = (source.probe(), sink.probe());
        let (call, ends) = looped_boxed(Box::new(source), Box::new(sink));
        (call, ends, source_probe, sink_probe)
    }

    fn looped_boxed(source: Box<dyn VideoSource>, sink: Box<dyn VideoSink>) -> (VideoCall, Ends) {
        let (out, frames) = mpsc::unbounded_channel();
        let (feedback, told) = mpsc::unbounded_channel();
        let (requests, keyframe_requests) = mpsc::unbounded_channel();
        let transport = VideoTransport {
            frames: WireOut(out.clone()),
            feedback: Told(told),
            remote: WireIn {
                frames,
                keyframe_requests: requests,
            },
        };
        let call = VideoCall::start(source, sink, transport, CONFIG).unwrap();
        let ends = Ends {
            feedback,
            keyframe_requests,
            inject: out,
        };
        (call, ends)
    }

    fn shown(probe: &Arc<Mutex<SinkProbe>>) -> Vec<u32> {
        lock(probe)
            .shown
            .iter()
            .filter_map(|frame| frame_index(&frame.data))
            .collect()
    }

    async fn run_for(millis: u64) {
        tokio::time::sleep(Duration::from_millis(millis)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_video_call_carries_frames_from_the_camera_to_the_display() {
        let (call, _ends, source, sink) = looped(FakeSource::new(), FakeSink::new());
        run_for(1_000).await;
        let stats = call.stop().await;

        let shown = shown(&sink);
        assert!((29..=31).contains(&shown.len()), "{shown:?}");
        assert_eq!(shown, (0..shown.len() as u32).collect::<Vec<_>>());
        assert_eq!(lock(&sink).broken, 0);
        assert!(stats.frames_sent - shown.len() as u64 <= 1, "{stats:?}");
        assert_eq!(stats.keyframes_sent, 1);
        assert_eq!(stats.frames_received, shown.len() as u64);
        assert_eq!(stats.bitrate_bps, 300_000);
        assert!(!lock(&source).running, "stop stops the camera");
        assert!(!lock(&sink).running, "and the display");
    }

    #[tokio::test(start_paused = true)]
    async fn a_keyframe_request_from_the_other_side_reaches_the_camera() {
        let (call, ends, source, _sink) = looped(FakeSource::new(), FakeSink::new());
        run_for(200).await;
        ends.feedback.send(Feedback::KeyframeRequest).unwrap();
        run_for(100).await;
        let stats = call.stop().await;

        assert_eq!(lock(&source).keyframe_requests, 1);
        assert_eq!(stats.keyframe_requests_received, 1);
        assert_eq!(stats.keyframes_sent, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn receiver_reports_steer_the_encoder_bitrate() {
        let (call, ends, source, _sink) = looped(FakeSource::new(), FakeSink::new());
        run_for(100).await;
        let report = |fraction_lost| Feedback::Report {
            fraction_lost,
            round_trip: Some(Duration::from_millis(60)),
        };
        ends.feedback.send(report(0.3)).unwrap();
        run_for(100).await;
        assert_eq!(call.stats().bitrate_bps, 255_000);

        ends.feedback.send(Feedback::Remb { bps: 200_000 }).unwrap();
        run_for(100).await;
        ends.feedback.send(report(0.05)).unwrap();
        run_for(100).await;
        let stats = call.stop().await;

        assert_eq!(lock(&source).bitrates, vec![255_000, 200_000]);
        assert_eq!(stats.bitrate_bps, 200_000);
    }

    #[tokio::test(start_paused = true)]
    async fn a_paused_call_sends_nothing_and_resumes_with_a_keyframe() {
        let (call, _ends, source, sink) = looped(FakeSource::new(), FakeSink::new());
        run_for(300).await;
        call.pause().unwrap();
        assert!(call.is_paused());
        assert!(!lock(&source).running);
        let sent = call.stats().frames_sent;
        run_for(500).await;
        assert!(
            call.stats().frames_sent <= sent + 1,
            "at most a frame in flight"
        );

        call.resume().unwrap();
        assert!(!call.is_paused());
        run_for(300).await;
        let stats = call.stop().await;

        assert_eq!(stats.keyframes_sent, 2);
        assert_eq!(lock(&sink).broken, 0);
        assert!(stats.frames_received >= 15, "{stats:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn switching_camera_reaches_the_source() {
        let (call, _ends, source, _sink) = looped(FakeSource::new(), FakeSink::new());
        assert_eq!(call.facing(), Facing::Front);
        run_for(100).await;
        call.switch_camera(Facing::Back).unwrap();
        run_for(100).await;
        assert_eq!(call.facing(), Facing::Back);
        assert_eq!(lock(&source).facing, Some(Facing::Back));
        let stats = call.stop().await;
        assert_eq!(stats.keyframes_sent, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_frame_the_display_cannot_decode_asks_the_other_side_for_a_keyframe() {
        let (call, mut ends, _source, sink) = looped(FakeSource::new(), FakeSink::new());
        call.pause().unwrap();
        run_for(100).await;
        let fake = |index: u32, keyframe: bool| EncodedFrame {
            data: crate::video::fake::frame_data(index, keyframe, 500),
            keyframe,
            timestamp: Duration::from_millis(u64::from(index) * 33),
            rotation: super::super::Rotation::Deg0,
        };
        ends.inject.send(fake(100, true)).unwrap();
        ends.inject.send(fake(102, false)).unwrap();
        run_for(100).await;
        let stats = call.stop().await;

        assert_eq!(stats.sink_errors, 1);
        assert_eq!(lock(&sink).broken, 1);
        assert!(ends.keyframe_requests.try_recv().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn stop_ends_the_tasks_and_lets_go_of_the_network() {
        let (call, ends, _source, _sink) = looped(FakeSource::new(), FakeSink::new());
        run_for(100).await;
        assert!(!ends.feedback.is_closed());
        call.stop().await;
        assert!(ends.feedback.is_closed());
        assert!(ends.inject.is_closed());
    }

    struct NeedySink {
        inner: FakeSink,
        needs: Arc<AtomicBool>,
        once: bool,
    }

    impl NeedySink {
        fn new(once: bool) -> (Self, Arc<AtomicBool>) {
            let needs = Arc::new(AtomicBool::new(false));
            let sink = Self {
                inner: FakeSink::new(),
                needs: needs.clone(),
                once,
            };
            (sink, needs)
        }
    }

    impl VideoSink for NeedySink {
        fn start(&mut self) -> Result<(), VideoError> {
            self.inner.start()
        }
        fn push(&mut self, frame: EncodedFrame) -> Result<(), VideoError> {
            self.inner.push(frame)
        }
        fn stop(&mut self) -> Result<(), VideoError> {
            self.inner.stop()
        }
        fn keyframe_needed(&mut self) -> bool {
            if self.once {
                self.needs.swap(false, Ordering::Relaxed)
            } else {
                self.needs.load(Ordering::Relaxed)
            }
        }
    }

    fn drain(requests: &mut mpsc::UnboundedReceiver<()>) -> usize {
        std::iter::from_fn(|| requests.try_recv().ok()).count()
    }

    #[tokio::test(start_paused = true)]
    async fn a_sink_that_needs_a_keyframe_asks_the_other_side_at_a_limited_rate() {
        let (sink, needs) = NeedySink::new(false);
        let (call, mut ends) = looped_boxed(Box::new(FakeSource::new()), Box::new(sink));
        run_for(300).await;
        assert_eq!(drain(&mut ends.keyframe_requests), 0, "the decoder is fine");

        needs.store(true, Ordering::Relaxed);
        run_for(1_100).await;
        let asked = drain(&mut ends.keyframe_requests);
        assert!((2..=3).contains(&asked), "{asked} requests");

        needs.store(false, Ordering::Relaxed);
        run_for(1_000).await;
        assert_eq!(
            drain(&mut ends.keyframe_requests),
            0,
            "the decoder is fine again"
        );
        let stats = call.stop().await;
        assert_eq!(stats.sink_keyframe_requests, asked as u64);
    }

    #[tokio::test(start_paused = true)]
    async fn a_keyframe_the_sink_asks_for_once_is_asked_for_once() {
        let (sink, needs) = NeedySink::new(true);
        let (call, mut ends) = looped_boxed(Box::new(FakeSource::new()), Box::new(sink));
        run_for(300).await;
        needs.store(true, Ordering::Relaxed);
        run_for(1_000).await;
        let stats = call.stop().await;
        assert_eq!(drain(&mut ends.keyframe_requests), 1);
        assert_eq!(stats.sink_keyframe_requests, 1);
    }

    struct FlakySource {
        inner: FakeSource,
        camera: Arc<CameraState>,
    }

    #[derive(Default)]
    struct CameraState {
        lost: AtomicBool,
        broken: AtomicBool,
        starts: AtomicU64,
    }

    impl FlakySource {
        fn new() -> (Self, Arc<CameraState>) {
            let camera = Arc::new(CameraState::default());
            let source = Self {
                inner: FakeSource::new(),
                camera: camera.clone(),
            };
            (source, camera)
        }
    }

    impl VideoSource for FlakySource {
        fn start(
            &mut self,
            config: VideoConfig,
            facing: Facing,
            out: FrameSender,
        ) -> Result<(), VideoError> {
            self.camera.starts.fetch_add(1, Ordering::Relaxed);
            if self.camera.broken.load(Ordering::Relaxed) {
                return Err(VideoError::NoCamera);
            }
            self.camera.lost.store(false, Ordering::Relaxed);
            self.inner.start(config, facing, out)
        }
        fn stop(&mut self) -> Result<(), VideoError> {
            self.inner.stop()
        }
        fn request_keyframe(&mut self) {
            self.inner.request_keyframe();
        }
        fn set_bitrate(&mut self, bps: u32) {
            self.inner.set_bitrate(bps);
        }
        fn switch_camera(&mut self, facing: Facing) -> Result<(), VideoError> {
            self.inner.switch_camera(facing)
        }
        fn lost(&self) -> bool {
            self.camera.lost.load(Ordering::Relaxed)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_camera_is_started_again() {
        let (source, camera) = FlakySource::new();
        let probe = source.inner.probe();
        let (call, _ends) = looped_boxed(Box::new(source), Box::new(FakeSink::new()));
        run_for(300).await;
        camera.lost.store(true, Ordering::Relaxed);
        run_for(300).await;
        let stats = call.stop().await;

        assert_eq!(camera.starts.load(Ordering::Relaxed), 2);
        assert_eq!(stats.camera_restarts, 1);
        assert!(!stats.camera_lost);
        assert_eq!(stats.keyframes_sent, 2, "it starts again with a keyframe");
        assert!(lock(&probe).keyframe_requests >= 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_camera_that_does_not_come_back_is_reported_and_retried_now_and_then() {
        let (source, camera) = FlakySource::new();
        let (call, _ends) = looped_boxed(Box::new(source), Box::new(FakeSink::new()));
        run_for(300).await;
        camera.broken.store(true, Ordering::Relaxed);
        camera.lost.store(true, Ordering::Relaxed);
        run_for(100).await;
        assert!(call.stats().camera_lost);

        run_for(2_000).await;
        let attempts = camera.starts.load(Ordering::Relaxed) - 1;
        assert!((2..=3).contains(&attempts), "{attempts} attempts");
        assert_eq!(call.stats().camera_restarts, 0);

        camera.broken.store(false, Ordering::Relaxed);
        run_for(1_100).await;
        let stats = call.stop().await;
        assert!(!stats.camera_lost);
        assert_eq!(stats.camera_restarts, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_paused_call_leaves_a_lost_camera_alone() {
        let (source, camera) = FlakySource::new();
        let (call, _ends) = looped_boxed(Box::new(source), Box::new(FakeSink::new()));
        run_for(100).await;
        call.pause().unwrap();
        camera.lost.store(true, Ordering::Relaxed);
        run_for(1_500).await;
        assert_eq!(camera.starts.load(Ordering::Relaxed), 1);
        call.resume().unwrap();
        run_for(100).await;
        let stats = call.stop().await;
        assert_eq!(camera.starts.load(Ordering::Relaxed), 2);
        assert!(!stats.camera_lost);
        assert_eq!(stats.camera_restarts, 0, "resuming is not a restart");
    }

    #[test]
    fn starting_outside_a_runtime_fails() {
        let (_out, frames) = mpsc::unbounded_channel();
        let (_feedback, told) = mpsc::unbounded_channel();
        let (requests, _keyframe_requests) = mpsc::unbounded_channel();
        let (sender, _receiver) = mpsc::unbounded_channel();
        let transport = VideoTransport {
            frames: WireOut(sender),
            feedback: Told(told),
            remote: WireIn {
                frames,
                keyframe_requests: requests,
            },
        };
        let result = VideoCall::start(
            Box::new(FakeSource::new()),
            Box::new(FakeSink::new()),
            transport,
            CONFIG,
        );
        assert!(matches!(result, Err(VideoCallError::NoRuntime)));
    }

    struct Stuck {
        dropped: u64,
    }

    impl FrameSource for Stuck {
        type Error = ();
        async fn recv(&mut self) -> Result<Option<EncodedFrame>, ()> {
            loop {
                self.dropped += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        async fn request_keyframe(&mut self) -> Result<(), ()> {
            Ok(())
        }
        fn stats(&self) -> AssemblerStats {
            AssemblerStats {
                dropped: self.dropped,
                ..AssemblerStats::default()
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn remote_stats_stay_fresh_while_no_frame_comes() {
        let (_out, _frames) = mpsc::unbounded_channel::<EncodedFrame>();
        let (_feedback, told) = mpsc::unbounded_channel();
        let (sender, _receiver) = mpsc::unbounded_channel();
        let transport = VideoTransport {
            frames: WireOut(sender),
            feedback: Told(told),
            remote: Stuck { dropped: 0 },
        };
        let call = VideoCall::start(
            Box::new(FakeSource::new()),
            Box::new(FakeSink::new()),
            transport,
            CONFIG,
        )
        .unwrap();
        run_for(1_000).await;
        assert!(call.stats().remote.dropped > 50, "{:?}", call.stats());
        call.stop().await;
    }

    struct Packets {
        packets: mpsc::UnboundedReceiver<VideoPacket>,
        keyframe_requests: u32,
    }

    impl VideoPacketSource for Packets {
        type Error = ();
        async fn recv(&mut self) -> Result<Option<VideoPacket>, ()> {
            Ok(self.packets.recv().await)
        }
        async fn request_keyframe(&mut self) -> Result<(), ()> {
            self.keyframe_requests += 1;
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn remote_video_reassembles_frames_and_asks_for_a_keyframe_after_a_loss() {
        let (packets, incoming) = mpsc::unbounded_channel();
        let mut remote = RemoteVideo::new(Packets {
            packets: incoming,
            keyframe_requests: 0,
        });
        let mut packetizer = Packetizer::new(1, 0).with_mtu(200);
        let frame = |index: u32, keyframe: bool| EncodedFrame {
            data: crate::video::fake::frame_data(index, keyframe, 500),
            keyframe,
            timestamp: Duration::from_millis(u64::from(index) * 33),
            rotation: super::super::Rotation::Deg0,
        };
        for packet in packetizer.packetize(&frame(0, true)) {
            packets.send(packet).unwrap();
        }
        let first = remote.recv().await.unwrap().unwrap();
        assert!(first.keyframe);
        assert_eq!(frame_index(&first.data), Some(0));

        let mut lost = packetizer.packetize(&frame(1, false));
        lost.remove(0);
        for packet in lost
            .into_iter()
            .chain(packetizer.packetize(&frame(2, false)))
        {
            packets.send(packet).unwrap();
        }
        let waited = tokio::time::timeout(MAX_WAIT * 2, remote.recv()).await;
        assert!(waited.is_err(), "nothing comes out");
        assert_eq!(remote.packets().keyframe_requests, 1);

        for packet in packetizer.packetize(&frame(3, true)) {
            packets.send(packet).unwrap();
        }
        let next = remote.recv().await.unwrap().unwrap();
        assert_eq!(frame_index(&next.data), Some(3));
        assert_eq!(remote.stats().dropped, 2);

        drop(packets);
        assert_eq!(remote.recv().await, Ok(None));
    }
}
