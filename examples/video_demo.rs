//! See the video path on a computer: the Mac's camera is encoded with OpenH264, runs through a
//! `VideoCall` (RTP packetisation, the simulated network with delay, jitter and loss, frame
//! reassembly, keyframe requests and bitrate control), is decoded again and shown in a window,
//! with a small self-preview in the corner.
//!
//! ```sh
//! cargo run --release --example video_demo --features desktop -- --loss 5 --jitter 30 --delay 50
//! cargo run --release --example video_demo --features desktop -- --source fake --seconds 10
//! ```
//!
//! With `--source fake` there is no camera and no window: a `FakeSource` and a `FakeSink` run
//! the same call and only the counters are printed.
//!
//! The window runs on the main thread (macOS requires it); the camera captures and encodes on its
//! own thread; the call's tasks, and the decoder inside the sink, run on a Tokio runtime.

use std::error::Error;
use std::time::{Duration, Instant};

use webrtc_engine::netsim::{Conditions, simulated_video_link};
use webrtc_engine::video::call::{
    RemoteVideo, VideoCall, VideoCallConfig, VideoCallError, VideoStats, VideoTransport,
};
use webrtc_engine::video::desktop::{CameraSource, FrameSlot, VideoWindow, WindowSink};
use webrtc_engine::video::fake::{FakeSink, FakeSource};
use webrtc_engine::video::{VideoConfig, VideoSink, VideoSource};

const USAGE: &str = "usage: video_demo [--loss PERCENT] [--jitter MS] [--delay MS] \
                     [--bitrate KBITS] [--seconds S] [--source camera|fake]";

/// How long the fake source runs when `--seconds` is not given: it has no window to close.
const FAKE_SECONDS: u64 = 10;

/// Where the video comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Source {
    /// The Mac's camera, shown in a window.
    #[default]
    Camera,
    /// Made-up frames, no window: counters only.
    Fake,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Options {
    loss_percent: f64,
    jitter_ms: u64,
    delay_ms: u64,
    bitrate_kbps: u32,
    /// Zero: until the window is closed.
    seconds: u64,
    source: Source,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            loss_percent: 0.0,
            jitter_ms: 0,
            delay_ms: 40,
            bitrate_kbps: VideoConfig::default().bitrate_bps / 1000,
            seconds: 0,
            source: Source::Camera,
        }
    }
}

impl Options {
    fn conditions(&self) -> Conditions {
        Conditions {
            delay: Duration::from_millis(self.delay_ms),
            jitter: Duration::from_millis(self.jitter_ms),
            loss: self.loss_percent / 100.0,
            ..Conditions::default()
        }
    }

