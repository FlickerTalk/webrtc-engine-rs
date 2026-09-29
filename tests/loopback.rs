//! Two in-process peers on loopback, host candidates only, no STUN: a call's Opus packets and
//! H.264 frames cross real RTP tracks in order. The offer and answer are handed over directly.

mod common;

use std::sync::Arc;
use std::time::Duration;

use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodecParameters, RtpCodecKind};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{MissedTickBehavior, timeout};
use webrtc::media_stream::track_remote::TrackRemote;
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCIceGatheringState, RTCPeerConnectionState,
};
use webrtc_engine::FRAME_SAMPLES;
use webrtc_engine::audio::{DeviceIo, audio_io};
use webrtc_engine::call::{Call, CallConfig, CallStats, RemoteAudio};
use webrtc_engine::rtp::{
    AudioReceiver, AudioSender, add_audio_track, opus_codec, peer_connection_builder,
};
use webrtc_engine::video::call::{VideoCall, VideoCallConfig, VideoTransport};
use webrtc_engine::video::fake::{FakeSink, FakeSource, frame_index};
use webrtc_engine::video::rtp::{VideoReceiver, add_video_track};
use webrtc_engine::video::{Facing, Rotation, VideoConfig};

use common::{align_windows, mean_correlation, test_signal};

/// Each wait gets this long; the whole call gets `CALL_LIMIT`, so the test can never hang.
const STEP_LIMIT: Duration = Duration::from_secs(10);
const CALL_LIMIT: Duration = Duration::from_secs(30);
const PACKETS: usize = 25;
const FRAME: Duration = Duration::from_millis(20);
const SAMPLES_PER_FRAME: u32 = 960;

