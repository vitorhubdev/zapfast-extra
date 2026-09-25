//! Audio playback and voice-message recording.
//!
//! Input and output devices are opened on demand and released when idle.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rodio::Source;

use crate::backend::Waker;
use crate::voice;

/// Maximum recording length. The phone uses a shorter limit.
const LONGEST_RECORDING: Duration = Duration::from_secs(15 * 60);
/// Raw samples pulled from a decoder per spool block. Small enough to keep
/// long clips out of memory while decoding.
const SPOOL_BLOCK: usize = 65_536;
/// Mono frames held while resampling spooled audio.
const RESAMPLE_BLOCK: usize = 32_768;
/// Samples reported per span so the output re-reads the playback rate.
///
/// Rodio rebuilds its rate converter at span boundaries. With an unbounded
/// span a speed change would never reach the samples already queued, so the
/// button would move while the voice stays at 1x. Four thousand samples are
/// about 85 ms at 48 kHz, inside the 150 ms budget for an audible change.
const SPAN_SAMPLES: usize = 4_096;

fn mono() -> NonZero<u16> {
    NonZero::<u16>::MIN
}

fn rate() -> NonZero<u32> {
    NonZero::new(voice::RATE).expect("48 kHz is not zero")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Loading,
    Playing,
    Paused,
}

/// Playback state for one message.
#[derive(Clone, Copy, Debug)]
pub struct Status {
    pub state: State,
    pub position: Duration,
    pub total: Duration,
}

impl Status {
    const IDLE: Self = Self {
        state: State::Idle,
        position: Duration::ZERO,
        total: Duration::ZERO,
    };
}

/// Finished decode: small clips in memory, long files spooled to disk.
enum DecodedClip {
    Memory(Vec<f32>),
    File(SpooledClip),
}

/// Mono 48 kHz PCM spooled to disk as little-endian f32, with its length.
struct SpooledClip {
    path: PathBuf,
    frames: u64,
}

type Decoded = Arc<Mutex<Option<Result<DecodedClip, String>>>>;

/// Playback speeds the audio button walks through, slowest first.
pub const SPEEDS: [f32; 3] = [1.0, 1.5, 2.0];

/// Snaps any number to a supported playback speed.
pub fn snap_speed(speed: f32) -> f32 {
    if !speed.is_finite() {
        return 1.0;
    }
    SPEEDS
        .iter()
        .copied()
        .min_by(|a, b| (a - speed).abs().total_cmp(&(b - speed).abs()))
        .unwrap_or(1.0)
}

/// The next playback speed, wrapping around at the fastest one.
pub fn next_speed(speed: f32) -> f32 {
    let snapped = snap_speed(speed);
    let index = SPEEDS
        .iter()
        .position(|value| (*value - snapped).abs() < f32::EPSILON)
        .unwrap_or(0);
    SPEEDS[(index + 1) % SPEEDS.len()]
}

/// Plays one clip at a time through the default output device.
pub struct Player {
    waker: Waker,
    output: Option<(rodio::MixerDeviceSink, rodio::Player)>,
    loaded: Option<Loaded>,
    decoding: Option<Decoding>,
    /// Generated waveforms for clips that did not include one.
    bars: HashMap<String, Vec<u8>>,
    /// Playback speed chosen for one clip, by message id.
    speeds: HashMap<String, f32>,
    /// The message that finished playing, reported once for autoplay.
    finished: Option<String>,
}

struct Loaded {
    message: String,
    clip: Clip,
    /// Full clip length, kept after the audio data is released.
    total: Duration,
    /// Start position of the queued audio after seeking.
    base: Duration,
    /// Output time when the base was established. Speeds only price output
    /// after their own instant, so switching mid-clip rebases here instead
    /// of repricing the stretch already heard.
    base_sink: Duration,
    paused: bool,
    done: bool,
    /// Original file, to decode again after the data was released.
    path: PathBuf,
}

/// Where the playable audio lives.
enum Clip {
    /// Small clip shared with the player without copying.
    Memory { samples: Arc<Vec<f32>> },
    /// Long clip streamed from its spool file.
    File { spool: PathBuf, frames: u64 },
    /// Released after playback ended; replaying decodes the original again.
    Released,
}

struct Decoding {
    message: String,
    /// Requested start position after decoding, from 0 to 1.
    start: f32,
    path: PathBuf,
    slot: Decoded,
}

/// Plays the tail of shared samples without copying them.
#[derive(Clone)]
struct SharedSamples {
    samples: Arc<Vec<f32>>,
    pos: usize,
    total: Duration,
}

impl SharedSamples {
    fn new(samples: Arc<Vec<f32>>, offset: usize) -> Self {
        let offset = offset.min(samples.len());
        Self {
            total: clip_length(samples.len() - offset),
            samples,
            pos: offset,
        }
    }
}

impl Iterator for SharedSamples {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let sample = *self.samples.get(self.pos)?;
        self.pos += 1;
        Some(sample)
    }
}

impl Source for SharedSamples {
    fn current_span_len(&self) -> Option<usize> {
        Some((self.samples.len() - self.pos).clamp(1, SPAN_SAMPLES))
    }

    fn channels(&self) -> NonZero<u16> {
        mono()
    }

    fn sample_rate(&self) -> NonZero<u32> {
        rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        Some(self.total)
    }
}

/// Streams spooled mono 48 kHz samples from disk.
struct FileSamples {
    reader: BufReader<File>,
    remaining: u64,
    total: Duration,
}

impl FileSamples {
    fn open(spool: &Path, skip: u64, frames: u64) -> Result<Self, String> {
        let mut reader = BufReader::new(
            File::open(spool).map_err(|error| format!("Could not play the audio: {error}"))?,
        );
        let skip = skip.min(frames);
        reader
            .seek(SeekFrom::Start(skip * 4))
            .map_err(|error| format!("Could not play the audio: {error}"))?;
        let remaining = frames - skip;
        Ok(Self {
            total: clip_length(remaining.min(usize::MAX as u64) as usize),
            reader,
            remaining,
        })
    }
}

impl Iterator for FileSamples {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.remaining == 0 {
            return None;
        }
        let mut bytes = [0u8; 4];
        self.reader.read_exact(&mut bytes).ok()?;
        self.remaining -= 1;
        Some(f32::from_le_bytes(bytes))
    }
}

impl Source for FileSamples {
    fn current_span_len(&self) -> Option<usize> {
        Some(self.remaining.clamp(1, SPAN_SAMPLES as u64) as usize)
    }

    fn channels(&self) -> NonZero<u16> {
        mono()
    }

    fn sample_rate(&self) -> NonZero<u32> {
        rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        Some(self.total)
    }
}

fn append_stretched<S>(sink: &rodio::Player, samples: S, speed: f32, total_in: u64)
where
    S: Iterator<Item = f32> + Send + 'static,
{
    sink.append(OverlapPlay::new(samples, speed, total_in));
}

/// Plays overlap-add from the incoming samples. The stretcher keeps a few
/// frames, and the same alignment continues for the whole clip.
struct OverlapPlay<S> {
    inner: S,
    stream: crate::timestretch::Stream,
    queued: Vec<f32>,
    pos: usize,
    fed: u64,
    total_in: u64,
    closed: bool,
    emitted: u64,
    expected: u64,
}

impl<S: Iterator<Item = f32>> OverlapPlay<S> {
    fn new(inner: S, speed: f32, total_in: u64) -> Self {
        Self {
            inner,
            stream: crate::timestretch::Stream::new(speed),
            queued: Vec::new(),
            pos: 0,
            fed: 0,
            total_in,
            closed: false,
            emitted: 0,
            expected: (total_in as f64 / f64::from(speed.max(1.0))).round() as u64,
        }
    }

    fn fill(&mut self) {
        if self.pos < self.queued.len() {
            return;
        }
        self.queued.clear();
        self.pos = 0;
        while self.queued.is_empty() {
            if let Some(sample) = self.stream.pop() {
                self.queued.push(sample);
                while self.queued.len() < 512 {
                    match self.stream.pop() {
                        Some(sample) => self.queued.push(sample),
                        None => break,
                    }
                }
                break;
            }
            if self.closed {
                break;
            }
            let mut chunk = Vec::with_capacity(4096);
            while chunk.len() < 4096 && self.fed < self.total_in {
                match self.inner.next() {
                    Some(sample) => {
                        chunk.push(sample);
                        self.fed += 1;
                    }
                    None => break,
                }
            }
            if chunk.is_empty() || self.fed >= self.total_in {
                self.stream.close(self.total_in as usize);
                self.closed = true;
            }
            if !chunk.is_empty() {
                self.stream.push(&chunk);
            }
        }
    }
}

impl<S: Iterator<Item = f32>> Iterator for OverlapPlay<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        self.fill();
        if self.pos >= self.queued.len() {
            return None;
        }
        let sample = self.queued[self.pos];
        self.pos += 1;
        self.emitted += 1;
        Some(sample)
    }
}

impl<S: Iterator<Item = f32> + Send> Source for OverlapPlay<S> {
    fn current_span_len(&self) -> Option<usize> {
        Some(
            self.expected
                .saturating_sub(self.emitted)
                .clamp(1, SPAN_SAMPLES as u64) as usize,
        )
    }

    fn channels(&self) -> NonZero<u16> {
        mono()
    }

