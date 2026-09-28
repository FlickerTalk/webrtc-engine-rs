//! See the video path on a computer: the Mac's camera is encoded with OpenH264, crosses a
//! simulated network (delay, jitter, loss), is decoded again and shown in a window, with a small
//! self-preview in the corner. A lost frame makes the receiver ask the camera for a keyframe.
//!
//! ```sh
//! cargo run --release --example video_demo --features desktop -- --loss 5 --jitter 30 --delay 50
//! ```
//!
//! The window runs on the main thread (macOS requires it); the camera captures and encodes on
//! its own thread; the simulated network, the receiver and the decoder run in the main loop.
//! Every simulated packet is one whole frame, so a loss costs a frame (a real RTP loss costs the
//! frame the packet belonged to).
//!
//! TODO: once `VideoCall` (`VideoSender` / `VideoReceiver`) is merged, replace the hand-made link
//! below (`FrameLink`) with a `VideoCall` over `netsim::simulated_link`, so that the demo runs
//! the engine's RTP packetisation, jitter buffer and PLI.

use std::collections::BTreeMap;
use std::error::Error;
use std::time::{Duration, Instant};

use webrtc_engine::netsim::{Conditions, NetworkSimulator};
use webrtc_engine::video::desktop::{CameraSource, FrameSlot, SinkStats, VideoWindow, WindowSink};
use webrtc_engine::video::{
    EncodedFrame, FRAME_CHANNEL_CAPACITY, Facing, VideoConfig, VideoSink, VideoSource,
    frame_channel,
};

const USAGE: &str = "usage: video_demo [--loss PERCENT] [--jitter MS] [--delay MS] \
                     [--bitrate KBITS] [--seconds S]";

/// How long the receiver waits for a missing frame before calling it lost.
const REORDER_WAIT: Duration = Duration::from_millis(80);
/// While a keyframe is owed, how often the receiver asks again.
const KEYFRAME_RETRY: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq)]
struct Options {
    loss_percent: f64,
    jitter_ms: u64,
    delay_ms: u64,
    bitrate_kbps: u32,
    /// Zero: until the window is closed.
    seconds: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            loss_percent: 0.0,
            jitter_ms: 0,
            delay_ms: 40,
            bitrate_kbps: VideoConfig::default().bitrate_bps / 1000,
            seconds: 0,
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
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    Ok(options)
}

/// The receiving end of the demo's link: puts the numbered frames back in order, waits a little
/// for a missing one, then gives it up and owes a keyframe, dropping delta frames until one
/// comes.
#[derive(Debug, Default)]
struct FrameLink {
    /// Frames that arrived ahead of `next`, with their arrival time.
    held: BTreeMap<u64, (EncodedFrame, Duration)>,
    next: u64,
    keyframe_owed: bool,
    last_request: Option<Duration>,
    counters: LinkCounters,
}

/// What the receiving end of the link did so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LinkCounters {
    delivered: u64,
    /// Frames given up as lost.
    missing: u64,
    /// Frames that came after they had been given up.
    late: u64,
    /// Delta frames dropped while a keyframe was owed.
    skipped: u64,
    keyframe_requests: u64,
}

impl FrameLink {
    /// Frame `sequence` (numbered from zero by the sender) arrives at `now`.
    fn arrive(&mut self, sequence: u64, frame: EncodedFrame, now: Duration) {
        if sequence < self.next {
            self.counters.late += 1;
            return;
        }
        self.held.entry(sequence).or_insert((frame, now));
    }

    /// The frames ready for the decoder at `now`, in order.
    fn ready(&mut self, now: Duration) -> Vec<EncodedFrame> {
        let mut ready = Vec::new();
        while let Some(entry) = self.held.first_entry() {
            let sequence = *entry.key();
            if sequence != self.next {
                let (_, arrived) = entry.get();
                if now < *arrived + REORDER_WAIT {
                    break;
                }
                // Give the missing frames up: what follows references them.
                self.counters.missing += sequence - self.next;
                self.next = sequence;
                self.keyframe_owed = true;
            }
            let (frame, _) = entry.remove();
            self.next += 1;
            if self.keyframe_owed && !frame.keyframe {
                self.counters.skipped += 1;
                continue;
            }
            if frame.keyframe {
                self.keyframe_owed = false;
                self.last_request = None;
            }
            self.counters.delivered += 1;
            ready.push(frame);
        }
        ready
    }