struct Events {
    gathered: watch::Sender<bool>,
    connected: watch::Sender<bool>,
    tracks: mpsc::UnboundedSender<Arc<dyn TrackRemote>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Events {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.send(true);
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if state == RTCPeerConnectionState::Connected {
            let _ = self.connected.send(true);
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let _ = self.tracks.send(track);
    }
}

struct Peer {
    connection: Arc<dyn PeerConnection>,
    sender: AudioSender,
    gathered: watch::Receiver<bool>,
    connected: watch::Receiver<bool>,
    tracks: mpsc::UnboundedReceiver<Arc<dyn TrackRemote>>,
}

async fn peer() -> Peer {
    peer_with(peer_connection_builder().expect("the builder is ready")).await
}

async fn peer_with(builder: PeerConnectionBuilder<String>) -> Peer {
    let (gathered_tx, gathered) = watch::channel(false);
    let (connected_tx, connected) = watch::channel(false);
    let (tracks_tx, tracks) = mpsc::unbounded_channel();
    let events = Events {
        gathered: gathered_tx,
        connected: connected_tx,
        tracks: tracks_tx,
    };
    let connection: Arc<dyn PeerConnection> = Arc::new(
        builder
            .with_handler(Arc::new(events))
            .with_udp_addrs(vec!["127.0.0.1:0".to_owned()])
            .build()
            .await
            .expect("the connection is built"),
    );
    let sender = add_audio_track(connection.as_ref())
        .await
        .expect("the audio track is added");
    Peer {
        connection,
        sender,
        gathered,
        connected,
        tracks,
    }
}

async fn wait(mut flag: watch::Receiver<bool>, what: &str) {
    timeout(STEP_LIMIT, flag.wait_for(|set| *set))
        .await
        .unwrap_or_else(|_| panic!("{what} timed out"))
        .unwrap_or_else(|_| panic!("{what}: the peer went away"));
}

/// Offer and answer, each sent once ICE gathering is complete, as FlickerTalk's signalling does.
async fn connect(caller: &Peer, callee: &Peer) {
    let offer = caller
        .connection
        .create_offer(None)
        .await
        .expect("an offer");
    caller
        .connection
        .set_local_description(offer)
        .await
        .expect("the offer is set");
    wait(caller.gathered.clone(), "the caller's ICE gathering").await;
    let offer = caller
        .connection
        .local_description()
        .await
        .expect("the gathered offer");

    callee
        .connection
        .set_remote_description(offer)
        .await
        .expect("the callee takes the offer");
    let answer = callee
        .connection
        .create_answer(None)
        .await
        .expect("an answer");
    callee
        .connection
        .set_local_description(answer)
        .await
        .expect("the answer is set");
    wait(callee.gathered.clone(), "the callee's ICE gathering").await;
    let answer = callee
        .connection
        .local_description()
        .await
        .expect("the gathered answer");

    caller
        .connection
        .set_remote_description(answer)
        .await
        .expect("the caller takes the answer");
    wait(caller.connected.clone(), "the caller's connection").await;
    wait(callee.connected.clone(), "the callee's connection").await;
}

fn frame(index: usize) -> Vec<u8> {
    // Distinct sizes and contents, so a lost, repeated or swapped packet shows.
    (0..=index).map(|byte| (index * 7 + byte) as u8).collect()
}

async fn speak(sender: &AudioSender) {
    let mut ticker = tokio::time::interval(FRAME);
    for index in 0..PACKETS {
        ticker.tick().await;
        sender
            .send(&frame(index))
            .await
            .expect("the packet is sent");
    }
}

async fn listen(tracks: &mut mpsc::UnboundedReceiver<Arc<dyn TrackRemote>>) {
    let track = timeout(STEP_LIMIT, tracks.recv())
        .await
        .expect("the remote track arrives in time")
        .expect("the remote track arrives");
    let receiver = AudioReceiver::new(track);

    let mut previous = None;
    for index in 0..PACKETS {
        let packet = timeout(STEP_LIMIT, receiver.recv_packet())
            .await
            .unwrap_or_else(|_| panic!("packet {index} arrives in time"))
            .expect("the track is readable")
            .unwrap_or_else(|| panic!("the track ended before packet {index}"));
        assert_eq!(
            packet.payload,
            frame(index),
            "packet {index} carries its Opus payload"
        );
        if let Some((sequence, timestamp)) = previous {
            assert_eq!(
                packet.sequence,
                u16::wrapping_add(sequence, 1),
                "consecutive sequence"
            );
            assert_eq!(
                packet.timestamp,
                u32::wrapping_add(timestamp, SAMPLES_PER_FRAME)
            );
        }
        previous = Some((packet.sequence, packet.timestamp));
    }
}

/// Both sides speak and listen at once, then hang up.
async fn call(mut caller: Peer, mut callee: Peer) {
    timeout(CALL_LIMIT, async {
        connect(&caller, &callee).await;
        tokio::join!(
            speak(&caller.sender),
            speak(&callee.sender),
            listen(&mut callee.tracks),
            listen(&mut caller.tracks),
        );
        caller
            .connection
            .close()
            .await
            .expect("the caller hangs up");
        callee
            .connection
            .close()
            .await
            .expect("the callee hangs up");
    })
    .await
    .expect("the call finishes within the limit");
}

#[tokio::test(flavor = "multi_thread")]
async fn opus_packets_cross_a_loopback_call_in_order_both_ways() {
    call(peer().await, peer().await).await;
}

// Firefox offers Opus as payload type 109, not Chrome's 111. The answer keeps the offerer's
// number, so our side has to send with 109 or its packets are refused.
#[tokio::test(flavor = "multi_thread")]
async fn answering_an_offer_that_numbers_opus_differently_still_carries_audio() {
    let mut engine = MediaEngine::default();
    engine
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: opus_codec(),
                payload_type: 109,
            },
            RtpCodecKind::Audio,
        )
        .expect("opus is registered as 109");
    let caller = peer_with(PeerConnectionBuilder::new().with_media_engine(engine)).await;
    call(caller, peer().await).await;
}

const VOICE_SECONDS: usize = 3;

/// The test voice, or its mirror image for the other side: hearing one's own voice instead of
/// the other side's would then correlate negatively.
fn voice(inverted: bool) -> Vec<i16> {
    let voice = test_signal(VOICE_SECONDS * 48_000);
    if inverted {
        voice.into_iter().map(|s| s.saturating_neg()).collect()
    } else {
        voice
    }
}

