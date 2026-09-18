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
/// Frames waiting to be shown. Caps memory while surviving decode hiccups.
const BUFFER_FRAMES: usize = 60;
/// How long a shown frame is kept behind the buffer for a pause or a seek.
const KEEP_BEHIND: Duration = Duration::from_secs(1);

/// What the viewer needs to know before the first frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    pub duration: Duration,
    pub width: u32,
    pub height: u32,
    pub has_audio: bool,
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
    let mp4 = mp4::Mp4Reader::read_header(BufReader::new(file), size)
        .map_err(|_| "This video is not an MP4 file, or it is damaged.".to_owned())?;
    let track = mp4
        .tracks()
        .values()
        .find(|track| track.track_type().ok() == Some(mp4::TrackType::Video))
        .ok_or_else(|| "This file has no video track.".to_owned())?;
    // Parameter sets only exist for H.264 tracks; their absence means a
    // codec openh264 cannot read, like HEVC.
    track.sequence_parameter_set().map_err(|_| {
        "This video uses a codec this app cannot play. Open it in the default app instead."
            .to_owned()
    })?;
    track.picture_parameter_set().map_err(|_| {
        "This video uses a codec this app cannot play. Open it in the default app instead."
            .to_owned()
    })?;
    let duration = track.duration();
    if duration.is_zero() {
        return Err("This video has no readable length.".to_owned());
    }
    let has_audio = mp4
        .tracks()
        .values()
        .any(|track| track.track_type().ok() == Some(mp4::TrackType::Audio));
    Ok(Clip {
        duration,
        width: u32::from(track.width().max(2)),
        height: u32::from(track.height().max(2)),
        has_audio,
    })
}

/// One decoded frame and when it shows, measured from the start.
struct Frame {
    pts: Duration,
    image: ColorImage,
}

struct Active {
    path: PathBuf,
    clip: Clip,
    /// The soundtrack. The device is held so the stream stays open; only the
    /// sink is read, which is why this is a tuple and not a named struct.
    audio: Option<(rodio::MixerDeviceSink, rodio::Player)>,
    playing: bool,
    /// Position when playback last (re)started; the sink or the wall clock
    /// counts from there.
    base: Duration,
    started: Instant,
    frames: Receiver<Frame>,
    buffered: VecDeque<Frame>,
    shown: Duration,
    texture: Option<(TextureHandle, Vec2, Duration)>,
    generation: Arc<AtomicU64>,
    decode_done: bool,
    finished: bool,
}

