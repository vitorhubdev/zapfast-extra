//! In-app playback for chat videos.
//!
//! A video bubble used to hand its file to the system player. The H.264
//! track now decodes in-process (the `mp4` crate demuxes, `openh264` decodes)
//! while the soundtrack plays through rodio, and the audio clock decides
//! which frame is on screen. Anything the in-process path cannot read keeps
//! the old behaviour: a poster with a button for the default app.

use std::collections::VecDeque;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc::{Receiver, SyncSender, sync_channel},
};
use std::time::{Duration, Instant};

use egui::{ColorImage, TextureHandle, TextureOptions, Vec2};

/// Widest frame decoded for playback. A chat video is a poster that moves,
/// not a cinema, and decoding full HD in software would drop frames.
const PLAY_WIDTH: u32 = 480;
/// Sample rate of extracted soundtracks, matching the voice pipeline.
const PCM_RATE: u32 = 48_000;
/// How much soundtrack is kept: five minutes cover any chat video.
const PCM_CAP_SECS: u64 = 300;
/// Frames waiting to be shown. Caps memory while surviving decode hiccups.
const BUFFER_FRAMES: usize = 60;
/// Samples reported per span by the cached soundtrack. Rodio rebuilds its
/// rate converter at span boundaries; an unbounded span would freeze the
/// converter on the opening rate (see audio playback for the same fix).
const SPAN_SAMPLES: usize = 4_096;
/// How long a shown frame is kept behind the buffer for a pause or a seek.
const KEEP_BEHIND: Duration = Duration::from_secs(1);

/// What the viewer needs to know before the first frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    pub duration: Duration,
    pub width: u32,
    pub height: u32,
    pub has_audio: bool,
    /// True when frames come from ffmpeg instead of the in-process decoder.
    pub ffmpeg: bool,
}

/// Reads the header of an MP4 and reports whether it can play in-app.
///
/// Only H.264 video tracks decode in-process. Anything else (HEVC, a broken
/// header, no video track at all) reports why, and the viewer offers the
/// system player instead of a spinner that never ends.
pub fn probe(path: &Path) -> Result<Clip, String> {
    let file =
        std::fs::File::open(path).map_err(|error| format!("Could not open the video: {error}"))?;
    let size = file
        .metadata()
        .map_err(|error| format!("Could not read the video: {error}"))?
        .len();
    let mut mp4 = mp4::Mp4Reader::read_header(BufReader::new(file), size)
        .map_err(|_| "This video is not an MP4 file, or it is damaged.".to_owned())?;
    let track = mp4
        .tracks()
        .values()
        .find(|track| track.track_type().ok() == Some(mp4::TrackType::Video))
        .ok_or_else(|| "This file has no video track.".to_owned())?;
    // Parameter sets only exist for H.264 tracks; their absence means a
    // codec openh264 cannot read, like HEVC (iPhones) or AV1/VP9.
    if track.sequence_parameter_set().is_err() || track.picture_parameter_set().is_err() {
        let kind = track
            .box_type()
            .map(|kind| kind.to_string())
            .unwrap_or_default();
        let lower = kind.to_ascii_lowercase();
        if lower.contains("hvc") || lower.contains("hev") {
            return Err("This video uses HEVC (often from iPhone), which this app cannot play in-process. Open it in the default app instead.".to_owned());
        }
        if lower.contains("av01") || lower.contains("vp09") || lower.contains("vp08") {
            return Err(format!(
                "This video uses {kind}, which this app cannot play in-process. Open it in the default app instead."
            ));
        }
        return Err(
            "This video uses a codec this app cannot play. Open it in the default app instead."
                .to_owned(),
        );
    }
    // Fragmented files stamp no length in any header; the last sample stamps it.
    let (track_id, timescale, width, height, header_count) = (
        track.track_id(),
        u64::from(track.timescale().max(1)),
        track.width(),
        track.height(),
        track.sample_count(),
    );
    let mut duration = mp4.duration();
    if duration.is_zero() {
        duration = sniff_duration(&mut mp4, track_id, timescale, header_count);
    }
    if duration.is_zero() {
        if mp4.is_fragmented() {
            return Err("This video is fragmented and needs ffmpeg to play in-process. Open it in the default app instead.".to_owned());
        }
        return Err("This video has no readable length.".to_owned());
    }
    let has_audio = mp4
        .tracks()
        .values()
        .any(|track| track.track_type().ok() == Some(mp4::TrackType::Audio));
    Ok(Clip {
        duration,
        width: u32::from(width.max(2)),
        height: u32::from(height.max(2)),
        has_audio,
        ffmpeg: false,
    })
}

/// Length from the last sample, for files whose headers carry none.
///
/// Fragmented files stamp no duration anywhere; one sample read at the end
/// measures the whole track. Absurd counts refuse outright.
fn sniff_duration(
    mp4: &mut mp4::Mp4Reader<BufReader<std::fs::File>>,
    track_id: u32,
    timescale: u64,
    header_count: u32,
) -> Duration {
    // The reader counts whole files; fragments only list per fragment, so the
    // header count backs it up.
    let count = mp4.sample_count(track_id).unwrap_or(0).max(header_count);
    if count == 0 || count > 1_000_000 {
        return Duration::ZERO;
    }
    match mp4.read_sample(track_id, count) {
        Ok(Some(sample)) => stamp(
            sample.start_time.saturating_add(u64::from(sample.duration)),
            0,
            timescale,
        ),
        _ => Duration::ZERO,
    }
}

/// One decoded frame and when it shows, measured from the start.
struct Frame {
    pts: Duration,
    image: ColorImage,
}
/// Reads a file ffmpeg understands when the in-process header cannot.
///
/// Fragmented files and other codecs play through ffmpeg's pipe instead of
/// refusing, as long as ffmpeg is installed.
pub fn probe_ffmpeg(path: &Path) -> Result<Clip, String> {
    if !ffmpeg_present() {
        return Err("This video needs ffmpeg to play, which is not installed. Open it in the default app instead.".to_owned());
    }
    let dims = ffprobe(
        path,
        [
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ],
    )?;
    let mut parts = dims.trim().split(',');
    let width: u32 = parts
        .next()
        .and_then(|part| part.trim().parse().ok())
        .unwrap_or(0);
    let height: u32 = parts
        .next()
        .and_then(|part| part.trim().parse().ok())
        .unwrap_or(0);
    if width == 0 || height == 0 {
        return Err("This video has no readable picture.".to_owned());
    }
    let duration: f64 = ffprobe(path, ["-show_entries", "format=duration", "-of", "csv=p=0"])?
        .trim()
        .parse()
        .map_err(|_| "This video has no readable length.".to_owned())?;
    if !duration.is_finite() || duration <= 0.0 {
        return Err("This video has no readable length.".to_owned());
    }
    let streams = ffprobe(
        path,
        ["-show_entries", "stream=codec_type", "-of", "csv=p=0"],
    )?;
    let has_audio = streams.lines().any(|line| line.trim() == "audio");
    Ok(Clip {
        duration: Duration::from_secs_f64(duration),
        width,
        height,
        has_audio,
        ffmpeg: true,
    })
}
/// Runs ffprobe with extra arguments and returns its standard output.
fn ffprobe<I, S>(path: &Path, args: I) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut probe = std::process::Command::new("ffprobe");
    let output = quiet(&mut probe)
        .args(["-v", "error"])
        .args(args)
        .arg(path)
        .output()
        .map_err(|_| "Cannot run ffprobe.".to_owned())?;
    if !output.status.success() {
        return Err("This file is not a readable video.".to_owned());
    }
    String::from_utf8(output.stdout).map_err(|_| "This file is not a readable video.".to_owned())
}
/// Mono 48 kHz samples extracted up front, played from any offset.
///
/// Some files arrange their metadata so the streaming decoder cannot read
/// the soundtrack at all. Those are decoded once in the background and
/// sliced here, so seeking stays instant after the first wait.
struct MemSamples {
    data: Arc<Vec<f32>>,
    pos: usize,
    total: Duration,
}
impl MemSamples {
    fn from_pcm(pcm: Arc<Vec<f32>>, skip: usize) -> Self {
        let pos = skip.min(pcm.len());
        let total =
            Duration::from_secs_f32((pcm.len().saturating_sub(pos) as f32) / PCM_RATE as f32);
        Self {
            data: pcm,
            pos,
            total,
        }
    }
}
impl Iterator for MemSamples {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        let sample = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(sample)
    }
}
impl rodio::Source for MemSamples {
    fn current_span_len(&self) -> Option<usize> {
        Some((self.data.len() - self.pos).clamp(1, SPAN_SAMPLES))
    }
    fn channels(&self) -> std::num::NonZero<u16> {
        std::num::NonZero::<u16>::MIN
    }
    fn sample_rate(&self) -> std::num::NonZero<u32> {
        std::num::NonZero::new(PCM_RATE).expect("48 kHz is not zero")
    }
    fn total_duration(&self) -> Option<Duration> {
        Some(self.total)
    }
}
/// What one background pass over a downloaded video learns: its real
/// length and a poster worth showing. The phone's metadata and thumbnail
/// stay on screen until this answers.
pub struct VideoAnalysis {
    pub seconds: Option<u32>,
    pub poster: Option<Vec<u8>>,
}

/// Reads a downloaded video's length and builds a poster from its own
/// frames. Zero stays unknown: the bubble omits the length instead of
/// showing a misleading zero.
pub fn analyze(path: &Path) -> VideoAnalysis {
    let seconds = probe(path)
        .or_else(|_| probe_ffmpeg(path))
        .ok()
        .map(|clip| clip.duration.as_secs() as u32)
        .filter(|seconds| *seconds > 0);
    let poster = best_frame(path, PLAY_WIDTH)
        .or_else(|| ffmpeg_poster(path))
        .and_then(encode_poster);
    VideoAnalysis { seconds, poster }
}

/// The first usable picture of a video, for posters.
///
/// A black opening or a transition is skipped in favour of the frames
/// right after it; what cannot be read keeps the phone's thumbnail.
fn best_frame(path: &Path, width: u32) -> Option<image::RgbaImage> {
    let (mut first, mut bright) = (None, None);
    decode_frames(path, width, 60, &mut |image| {
        if first.is_none() {
            first = Some(image.clone());
        }
        if bright.is_none() && mean_luma(&image) > 10.0 {
            bright = Some(image.clone());
        }
        bright.is_some()
    });
    bright.or(first)
}

