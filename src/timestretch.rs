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
    let mut stream = Stream::new(factor);
    for chunk in samples.chunks(voice_chunk()) {
        if cancelled.load(Ordering::Relaxed) {
            return None;
        }
        stream.push(chunk);
    }
    stream.close(samples.len());
    let mut out = Vec::new();
    while let Some(sample) = stream.pop() {
        if cancelled.load(Ordering::Relaxed) {
            return None;
        }
        out.push(sample);
    }
    Some(out)
}

/// Samples kept on each push. The stretcher's own state is a few frames,
/// not this chunk, and it continues across pushes.
fn voice_chunk() -> usize {
    48_000
}

/// Overlap-add that keeps only the input still in reach of the search and
/// the output overlap that later frames still add to. Pushing the signal in
/// pieces is the same run as pushing it at once: alignment and overlap
/// carry across the boundary.
pub struct Stream {
    factor: f32,
    analysis_hop: usize,
    window: Vec<f32>,
    input: Vec<f32>,
    origin: usize,
    nominal: usize,
    previous: Option<usize>,
    acc: Vec<f32>,
    weight: Vec<f32>,
    /// Absolute output index of the next sample `pop` returns.
    head: usize,
    /// Next unread index inside `acc`. Drained in blocks so a long clip
    /// does not slide the whole buffer one sample at a time.
    emit: usize,
    /// Absolute output index where the next frame is added.
    place: usize,
    closed: bool,
    total_in: usize,
    target: usize,
}

impl Stream {
    pub fn new(factor: f32) -> Self {
        let window: Vec<f32> = (0..FRAME)
            .map(|i| {
                (std::f32::consts::PI * i as f32 / FRAME as f32)
                    .sin()
                    .powi(2)
            })
            .collect();
        let analysis_hop = ((HOP as f64) * f64::from(factor)).round().max(1.0) as usize;
        Self {
            factor,
            analysis_hop,
            window,
            input: Vec::new(),
            origin: 0,
            nominal: 0,
            previous: None,
            acc: Vec::new(),
            weight: Vec::new(),
            head: 0,
            emit: 0,
            place: 0,
            closed: false,
            total_in: 0,
            target: 0,
        }
    }

    pub fn push(&mut self, chunk: &[f32]) {
        if self.closed {
            return;
        }
        self.input.extend_from_slice(chunk);
        self.pump();
    }

    pub fn close(&mut self, total_in: usize) {
        self.closed = true;
        self.total_in = total_in;
        self.target = ((total_in as f64) / f64::from(self.factor)).round() as usize;
        self.pump();
    }

    /// Input samples plus overlap still held. Playback stays near a few
    /// frames, not the length of the clip.
    pub fn retained(&self) -> usize {
        self.input.len() + self.acc.len()
    }

    pub fn pop(&mut self) -> Option<f32> {
        self.pump();
        let ready = self.ready();
        if self.head >= ready {
            return None;
        }
        let sample = self.acc[self.emit] / self.weight[self.emit].max(1e-3);
        self.emit += 1;
        self.head += 1;
        if self.emit >= 4096 {
            self.acc.drain(..self.emit);
            self.weight.drain(..self.emit);
            self.emit = 0;
        }
        Some(sample)
    }

    fn ready(&self) -> usize {
        let end = self.head - self.emit + self.acc.len();
        if !self.closed {
            return self.place.min(end);
        }
        self.target.min(end)
    }

    fn pump(&mut self) {
        if self.closed && self.total_in < FRAME * 2 {
            self.fill_short();
            return;
        }
        loop {
            let have = self.origin + self.input.len();
            let last_start = have.saturating_sub(FRAME);
            let can_search = self.nominal + FRAME + SEARCH <= have;
            let at_tail = self.closed && !can_search && have >= FRAME && self.nominal <= last_start;
            if !can_search && !at_tail {
                break;
            }
            if self.closed && self.place >= self.target.saturating_add(FRAME) {
                break;
            }
            let chosen = if can_search {
                self.previous
                    .map(|prev| {
                        let shift =
                            best_alignment_raw(&self.input, self.origin, self.nominal, prev);
                        (self.nominal as isize + shift).clamp(0, last_start as isize) as usize
                    })
                    .unwrap_or(self.nominal)
            } else {
                last_start
            };
            self.add_frame(chosen);
            self.previous = Some(chosen);
            self.place += HOP;
            if !can_search {
                break;
            }
            self.nominal += self.analysis_hop;
            self.forget();
        }
    }

    fn fill_short(&mut self) {
        if self.target == 0 || self.head > 0 {
            return;
        }
        let last = self.total_in.saturating_sub(1);
        self.acc = (0..self.target)
            .map(|i| {
                let at = (((i as f64) * f64::from(self.factor)) as usize).min(last);
                self.input.get(at).copied().unwrap_or(0.0)
            })
            .collect();
        self.weight = vec![1.0; self.acc.len()];
        self.head = 0;
        self.place = self.target;
    }

    fn add_frame(&mut self, chosen: usize) {
        let local = chosen - self.origin;
        let start = self.place - self.head + self.emit;
        let need = start + FRAME;
        if self.acc.len() < need {
            self.acc.resize(need, 0.0);
            self.weight.resize(need, 0.0);
        }
        for i in 0..FRAME {
            let sample = self.input.get(local + i).copied().unwrap_or(0.0);
            self.acc[start + i] += sample * self.window[i];
            self.weight[start + i] += self.window[i];
        }
    }

    fn forget(&mut self) {
        let keep_from = self
            .nominal
            .saturating_sub(SEARCH + FRAME)
            .min(self.previous.unwrap_or(self.nominal));
        if keep_from > self.origin {
            let drop = (keep_from - self.origin).min(self.input.len());
            self.input.drain(..drop);
            self.origin += drop;
        }
    }
}

fn best_alignment_raw(input: &[f32], origin: usize, nominal: usize, previous: usize) -> isize {
    let decimated_at = |index: usize| -> f32 {
        let start = index.saturating_sub(origin);
        if start + STEP > input.len() {
            return 0.0;
        }
        input[start..start + STEP].iter().sum::<f32>() / STEP as f32
    };
    let width = FRAME / STEP;
    let template = previous / STEP + HOP / STEP;
    let centre = nominal / STEP;
    let reach = SEARCH / STEP;
    let mut best = 0isize;
    let mut best_score = f32::NEG_INFINITY;
    for shift in -(reach as isize)..=(reach as isize) {
        let at = centre as isize + shift;
        if at < 0 {
            continue;
        }
        let mut dot = 0.0;
        let mut left = 0.0;
        let mut right = 0.0;
        let mut ok = true;
        for i in 0..width {
            let a_at = (at as usize + i) * STEP;
            let b_at = (template + i) * STEP;
            if a_at < origin || b_at < origin {
                ok = false;
                break;
            }
            let a = decimated_at(a_at);
            let b = decimated_at(b_at);
            dot += a * b;
            left += a * a;
            right += b * b;
        }
        if !ok {
            continue;
        }
        let score = dot / (left * right).sqrt().max(1e-9);
        if score > best_score {
            best_score = score;
            best = shift * STEP as isize;
        }
    }
    best
}
