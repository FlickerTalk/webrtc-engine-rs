pub mod downlink;
pub mod uplink;

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, Interval, MissedTickBehavior, interval};

pub use downlink::{Downlink, ReceiveStats};
pub use uplink::{SendStats, Uplink};

use crate::audio::EngineIo;
use crate::codec;
use crate::rtp::{AudioPacket, AudioReceiver, AudioSender, RtpError};
use webrtc::media_stream::track_remote::TrackRemote;

pub trait PacketSink: Send + 'static {
    type Error: Send;
    fn send(&mut self, payload: &[u8]) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

pub trait PacketSource: Send + 'static {
    type Error: Send;
    fn recv(&mut self) -> impl Future<Output = Result<Option<AudioPacket>, Self::Error>> + Send;
}

impl PacketSink for AudioSender {
    type Error = RtpError;
    fn send(&mut self, payload: &[u8]) -> impl Future<Output = Result<(), RtpError>> + Send {
        AudioSender::send(self, payload)
    }
}

impl PacketSource for AudioReceiver {
    type Error = RtpError;
    fn recv(&mut self) -> impl Future<Output = Result<Option<AudioPacket>, RtpError>> + Send {
        self.recv_packet()
    }
}

/// webrtc-rs only fires `on_track` when the first RTP packet arrives, so the call starts with
/// this and the first `recv` waits for the track.
pub struct RemoteAudio {
    track: Option<oneshot::Receiver<Arc<dyn TrackRemote>>>,
    receiver: Option<AudioReceiver>,
}

impl RemoteAudio {
    pub fn new(track: oneshot::Receiver<Arc<dyn TrackRemote>>) -> Self {
        Self {
            track: Some(track),
            receiver: None,
        }
    }
}

impl PacketSource for RemoteAudio {
    type Error = RtpError;
    async fn recv(&mut self) -> Result<Option<AudioPacket>, RtpError> {
        if self.receiver.is_none() {
            let Some(pending) = self.track.take() else {
                return Ok(None);
            };
            match pending.await {
                Ok(track) => self.receiver = Some(AudioReceiver::new(track)),
                Err(_) => return Ok(None),
            }
        }
        match &self.receiver {
            Some(receiver) => receiver.recv_packet().await,
            None => Ok(None),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallConfig {
    /// This delay does not adapt, so keep it just enough for the device callback and scheduling.
    pub playout_queue_frames: usize,
    /// The rings have no wake-up, so they are polled.
    pub poll_interval: Duration,
}

impl Default for CallConfig {
    fn default() -> Self {
        Self {
            playout_queue_frames: 2,
            poll_interval: Duration::from_millis(5),
        }
    }
}

/// Counters only, no audio content or identifiers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallStats {
    pub send: SendStats,
    pub sent: u64,
    pub send_errors: u64,
    pub receive: ReceiveStats,
    pub receive_errors: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallError {
    Codec(codec::Error),
    NoRuntime,
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Codec(error) => write!(f, "codec: {error}"),
            Self::NoRuntime => f.write_str("a call needs a Tokio runtime"),
        }
    }
}

impl std::error::Error for CallError {}

impl From<codec::Error> for CallError {
    fn from(error: codec::Error) -> Self {
        Self::Codec(error)
    }
}

struct Shared {
    muted: AtomicBool,
    stats: Mutex<CallStats>,
}

impl Shared {
    fn update(&self, change: impl FnOnce(&mut CallStats)) {
        change(&mut self.stats.lock().unwrap_or_else(PoisonError::into_inner));
    }