/// Mean brightness of a frame, from black (0) to white (255).
fn mean_luma(image: &image::RgbaImage) -> f32 {
    if image.as_raw().is_empty() {
        return 0.0;
    }
    let (mut sum, mut count) = (0u64, 0u64);
    for pixel in image.pixels().step_by(7) {
        let [red, green, blue, _] = pixel.0;
        sum += u64::from(red) + u64::from(green) + u64::from(blue);
        count += 1;
    }
    if count == 0 {
        return 0.0;
    }
    sum as f32 / (3.0 * count as f32)
}

/// Decodes up to `limit` pictures from the start, scaled to a width,
/// stopping early when the visitor has seen enough.
fn decode_frames(
    path: &Path,
    width: u32,
    limit: u32,
    visit: &mut dyn FnMut(image::RgbaImage) -> bool,
) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let Ok(size) = file.metadata().map(|meta| meta.len()) else {
        return;
    };
    let Ok(mut mp4) = mp4::Mp4Reader::read_header(BufReader::new(file), size) else {
        return;
    };
    let Some(track) = mp4
        .tracks()
        .values()
        .find(|track| track.track_type().ok() == Some(mp4::TrackType::Video))
    else {
        return;
    };
    // Fragmented files list their samples per fragment; the header count stays empty.
    let (track_id, count) = (
        track.track_id(),
        mp4.sample_count(track.track_id()).unwrap_or(0),
    );
    let (Ok(sps), Ok(pps)) = (
        track.sequence_parameter_set().map(|bytes| bytes.to_vec()),
        track.picture_parameter_set().map(|bytes| bytes.to_vec()),
    ) else {
        return;
    };
    let Ok(mut decoder) = openh264::decoder::Decoder::new() else {
        return;
    };
    let mut parameters = Vec::new();
    push_annex_b(&mut parameters, &sps);
    push_annex_b(&mut parameters, &pps);
    let _ = decoder.decode(&parameters);
    for sample_id in 1..=count.min(limit) {
        let Ok(Some(sample)) = mp4.read_sample(track_id, sample_id) else {
            continue;
        };
        let mut annex_b = Vec::with_capacity(sample.bytes.len() + 16);
        avcc_to_annex_b(&mut annex_b, &sample.bytes);
        let Ok(Some(yuv)) = decoder.decode(&annex_b) else {
            continue;
        };
        use openh264::formats::YUVSource;
        let (w, h) = yuv.dimensions();
        if w == 0 || h == 0 {
            continue;
        }
        let mut rgba = vec![0u8; w * h * 4];
        yuv.write_rgba8(&mut rgba);
        let Some(image) = image::RgbaImage::from_raw(w as u32, h as u32, rgba) else {
            continue;
        };
        let out_width = (w as u32).min(width).max(2);
        let out_height = ((h as u64 * u64::from(out_width) / w as u64) as u32).max(2);
        let image = if out_width == w as u32 {
            image
        } else {
            image::imageops::resize(
                &image,
                out_width,
                out_height,
                image::imageops::FilterType::Triangle,
            )
        };
        if visit(image) {
            break;
        }
    }
}

/// Encodes a poster frame as JPEG bytes for the archive.
fn encode_poster(image: image::RgbaImage) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Jpeg,
        )
        .ok()?;
    Some(bytes)
}
/// One still through ffmpeg, for files the in-process decoder cannot open.
fn ffmpeg_poster(path: &Path) -> Option<image::RgbaImage> {
    if !ffmpeg_present() {
        return None;
    }
    let mut poster = std::process::Command::new("ffmpeg");
    let output = quiet(&mut poster)
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "pipe:1",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let image = image::load_from_memory(&output.stdout).ok()?.to_rgba8();
    let out_width = image.width().clamp(2, 320);
    let out_height = ((u64::from(image.height()) * u64::from(out_width)
        / u64::from(image.width().max(1))) as u32)
        .max(2);
    Some(if out_width == image.width() {
        image
    } else {
        image::imageops::resize(
            &image,
            out_width,
            out_height,
            image::imageops::FilterType::Triangle,
        )
    })
}
struct Active {
    path: PathBuf,
    clip: Clip,
    /// The soundtrack. The device is held so the stream stays open; only the
    /// sink is read, which is why this is a tuple and not a named struct.
    audio: Option<(rodio::MixerDeviceSink, rodio::Player)>,
    /// A soundtrack decoded up front, sliced instantly on later jumps.
    pcm: Option<Arc<Vec<f32>>>,
    /// A soundtrack still being decoded in the background.
    audio_rx: Option<std::sync::mpsc::Receiver<Vec<f32>>>,
    /// A soundtrack opening in the background after a jump: streaming
    /// sound first, a full extraction when seeking defeats it.
    audio_task: Option<(u64, std::sync::mpsc::Receiver<SeekAudio>)>,
    playing: bool,
    /// Position when playback last (re)started; the sink or the wall clock
    /// counts from there.
    base: Duration,
    /// Sink position when base was established. The sink keeps its own
    /// count across a pause, so resuming from base plus position alone would
    /// count the stretch before the pause twice and jump the picture ahead.
    anchor: Duration,
    started: Instant,
    /// A jump is still catching up: the keyframe still shows until live frames arrive.
    seeking: bool,
    frames: Receiver<Frame>,
    buffered: VecDeque<Frame>,
    /// One arrival that did not fit, kept for the next tick. The channel
    /// cannot take it back, so without this slot a full buffer would drop
    /// one frame per tick.
    held: Option<Frame>,
    shown: Duration,
    /// One texture for the whole clip, updated in place. Allocating a fresh
    /// texture per frame churned GPU memory for nothing, and the handle has
    /// to survive a seek because the picture size never changes.
    texture: Option<TextureHandle>,
    generation: Arc<AtomicU64>,
    decode_done: bool,
    finished: bool,
}

/// Plays the video open in the viewer. Only one plays at a time.
pub struct Player {
    active: Option<Active>,
    /// The file the viewer already refused, and why. The view reads this so
    /// opening an unplayable file complains once instead of every frame.
    refused: Option<(PathBuf, String)>,
    volume: f32,
    muted: bool,
}
impl Default for Player {
    fn default() -> Self {
        Self {
            active: None,
            refused: None,
            volume: 1.0,
            muted: false,
        }
    }
}

/// What the viewer paints for the open video.
pub enum State {
    /// The decoder thread is still on its way to the first frame.
    Loading,
    /// A frame to paint, with where playback stands.
    Showing {
        texture: TextureHandle,
        size: Vec2,
        position: Duration,
        total: Duration,
        playing: bool,
        finished: bool,
        /// A jump is catching up: the keyframe still shows, not a scan.
        seeking: bool,
    },
    /// The file cannot play in-process; the viewer offers the system player.
    Unsupported(String),
}

impl Player {
    /// Plays or pauses the open video. A new file starts from the beginning,
    /// after anything else making sound has stopped.
    pub fn toggle(&mut self, path: &Path, stop_audio: &mut dyn FnMut()) -> Result<(), String> {
        if let Some(active) = self.active.as_mut()
            && active.path == path
        {
            if active.playing {
                active.base = active.position();
                if let Some((_, sink)) = &active.audio {
                    active.anchor = sink.get_pos();
                    sink.pause();
                }
                active.playing = false;
            } else {
                active.finished = false;
                if active.base >= active.clip.duration {
                    // Replaying from the end always starts playing: a
                    // finished clip holds no paused position worth keeping,
                    // and a bare seek would preserve the pause.
                    self.seek(path, 0.0)?;
                    if let Some(active) = self.active.as_mut()
                        && active.path == path
                    {
                        active.started = Instant::now();
                        active.playing = true;
                        if let Some((_, sink)) = &active.audio {
                            sink.play();
                        }
                    }
                    return Ok(());
                }
                // Sound cached while paused joins from where it stopped.
                if active.audio.is_none() && active.pcm.is_some() {
                    let base = active.base;
                    let (volume, muted) = (self.volume, self.muted);
                    attach_cached(active, volume, muted, base);
                }
                if let Some((_, sink)) = &active.audio {
                    sink.play();
                }
                active.started = Instant::now();
                active.playing = true;
            }
            return Ok(());
        }
        stop_audio();
        self.open(path, Duration::ZERO)
    }

    /// Jumps to a fraction of the clip, from 0 to 1, and keeps playing.
    pub fn seek(&mut self, path: &Path, fraction: f32) -> Result<(), String> {
        let total = match self.active.as_ref() {
            Some(active) if active.path == path => active.clip.duration,
            _ => return self.open(path, Duration::ZERO),
        };
        let target = total.mul_f32(fraction.clamp(0.0, 1.0));
        // Cached sound makes the jump instant; otherwise the file reopens.
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.pcm.is_some())
        {
            let (volume, muted) = (self.volume, self.muted);
            let active = self.active.as_mut().expect("just checked");
            // A new pass over the same counter stands the old thread down.
            active.generation.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = sync_channel::<Frame>(BUFFER_FRAMES);
            active.frames = rx;
            active.buffered.clear();
            // Frames from the retired generation never come back.
            active.held = None;
            // The clip keeps its texture; a sentinel timestamp forces the
            // next decoded frame to upload over the still on screen.
            active.shown = Duration::MAX;
            active.decode_done = false;
            active.finished = false;
            active.seeking = !target.is_zero();
            spawn_decode(
                active.path.clone(),
                target,
                active.clip.duration,
                active.clip.ffmpeg,
                active.clip.width,
                active.clip.height,
                active.generation.clone(),
                tx,
            );
            attach_cached(active, volume, muted, target);
            return Ok(());
        }
        // Without cached sound the jump retargets in place: the headers
        // stay read, the still stays on screen, and the clock holds the
        // target until live frames arrive. Reopening here would reread
        // everything on the interface thread and restart picture and
        // sound apart.
        let active = self.active.as_mut().expect("same path checked");
        let duration = active.clip.duration;
        let target = target.min(duration);
        // A new pass over the same counter stands down the old decode,
        // the old extraction and the old soundtrack task together: only
        // the newest jump may answer.
        active.generation.fetch_add(1, Ordering::SeqCst);
        let current = active.generation.load(Ordering::SeqCst);
        let (tx, rx) = sync_channel::<Frame>(BUFFER_FRAMES);
        active.frames = rx;
        active.buffered.clear();
        // Frames from the retired generation never come back.
        active.held = None;
        active.decode_done = false;
        active.finished = false;
        active.seeking = !target.is_zero();
        active.base = target;
        active.anchor = Duration::ZERO;
        active.started = Instant::now();
        // The old soundtrack belongs to the old position.
        active.audio = None;
        active.audio_rx = None;
        active.audio_task = None;
        let (generation, file, clip) = (
            active.generation.clone(),
            active.path.clone(),
            active.clip.clone(),
        );
        spawn_decode(
            file.clone(),
            target,
            duration,
            clip.ffmpeg,
            clip.width,
            clip.height,
            generation.clone(),
            tx,
        );
        let (atx, arx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("video-seek-audio".into())
            .spawn(move || {
                let _ = atx.send(open_seek_audio(&file, target, &generation, current));
            })
            .ok();
        active.audio_task = Some((current, arx));
        Ok(())
    }

