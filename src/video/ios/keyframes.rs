use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Requests between two `take`s collapse into one, and the sink already spaces them out.
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
    pub(crate) fn request(&self) {
        self.inner.total.fetch_add(1, Ordering::Relaxed);
        self.inner.pending.store(true, Ordering::Relaxed);
    }

    pub fn take(&self) -> bool {
        self.inner.pending.swap(false, Ordering::Relaxed)
    }

    /// Counted before collapsing.
    pub fn total(&self) -> u64 {
        self.inner.total.load(Ordering::Relaxed)
    }
}

/// Measured in frame time.
pub(crate) const KEYFRAME_RETRY: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Admission {
    pub decode: bool,
    pub request_keyframe: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct DecodeGate {
    waiting: bool,
    last_request: Option<Duration>,
}

impl DecodeGate {
    pub(crate) fn new() -> Self {
        Self {
            waiting: true,
            last_request: None,
        }
    }

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

    pub(crate) fn lost(&mut self) {
        self.waiting = true;
        self.last_request = None;
    }

    #[cfg(test)]
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