    /// Whether to ask the sender for a keyframe now: when one is owed, here or by the decoder,
    /// at most every [`KEYFRAME_RETRY`].
    fn wants_keyframe(&mut self, decoder_needs_one: bool, now: Duration) -> bool {
        if !(self.keyframe_owed || decoder_needs_one) {
            return false;
        }
        if self
            .last_request
            .is_some_and(|last| now < last + KEYFRAME_RETRY)
        {
            return false;
        }
        self.last_request = Some(now);
        self.counters.keyframe_requests += 1;
        true
    }
}

/// Totals at one moment, to print the difference every second.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Totals {
    sent: u64,
    sent_bytes: u64,
    keyframes: u64,
    /// Dropped by the camera's frame channel.
    channel_dropped: u64,
    link: LinkCounters,
    sink: SinkStats,
    network_lost: u64,
}

fn stats_line(second: u64, now: &Totals, before: &Totals) -> String {
    let dropped = |totals: &Totals| {
        totals.channel_dropped + totals.link.skipped + totals.link.late + totals.sink.dropped
    };
    format!(
        "{second:>3}s  sent {} fps, {} kbit/s | keyframes {} | received {} fps | lost {} | \
         dropped {} | keyframe requests {} | decode errors {}",
        now.sent - before.sent,
        (now.sent_bytes - before.sent_bytes) * 8 / 1000,
        now.keyframes - before.keyframes,
        now.sink.decoded - before.sink.decoded,
        now.network_lost - before.network_lost,
        dropped(now) - dropped(before),
        now.link.keyframe_requests - before.link.keyframe_requests,
        now.sink.errors - before.sink.errors,
    )
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = parse(std::env::args().skip(1)).map_err(|error| format!("{error}\n{USAGE}"))?;
    run(options)
}