    fn sample_rate(&self) -> NonZero<u32> {
        rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        Some(clip_length(self.expected as usize))
    }
}

/// Window, synthesis hop, search radius and search stride of the
/// streaming time-stretch, in samples at 48 kHz. One synthesis step
/// emits one hop after scanning a few dozen candidates; all state stays
/// within a few windows no matter how long the clip is.
const STRETCH_WINDOW: usize = 2048;
const STRETCH_HOP: usize = 512;
const STRETCH_SEARCH: usize = 256;
const STRETCH_STEP: usize = 8;
/// Inputs shorter than two hops bypass the stretch and play unchanged:
/// periodicity is meaningless below one window, so identity beats any
/// resampling there. Bounded prebuffer, read once at construction.
const STRETCH_PASSTHROUGH: usize = 2 * STRETCH_HOP;
/// Output length promise: within one window plus one hop of input over
/// speed (2560 samples, about 53 ms at 48 kHz). The first frame has no
/// tail to blend with and the tail flush pads one partial frame at most.
/// Time-stretch preserving pitch for mono 48 kHz samples: emitted audio
/// is made of the original samples, so periodicity survives while tempo
/// follows speed. The sink always runs at 1x; only this source shortens
/// the stream. Both player inputs are mono 48 kHz by construction, so one
/// channel-free implementation covers memory and spool sources alike.
struct Stretched<S> {
    inner: S,
    hop_in: usize,
    /// Lookahead with absolute index origin: buf[0] is input `consumed`.
    buf: Vec<f32>,
    consumed: u64,
    /// Absolute input index of the next analysis frame.
    next_in: u64,
    /// Last emitted hop: the next junction blends against it.
    last_out: [f32; STRETCH_HOP],
    has_last: bool,
    /// Input hop after the previous analysis start: the next frame must
    /// continue this waveform, so similarity runs against it rather than
    /// against the blended output behind it.
    prev_cont: [f32; STRETCH_HOP],
    /// Pending emission, capped well below one hop of backlog per pull.
    out: Vec<f32>,
    out_pos: usize,
    exhausted: bool,
    emitted: u64,
    expected_out: u64,
    /// Short input: drain the prebuffer unchanged and finish.
    passthrough: bool,
}

impl<S: Iterator<Item = f32>> Stretched<S> {
    fn new(mut inner: S, speed: f32, total_in: u64) -> Self {
        debug_assert!(speed > 1.0);
        let hop_in = ((STRETCH_HOP as f32) * speed).round().max(1.0) as usize;
        let mut buf = Vec::with_capacity(STRETCH_WINDOW + hop_in + 2 * STRETCH_SEARCH);
        let mut ended = false;
        while buf.len() < STRETCH_PASSTHROUGH {
            match inner.next() {
                Some(sample) => buf.push(sample),
                None => {
                    ended = true;
                    break;
                }
            }
        }
        let passthrough = ended;
        let expected_out = if passthrough {
            buf.len() as u64
        } else {
            (total_in as f64 / speed.max(1.0) as f64) as u64
        };
        Self {
            inner,
            hop_in,
            buf,
            consumed: 0,
            next_in: 0,
            last_out: [0.0; STRETCH_HOP],
            has_last: false,
            prev_cont: [0.0; STRETCH_HOP],
            out: Vec::with_capacity(2 * STRETCH_HOP),
            out_pos: 0,
            exhausted: ended,
            emitted: 0,
            expected_out,
            passthrough,
        }
    }

    /// Absolute energy of a slice: silence skips the search entirely.
    fn energy(values: &[f32]) -> f32 {
        values.iter().map(|sample| sample.abs()).sum()
    }

    /// Best local offset around the expected analysis start, keeping
    /// waveform continuity where the new hop joins the old audio.
    fn best_offset(&self, rel: usize) -> isize {
        // The frame must continue the input hop after the previous
        // analysis start, not the blended output behind it: comparing
        // against the output tail picks phase matches that skip the
        // wrong amount of input and imprint the hop rate on the tone.
        if !self.has_last || self.buf.len() < STRETCH_HOP || Self::energy(&self.prev_cont) < 1e-6 {
            return 0;
        }
        let rel = rel.min(self.buf.len() - STRETCH_HOP);
        let mut best = 0isize;
        let mut best_cost = f32::INFINITY;
        let mut delta = -(STRETCH_SEARCH as isize);
        while delta <= STRETCH_SEARCH as isize {
            let at = rel
                .saturating_add_signed(delta)
                .min(self.buf.len() - STRETCH_HOP);
            let mut cost = 0.0;
            for (prev, new) in self
                .prev_cont
                .iter()
                .zip(self.buf[at..at + STRETCH_HOP].iter())
            {
                cost += (prev - new).abs();
            }
            if cost < best_cost {
                best_cost = cost;
                best = delta;
            }
            delta += STRETCH_STEP as isize;
        }
        best
    }

    /// One synthesis step: align, blend one hop, advance the pointer.
    /// Returns false when the input is spent.
    fn step(&mut self) -> bool {
        if self.passthrough {
            return false;
        }
        let rel = (self.next_in - self.consumed) as usize;
        // Keep the search window plus one frame buffered.
        while self.buf.len() < rel + STRETCH_WINDOW + STRETCH_SEARCH {
            match self.inner.next() {
                Some(sample) => self.buf.push(sample),
                None => {
                    self.exhausted = true;
                    break;
                }
            }
        }
        if rel >= self.buf.len() {
            return false;
        }
        let delta = self.best_offset(rel);
        // Pad the tail with silence instead of dropping it.
        let start = rel.saturating_add_signed(delta);
        let mut aligned = [0.0f32; STRETCH_WINDOW];
        let have = (self.buf.len() - start.min(self.buf.len())).min(STRETCH_WINDOW);
        aligned[..have].copy_from_slice(&self.buf[start.min(self.buf.len())..][..have]);
        if self.has_last {
            for (index, (prev, new)) in self.last_out.iter().zip(aligned.iter()).enumerate() {
                let t = (index + 1) as f32 / STRETCH_HOP as f32;
                self.out.push(prev * (1.0 - t) + new * t);
            }
        } else {
            self.out.extend_from_slice(&aligned[..STRETCH_HOP]);
        }
        self.last_out.copy_from_slice(&aligned[..STRETCH_HOP]);
        self.has_last = true;
        // Remember the true continuation for the next search: the input
        // hop right after the 512 samples this step just consumed.
        let cont = start.saturating_add(STRETCH_HOP).min(self.buf.len());
        let have_cont = (self.buf.len() - cont).min(STRETCH_HOP);
        self.prev_cont[..have_cont].copy_from_slice(&self.buf[cont..cont + have_cont]);
        self.prev_cont[have_cont..].fill(0.0);
        self.next_in += self.hop_in as u64;
        // Forget everything the next search cannot reach.
        let floor = self.next_in.saturating_sub(STRETCH_SEARCH as u64 + 1);
        if floor > self.consumed {
            let drop = (floor - self.consumed).min(self.buf.len() as u64) as usize;
            self.buf.drain(..drop);
            self.consumed += drop as u64;
        }
        true
    }
}

impl<S: Iterator<Item = f32>> Iterator for Stretched<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.passthrough {
            // Short input drains unchanged; out_pos doubles as cursor.
            if self.out_pos < self.buf.len() {
                let sample = self.buf[self.out_pos];
                self.out_pos += 1;
                self.emitted += 1;
                return Some(sample);
            }
            return None;
        }
        if self.out_pos >= self.out.len() {
            self.out.clear();
            self.out_pos = 0;
            if !self.step() {
                return None;
            }
        }
        let sample = self.out[self.out_pos];
        self.out_pos += 1;
        self.emitted += 1;
        Some(sample)
    }
}

impl<S: Iterator<Item = f32> + Send> Source for Stretched<S> {
    fn current_span_len(&self) -> Option<usize> {
        // Spans only re-chunk the stream now that the sink never
        // resamples: report what is actually queued or due.
        if self.passthrough {
            return Some(
                self.buf
                    .len()
                    .saturating_sub(self.out_pos)
                    .clamp(1, SPAN_SAMPLES),
            );
        }
        let queued = self.out.len().saturating_sub(self.out_pos);
        if queued > 0 {
            return Some(queued.min(SPAN_SAMPLES));
        }
        Some(
            self.expected_out
                .saturating_sub(self.emitted)
                .clamp(1, SPAN_SAMPLES as u64) as usize,
        )
    }

    fn channels(&self) -> NonZero<u16> {
        mono()
    }

    fn sample_rate(&self) -> NonZero<u32> {
        rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        Some(clip_length(
            self.expected_out.min(usize::MAX as u64) as usize
        ))
    }
}

impl Player {
    pub fn new(waker: Waker) -> Self {
        Self {
            waker,
            output: None,
            loaded: None,
            decoding: None,
            bars: HashMap::new(),
            speeds: HashMap::new(),
            finished: None,
        }
    }