/// A device in real time: every 10 ms it captures 10 ms of `voice` and plays 10 ms.
/// Returns what it played.
async fn talk(mut device: DeviceIo, voice: Vec<i16>) -> Vec<i16> {
    const CHUNK: usize = FRAME_SAMPLES / 2;
    let mut ticker = tokio::time::interval(Duration::from_millis(10));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Burst);
    let mut played = Vec::with_capacity(voice.len());
    for chunk in voice.chunks(CHUNK) {
        ticker.tick().await;
        device.capture.push(chunk);
        let mut out = [0i16; CHUNK];
        device.playout.pop(&mut out);
        played.extend_from_slice(&out);
    }
    played
}

/// Starts the whole pipeline on one side: its microphone ring goes out through `sender`, and
/// the other side's track, once `on_track` hands it over, plays into its speaker ring.
fn start_call(
    sender: AudioSender,
    mut tracks: mpsc::UnboundedReceiver<Arc<dyn TrackRemote>>,
) -> (Call, DeviceIo) {
    let (device, engine) = audio_io(16);
    let (track, pending) = oneshot::channel();
    tokio::spawn(async move {
        if let Some(remote) = tracks.recv().await {
            let _ = track.send(remote);
        }
    });
    let call = Call::start(
        engine,
        sender,
        RemoteAudio::new(pending),
        CallConfig::default(),
    )
    .expect("the call starts");
    (call, device)
}

fn assert_heard(name: &str, played: &[i16], other_voice: &[i16], stats: &CallStats) {
    // From the second second on, in quarter-second windows.
    let alignments = align_windows(other_voice, played, 48_000, 12_000, 6, 24_000);
    let mean = mean_correlation(&alignments);
    eprintln!("{name}: mean correlation {mean:.3}, {alignments:?}\n  {stats:?}");
    assert!(mean > 0.7, "{name}: mean correlation {mean}");
    assert!(stats.receive.decoded > 100, "{name}: {stats:?}");
    assert_eq!(stats.send_errors, 0, "{name}: {stats:?}");
}

// The whole pipeline on both sides of a real webrtc-rs call: microphone ring, Opus, RTP over
// loopback, jitter buffer, Opus, speaker ring.
#[tokio::test(flavor = "multi_thread")]
async fn a_voice_crosses_a_loopback_call_through_the_whole_pipeline() {
    timeout(CALL_LIMIT, async {
        let caller = peer().await;
        let callee = peer().await;
        connect(&caller, &callee).await;
        let (caller_call, caller_device) = start_call(caller.sender, caller.tracks);
        let (callee_call, callee_device) = start_call(callee.sender, callee.tracks);

        let (caller_heard, callee_heard) = tokio::join!(
            talk(caller_device, voice(false)),
            talk(callee_device, voice(true))
        );
        let caller_stats = caller_call.stop().await;
        let callee_stats = callee_call.stop().await;
        caller
            .connection
            .close()
            .await
            .expect("the caller hangs up");
        callee
            .connection
            .close()
            .await
            .expect("the callee hangs up");

        assert_heard("callee", &callee_heard, &voice(false), &callee_stats);
        assert_heard("caller", &caller_heard, &voice(true), &caller_stats);
    })
    .await
    .expect("the call finishes within the limit");
}

/// Splits the remote tracks `on_track` hands over by kind: the first audio track goes to the
/// first channel, the first video track to the second.
type RemoteTrack = Arc<dyn TrackRemote>;

fn by_kind(
    mut tracks: mpsc::UnboundedReceiver<RemoteTrack>,
) -> (
    mpsc::UnboundedReceiver<RemoteTrack>,
    oneshot::Receiver<RemoteTrack>,
) {
    let (audio, audio_tracks) = mpsc::unbounded_channel();
    let (video, video_track) = oneshot::channel();
    tokio::spawn(async move {
        let mut video = Some(video);
        while let Some(track) = tracks.recv().await {
            if track.kind().await == RtpCodecKind::Video {
                if let Some(video) = video.take() {
                    let _ = video.send(track);
                }
            } else {
                let _ = audio.send(track);
            }
        }
    });
    (audio_tracks, video_track)
}

const VIDEO_CONFIG: VideoCallConfig = VideoCallConfig {
    video: VideoConfig {
        width: 320,
        height: 240,
        fps: 30,
        bitrate_bps: 300_000,
    },
    facing: Facing::Front,
    poll_interval: Duration::from_millis(5),
};
const VIDEO_FRAMES: usize = 30;

