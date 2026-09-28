//! The numbers the camera and the VideoToolbox encoder are set up with.

use std::time::Duration;

use crate::video::VideoError;

/// Time between forced keyframes: a receiver that lost the stream recovers within it even if its
/// keyframe request is lost.
pub(crate) const KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);

/// The encoder's hard data-rate limit, as a multiple of the average bitrate (libwebrtc's too).
const DATA_RATE_LIMIT_FACTOR_TENTHS: u64 = 15;

/// `kVTCompressionPropertyKey_MaxKeyFrameInterval`: [`KEYFRAME_INTERVAL`] in frames at `fps`.
pub(crate) fn keyframe_interval_frames(fps: u32) -> u32 {
    let seconds = KEYFRAME_INTERVAL.as_secs() as u32;
    fps.max(1).saturating_mul(seconds)
}

/// `kVTCompressionPropertyKey_DataRateLimits` for a target of `bps`: at most this many bytes
/// over this many seconds.
pub(crate) fn data_rate_limit(bps: u32) -> (u32, f64) {
    let bytes = u64::from(bps) * DATA_RATE_LIMIT_FACTOR_TENTHS / 10 / 8;
    (u32::try_from(bytes).unwrap_or(u32::MAX), 1.0)
}

/// An `AVCaptureSessionPreset` that gives a fixed frame size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionPreset {
    Cif352x288,
    Vga640x480,
    Hd1280x720,
    Hd1920x1080,
}

/// The smallest preset at least as large as `width`×`height` (either way round), or the
/// largest.
pub(crate) fn session_preset(width: u32, height: u32) -> SessionPreset {
    let (long, short) = (width.max(height), width.min(height));
    [
        SessionPreset::Cif352x288,
        SessionPreset::Vga640x480,
        SessionPreset::Hd1280x720,
    ]
    .into_iter()
    .find(|preset| {
        let (w, h) = preset.size();
        long <= w && short <= h
    })
    .unwrap_or(SessionPreset::Hd1920x1080)
}

impl SessionPreset {
    /// The frame size it gives, landscape.
    pub(crate) fn size(self) -> (u32, u32) {
        match self {
            Self::Cif352x288 => (352, 288),
            Self::Vga640x480 => (640, 480),
            Self::Hd1280x720 => (1280, 720),
            Self::Hd1920x1080 => (1920, 1080),
        }
    }
}

/// `AVAuthorizationStatus` for video → whether the camera may be opened. Only `authorized` (3)
/// may: `notDetermined` (0) too is a refusal, because asking is the app's job.
pub(crate) fn check_authorization(status: isize) -> Result<(), VideoError> {
    const AUTHORIZED: isize = 3;
    if status == AUTHORIZED {
        Ok(())
    } else {
        Err(VideoError::PermissionDenied)
    }
}

/// Lets through frames at no more than a target rate, from a camera that may run faster.
#[derive(Debug, Clone)]
pub(crate) struct FramePacer {
    min_gap: Duration,
    last: Option<Duration>,
}

impl FramePacer {
    /// A pacer for `fps` frames per second.
    pub(crate) fn new(fps: u32) -> Self {
        // A little under one frame, so a camera at the same rate with some jitter passes whole.
        let frame = Duration::from_secs(1) / fps.max(1);
        Self {
            min_gap: frame * 4 / 5,
            last: None,
        }
    }

    /// Whether a frame captured at `timestamp` goes to the encoder.
    pub(crate) fn admit(&mut self, timestamp: Duration) -> bool {
        let due = match self.last {
            Some(last) => timestamp < last || timestamp - last >= self.min_gap,
            None => true,
        };
        if due {
            self.last = Some(timestamp);
        }
        due
    }
}

/// A `CMTime` (`value` / `timescale` seconds) as a [`Duration`], if it is a valid, non-negative
/// time.
pub(crate) fn cmtime_to_duration(value: i64, timescale: i32) -> Option<Duration> {
    let value = u64::try_from(value).ok()?;
    let timescale = u64::try_from(timescale).ok().filter(|&scale| scale > 0)?;
    let nanos = u128::from(value % timescale) * 1_000_000_000 / u128::from(timescale);
    Some(Duration::new(value / timescale, nanos as u32))
}