    /// Sets the speed of one clip and applies it right away.
    ///
    /// Faster speeds compress time while keeping the pitch: the sink
    /// always runs at 1x and a streaming time-stretch shortens the source
    /// instead of resampling it. The change reaches the clip that is
    /// playing at the moment it is made, from where it is on the original
    /// timeline; a paused clip stays paused.
    pub fn set_speed(&mut self, message: &str, speed: f32) {
        let speed = snap_speed(speed);
        let old = self.speed_of(message);
        if speed == old {
            return;
        }
        // Rebase the marker first: the new speed only prices output after
        // this instant, so switching mid-clip neither jumps ahead nor rewinds.
        if let Some(loaded) = self.loaded.as_mut()
            && loaded.message == message
            && !loaded.done
            && let Some((_, sink)) = &self.output
        {
            let total = loaded.total;
            loaded.base = speed_position(loaded.base, sink.get_pos(), loaded.base_sink, old, total);
            loaded.base_sink = sink.get_pos();
        }
        self.speeds.insert(message.to_owned(), speed);
        // Rebuild the playing source at the same original-timeline
        // position: tempo comes from the new source while the sink stays
        // at 1x, and the cleared queue drops the stale speed with it.
        let resume = match &self.loaded {
            Some(loaded) if loaded.message == message && !loaded.done => {
                let total = loaded.total;
                let paused = loaded.paused;
                match &self.output {
                    Some((_, sink)) => {
                        let position = speed_position(
                            loaded.base,
                            sink.get_pos(),
                            loaded.base_sink,
                            old,
                            total,
                        );
                        Some((position, paused, total))
                    }
                    None => None,
                }
            }
            _ => None,
        };
        if let Some((position, paused, total)) = resume {
            let fraction = if total.is_zero() {
                0.0
            } else {
                (position.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 1.0) as f32
            };
            let _ = self.restart(fraction);
            if paused {
                if let Some((_, sink)) = &self.output {
                    sink.pause();
                }
                if let Some(loaded) = self.loaded.as_mut() {
                    loaded.paused = true;
                }
            }
        }
    }

    /// The speed chosen for one clip, 1x until the reader says otherwise.
    pub fn speed_of(&self, message: &str) -> f32 {
        self.speeds.get(message).copied().unwrap_or(1.0)
    }

    /// The message that just reached its end, reported once.
    pub fn take_finished(&mut self) -> Option<String> {
        self.finished.take()
    }

    /// The message with sound held right now, loaded or still decoding,
    /// whether playing or paused. Deleting it must stop the player; any
    /// other message keeps playing.
    pub fn playing_message(&self) -> Option<&str> {
        self.loaded
            .as_ref()
            .map(|loaded| loaded.message.as_str())
            .or_else(|| self.decoding.as_ref().map(|job| job.message.as_str()))
    }

    /// Plays or pauses a message. Finished clips decode again; new clips decode first.
    pub fn toggle(&mut self, message: &str, path: &Path) -> Result<(), String> {
        match self.loaded.as_mut() {
            Some(loaded) if loaded.message == message && !loaded.done => {
                if let Some((_, sink)) = &self.output {
                    if loaded.paused {
                        sink.play();
                    } else {
                        sink.pause();
                    }
                    loaded.paused = !loaded.paused;
                }
                Ok(())
            }
            _ => self.load(message, path, 0.0),
        }
    }

    /// Seeks to a fraction from 0 to 1 and starts playback.
    pub fn seek(&mut self, message: &str, path: &Path, fraction: f32) -> Result<(), String> {
        match &self.loaded {
            Some(loaded) if loaded.message == message && !loaded.done => self.restart(fraction),
            _ => self.load(message, path, fraction),
        }
    }

    /// Clears the loaded clip and releases the output device.
    pub fn stop(&mut self) {
        self.release_spool();
        self.output = None;
        self.loaded = None;
        self.decoding = None;
        self.finished = None;
    }

    /// Deletes the spool file behind the loaded clip, if any.
    fn release_spool(&mut self) {
        if let Some(loaded) = self.loaded.as_ref()
            && let Clip::File { spool, .. } = &loaded.clip
        {
            let _ = std::fs::remove_file(spool);
        }
    }

    /// Whether audio is currently playing.
    pub fn is_playing(&self) -> bool {
        self.decoding.is_some()
            || self
                .loaded
                .as_ref()
                .is_some_and(|loaded| !loaded.paused && !loaded.done)
    }

    pub fn status(&self, message: &str) -> Status {
        if let Some(decoding) = &self.decoding
            && decoding.message == message
        {
            return Status {
                state: State::Loading,
                ..Status::IDLE
            };
        }
        match &self.loaded {
            Some(loaded) if loaded.message == message => {
                let total = loaded.total;
                if loaded.done {
                    return Status {
                        state: State::Idle,
                        position: Duration::ZERO,
                        total,
                    };
                }
                let speed = self.speed_of(message);
                let position = self
                    .output
                    .as_ref()
                    .map(|(_, sink)| {
                        speed_position(loaded.base, sink.get_pos(), loaded.base_sink, speed, total)
                    })
                    .unwrap_or(loaded.base)
                    .min(total);
                Status {
                    state: if loaded.paused {
                        State::Paused
                    } else {
                        State::Playing
                    },
                    position,
                    total,
                }
            }
            _ => Status::IDLE,
        }
    }

    /// Generated waveform for a decoded clip.
    pub fn bars(&self, message: &str) -> Option<&[u8]> {
        self.bars.get(message).map(Vec::as_slice)
    }

    /// Handles completed decodes and finished playback once per frame.
    pub fn poll(&mut self) -> Result<(), String> {
        let decoded = self.decoding.as_ref().and_then(|decoding| {
            decoding
                .slot
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
        });
        if let Some(result) = decoded {
            let Decoding {
                message,
                start,
                path,
                ..
            } = self.decoding.take().expect("just seen");
            let (clip, total) = match result? {
                DecodedClip::Memory(samples) => {
                    if samples.is_empty() {
                        return Err("The clip is empty".to_owned());
                    }
                    let total = clip_length(samples.len());
                    self.bars
                        .entry(message.clone())
                        .or_insert_with(|| voice::waveform(&samples));
                    (
                        Clip::Memory {
                            samples: Arc::new(samples),
                        },
                        total,
                    )
                }
                DecodedClip::File(spool) => {
                    if spool.frames == 0 {
                        let _ = std::fs::remove_file(&spool.path);
                        return Err("The clip is empty".to_owned());
                    }
                    let total = clip_length(spool.frames.min(usize::MAX as u64) as usize);
                    let bars = waveform_from_spool(&spool.path, spool.frames)
                        .unwrap_or_else(|_| vec![0; voice::BARS]);
                    self.bars.entry(message.clone()).or_insert(bars);
                    (
                        Clip::File {
                            spool: spool.path,
                            frames: spool.frames,
                        },
                        total,
                    )
                }
            };
            self.loaded = Some(Loaded {
                message,
                clip,
                total,
                base: Duration::ZERO,
                base_sink: Duration::ZERO,
                paused: false,
                done: false,
                path,
            });
            self.restart(start)?;
        }
        let ended = match (&mut self.loaded, &self.output) {
            (Some(loaded), Some((_, sink))) if !loaded.done && !loaded.paused && sink.empty() => {
                loaded.done = true;
                true
            }
            _ => false,
        };
        if ended {
            // Release the device and the audio data after playback ends. Only
            // the waveform and the total length stay behind; replaying decodes
            // the original file again instead of holding every sample.
            if let Some(loaded) = self.loaded.as_mut() {
                if let Clip::File { spool, .. } = &loaded.clip {
                    let _ = std::fs::remove_file(spool);
                }
                self.finished = Some(loaded.message.clone());
                loaded.clip = Clip::Released;
            }
            self.output = None;
        }
        Ok(())
    }

    fn load(&mut self, message: &str, path: &Path, start: f32) -> Result<(), String> {
        self.stop();
        let slot: Decoded = Default::default();
        let file = path.to_owned();
        let waker = self.waker.clone();
        let thread_slot = Arc::clone(&slot);
        let spawned = std::thread::Builder::new()
            .name("voice-decode".to_owned())
            .spawn(move || {
                let result = decode_file(&file);
                *thread_slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
                waker.wake();
            });
        if let Err(error) = spawned {
            return Err(format!("Could not decode audio: {error}"));
        }
        self.decoding = Some(Decoding {
            message: message.to_owned(),
            start,
            path: path.to_owned(),
            slot,
        });
        Ok(())
    }

