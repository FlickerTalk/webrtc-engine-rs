//! The whole audio pipeline over the simulated network, without webrtc: a test voice goes into
//! the capture ring and comes out of the playout ring. A fake device and a fake clock move in
//! 5 ms steps, so the test is fast and gives the same result every run.

mod common;

use std::time::Duration;

use webrtc_engine::FRAME_SAMPLES;
use webrtc_engine::audio::audio_io;
use webrtc_engine::call::{Call, CallConfig, Downlink, ReceiveStats, Uplink};
use webrtc_engine::netsim::{Conditions, NetStats, NetworkSimulator, simulated_link};
use webrtc_engine::rtp::AudioPacket;

use common::{Alignment, align_windows, mean_correlation, test_signal};

const STEP: Duration = Duration::from_millis(5);
const STEP_SAMPLES: usize = FRAME_SAMPLES / 4;
const SECONDS: usize = 6;
const PLAYOUT_QUEUE: usize = 2;
/// Half a second of each side: the first second is left out, the encoder and the jitter
/// buffer are still settling.
const WINDOW: usize = 24_000;
const WINDOWS: usize = 8;
const FIRST_WINDOW: usize = 48_000;
const MAX_LAG: usize = 24_000;

struct Outcome {
    alignments: Vec<Alignment>,
    receive: ReceiveStats,
    network: NetStats,
    /// Device steps that found less than a step's worth of audio in the playout ring.
    device_underruns: usize,
    frames_played: u64,
}

fn call_over(conditions: Conditions, seed: u64) -> Outcome {
    let input = test_signal(SECONDS * 48_000);
    let (mut device, engine) = audio_io(8);
    let mut uplink = Uplink::new(engine.capture).expect("the encoder starts");
    let mut downlink = Downlink::new(engine.playout, PLAYOUT_QUEUE).expect("the decoder starts");
    let mut network = NetworkSimulator::new(conditions, seed);
    let (mut sequence, mut timestamp) = (0u16, 0u32);
    let mut output = Vec::with_capacity(input.len());
    let mut device_underruns = 0;

    for (step, chunk) in input.chunks(STEP_SAMPLES).enumerate() {
        let now = STEP * step as u32;
        device.capture.push(chunk);
        while let Some(payload) = uplink.next_packet() {
            let packet = AudioPacket {
                sequence,
                timestamp,
                payload,
            };
            network.send(packet, now);
            sequence = sequence.wrapping_add(1);
            timestamp = timestamp.wrapping_add(FRAME_SAMPLES as u32);
        }
        for packet in network.deliver(now) {
            downlink.receive(packet, now);
        }
        downlink.pump();
        let mut played = [0i16; STEP_SAMPLES];
        if device.playout.pop(&mut played) < STEP_SAMPLES {
            device_underruns += 1;
        }
        output.extend_from_slice(&played);
    }

    let receive = downlink.stats();
    Outcome {
        alignments: align_windows(&input, &output, FIRST_WINDOW, WINDOW, WINDOWS, MAX_LAG),
        frames_played: receive.decoded + receive.recovered + receive.concealed,
        receive,
        network: network.stats(),
        device_underruns,
    }
}

fn report(name: &str, outcome: &Outcome) {
    let correlations: Vec<String> = outcome
        .alignments
        .iter()
        .map(|a| format!("{:.3}@{}ms", a.correlation, a.lag / 48))
        .collect();
    eprintln!(
        "{name}: {correlations:?}\n  {:?}\n  {:?}\n  device underruns {}",
        outcome.receive, outcome.network, outcome.device_underruns
    );
}

#[test]
fn a_clean_network_carries_the_voice_through() {
    let conditions = Conditions {
        delay: Duration::from_millis(40),
        ..Conditions::default()
    };
    let outcome = call_over(conditions, 1);
    report("clean", &outcome);

    for alignment in &outcome.alignments {
        assert!(alignment.correlation > 0.9, "{alignment:?}");
        // Network delay plus jitter buffer, speaker queue and codec: well under 150 ms.
        assert!(alignment.lag < 150 * 48, "{alignment:?}");
    }
    let receive = &outcome.receive;
    assert_eq!(receive.recovered, 0);
    assert_eq!(receive.concealed, 0);
    assert_eq!(receive.underruns, 0);
    assert_eq!(receive.decode_errors, 0);
    assert_eq!(outcome.device_underruns, 0);
}

#[test]
fn a_lossy_jittery_network_is_concealed_without_underrun_storms() {
    let conditions = Conditions {
        delay: Duration::from_millis(40),
        jitter: Duration::from_millis(40),
        loss: 0.1,
        ..Conditions::default()
    };
    let outcome = call_over(conditions, 7);
    report("lossy", &outcome);

    let mean = mean_correlation(&outcome.alignments);
    assert!(mean > 0.7, "mean correlation {mean}");
    for alignment in &outcome.alignments {
        assert!(alignment.lag < 300 * 48, "{alignment:?}");
    }

    let receive = &outcome.receive;
    let lost = outcome.network.lost;
    assert!(lost > 0);
    // Losses are covered: rebuilt from FEC where the next packet was there, concealed otherwise.
    assert!(receive.recovered > 0, "{receive:?}");
    assert!(receive.concealed > 0, "{receive:?}");
    assert!(
        (receive.recovered + receive.concealed) * 10 >= lost * 8,
        "{lost} lost, {receive:?}"
    );
    // The buffer may run dry now and then, but not over and over.
    assert!(
        receive.underruns * 20 <= outcome.frames_played,
        "{} underruns in {} frames",
        receive.underruns,
        outcome.frames_played
    );
    assert_eq!(outcome.device_underruns, 0);
}

// What the demo runs: a `Call` on Tokio tasks over the simulated link, here on Tokio's paused
// clock with a fake device that talks into the microphone and listens to the speaker.
#[tokio::test(start_paused = true)]
async fn a_call_runs_over_the_simulated_link() {
    let conditions = Conditions {
        delay: Duration::from_millis(40),
        jitter: Duration::from_millis(20),
        loss: 0.05,
        ..Conditions::default()
    };
    let (sender, receiver) = simulated_link(conditions, 3);
    let (mut device, engine) = audio_io(16);
    let call =
        Call::start(engine, sender, receiver, CallConfig::default()).expect("the call starts");

    let input = test_signal(4 * 48_000);
    let mut output = Vec::with_capacity(input.len());
    let mut ticker = tokio::time::interval(Duration::from_millis(10));
    for chunk in input.chunks(FRAME_SAMPLES / 2) {
        ticker.tick().await;
        device.capture.push(chunk);
        let mut played = [0i16; FRAME_SAMPLES / 2];
        device.playout.pop(&mut played);
        output.extend_from_slice(&played);
    }
    let stats = call.stop().await;

    let alignments = align_windows(&input, &output, 48_000, 12_000, 8, MAX_LAG);
    let mean = mean_correlation(&alignments);
    eprintln!("simulated call: mean correlation {mean:.3}\n  {stats:?}");
    assert!(mean > 0.7, "mean correlation {mean}");
    assert!(stats.receive.decoded > 150, "{stats:?}");
    assert!(
        stats.receive.recovered + stats.receive.concealed > 0,
        "{stats:?}"
    );
}