    fn call_config(&self) -> VideoCallConfig {
        let defaults = VideoCallConfig::default();
        VideoCallConfig {
            video: VideoConfig {
                bitrate_bps: self.bitrate_kbps.saturating_mul(1000),
                ..defaults.video
            },
            ..defaults
        }
    }
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        let bad = || format!("{flag}: {value:?} is not a number");
        match flag.as_str() {
            "--loss" => {
                options.loss_percent = value.parse().map_err(|_| bad())?;
                if !(0.0..=100.0).contains(&options.loss_percent) {
                    return Err("--loss goes from 0 to 100".to_owned());
                }
            }
            "--jitter" => options.jitter_ms = value.parse().map_err(|_| bad())?,
            "--delay" => options.delay_ms = value.parse().map_err(|_| bad())?,
            "--bitrate" => options.bitrate_kbps = value.parse().map_err(|_| bad())?,
            "--seconds" => options.seconds = value.parse().map_err(|_| bad())?,
            "--source" => {
                options.source = match value.as_str() {
                    "camera" => Source::Camera,
                    "fake" => Source::Fake,
                    _ => return Err(format!("--source: {value:?} is not camera or fake")),
                }
            }
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    Ok(options)
}

/// A video call whose frames go out over the simulated network and come back to its own sink:
/// what the other side of a call would see of us. Must be called inside a Tokio runtime.
fn start_call(
    source: Box<dyn VideoSource>,
    sink: Box<dyn VideoSink>,
    options: &Options,
) -> Result<VideoCall, VideoCallError> {
    let (frames, packets, feedback) = simulated_video_link(options.conditions(), 1);
    let transport = VideoTransport {
        frames,
        feedback,
        remote: RemoteVideo::new(packets),
    };
    VideoCall::start(source, sink, transport, options.call_config())
}

/// What happened in the last second, from two readings of the call's counters.
fn stats_line(second: u64, now: &VideoStats, before: &VideoStats) -> String {
    let dropped = |stats: &VideoStats| stats.source_dropped + stats.remote.dropped;
    let requests =
        |stats: &VideoStats| stats.remote.keyframe_requests + stats.sink_keyframe_requests;
    format!(
        "{second:>3}s  sent {} fps at {} kbit/s | keyframes {} | received {} fps | dropped {} | \
         keyframe requests {} | decode errors {}",
        now.frames_sent - before.frames_sent,
        now.bitrate_bps / 1000,
        now.keyframes_sent - before.keyframes_sent,
        now.frames_received - before.frames_received,
        dropped(now) - dropped(before),
        requests(now) - requests(before),
        now.sink_errors - before.sink_errors,
    )
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = parse(std::env::args().skip(1)).map_err(|error| format!("{error}\n{USAGE}"))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()?;
    let _inside = runtime.enter();
    let call = match options.source {
        Source::Camera => {
            // The window first, on the main thread, as macOS wants.
            let mut window = VideoWindow::open("webrtc-engine video demo", 960, 720)?;
            let remote = FrameSlot::new();
            let camera = CameraSource::new();
            let preview = camera.preview();
            let sink = WindowSink::new(remote.clone());
            let call = start_call(Box::new(camera), Box::new(sink), &options)?;
            announce(&options, "Camera -> OpenH264", "OpenH264 -> window");
            println!("Close the window or press Escape to stop.");
            let mut printer = Printer::new();
            let clock = Instant::now();
            // Drawing waits for the next screen refresh: this paces the loop.
            while window.show(&remote, Some(&preview))? {
                printer.tick(&call, clock.elapsed());
                if options.seconds > 0 && clock.elapsed() >= Duration::from_secs(options.seconds) {
                    break;
                }
            }
            call
        }
        Source::Fake => {
            let call = start_call(
                Box::new(FakeSource::new()),
                Box::new(FakeSink::new()),
                &options,
            )?;
            announce(&options, "FakeSource", "FakeSink");
            let seconds = if options.seconds > 0 {
                options.seconds
            } else {
                FAKE_SECONDS
            };
            let mut printer = Printer::new();
            let clock = Instant::now();
            while clock.elapsed() < Duration::from_secs(seconds) {
                std::thread::sleep(Duration::from_millis(50));
                printer.tick(&call, clock.elapsed());
            }
            call
        }
    };
    let stats = runtime.block_on(call.stop());
    println!("final: {stats:?}");
    Ok(())
}

fn announce(options: &Options, from: &str, to: &str) {
    println!(
        "{from} {} kbit/s -> VideoCall: RTP, {} ms delay, {} ms jitter, {} % loss -> {to}.",
        options.bitrate_kbps, options.delay_ms, options.jitter_ms, options.loss_percent
    );
}

/// Prints a stats line once a second.
struct Printer {
    before: VideoStats,
    second: u64,
}

impl Printer {
    fn new() -> Self {
        Self {
            before: VideoStats::default(),
            second: 1,
        }
    }