    /// Plays the loaded clip from a fraction from 0 to 1.
    fn restart(&mut self, fraction: f32) -> Result<(), String> {
        enum Start {
            Memory {
                samples: Arc<Vec<f32>>,
                offset: usize,
            },
            File {
                spool: PathBuf,
                skip: u64,
                frames: u64,
            },
            Reload {
                message: String,
                path: PathBuf,
            },
        }
        let start = match self.loaded.as_ref() {
            None => return Ok(()),
            Some(loaded) => match &loaded.clip {
                Clip::Released => Start::Reload {
                    message: loaded.message.clone(),
                    path: loaded.path.clone(),
                },
                Clip::Memory { samples } => {
                    let offset = ((fraction.clamp(0.0, 1.0) * samples.len() as f32) as usize)
                        .min(samples.len());
                    Start::Memory {
                        samples: Arc::clone(samples),
                        offset,
                    }
                }
                Clip::File { spool, frames } => {
                    let skip =
                        ((fraction.clamp(0.0, 1.0) as f64 * *frames as f64) as u64).min(*frames);
                    Start::File {
                        spool: spool.clone(),
                        skip,
                        frames: *frames,
                    }
                }
            },
        };
        if let Start::Reload { message, path } = start {
            return self.load(&message, &path, fraction);
        }
        if self.output.is_none() {
            let device = rodio::DeviceSinkBuilder::open_default_sink()
                .map_err(|error| format!("No sound output: {error}"))?;
            let sink = rodio::Player::connect_new(device.mixer());
            self.output = Some((device, sink));
        }
        // The spool file may be gone (removed while released); decode again.
        if let Start::File { spool, .. } = &start
            && !spool.exists()
        {
            let (message, path) = match self.loaded.as_ref() {
                Some(loaded) => (loaded.message.clone(), loaded.path.clone()),
                None => return Ok(()),
            };
            if let Some(loaded) = self.loaded.as_mut() {
                loaded.clip = Clip::Released;
            }
            return self.load(&message, &path, fraction);
        }
        let (_, sink) = self.output.as_ref().expect("just opened");
        sink.clear();
        // Every clip keeps the speed the reader chose for it. The sink
        // always runs at 1x: tempo comes from a stretching source, and
        // 1x keeps the original untouched path.
        let speed = self
            .loaded
            .as_ref()
            .map(|loaded| self.speed_of(&loaded.message))
            .unwrap_or(1.0);
        sink.set_speed(1.0);
        let stretched = speed != 1.0;
        // Seeking never copies the tail anymore: memory clips play from the
        // shared samples at an offset, file clips stream from their spool.
        let base = match start {
            Start::Memory { samples, offset } => {
                let base = clip_length(offset);
                let tail = samples.len() - offset;
                if stretched {
                    append_stretched(
                        sink,
                        SharedSamples::new(samples, offset),
                        speed,
                        tail as u64,
                    );
                } else {
                    sink.append(SharedSamples::new(samples, offset));
                }
                base
            }
            Start::File {
                spool,
                skip,
                frames,
            } => {
                let base = clip_length(skip.min(usize::MAX as u64) as usize);
                if stretched {
                    let total_in = frames.saturating_sub(skip);
                    append_stretched(
                        sink,
                        FileSamples::open(&spool, skip, frames)?,
                        speed,
                        total_in,
                    );
                } else {
                    sink.append(FileSamples::open(&spool, skip, frames)?);
                }
                base
            }
            Start::Reload { .. } => unreachable!("handled above"),
        };
        sink.play();
        let Some(loaded) = self.loaded.as_mut() else {
            return Ok(());
        };
        loaded.base = base;
        // A fresh source resets the output clock, so the anchor restarts too.
        loaded.base_sink = Duration::ZERO;
        loaded.paused = false;
        loaded.done = false;
        Ok(())
    }
}

fn clip_length(samples: usize) -> Duration {
    Duration::from_secs_f64(samples as f64 / f64::from(voice::RATE))
}

/// Media position from output time. Rodio reports wall-clock output, so the
/// speed scales only the output after the anchor: the stretch already heard
/// at an older speed keeps its price, and switching mid-clip never jumps.
fn speed_position(
    base: Duration,
    output: Duration,
    anchor: Duration,
    speed: f32,
    total: Duration,
) -> Duration {
    (base + output.saturating_sub(anchor).mul_f32(speed.max(0.0))).min(total)
}

/// Decodes a file for playback. OGG/Opus voice notes decode in memory;
/// anything else is decoded to a mono 48 kHz spool file so a long attachment
/// never materializes as hundreds of megabytes of samples.
fn decode_file(path: &Path) -> Result<DecodedClip, String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("Could not read the audio: {error}"))?;
    if bytes.starts_with(b"OggS")
        && let Ok(samples) = voice::decode(&bytes)
    {
        return Ok(DecodedClip::Memory(samples));
    }
    decode_to_spool(path).map(DecodedClip::File)
}

/// Next spool file number. Spool files die with their clip; leftovers from a
/// crash share the OS temporary directory with everything else.
static SPOOL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn spool_path() -> PathBuf {
    let id = SPOOL_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("zapfast-audio-{}-{id}.pcm", std::process::id()))
}

fn remove_quiet(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Decodes any rodio-supported file to a mono 48 kHz spool file.
fn decode_to_spool(path: &Path) -> Result<SpooledClip, String> {
    let file = File::open(path).map_err(|error| format!("Could not read the audio: {error}"))?;
    let decoder = rodio::Decoder::new(BufReader::new(file))
        .map_err(|error| format!("Could not decode the audio: {error}"))?;
    let channels = decoder.channels().get();
    let rate = decoder.sample_rate().get();
    let raw = spool_path();
    let spooled = (|| {
        let mut writer = BufWriter::new(
            File::create(&raw).map_err(|error| format!("Could not buffer the audio: {error}"))?,
        );
        let mut decoder = decoder;
        loop {
            let block: Vec<f32> = decoder.by_ref().take(SPOOL_BLOCK).collect();
            if block.is_empty() {
                break;
            }
            append_raw(&mut writer, &block)?;
        }
        writer
            .flush()
            .map_err(|error| format!("Could not buffer the audio: {error}"))?;
        convert_spooled_raw(&raw, channels, rate)
    })();
    if spooled.is_err() {
        remove_quiet(&raw);
    }
    spooled
}

fn append_raw(writer: &mut BufWriter<File>, samples: &[f32]) -> Result<(), String> {
    for sample in samples {
        writer
            .write_all(&sample.to_le_bytes())
            .map_err(|error| format!("Could not buffer the audio: {error}"))?;
    }
    Ok(())
}

/// Converts spooled device-rate interleaved samples to mono 48 kHz.
/// The raw spool is always removed; only the converted spool is returned.
fn convert_spooled_raw(raw: &Path, channels: u16, rate: u32) -> Result<SpooledClip, String> {
    let mono = spool_path();
    let frames = write_mixed_mono(raw, &mono, channels)?;
    remove_quiet(raw);
    if rate == voice::RATE || rate == 0 {
        return Ok(SpooledClip { path: mono, frames });
    }
    let out = spool_path();
    let converted = resample_mono_file(&mono, &out, rate, frames);
    remove_quiet(&mono);
    converted.map(|out_frames| SpooledClip {
        path: out,
        frames: out_frames,
    })
}

/// Mixes spooled interleaved samples down to mono, streaming block by block.
/// Matches `voice::mono_at_rate` exactly, including dropping a trailing
/// partial frame the way `chunks_exact` does.
fn write_mixed_mono(raw: &Path, mono: &Path, channels: u16) -> Result<u64, String> {
    let channels = usize::from(channels.max(1));
    let mut input = BufReader::new(
        File::open(raw).map_err(|error| format!("Could not buffer the audio: {error}"))?,
    );
    let mut output = BufWriter::new(
        File::create(mono).map_err(|error| format!("Could not buffer the audio: {error}"))?,
    );
    let mut pending: Vec<f32> = Vec::new();
    let mut block: Vec<f32> = Vec::new();
    let mut frames: u64 = 0;
    loop {
        let read = read_f32_block(&mut input, &mut block, SPOOL_BLOCK)?;
        if read == 0 {
            break;
        }
        pending.extend_from_slice(&block);
        let complete = pending.len() / channels * channels;
        for frame in pending[..complete].chunks_exact(channels) {
            let mixed = frame.iter().sum::<f32>() / channels as f32;
            output
                .write_all(&mixed.to_le_bytes())
                .map_err(|error| format!("Could not buffer the audio: {error}"))?;
        }
        frames += (complete / channels) as u64;
        pending.drain(..complete);
    }
    output
        .flush()
        .map_err(|error| format!("Could not buffer the audio: {error}"))?;
    Ok(frames)
}

/// Resamples spooled mono samples to 48 kHz, streaming with one frame of
/// overlap. Matches `voice::mono_at_rate` exactly, including the final
/// sample clamp and the empty output for clips too short to convert.
fn resample_mono_file(mono: &Path, out: &Path, rate: u32, frames: u64) -> Result<u64, String> {
    let ratio = f64::from(rate) / f64::from(voice::RATE);
    let count = (frames as f64 / ratio).floor() as u64;
    let mut output = BufWriter::new(
        File::create(out).map_err(|error| format!("Could not buffer the audio: {error}"))?,
    );
    if count == 0 {
        output
            .flush()
            .map_err(|error| format!("Could not buffer the audio: {error}"))?;
        return Ok(0);
    }
    let mut input = BufReader::new(
        File::open(mono).map_err(|error| format!("Could not buffer the audio: {error}"))?,
    );
    // Available mono samples; window[0] is sample `base`.
    let mut window: Vec<f32> = Vec::new();
    let mut base: u64 = 0;
    let mut block: Vec<f32> = Vec::new();
    let mut eof = false;
    let mut next: u64 = 0;
    while next < count {
        let pos = next as f64 * ratio;
        let left = pos.floor() as u64;
        // Mono indices only move forward, so everything before `left` goes.
        if left > base {
            let drop = (left - base).min(window.len() as u64) as usize;
            window.drain(..drop);
            base += drop as u64;
        }
        // Every output needs sample `left + 1` unless the clip ends there.
        while !eof && left + 1 < frames && base + window.len() as u64 <= left + 1 {
            if read_f32_block(&mut input, &mut block, RESAMPLE_BLOCK)? == 0 {
                eof = true;
            } else {
                window.extend_from_slice(&block);
            }
        }
        let slot = (left - base) as usize;
        let a = window[slot];
        let b = if left + 1 < frames {
            window[slot + 1]
        } else {
            a
        };
        let mixed = a + (b - a) * (pos - left as f64) as f32;
        output
            .write_all(&mixed.to_le_bytes())
            .map_err(|error| format!("Could not buffer the audio: {error}"))?;
        next += 1;
    }
    output
        .flush()
        .map_err(|error| format!("Could not buffer the audio: {error}"))?;
    Ok(count)
}

/// Reads up to `max` little-endian f32 samples, returning how many arrived.
/// A trailing partial sample at end of file is dropped.
fn read_f32_block(
    reader: &mut BufReader<File>,
    out: &mut Vec<f32>,
    max: usize,
) -> Result<usize, String> {
    out.clear();
    let mut bytes = vec![0u8; max * 4];
    let mut filled = 0;
    while filled < bytes.len() {
        match reader
            .read(&mut bytes[filled..])
            .map_err(|error| format!("Could not buffer the audio: {error}"))?
        {
            0 => break,
            read => filled += read,
        }
    }
    out.extend(
        bytes[..filled / 4 * 4]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&chunk| f32::from_le_bytes(chunk)),
    );
    Ok(out.len())
}