    /// Opens a file at a position, replacing whatever was playing.
    fn open(&mut self, path: &Path, at: Duration) -> Result<(), String> {
        self.stop();
        // Fragmented files and other codecs fall back to ffmpeg instead of refusing.
        let clip = match probe(path) {
            Ok(clip) => {
                if self
                    .refused
                    .as_ref()
                    .is_some_and(|(known, _)| known == path)
                {
                    self.refused = None;
                }
                clip
            }
            // The in-process header failed; ffmpeg gets its chance before refusing.
            Err(_) => match probe_ffmpeg(path) {
                Ok(clip) => {
                    if self
                        .refused
                        .as_ref()
                        .is_some_and(|(known, _)| known == path)
                    {
                        self.refused = None;
                    }
                    clip
                }
                Err(error) => {
                    self.refused = Some((path.to_path_buf(), error.clone()));
                    return Err(error);
                }
            },
        };
        let at = at.min(clip.duration);
        // The counter exists before the soundtrack does, so a jump that
        // lands while it still opens retires this extraction as well.
        let generation = Arc::new(AtomicU64::new(1));
        let mut audio_rx = None;
        let (audio, at) = match audio_at(path, at, &generation, 1) {
            Audio::Sound(audio) => (Some(audio), at),
            Audio::Extracting(rx) => {
                audio_rx = Some(rx);
                (None, at)
            }
            Audio::Silent => (None, at),
        };
        // A jump opens on its keyframe still and resumes at the target once
        // live frames arrive; opening at zero plays straight away.
        let seeking = !at.is_zero();
        let (tx, rx) = sync_channel::<Frame>(BUFFER_FRAMES);
        spawn_decode(
            path.to_path_buf(),
            at,
            clip.duration,
            clip.ffmpeg,
            clip.width,
            clip.height,
            generation.clone(),
            tx,
        );
        if let Some((_, sink)) = &audio {
            sink.set_volume(if self.muted { 0.0 } else { self.volume });
            sink.play();
        }
        self.active = Some(Active {
            path: path.to_path_buf(),
            clip,
            audio,
            pcm: None,
            audio_rx,
            audio_task: None,
            playing: true,
            base: at,
            anchor: Duration::ZERO,
            started: Instant::now(),
            frames: rx,
            buffered: VecDeque::new(),
            // Nothing has shown yet; a first frame stamped at zero must still
            // upload instead of looking already painted.
            shown: Duration::MAX,
            held: None,
            texture: None,
            generation,
            decode_done: false,
            seeking,
            finished: false,
        });
        Ok(())
    }

    /// Stops playback and drops the soundtrack. The decode thread stands
    /// down on its own once it sees its generation is old.
    pub fn stop(&mut self) {
        if let Some(active) = self.active.take() {
            active.generation.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Whether this file is the one playing right now.
    pub fn is_active(&self, path: &Path) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.path == path)
    }

    /// Why this file refused to play, if it already did.
    pub fn refusal(&self, path: &Path) -> Option<String> {
        self.refused
            .as_ref()
            .filter(|(known, _)| known == path)
            .map(|(_, why)| why.clone())
    }
    /// Remembers the output level for the next file and applies it to the
    /// one playing now. The view calls this every frame, so playback
    /// follows the slider without reopening anything.
    pub fn set_output(&mut self, volume: f32, muted: bool) {
        let volume = volume.clamp(0.0, 1.0);
        if self.volume == volume && self.muted == muted {
            return;
        }
        self.volume = volume;
        self.muted = muted;
        if let Some(active) = self.active.as_mut()
            && let Some((_, sink)) = &active.audio
        {
            sink.set_volume(if muted { 0.0 } else { volume });
        }
    }

    /// Pumps decoded frames and answers what the viewer should paint.
    pub fn poll(&mut self, ctx: &egui::Context, path: &Path) -> State {
        let Some(active) = self.active.as_mut() else {
            // A refused file runs no decoder: say why instead of spinning on
            // a player that will never arrive.
            if let Some(why) = self.refusal(path) {
                return State::Unsupported(why);
            }
            return State::Loading;
        };
        if active.path != path {
            return State::Loading;
        }
        // A background extraction joins wherever playback stands. Grabbing
        // the level first keeps one mutable borrow.
        let (volume, muted) = (self.volume, self.muted);
        if active.audio.is_none() {
            let mut arrived = None;
            if let Some(rx) = active.audio_rx.as_ref() {
                arrived = rx.try_recv().ok();
            }
            if let Some(pcm) = arrived {
                active.audio_rx = None;
                if !pcm.is_empty() {
                    active.pcm = Some(Arc::new(pcm));
                    let at = active.position();
                    attach_cached(active, volume, muted, at);
                }
            }
        }
        // A jump's soundtrack opens beside it. Only the newest jump may
        // answer; an older task's delivery dies with its generation.
        if active.audio.is_none() && active.audio_rx.is_none() {
            let mut outcome = None;
            if let Some((current, rx)) = active.audio_task.as_ref() {
                let current = *current;
                if let Ok(answer) = rx.try_recv() {
                    active.audio_task = None;
                    if current == active.generation.load(Ordering::SeqCst) {
                        outcome = Some(answer);
                    }
                }
            }
            match outcome {
                Some(SeekAudio::Stream((device, sink))) => {
                    sink.set_volume(if muted { 0.0 } else { volume });
                    let at = active.position();
                    active.audio = Some((device, sink));
                    active.base = at;
                    active.anchor = Duration::ZERO;
                    active.started = Instant::now();
                    if !active.playing {
                        if let Some((_, sink)) = &active.audio {
                            sink.pause();
                        }
                    } else if let Some((_, sink)) = &active.audio {
                        sink.play();
                    }
                }
                Some(SeekAudio::Extracting(rx)) => {
                    active.audio_rx = Some(rx);
                }
                Some(SeekAudio::Silent) | None => {}
            }
        }
        // The held arrival goes first: it never went back to the channel.
        if let Some(frame) = active.held.take() {
            active.place_frame(frame);
        }
        // Drain only while nothing is held: whatever does not fit stays
        // queued in the channel, and the decoder waits on it. Nothing is
        // ever taken just to be dropped.
        while active.held.is_none() {
            match active.frames.try_recv() {
                Ok(frame) => active.place_frame(frame),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    active.decode_done = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }
        let position = active.position();
        // Drop what is well behind, but keep the frame on screen. While a jump
        // catches up every frame stays, keyframe still included.
        if !active.seeking {
            while active.buffered.len() > 1 && active.buffered[1].pts + KEEP_BEHIND < position {
                active.buffered.pop_front();
            }
        }
        let total = active.clip.duration;
        if position >= total {
            active.playing = false;
            active.finished = true;
            active.base = total;
            if let Some((_, sink)) = &active.audio {
                active.anchor = sink.get_pos();
                sink.pause();
            }
        }
        if active.audio.as_ref().is_some_and(|(_, sink)| sink.empty())
            && position < total
            && !active.finished
        {
            // The soundtrack ended before the picture (a short track, a
            // capped extraction): the wall clock drives on instead of
            // freezing the picture on the silent sink.
            active.base = position;
            active.anchor = Duration::ZERO;
            active.started = Instant::now();
            active.audio = None;
        }
        if active.seeking {
            // A jump shows its keyframe still, never a scan.
            let live = active.base.saturating_sub(Duration::from_millis(80));
            match active
                .buffered
                .iter()
                .find(|frame| frame.pts >= live)
                .map(|frame| frame.pts)
            {
                Some(pts) => {
                    active.seeking = false;
                    // Live picture and clock resume together from the
                    // target: without this the sound would start ahead by
                    // however long the jump took to decode.
                    active.started = Instant::now();
                    if let Some((_, sink)) = &active.audio {
                        active.anchor = sink.get_pos();
                    }
                    show_frame(active, ctx, path, pts, position, total)
                }
                None => {
                    // Work is in flight whether paused or playing: the
                    // window must wake when the jump lands, with no mouse
                    // needed.
                    ctx.request_repaint_after(Duration::from_millis(100));
                    match active.buffered.front().map(|frame| frame.pts) {
                        Some(pts) => show_frame(active, ctx, path, pts, position, total),
                        None if active.decode_done => {
                            active.playing = false;
                            active.seeking = false;
                            if let Some((_, sink)) = &active.audio {
                                sink.pause();
                            }
                            State::Unsupported("This video could not be decoded.".to_owned())
                        }
                        None => State::Loading,
                    }
                }
            }
        } else {
            let pts = choose_pts(
                active.buffered.iter().map(|frame| frame.pts),
                position,
                active.shown,
            );
            match pts {
                Some(pts) => show_frame(active, ctx, path, pts, position, total),
                None if active.decode_done => {
                    active.playing = false;
                    if let Some((_, sink)) = &active.audio {
                        sink.pause();
                    }
                    State::Unsupported("This video could not be decoded.".to_owned())
                }
                None => {
                    if active.playing {
                        ctx.request_repaint_after(Duration::from_millis(100));
                    }
                    State::Loading
                }
            }
        }
    }
}
/// Uploads one frame and answers what the viewer paints for it.
fn show_frame(
    active: &mut Active,
    ctx: &egui::Context,
    path: &Path,
    pts: Duration,
    position: Duration,
    total: Duration,
) -> State {
    // The picture between two decode steps is the same one, so only a new
    // presentation time touches the GPU, and it lands in the clip's own
    // texture instead of a new one.
    if active.shown != pts
        && let Some(frame) = active.buffered.iter().find(|frame| frame.pts == pts)
    {
        let image = frame.image.clone();
        match active.texture.as_mut() {
            Some(handle) => handle.set(image, TextureOptions::LINEAR),
            None => {
                active.texture = Some(ctx.load_texture(
                    format!("video-{}", path.display()),
                    image,
                    TextureOptions::LINEAR,
                ));
            }
        }
        active.shown = pts;
    }
    let Some(handle) = active.texture.as_ref() else {
        return State::Loading;
    };
    let (texture, size) = (handle.clone(), handle.size_vec2());
    if active.playing {
        ctx.request_repaint_after(next_repaint(active, position));
    }
    State::Showing {
        texture,
        size,
        position: position.min(total),
        total,
        playing: active.playing,
        finished: active.finished,
        seeking: active.seeking,
    }
}

impl Active {
    fn position(&self) -> Duration {
        // Paused or still catching a jump, the clock holds its base: the
        // picture and the sound resume together once live frames arrive,
        // instead of the sound running ahead of a picture still decoding.
        if !self.playing || self.seeking {
            return self.base;
        }
        match &self.audio {
            Some((_, sink)) => audio_position(self.base, sink.get_pos(), self.anchor),
            None => self.base + self.started.elapsed(),
        }
    }