fn run(options: Options) -> Result<(), Box<dyn Error>> {
    // The window first, on the main thread, as macOS wants.
    let mut window = VideoWindow::open("webrtc-engine video demo", 960, 720)?;
    let remote = FrameSlot::new();
    let mut sink = WindowSink::new(remote.clone());
    let monitor = sink.monitor();
    sink.start()?;

    let (sender, frames) = frame_channel(FRAME_CHANNEL_CAPACITY);
    let mut camera = CameraSource::new();
    let preview = camera.preview();
    let config = VideoConfig {
        bitrate_bps: options.bitrate_kbps.saturating_mul(1000),
        ..VideoConfig::default()
    };
    camera.start(config, Facing::Front, sender)?;
    println!(
        "Camera -> OpenH264 {} kbit/s -> {} ms delay, {} ms jitter, {} % loss -> OpenH264 -> \
         window. Close the window or press Escape to stop.",
        options.bitrate_kbps, options.delay_ms, options.jitter_ms, options.loss_percent
    );

    let mut network = NetworkSimulator::new(options.conditions(), 1);
    let mut link = FrameLink::default();
    let mut totals = Totals::default();
    let mut printed = Totals::default();
    let mut sequence = 0;
    let mut second = 1;
    let clock = Instant::now();
    loop {
        let now = clock.elapsed();
        while let Ok(frame) = frames.try_recv() {
            totals.sent += 1;
            totals.sent_bytes += frame.data.len() as u64;
            totals.keyframes += u64::from(frame.keyframe);
            network.send((sequence, frame), now);
            sequence += 1;
        }
        for (sequence, frame) in network.deliver(now) {
            link.arrive(sequence, frame, now);
        }
        for frame in link.ready(now) {
            // A frame that fails to decode is counted by the sink, which then owes a keyframe:
            // asked for just below.
            let _ = sink.push(frame);
        }
        if link.wants_keyframe(monitor.stats().keyframe_needed, now) {
            camera.request_keyframe();
        }
        // Draws and waits for the next screen refresh: this paces the loop.
        if !window.show(&remote, Some(&preview))? {
            break;
        }
        if now >= Duration::from_secs(second) {
            totals.channel_dropped = frames.dropped();
            totals.link = link.counters;
            totals.sink = monitor.stats();
            totals.network_lost = network.stats().lost;
            println!("{}", stats_line(second, &totals, &printed));
            printed = totals;
            second += 1;
        }
        if options.seconds > 0 && now >= Duration::from_secs(options.seconds) {
            break;
        }
    }

    camera.stop()?;
    sink.stop()?;
    println!("final: {totals:?}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use webrtc_engine::video::Rotation;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn without_flags_it_is_a_clean_network_until_the_window_closes() {
        let options = parse(args("")).unwrap();
        assert_eq!(options, Options::default());
        assert_eq!(options.seconds, 0);
        assert_eq!(options.bitrate_kbps, 800);
        assert_eq!(options.conditions().loss, 0.0);
    }

    #[test]
    fn reads_every_flag() {
        let options = parse(args(
            "--loss 5 --jitter 30 --delay 80 --bitrate 400 --seconds 9",
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
            }
        );
        let conditions = options.conditions();
        assert_eq!(conditions.loss, 0.05);
        assert_eq!(conditions.jitter, Duration::from_millis(30));
        assert_eq!(conditions.delay, Duration::from_millis(80));
    }

    #[test]
    fn rejects_what_it_does_not_understand() {
        assert!(parse(args("--loss")).is_err());
        assert!(parse(args("--loss lots")).is_err());
        assert!(parse(args("--loss 150")).is_err());
        assert!(parse(args("--zoom 2")).is_err());
    }

    fn frame(keyframe: bool, tag: u8) -> EncodedFrame {
        EncodedFrame {
            data: vec![0, 0, 0, 1, tag],
            keyframe,
            timestamp: Duration::ZERO,
            rotation: Rotation::Deg0,
        }
    }

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    fn tags(frames: Vec<EncodedFrame>) -> Vec<u8> {
        frames.iter().map(|frame| frame.data[4]).collect()
    }

    #[test]
    fn frames_that_arrive_out_of_order_are_put_back_in_order() {
        let mut link = FrameLink::default();
        link.arrive(0, frame(true, 0), ms(0));
        link.arrive(2, frame(false, 2), ms(10));
        assert_eq!(tags(link.ready(ms(10))), [0], "waits for frame 1");
        link.arrive(1, frame(false, 1), ms(20));
        assert_eq!(tags(link.ready(ms(20))), [1, 2]);
        assert!(!link.wants_keyframe(false, ms(20)));
        assert_eq!(link.counters.delivered, 3);
    }

    #[test]
    fn a_frame_missing_for_too_long_is_lost_and_a_keyframe_is_owed() {
        let mut link = FrameLink::default();
        link.arrive(0, frame(true, 0), ms(0));
        link.arrive(2, frame(false, 2), ms(40));
        link.arrive(3, frame(false, 3), ms(70));
        assert_eq!(tags(link.ready(ms(70))), [0]);
        // Frame 1 never comes: the rest reference it, so they are dropped.
        assert!(link.ready(ms(40) + REORDER_WAIT).is_empty());
        assert_eq!(link.counters.missing, 1);
        assert_eq!(link.counters.skipped, 2);

        assert!(link.wants_keyframe(false, ms(130)));
        assert!(!link.wants_keyframe(false, ms(200)), "not again so soon");
        assert!(link.wants_keyframe(false, ms(130) + KEYFRAME_RETRY));
        assert_eq!(link.counters.keyframe_requests, 2);

        link.arrive(1, frame(false, 1), ms(500));
        assert_eq!(link.counters.late, 1);
        link.arrive(4, frame(false, 4), ms(510));
        link.arrive(5, frame(true, 5), ms(520));
        link.arrive(6, frame(false, 6), ms(530));
        assert_eq!(tags(link.ready(ms(530))), [5, 6]);
        assert!(!link.wants_keyframe(false, ms(2_000)));
    }

    #[test]
    fn the_decoder_can_ask_for_a_keyframe_too() {
        let mut link = FrameLink::default();
        assert!(link.wants_keyframe(true, ms(0)));
        assert!(!link.wants_keyframe(true, ms(10)));
    }

    #[test]
    fn the_stats_line_shows_what_the_viewer_should_watch() {
        let before = Totals {
            sent: 30,
            sent_bytes: 100_000,
            keyframes: 1,
            ..Totals::default()
        };
        let mut now = before;
        now.sent = 60;
        now.sent_bytes = 200_000;
        now.keyframes = 3;
        now.channel_dropped = 2;
        now.network_lost = 4;
        now.link.missing = 4;
        now.link.skipped = 5;
        now.link.keyframe_requests = 2;
        now.sink.decoded = 25;
        let line = stats_line(7, &now, &before);
        for expected in [
            "7s",
            "sent 30 fps",
            "800 kbit/s",
            "keyframes 2",
            "received 25 fps",
            "lost 4",
            "dropped 7",
            "keyframe requests 2",
        ] {
            assert!(line.contains(expected), "{expected:?} missing in {line:?}");
        }
    }
}
