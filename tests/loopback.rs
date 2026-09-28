//! Two in-process peers on loopback, host candidates only, no STUN: a call's Opus packets cross
//! real RTP tracks in order. The offer and answer are handed over directly.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::timeout;
use webrtc::media_stream::track_remote::TrackRemote;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionEventHandler, RTCIceGatheringState, RTCPeerConnectionState,
};
use webrtc_engine::rtp::{AudioReceiver, AudioSender, add_audio_track, peer_connection_builder};

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
    let (gathered_tx, gathered) = watch::channel(false);
    let (connected_tx, connected) = watch::channel(false);
    let (tracks_tx, tracks) = mpsc::unbounded_channel();
    let events = Events {
        gathered: gathered_tx,
        connected: connected_tx,
        tracks: tracks_tx,
    };
    let connection: Arc<dyn PeerConnection> = Arc::new(
        peer_connection_builder()
            .expect("the builder is ready")
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

#[tokio::test(flavor = "multi_thread")]
async fn opus_packets_cross_a_loopback_call_in_order_both_ways() {
    timeout(CALL_LIMIT, async {
        let mut caller = peer().await;
        let mut callee = peer().await;
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
