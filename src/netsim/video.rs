//! The simulator as a video call's transport, on Tokio's clock.
//!
//! [`simulated_video_link`] is one direction of a call's video: frames go in at
//! [`VideoLinkSender`], are cut into RTP packets as a webrtc-rs track would cut them, and cross
//! the simulated network to [`VideoLinkReceiver`]; its keyframe requests cross back, through a
//! second simulated path with the same conditions, to [`VideoLinkFeedback`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::time::{Instant, sleep_until};

use super::{Conditions, NetStats, NetworkSimulator};
use crate::video::EncodedFrame;
use crate::video::call::{Feedback, FeedbackSource, FrameSink, VideoPacketSource};
use crate::video::packet::{Packetizer, VideoPacket};

/// Longest wait between two looks at the network.
const POLL: Duration = Duration::from_millis(1);

/// One simulated path, and whether its sending end is gone.
struct Pipe<T> {
    network: Mutex<NetworkSimulator<T>>,
    clock: Instant,
    sender_gone: AtomicBool,
}

impl<T: Clone> Pipe<T> {
    fn new(conditions: Conditions, seed: u64) -> Arc<Self> {
        Arc::new(Self {
            network: Mutex::new(NetworkSimulator::new(conditions, seed)),
            clock: Instant::now(),
            sender_gone: AtomicBool::new(false),
        })
    }

    fn network(&self) -> MutexGuard<'_, NetworkSimulator<T>> {
        self.network.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn send(&self, item: T) {
        let now = self.clock.elapsed();
        self.network().send(item, now);
    }

    /// The next item off the path; `None` once the sender is gone and nothing is in flight.
    async fn recv(&self, ready: &mut VecDeque<T>) -> Option<T> {
        loop {
            if let Some(item) = ready.pop_front() {
                return Some(item);
            }
            let now = self.clock.elapsed();
            let next = {
                let mut network = self.network();
                ready.extend(network.deliver(now));
                network.next_arrival()
            };
            if !ready.is_empty() {
                continue;
            }
            let wake = match next {
                Some(arrival) => arrival.min(now + POLL),
                None if self.sender_gone.load(Ordering::Relaxed) => return None,
                None => now + POLL,
            };
            sleep_until(self.clock + wake).await;
        }
    }
}

/// One direction of a call's video over the simulated network. `seed` draws its losses and
/// delays; the feedback path draws from `seed + 1`.
pub fn simulated_video_link(
    conditions: Conditions,
    seed: u64,
) -> (VideoLinkSender, VideoLinkReceiver, VideoLinkFeedback) {
    let packets = Pipe::new(conditions, seed);
    let feedback = Pipe::new(conditions, seed.wrapping_add(1));
    (
        VideoLinkSender {
            packets: packets.clone(),
            packetizer: Packetizer::new(0, 0),
        },
        VideoLinkReceiver {
            packets,
            ready: VecDeque::new(),
            feedback: feedback.clone(),
        },
        VideoLinkFeedback {
            feedback,
            ready: VecDeque::new(),
        },
    )
}

/// The sending end: cuts frames into RTP packets.
pub struct VideoLinkSender {
    packets: Arc<Pipe<VideoPacket>>,
    packetizer: Packetizer,
}

impl FrameSink for VideoLinkSender {
    type Error = std::convert::Infallible;
    async fn send(&mut self, frame: &EncodedFrame) -> Result<(), Self::Error> {
        for packet in self.packetizer.packetize(frame) {
            self.packets.send(packet);
        }
        Ok(())
    }
}

impl Drop for VideoLinkSender {
    fn drop(&mut self) {
        self.packets.sender_gone.store(true, Ordering::Relaxed);
    }
}

/// The receiving end: the packets as the network delivers them. Its keyframe requests go back
/// to the sender's [`VideoLinkFeedback`].
pub struct VideoLinkReceiver {
    packets: Arc<Pipe<VideoPacket>>,
    ready: VecDeque<VideoPacket>,
    feedback: Arc<Pipe<Feedback>>,
}

