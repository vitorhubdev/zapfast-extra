//! Playback for WhatsApp GIFs, animated WebP stickers, and GIF files.
//!
//! Decoding runs off the UI thread. WebP, GIF, and H.264 MP4 decode in-process;
//! other MP4 codecs use `ffmpeg` when available. Idle animations are removed
//! from memory.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{ColorImage, TextureHandle, TextureOptions};

/// Maximum frame width uploaded to the GPU.
const MAX_WIDTH: u32 = 320;
/// Maximum decoded bytes kept per animation (RGBA8, at display size). A long
/// sticker plays to its end instead of looping a truncated head; the byte
/// cap bounds memory instead of an arbitrary frame count. Sixty-four
/// megabytes hold more than 150 full-width frames, so the old count limit
/// never cuts first.
const MAX_ANIM_BYTES: usize = 64 * 1024 * 1024;
/// Textures uploaded per interface tick. A long animation spreads its first
/// paint over several frames instead of stalling the scroll once.
/// Kept small on purpose: the budget applies per visible animation and tick,
/// so several stickers on screen multiply it. A shared per-tick budget is
/// future work once upload pressure is measured.
const UPLOAD_PER_TICK: usize = 12;
/// Time an unseen animation remains decoded.
const IDLE: Duration = Duration::from_secs(20);
/// Time a failed or stuck decode is remembered before trying again.
///
/// A sticker whose file was still being written, or a decoder that ran out of
/// memory once, must not be blank for the rest of the session: the entry is
/// forgotten and the next paint decodes it again, silently.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// Maximum concurrent decoders.
const MAX_DECODERS: usize = 2;
/// Global decoded-image budget in bytes. Least-recently-used animations go
/// first, preparing ones included, so the set stays bounded even while
/// several uploads are still queued.
const MAX_RESIDENT_BYTES: usize = 96 * 1024 * 1024;

static DECODING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Guard for one decoder slot.
struct DecodeSlot;

impl Drop for DecodeSlot {
    fn drop(&mut self) {
        DECODING.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

struct Decoded {
    frames: Vec<(ColorImage, Duration)>,
}

struct Playing {
    frames: Vec<(TextureHandle, Duration)>,
    total: Duration,
    started: Instant,
    last_drawn: Instant,
}

enum Entry {
    Decoding(Instant),
    Uploading(UploadState),
    Failed(Instant),
    Ready(Playing),
}

/// Decoded frames waiting for their turn on the GPU.
struct UploadState {
    queue: VecDeque<(ColorImage, Duration)>,
    done: Vec<(TextureHandle, Duration)>,
    next_index: usize,
    total: Duration,
    touched: Instant,
}

/// Decoded bytes of one display frame (RGBA8).
fn image_bytes(image: &ColorImage) -> usize {
    image.size[0] * image.size[1] * 4
}

/// GPU bytes of one uploaded frame (RGBA8).
fn texture_bytes(texture: &TextureHandle) -> usize {
    let [width, height] = texture.size();
    width * height * 4
}

/// Decoded bytes one cache entry holds: ready textures plus everything
/// still queued for upload. Decoding entries count zero here; at most two
/// decode at once, and their in-flight frames join the budget on delivery.
fn entry_bytes(entry: &Entry) -> usize {
    match entry {
        Entry::Ready(playing) => playing
            .frames
            .iter()
            .map(|(texture, _)| texture_bytes(texture))
            .sum(),
        Entry::Uploading(upload) => {
            upload
                .done
                .iter()
                .map(|(texture, _)| texture_bytes(texture))
                .sum::<usize>()
                + upload
                    .queue
                    .iter()
                    .map(|(image, _)| image_bytes(image))
                    .sum::<usize>()
        }
        Entry::Decoding(_) | Entry::Failed(_) => 0,
    }
}

/// When an entry was last seen on screen, for least-recently-used eviction.
fn entry_touched(entry: &Entry) -> Option<Instant> {
    match entry {
        Entry::Ready(playing) => Some(playing.last_drawn),
        Entry::Uploading(upload) => Some(upload.touched),
        Entry::Decoding(_) | Entry::Failed(_) => None,
    }
}

#[derive(Clone, Default)]
struct Cache(Arc<Mutex<HashMap<PathBuf, Entry>>>);

/// Result returned by a decoder thread.
type Delivery = (PathBuf, Option<Decoded>);

/// Decoded frames waiting for texture upload.
#[derive(Clone, Default)]
struct Inbox(Arc<Mutex<Vec<Delivery>>>);

fn cache(ctx: &egui::Context) -> Cache {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<Cache>(egui::Id::new("animations"))
            .clone()
    })
}