    /// Files one arrival into the buffer, or holds it for the next tick
    /// when only future frames fill the buffer. The channel cannot take a
    /// frame back, so holding is what keeps a full queue lossless.
    fn place_frame(&mut self, frame: Frame) {
        let room = buffer_room(
            self.buffered.len(),
            self.buffered.front().map(|frame| frame.pts),
            self.position(),
        );
        match room {
            BufferRoom::Push => self.buffered.push_back(frame),
            BufferRoom::EvictThenPush => {
                self.buffered.pop_front();
                self.buffered.push_back(frame);
            }
            BufferRoom::Hold => {
                self.held = Some(frame);
            }
        }
    }
}

/// Media position over a live soundtrack. The sink keeps its own count
/// across a pause, so the anchor taken when the base was established is
/// subtracted first: without it the stretch before the pause counts twice.
fn audio_position(base: Duration, sink_pos: Duration, anchor: Duration) -> Duration {
    base + sink_pos.saturating_sub(anchor)
}

/// Presentation time to paint: the newest buffered frame due at position.
/// When none is due yet the current picture holds instead of flashing a
/// future frame early; None means nothing has shown and the poster or the
/// spinner stays up.
fn choose_pts(
    times: impl Iterator<Item = Duration> + Clone,
    position: Duration,
    shown: Duration,
) -> Option<Duration> {
    let mut due = None;
    for pts in times.clone() {
        if pts <= position {
            due = Some(pts);
        }
    }
    if due.is_some() {
        return due;
    }
    for pts in times {
        if pts == shown {
            return Some(shown);
        }
    }
    None
}

/// What a full frame buffer does with one more arrival. The buffer only
/// sheds frames already behind playback, so a buffer full of future frames
/// holds the arrival back (it stays queued, the decoder waits) instead of
/// dropping the future and freezing or jumping the picture later.
enum BufferRoom {
    Push,
    EvictThenPush,
    Hold,
}

fn buffer_room(len: usize, front: Option<Duration>, position: Duration) -> BufferRoom {
    if len < BUFFER_FRAMES {
        return BufferRoom::Push;
    }
    if len > 1 && front.is_some_and(|pts| pts + KEEP_BEHIND < position) {
        return BufferRoom::EvictThenPush;
    }
    BufferRoom::Hold
}

/// How soon the viewer needs another frame.
fn next_repaint(active: &Active, position: Duration) -> Duration {
    active
        .buffered
        .iter()
        .find(|frame| frame.pts > position)
        .map(|frame| frame.pts - position)
        .unwrap_or(Duration::from_millis(100))
        .clamp(Duration::from_millis(10), Duration::from_millis(500))
}

/// The soundtrack from a position, or silence when the file has none.
/// What opening the soundtrack at a position gives.
enum Audio {
    /// Sound from the asked position.
    Sound((rodio::MixerDeviceSink, rodio::Player)),
    /// Sound on its way: ffmpeg decodes in the background while the picture
    /// plays muted, joining wherever playback stands when it arrives.
    Extracting(std::sync::mpsc::Receiver<Vec<f32>>),
    /// No soundtrack to play; the wall clock drives the picture.
    Silent,
}
/// What a jump's background soundtrack task answers. Only the newest jump
/// may apply it; older tasks die with their generation.
enum SeekAudio {
    /// Streaming sound opened at the target, still paused.
    Stream((rodio::MixerDeviceSink, rodio::Player)),
    /// A full extraction on its way; it joins at the live position.
    Extracting(std::sync::mpsc::Receiver<Vec<f32>>),
    /// No soundtrack to play; the wall clock drives the picture.
    Silent,
}
/// The soundtrack from a position, or why it starts elsewhere.
fn audio_at(path: &Path, at: Duration, generation: &Arc<AtomicU64>, current: u64) -> Audio {
    use rodio::Source;
    let Some(file) = std::fs::File::open(path).ok() else {
        return Audio::Silent;
    };
    let mut decoder = match rodio::Decoder::new(BufReader::new(file)) {
        Ok(decoder) => decoder,
        // Some layouts (trailing metadata, odd streams) defeat the streaming
        // decoder. Those fall back to a background extraction instead of
        // going quiet, and the reason is logged for the bug report.
        Err(error) => {
            log::warn!(
                "soundtrack not streaming from {}: {error:?}; extracting",
                path.display()
            );
            return extract_audio(path, generation.clone(), current);
        }
    };
    if !at.is_zero() && decoder.try_seek(at).is_err() {
        // The streaming decoder cannot land on the jump: the picture keeps
        // its target and the soundtrack joins from a background extraction
        // instead of silently restarting the clip from zero.
        log::warn!("soundtrack cannot seek in {}; extracting", path.display());
        return extract_audio(path, generation.clone(), current);
    }
    // The player opens a fresh sink on every seek, so rodio's drop notice
    // would print on each one; the sink is dropped on purpose here.
    let Some(mut device) = rodio::DeviceSinkBuilder::open_default_sink().ok() else {
        return Audio::Silent;
    };
    device.log_on_drop(false);
    let sink = rodio::Player::connect_new(device.mixer());
    sink.append(decoder);
    sink.pause();
    Audio::Sound((device, sink))
}
/// Opens streaming sound at a jump target off the interface thread. Only
/// the newest jump may apply the answer; anything older is refused before
/// it can move the clock.
fn open_seek_audio(
    path: &Path,
    at: Duration,
    generation: &Arc<AtomicU64>,
    current: u64,
) -> SeekAudio {
    use rodio::Source;
    let alive = || generation.load(Ordering::SeqCst) == current;
    let Some(file) = std::fs::File::open(path).ok() else {
        return SeekAudio::Silent;
    };
    let mut decoder = match rodio::Decoder::new(BufReader::new(file)) {
        Ok(decoder) => decoder,
        Err(error) => {
            log::warn!(
                "soundtrack not streaming from {}: {error:?}; extracting",
                path.display()
            );
            return SeekAudio::Extracting(extract_rx(path, generation.clone(), current));
        }
    };
    if !at.is_zero() && decoder.try_seek(at).is_err() {
        log::warn!("soundtrack cannot seek in {}; extracting", path.display());
        return SeekAudio::Extracting(extract_rx(path, generation.clone(), current));
    }
    let Some(mut device) = rodio::DeviceSinkBuilder::open_default_sink().ok() else {
        return SeekAudio::Silent;
    };
    device.log_on_drop(false);
    let sink = rodio::Player::connect_new(device.mixer());
    sink.append(decoder);
    sink.pause();
    if !alive() {
        return SeekAudio::Silent;
    }
    SeekAudio::Stream((device, sink))
}
/// Starts a background extraction of the whole soundtrack, or silence.
/// The thread stands down as soon as a newer jump retires it, instead of
/// decoding a file nobody is watching anymore.
fn extract_rx(
    path: &Path,
    generation: Arc<AtomicU64>,
    current: u64,
) -> std::sync::mpsc::Receiver<Vec<f32>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let path = path.to_path_buf();
    let _ = std::thread::Builder::new()
        .name("video-audio".into())
        .spawn(move || {
            let alive = || generation.load(Ordering::SeqCst) == current;
            // In-process first so sound works without any external binary;
            // ffmpeg stays only as a last resort for exotic codecs.
            if let Some(pcm) = symphonia_pcm(&path, &alive).or_else(|| ffmpeg_pcm(&path, &alive))
                && !pcm.is_empty()
                && alive()
            {
                let _ = tx.send(pcm);
            } else if !alive() {
                log::debug!("soundtrack extraction retired for {}", path.display());
            } else {
                log::warn!("soundtrack not decodable from {}", path.display());
            }
        });
    rx
}
/// Starts a background extraction of the whole soundtrack, or silence.
fn extract_audio(path: &Path, generation: Arc<AtomicU64>, current: u64) -> Audio {
    let (tx, rx) = std::sync::mpsc::channel();
    let path = path.to_path_buf();
    let _ = std::thread::Builder::new()
        .name("video-audio".into())
        .spawn(move || {
            let alive = || generation.load(Ordering::SeqCst) == current;
            // In-process first so sound works without any external binary;
            // ffmpeg stays only as a last resort for exotic codecs.
            if let Some(pcm) = symphonia_pcm(&path, &alive).or_else(|| ffmpeg_pcm(&path, &alive))
                && !pcm.is_empty()
                && alive()
            {
                let _ = tx.send(pcm);
            } else if !alive() {
                log::debug!("soundtrack extraction retired for {}", path.display());
            } else {
                log::warn!("soundtrack not decodable from {}", path.display());
            }
        });
    Audio::Extracting(rx)
}
/// Whether ffmpeg can extract what the streaming decoder cannot.
fn ffmpeg_present() -> bool {
    static KNOWN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *KNOWN.get_or_init(|| {
        let mut check = std::process::Command::new("ffmpeg");
        quiet(&mut check)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// A helper media process that never flashes a console window on Windows.
fn quiet(command: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: probing a chat video must not pop a console.
        command.creation_flags(0x0800_0000);
    }
    command
}
/// Decodes a soundtrack to mono 48 kHz samples, capped for chat videos.
/// Decodes a soundtrack in-process with symphonia, so sound works without
/// any external binary. Resamples to mono 48 kHz with a linear pass and
/// caps at five minutes for chat videos. Returns `None` when the file has
/// no decodable audio track.
/// The thread stands down as soon as a newer jump retires it.
fn symphonia_pcm(path: &Path, alive: &dyn Fn() -> bool) -> Option<Vec<f32>> {
    let file = std::fs::File::open(path).ok()?;
    let source = symphonia::core::io::MediaSourceStream::new(
        Box::new(file) as Box<dyn symphonia::core::io::MediaSource>,
        Default::default(),
    );
    let probe = symphonia::default::get_probe();
    let mut probed = probe
        .format(
            &Default::default(),
            source,
            &Default::default(),
            &Default::default(),
        )
        .ok()?;
    let track = probed
        .format
        .tracks()
        .iter()
        .find(|track| {
            track.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL
                && matches!(
                    track.codec_params.codec,
                    symphonia::core::codecs::CODEC_TYPE_AAC
                        | symphonia::core::codecs::CODEC_TYPE_MP3
                        | symphonia::core::codecs::CODEC_TYPE_VORBIS
                        | symphonia::core::codecs::CODEC_TYPE_PCM_S16LE
                        | symphonia::core::codecs::CODEC_TYPE_PCM_S24LE
                        | symphonia::core::codecs::CODEC_TYPE_PCM_S32LE
                        | symphonia::core::codecs::CODEC_TYPE_PCM_F32LE
                        | symphonia::core::codecs::CODEC_TYPE_PCM_F64LE
                )
        })
        .or_else(|| {
            probed.format.default_track().filter(|track| {
                track.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL
            })
        })?;
    let (track_id, rate) = (track.id, track.codec_params.sample_rate.unwrap_or(PCM_RATE));
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &Default::default())
        .ok()?;
    let cap = PCM_RATE as usize * PCM_CAP_SECS as usize;
    let mut mono = Vec::new();
    let mut capped = false;
    loop {
        if !alive() {
            return None;
        }
        if mono.len() >= cap {
            capped = true;
            break;
        }
        let packet = match probed.format.next_packet() {
            Ok(packet) => packet,
            Err(_) => break,
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(_) => continue,
        };
        append_mono(&mut mono, &decoded, cap);
    }
    if mono.is_empty() {
        return None;
    }
    if capped {
        // Chat videos longer than the cap keep their picture; the clock
        // falls back to the wall clock once this sound runs out.
        log::warn!("soundtrack of {} capped at {PCM_CAP_SECS}s", path.display());
    }
    // Resample only when the source rate differs; a linear pass is plenty
    // for a chat soundtrack and keeps this dependency-free.
    if rate == PCM_RATE {
        return Some(mono);
    }
    Some(resample_linear(&mono, rate, PCM_RATE))
}
/// Appends one decoded audio buffer as mono f32 samples, capped.
fn append_mono(out: &mut Vec<f32>, decoded: &symphonia::core::audio::AudioBufferRef, cap: usize) {
    use symphonia::core::audio::{AudioBuffer, Signal};
    if out.len() >= cap || decoded.frames() == 0 {
        return;
    }
    let spec = *decoded.spec();
    let mut float = AudioBuffer::<f32>::new(decoded.frames() as u64, spec);
    decoded.convert(&mut float);
    let channels = spec.channels.count().max(1);
    for i in 0..decoded.frames() {
        if out.len() >= cap {
            break;
        }
        let mut sum = 0.0f32;
        for channel in 0..channels {
            sum += float.chan(channel)[i];
        }
        out.push(sum / channels as f32);
    }
}
/// Linear resampling between sample rates, for chat soundtracks.
fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if input.is_empty() || from == 0 || to == 0 || from == to {
        return input.to_vec();
    }
    let duration = input.len() as f64 / f64::from(from);
    let mut output = Vec::with_capacity((duration * f64::from(to)) as usize);
    let cap = PCM_RATE as usize * PCM_CAP_SECS as usize;
    let len = ((duration * f64::from(to)) as usize).min(cap);
    for i in 0..len {
        let pos = i as f64 * f64::from(from) / f64::from(to);
        let base = pos.floor() as usize;
        let frac = (pos - base as f64) as f32;
        let first = *input.get(base).unwrap_or(&0.0);
        let second = *input.get(base + 1).unwrap_or(&first);
        output.push(first + (second - first) * frac);
    }
    output
}
fn ffmpeg_pcm(path: &Path, alive: &dyn Fn() -> bool) -> Option<Vec<f32>> {
    use std::io::Read;
    let mut launch = std::process::Command::new("ffmpeg");
    let mut child = quiet(&mut launch)
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-vn", "-ar", "48000", "-ac", "1", "-f", "f32le", "pipe:1"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        // No pipe to read: stop the helper instead of leaving it running.
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    // Streamed, so a long file never sits whole in RAM: past the cap the
    // helper is stopped instead of decoding the tail for nothing.
    let cap = PCM_RATE as usize * PCM_CAP_SECS as usize;
    let mut pcm = Vec::new();
    // A pipe read can split a four-byte sample anywhere; the tail waits
    // for the next read instead of being dropped and misaligning the rest.
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 32_768];
    loop {
        if !alive() {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        if pcm.len() >= cap {
            break;
        }
        let read = match stdout.read(&mut chunk) {
            Ok(read) => read,
            Err(_) => break,
        };
        if read == 0 {
            break;
        }
        push_pcm(&mut pcm, &mut pending, &chunk[..read]);
        if pcm.len() >= cap {
            pcm.truncate(cap);
            break;
        }
    }
    let _ = child.kill();
    let status = child.wait().ok()?;
    if !status.success() && pcm.is_empty() {
        return None;
    }
    (!pcm.is_empty()).then_some(pcm)
}

