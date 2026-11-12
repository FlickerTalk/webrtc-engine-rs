#![allow(dead_code)]

use std::f64::consts::TAU;

use webrtc_engine::SAMPLE_RATE;

pub fn test_signal(samples: usize) -> Vec<i16> {
    let rate = f64::from(SAMPLE_RATE);
    let mut phase = 0.0f64;
    (0..samples)
        .map(|n| {
            let t = n as f64 / rate;
            let glide = 350.0 + 120.0 * (TAU * 0.9 * t).sin();
            phase += TAU * glide / rate;
            let level = 0.25 * phase.sin() + 0.1 * (TAU * 1_100.0 * t).sin();
            (level * f64::from(i16::MAX)) as i16
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub struct Alignment {
    pub correlation: f64,
    pub lag: usize,
}

fn correlation_at(reference: &[i16], output: &[i16], lag: usize, step: usize) -> f64 {
    let Some(shifted) = output.get(lag..lag + reference.len()) else {
        return 0.0;
    };
    let (mut dot, mut ref_energy, mut out_energy) = (0.0, 0.0, 0.0);
    for (a, b) in reference.iter().zip(shifted).step_by(step) {
        let (a, b) = (f64::from(*a), f64::from(*b));
        dot += a * b;
        ref_energy += a * a;
        out_energy += b * b;
    }
    if ref_energy == 0.0 || out_energy == 0.0 {
        return 0.0;
    }
    dot / (ref_energy * out_energy).sqrt()
}

pub fn align(
    input: &[i16],
    output: &[i16],
    start: usize,
    window: usize,
    max_lag: usize,
) -> Alignment {
    const COARSE: usize = 8;
    let reference = &input[start..start + window];
    let tail = &output[start.min(output.len())..];
    let coarse = (0..=max_lag)
        .step_by(COARSE)
        .map(|lag| (correlation_at(reference, tail, lag, COARSE), lag))
        .fold(
            (f64::MIN, 0),
            |best, next| if next.0 > best.0 { next } else { best },
        );
    let (correlation, lag) = (coarse.1.saturating_sub(COARSE)..=coarse.1 + COARSE)
        .map(|lag| (correlation_at(reference, tail, lag, 1), lag))
        .fold(
            (f64::MIN, 0),
            |best, next| if next.0 > best.0 { next } else { best },
        );
    Alignment { correlation, lag }
}

pub fn align_windows(
    input: &[i16],
    output: &[i16],
    start: usize,
    window: usize,
    count: usize,
    max_lag: usize,
) -> Vec<Alignment> {
    (0..count)
        .map(|index| align(input, output, start + index * window, window, max_lag))
        .collect()
}

pub fn mean_correlation(alignments: &[Alignment]) -> f64 {
    alignments.iter().map(|a| a.correlation).sum::<f64>() / alignments.len() as f64
}