fn inbox(ctx: &egui::Context) -> Inbox {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<Inbox>(egui::Id::new("animation-inbox"))
            .clone()
    })
}

/// Current display state for an animated file.
pub enum Frame {
    /// Current animation frame.
    Ready(TextureHandle),
    /// Decode in progress; show the poster.
    Pending,
    /// Unsupported in-app; show the poster and allow opening the file.
    Unavailable,
}

/// Returns the current frame, starting decoding when needed. Schedules the next repaint.
pub fn frame(ui: &egui::Ui, path: &Path, rect: egui::Rect) -> Frame {
    // ScrollArea still lays out clipped rows. They must neither start decoders
    // nor keep the window repainting while their pixels are off screen.
    if !ui.is_rect_visible(rect) {
        return Frame::Pending;
    }
    let ctx = ui.ctx();
    let cache = cache(ctx);
    let inbox = inbox(ctx);
    // Upload decoded frames on the UI thread.
    let arrived: Vec<Delivery> =
        std::mem::take(&mut *inbox.0.lock().unwrap_or_else(|p| p.into_inner()));
    let mut entries = cache.0.lock().unwrap_or_else(|p| p.into_inner());
    for (arrived_path, decoded) in arrived {
        let entry = match decoded {
            Some(decoded) if !decoded.frames.is_empty() => {
                let mut total = Duration::ZERO;
                let mut queue = VecDeque::with_capacity(decoded.frames.len());
                for (image, delay) in decoded.frames {
                    total += delay;
                    queue.push_back((image, delay));
                }
                Entry::Uploading(UploadState {
                    queue,
                    done: Vec::new(),
                    next_index: 0,
                    total: total.max(Duration::from_millis(50)),
                    touched: Instant::now(),
                })
            }
            _ => Entry::Failed(Instant::now()),
        };
        entries.insert(arrived_path, entry);
    }
    // Remove idle and least-recently-used animations.
    let now = Instant::now();
    entries.retain(|_, entry| match entry {
        Entry::Ready(playing) => now.duration_since(playing.last_drawn) < IDLE,
        Entry::Uploading(upload) => now.duration_since(upload.touched) < IDLE,
        Entry::Failed(at) | Entry::Decoding(at) => now.duration_since(*at) < RETRY_AFTER,
    });
    let mut resident: usize = entries.values().map(entry_bytes).sum();
    while resident > MAX_RESIDENT_BYTES {
        let victim = entries
            .iter()
            .filter_map(|(entry_path, entry)| {
                if entry_path.as_path() == path {
                    return None;
                }
                Some((
                    entry_path.clone(),
                    entry_touched(entry)?,
                    entry_bytes(entry),
                ))
            })
            .min_by_key(|(_, touched, _)| *touched);
        let Some((victim, _, freed)) = victim else {
            break;
        };
        entries.remove(&victim);
        resident = resident.saturating_sub(freed);
    }
    // Uploads land a few textures per tick, so a long animation spreads its
    // first paint over several frames instead of stalling the scroll once.
    if let Some(Entry::Uploading(upload)) = entries.get_mut(path) {
        let mut spent = 0;
        while spent < UPLOAD_PER_TICK {
            let Some((image, delay)) = upload.queue.pop_front() else {
                break;
            };
            let name = format!("{}#{}", path.display(), upload.next_index);
            upload.next_index += 1;
            upload
                .done
                .push((ctx.load_texture(name, image, TextureOptions::LINEAR), delay));
            spent += 1;
        }
        upload.touched = Instant::now();
        if upload.queue.is_empty() {
            let upload = match entries.remove(path) {
                Some(Entry::Uploading(upload)) => upload,
                _ => unreachable!("an upload just finished"),
            };
            entries.insert(
                path.to_path_buf(),
                Entry::Ready(Playing {
                    frames: upload.done,
                    total: upload.total,
                    started: Instant::now(),
                    last_drawn: Instant::now(),
                }),
            );
        } else {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }
    match entries.get_mut(path) {
        Some(Entry::Ready(playing)) => {
            playing.last_drawn = now;
            let elapsed = now.duration_since(playing.started);
            let mut position =
                Duration::from_nanos((elapsed.as_nanos() % playing.total.as_nanos()) as u64);
            let mut chosen = 0;
            let mut until_next = Duration::from_millis(40);
            for (index, (_, delay)) in playing.frames.iter().enumerate() {
                if position < *delay {
                    chosen = index;
                    until_next = *delay - position;
                    break;
                }
                position -= *delay;
            }
            if playing.frames.len() > 1 {
                ctx.request_repaint_after(until_next.max(Duration::from_millis(10)));
            }
            Frame::Ready(playing.frames[chosen].0.clone())
        }
        Some(Entry::Uploading(_)) => Frame::Pending,
        Some(Entry::Decoding(_)) => Frame::Pending,
        Some(Entry::Failed(_)) => Frame::Unavailable,
        None => {
            if DECODING.load(std::sync::atomic::Ordering::Acquire) >= MAX_DECODERS {
                // Retry shortly when all decoder slots are busy.
                ctx.request_repaint_after(Duration::from_millis(150));
                return Frame::Pending;
            }
            DECODING.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let slot = DecodeSlot;
            entries.insert(path.to_path_buf(), Entry::Decoding(now));
            let file = path.to_path_buf();
            let ctx = ctx.clone();
            let spawned = std::thread::Builder::new()
                .name("animation-decode".into())
                .spawn(move || {
                    let _slot = slot;
                    // Convert decoder panics to failed results.
                    let decoded =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode(&file)))
                            .unwrap_or(None);
                    inbox
                        .0
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push((file, decoded));
                    ctx.request_repaint();
                });
            if spawned.is_err() {
                entries.insert(path.to_path_buf(), Entry::Failed(Instant::now()));
                return Frame::Unavailable;
            }
            Frame::Pending
        }
    }
}