/// Pushes stream bytes into samples, keeping one to three leftover bytes
/// for the next read. Pipe reads split samples anywhere; dropping the tail
/// would misalign every sample after it into noise.
fn push_pcm(pcm: &mut Vec<f32>, pending: &mut Vec<u8>, bytes: &[u8]) {
    pending.extend_from_slice(bytes);
    let samples = pending.len() / 4;
    for chunk in pending[..samples * 4].as_chunks::<4>().0 {
        pcm.push(f32::from_le_bytes(*chunk));
    }
    pending.drain(..samples * 4);
}
/// Plays cached samples from a position on a fresh output, if any.
fn attach_cached(active: &mut Active, volume: f32, muted: bool, from: Duration) {
    let Some(pcm) = active.pcm.clone() else {
        return;
    };
    let skip = (from.as_secs_f32() * PCM_RATE as f32) as usize;
    let Some(mut device) = rodio::DeviceSinkBuilder::open_default_sink().ok() else {
        return;
    };
    device.log_on_drop(false);
    let sink = rodio::Player::connect_new(device.mixer());
    sink.append(MemSamples::from_pcm(pcm, skip));
    sink.set_volume(if muted { 0.0 } else { volume.clamp(0.0, 1.0) });
    if !active.playing {
        sink.pause();
    }
    active.audio = Some((device, sink));
    active.base = from;
    active.anchor = Duration::ZERO;
    active.started = Instant::now();
}