/// Plays the video open in the viewer. Only one plays at a time.
#[derive(Default)]
pub struct Player {
    active: Option<Active>,
    /// The file the viewer already refused, and why. The view reads this so
    /// opening an unplayable file complains once instead of every frame.
    refused: Option<(PathBuf, String)>,
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
                    sink.pause();
                }
                active.playing = false;
            } else {
                active.finished = false;
                if active.base >= active.clip.duration {
                    self.seek(path, 0.0)?;
                    return Ok(());
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
        self.open(path, total.mul_f32(fraction.clamp(0.0, 1.0)))
    }

    /// Opens a file at a position, replacing whatever was playing.
    fn open(&mut self, path: &Path, at: Duration) -> Result<(), String> {
        self.stop();
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
            Err(error) => {
                self.refused = Some((path.to_path_buf(), error.clone()));
                return Err(error);
            }
        };
        let at = at.min(clip.duration);
        let audio = audio_at(path, at);
        let (tx, rx) = sync_channel::<Frame>(BUFFER_FRAMES);
        let generation = Arc::new(AtomicU64::new(1));
        spawn_decode(
            path.to_path_buf(),
            at,
            clip.duration,
            generation.clone(),
            tx,
        );
        if let Some((_, sink)) = &audio {
            sink.play();
        }
        self.active = Some(Active {
            path: path.to_path_buf(),
            clip,
            audio,
            playing: true,
            base: at,
            started: Instant::now(),
            frames: rx,
            buffered: VecDeque::new(),
            shown: Duration::ZERO,
            texture: None,
            generation,
            decode_done: false,
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

    /// Pumps decoded frames and answers what the viewer should paint.
    pub fn poll(&mut self, ctx: &egui::Context, path: &Path) -> State {
        let Some(active) = self.active.as_mut() else {
            return State::Loading;
        };
        if active.path != path {
            return State::Loading;
        }
        loop {
            match active.frames.try_recv() {
                Ok(frame) => {
                    if active.buffered.len() >= BUFFER_FRAMES {
                        active.buffered.pop_front();
                    }
                    active.buffered.push_back(frame);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    active.decode_done = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }
        let position = active.position();
        // Drop what is well behind, but keep the frame on screen.
        while active.buffered.len() > 1 && active.buffered[1].pts + KEEP_BEHIND < position {
            active.buffered.pop_front();
        }
        let total = active.clip.duration;
        if position >= total {
            active.playing = false;
            active.finished = true;
            active.base = total;
            if let Some((_, sink)) = &active.audio {
                sink.pause();
            }
        }
        let pts = active
            .buffered
            .iter()
            .rev()
            .find(|frame| frame.pts <= position)
            .or(active.buffered.front())
            .map(|frame| frame.pts);
        match pts {
            Some(pts) => {
                // The texture name carries the timestamp, so every upload is a
                // new texture and dropping the old handle frees it.
                let texture = match active.texture.clone() {
                    Some((texture, size, shown)) if shown == pts => Some((texture, size)),
                    _ => active
                        .buffered
                        .iter()
                        .find(|frame| frame.pts == pts)
                        .map(|frame| {
                            let texture = ctx.load_texture(
                                format!("video-{}-{}", path.display(), pts.as_millis()),
                                frame.image.clone(),
                                TextureOptions::LINEAR,
                            );
                            let size =
                                Vec2::new(frame.image.width() as f32, frame.image.height() as f32);
                            active.shown = pts;
                            active.texture = Some((texture.clone(), size, pts));
                            (texture, size)
                        }),
                };
                match texture {
                    Some((texture, size)) => {
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
                        }
                    }
                    None => State::Loading,
                }
            }
            None if active.decode_done => {
                // The track ended without a single frame: the file is damaged
                // in a way the header did not show. Say so instead of spinning.
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

impl Active {
    fn position(&self) -> Duration {
        if !self.playing {
            return self.base;
        }
        match &self.audio {
            Some((_, sink)) => self.base + sink.get_pos(),
            None => self.base + self.started.elapsed(),
        }
    }
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
fn audio_at(path: &Path, at: Duration) -> Option<(rodio::MixerDeviceSink, rodio::Player)> {
    use rodio::Source;
    let file = std::fs::File::open(path).ok()?;
    let mut decoder = rodio::Decoder::new(BufReader::new(file)).ok()?;
    if !at.is_zero() && decoder.try_seek(at).is_err() {
        // Without seeking, sound and picture would drift apart from the
        // first second, so a file that cannot seek restarts instead.
        return audio_at(path, Duration::ZERO);
    }
    let device = rodio::DeviceSinkBuilder::open_default_sink().ok()?;
    let sink = rodio::Player::connect_new(device.mixer());
    sink.append(decoder);
    sink.pause();
    Some((device, sink))
}

/// Decodes one video track from a position, sending frames with timestamps.
///
/// Decoding restarts at the nearest key frame at or before the position and
/// drops everything earlier, so a seek pays only for the frames between the
/// two. The thread ends when the track does or when a newer generation
/// replaces it.
fn spawn_decode(
    path: PathBuf,
    at: Duration,
    total: Duration,
    generation: Arc<AtomicU64>,
    out: SyncSender<Frame>,
) {
    let current = generation.load(Ordering::SeqCst);
    let alive = move || generation.load(Ordering::SeqCst) == current;
    let _ = std::thread::Builder::new()
        .name("video-decode".into())
        .spawn(move || {
            decode(&path, at, total, &alive, &out);
        });
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
    let (track_id, timescale, count) = (
        track.track_id(),
        u64::from(track.timescale().max(1)),
        track.sample_count(),
    );
    let Ok(sps) = track.sequence_parameter_set().map(|bytes| bytes.to_vec()) else {
        return;
    };
    let Ok(pps) = track.picture_parameter_set().map(|bytes| bytes.to_vec()) else {
        return;
    };
    let target = at.min(total);
    let start_sample = first_sample_at(&mut mp4, track_id, timescale, target);
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

/// The 1-based sample to decode from: the nearest key frame at or before the
/// target in a table of start time and key-frame flag pairs.
fn pick_start(table: &[(u64, bool)], target_units: u64) -> u32 {
    let mut start = 1u32;
    for (index, entry) in table.iter().enumerate() {
        if entry.0 > target_units {
            break;
        }
        if entry.1 {
            start = index as u32 + 1;
        }
    }
    start
}
/// The sample to start decoding at: the nearest key frame at or before the
/// target, so the decoder sees a clean entry point without replaying the
/// whole file.
fn first_sample_at(
    mp4: &mut mp4::Mp4Reader<BufReader<std::fs::File>>,
    track_id: u32,
    timescale: u64,
    target: Duration,
) -> u32 {
    if target.is_zero() {
        return 1;
    }
    let target_units = (target.as_secs_f64() * timescale as f64) as u64;
    // One sequential pass; only the bytes from the chosen sample decode.
    let mut table = Vec::new();
    let mut sample_id = 1u32;
    while let Ok(Some(sample)) = mp4.read_sample(track_id, sample_id) {
        if sample.start_time > target_units {
            break;
        }
        table.push((sample.start_time, sample.is_sync));
        sample_id += 1;
        if sample_id > 60_000 {
            break;
        }
    }
    pick_start(&table, target_units)
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
    fn seeking_starts_at_the_nearest_earlier_key_frame() {
        // Samples 1 and 4 are key frames; the target sits past sample 5.
        let table = vec![
            (0, true),
            (33, false),
            (66, false),
            (99, true),
            (132, false),
        ];
        assert_eq!(pick_start(&table, 150), 4);
        // Before the second key frame, the first one still opens.
        assert_eq!(pick_start(&table, 90), 1);
        // Past the end, the last key frame opens.
        assert_eq!(pick_start(&table, 10_000), 4);
        // With no key frame at all, decoding starts at the beginning.
        let plain = vec![(0, false), (33, false)];
        assert_eq!(pick_start(&plain, 100), 1);
        assert_eq!(pick_start(&[], 100), 1);
    }

    #[test]
    fn seek_fractions_stay_inside_the_clip() {
        let total = Duration::from_secs(64);
        assert_eq!(total.mul_f32(0.0), Duration::ZERO);
        assert_eq!(total.mul_f32(1.0), total);
        assert_eq!(total.mul_f32(2.0_f32.clamp(0.0, 1.0)), total);
        assert_eq!(total.mul_f32((-1.0_f32).clamp(0.0, 1.0)), Duration::ZERO);
    }
    /// Makes a two-second H.264 video with a tone, or nothing without ffmpeg.
    fn sample_clip(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        std::fs::create_dir_all(dir).ok()?;
        let path = dir.join("clip.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y"])
            .args(["-f", "lavfi", "-i", "color=c=blue:s=64x64:d=2:r=10"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=2"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            // WhatsApp sends baseline video, so the clip has no B-frames either.
            .args(["-profile:v", "baseline", "-bf", "0"])
            .args(["-c:a", "aac", "-shortest", "-movflags", "+faststart"])
            .arg(&path)
            .status()
            .is_ok_and(|status| status.success());
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
}