/// MP4 playback is always available because H.264 decodes in-process.
pub fn can_play_video() -> bool {
    true
}

/// Whether `ffmpeg` is available for other MP4 codecs.
fn ffmpeg_present() -> bool {
    static KNOWN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *KNOWN.get_or_init(|| {
        let mut check = Command::new("ffmpeg");
        quiet(&mut check)
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// A helper media process that never flashes a console window on Windows.
fn quiet(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: decoding a sticker must not pop a console.
        command.creation_flags(0x0800_0000);
    }
    command
}

fn decode(path: &Path) -> Option<Decoded> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "webp" | "gif" => decode_image(path, &extension),
        _ => decode_video(path),
    }
}

/// Decodes animated GIF with the `image` crate.
fn decode_image(path: &Path, extension: &str) -> Option<Decoded> {
    use image::AnimationDecoder;
    if extension != "gif" {
        return decode_webp(path);
    }
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let frames = image::codecs::gif::GifDecoder::new(reader)
        .ok()?
        .into_frames();
    let mut decoded = Vec::new();
    let mut spent = 0usize;
    for frame in frames {
        let frame = frame.ok()?;
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        let delay = Duration::from_millis(u64::from(numerator / denominator.max(1)).max(20));
        let image = frame.into_buffer();
        // The budget prices display pixels: a 512-wide sticker shows at 320.
        let shown = to_color_image(&image);
        spent += shown.size[0] * shown.size[1] * 4;
        decoded.push((shown, delay));
        if spent >= MAX_ANIM_BYTES {
            break;
        }
    }
    Some(Decoded { frames: decoded })
}