/// The timescale this crate writes `CMTime`s in: microseconds.
pub(crate) const CMTIME_TIMESCALE: i32 = 1_000_000;

/// A [`Duration`] as a `CMTime` value in [`CMTIME_TIMESCALE`], saturating.
pub(crate) fn duration_to_cmtime(duration: Duration) -> i64 {
    i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::{MAX_BITRATE_BPS, clamp_bitrate};

    #[test]
    fn keyframes_come_every_two_seconds_of_frames() {
        assert_eq!(keyframe_interval_frames(30), 60);
        assert_eq!(keyframe_interval_frames(15), 30);
        assert_eq!(keyframe_interval_frames(0), 2);
    }

    #[test]
    fn the_hard_limit_is_one_and_a_half_times_the_average_per_second() {
        assert_eq!(data_rate_limit(800_000), (150_000, 1.0));
        assert_eq!(data_rate_limit(clamp_bitrate(u32::MAX)), (468_750, 1.0));
        // No overflow on anything a u32 holds.
        assert_eq!(data_rate_limit(u32::MAX).0, 805_306_367);
        assert!(data_rate_limit(MAX_BITRATE_BPS).0 > 0);
    }

    #[test]
    fn the_smallest_preset_that_fits_is_chosen() {
        use SessionPreset::*;
        assert_eq!(session_preset(320, 240), Cif352x288);
        assert_eq!(session_preset(352, 288), Cif352x288);
        assert_eq!(session_preset(640, 480), Vga640x480);
        assert_eq!(session_preset(480, 640), Vga640x480);
        assert_eq!(session_preset(800, 600), Hd1280x720);
        assert_eq!(session_preset(1280, 720), Hd1280x720);
        assert_eq!(session_preset(1920, 1080), Hd1920x1080);
        assert_eq!(session_preset(4000, 3000), Hd1920x1080);
    }

    #[test]
    fn only_an_authorized_camera_is_opened() {
        assert_eq!(check_authorization(3), Ok(()));
        for status in [0, 1, 2, 4, -1] {
            assert_eq!(
                check_authorization(status),
                Err(VideoError::PermissionDenied),
                "{status}"
            );
        }
    }

    fn at(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[test]
    fn a_camera_at_the_target_rate_passes_whole() {
        let mut pacer = FramePacer::new(30);
        let admitted = (0..30).filter(|i| pacer.admit(at(i * 1000 / 30))).count();
        assert_eq!(admitted, 30);
    }

    #[test]
    fn a_faster_camera_is_thinned_to_the_target_rate() {
        let mut pacer = FramePacer::new(15);
        let admitted = (0..30).filter(|i| pacer.admit(at(i * 1000 / 30))).count();
        assert_eq!(admitted, 15);
    }

    #[test]
    fn a_clock_that_jumps_back_restarts_the_pacer() {
        let mut pacer = FramePacer::new(15);
        assert!(pacer.admit(at(1000)));
        assert!(pacer.admit(at(10)));
        assert!(!pacer.admit(at(20)));
    }

    #[test]
    fn cmtimes_become_durations() {
        assert_eq!(cmtime_to_duration(3, 2), Some(Duration::from_millis(1500)));
        assert_eq!(
            cmtime_to_duration(90_000, 90_000),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            cmtime_to_duration(1_000_000_001, 1_000_000_000),
            Some(Duration::new(1, 1))
        );
        assert_eq!(
            cmtime_to_duration(i64::MAX, 1),
            Some(Duration::from_secs(i64::MAX as u64))
        );
        assert_eq!(cmtime_to_duration(1, 0), None);
        assert_eq!(cmtime_to_duration(1, -1), None);
        assert_eq!(cmtime_to_duration(-1, 1), None);
    }

    #[test]
    fn durations_become_microsecond_cmtimes() {
        assert_eq!(duration_to_cmtime(Duration::from_millis(1500)), 1_500_000);
        assert_eq!(duration_to_cmtime(Duration::MAX), i64::MAX);
        assert_eq!(
            cmtime_to_duration(
                duration_to_cmtime(Duration::from_micros(33_367)),
                CMTIME_TIMESCALE
            ),
            Some(Duration::from_micros(33_367))
        );
    }
}
