//! Hear the call pipeline on a computer: the microphone goes through Opus, a simulated network,
//! the jitter buffer and Opus again to the speaker, as the other side of a call would hear you.
//! Use headphones, or the speaker feeds back into the microphone.
//!
//! ```sh
//! cargo run --release --example call_demo --features desktop -- --loss 10 --jitter 40 --delay 50
//! ```

use std::error::Error;
use std::time::Duration;

use webrtc_engine::audio::AudioBackend;
use webrtc_engine::audio::audio_io;
use webrtc_engine::audio::desktop::DesktopBackend;
use webrtc_engine::call::{Call, CallConfig, CallStats};
use webrtc_engine::netsim::{Conditions, simulated_link};

const USAGE: &str = "usage: call_demo [--loss PERCENT] [--jitter MS] [--delay MS] [--seconds S]";

/// Room in each ring for scheduling hiccups; the playout queue itself stays at two frames.
const RING_FRAMES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Options {
    loss_percent: f64,
    jitter_ms: u64,
    delay_ms: u64,
    seconds: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            loss_percent: 0.0,
            jitter_ms: 0,
            delay_ms: 40,
            seconds: 30,
        }
    }
}

impl Options {
    fn conditions(&self) -> Conditions {
        Conditions {
            delay: Duration::from_millis(self.delay_ms),
            jitter: Duration::from_millis(self.jitter_ms),
            loss: self.loss_percent / 100.0,
            ..Conditions::default()
        }
    }
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        let bad = || format!("{flag}: {value:?} is not a number");
        match flag.as_str() {
            "--loss" => {
                options.loss_percent = value.parse().map_err(|_| bad())?;
                if !(0.0..=100.0).contains(&options.loss_percent) {
                    return Err("--loss goes from 0 to 100".to_owned());
                }
            }
            "--jitter" => options.jitter_ms = value.parse().map_err(|_| bad())?,
            "--delay" => options.delay_ms = value.parse().map_err(|_| bad())?,
            "--seconds" => options.seconds = value.parse().map_err(|_| bad())?,
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    Ok(options)
}

fn stats_line(second: u64, stats: &CallStats, device_underruns: u64) -> String {
    let receive = &stats.receive;
    format!(
        "{second:>3}s  received {} | concealed {} | FEC {} | depth {}/{} frames | \
         jitter {:.1} ms | underruns {} (device {})",
        receive.received,
        receive.concealed,
        receive.recovered,
        receive.depth,
        receive.target,
        receive.jitter.as_secs_f64() * 1000.0,
        receive.underruns,
        device_underruns,
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let options = parse(std::env::args().skip(1)).map_err(|error| format!("{error}\n{USAGE}"))?;
    let (device, engine) = audio_io(RING_FRAMES);
    let (sender, receiver) = simulated_link(options.conditions(), 1);

    let mut backend = DesktopBackend::new();
    backend.start(device)?;
    let call = Call::start(engine, sender, receiver, CallConfig::default())?;
    println!(
        "Microphone -> Opus -> {} ms delay, {} ms jitter, {} % loss -> jitter buffer -> speaker, \
         for {} s. Use headphones.",
        options.delay_ms, options.jitter_ms, options.loss_percent, options.seconds
    );

    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.tick().await;
    for second in 1..=options.seconds {
        ticker.tick().await;
        println!(
            "{}",
            stats_line(second, &call.stats(), backend.playout_underruns().get())
        );
    }

    // The device first: its callbacks read the rings the call feeds.
    backend.stop()?;
    let stats = call.stop().await;
    println!("final: {stats:?}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn without_flags_it_is_a_clean_network() {
        assert_eq!(parse(args("")), Ok(Options::default()));
        assert_eq!(Options::default().conditions().loss, 0.0);
    }

    #[test]
    fn reads_every_flag() {
        let options = parse(args("--loss 10 --jitter 40 --delay 80 --seconds 5")).unwrap();
        assert_eq!(
            options,
            Options {
                loss_percent: 10.0,
                jitter_ms: 40,
                delay_ms: 80,
                seconds: 5,
            }
        );
        let conditions = options.conditions();
        assert_eq!(conditions.loss, 0.1);
        assert_eq!(conditions.jitter, Duration::from_millis(40));
        assert_eq!(conditions.delay, Duration::from_millis(80));
    }

    #[test]
    fn rejects_what_it_does_not_understand() {
        assert!(parse(args("--loss")).is_err());
        assert!(parse(args("--loss lots")).is_err());
        assert!(parse(args("--loss 150")).is_err());
        assert!(parse(args("--volume 11")).is_err());
    }

    #[test]
    fn the_stats_line_shows_what_the_listener_should_watch() {
        let mut stats = CallStats::default();
        stats.receive.received = 48;
        stats.receive.concealed = 2;
        stats.receive.recovered = 3;
        stats.receive.depth = 2;
        stats.receive.target = 3;
        stats.receive.underruns = 1;
        let line = stats_line(4, &stats, 7);
        for expected in [
            "4s",
            "received 48",
            "concealed 2",
            "FEC 3",
            "depth 2/3",
            "underruns 1",
            "device 7",
        ] {
            assert!(line.contains(expected), "{expected:?} missing in {line:?}");
        }
    }
}