/// Decodes animated WebP with libwebp. It returns complete canvas frames,
/// unlike the `image` decoder, which did not apply frame disposal correctly.
fn decode_webp(path: &Path) -> Option<Decoded> {
    let bytes = std::fs::read(path).ok()?;
    let decoder = webp_animation::Decoder::new(&bytes).ok()?;
    let (width, height) = decoder.dimensions();
    let mut decoded = Vec::new();
    let mut previous = 0i64;
    let mut spent = 0usize;
    for frame in decoder.into_iter() {
        let image = image::RgbaImage::from_raw(width, height, frame.data().to_vec())?;
        let delay = (i64::from(frame.timestamp()) - previous).max(20) as u64;
        previous = i64::from(frame.timestamp());
        // The budget prices display pixels, not the encoded canvas.
        let shown = to_color_image(&image);
        spent += shown.size[0] * shown.size[1] * 4;
        decoded.push((shown, Duration::from_millis(delay)));
        if spent >= MAX_ANIM_BYTES {
            break;
        }
    }
    // Single-frame files use the static-image path.
    (decoded.len() > 1).then_some(Decoded { frames: decoded })
}

fn to_color_image(image: &image::RgbaImage) -> ColorImage {
    let image = if image.width() > MAX_WIDTH {
        let height = (image.height() * MAX_WIDTH / image.width()).max(1);
        image::imageops::resize(
            image,
            MAX_WIDTH,
            height,
            image::imageops::FilterType::Triangle,
        )
    } else {
        image.clone()
    };
    ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    )
}

/// Decodes MP4 to scaled RGBA frames with `ffmpeg`.
fn decode_video(path: &Path) -> Option<Decoded> {
    // Decode WhatsApp's H.264 MP4s in-process and use ffmpeg for other codecs.
    decode_mp4(path).or_else(|| decode_with_ffmpeg(path))
}

/// Decodes an MP4 video track in-process.
fn decode_mp4(path: &Path) -> Option<Decoded> {
    let file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let mut mp4 = mp4::Mp4Reader::read_header(std::io::BufReader::new(file), size).ok()?;
    let (track_id, timescale, sps, pps, count) = {
        let track = mp4
            .tracks()
            .values()
            .find(|track| track.track_type().ok() == Some(mp4::TrackType::Video))?;
        (
            track.track_id(),
            u64::from(track.timescale().max(1)),
            track.sequence_parameter_set().ok()?.to_vec(),
            track.picture_parameter_set().ok()?.to_vec(),
            track.sample_count(),
        )
    };
    let mut decoder = openh264::decoder::Decoder::new().ok()?;
    let mut frames: Vec<(ColorImage, Duration)> = Vec::new();
    let mut spent = 0usize;
    let mut delays: std::collections::VecDeque<Duration> = std::collections::VecDeque::new();
    // Send parameter sets and samples to the decoder in Annex B format.
    let mut parameters = Vec::new();
    push_annex_b(&mut parameters, &sps);
    push_annex_b(&mut parameters, &pps);
    let _ = decoder.decode(&parameters);
    for sample_id in 1..=count {
        let Ok(Some(sample)) = mp4.read_sample(track_id, sample_id) else {
            break;
        };
        let delay =
            Duration::from_millis((u64::from(sample.duration) * 1000 / timescale).clamp(20, 1000));
        delays.push_back(delay);
        let mut annex_b = Vec::with_capacity(sample.bytes.len() + 16);
        avcc_to_annex_b(&mut annex_b, &sample.bytes);
        if let Ok(Some(yuv)) = decoder.decode(&annex_b) {
            let delay = delays.pop_front().unwrap_or(delay);
            if let Some(frame) = frame_of(&yuv, delay) {
                spent += frame.0.size[0] * frame.0.size[1] * 4;
                frames.push(frame);
                if spent >= MAX_ANIM_BYTES {
                    break;
                }
            }
        }
    }
    if let Ok(rest) = decoder.flush_remaining() {
        for yuv in &rest {
            if spent >= MAX_ANIM_BYTES {
                break;
            }
            let delay = delays.pop_front().unwrap_or(Duration::from_millis(66));
            if let Some(frame) = frame_of(yuv, delay) {
                spent += frame.0.size[0] * frame.0.size[1] * 4;
                frames.push(frame);
            }
        }
    }
    (!frames.is_empty()).then_some(Decoded { frames })
}

