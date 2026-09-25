//! Pitch-preserving speed-up, the overlap-add used by ZapFast 0.16.2.
//!
//! Frames overlap by three quarters under a sine-squared window. The sink
//! then plays the result at 1x, so the pitch stays put.

use std::sync::atomic::{AtomicBool, Ordering};

const FRAME: usize = 2048;
const HOP: usize = FRAME / 4;
const SEARCH: usize = 448;
const STEP: usize = 8;

/// Compresses mono samples by `factor` (above 1) without changing the pitch.
pub fn speed_up(samples: &[f32], factor: f32) -> Vec<f32> {
    speed_up_unless(samples, factor, &AtomicBool::new(false)).expect("never cancelled")
}

/// Like [`speed_up`], but stops when `cancelled` is set.
pub fn speed_up_unless(samples: &[f32], factor: f32, cancelled: &AtomicBool) -> Option<Vec<f32>> {
    if !(factor.is_finite() && factor > 1.0) {
        return Some(samples.to_vec());
    }
    let target = ((samples.len() as f64) / f64::from(factor)).round() as usize;
    if samples.len() < FRAME * 2 {
        let last = samples.len().saturating_sub(1);
        return Some(
            (0..target)
                .map(|i| samples[(((i as f64) * f64::from(factor)) as usize).min(last)])
                .collect(),
        );
    }
    let decimated: Vec<f32> = (0..samples.len() / STEP)
        .map(|i| {
            let start = i * STEP;
            samples[start..start + STEP].iter().sum::<f32>() / STEP as f32
        })
        .collect();
    let window: Vec<f32> = (0..FRAME)
        .map(|i| {
            (std::f32::consts::PI * i as f32 / FRAME as f32)
                .sin()
                .powi(2)
        })
        .collect();
    let analysis_hop = ((HOP as f64) * f64::from(factor)).round() as usize;
    let mut out = vec![0.0f32; target + FRAME];
    let mut weight = vec![0.0f32; target + FRAME];
    let mut nominal = 0usize;
    let mut written = 0usize;
    let mut previous: Option<usize> = None;
    while written + FRAME <= out.len() {
        if cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let last_start = samples.len() - FRAME;
        let chosen = if nominal + FRAME + SEARCH <= samples.len() {
            previous
                .map(|prev| {
                    let shift = best_alignment(&decimated, nominal, prev);
                    (nominal as isize + shift).clamp(0, last_start as isize) as usize
                })
                .unwrap_or(nominal)
        } else {
            last_start
        };
        for i in 0..FRAME {
            out[written + i] += samples[chosen + i] * window[i];
            weight[written + i] += window[i];
        }
        previous = Some(chosen);
        written += HOP;
        if chosen == last_start {
            break;
        }
        nominal += analysis_hop;
    }
    for i in 0..out.len() {
        out[i] /= weight[i].max(1e-3);
    }
    out.truncate(target);
    Some(out)
}

fn best_alignment(decimated: &[f32], nominal: usize, previous: usize) -> isize {
    let width = FRAME / STEP;
    let template = previous / STEP + HOP / STEP;
    let centre = nominal / STEP;
    let reach = SEARCH / STEP;
    if template + width > decimated.len()
        || centre < reach
        || centre + reach + width > decimated.len()
    {
        return 0;
    }
    let mut best = 0isize;
    let mut best_score = f32::NEG_INFINITY;
    for shift in -(reach as isize)..=(reach as isize) {
        let at = (centre as isize + shift) as usize;
        let mut dot = 0.0;
        let mut left = 0.0;
        let mut right = 0.0;
        for i in 0..width {
            let a = decimated[at + i];
            let b = decimated[template + i];
            dot += a * b;
            left += a * a;
            right += b * b;
        }
        let score = dot / (left * right).sqrt().max(1e-9);
        if score > best_score {
            best_score = score;
            best = shift * STEP as isize;
        }
    }
    best
}
