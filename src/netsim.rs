//! A simulated network for tests and demos: delay, random jitter, loss, reordering and
//! duplication, all deterministic for a given seed.
//!
//! [`NetworkSimulator`] is sans-IO: the caller says what time it is, as an offset on any
//! monotonic clock. [`simulated_link`] runs it as a call's transport on Tokio's clock.

mod link;

use std::collections::BTreeMap;
use std::time::Duration;

pub use link::{LinkReceiver, LinkSender, simulated_link};

/// How the simulated network treats every packet.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Conditions {
    /// One-way delay every packet gets.
    pub delay: Duration,
    /// Extra delay, uniform between zero and this, drawn for each packet. Beyond the packet
    /// spacing it reorders packets too, as a real network does.
    pub jitter: Duration,
    /// Share of packets lost, from 0 to 1.
    pub loss: f64,
    /// Share of packets held back by [`REORDER_HOLD`], so that later ones overtake them.
    pub reorder: f64,
    /// Share of packets delivered twice, the copy with its own delay.
    pub duplicate: f64,
}

/// Extra delay of a reordered packet: more than two 20 ms frames, so it lands behind the next
/// packets.
pub const REORDER_HOLD: Duration = Duration::from_millis(50);

/// What the simulated network did so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetStats {
    /// Packets handed to [`NetworkSimulator::send`].
    pub sent: u64,
    /// Packets the network dropped.
    pub lost: u64,
    /// Extra copies the network made.
    pub duplicated: u64,
    /// Packets held back to be overtaken.
    pub reordered: u64,
    /// Packets (copies included) handed out by [`NetworkSimulator::deliver`].
    pub delivered: u64,
}

/// A one-way network path.
pub struct NetworkSimulator<T> {
    conditions: Conditions,
    rng: SplitMix64,
    stats: NetStats,
    /// Packets in flight by arrival time, then by send order so that ties keep their order.
    in_flight: BTreeMap<(Duration, u64), T>,
    sent_order: u64,
}

impl<T: Clone> NetworkSimulator<T> {
    /// A path with `conditions`; the same `seed` always gives the same losses and delays.
    pub fn new(conditions: Conditions, seed: u64) -> Self {
        Self {
            conditions,
            rng: SplitMix64(seed),
            stats: NetStats::default(),
            in_flight: BTreeMap::new(),
            sent_order: 0,
        }
    }

    /// Puts `packet` on the network at `now`.
    pub fn send(&mut self, packet: T, now: Duration) {
        self.stats.sent += 1;
        // Every packet draws the same numbers whatever happens to it, so changing one
        // condition does not reshuffle the others.
        let lost = self.rng.unit() < self.conditions.loss;
        let reordered = self.rng.unit() < self.conditions.reorder;
        let duplicated = self.rng.unit() < self.conditions.duplicate;
        let delay = self.delay();
        let copy_delay = self.delay();
        if lost {
            self.stats.lost += 1;
            return;
        }
        let hold = if reordered {
            self.stats.reordered += 1;
            REORDER_HOLD
        } else {
            Duration::ZERO
        };
        if duplicated {
            self.stats.duplicated += 1;
            self.schedule(packet.clone(), now + copy_delay + hold);
        }
        self.schedule(packet, now + delay + hold);
    }

    fn delay(&mut self) -> Duration {
        self.conditions.delay + self.conditions.jitter.mul_f64(self.rng.unit())
    }

    fn schedule(&mut self, packet: T, arrival: Duration) {
        self.in_flight.insert((arrival, self.sent_order), packet);
        self.sent_order += 1;
    }

    /// The packets that have arrived by `now`, in arrival order.
    pub fn deliver(&mut self, now: Duration) -> Vec<T> {
        let mut arrived = Vec::new();
        while let Some(entry) = self.in_flight.first_entry() {
            if entry.key().0 > now {
                break;
            }
            arrived.push(entry.remove());
        }
        self.stats.delivered += arrived.len() as u64;
        arrived
    }

    /// When the next packet in flight arrives, if any is.
    pub fn next_arrival(&self) -> Option<Duration> {
        self.in_flight
            .first_key_value()
            .map(|(&(arrival, _), _)| arrival)
    }

    pub fn stats(&self) -> NetStats {
        self.stats.clone()
    }
}