impl VideoLinkReceiver {
    /// What the network did to the packets so far.
    pub fn stats(&self) -> NetStats {
        self.packets.network().stats()
    }
}

impl VideoPacketSource for VideoLinkReceiver {
    type Error = std::convert::Infallible;
    async fn recv(&mut self) -> Result<Option<VideoPacket>, Self::Error> {
        Ok(self.packets.recv(&mut self.ready).await)
    }
    async fn request_keyframe(&mut self) -> Result<(), Self::Error> {
        self.feedback.send(Feedback::KeyframeRequest);
        Ok(())
    }
}

impl Drop for VideoLinkReceiver {
    fn drop(&mut self) {
        self.feedback.sender_gone.store(true, Ordering::Relaxed);
    }
}

/// What the receiving end says about the video, back at the sender.
pub struct VideoLinkFeedback {
    feedback: Arc<Pipe<Feedback>>,
    ready: VecDeque<Feedback>,
}

impl FeedbackSource for VideoLinkFeedback {
    async fn recv(&mut self) -> Option<Feedback> {
        self.feedback.recv(&mut self.ready).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::Rotation;

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    fn frame(len: usize, millis: u64) -> EncodedFrame {
        let mut data = vec![0, 0, 0, 1, 0x41];
        data.extend((0..len).map(|i| (i % 255) as u8 + 1));
        EncodedFrame {
            data,
            keyframe: false,
            timestamp: ms(millis),
            rotation: Rotation::Deg0,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn frames_cross_as_numbered_packets_after_the_delay() {
        let conditions = Conditions {
            delay: ms(40),
            ..Conditions::default()
        };
        let (mut sender, mut receiver, _feedback) = simulated_video_link(conditions, 1);
        let start = Instant::now();
        sender.send(&frame(3_000, 0)).await.unwrap();

        let mut packets = Vec::new();
        while packets
            .last()
            .is_none_or(|packet: &VideoPacket| !packet.marker)
        {
            packets.push(receiver.recv().await.unwrap().expect("a packet"));
        }
        assert_eq!(start.elapsed(), ms(40));
        assert_eq!(packets.len(), 3, "3000 bytes in 1200-byte payloads");
        let sequences: Vec<u16> = packets.iter().map(|packet| packet.sequence).collect();
        assert_eq!(sequences, vec![0, 1, 2]);
        assert_eq!(receiver.stats().delivered, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn keyframe_requests_travel_back_to_the_sender() {
        let conditions = Conditions {
            delay: ms(25),
            ..Conditions::default()
        };
        let (_sender, mut receiver, mut feedback) = simulated_video_link(conditions, 1);
        let start = Instant::now();
        receiver.request_keyframe().await.unwrap();
        assert_eq!(feedback.recv().await, Some(Feedback::KeyframeRequest));
        assert_eq!(start.elapsed(), ms(25));
    }

    #[tokio::test(start_paused = true)]
    async fn each_end_sees_the_other_one_go() {
        let (sender, mut receiver, mut feedback) = simulated_video_link(Conditions::default(), 1);
        drop(sender);
        assert_eq!(receiver.recv().await, Ok(None));
        drop(receiver);
        assert_eq!(feedback.recv().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_lossy_path_loses_packets() {
        let conditions = Conditions {
            loss: 0.5,
            ..Conditions::default()
        };
        let (mut sender, mut receiver, _feedback) = simulated_video_link(conditions, 7);
        for index in 0..20 {
            sender.send(&frame(100, index * 33)).await.unwrap();
        }
        drop(sender);
        let mut delivered = 0;
        while receiver.recv().await.unwrap().is_some() {
            delivered += 1;
        }
        assert!((4..=16).contains(&delivered), "{delivered}");
        assert_eq!(receiver.stats().lost, 20 - delivered);
    }
}
