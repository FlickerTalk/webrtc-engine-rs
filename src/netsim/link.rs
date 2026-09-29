//! The simulator as a call's transport, on Tokio's clock: what one side sends through
//! [`LinkSender`] comes out of [`LinkReceiver`] as the simulated network delivers it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::time::{Instant, sleep_until};

use super::{Conditions, NetStats, NetworkSimulator};
use crate::FRAME_SAMPLES;
use crate::call::{PacketSink, PacketSource};
use crate::rtp::AudioPacket;

/// Longest wait between two looks at the network: a packet sent meanwhile may arrive sooner
/// than the one we were waiting for.
const POLL: std::time::Duration = std::time::Duration::from_millis(1);

struct Link {
    network: Mutex<NetworkSimulator<AudioPacket>>,
    clock: Instant,
    sender_gone: AtomicBool,
}

impl Link {
    fn network(&self) -> std::sync::MutexGuard<'_, NetworkSimulator<AudioPacket>> {
        self.network.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A one-way simulated path for Opus packets.
pub fn simulated_link(conditions: Conditions, seed: u64) -> (LinkSender, LinkReceiver) {
    let link = Arc::new(Link {
        network: Mutex::new(NetworkSimulator::new(conditions, seed)),
        clock: Instant::now(),
        sender_gone: AtomicBool::new(false),
    });
    (
        LinkSender {
            link: link.clone(),
            sequence: 0,
        },
        LinkReceiver {
            link,
            ready: VecDeque::new(),
        },
    )
}

/// The sending end: numbers and stamps packets as an RTP track would.
pub struct LinkSender {
    link: Arc<Link>,
    sequence: u16,
}

impl PacketSink for LinkSender {
    type Error = std::convert::Infallible;
    async fn send(&mut self, payload: &[u8]) -> Result<(), Self::Error> {
        let packet = AudioPacket {
            sequence: self.sequence,
            timestamp: u32::from(self.sequence).wrapping_mul(FRAME_SAMPLES as u32),
            payload: payload.to_vec(),
        };
        self.sequence = self.sequence.wrapping_add(1);
        let now = self.link.clock.elapsed();
        self.link.network().send(packet, now);
        Ok(())
    }
}

impl Drop for LinkSender {
    fn drop(&mut self) {
        self.link.sender_gone.store(true, Ordering::Relaxed);
    }
}

/// The receiving end.
pub struct LinkReceiver {
    link: Arc<Link>,
    ready: VecDeque<AudioPacket>,
}

impl LinkReceiver {
    /// What the simulated network did so far.
    pub fn stats(&self) -> NetStats {
        self.link.network().stats()
    }
}

impl PacketSource for LinkReceiver {
    type Error = std::convert::Infallible;
    async fn recv(&mut self) -> Result<Option<AudioPacket>, Self::Error> {
        loop {
            if let Some(packet) = self.ready.pop_front() {
                return Ok(Some(packet));
            }
            let now = self.link.clock.elapsed();
            let next = {
                let mut network = self.link.network();
                self.ready.extend(network.deliver(now));
                network.next_arrival()
            };
            if !self.ready.is_empty() {
                continue;
            }
            let wake = match next {
                Some(arrival) => arrival.min(now + POLL),
                None if self.link.sender_gone.load(Ordering::Relaxed) => return Ok(None),
                None => now + POLL,
            };
            sleep_until(self.link.clock + wake).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ms(milliseconds: u64) -> Duration {
        Duration::from_millis(milliseconds)
    }

    #[tokio::test(start_paused = true)]
    async fn delivers_numbered_packets_after_the_delay() {
        let conditions = Conditions {
            delay: ms(30),
            ..Conditions::default()
        };
        let (mut sender, mut receiver) = simulated_link(conditions, 1);
        let start = Instant::now();
        let talking = tokio::spawn(async move {
            for index in 0..3u8 {
                sender.send(&[index; 4]).await.unwrap();
                tokio::time::sleep(ms(20)).await;
            }
        });
        for index in 0..3u8 {
            let packet = receiver.recv().await.unwrap().unwrap();
            let arrived = start.elapsed();
            assert_eq!(packet.sequence, u16::from(index));
            assert_eq!(packet.timestamp, u32::from(index) * FRAME_SAMPLES as u32);
            assert_eq!(packet.payload, vec![index; 4]);
            let expected = ms(20 * u64::from(index) + 30);
            assert!(
                arrived >= expected && arrived - expected <= ms(1),
                "packet {index} at {arrived:?}"
            );
        }
        talking.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn ends_once_the_sender_is_gone_and_nothing_is_in_flight() {
        let conditions = Conditions {
            delay: ms(30),
            ..Conditions::default()
        };
        let (mut sender, mut receiver) = simulated_link(conditions, 1);
        sender.send(&[1]).await.unwrap();
        drop(sender);
        assert!(receiver.recv().await.unwrap().is_some());
        assert_eq!(receiver.recv().await.unwrap(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn counts_what_the_network_lost() {
        let conditions = Conditions {
            loss: 1.0,
            ..Conditions::default()
        };
        let (mut sender, mut receiver) = simulated_link(conditions, 1);
        for _ in 0..5 {
            sender.send(&[1]).await.unwrap();
        }
        drop(sender);
        assert_eq!(receiver.recv().await.unwrap(), None);
        let stats = receiver.stats();
        assert_eq!(stats.sent, 5);
        assert_eq!(stats.lost, 5);
    }
}