/// Rebuilds the waveform `voice::waveform` would compute, reading the spool
/// in chunk-sized passes instead of holding every sample.
fn waveform_from_spool(path: &Path, frames: u64) -> Result<Vec<u8>, String> {
    if frames == 0 {
        return Ok(vec![0; voice::BARS]);
    }
    let slice = frames.div_ceil(voice::BARS as u64);
    let mut input = BufReader::new(
        File::open(path).map_err(|error| format!("Could not read the audio: {error}"))?,
    );
    let mut loudness: Vec<f32> = Vec::new();
    let mut start: u64 = 0;
    let mut bytes = vec![0u8; (RESAMPLE_BLOCK * 4).max(4)];
    while start < frames {
        let end = (start + slice).min(frames);
        let mut sum = 0.0f32;
        let mut got: u64 = 0;
        let mut left = end - start;
        input
            .seek(SeekFrom::Start(start * 4))
            .map_err(|error| format!("Could not read the audio: {error}"))?;
        while left > 0 {
            let want = (left.min(RESAMPLE_BLOCK as u64) as usize * 4).min(bytes.len());
            let mut filled = 0;
            while filled < want {
                match input
                    .read(&mut bytes[filled..want])
                    .map_err(|error| format!("Could not read the audio: {error}"))?
                {
                    0 => break,
                    read => filled += read,
                }
            }
            if filled == 0 {
                break;
            }
            for &chunk in bytes[..filled / 4 * 4].as_chunks::<4>().0 {
                let sample = f32::from_le_bytes(chunk);
                sum += sample * sample;
            }
            let arrived = (filled / 4) as u64;
            got += arrived;
            left -= arrived;
            if filled < want {
                break;
            }
        }
        if got == 0 {
            break;
        }
        loudness.push((sum / got as f32).sqrt());
        start = end;
    }
    let loudest = loudness.iter().copied().fold(0.0f32, f32::max);
    let mut bars: Vec<u8> = loudness
        .iter()
        .map(|value| {
            if loudest > 0.0 {
                (value / loudest * 100.0).round() as u8
            } else {
                0
            }
        })
        .collect();
    bars.resize(voice::BARS, 0);
    Ok(bars)
}

type Outcome = Arc<Mutex<Option<Result<Vec<f32>, String>>>>;

/// Records from the default microphone until told to stop.
pub struct Recorder {
    started: Instant,
    stop: Arc<AtomicBool>,
    /// Loudness for each recorded 50 ms segment.
    levels: Arc<Mutex<Vec<f32>>>,
    outcome: Outcome,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Recorder {
    pub fn start(waker: Waker) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let levels: Arc<Mutex<Vec<f32>>> = Default::default();
        let outcome: Outcome = Default::default();
        let spawned = {
            let stop = Arc::clone(&stop);
            let levels = Arc::clone(&levels);
            let outcome = Arc::clone(&outcome);
            std::thread::Builder::new()
                .name("voice-record".to_owned())
                .spawn(move || {
                    let result = record(&stop, &levels, &waker);
                    *outcome.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
                    waker.wake();
                })
        };
        let thread = match spawned {
            Ok(thread) => Some(thread),
            Err(error) => {
                *outcome.lock().unwrap_or_else(|p| p.into_inner()) = Some(Err(error.to_string()));
                None
            }
        };
        Self {
            started: Instant::now(),
            stop,
            levels,
            outcome,
            thread,
        }
    }