    fn tick(&mut self, call: &VideoCall, elapsed: Duration) {
        if elapsed >= Duration::from_secs(self.second) {
            let now = call.stats();
            println!("{}", stats_line(self.second, &now, &self.before));
            self.before = now;
            self.second += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use webrtc_engine::video::fake::frame_index;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn without_flags_it_is_the_camera_on_a_clean_network_until_the_window_closes() {
        let options = parse(args("")).unwrap();
        assert_eq!(options, Options::default());
        assert_eq!(options.seconds, 0);
        assert_eq!(options.bitrate_kbps, 800);
        assert_eq!(options.source, Source::Camera);
        assert_eq!(options.conditions().loss, 0.0);
        assert_eq!(options.call_config().video.bitrate_bps, 800_000);
    }

    #[test]
    fn reads_every_flag() {
        let options = parse(args(
            "--loss 5 --jitter 30 --delay 80 --bitrate 400 --seconds 9 --source fake",
        ))
        .unwrap();
        assert_eq!(
            options,
            Options {
                loss_percent: 5.0,
                jitter_ms: 30,
                delay_ms: 80,
                bitrate_kbps: 400,
                seconds: 9,
                source: Source::Fake,
            }
        );
        let conditions = options.conditions();
        assert_eq!(conditions.loss, 0.05);
        assert_eq!(conditions.jitter, Duration::from_millis(30));
        assert_eq!(conditions.delay, Duration::from_millis(80));
        assert_eq!(options.call_config().video.bitrate_bps, 400_000);
        assert_eq!(
            parse(args("--source camera")).unwrap().source,
            Source::Camera
        );
    }

    #[test]
    fn rejects_what_it_does_not_understand() {
        assert!(parse(args("--loss")).is_err());
        assert!(parse(args("--loss lots")).is_err());
        assert!(parse(args("--loss 150")).is_err());
        assert!(parse(args("--zoom 2")).is_err());
        assert!(parse(args("--source phone")).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn the_call_carries_the_frames_over_the_simulated_network_back_to_the_sink() {
        let options = parse(args("--delay 50 --jitter 10")).unwrap();
        let sink = FakeSink::new();
        let shown = sink.probe();
        let call = start_call(Box::new(FakeSource::new()), Box::new(sink), &options).unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let stats = call.stop().await;

        let shown: Vec<u32> = shown
            .lock()
            .unwrap()
            .shown
            .iter()
            .filter_map(|frame| frame_index(&frame.data))
            .collect();
        assert!(shown.len() >= 50, "{stats:?}");
        assert_eq!(shown, (0..shown.len() as u32).collect::<Vec<_>>());
        assert_eq!(stats.sink_errors, 0);
        assert_eq!(stats.remote.keyframe_requests, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn with_loss_the_receiver_asks_for_keyframes_and_shows_no_garbage() {
        let options = parse(args("--loss 5 --delay 50")).unwrap();
        let sink = FakeSink::new();
        let probe = sink.probe();
        let call = start_call(Box::new(FakeSource::new()), Box::new(sink), &options).unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        let stats = call.stop().await;

        assert!(stats.remote.keyframe_requests > 0, "{stats:?}");
        assert!(stats.keyframe_requests_received > 0, "{stats:?}");
        assert!(stats.keyframes_sent > 1, "{stats:?}");
        assert_eq!(probe.lock().unwrap().broken, 0);
    }

    #[test]
    fn the_stats_line_shows_what_the_viewer_should_watch() {
        let before = VideoStats {
            frames_sent: 30,
            keyframes_sent: 1,
            frames_received: 28,
            ..VideoStats::default()
        };
        let mut now = before.clone();
        now.frames_sent = 60;
        now.keyframes_sent = 3;
        now.frames_received = 53;
        now.bitrate_bps = 640_000;
        now.source_dropped = 2;
        now.remote.dropped = 5;
        now.remote.keyframe_requests = 2;
        now.sink_keyframe_requests = 1;
        now.sink_errors = 1;
        let line = stats_line(7, &now, &before);
        for expected in [
            "7s",
            "sent 30 fps",
            "640 kbit/s",
            "keyframes 2",
            "received 25 fps",
            "dropped 7",
            "keyframe requests 3",
            "decode errors 1",
        ] {
            assert!(line.contains(expected), "{expected:?} missing in {line:?}");
        }
    }
}
