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
    /// Rodio speeds a clip up by resampling, so the voice rises in pitch the
    /// same way WhatsApp's own faster playback does. The change reaches the
    /// clip that is playing at the moment it is made, from where it is.
    pub fn set_speed(&mut self, message: &str, speed: f32) {
        let speed = snap_speed(speed);
        self.speeds.insert(message.to_owned(), speed);
        let playing = self
            .loaded
            .as_ref()
            .is_some_and(|loaded| loaded.message == message);
        if playing && let Some((_, sink)) = &self.output {
            sink.set_speed(speed);
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
                    .map(|(_, sink)| scaled_position(loaded.base, sink.get_pos(), speed, total))
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
        // Every clip keeps the speed the reader chose for it.
        let speed = self
            .loaded
            .as_ref()
            .map(|loaded| self.speed_of(&loaded.message))
            .unwrap_or(1.0);
        sink.set_speed(speed);
        // Seeking never copies the tail anymore: memory clips play from the
        // shared samples at an offset, file clips stream from their spool.
        let base = match start {
            Start::Memory { samples, offset } => {
                let base = clip_length(offset);
                sink.append(SharedSamples::new(samples, offset));
                base
            }
            Start::File {
                spool,
                skip,
                frames,
            } => {
                let base = clip_length(skip.min(usize::MAX as u64) as usize);
                sink.append(FileSamples::open(&spool, skip, frames)?);
                base
            }
            Start::Reload { .. } => unreachable!("handled above"),
        };
        sink.play();
        let Some(loaded) = self.loaded.as_mut() else {
            return Ok(());
        };
        loaded.base = base;
        loaded.paused = false;
        loaded.done = false;
        Ok(())
    }
}

fn clip_length(samples: usize) -> Duration {
    Duration::from_secs_f64(samples as f64 / f64::from(voice::RATE))
}

/// Media position from output time. Rodio reports wall-clock output, so at
/// 1.5x or 2x the marker has to scale the output or it lags a full speed
/// factor behind, showing half the clip when the voice already ended.
fn scaled_position(base: Duration, output: Duration, speed: f32, total: Duration) -> Duration {
    (base + output.mul_f32(speed.max(0.0))).min(total)
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
            scaled_position(Duration::ZERO, Duration::from_secs(6), 2.0, total),
            total
        );
        // Four output seconds at 1.5x land six seconds into the clip.
        assert_eq!(
            scaled_position(Duration::ZERO, Duration::from_secs(4), 1.5, total),
            Duration::from_secs(6)
        );
        // Normal speed reports the output unchanged, and the marker never
        // runs past the end of the clip.
        assert_eq!(
            scaled_position(Duration::ZERO, Duration::from_secs(3), 1.0, total),
            Duration::from_secs(3)
        );
        assert_eq!(
            scaled_position(Duration::from_secs(10), Duration::from_secs(4), 2.0, total),
            total
        );
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
            .args(["-f", "lavfi", "-i", "color=c=blue:s=64x64:d=1"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .args(["-c:a", "aac", "-shortest", "-movflags", "+faststart"])
            .arg(&path)
            .status()
            .is_ok_and(|status| status.success());
        if !made {
            return;
        }
        let spooled = decode_to_spool(&path).expect("the soundtrack decodes");
        assert!(spooled.frames > 0, "the clip is not empty");
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
