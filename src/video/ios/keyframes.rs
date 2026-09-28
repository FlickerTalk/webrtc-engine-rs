//! How the display side asks for a keyframe, and when it does.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Keyframe requests from a [`DisplaySink`](super::DisplaySink) to the pipeline.
///
/// The sink cannot reach the sender, so it raises a flag: whenever it cannot decode what comes
/// next (the first frames of a call, a delta frame after a decode failure, a layer that the
/// system flushed while the app was in the background) it calls for a keyframe here. The
/// pipeline polls [`KeyframeRequests::take`] after each [`push`](crate::video::VideoSink::push)
/// (or on a timer) and, when it returns `true`, sends the other side an RTCP PLI. Requests made
/// between two polls collapse into one. The sink already spaces them out
/// ([`KEYFRAME_RETRY`]), so the pipeline may send a PLI for every `true`.
///
/// Cheap to clone; every clone sees the same flag.
#[derive(Debug, Clone, Default)]
pub struct KeyframeRequests {
    inner: Arc<Requests>,
}

#[derive(Debug, Default)]
struct Requests {
    pending: AtomicBool,
    total: AtomicU64,
}

impl KeyframeRequests {
    /// Raises the flag.
    pub(crate) fn request(&self) {
        self.inner.total.fetch_add(1, Ordering::Relaxed);
        self.inner.pending.store(true, Ordering::Relaxed);
    }

    /// Whether a keyframe was asked for since the last call, clearing the flag.
    pub fn take(&self) -> bool {
        self.inner.pending.swap(false, Ordering::Relaxed)
    }

    /// Keyframes asked for so far, counted before collapsing.
    pub fn total(&self) -> u64 {
        self.inner.total.load(Ordering::Relaxed)
    }
}

/// While the decoder waits for a keyframe, how often (in frame time) it asks again.
pub(crate) const KEYFRAME_RETRY: Duration = Duration::from_millis(500);

/// What to do with one received frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Admission {
    /// Hand it to the decoder.
    pub decode: bool,
    /// Ask the sender for a keyframe.
    pub request_keyframe: bool,
}

/// Keeps delta frames away from a decoder that has no reference to decode them against.
#[derive(Debug, Clone)]
pub(crate) struct DecodeGate {
    waiting: bool,
    last_request: Option<Duration>,
}

impl DecodeGate {
    /// A gate waiting for the first keyframe.
    pub(crate) fn new() -> Self {
        Self {
            waiting: true,
            last_request: None,
        }
    }

    /// Decides for a frame captured at `timestamp`.
    pub(crate) fn admit(&mut self, keyframe: bool, timestamp: Duration) -> Admission {
        if keyframe {
            self.waiting = false;
            self.last_request = None;
            return Admission {
                decode: true,
                request_keyframe: false,
            };
        }
        if !self.waiting {
            return Admission {
                decode: true,
                request_keyframe: false,
            };
        }
        let request_keyframe = self
            .last_request
            .is_none_or(|last| timestamp < last || timestamp - last >= KEYFRAME_RETRY);
        if request_keyframe {
            self.last_request = Some(timestamp);
        }
        Admission {
            decode: false,
            request_keyframe,
        }
    }

    /// The decoder lost its references (a failure, a flush, a stop): wait for a keyframe again
    /// and ask for one at the next delta frame.
    pub(crate) fn lost(&mut self) {
        self.waiting = true;
        self.last_request = None;
    }

    /// Whether it is waiting for a keyframe.
    pub(crate) fn waiting(&self) -> bool {
        self.waiting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    const DECODE: Admission = Admission {
        decode: true,
        request_keyframe: false,
    };
    const DROP: Admission = Admission {
        decode: false,
        request_keyframe: false,
    };
    const DROP_AND_ASK: Admission = Admission {
        decode: false,
        request_keyframe: true,
    };

    #[test]
    fn requests_collapse_until_taken() {
        let requests = KeyframeRequests::default();
        let pipeline = requests.clone();
        assert!(!pipeline.take());
        requests.request();
        requests.request();
        assert!(pipeline.take());
        assert!(!pipeline.take());
        assert_eq!(pipeline.total(), 2);
    }

    #[test]
    fn delta_frames_wait_for_the_first_keyframe_and_ask_for_it() {
        let mut gate = DecodeGate::new();
        assert!(gate.waiting());
        assert_eq!(gate.admit(false, at(0)), DROP_AND_ASK);
        assert_eq!(gate.admit(true, at(33)), DECODE);
        assert!(!gate.waiting());
        assert_eq!(gate.admit(false, at(66)), DECODE);
    }

    #[test]
    fn while_waiting_it_asks_again_only_every_retry_interval() {
        let mut gate = DecodeGate::new();
        assert_eq!(gate.admit(false, at(0)), DROP_AND_ASK);
        assert_eq!(gate.admit(false, at(33)), DROP);
        assert_eq!(gate.admit(false, at(499)), DROP);
        assert_eq!(gate.admit(false, at(500)), DROP_AND_ASK);
        assert_eq!(gate.admit(false, at(533)), DROP);
        // A timestamp that jumps back (the sender restarted) asks at once.
        assert_eq!(gate.admit(false, at(10)), DROP_AND_ASK);
    }

    #[test]
    fn a_lost_decoder_asks_at_the_next_delta_frame() {
        let mut gate = DecodeGate::new();
        assert_eq!(gate.admit(true, at(0)), DECODE);
        gate.lost();
        assert!(gate.waiting());
        assert_eq!(gate.admit(false, at(33)), DROP_AND_ASK);
        assert_eq!(gate.admit(false, at(66)), DROP);
        assert_eq!(gate.admit(true, at(100)), DECODE);
        // After a keyframe, a new loss asks again straight away.
        gate.lost();
        assert_eq!(gate.admit(false, at(133)), DROP_AND_ASK);
    }

    #[test]
    fn a_keyframe_needs_no_request() {
        let mut gate = DecodeGate::new();
        gate.lost();
        assert_eq!(gate.admit(true, at(0)), DECODE);
    }
}
