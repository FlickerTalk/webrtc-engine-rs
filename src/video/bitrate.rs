//! Adapting the encoder's bitrate to the path, from what the other side reports.
//!
//! A small loss-based controller in the spirit of Google Congestion Control's loss part
//! (draft-ietf-rmcat-gcc §6): heavy loss cuts the target in proportion to the loss, a clean
//! path raises it slowly, and anything in between holds it. A REMB from the receiver caps it.
//! Pure and deterministic: the caller passes the reports and the time.

use std::time::Duration;

use super::clamp_bitrate;

/// Loss above this share cuts the bitrate.
pub const LOSS_HIGH: f64 = 0.10;
/// Loss below this share lets the bitrate grow.
pub const LOSS_LOW: f64 = 0.02;
/// How much a clean report raises the bitrate.
pub const INCREASE_FACTOR: f64 = 1.05;
/// The shortest time between two raises: one step per second at most, however often reports
/// come.
pub const INCREASE_INTERVAL: Duration = Duration::from_secs(1);
/// The shortest time between two cuts, so one burst of loss seen in two reports is not cut
/// twice.
pub const DECREASE_INTERVAL: Duration = Duration::from_millis(300);
/// A round trip longer than this holds the bitrate even without loss: queues are building up.
pub const HIGH_ROUND_TRIP: Duration = Duration::from_millis(500);

/// Picks the encoder's target bitrate. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct BitrateController {
    target: u32,
    /// The receiver's last estimate (REMB), if it sent one.
    cap: Option<u32>,
    last_increase: Option<Duration>,
    last_decrease: Option<Duration>,
}

impl BitrateController {
    /// Starts at `start_bps`, clamped with [`clamp_bitrate`].
    pub fn new(start_bps: u32) -> Self {
        Self {
            target: clamp_bitrate(start_bps),
            cap: None,
            last_increase: None,
            last_decrease: None,
        }
    }

    /// The bitrate the encoder should use now.
    pub fn target(&self) -> u32 {
        self.target
    }

    /// Takes a receiver report: the share of packets lost since the previous one (0 to 1) and
    /// the round trip, if it could be measured. Returns the new target.
    pub fn on_report(
        &mut self,
        fraction_lost: f64,
        round_trip: Option<Duration>,
        now: Duration,
    ) -> u32 {
        let since = |last: Option<Duration>| last.map(|last| now.saturating_sub(last));
        if fraction_lost > LOSS_HIGH {
            if since(self.last_decrease).is_none_or(|elapsed| elapsed >= DECREASE_INTERVAL) {
                let factor = 1.0 - 0.5 * fraction_lost.min(1.0);
                self.set(f64::from(self.target) * factor);
                self.last_decrease = Some(now);
            }
        } else if fraction_lost < LOSS_LOW
            && round_trip.is_none_or(|round_trip| round_trip <= HIGH_ROUND_TRIP)
            && since(self.last_increase).is_none_or(|elapsed| elapsed >= INCREASE_INTERVAL)
        {
            self.set(f64::from(self.target) * INCREASE_FACTOR);
            self.last_increase = Some(now);
        }
        self.target
    }

    /// Takes a REMB: the receiver's estimate of what it can take, in bits per second. Returns
    /// the new target.
    pub fn on_remb(&mut self, bps: u64) -> u32 {
        let cap = u32::try_from(bps).unwrap_or(u32::MAX);
        self.cap = Some(cap);
        self.set(f64::from(self.target.min(cap)));
        self.target
    }

