//! End to end over the network simulator, on Tokio's paused clock: two video calls, each with a
//! fake camera and a fake display, sending to each other through lossy simulated paths.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use webrtc_engine::netsim::{Conditions, simulated_video_link};
use webrtc_engine::video::call::{
    RemoteVideo, VideoCall, VideoCallConfig, VideoStats, VideoTransport,
};
use webrtc_engine::video::fake::{FakeSink, FakeSource, SinkProbe, SourceProbe, frame_index};
use webrtc_engine::video::{Facing, VideoConfig};

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

struct Side {
    call: VideoCall,
    source: Arc<Mutex<SourceProbe>>,
    sink: Arc<Mutex<SinkProbe>>,
}

/// Two calls sending video to each other, one path each way.
fn call_pair(conditions: Conditions, seed: u64) -> (Side, Side) {
    let (a_out, b_in, a_feedback) = simulated_video_link(conditions, seed);
    let (b_out, a_in, b_feedback) = simulated_video_link(conditions, seed + 100);
    let side = |out, feedback, incoming| {
        let (source, sink) = (FakeSource::new(), FakeSink::new());
        let (source_probe, sink_probe) = (source.probe(), sink.probe());
        let transport = VideoTransport {
            frames: out,
            feedback,
            remote: RemoteVideo::new(incoming),
        };
        let call = VideoCall::start(Box::new(source), Box::new(sink), transport, CONFIG)
            .expect("the video call starts");
        Side {
            call,
            source: source_probe,
            sink: sink_probe,
        }
    };
    (side(a_out, a_feedback, a_in), side(b_out, b_feedback, b_in))
}

struct Seen {
    stats: VideoStats,
    /// The frame numbers the display showed, and whether each was a keyframe.
    shown: Vec<(u32, bool)>,
    broken: u64,
    /// Keyframe requests that reached the other side's camera.
    requests_to_other_camera: u64,
    sent_by_other_camera: u64,
}

async fn run(conditions: Conditions, seed: u64, seconds: u64) -> (Seen, Seen) {
    let (a, b) = call_pair(conditions, seed);
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    let a_stats = a.call.stop().await;
    let b_stats = b.call.stop().await;
    let seen =
        |stats: VideoStats, sink: &Arc<Mutex<SinkProbe>>, other: &Arc<Mutex<SourceProbe>>| {
            let sink = sink.lock().expect("the sink probe").clone();
            let other = other.lock().expect("the source probe").clone();
            Seen {
                stats,
                shown: sink
                    .shown
                    .iter()
                    .map(|frame| {
                        (
                            frame_index(&frame.data).expect("a fake frame"),
                            frame.keyframe,
                        )
                    })
                    .collect(),
                broken: sink.broken,
                requests_to_other_camera: other.keyframe_requests,
                sent_by_other_camera: other.sent,
            }
        };
    (
        seen(a_stats, &a.sink, &b.source),
        seen(b_stats, &b.sink, &a.source),
    )
}

/// Every frame shown decodes: numbers only go up, and after a gap comes a keyframe.
fn assert_decodable(name: &str, seen: &Seen) {
    assert_eq!(seen.broken, 0, "{name}: garbage on the screen");
    assert!(
        seen.shown.first().is_some_and(|&(_, keyframe)| keyframe),
        "{name}"
    );
    for pair in seen.shown.windows(2) {
        let ((before, _), (after, keyframe)) = (pair[0], pair[1]);
        assert!(after > before, "{name}: {before} then {after}");
        assert!(
            after == before + 1 || keyframe,
            "{name}: {after} follows {before} without a keyframe"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn video_crosses_a_clean_network_whole_and_in_order() {
    let conditions = Conditions {
        delay: Duration::from_millis(40),
        jitter: Duration::from_millis(10),
        ..Conditions::default()
    };
    let (a, b) = run(conditions, 1, 5).await;
    for (name, seen) in [("a", &a), ("b", &b)] {
        eprintln!("{name}: {:?}", seen.stats);
        assert_decodable(name, seen);
        let shown = seen.shown.len() as u64;
        assert!(
            shown + 3 >= seen.sent_by_other_camera,
            "{name}: {shown} shown"
        );
        assert_eq!(seen.stats.remote.dropped, 0, "{name}");
        assert_eq!(seen.stats.remote.keyframe_requests, 0, "{name}");
        assert_eq!(seen.requests_to_other_camera, 0, "{name}");
        assert_eq!(
            seen.shown.iter().filter(|(_, keyframe)| *keyframe).count(),
            1
        );
    }
}

#[tokio::test(start_paused = true)]
async fn after_a_loss_a_keyframe_is_requested_and_the_display_recovers_with_it() {
    let conditions = Conditions {
        delay: Duration::from_millis(40),
        jitter: Duration::from_millis(20),
        loss: 0.01,
        reorder: 0.01,
        ..Conditions::default()
    };
    let (a, b) = run(conditions, 7, 10).await;
    for (name, seen) in [("a", &a), ("b", &b)] {
        eprintln!(
            "{name}: shown {} of {}, {:?}",
            seen.shown.len(),
            seen.sent_by_other_camera,
            seen.stats
        );
        assert_decodable(name, seen);
        assert!(
            seen.stats.remote.dropped > 0,
            "{name}: the network lost nothing"
        );
        assert!(seen.stats.remote.keyframe_requests > 0, "{name}");
        assert!(
            seen.requests_to_other_camera > 0,
            "{name}: no request reached the camera"
        );
        let keyframes = seen.shown.iter().filter(|(_, keyframe)| *keyframe).count();
        assert!(keyframes > 1, "{name}: never recovered with a new keyframe");
        assert!(
            seen.shown.len() as u64 * 2 > seen.sent_by_other_camera,
            "{name}: under half the frames shown"
        );
        let last = seen.shown.last().map_or(0, |&(index, _)| index);
        assert!(
            u64::from(last) + 30 > seen.sent_by_other_camera,
            "{name}: the display stopped at {last}"
        );
    }
}
