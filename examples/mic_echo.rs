//! Plays the microphone back through the speaker about 200 ms later, through the rings and the
//! frame layer. A manual check on a computer: use headphones, or it will howl.
//!
//! ```sh
//! cargo run --example mic_echo --features desktop
//! ```

use std::error::Error;
use std::io::BufRead;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use webrtc_engine::FRAME_SAMPLES;
use webrtc_engine::audio::desktop::DesktopBackend;
use webrtc_engine::audio::{AudioBackend, audio_io};

const DELAY: Duration = Duration::from_millis(200);
/// Room for the delay plus plenty of slack for scheduling and for the two devices' clocks.
const RING_FRAMES: usize = 32;
const FRAME: Duration = Duration::from_millis(20);

/// Whole frames of silence that make up `delay`.
fn delay_frames(delay: Duration) -> usize {
    (delay.as_millis() / FRAME.as_millis()) as usize
}

fn main() -> Result<(), Box<dyn Error>> {
    let (device, mut engine) = audio_io(RING_FRAMES);

    // The delay is silence queued ahead of the microphone's first frame.
    let silence = [0i16; FRAME_SAMPLES];
    for _ in 0..delay_frames(DELAY) {
        engine.playout.write_frame(&silence);
    }

    let mut backend = DesktopBackend::new();
    backend.start(device)?;
    println!(
        "Echoing the microphone {} ms late. Press Enter to stop.",
        DELAY.as_millis()
    );

    let stop = Arc::new(AtomicBool::new(false));
    let stop_on_enter = Arc::clone(&stop);
    std::thread::spawn(move || {
        let _ = std::io::stdin().lock().lines().next();
        stop_on_enter.store(true, Ordering::Relaxed);
    });

    let mut frame = [0i16; FRAME_SAMPLES];
    let mut frames = 0u64;
    let mut overflows = 0u64;
    while !stop.load(Ordering::Relaxed) {
        while engine.capture.read_frame(&mut frame) {
            frames += 1;
            if !engine.playout.write_frame(&frame) {
                overflows += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    backend.stop()?;

    println!(
        "{frames} frames echoed; {overflows} did not fit; {} samples dropped at capture; \
         {} playout underruns; {} stream errors",
        backend.capture_dropped().get(),
        backend.playout_underruns().get(),
        backend.stream_errors().get(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_hundred_milliseconds_are_ten_frames() {
        assert_eq!(delay_frames(Duration::from_millis(200)), 10);
    }

    #[test]
    fn a_delay_rounds_down_to_whole_frames() {
        assert_eq!(delay_frames(Duration::from_millis(59)), 2);
    }
}