    fn stats(&self) -> CallStats {
        self.stats
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

pub struct Call {
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

const INCOMING_CAPACITY: usize = 50;

impl Call {
    /// Must run inside a Tokio runtime.
    pub fn start<S: PacketSink, R: PacketSource>(
        io: EngineIo,
        sink: S,
        source: R,
        config: CallConfig,
    ) -> Result<Call, CallError> {
        let runtime = Handle::try_current().map_err(|_| CallError::NoRuntime)?;
        let uplink = Uplink::new(io.capture)?;
        let downlink = Downlink::new(io.playout, config.playout_queue_frames)?;
        let shared = Arc::new(Shared {
            muted: AtomicBool::new(false),
            stats: Mutex::new(CallStats::default()),
        });
        let (stop, stopped) = watch::channel(false);
        let (incoming, arrived) = mpsc::channel(INCOMING_CAPACITY);
        let tasks = vec![
            runtime.spawn(send_task(
                uplink,
                sink,
                shared.clone(),
                config,
                stopped.clone(),
            )),
            runtime.spawn(receive_task(
                source,
                incoming,
                shared.clone(),
                stopped.clone(),
            )),
            runtime.spawn(playout_task(
                downlink,
                arrived,
                shared.clone(),
                config,
                stopped,
            )),
        ];
        Ok(Call {
            shared,
            stop,
            tasks,
        })
    }

    /// Silence is sent while muted.
    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.shared.muted.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> CallStats {
        self.shared.stats()
    }

    pub async fn stop(mut self) -> CallStats {
        let _ = self.stop.send(true);
        for task in std::mem::take(&mut self.tasks) {
            let _ = task.await;
        }
        self.shared.stats()
    }
}

async fn stopping(stopped: &mut watch::Receiver<bool>) {
    let _ = stopped.wait_for(|stop| *stop).await;
}

fn ticker(config: CallConfig) -> Interval {
    let mut ticker = interval(config.poll_interval);
    // After a stall tick once. The rings say how much work there is, not the ticks.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker
}

async fn send_task<S: PacketSink>(
    mut uplink: Uplink,
    mut sink: S,
    shared: Arc<Shared>,
    config: CallConfig,
    mut stopped: watch::Receiver<bool>,
) {
    let mut ticker = ticker(config);
    let mut sent = 0;
    let mut errors = 0;
    let work = async {
        loop {
            ticker.tick().await;
            uplink.set_muted(shared.muted.load(Ordering::Relaxed));
            while let Some(packet) = uplink.next_packet() {
                match sink.send(&packet).await {
                    Ok(()) => sent += 1,
                    Err(_) => errors += 1,
                }
            }
            shared.update(|stats| {
                stats.send = uplink.stats();
                stats.sent = sent;
                stats.send_errors = errors;
            });
        }
    };
    tokio::select! {
        () = stopping(&mut stopped) => {}
        () = work => {}
    }
}

async fn receive_task<R: PacketSource>(
    mut source: R,
    incoming: mpsc::Sender<(AudioPacket, Duration)>,
    shared: Arc<Shared>,
    mut stopped: watch::Receiver<bool>,
) {
    let clock = Instant::now();
    let work = async {
        loop {
            match source.recv().await {
                Ok(Some(packet)) => {
                    // Stamp on arrival, the jitter buffer measures jitter from it.
                    let arrival = clock.elapsed();
                    // Only full if the playout task is stuck. A network would drop it too.
                    let _ = incoming.try_send((packet, arrival));
                }
                Ok(None) => break,
                Err(_) => {
                    shared.update(|stats| stats.receive_errors += 1);
                    break;
                }
            }
        }
    };
    tokio::select! {
        () = stopping(&mut stopped) => {}
        () = work => {}
    }
}

async fn playout_task(
    mut downlink: Downlink,
    mut arrived: mpsc::Receiver<(AudioPacket, Duration)>,
    shared: Arc<Shared>,
    config: CallConfig,
    mut stopped: watch::Receiver<bool>,
) {
    let mut ticker = ticker(config);
    let work = async {
        loop {
            ticker.tick().await;
            while let Ok((packet, arrival)) = arrived.try_recv() {
                downlink.receive(packet, arrival);
            }
            downlink.pump();
            shared.update(|stats| stats.receive = downlink.stats());
        }
    };
    tokio::select! {
        () = stopping(&mut stopped) => {}
        () = work => {}
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FRAME_SAMPLES;
    use crate::audio::{DeviceIo, audio_io};
    use std::f64::consts::TAU;

    struct WireSink {
        packets: mpsc::UnboundedSender<AudioPacket>,
        sequence: u16,
    }

    impl PacketSink for WireSink {
        type Error = ();
        async fn send(&mut self, payload: &[u8]) -> Result<(), ()> {
            let packet = AudioPacket {
                sequence: self.sequence,
                timestamp: u32::from(self.sequence) * FRAME_SAMPLES as u32,
                payload: payload.to_vec(),
            };
            self.sequence = self.sequence.wrapping_add(1);
            self.packets.send(packet).map_err(drop)
        }
    }

    struct WireSource(mpsc::UnboundedReceiver<AudioPacket>);

    impl PacketSource for WireSource {
        type Error = ();
        async fn recv(&mut self) -> Result<Option<AudioPacket>, ()> {
            Ok(self.0.recv().await)
        }
    }

    fn wire() -> (WireSink, WireSource) {
        let (packets, receiver) = mpsc::unbounded_channel();
        (
            WireSink {
                packets,
                sequence: 0,
            },
            WireSource(receiver),
        )
    }

    struct Refusing;

    impl PacketSink for Refusing {
        type Error = ();
        async fn send(&mut self, _payload: &[u8]) -> Result<(), ()> {
            Err(())
        }
    }

    async fn run_device(mut device: DeviceIo, duration: Duration) -> Vec<i16> {
        const CHUNK: usize = FRAME_SAMPLES / 2;
        let mut ticker = interval(Duration::from_millis(10));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Burst);
        let mut played = Vec::new();
        let chunks = duration.as_millis() as usize / 10;
        for index in 0..chunks {
            ticker.tick().await;
            let tone: Vec<i16> = (0..CHUNK)
                .map(|n| {
                    let t = (index * CHUNK + n) as f64 / 48_000.0;
                    (12_000.0 * (TAU * 440.0 * t).sin()) as i16
                })
                .collect();
            device.capture.push(&tone);
            let mut out = [0i16; CHUNK];
            device.playout.pop(&mut out);
            played.extend_from_slice(&out);
        }
        played
    }

    fn rms(samples: &[i16]) -> f64 {
        let energy: f64 = samples.iter().map(|&s| f64::from(s).powi(2)).sum();
        (energy / samples.len() as f64).sqrt()
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_carries_audio_from_the_microphone_to_the_speaker() {
        let (device, engine) = audio_io(16);
        let (sink, source) = wire();
        let call = Call::start(engine, sink, source, CallConfig::default()).unwrap();
        let played = run_device(device, Duration::from_secs(1)).await;
        let stats = call.stop().await;

        assert!((45..=50).contains(&stats.send.encoded), "{stats:?}");
        assert_eq!(stats.sent, stats.send.encoded);
        assert_eq!(stats.send_errors, 0);
        assert!(stats.receive.decoded >= 40, "{stats:?}");
        assert!(
            rms(&played[24_000..]) > 3_000.0,
            "rms {}",
            rms(&played[24_000..])
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_muted_call_sends_silence() {
        let (device, engine) = audio_io(16);
        let (sink, source) = wire();
        let call = Call::start(engine, sink, source, CallConfig::default()).unwrap();
        call.set_muted(true);
        assert!(call.is_muted());
        let played = run_device(device, Duration::from_secs(1)).await;
        let stats = call.stop().await;

        assert!(stats.send.muted >= 45, "{stats:?}");
        assert!(stats.receive.decoded >= 40, "{stats:?}");
        assert!(rms(&played) < 30.0, "rms {}", rms(&played));
    }

    #[tokio::test(start_paused = true)]
    async fn stop_ends_the_tasks() {
        let (_device, engine) = audio_io(16);
        let (sink, mut outgoing) = wire();
        let (incoming, source) = wire();
        let call = Call::start(engine, sink, source, CallConfig::default()).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!incoming.packets.is_closed());

        call.stop().await;

        assert!(incoming.packets.is_closed());
        assert_eq!(outgoing.recv().await, Ok(None));
    }

    #[tokio::test(start_paused = true)]
    async fn refused_packets_are_counted() {
        let (device, engine) = audio_io(16);
        let (_sink, source) = wire();
        let call = Call::start(engine, Refusing, source, CallConfig::default()).unwrap();
        run_device(device, Duration::from_millis(500)).await;
        let stats = call.stop().await;
        assert!(stats.send_errors >= 20, "{stats:?}");
        assert_eq!(stats.sent, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn remote_audio_ends_when_the_track_never_comes() {
        let (track, pending) = oneshot::channel();
        let mut remote = RemoteAudio::new(pending);
        drop(track);
        let received = tokio::time::timeout(Duration::from_secs(1), remote.recv()).await;
        assert!(matches!(received, Ok(Ok(None))));
    }

    #[test]
    fn starting_outside_a_runtime_fails() {
        let (_device, engine) = audio_io(16);
        let (sink, source) = wire();
        let result = Call::start(engine, sink, source, CallConfig::default());
        assert!(matches!(result, Err(CallError::NoRuntime)));
    }
}