/// Decodes one video track from a position, sending frames with timestamps.
///
/// Decoding restarts at the nearest key frame at or before the position and
/// drops everything earlier, so a seek pays only for the frames between the
/// two. The thread ends when the track does or when a newer generation
/// replaces it.
#[allow(clippy::too_many_arguments)]
fn spawn_decode(
    path: PathBuf,
    at: Duration,
    total: Duration,
    ffmpeg: bool,
    width: u32,
    height: u32,
    generation: Arc<AtomicU64>,
    out: SyncSender<Frame>,
) {
    let current = generation.load(Ordering::SeqCst);
    let alive = move || generation.load(Ordering::SeqCst) == current;
    let _ = std::thread::Builder::new()
        .name("video-decode".into())
        .spawn(move || {
            if ffmpeg {
                decode_ffmpeg(&path, at, width, height, &alive, &out);
            } else {
                decode(&path, at, total, &alive, &out);
            }
        });
}
/// Frames per second pulled through the ffmpeg pipe.
/// Thirty keeps motion smooth; the pipe carries small chat frames, so the
/// extra throughput stays well inside what a desktop moves without trying.
const PIPE_FPS: u32 = 30;
/// Pulls frames through ffmpeg for files the in-process decoder cannot read.
///
/// Input seeking starts at the nearest key frame; the viewer holds its still
/// until live frames arrive, exactly like a jump.
fn decode_ffmpeg(
    path: &Path,
    at: Duration,
    width: u32,
    height: u32,
    alive: &dyn Fn() -> bool,
    out: &SyncSender<Frame>,
) {
    let out_width = width.clamp(2, PLAY_WIDTH) & !1;
    let out_height =
        ((u64::from(height) * u64::from(out_width) / u64::from(width.max(1))) as u32).max(2) & !1;
    let mut launch = std::process::Command::new("ffmpeg");
    let mut child = match quiet(&mut launch)
        .args(["-v", "error", "-ss", &at.as_secs_f32().to_string(), "-i"])
        .arg(path)
        .args([
            "-vf",
            &format!("fps={PIPE_FPS},scale={out_width}:{out_height}"),
            "-an",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "pipe:1",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return,
    };
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => return,
    };
    use std::io::Read;
    let frame_bytes = (out_width * out_height * 4) as usize;
    let mut buffer = vec![0u8; frame_bytes];
    let mut index = 0u64;
    loop {
        if !alive() {
            break;
        }
        if stdout.read_exact(&mut buffer).is_err() {
            break;
        }
        let pts = at + Duration::from_secs_f64(index as f64 / f64::from(PIPE_FPS));
        index += 1;
        let image =
            ColorImage::from_rgba_unmultiplied([out_width as usize, out_height as usize], &buffer);
        if send_frame(out, alive, Frame { pts, image }).is_err() {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn decode(
    path: &Path,
    at: Duration,
    total: Duration,
    alive: &dyn Fn() -> bool,
    out: &SyncSender<Frame>,
) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let Ok(size) = file.metadata().map(|meta| meta.len()) else {
        return;
    };
    let Ok(mut mp4) = mp4::Mp4Reader::read_header(BufReader::new(file), size) else {
        return;
    };
    let Some(track) = mp4
        .tracks()
        .values()
        .find(|track| track.track_type().ok() == Some(mp4::TrackType::Video))
    else {
        return;
    };
    let (track_id, timescale) = (track.track_id(), u64::from(track.timescale().max(1)));
    // Fragmented files list their samples per fragment; the header count stays empty.
    let count = mp4.sample_count(track_id).unwrap_or(0);
    let Ok(sps) = track.sequence_parameter_set().map(|bytes| bytes.to_vec()) else {
        return;
    };
    let Ok(pps) = track.picture_parameter_set().map(|bytes| bytes.to_vec()) else {
        return;
    };
    let target = at.min(total);
    let start_sample = first_sample_at(&mut mp4, track_id, timescale, target, total);
    let mut decoder = match openh264::decoder::Decoder::new() {
        Ok(decoder) => decoder,
        Err(_) => return,
    };
    let mut parameters = Vec::new();
    push_annex_b(&mut parameters, &sps);
    push_annex_b(&mut parameters, &pps);
    let _ = decoder.decode(&parameters);
    let mut pending: VecDeque<Duration> = VecDeque::new();
    // The last timestamp handed to the viewer. Flushed frames carry no time
    // of their own, so they inherit this instead of falling back to zero.
    let mut sent = target;
    // Presentation times queue in decode order, which matches the decoder's
    // output order for the baseline encodes WhatsApp sends (no B-frames).
    for sample_id in start_sample..=count {
        if !alive() {
            return;
        }
        let Ok(Some(sample)) = mp4.read_sample(track_id, sample_id) else {
            break;
        };
        let pts = stamp(sample.start_time, sample.rendering_offset, timescale);
        pending.push_back(pts);
        let mut annex_b = Vec::with_capacity(sample.bytes.len() + 16);
        avcc_to_annex_b(&mut annex_b, &sample.bytes);
        let decoded = decoder.decode(&annex_b).ok().flatten();
        // Frames delayed by the decoder keep their presentation order.
        if let Some(yuv) = decoded
            && let Some(delay) = pending.pop_front()
            && let Some(frame) = frame_of(&yuv, delay)
            && delay >= target.saturating_sub(Duration::from_millis(80))
        {
            sent = sent.max(delay);
            if send_frame(out, alive, frame).is_err() {
                return;
            }
        }
    }
    if !alive() {
        return;
    }
    if let Ok(rest) = decoder.flush_remaining() {
        for yuv in &rest {
            if !alive() {
                return;
            }
            let delay = pending.pop_front().unwrap_or(sent);
            if let Some(frame) = frame_of(yuv, delay)
                && delay >= target.saturating_sub(Duration::from_millis(80))
                && send_frame(out, alive, frame).is_err()
            {
                return;
            }
        }
    }
}

/// Samples a seek may search, either side of its estimate. A chat encode
/// puts a key frame every couple of seconds, so a few hundred samples cover
/// it several times over.
const SEEK_WINDOW: u32 = 400;
/// The 1-based key frame a seek should start from, in a table of start time
/// and key-frame flag pairs in time order: the last one at or before the
/// target. `None` when the table holds none at or before it.
///
/// A decode has to begin on a key frame: starting on a delta frame feeds the
/// decoder pictures it cannot reconstruct, and the timestamps after it line
/// up wrong, which is what made a jump play as a stutter.
fn pick_keyframe(table: &[(u64, bool)], target_units: u64) -> Option<u32> {
    let mut before = None;
    for (index, entry) in table.iter().enumerate() {
        if !entry.1 {
            continue;
        }
        if entry.0 <= target_units {
            before = Some(index as u32 + 1);
        } else {
            // Past the target already, and time order means nothing later
            // can sit behind it: widen the search instead of opening ahead
            // of the target, where the frames to show could never decode.
            return before;
        }
    }
    before
}
/// Reads the samples of one search window, in time order.
///
/// Reads stop at the first key frame past the target: nothing later can
/// change the answer, and every sample read loads its bytes from the file.
fn sample_window(
    mp4: &mut mp4::Mp4Reader<BufReader<std::fs::File>>,
    track_id: u32,
    from: u32,
    span: u32,
    count: u32,
    target_units: u64,
) -> Vec<(u64, bool)> {
    let mut table = Vec::with_capacity(span as usize);
    let last = from.saturating_add(span).min(count);
    let mut sample_id = from.max(1);
    while sample_id <= last {
        let Ok(Some(sample)) = mp4.read_sample(track_id, sample_id) else {
            break;
        };
        table.push((sample.start_time, sample.is_sync));
        if sample.is_sync && sample.start_time > target_units {
            break;
        }
        sample_id += 1;
    }
    table
}
/// The sample to start decoding at: a real key frame at or before the target
/// when one is near, so the decoder enters cleanly without replaying the
/// whole file.
///
/// The estimate comes from where the target sits in the clip, because
/// walking the sample table from the start loads every sample's bytes and
/// made a jump feel like a fast forward. The window widens only for files
/// with an unusually long key-frame interval.
fn first_sample_at(
    mp4: &mut mp4::Mp4Reader<BufReader<std::fs::File>>,
    track_id: u32,
    timescale: u64,
    target: Duration,
    total: Duration,
) -> u32 {
    if target.is_zero() {
        return 1;
    }
    let count = mp4.sample_count(track_id).unwrap_or(0);
    if count == 0 {
        return 1;
    }
    let target_units = (target.as_secs_f64() * timescale.max(1) as f64) as u64;
    let fraction = if total.is_zero() {
        0.0
    } else {
        (target.as_secs_f64() / total.as_secs_f64()).clamp(0.0, 1.0)
    };
    let guess = ((fraction * count as f64) as u32).clamp(1, count);
    for span in [SEEK_WINDOW, SEEK_WINDOW * 3, SEEK_WINDOW * 9] {
        let from = guess.saturating_sub(span).max(1);
        let table = sample_window(mp4, track_id, from, span * 2, count, target_units);
        if let Some(picked) = pick_keyframe(&table, target_units) {
            return from + picked - 1;
        }
    }
    // A track with no key frame anywhere decodes from the top.
    1
}

/// Presentation time of a sample, honouring the composition offset that
/// reorders frames.
fn stamp(start_time: u64, rendering_offset: i32, timescale: u64) -> Duration {
    let units = start_time as i64 + i64::from(rendering_offset);
    Duration::from_secs_f64((units.max(0) as f64) / timescale.max(1) as f64)
}

/// Sends a frame, waiting briefly when the viewer fell behind, and giving up
/// when a newer playback replaced this one.
fn send_frame(
    out: &SyncSender<Frame>,
    alive: &dyn Fn() -> bool,
    mut frame: Frame,
) -> Result<(), ()> {
    loop {
        if !alive() {
            return Err(());
        }
        match out.try_send(frame) {
            Ok(()) => return Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(taken)) => {
                frame = taken;
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return Err(()),
        }
    }
}

fn push_annex_b(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
}

/// Converts length-prefixed AVCC NAL units to Annex B start codes.
fn avcc_to_annex_b(out: &mut Vec<u8>, sample: &[u8]) {
    let mut rest = sample;
    while rest.len() >= 4 {
        let length = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        rest = &rest[4..];
        if length == 0 || length > rest.len() {
            break;
        }
        push_annex_b(out, &rest[..length]);
        rest = &rest[length..];
    }
}

/// Converts and scales one decoded frame.
fn frame_of(yuv: &openh264::decoder::DecodedYUV<'_>, pts: Duration) -> Option<Frame> {
    use openh264::formats::YUVSource;
    let (width, height) = yuv.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    let mut rgba = vec![0u8; width * height * 4];
    yuv.write_rgba8(&mut rgba);
    let image = image::RgbaImage::from_raw(width as u32, height as u32, rgba)?;
    let out_width = (width as u32).min(PLAY_WIDTH);
    let out_height = ((height as u64 * u64::from(out_width) / width as u64) as u32).max(1);
    let scaled = if out_width == width as u32 {
        image
    } else {
        image::imageops::resize(
            &image,
            out_width,
            out_height,
            image::imageops::FilterType::Triangle,
        )
    };
    Some(Frame {
        pts,
        image: ColorImage::from_rgba_unmultiplied(
            [scaled.width() as usize, scaled.height() as usize],
            scaled.as_raw(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_time_honours_the_composition_offset() {
        // A frame stored early but shown late (B-frame reordering).
        assert_eq!(stamp(3000, 2000, 1000), Duration::from_secs(5));
        // A negative offset never goes below zero.
        assert_eq!(stamp(100, -5000, 1000), Duration::ZERO);
        // A zero timescale never divides by zero.
        assert_eq!(stamp(100, 0, 0), Duration::from_secs(100));
    }

    #[test]
    fn pausing_and_resuming_never_counts_a_stretch_twice() {
        // Ten seconds in, the pause bakes the position into the base and
        // freezes the anchor on the sink. Five more output seconds resume
        // from twelve, not from nineteen.
        let paused = audio_position(Duration::ZERO, Duration::from_secs(7), Duration::ZERO);
        assert_eq!(paused, Duration::from_secs(7));
        let anchor = Duration::from_secs(7);
        let resumed = audio_position(paused, Duration::from_secs(12), anchor);
        assert_eq!(resumed, Duration::from_secs(12));
        // Ten quick pause cycles drift by nothing at all.
        let mut base = Duration::ZERO;
        let mut anchor = Duration::ZERO;
        let mut sink = Duration::ZERO;
        for _ in 0..10 {
            sink += Duration::from_secs(1);
            base = audio_position(base, sink, anchor);
            anchor = sink;
        }
        assert_eq!(base, Duration::from_secs(10));
    }

    #[test]
    fn the_picture_holds_instead_of_showing_the_future() {
        fn times(seconds: &[u64]) -> Vec<Duration> {
            seconds.iter().map(|s| Duration::from_secs(*s)).collect()
        }
        // Future frames wait their turn; with nothing shown the poster stays.
        assert_eq!(
            choose_pts(
                times(&[5, 6]).into_iter(),
                Duration::from_secs(3),
                Duration::MAX
            ),
            None
        );
        // Due frames show the newest one at or behind the position.
        assert_eq!(
            choose_pts(
                times(&[5, 6]).into_iter(),
                Duration::from_secs(5),
                Duration::MAX
            ),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            choose_pts(
                times(&[5, 6]).into_iter(),
                Duration::from_secs(7),
                Duration::MAX
            ),
            Some(Duration::from_secs(6))
        );
        // With no frame due, the picture on screen holds.
        assert_eq!(
            choose_pts(
                times(&[5, 6]).into_iter(),
                Duration::from_secs(4),
                Duration::from_secs(5)
            ),
            Some(Duration::from_secs(5))
        );
        // A picture no longer buffered cannot hold: back to the poster.
        assert_eq!(
            choose_pts(
                times(&[5, 6]).into_iter(),
                Duration::from_secs(4),
                Duration::from_secs(2)
            ),
            None
        );
    }

    #[test]
    fn a_full_buffer_of_future_frames_holds_instead_of_dropping() {
        assert!(matches!(
            buffer_room(10, None, Duration::ZERO),
            BufferRoom::Push
        ));
        assert!(matches!(
            buffer_room(BUFFER_FRAMES - 1, None, Duration::ZERO),
            BufferRoom::Push
        ));
        // Full of future frames: hold, so the queued frames survive and
        // the decoder waits on its channel.
        assert!(matches!(
            buffer_room(BUFFER_FRAMES, Some(Duration::from_secs(5)), Duration::ZERO),
            BufferRoom::Hold
        ));
        // A frame well behind playback makes room for the arrival.
        assert!(matches!(
            buffer_room(BUFFER_FRAMES, Some(Duration::ZERO), Duration::from_secs(5)),
            BufferRoom::EvictThenPush
        ));
    }

    #[test]
    fn a_full_queue_loses_no_frame_across_ticks() {
        // A full buffer of future frames plus a loaded channel: every
        // numbered frame must survive repeated ticks, none silently dropped.
        let path = std::path::PathBuf::from("queue.mp4");
        let (tx, rx) = sync_channel::<Frame>(BUFFER_FRAMES);
        let frame = |secs: u64| Frame {
            pts: Duration::from_secs(secs),
            image: ColorImage::new([2, 2], vec![egui::Color32::BLACK; 4]),
        };
        let mut buffered = VecDeque::new();
        for secs in 10..10 + BUFFER_FRAMES as u64 {
            buffered.push_back(frame(secs));
        }
        for secs in 100..105 {
            tx.send(frame(secs)).expect("room in the channel");
        }
        let mut player = Player {
            active: Some(Active {
                path: path.clone(),
                clip: Clip {
                    duration: Duration::from_secs(200),
                    width: 64,
                    height: 64,
                    has_audio: false,
                    ffmpeg: false,
                },
                audio: None,
                pcm: None,
                audio_rx: None,
                audio_task: None,
                playing: true,
                base: Duration::ZERO,
                anchor: Duration::ZERO,
                started: Instant::now(),
                frames: rx,
                buffered,
                held: None,
                shown: Duration::MAX,
                texture: None,
                generation: Arc::new(AtomicU64::new(1)),
                decode_done: false,
                seeking: false,
                finished: false,
            }),
            ..Player::default()
        };
        let ctx = egui::Context::default();
        for _ in 0..3 {
            player.poll(&ctx, &path);
        }
        // Reap everything: the buffer, the held slot and the channel.
        let mut seen: Vec<u64> = player
            .active
            .as_ref()
            .expect("still open")
            .buffered
            .iter()
            .map(|frame| frame.pts.as_secs())
            .collect();
        let active = player.active.as_mut().expect("still open");
        assert!(active.held.is_some(), "the overflow parks in the held slot");
        if let Some(frame) = active.held.take() {
            seen.push(frame.pts.as_secs());
        }
        while let Ok(frame) = active.frames.try_recv() {
            seen.push(frame.pts.as_secs());
        }
        seen.sort_unstable();
        let mut expected: Vec<u64> = (10..10 + BUFFER_FRAMES as u64).collect();
        expected.extend(100..105);
        expected.sort_unstable();
        assert_eq!(seen, expected, "every queued frame survives");
        player.stop();
    }

    #[test]
    fn split_pipe_reads_keep_every_sample_aligned() {
        // One hundred samples as raw bytes, fed whole and in hostile splits:
        // identical samples out, nothing dropped, nothing misaligned.
        let samples: Vec<f32> = (0..100).map(|i| i as f32 * 0.25 - 12.0).collect();
        let mut bytes = Vec::new();
        for sample in &samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let whole = {
            let mut pcm = Vec::new();
            let mut pending = Vec::new();
            push_pcm(&mut pcm, &mut pending, &bytes);
            assert!(pending.is_empty());
            pcm
        };
        assert_eq!(whole, samples);
        for width in [1usize, 3, 5, 7] {
            let mut pcm = Vec::new();
            let mut pending = Vec::new();
            for chunk in bytes.chunks(width) {
                push_pcm(&mut pcm, &mut pending, chunk);
            }
            assert!(pending.is_empty(), "width {width} leaves no tail");
            assert_eq!(pcm, samples, "width {width} aligns");
        }
    }

    #[test]
    fn replaying_a_finished_video_starts_playing() {
        let dir = std::env::temp_dir().join(format!("zapfast-replay-{}", std::process::id()));
        let Some(path) = sample_clip(&dir) else {
            return;
        };
        let mut player = Player::default();
        let mut stop = || {};
        player.toggle(&path, &mut stop).expect("opens");
        assert!(
            player.active.as_ref().is_some_and(|active| active.playing),
            "opening plays"
        );
        let total = player
            .active
            .as_ref()
            .map(|active| active.clip.duration)
            .expect("clip");
        // Watch it to the end: base reaches the length, playback stops.
        {
            let active = player.active.as_mut().expect("open");
            active.base = total;
            active.finished = true;
            active.playing = false;
        }
        player.toggle(&path, &mut stop).expect("replays");
        let active = player.active.as_ref().expect("still open");
        assert!(
            active.playing,
            "one click replays instead of parking at zero"
        );
        assert!(active.base < total, "back at the start");
        player.stop();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn seeking_starts_at_the_nearest_earlier_key_frame() {
        // Samples 1 and 4 are key frames; the target sits past sample 5.
        let table = vec![
            (0, true),
            (33, false),
            (66, false),
            (99, true),
            (132, false),
        ];
        assert_eq!(pick_keyframe(&table, 150), Some(4));
        // Before the second key frame, the first one still opens.
        assert_eq!(pick_keyframe(&table, 90), Some(1));
        // Past the end, the last key frame opens.
        assert_eq!(pick_keyframe(&table, 10_000), Some(4));
        // A target before the first key frame answers nothing, so the
        // search widens instead of opening ahead of the target, where the
        // frames to show could never decode.
        let late = vec![(99, false), (132, true), (165, false)];
        assert_eq!(pick_keyframe(&late, 0), None);
        // With no key frame at all there is nothing to open on.
        let plain = vec![(0, false), (33, false)];
        assert_eq!(pick_keyframe(&plain, 100), None);
        assert_eq!(pick_keyframe(&[], 100), None);
    }
    #[test]
    fn a_wide_gap_still_opens_behind_the_target() {
        // A long interval between key frames must not open ahead: with a
        // key frame every six seconds, a jump to fifteen seconds starts
        // from twelve, never from eighteen.
        let mut table = Vec::new();
        for secs in [0u64, 6, 12, 18] {
            table.push((secs * 1000, true));
            table.push((secs * 1000 + 100, false));
        }
        assert_eq!(pick_keyframe(&table, 15_000), Some(5));
        assert_eq!(pick_keyframe(&table, 12_000), Some(5));
        // Nothing behind the target widens the search instead of taking
        // the key frame ahead.
        assert_eq!(pick_keyframe(&table[6..], 15_000), None);
    }
    #[test]
    fn a_seek_lands_on_a_real_key_frame() {
        // The decoder can only enter on a key frame, so a jump has to answer
        // one no matter where the target falls.
        let dir = std::env::temp_dir().join(format!("zapfast-seek-{}", std::process::id()));
        let Some(path) = sample_clip_seconds(&dir, 6) else {
            return;
        };
        let file = std::fs::File::open(&path).expect("opens");
        let size = file.metadata().expect("stats").len();
        let mut mp4 = mp4::Mp4Reader::read_header(BufReader::new(file), size).expect("header");
        let track = mp4
            .tracks()
            .values()
            .find(|track| track.track_type().ok() == Some(mp4::TrackType::Video))
            .expect("video track");
        let (track_id, timescale) = (track.track_id(), u64::from(track.timescale().max(1)));
        let total = mp4.duration();
        assert!(!total.is_zero(), "the sample clip has a length");
        for fraction in [0.05f32, 0.25, 0.5, 0.75, 0.95, 1.0] {
            let at = total.mul_f32(fraction);
            if at.is_zero() {
                continue;
            }
            let start = first_sample_at(&mut mp4, track_id, timescale, at, total);
            let sample = mp4
                .read_sample(track_id, start)
                .expect("reads")
                .expect("a sample");
            assert!(
                sample.is_sync,
                "a seek to {fraction} starts on sample {start}, a key frame"
            );
            let pts = stamp(sample.start_time, sample.rendering_offset, timescale);
            assert!(
                pts <= at,
                "the key frame at {pts:?} sits at or before the target {at:?}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_long_gap_opens_behind_the_target() {
        // Key frames six seconds apart: jumps to 7, 13, 15 and 19 seconds
        // must all start behind their target, never on the key frame ahead.
        let dir = std::env::temp_dir().join(format!("zapfast-longgap-{}", std::process::id()));
        let Some(path) = sample_clip_gop(&dir, 20, 60) else {
            return;
        };
        let file = std::fs::File::open(&path).expect("opens");
        let size = file.metadata().expect("stats").len();
        let mut mp4 = mp4::Mp4Reader::read_header(BufReader::new(file), size).expect("header");
        let track = mp4
            .tracks()
            .values()
            .find(|track| track.track_type().ok() == Some(mp4::TrackType::Video))
            .expect("video track");
        let (track_id, timescale) = (track.track_id(), u64::from(track.timescale().max(1)));
        let total = mp4.duration();
        assert!(
            total >= Duration::from_secs(19),
            "twenty seconds of clip: {total:?}"
        );
        for at in [7, 13, 15, 19].map(Duration::from_secs) {
            let start = first_sample_at(&mut mp4, track_id, timescale, at, total);
            let sample = mp4
                .read_sample(track_id, start)
                .expect("reads")
                .expect("a sample");
            assert!(sample.is_sync, "a jump to {at:?} opens on a key frame");
            let pts = stamp(sample.start_time, sample.rendering_offset, timescale);
            assert!(
                pts <= at,
                "the key frame at {pts:?} sits behind the target {at:?}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Drives one open clip until the viewer shows a frame, or nothing
    /// past the deadline.
    fn drive_until(
        player: &mut Player,
        ctx: &egui::Context,
        path: &std::path::Path,
        deadline: std::time::Instant,
        mut done: impl FnMut(Duration, bool) -> bool,
    ) -> (Duration, bool) {
        loop {
            let (position, playing) = match player.poll(ctx, path) {
                State::Showing {
                    position, playing, ..
                } => (position, playing),
                State::Loading => (Duration::ZERO, true),
                State::Unsupported(why) => panic!("the sample clip plays: {why}"),
            };
            if done(position, playing) {
                return (position, playing);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the jump lands: {position:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_jump_lands_and_stays_in_sync() {
        // A twenty-second clip with a tone, played for real: jump to
        // fifteen seconds while playing, then to five while paused. The
        // picture must arrive at the target and stay with the sound.
        let dir = std::env::temp_dir().join(format!("zapfast-jump-{}", std::process::id()));
        let Some(path) = sample_clip_seconds(&dir, 20) else {
            return;
        };
        let clip = probe(&path).expect("the header reads");
        let total = clip.duration;
        let ctx = egui::Context::default();
        let mut player = Player::default();
        let mut stop = || {};
        player.toggle(&path, &mut stop).expect("opens");
        // Playing jump to three quarters: the picture lands near fifteen
        // seconds and keeps playing.
        player.seek(&path, 0.75).expect("jumps");
        let target = total.mul_f32(0.75);
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let (position, playing) = drive_until(&mut player, &ctx, &path, deadline, |position, _| {
            position >= target.saturating_sub(Duration::from_secs(2))
        });
        assert!(playing, "a playing jump keeps playing");
        assert!(
            position <= target + Duration::from_secs(5),
            "the picture lands near its target: {position:?} for {target:?}"
        );
        // Paused jump to one quarter: the frame shows on its own, still
        // paused, with no mouse needed.
        player.toggle(&path, &mut stop).expect("pauses");
        player.seek(&path, 0.25).expect("jumps paused");
        let target = total.mul_f32(0.25);
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let (position, playing) = drive_until(&mut player, &ctx, &path, deadline, |position, _| {
            position >= target.saturating_sub(Duration::from_millis(500))
        });
        assert!(!playing, "a paused jump stays paused");
        assert!(
            (position.as_secs_f32() - target.as_secs_f32()).abs() < 1.5,
            "the paused frame is the target: {position:?} for {target:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rapid_jumps_keep_only_the_newest_target() {
        // Three jumps with no paint between them: the retired decodes and
        // extractions die, and playback settles at the last target.
        let dir = std::env::temp_dir().join(format!("zapfast-rapid-{}", std::process::id()));
        let Some(path) = sample_clip_seconds(&dir, 20) else {
            return;
        };
        let clip = probe(&path).expect("the header reads");
        let total = clip.duration;
        let ctx = egui::Context::default();
        let mut player = Player::default();
        let mut stop = || {};
        player.toggle(&path, &mut stop).expect("opens");
        player.seek(&path, 0.9).expect("first");
        player.seek(&path, 0.1).expect("second");
        player.seek(&path, 0.8).expect("third");
        let target = total.mul_f32(0.8);
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let (position, _) = drive_until(&mut player, &ctx, &path, deadline, |position, _| {
            position >= target.saturating_sub(Duration::from_secs(2))
        });
        assert!(
            position <= target + Duration::from_secs(5),
            "only the newest jump survives: {position:?} for {target:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn analyze_reports_length_and_a_real_poster() {
        // A two-second clip: the analysis answers about two seconds and a
        // JPEG poster, both decoded from the file itself.
        let dir = std::env::temp_dir().join(format!("zapfast-analyze-{}", std::process::id()));
        let Some(path) = sample_clip_seconds(&dir, 2) else {
            return;
        };
        let analysis = analyze(&path);
        assert!(
            analysis
                .seconds
                .is_some_and(|seconds| (1..=3).contains(&seconds)),
            "about two seconds: {:?}",
            analysis.seconds
        );
        let poster = analysis.poster.expect("a poster");
        assert!(
            poster.len() > 2 && poster[0] == 0xFF && poster[1] == 0xD8,
            "a JPEG poster, {} bytes",
            poster.len()
        );
        // A file cut to nothing stays unknown instead of answering zero.
        std::fs::write(&path, b"not a video").expect("truncates");
        let analysis = analyze(&path);
        assert!(analysis.seconds.is_none(), "no length is no answer");
        assert!(analysis.poster.is_none(), "no frames is no poster");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn seek_fractions_stay_inside_the_clip() {
        let total = Duration::from_secs(64);
        assert_eq!(total.mul_f32(0.0), Duration::ZERO);
        assert_eq!(total.mul_f32(1.0), total);
        assert_eq!(total.mul_f32(2.0_f32.clamp(0.0, 1.0)), total);
        assert_eq!(total.mul_f32((-1.0_f32).clamp(0.0, 1.0)), Duration::ZERO);
    }
    #[test]
    fn linear_resampling_keeps_duration() {
        // Two seconds at 44.1 kHz become two seconds at 48 kHz.
        let input = vec![0.0f32; 44_100 * 2];
        let output = resample_linear(&input, 44_100, PCM_RATE);
        assert_eq!(output.len(), PCM_RATE as usize * 2);
        // Silence in, silence out.
        assert!(output.iter().all(|sample| *sample == 0.0));
        // Identity rates copy.
        let input = vec![0.5f32; 8];
        assert_eq!(resample_linear(&input, PCM_RATE, PCM_RATE), input);
    }
    #[test]
    fn soundtrack_decodes_in_process_without_ffmpeg() {
        let dir = std::env::temp_dir().join(format!("zapfast-symphonia-{}", std::process::id()));
        let Some(path) = sample_clip(&dir) else {
            return;
        };
        let pcm = symphonia_pcm(&path, &|| true).expect("AAC decodes in-process");
        assert!(pcm.len() > PCM_RATE as usize, "about two seconds of sound");
        assert!(
            pcm.iter().any(|sample| sample.abs() > 0.01),
            "the tone is audible, not silence"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
    /// Makes a two-second H.264 video with a tone, or nothing without ffmpeg.
    fn sample_clip(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        sample_clip_seconds(dir, 2)
    }
    /// Makes a clip of that many seconds with a key frame every second, so a
    /// seek has something to land on, or nothing without ffmpeg.
    fn sample_clip_seconds(dir: &std::path::Path, secs: u32) -> Option<std::path::PathBuf> {
        sample_clip_gop(dir, secs, 10)
    }
    /// Makes a clip with a key frame every `gop` frames (ten frames per
    /// second), or nothing without ffmpeg.
    fn sample_clip_gop(dir: &std::path::Path, secs: u32, gop: u32) -> Option<std::path::PathBuf> {
        std::fs::create_dir_all(dir).ok()?;
        let path = dir.join(format!("clip-{secs}-{gop}.mp4"));
        let gop = gop.to_string();
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y"])
            .args([
                "-f",
                "lavfi",
                "-i",
                &format!("color=c=blue:s=64x64:d={secs}:r=10"),
            ])
            .args([
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:duration={secs}"),
            ])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            // WhatsApp sends baseline video, so the clip has no B-frames either.
            .args(["-profile:v", "baseline", "-bf", "0"])
            .args(["-g", &gop, "-keyint_min", &gop, "-sc_threshold", "0"])
            .args(["-c:a", "aac", "-shortest", "-movflags", "+faststart"])
            .arg(&path)
            .status()
            .is_ok_and(|status| status.success());
        made.then_some(path)
    }
    /// Makes a variant of the sample clip: fast metadata, trailing metadata, or fragments.
    fn sample_variant(
        dir: &std::path::Path,
        name: &str,
        flags: &[&str],
    ) -> Option<std::path::PathBuf> {
        let path = dir.join(name);
        let mut command = std::process::Command::new("ffmpeg");
        command.args(["-v", "error", "-y"]);
        command.args(["-f", "lavfi", "-i", "color=c=blue:s=64x64:d=2:r=10"]);
        command.args(["-f", "lavfi", "-i", "sine=frequency=440:duration=2"]);
        command.args(["-c:v", "libx264", "-pix_fmt", "yuv420p"]);
        command.args(["-profile:v", "baseline", "-bf", "0"]);
        command.args(["-c:a", "aac", "-shortest"]);
        for flag in flags {
            command.args(["-movflags", flag]);
        }
        command.arg(&path);
        let made = command.status().is_ok_and(|status| status.success());
        made.then_some(path)
    }
    #[test]
    fn a_chat_video_probes_and_decodes_frames_in_order() {
        // Its own folder: the audio test uses a similar name in this process.
        let dir = std::env::temp_dir().join(format!("zapfast-playback-{}", std::process::id()));
        let Some(path) = sample_clip(&dir) else {
            return;
        };
        let clip = probe(&path).expect("the header reads");
        assert!(clip.has_audio, "the tone is seen");
        assert!(
            clip.duration >= Duration::from_secs(1),
            "about two seconds: {:?}",
            clip.duration
        );
        assert_eq!((clip.width, clip.height), (64, 64));
        let (tx, rx) = sync_channel::<Frame>(16);
        let generation = Arc::new(AtomicU64::new(1));
        let alive = || generation.load(Ordering::SeqCst) == 1;
        // Decoding runs beside the test, like the viewer does: the channel
        // holds 16 frames and the test drains it.
        let total = clip.duration;
        std::thread::scope(|scope| {
            let held = tx.clone();
            scope.spawn(move || decode(&path, Duration::ZERO, total, &alive, &held));
            drop(tx);
            let mut pts: Vec<Duration> = Vec::new();
            let mut sizes = 0;
            while let Ok(frame) = rx.recv() {
                if let Some(last) = pts.last() {
                    assert!(
                        frame.pts >= *last,
                        "frames stay in presentation order: {pts:?} then {:?}",
                        frame.pts
                    );
                }
                sizes += 1;
                pts.push(frame.pts);
            }
            assert!(sizes >= 10, "most of the twenty frames arrive");
        });
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn every_layout_probes_and_sounds() {
        // Metadata first, metadata last, and fragments: the soundtrack must
        // decode on all of them, or the player goes quiet without saying why.
        let dir = std::env::temp_dir().join(format!("zapfast-layouts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let mut made = 0;
        for (name, flags) in [
            ("fast.mp4", vec!["+faststart"]),
            ("slow.mp4", vec![]),
            ("frag.mp4", vec!["+frag_keyframe", "+empty_moov"]),
        ] {
            let Some(path) = sample_variant(&dir, name, &flags) else {
                continue;
            };
            made += 1;
            // The player tries the in-process header first, then ffmpeg.
            let clip = probe(&path)
                .or_else(|_| probe_ffmpeg(&path))
                .unwrap_or_else(|error| panic!("{name}: the header reads: {error}"));
            let expect_ffmpeg = name == "frag.mp4";
            assert_eq!(clip.ffmpeg, expect_ffmpeg, "{name} picks its engine");
            assert!(clip.has_audio, "{name} carries sound");
            // Streaming or background extraction: every layout must sound.
            let generation = Arc::new(AtomicU64::new(1));
            assert!(
                matches!(
                    audio_at(&path, Duration::ZERO, &generation, 1),
                    Audio::Sound(_) | Audio::Extracting(_)
                ),
                "{name} sounds"
            );
        }
        if made == 0 {
            return;
        }
        assert_eq!(made, 3, "all three layouts are made");
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn fragments_fall_back_to_ffmpeg_for_their_length() {
        if !ffmpeg_present() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("zapfast-fraglen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let Some(path) = sample_variant(&dir, "frag.mp4", &["+frag_keyframe", "+empty_moov"])
        else {
            return;
        };
        // Fragments keep no sample table the in-process reader can walk, so the
        // header honestly refuses and ffmpeg measures instead.
        assert!(
            probe(&path).is_err(),
            "fragments refuse the in-process header"
        );
        let clip = probe_ffmpeg(&path).expect("ffmpeg measures fragments");
        assert!(clip.ffmpeg, "fragments play through the ffmpeg engine");
        assert!(
            clip.duration >= Duration::from_secs(1),
            "about two seconds: {:?}",
            clip.duration
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