// Opus and H.264 on one connection, as a video call carries them: the caller's camera crosses
// real RTP (FU-A, STAP-A, CVO) to the callee's receiver while both sides talk, and a PLI from
// the callee reaches the caller's camera through the RTCP feedback path.
#[tokio::test(flavor = "multi_thread")]
async fn audio_and_video_share_a_loopback_call_and_a_pli_reaches_the_camera() {
    timeout(CALL_LIMIT, async {
        let caller = peer().await;
        let caller_video = add_video_track(caller.connection.as_ref())
            .await
            .expect("the caller's video track");
        let callee = peer().await;
        let callee_video = add_video_track(callee.connection.as_ref())
            .await
            .expect("the callee's video track");
        connect(&caller, &callee).await;
        let (mut caller_audio, caller_video_track) = by_kind(caller.tracks);
        let (mut callee_audio, callee_video_track) = by_kind(callee.tracks);

        let camera = FakeSource::new().with_rotation(Rotation::Deg90);
        let camera_probe = camera.probe();
        let transport = VideoTransport {
            feedback: caller_video.feedback(),
            remote: VideoReceiver::pending(caller_video_track, &caller_video),
            frames: caller_video,
        };
        let video_call = VideoCall::start(
            Box::new(camera),
            Box::new(FakeSink::new()),
            transport,
            VIDEO_CONFIG,
        )
        .expect("the video call starts");

        let watch = async {
            let mut receiver = VideoReceiver::pending(callee_video_track, &callee_video);
            let mut frames = Vec::new();
            while frames.len() < VIDEO_FRAMES {
                let frame = timeout(STEP_LIMIT, receiver.recv())
                    .await
                    .expect("a frame in time")
                    .expect("the track is readable")
                    .expect("the track goes on");
                frames.push(frame);
            }
            receiver
                .request_keyframe()
                .await
                .expect("the PLI is written");
            let keyframe = loop {
                let frame = timeout(STEP_LIMIT, receiver.recv())
                    .await
                    .expect("a keyframe in time")
                    .expect("the track is readable")
                    .expect("the track goes on");
                if frame.keyframe {
                    break frame;
                }
            };
            (frames, keyframe, receiver.stats())
        };
        let ((), (), (), (frames, keyframe, received)) = tokio::join!(
            speak(&caller.sender),
            speak(&callee.sender),
            async {
                listen(&mut callee_audio).await;
                listen(&mut caller_audio).await;
            },
            watch,
        );
        let sent = video_call.stop().await;
        caller
            .connection
            .close()
            .await
            .expect("the caller hangs up");
        callee
            .connection
            .close()
            .await
            .expect("the callee hangs up");
        eprintln!("sent {sent:?}\nreceived {received:?}");

        assert!(frames[0].keyframe, "the first frame is a keyframe");
        let first = frame_index(&frames[0].data).expect("a fake frame");
        for (offset, frame) in frames.iter().enumerate() {
            assert_eq!(
                frame_index(&frame.data),
                Some(first + offset as u32),
                "in order"
            );
            assert_eq!(frame.rotation, Rotation::Deg90, "CVO carries the rotation");
        }
        // The fake camera stamps real capture times, 1/30 s apart give or take the scheduler.
        let span = frames[VIDEO_FRAMES - 1].timestamp - frames[0].timestamp;
        let expected = Duration::from_secs(1) * (VIDEO_FRAMES as u32 - 1) / 30;
        assert!(
            span.abs_diff(expected) < expected / 5,
            "90 kHz timestamps: {span:?} for {expected:?}"
        );
        assert!(frame_index(&keyframe.data) > frame_index(&frames[VIDEO_FRAMES - 1].data));
        assert_eq!(received.dropped, 0);
        assert!(
            camera_probe.lock().expect("the probe").keyframe_requests >= 1,
            "the PLI reached the camera"
        );
        assert!(sent.keyframe_requests_received >= 1, "{sent:?}");
        assert_eq!(sent.send_errors, 0, "{sent:?}");
    })
    .await
    .expect("the call finishes within the limit");
}