/// Converts and scales one decoded frame.
fn frame_of(
    yuv: &openh264::decoder::DecodedYUV<'_>,
    delay: Duration,
) -> Option<(ColorImage, Duration)> {
    use openh264::formats::YUVSource;

    let (width, height) = yuv.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    let mut rgba = vec![0u8; width * height * 4];
    yuv.write_rgba8(&mut rgba);
    let image = image::RgbaImage::from_raw(width as u32, height as u32, rgba)?;
    let out_width = (width as u32).min(MAX_WIDTH);
    let out_height = ((height as u64 * out_width as u64 / width as u64) as u32).max(1);
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
    Some((to_color_image(&scaled), delay))
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

fn decode_with_ffmpeg(path: &Path) -> Option<Decoded> {
    if !ffmpeg_present() {
        return None;
    }
    let mut probe_cmd = Command::new("ffprobe");
    let probe = quiet(&mut probe_cmd)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok()?;
    let dimensions = String::from_utf8_lossy(&probe.stdout);
    let mut parts = dimensions.trim().split(',');
    let width: u32 = parts.next()?.trim().parse().ok()?;
    let height: u32 = parts.next()?.trim().parse().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    let out_width = width.min(MAX_WIDTH);
    // Use even dimensions and preserve aspect ratio.
    let out_height = ((height as u64 * out_width as u64 / width as u64) as u32).max(2) & !1;
    let fps = 15u32;
    let mut launch = Command::new("ffmpeg");
    let mut child = quiet(&mut launch)
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-an",
            "-vf",
            &format!("fps={fps},scale={out_width}:{out_height}"),
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let frame_len = (out_width * out_height * 4) as usize;
    let mut frames = Vec::new();
    let delay = Duration::from_millis(1000 / u64::from(fps));
    let mut buffer = vec![0u8; frame_len];
    let mut spent = 0usize;
    loop {
        if stdout.read_exact(&mut buffer).is_err() {
            break;
        }
        frames.push((
            ColorImage::from_rgba_unmultiplied([out_width as usize, out_height as usize], &buffer),
            delay,
        ));
        spent += frame_len;
        if spent >= MAX_ANIM_BYTES {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    (!frames.is_empty()).then_some(Decoded { frames })
}

#[cfg(test)]
mod tests {
    #[test]
    fn clipped_animation_does_not_decode_or_schedule_frames() {
        let ctx = egui::Context::default();
        let path = std::path::Path::new("offscreen.gif");
        let mut delay = Duration::ZERO;
        for index in 0..4 {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    time: Some(index as f64),
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(200.0, 200.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    let rect =
                        egui::Rect::from_min_size(egui::pos2(0.0, 1000.0), egui::vec2(50.0, 50.0));
                    assert!(matches!(
                        super::frame(ui, path, rect),
                        super::Frame::Pending
                    ));
                },
            );
            delay = output.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
            output.textures_delta.clear();
        }
        assert!(super::cache(&ctx).0.lock().unwrap().is_empty());
        assert!(
            delay > Duration::from_secs(1),
            "offscreen media requested {delay:?}"
        );
    }

    use super::*;

    #[test]
    fn a_failure_that_aged_out_is_decoded_again() {
        let ctx = egui::Context::default();
        let path = std::path::Path::new("sticker-that-failed.webp");
        // A decode failure remembered long ago must not keep the sticker blank.
        {
            let cache = super::cache(&ctx);
            cache.0.lock().unwrap().insert(
                path.to_path_buf(),
                super::Entry::Failed(Instant::now() - RETRY_AFTER - Duration::from_secs(1)),
            );
        }
        let mut output = ctx.run_ui(
            egui::RawInput {
                time: Some(1.0),
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(200.0, 200.0),
                )),
                ..Default::default()
            },
            |ui| {
                let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(50.0, 50.0));
                assert!(matches!(
                    super::frame(ui, path, rect),
                    super::Frame::Pending
                ));
            },
        );
        // The frame's texture deltas belong to a real painting context.
        output.textures_delta.clear();
        let cache = super::cache(&ctx);
        let entries = cache.0.lock().unwrap();
        let retried = match entries.get(path) {
            Some(super::Entry::Decoding(at)) | Some(super::Entry::Failed(at)) => {
                at.elapsed() < RETRY_AFTER
            }
            _ => false,
        };
        assert!(
            retried,
            "a stale failure must be forgotten and decoded again"
        );
    }

    /// Verifies animated WebP frame disposal.
    #[test]
    fn a_moving_subject_leaves_no_trace_behind() {
        use webp_animation::prelude::*;
        let side = 64u32;
        let square = |x0: u32, y0: u32, color: [u8; 4]| {
            let mut frame = vec![0u8; (side * side * 4) as usize];
            for y in y0..y0 + 16 {
                for x in x0..x0 + 16 {
                    let at = ((y * side + x) * 4) as usize;
                    frame[at..at + 4].copy_from_slice(&color);
                }
            }
            frame
        };
        let mut encoder = Encoder::new((side, side)).expect("encoder");
        encoder
            .add_frame(&square(0, 0, [255, 0, 0, 255]), 0)
            .expect("frame");
        encoder
            .add_frame(&square(40, 40, [0, 255, 0, 255]), 100)
            .expect("frame");
        let webp = encoder.finalize(200).expect("finalizes");
        let dir = std::env::temp_dir().join(format!("zapfast-ghost-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("moving.webp");
        std::fs::write(&path, &webp).expect("writes");
        let decoded = decode(&path).expect("decodes");
        assert_eq!(decoded.frames.len(), 2);
        let second = &decoded.frames[1].0;
        let old = second.pixels[8 * second.width() + 8];
        assert_eq!(old.a(), 0, "the first frame's square is gone: {old:?}");
        let new = second.pixels[48 * second.width() + 48];
        assert!(new.a() > 200, "the second frame's square shows: {new:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn animated_webp_decodes_into_frames() {
        // Two frames 100 ms apart.
        let dir = std::env::temp_dir().join(format!("zapfast-anim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("two.gif");
        {
            let file = std::fs::File::create(&path).expect("file");
            let mut encoder = image::codecs::gif::GifEncoder::new(file);
            encoder
                .set_repeat(image::codecs::gif::Repeat::Infinite)
                .expect("repeat");
            for shade in [40u8, 200u8] {
                let frame = image::Frame::from_parts(
                    image::RgbaImage::from_pixel(8, 8, image::Rgba([shade, shade, shade, 255])),
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(100, 1),
                );
                encoder.encode_frame(frame).expect("frame");
            }
        }
        let decoded = decode(&path).expect("decodes");
        assert_eq!(decoded.frames.len(), 2);
        assert_eq!(decoded.frames[0].1, Duration::from_millis(100));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_long_sticker_plays_to_its_end() {
        // Two hundred tiny frames: the old frame-count cap would have cut
        // the tail off and looped a truncated head; the byte budget keeps
        // the whole animation.
        let dir = std::env::temp_dir().join(format!("zapfast-long-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("long.gif");
        {
            let file = std::fs::File::create(&path).expect("file");
            let mut encoder = image::codecs::gif::GifEncoder::new(file);
            encoder
                .set_repeat(image::codecs::gif::Repeat::Infinite)
                .expect("repeat");
            for shade in 0..200u8 {
                let frame = image::Frame::from_parts(
                    image::RgbaImage::from_pixel(8, 8, image::Rgba([shade, shade, shade, 255])),
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(50, 1),
                );
                encoder.encode_frame(frame).expect("frame");
            }
        }
        let decoded = decode(&path).expect("decodes");
        assert_eq!(decoded.frames.len(), 200);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_feature_length_webp_keeps_every_frame() {
        // Past the old 150-frame cut: 160 canvas frames must all survive,
        // priced at display size instead of the encoded canvas.
        use webp_animation::prelude::*;
        let side = 48u32;
        let mut encoder = Encoder::new((side, side)).expect("encoder");
        for index in 0..160i32 {
            let shade = (index % 251) as u8;
            let frame = vec![shade; (side * side * 4) as usize];
            encoder.add_frame(&frame, index * 50).expect("frame");
        }
        let webp = encoder.finalize(160 * 50).expect("finalizes");
        let dir = std::env::temp_dir().join(format!("zapfast-long-webp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("long.webp");
        std::fs::write(&path, &webp).expect("writes");
        let decoded = decode(&path).expect("decodes");
        assert_eq!(decoded.frames.len(), 160);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_resident_budget_counts_display_bytes() {
        let image = ColorImage::new([10, 20], vec![egui::Color32::BLACK; 200]);
        assert_eq!(image_bytes(&image), 800);
        let mut queue = VecDeque::new();
        queue.push_back((image, Duration::from_millis(50)));
        let entry = Entry::Uploading(UploadState {
            queue,
            done: Vec::new(),
            next_index: 0,
            total: Duration::from_millis(50),
            touched: Instant::now(),
        });
        assert_eq!(entry_bytes(&entry), 800);
        assert!(entry_touched(&entry).is_some());
    }

    #[test]
    fn an_mp4_made_by_ffmpeg_decodes_in_process() {
        if !can_play_video() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("zapfast-mp4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("clip.mp4");
        let made = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=0.5:size=64x48:rate=10",
            ])
            .args(["-pix_fmt", "yuv420p"])
            .arg(&path)
            .status()
            .is_ok_and(|status| status.success());
        if !made {
            // Skip when this ffmpeg lacks the encoder.
            return;
        }
        let decoded = decode(&path).expect("decodes");
        // Five frames at 10 fps. The in-process path preserves their timing.
        assert_eq!(decoded.frames.len(), 5);
        assert_eq!(decoded.frames[0].1, Duration::from_millis(100));
        assert_eq!(decoded.frames[0].0.size, [64, 48]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_still_webp_is_not_an_animation() {
        let dir = std::env::temp_dir().join(format!("zapfast-still-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("still.webp");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]))
            .save(&path)
            .expect("saves");
        assert!(decode(&path).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod probe {
    use super::*;

    /// Decodes the file in `ZAPFAST_MP4_PROBE`:
    /// `ZAPFAST_MP4_PROBE=some.mp4 cargo test --all-features probe -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a file to look at"]
    fn decodes_the_file_named_by_the_environment() {
        let Some(path) = std::env::var_os("ZAPFAST_MP4_PROBE") else {
            return;
        };
        let started = Instant::now();
        let decoded = decode_mp4(Path::new(&path)).expect("decodes in-process");
        eprintln!(
            "{} frames of {:?}, first delay {:?}, in {:?}",
            decoded.frames.len(),
            decoded.frames[0].0.size,
            decoded.frames[0].1,
            started.elapsed()
        );
        assert!(!decoded.frames.is_empty());
    }
}