    fn set(&mut self, bps: f64) {
        // `as` saturates: a NaN or a huge value cannot wrap.
        let bps = bps.round() as u32;
        self.target = clamp_bitrate(self.cap.map_or(bps, |cap| bps.min(cap)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::{MAX_BITRATE_BPS, MIN_BITRATE_BPS};

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[test]
    fn starts_at_the_configured_bitrate_within_the_range() {
        assert_eq!(BitrateController::new(800_000).target(), 800_000);
        assert_eq!(BitrateController::new(0).target(), MIN_BITRATE_BPS);
        assert_eq!(BitrateController::new(u32::MAX).target(), MAX_BITRATE_BPS);
    }

    #[test]
    fn heavy_loss_cuts_the_bitrate_in_proportion() {
        let mut controller = BitrateController::new(1_000_000);
        // 20 % lost: a cut of half of that, 10 %.
        assert_eq!(controller.on_report(0.2, None, ms(0)), 900_000);
    }

    #[test]
    fn one_burst_of_loss_is_not_cut_twice() {
        let mut controller = BitrateController::new(1_000_000);
        controller.on_report(0.2, None, ms(0));
        assert_eq!(controller.on_report(0.2, None, ms(100)), 900_000);
        assert_eq!(
            controller.on_report(0.2, None, DECREASE_INTERVAL),
            810_000,
            "the loss goes on"
        );
    }

    #[test]
    fn moderate_loss_holds_the_bitrate() {
        let mut controller = BitrateController::new(1_000_000);
        for second in 0..5 {
            assert_eq!(
                controller.on_report(0.05, None, ms(second * 1_000)),
                1_000_000
            );
        }
    }

    #[test]
    fn a_clean_path_raises_the_bitrate_slowly() {
        let mut controller = BitrateController::new(1_000_000);
        assert_eq!(controller.on_report(0.0, None, ms(0)), 1_050_000);
        // Reports every 200 ms: still one raise per second.
        for step in 1..5 {
            assert_eq!(controller.on_report(0.0, None, ms(step * 200)), 1_050_000);
        }
        assert_eq!(controller.on_report(0.01, None, ms(1_000)), 1_102_500);
    }

    #[test]
    fn a_long_round_trip_holds_the_bitrate() {
        let mut controller = BitrateController::new(1_000_000);
        let slow = Some(HIGH_ROUND_TRIP + ms(1));
        assert_eq!(controller.on_report(0.0, slow, ms(0)), 1_000_000);
        assert_eq!(
            controller.on_report(0.0, Some(ms(80)), ms(1_000)),
            1_050_000
        );
    }

    #[test]
    fn the_bitrate_stays_within_the_range() {
        let mut controller = BitrateController::new(MIN_BITRATE_BPS);
        assert_eq!(controller.on_report(1.0, None, ms(0)), MIN_BITRATE_BPS);

        let mut controller = BitrateController::new(MAX_BITRATE_BPS);
        assert_eq!(controller.on_report(0.0, None, ms(0)), MAX_BITRATE_BPS);
    }

    #[test]
    fn a_clean_path_climbs_from_the_floor_to_the_ceiling_in_under_a_minute() {
        let mut controller = BitrateController::new(MIN_BITRATE_BPS);
        let seconds = (0..120)
            .find(|&second| controller.on_report(0.0, None, ms(second * 1_000)) == MAX_BITRATE_BPS)
            .expect("the ceiling is reached");
        assert!((50..60).contains(&seconds), "{seconds} s");
    }

    #[test]
    fn remb_caps_the_bitrate_and_its_raises() {
        let mut controller = BitrateController::new(1_000_000);
        assert_eq!(controller.on_remb(600_000), 600_000);
        assert_eq!(controller.on_report(0.0, None, ms(0)), 600_000);

        // A higher estimate lifts the cap; the bitrate climbs to it at its own pace.
        assert_eq!(controller.on_remb(2_000_000), 600_000);
        assert_eq!(controller.on_report(0.0, None, ms(1_000)), 630_000);
    }

    #[test]
    fn a_remb_below_the_floor_leaves_the_floor() {
        let mut controller = BitrateController::new(1_000_000);
        assert_eq!(controller.on_remb(10_000), MIN_BITRATE_BPS);
    }
}