    /// Simulated recorder for demos and tests.
    #[cfg(any(test, feature = "demo"))]
    pub fn rehearsal() -> Self {
        let levels: Vec<f32> = (0..90)
            .map(|index| 0.05 + 0.2 * ((index as f32 * 0.6).sin().abs()))
            .collect();
        Self {
            started: Instant::now() - Duration::from_millis(4_500),
            stop: Arc::new(AtomicBool::new(true)),
            levels: Arc::new(Mutex::new(levels)),
            outcome: Default::default(),
            thread: None,
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn levels(&self) -> Vec<f32> {
        self.levels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Error that stopped recording early.
    pub fn failure(&self) -> Option<String> {
        match self
            .outcome
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            Some(Err(error)) => Some(error.clone()),
            _ => None,
        }
    }

    /// Stops and returns mono 48 kHz samples.
    pub fn finish(mut self) -> Result<Vec<f32>, String> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.outcome
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .unwrap_or_else(|| Err("No audio was recorded".to_owned()))
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn record(stop: &AtomicBool, levels: &Mutex<Vec<f32>>, waker: &Waker) -> Result<Vec<f32>, String> {
    let mut microphone = rodio::microphone::MicrophoneBuilder::new()
        .default_device()
        .map_err(|error| format!("No microphone available: {error}"))?
        .default_config()
        .map_err(|error| format!("The microphone has no supported format: {error}"))?
        .open_stream()
        .map_err(|error| format!("Could not open the microphone: {error}"))?;
    let channels = microphone.channels().get();
    let rate = microphone.sample_rate().get();
    let chunk = (rate as usize * usize::from(channels) / 20).max(1);
    let started = Instant::now();
    // Raw device samples spill to disk as they arrive: holding the whole
    // recording in memory and converting it at the end peaks at twice the
    // clip. Only the converted clip below ever lives in memory at once.
    let raw = spool_path();
    let mut spool = BufWriter::new(
        File::create(&raw).map_err(|error| format!("Could not record audio: {error}"))?,
    );
    let mut heard: u64 = 0;
    while !stop.load(Ordering::Relaxed) && started.elapsed() < LONGEST_RECORDING {
        let taken: Vec<f32> = microphone.by_ref().take(chunk).collect();
        if taken.is_empty() {
            break;
        }
        append_raw(&mut spool, &taken)
            .map_err(|error| error.replace("buffer the audio", "record audio"))?;
        let loudness = (taken.iter().map(|s| s * s).sum::<f32>() / taken.len() as f32).sqrt();
        levels
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(loudness);
        waker.wake();
        heard += taken.len() as u64;
        if taken.len() < chunk {
            // The device disappeared before recording stopped.
            break;
        }
    }
    spool
        .flush()
        .map_err(|error| format!("Could not record audio: {error}"))?;
    drop(spool);
    if heard == 0 {
        remove_quiet(&raw);
        return Err("The microphone did not record any audio".to_owned());
    }
    let converted = convert_spooled_raw(&raw, channels, rate);
    remove_quiet(&raw);
    let spool = converted?;
    let mut reader = BufReader::new(
        File::open(&spool.path).map_err(|error| format!("Could not record audio: {error}"))?,
    );
    let mut samples: Vec<f32> = Vec::new();
    let mut block: Vec<f32> = Vec::new();
    loop {
        if read_f32_block(&mut reader, &mut block, SPOOL_BLOCK)
            .map_err(|error| error.replace("buffer the audio", "record audio"))?
            == 0
        {
            break;
        }
        samples.extend_from_slice(&block);
    }
    remove_quiet(&spool.path);
    Ok(samples)
}

/// Temporary recording path used before sending and archiving.
#[allow(dead_code)]
pub fn recording_path(dir: &Path) -> PathBuf {
    dir.join(format!("voice-{}.ogg", crate::util::now()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Stretch fixtures: everything below is mono 48 kHz, the only
    // shape the player ever feeds the stretcher.
    const RATE: usize = 48_000;
    fn sine(seconds: f32, freq: f32) -> Vec<f32> {
        let total = (seconds * RATE as f32) as usize;
        (0..total)
            .map(|n| (2.0 * std::f32::consts::PI * freq * n as f32 / RATE as f32).sin())
            .collect()
    }
    fn silence(seconds: f32) -> Vec<f32> {
        vec![0.0; (seconds * RATE as f32) as usize]
    }
    // Deterministic pseudo-speech: pitch gliding 110 to 140 Hz,
    // harmonics decaying 1/n, syllable-rate amplitude wobble. A
    // periodicity probe, never a claim about intelligibility.
    fn speech(seconds: f32) -> Vec<f32> {
        let total = (seconds * RATE as f32) as usize;
        // Phase integrates the gliding pitch, so the instantaneous
        // frequency really sweeps 110 to 140 Hz.
        let mut phase = 0.0f32;
        (0..total)
            .map(|n| {
                let t = n as f32 / RATE as f32;
                let pitch = 110.0 + 30.0 * t / seconds;
                phase += 2.0 * std::f32::consts::PI * pitch / RATE as f32;
                let harmonics =
                    phase.sin() + 0.5 * (2.0 * phase).sin() + 0.33 * (3.0 * phase).sin();
                let syllable = 0.6 + 0.4 * (2.0 * std::f32::consts::PI * 4.0 * t).sin();
                0.4 * harmonics * syllable
            })
            .collect()
    }
    // Dominant frequency by rising zero crossings over the middle
    // 80 percent, skipping junction and tail edges.
    fn crossing_freq(samples: &[f32]) -> f32 {
        let skip = samples.len() / 10;
        let body = &samples[skip..samples.len() - skip];
        let mut crossings = 0u32;
        for pair in body.windows(2) {
            if pair[0] <= 0.0 && pair[1] > 0.0 {
                crossings += 1;
            }
        }
        crossings as f32 / (body.len() as f32 / RATE as f32)
    }
    // Dominant pitch by normalized autocorrelation over lags for
    // 60 to 300 Hz, measured on the middle half of the clip.
    fn autocorr_pitch(samples: &[f32]) -> f32 {
        let start = samples.len() / 4;
        let body = &samples[start..start + samples.len() / 2];
        let min_lag = RATE / 300;
        let max_lag = RATE / 60;
        let energy: f32 = body.iter().map(|s| s * s).sum();
        let mut best_lag = min_lag;
        let mut best_corr = f32::NEG_INFINITY;
        for lag in min_lag..=max_lag.min(body.len() - 1) {
            let mut corr = 0.0;
            for (a, b) in body.iter().zip(body[lag..].iter()).step_by(7) {
                corr += a * b;
            }
            if corr > best_corr {
                best_corr = corr;
                best_lag = lag;
            }
        }
        let _ = energy;
        RATE as f32 / best_lag as f32
    }
    fn stretched(samples: Vec<f32>, speed: f32) -> Vec<f32> {
        let total = samples.len() as u64;
        Stretched::new(samples.into_iter(), speed, total).collect()
    }

    #[test]
    fn the_speed_cycle_walks_one_and_a_half_and_two() {
        assert_eq!(snap_speed(1.4), 1.5);
        assert_eq!(snap_speed(0.1), 1.0);
        assert_eq!(snap_speed(f32::NAN), 1.0);
        assert_eq!(snap_speed(-3.0), 1.0);
        assert_eq!(next_speed(1.0), 1.5);
        assert_eq!(next_speed(1.5), 2.0);
        assert_eq!(next_speed(2.0), 1.0);
        // A strange number snaps to a supported speed first.
        assert_eq!(next_speed(1.9), 1.0);
    }

    #[test]
    fn stretch_keeps_tone_and_duration() {
        // A 440 Hz tone keeps its pitch at every speed: zero crossings
        // over the middle 80 percent resolve about 2 Hz here, so a 2
        // percent band holds real preservation with room to spare.
        // The 1x path is the untouched bare source, identical samples
        // by construction; the stretcher only ever sees faster speeds.
        let reference = crossing_freq(&sine(12.0, 440.0));
        assert!((reference - 440.0).abs() / 440.0 < 0.02);
        for speed in [1.5, 2.0] {
            let out = stretched(sine(12.0, 440.0), speed);
            let freq = crossing_freq(&out);
            assert!(
                (freq - reference).abs() / reference < 0.02,
                "tone holds at {speed}x: {freq} Hz"
            );
            let expected = 12.0 * RATE as f32 / speed;
            assert!(
                (out.len() as f32 - expected).abs() < 4096.0,
                "12 s lasts 12, 8, 6 s at {speed}x: {} samples",
                out.len()
            );
        }
    }

    #[test]
    fn stretch_silence_and_short_clips() {
        for speed in [1.5, 2.0] {
            let out = stretched(silence(1.0), speed);
            let expected = RATE as f32 / speed;
            assert!((out.len() as f32 - expected).abs() < 4096.0);
            assert!(out.iter().all(|sample| *sample == 0.0));
        }
        // Below two hops the input drains unchanged: identity beats
        // resampling where periodicity is meaningless.
        let tiny: Vec<f32> = (0..100).map(|n| n as f32 / 100.0).collect();
        for speed in [1.5, 2.0] {
            let out = stretched(tiny.clone(), speed);
            assert_eq!(out, tiny);
        }
        // Just above the threshold the length still tracks the ratio.
        let small: Vec<f32> = (0..1500).map(|n| (n as f32 / 48.0).sin()).collect();
        for speed in [1.5, 2.0] {
            let out = stretched(small.clone(), speed);
            assert!((out.len() as f32 - 1500.0 / speed).abs() < 4096.0);
            assert_eq!(out[..512], small[..512]);
        }
    }

    #[test]
    fn stretch_speech_like_audio_is_less_jagged_with_overlap_add() {
        let input = speech_like();
        let fork = stretched(input.clone(), 1.5);
        let upstream = crate::timestretch::speed_up(&input, 1.5);
        assert!(fork.iter().all(|sample| sample.is_finite()));
        assert!(upstream.iter().all(|sample| sample.is_finite()));
        assert!(
            peak(&upstream) < 1.5,
            "overlap-add peak {}",
            peak(&upstream)
        );
        // A plain sample step is higher for overlap-add on this formant
        // (about 0.029 against 0.026). That is the pulse slope, not hiss.
        // The second difference is the high band the hop blend was raising.
        let fork_hiss = high_band(&fork);
        let upstream_hiss = high_band(&upstream);
        assert!(
            upstream_hiss < fork_hiss,
            "formant overlap-add high band {upstream_hiss} should be under the hop blend {fork_hiss}"
        );
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".local-roadmap/voice-samples");
        let _ = std::fs::create_dir_all(&dir);
        write_wav(&dir.join("speech-like-1x.wav"), &input);
        write_wav(&dir.join("speech-like-1.5x-hop-blend.wav"), &fork);
        write_wav(&dir.join("speech-like-1.5x-overlap-add.wav"), &upstream);
        write_wav(
            &dir.join("speech-like-2x-overlap-add.wav"),
            &crate::timestretch::speed_up(&input, 2.0),
        );
        write_wav(&dir.join("speech-like-2x-hop-blend.wav"), &fork_2x());
    }

    fn fork_2x() -> Vec<f32> {
        stretched(speech_like(), 2.0)
    }

    #[test]
    fn overlap_add_continues_across_pushes_and_past_two_minutes() {
        let rate = voice::RATE as usize;
        for seconds in [119, 120, 121] {
            let input = long_formant(seconds * rate);
            let whole = crate::timestretch::speed_up(&input, 1.5);
            let mut stream = crate::timestretch::Stream::new(1.5);
            for chunk in input.chunks(10_000) {
                stream.push(chunk);
            }
            stream.close(input.len());
            let mut parted = Vec::new();
            while let Some(sample) = stream.pop() {
                parted.push(sample);
            }
            assert_eq!(parted.len(), whole.len(), "{seconds}s length");
            let max = parted
                .iter()
                .zip(whole.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f32::max);
            assert!(max < 1e-4, "{seconds}s streams differ by {max}");
        }
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".local-roadmap/voice-samples");
        let _ = std::fs::create_dir_all(&dir);
        let around = long_formant(121 * rate);
        let played = crate::timestretch::speed_up(&around, 1.5);
        let at = (120.0 * rate as f32 / 1.5) as usize;
        let excerpt = &played[at.saturating_sub(rate)..(at + rate).min(played.len())];
        write_wav(&dir.join("speech-like-120s-join-1.5x.wav"), excerpt);
    }

    fn long_formant(n: usize) -> Vec<f32> {
        let rate = voice::RATE as f32;
        (0..n)
            .map(|i| {
                let t = i as f32 / rate;
                let cycle = (t * 120.0).fract();
                let pulse = if cycle < 0.4 {
                    (cycle / 0.4 * std::f32::consts::PI).sin()
                } else {
                    0.0
                };
                pulse * 0.45
                    + (t * 700.0 * std::f32::consts::TAU).sin() * 0.35
                    + (t * 1220.0 * std::f32::consts::TAU).sin() * 0.2
            })
            .collect()
    }

    /// A short voiced stretch: a noise burst, then a 120 Hz pulse with three
    /// formants. This is not a recording of speech. It is the signal the
    /// stretch comparison is allowed to use.
    fn speech_like() -> Vec<f32> {
        let mut out = Vec::new();
        for i in 0..(voice::RATE as usize / 5) {
            let n = i as f32 / voice::RATE as f32;
            let noise = ((i as u32).wrapping_mul(17).wrapping_mul(1_103_515_245) as f32
                / u32::MAX as f32)
                - 0.5;
            out.push(noise * (1.0 - n * 4.0).max(0.0) * 0.3);
        }
        let rate = voice::RATE as f32;
        for i in 0..(voice::RATE as usize / 2) {
            let t = i as f32 / rate;
            let cycle = (t * 120.0).fract();
            let pulse = if cycle < 0.4 {
                (cycle / 0.4 * std::f32::consts::PI).sin()
            } else {
                0.0
            };
            let f1 = (t * 700.0 * std::f32::consts::TAU).sin() * 0.35;
            let f2 = (t * 1220.0 * std::f32::consts::TAU).sin() * 0.2;
            let f3 = (t * 2600.0 * std::f32::consts::TAU).sin() * 0.08;
            out.push(pulse * 0.45 + f1 + f2 + f3);
        }
        out
    }

    /// Energy of the second difference over the steady vowel, after the burst.
    /// That is the high band. A click or a steeper formant slope would dominate
    /// a plain sample-to-sample maximum and would not describe hiss.
    fn high_band(samples: &[f32]) -> f32 {
        let start = voice::RATE as usize / 5;
        let body = samples.get(start..).unwrap_or(samples);
        if body.len() < 3 {
            return 0.0;
        }
        let sum: f32 = body
            .windows(3)
            .map(|w| {
                let d = w[2] - 2.0 * w[1] + w[0];
                d * d
            })
            .sum();
        (sum / (body.len() - 2) as f32).sqrt()
    }

    fn peak(samples: &[f32]) -> f32 {
        samples
            .iter()
            .map(|sample| sample.abs())
            .fold(0.0, f32::max)
    }

    fn write_wav(path: &std::path::Path, samples: &[f32]) {
        use std::io::Write;
        let mut file = std::fs::File::create(path).expect("wav");
        let data_len = samples.len() as u32 * 2;
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + data_len).to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt ").unwrap();
        file.write_all(&16u32.to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap();
        file.write_all(&voice::RATE.to_le_bytes()).unwrap();
        file.write_all(&(voice::RATE * 2).to_le_bytes()).unwrap();
        file.write_all(&2u16.to_le_bytes()).unwrap();
        file.write_all(&16u16.to_le_bytes()).unwrap();
        file.write_all(b"data").unwrap();
        file.write_all(&data_len.to_le_bytes()).unwrap();
        for sample in samples {
            let clipped = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            file.write_all(&clipped.to_le_bytes()).unwrap();
        }
    }

    #[test]
    fn stretch_speech_pitch_and_speech_duration() {
        // Periodicity probe across speeds, never an intelligibility
        // claim: the 110 to 140 Hz glide must read back near itself.
        // The 1x reference is the bare input the untouched path emits.
        let input = speech(4.0);
        let reference = autocorr_pitch(&input);
        assert!((100.0..150.0).contains(&reference));
        for speed in [1.5, 2.0] {
            let out = stretched(input.clone(), speed);
            let pitch = autocorr_pitch(&out);
            assert!(
                (100.0..150.0).contains(&pitch),
                "glide range at {speed}x: {pitch}"
            );
            assert!(
                (pitch - reference).abs() / reference < 0.05,
                "pitch holds at {speed}x: {pitch} Hz over {reference} Hz"
            );
            let expected = 4.0 * RATE as f32 / speed;
            assert!((out.len() as f32 - expected).abs() < 4096.0);
        }
    }

    #[test]
    fn stretch_restart_offset_keeps_content_and_ratio() {
        // A speed switch rebuilds from an original-timeline offset, the
        // same shape restart() uses: the fresh source opens on the input
        // slice and shortens what follows by the new speed.
        let input = Arc::new(sine(4.0, 220.0));
        let offset = 48_000;
        let tail = input.len() - offset;
        let out: Vec<f32> = Stretched::new(
            SharedSamples::new(Arc::clone(&input), offset),
            1.5,
            tail as u64,
        )
        .collect();
        assert_eq!(out[..512], input[offset..offset + 512]);
        assert!((out.len() as f32 - tail as f32 / 1.5).abs() < 4096.0);
    }

    #[test]
    fn stretch_memory_stays_bounded_on_long_audio() {
        // Fifteen minutes as a generator, never a Vec: the stretcher
        // must pull through with constant state and an exact count.
        struct Gen(u64);
        impl Iterator for Gen {
            type Item = f32;
            fn next(&mut self) -> Option<f32> {
                if self.0 == 0 {
                    return None;
                }
                self.0 -= 1;
                Some(0.25)
            }
        }
        let total_in: u64 = 48_000 * 60 * 15;
        let mut stretcher = Stretched::new(Gen(total_in), 2.0, total_in);
        let mut count = 0u64;
        while stretcher.next().is_some() {
            count += 1;
        }
        assert!((count as f64 - total_in as f64 / 2.0).abs() < 4096.0);
        assert!(stretcher.buf.capacity() <= 16_384);
        assert!(stretcher.out.capacity() <= 2 * STRETCH_HOP);
    }

    /// 48 kHz mono 16-bit WAV bytes for listening samples and fixtures.
    fn wav_48k(samples: &[f32]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + samples.len() as u32 * 2).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&96_000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(samples.len() as u32 * 2).to_le_bytes());
        for sample in samples {
            let clamped = sample.clamp(-1.0, 1.0);
            bytes.extend_from_slice(&((clamped * 32767.0) as i16).to_le_bytes());
        }
        bytes
    }

    #[test]
    fn stretch_spool_source_keeps_tone_and_ratio() {
        // The production file path: synthetic speech through a WAV
        // fixture, spool decode, then the same stretcher the sink pulls.
        let dir = std::env::temp_dir().join(format!("zapfast-stretch-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let path = dir.join("speech.wav");
        std::fs::write(&path, wav_48k(&speech(2.0))).expect("writes");
        let spool = decode_to_spool(&path).expect("spools");
        let total_in = spool.frames;
        let out: Vec<f32> = Stretched::new(
            FileSamples::open(&spool.path, 0, spool.frames).expect("streams"),
            2.0,
            total_in,
        )
        .collect();
        assert!((out.len() as f32 - total_in as f32 / 2.0).abs() < 4096.0);
        assert!((100.0..150.0).contains(&autocorr_pitch(&out)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn generate_listening_samples() {
        // Same synthetic speech through the production stretcher at
        // 1x, 1.5x and 2x, for listening by ear. Synthetic prosody
        // only: natural-voice quality is not claimed by any test here.
        let dir = std::path::PathBuf::from(".local-roadmap/voice-samples");
        std::fs::create_dir_all(&dir).expect("creates");
        let input = speech(4.0);
        for (name, speed) in [
            ("speech-1x.wav", 1.0),
            ("speech-15x.wav", 1.5),
            ("speech-2x.wav", 2.0),
        ] {
            let rendered = if speed == 1.0 {
                input.clone()
            } else {
                stretched(input.clone(), speed)
            };
            std::fs::write(dir.join(name), wav_48k(&rendered)).expect("writes");
        }
    }

    #[test]
    fn speed_switch_without_output_only_records() {
        // Without an output device a speed switch only records: the
        // device opens later through restart, which reads the choice.
        // Restart, pause and the rebuilt source need a live sink, so
        // they stay covered by inspection, not by this headless test.
        let dir = std::env::temp_dir().join(format!("zapfast-voice-nosink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let path = dir.join("note.wav");
        std::fs::write(&path, wav_bytes()).expect("writes");
        let mut player = Player::new(crate::backend::Waker::default());
        player.toggle("m1", &path).expect("loads");
        player.set_speed("m1", 2.0);
        assert_eq!(player.speed_of("m1"), 2.0);
        assert!(player.output.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deleting_the_playing_message_really_stops() {
        // Cessation by identity: the setup needs no audio device, only a
        // decodable file, because loading registers before output opens.
        let dir = std::env::temp_dir().join(format!("zapfast-voice-stop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let path = dir.join("note.wav");
        std::fs::write(&path, wav_bytes()).expect("writes");
        let mut player = Player::new(crate::backend::Waker::default());
        player.toggle("m1", &path).expect("loads");
        assert_eq!(player.playing_message(), Some("m1"));
        // An unrelated message keeps its sound.
        let mut other = Player::new(crate::backend::Waker::default());
        other.toggle("m2", &path).expect("loads");
        other.stop();
        assert_eq!(other.playing_message(), None);
        assert!(!other.is_playing());
        player.stop();
        assert_eq!(player.playing_message(), None);
        assert!(!player.is_playing());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Half a second of mono 16-bit PCM silence with a header: decodable
    /// with no device and no dependencies.
    fn wav_bytes() -> Vec<u8> {
        let samples: Vec<i16> = (0..4000).map(|_| 0).collect();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + samples.len() as u32 * 2).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&8000u32.to_le_bytes());
        bytes.extend_from_slice(&16000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(samples.len() as u32 * 2).to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }
    #[test]
    fn a_speed_stays_with_the_clip_it_was_set_on() {
        let mut player = Player::new(crate::backend::Waker::default());
        assert_eq!(player.speed_of("a"), 1.0);
        player.set_speed("a", 2.0);
        assert_eq!(player.speed_of("a"), 2.0);
        assert_eq!(player.speed_of("b"), 1.0);
    }

    #[test]
    fn the_marker_scales_with_the_speed() {
        let total = Duration::from_secs(12);
        // Six output seconds at 2x cover the whole twelve-second clip.
        assert_eq!(
            speed_position(
                Duration::ZERO,
                Duration::from_secs(6),
                Duration::ZERO,
                2.0,
                total
            ),
            total
        );
        // Four output seconds at 1.5x land six seconds into the clip.
        assert_eq!(
            speed_position(
                Duration::ZERO,
                Duration::from_secs(4),
                Duration::ZERO,
                1.5,
                total
            ),
            Duration::from_secs(6)
        );
        // Normal speed reports the output unchanged, and the marker never
        // runs past the end of the clip.
        assert_eq!(
            speed_position(
                Duration::ZERO,
                Duration::from_secs(3),
                Duration::ZERO,
                1.0,
                total
            ),
            Duration::from_secs(3)
        );
        assert_eq!(
            speed_position(
                Duration::from_secs(10),
                Duration::from_secs(4),
                Duration::ZERO,
                2.0,
                total
            ),
            total
        );
    }

    #[test]
    fn switching_speed_mid_clip_never_jumps() {
        let total = Duration::from_secs(30);
        // Ten output seconds at 1x: the marker sits at ten seconds.
        let at_switch = speed_position(
            Duration::ZERO,
            Duration::from_secs(10),
            Duration::ZERO,
            1.0,
            total,
        );
        assert_eq!(at_switch, Duration::from_secs(10));
        // Five more output seconds at 2x add ten heard seconds: twenty in
        // all, not thirty. The old code repriced the first stretch too.
        let faster = speed_position(
            at_switch,
            Duration::from_secs(15),
            Duration::from_secs(10),
            2.0,
            total,
        );
        assert_eq!(faster, Duration::from_secs(20));
        // Slowing back to 1x continues from twenty without rewinding.
        let slower = speed_position(
            faster,
            Duration::from_secs(18),
            Duration::from_secs(15),
            1.0,
            total,
        );
        assert_eq!(slower, Duration::from_secs(23));
    }

    #[test]
    fn sources_report_bounded_spans_for_speed_changes() {
        let samples: Arc<Vec<f32>> = Arc::new(vec![0.0; SPAN_SAMPLES * 3]);
        let source = SharedSamples::new(Arc::clone(&samples), 0);
        assert_eq!(source.current_span_len(), Some(SPAN_SAMPLES));
        let path = test_spool("span");
        write_raw(&path, &vec![0.0; SPAN_SAMPLES * 3]);
        let file = FileSamples::open(&path, 0, (SPAN_SAMPLES * 3) as u64).expect("opens");
        assert_eq!(file.current_span_len(), Some(SPAN_SAMPLES));
        let tail = FileSamples::open(
            &path,
            (SPAN_SAMPLES * 3 - 10) as u64,
            (SPAN_SAMPLES * 3) as u64,
        )
        .expect("opens");
        assert_eq!(tail.current_span_len(), Some(10));
        let _ = std::fs::remove_file(&path);
    }

    /// A video file gives up its soundtrack once its metadata leads.
    ///
    /// Rodio and symphonia want the moov atom before the media, which is
    /// what -movflags +faststart writes. A clip whose metadata sits at the
    /// end is not recognized at all, and that is what an in-app video player
    /// has to work around.
    #[test]
    fn a_videos_soundtrack_decodes_when_its_metadata_leads() {
        // The clip is made here, so the test skips without ffmpeg.
        let dir = std::env::temp_dir().join(format!("zapfast-video-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("clip.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y"])
            .args(["-f", "lavfi", "-i", "color=c=blue:s=64x64:d=6:r=10"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=6"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .args(["-c:a", "aac", "-shortest", "-movflags", "+faststart"])
            .arg(&path)
            .status()
            .is_ok_and(|status| status.success());
        if !made {
            return;
        }
        let spooled = decode_to_spool(&path).expect("the soundtrack decodes");
        // An interleaved MP4 used to stop the soundtrack after the first
        // video packet, leaving about a tenth of a second. Frames above
        // zero do not catch that.
        let seconds = spooled.frames as f64 / f64::from(voice::RATE);
        assert!(
            seconds >= 5.5,
            "the whole soundtrack decodes, got {seconds:.2}s from {} frames",
            spooled.frames
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn test_spool(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "zapfast-audio-test-{}-{name}.pcm",
            std::process::id()
        ))
    }

    fn write_raw(path: &Path, samples: &[f32]) {
        let mut writer = BufWriter::new(File::create(path).expect("spool"));
        append_raw(&mut writer, samples).expect("writes");
        writer.flush().expect("flushes");
    }

    fn read_spool(path: &Path) -> Vec<f32> {
        let mut reader = BufReader::new(File::open(path).expect("spool opens"));
        let mut samples = Vec::new();
        let mut block = Vec::new();
        while read_f32_block(&mut reader, &mut block, SPOOL_BLOCK).expect("reads") != 0 {
            samples.extend_from_slice(&block);
        }
        samples
    }

    #[test]
    fn shared_samples_play_only_the_tail() {
        let samples: Arc<Vec<f32>> = Arc::new((0..1000).map(|i| i as f32).collect());
        let tail: Vec<f32> = SharedSamples::new(Arc::clone(&samples), 250).collect();
        assert_eq!(tail, (250..1000).map(|i| i as f32).collect::<Vec<_>>());
        let source = SharedSamples::new(Arc::clone(&samples), 250);
        assert_eq!(source.channels(), mono());
        assert_eq!(source.sample_rate(), rate());
        assert_eq!(source.total_duration(), Some(clip_length(750)));
    }

    #[test]
    fn file_samples_read_back_with_skip() {
        let path = test_spool("skip");
        let samples: Vec<f32> = (0..5000).map(|i| i as f32 * 0.5).collect();
        write_raw(&path, &samples);
        let tail: Vec<f32> = FileSamples::open(&path, 1234, 5000)
            .expect("opens")
            .collect();
        assert_eq!(tail, samples[1234..]);
        let source = FileSamples::open(&path, 1234, 5000).expect("opens");
        assert_eq!(source.total_duration(), Some(clip_length(3766)));
        let _ = std::fs::remove_file(&path);
    }

    /// The spooled conversion must sound exactly like the in-memory one.
    #[test]
    fn spooled_conversion_matches_in_memory() {
        for (channels, rate, seconds) in
            [(2u16, 44_100u32, 2.3f32), (1, 8_000, 1.7), (2, 48_000, 0.9)]
        {
            let frames = (rate as f32 * seconds) as usize;
            let interleaved: Vec<f32> = (0..frames * usize::from(channels))
                .map(|i| (i as f32 * 0.7).sin() * 0.4)
                .collect();
            let raw = test_spool(&format!("convert-{channels}-{rate}"));
            write_raw(&raw, &interleaved);
            let converted = convert_spooled_raw(&raw, channels, rate).expect("converts");
            let spooled = read_spool(&converted.path);
            assert_eq!(
                spooled,
                voice::mono_at_rate(&interleaved, channels, rate),
                "{channels} channels at {rate} Hz"
            );
            let _ = std::fs::remove_file(&converted.path);
        }
    }

    #[test]
    fn spooled_waveform_matches() {
        let samples: Vec<f32> = (0..100_000).map(|i| (i as f32 * 0.11).sin()).collect();
        let path = test_spool("waveform");
        write_raw(&path, &samples);
        assert_eq!(
            waveform_from_spool(&path, samples.len() as u64).expect("waveform"),
            voice::waveform(&samples)
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Plays a one-second test tone:
    /// `cargo test audio::tests::plays -- --ignored --nocapture`.
    #[test]
    #[ignore = "makes a sound on this machine"]
    fn plays_a_clip_on_this_machine() {
        let dir = std::env::temp_dir();
        let path = dir.join("zapfast-audio-test.ogg");
        let tone: Vec<f32> = (0..voice::RATE)
            .map(|i| (i as f32 * 330.0 * std::f32::consts::TAU / voice::RATE as f32).sin() * 0.3)
            .collect();
        std::fs::write(&path, voice::encode(&tone).expect("encodes")).expect("written");
        let mut player = Player::new(Waker::default());
        player.toggle("clip", &path).expect("starts decoding");
        assert_eq!(player.status("clip").state, State::Loading);
        let started = Instant::now();
        let mut seen_playing = false;
        while started.elapsed() < Duration::from_secs(3) {
            player.poll().expect("plays");
            let status = player.status("clip");
            if status.state == State::Playing && status.position > Duration::from_millis(300) {
                seen_playing = true;
                eprintln!("playing at {:?} of {:?}", status.position, status.total);
            }
            if seen_playing && status.state == State::Idle {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        assert!(seen_playing, "never heard it playing");
        assert_eq!(player.status("clip").state, State::Idle, "ends on its own");
        assert_eq!(player.bars("clip").map(<[u8]>::len), Some(voice::BARS));
        let _ = std::fs::remove_file(path);
    }

    /// Records one second from the default microphone:
    /// `cargo test audio::tests::records -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a microphone"]
    fn records_a_second_on_this_machine() {
        let recorder = Recorder::start(Waker::default());
        std::thread::sleep(Duration::from_millis(1_000));
        assert!(recorder.failure().is_none(), "{:?}", recorder.failure());
        let levels = recorder.levels();
        let heard = recorder.finish().expect("something was heard");
        eprintln!("{} samples, {} level readings", heard.len(), levels.len());
        assert!(
            heard.len() > voice::RATE as usize * 8 / 10,
            "{}",
            heard.len()
        );
        assert!(levels.len() >= 15, "{}", levels.len());
    }
}