/// SplitMix64: tiny, and its output for a seed never changes with a dependency upgrade, so a
/// seeded test sees the same network forever. Not for anything secret.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(milliseconds: u64) -> Duration {
        Duration::from_millis(milliseconds)
    }

    /// Sends `count` numbered packets 20 ms apart and collects every delivery, checking the
    /// network each millisecond, as (packet, arrival in ms).
    fn run(conditions: Conditions, seed: u64, count: u32) -> (Vec<(u32, u64)>, NetStats) {
        let mut network = NetworkSimulator::new(conditions, seed);
        let mut arrivals = Vec::new();
        let end = u64::from(count) * 20 + 2_000;
        for now_ms in 0..end {
            let now = ms(now_ms);
            if now_ms % 20 == 0 && now_ms / 20 < u64::from(count) {
                network.send((now_ms / 20) as u32, now);
            }
            for packet in network.deliver(now) {
                arrivals.push((packet, now_ms));
            }
        }
        (arrivals, network.stats())
    }

    fn out_of_order(arrivals: &[(u32, u64)]) -> usize {
        arrivals.windows(2).filter(|w| w[1].0 < w[0].0).count()
    }

    #[test]
    fn a_clean_network_delivers_everything_in_order_after_the_delay() {
        let conditions = Conditions {
            delay: ms(30),
            ..Conditions::default()
        };
        let (arrivals, stats) = run(conditions, 1, 100);
        let expected: Vec<(u32, u64)> = (0..100).map(|n| (n, u64::from(n) * 20 + 30)).collect();
        assert_eq!(arrivals, expected);
        assert_eq!(stats.sent, 100);
        assert_eq!(stats.delivered, 100);
        assert_eq!(stats.lost, 0);
    }

    #[test]
    fn tells_when_the_next_packet_arrives() {
        let mut network = NetworkSimulator::new(
            Conditions {
                delay: ms(30),
                ..Conditions::default()
            },
            1,
        );
        assert_eq!(network.next_arrival(), None);
        network.send(7u8, ms(100));
        assert_eq!(network.next_arrival(), Some(ms(130)));
        assert!(network.deliver(ms(129)).is_empty());
        assert_eq!(network.deliver(ms(130)), vec![7]);
        assert_eq!(network.next_arrival(), None);
    }

    // Jitter spreads arrivals over [delay, delay + jitter]; beyond the 20 ms spacing it also
    // swaps packets.
    #[test]
    fn jitter_spreads_arrivals_and_reorders() {
        let conditions = Conditions {
            delay: ms(30),
            jitter: ms(40),
            ..Conditions::default()
        };
        let (arrivals, _) = run(conditions, 7, 500);
        assert_eq!(arrivals.len(), 500);
        let mut extra = Vec::new();
        for &(packet, arrival) in &arrivals {
            let sent = u64::from(packet) * 20;
            let late = arrival - sent - 30;
            assert!(late <= 40, "packet {packet} {late} ms late");
            extra.push(late);
        }
        let mean = extra.iter().sum::<u64>() as f64 / extra.len() as f64;
        assert!((15.0..25.0).contains(&mean), "mean extra delay {mean} ms");
        assert!(out_of_order(&arrivals) > 10);
    }

    #[test]
    fn loses_about_the_share_asked_for() {
        let conditions = Conditions {
            loss: 0.1,
            ..Conditions::default()
        };
        let (arrivals, stats) = run(conditions, 3, 10_000);
        assert!((800..1_200).contains(&stats.lost), "lost {}", stats.lost);
        assert_eq!(arrivals.len() as u64, 10_000 - stats.lost);
        assert_eq!(stats.delivered, 10_000 - stats.lost);
    }

    #[test]
    fn reorders_packets_even_without_jitter() {
        let conditions = Conditions {
            reorder: 0.1,
            ..Conditions::default()
        };
        let (arrivals, stats) = run(conditions, 5, 1_000);
        assert_eq!(arrivals.len(), 1_000);
        assert!((50..150).contains(&stats.reordered), "{}", stats.reordered);
        assert!(out_of_order(&arrivals) > 0);
        for &(packet, arrival) in &arrivals {
            let late = arrival - u64::from(packet) * 20;
            assert!(late == 0 || late == REORDER_HOLD.as_millis() as u64);
        }
    }

    #[test]
    fn duplicates_packets() {
        let conditions = Conditions {
            duplicate: 0.1,
            ..Conditions::default()
        };
        let (arrivals, stats) = run(conditions, 9, 1_000);
        assert!(
            (50..150).contains(&stats.duplicated),
            "{}",
            stats.duplicated
        );
        assert_eq!(arrivals.len() as u64, 1_000 + stats.duplicated);
        let mut packets: Vec<u32> = arrivals.iter().map(|&(packet, _)| packet).collect();
        packets.dedup();
        packets.sort_unstable();
        packets.dedup();
        assert_eq!(packets, (0..1_000).collect::<Vec<_>>());
    }

    #[test]
    fn the_same_seed_gives_the_same_network() {
        let conditions = Conditions {
            delay: ms(10),
            jitter: ms(40),
            loss: 0.1,
            reorder: 0.05,
            duplicate: 0.05,
        };
        let (first, _) = run(conditions, 42, 300);
        let (again, _) = run(conditions, 42, 300);
        let (other, _) = run(conditions, 43, 300);
        assert_eq!(first, again);
        assert_ne!(first, other);
    }
}
