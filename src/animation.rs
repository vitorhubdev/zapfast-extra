//! Playback for WhatsApp GIFs, animated WebP stickers, and GIF files.
//!
//! Decoding runs off the UI thread. WebP, GIF, and H.264 MP4 decode in-process;
//! other MP4 codecs use `ffmpeg` when available. Idle animations are removed
//! from memory.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{ColorImage, TextureHandle, TextureOptions};

/// Maximum frame width uploaded to the GPU.
const MAX_WIDTH: u32 = 320;
/// Display pixels kept in RAM per animation before the tail spills to disk.
/// Sixteen megabytes hold forty full-width frames; everything past that
/// pages from a spool file, so duration is never cut to fit RAM.
const MAX_ANIM_BYTES: usize = 16 * 1024 * 1024;
/// Frames decoded per animation, a backstop for absurd inputs. Four thousand
/// frames are minutes of sticker; past that the head stays playable and the
/// tail is dropped with a warning instead of growing a spool without end.
const MAX_SOURCE_FRAMES: usize = 4_096;
/// Textures materialized per interface tick. A long animation spreads its
/// first paint over several frames instead of stalling the scroll once.
/// Kept small on purpose: the budget applies per visible animation and tick,
/// so several stickers on screen multiply it. A shared per-tick budget is
/// future work once upload pressure is measured.
const UPLOAD_PER_TICK: usize = 12;
/// How far ahead of the playhead textures are prepared.
const PREFETCH: Duration = Duration::from_millis(500);
/// How far behind the playhead textures survive; the current one always stays.
const WINDOW_KEEP: Duration = Duration::from_secs(1);
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

/// A fully decoded animation: every frame's timing in RAM, pixels in RAM up
/// to the budget and spilled to disk past it. Duration is never truncated.
struct Decoded {
    source: AnimSource,
}

impl Decoded {
    /// One display frame by index, from RAM or from the spool file.
    #[cfg(test)]
    fn image(&self, index: usize) -> Option<ColorImage> {
        load_frame(&self.source, index)
    }
}

struct AnimSource {
    /// Start time of each frame, in order; the loop length follows separately.
    starts: Vec<Duration>,
    /// Full loop length.
    total: Duration,
    width: usize,
    height: usize,
    storage: Storage,
}

impl AnimSource {
    fn frame_count(&self) -> usize {
        self.starts.len()
    }
}

enum Storage {
    /// Every frame, for animations within the RAM budget.
    Ram(Vec<ColorImage>),
    /// Head frames in RAM, the tail paged from disk by frame index.
    Spool {
        head: Vec<ColorImage>,
        file: PathBuf,
    },
}

struct Playing {
    source: AnimSource,
    /// Resident textures by frame index, in order. Only the frames around
    /// the playhead stay uploaded; the window pages forward as it plays.
    window: VecDeque<(usize, TextureHandle)>,
    /// Last playhead, to spot loop wraps and rebase the window.
    last_position: Duration,
    started: Instant,
    last_drawn: Instant,
}

enum Entry {
    Decoding(Instant),
    Failed(Instant),
    Ready(Playing),
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

/// RAM bytes one cache entry holds: its texture window plus a RAM source.
/// Spool files live on disk and count nothing here; decoding entries count
/// zero too, at most two decode at once and join the budget on delivery.
fn entry_bytes(entry: &Entry) -> usize {
    match entry {
        Entry::Ready(playing) => {
            playing
                .window
                .iter()
                .map(|(_, texture)| texture_bytes(texture))
                .sum::<usize>()
                + source_bytes(&playing.source)
        }
        Entry::Decoding(_) | Entry::Failed(_) => 0,
    }
}

/// RAM bytes an animation source holds; spooled tails page from disk.
fn source_bytes(source: &AnimSource) -> usize {
    match &source.storage {
        Storage::Ram(images) => images.iter().map(image_bytes).sum(),
        Storage::Spool { head, .. } => head.iter().map(image_bytes).sum(),
    }
}

/// When an entry was last seen on screen, for least-recently-used eviction.
fn entry_touched(entry: &Entry) -> Option<Instant> {
    match entry {
        Entry::Ready(playing) => Some(playing.last_drawn),
        Entry::Decoding(_) | Entry::Failed(_) => None,
    }
}

/// Loads one display frame by index, from RAM or from the spool file.
fn load_frame(source: &AnimSource, index: usize) -> Option<ColorImage> {
    match &source.storage {
        Storage::Ram(images) => images.get(index).cloned(),
        Storage::Spool { head, file, .. } => {
            if let Some(image) = head.get(index) {
                return Some(image.clone());
            }
            let tail = index - head.len();
            let frame_bytes = source.width * source.height * 4;
            let mut input = BufReader::new(File::open(file).ok()?);
            input
                .seek(SeekFrom::Start(tail as u64 * frame_bytes as u64))
                .ok()?;
            let mut pixels = vec![0u8; frame_bytes];
            input.read_exact(&mut pixels).ok()?;
            Some(ColorImage::from_rgba_unmultiplied(
                [source.width, source.height],
                &pixels,
            ))
        }
    }
}

/// Index of the frame showing at a playhead position.
fn frame_at(starts: &[Duration], position: Duration) -> usize {
    let mut index = 0;
    for (i, start) in starts.iter().enumerate() {
        if *start <= position {
            index = i;
        } else {
            break;
        }
    }
    index
}

static SPOOL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn spool_path() -> PathBuf {
    let id = SPOOL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("zapfast-anim-{}-{id}.bin", std::process::id()))
}

struct SpillWriter {
    file: BufWriter<File>,
    path: PathBuf,
}

/// Opens the disk overflow for a decoded animation. Crash leftovers share
/// the OS temporary directory with the audio spool and die with it.
fn open_spill() -> Option<SpillWriter> {
    let path = spool_path();
    let file = BufWriter::new(File::create(&path).ok()?);
    Some(SpillWriter { file, path })
}

/// Collects decoded frames, spilling past the RAM budget to disk instead of
/// cutting the tail. Display-sized images go in; the sink decides where.
struct BudgetSink {
    head: Vec<ColorImage>,
    delays: Vec<Duration>,
    spent: usize,
    width: usize,
    height: usize,
    spill: Option<SpillWriter>,
    warned: bool,
}

impl BudgetSink {
    fn new() -> Self {
        Self {
            head: Vec::new(),
            delays: Vec::new(),
            spent: 0,
            width: 0,
            height: 0,
            spill: None,
            warned: false,
        }
    }

    fn push(&mut self, image: ColorImage, delay: Duration) {
        if self.delays.len() >= MAX_SOURCE_FRAMES {
            if !self.warned {
                self.warned = true;
                log::warn!("animation capped at {MAX_SOURCE_FRAMES} frames");
            }
            return;
        }
        if self.head.is_empty() {
            self.width = image.size[0];
            self.height = image.size[1];
        }
        let cost = image.size[0] * image.size[1] * 4;
        if self.spill.is_none() && !self.head.is_empty() && self.spent + cost > MAX_ANIM_BYTES {
            self.spill = open_spill();
        }
        match self.spill.as_mut() {
            Some(spill) => {
                if spill.file.write_all(image.as_raw()).is_err() {
                    // A broken spool must not eat the frame: fall back to RAM.
                    let path = spill.path.clone();
                    self.spill = None;
                    let _ = std::fs::remove_file(path);
                    self.spent += cost;
                    self.head.push(image);
                }
            }
            None => {
                self.spent += cost;
                self.head.push(image);
            }
        }
        self.delays.push(delay);
    }

    fn finish(self) -> Option<AnimSource> {
        if self.delays.is_empty() {
            if let Some(spill) = self.spill {
                drop(spill.file);
                let _ = std::fs::remove_file(spill.path);
            }
            return None;
        }
        let mut total = Duration::ZERO;
        let mut starts = Vec::with_capacity(self.delays.len());
        for delay in &self.delays {
            starts.push(total);
            total += *delay;
        }
        let storage = match self.spill {
            Some(mut spill) => {
                // Flush before the player starts seeking through it.
                let _ = spill.file.flush();
                drop(spill.file);
                Storage::Spool {
                    head: self.head,
                    file: spill.path,
                }
            }
            None => Storage::Ram(self.head),
        };
        Some(AnimSource {
            starts,
            total: total.max(Duration::from_millis(50)),
            width: self.width,
            height: self.height,
            storage,
        })
    }
}

/// What a window tick should drop from the front and load next. Pure plan,
/// executed by the player below; tested without a graphics context.
struct WindowPlan {
    /// Resident entries to drop from the front.
    drop: usize,
    /// Frame indices to materialize, in order.
    load: Vec<usize>,
}

fn window_plan(
    starts: &[Duration],
    total: Duration,
    resident: &[usize],
    position: Duration,
) -> WindowPlan {
    let count = starts.len();
    let frame_end = |index: usize| starts.get(index + 1).copied().unwrap_or(total);
    let mut drop = 0;
    while drop + 1 < resident.len() {
        if frame_end(resident[drop]) + WINDOW_KEEP < position {
            drop += 1;
        } else {
            break;
        }
    }
    let mut load = Vec::new();
    let mut next = resident
        .last()
        .copied()
        .map(|last| last + 1)
        .unwrap_or_else(|| frame_at(starts, position));
    while load.len() < UPLOAD_PER_TICK && next < count && starts[next] <= position + PREFETCH {
        if !resident.contains(&next) {
            load.push(next);
        }
        next += 1;
    }
    WindowPlan { drop, load }
}

/// Least-recently-seen victim outside the protected set. Visible animations
/// are spared while anything else can go; only when everything left is on
/// screen does the stalest visible one yield. The current path is never it.
fn pick_victim(
    candidates: &[(PathBuf, Instant)],
    current: &Path,
    is_visible: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let mut best: Option<(&PathBuf, Instant)> = None;
    let mut fallback: Option<(&PathBuf, Instant)> = None;
    for (path, touched) in candidates {
        if path.as_path() == current {
            continue;
        }
        let slot = if is_visible(path) {
            &mut fallback
        } else {
            &mut best
        };
        if slot.is_none_or(|(_, at)| *touched < at) {
            *slot = Some((path, *touched));
        }
    }
    best.or(fallback).map(|(path, _)| path.clone())
}

impl AnimSource {
    /// How long one frame shows: the next start, or the loop end.
    #[cfg(test)]
    fn delay(&self, index: usize) -> Duration {
        self.starts
            .get(index + 1)
            .copied()
            .unwrap_or(self.total)
            .saturating_sub(self.starts.get(index).copied().unwrap_or(Duration::ZERO))
    }
}

/// Spool file behind an entry, if it pages frames from disk.
fn spool_of(entry: &Entry) -> Option<PathBuf> {
    match entry {
        Entry::Ready(playing) => match &playing.source.storage {
            Storage::Spool { file, .. } => Some(file.clone()),
            Storage::Ram(_) => None,
        },
        Entry::Decoding(_) | Entry::Failed(_) => None,
    }
}

/// Deletes the spool file behind a retired entry, if any.
fn forget_spool(entry: &Entry) {
    if let Some(file) = spool_of(entry) {
        let _ = std::fs::remove_file(file);
    }
}

fn visible_id() -> egui::Id {
    egui::Id::new("anim-visible")
}

/// Records an on-screen animation; victims spare recently seen ones.
fn note_visible(ctx: &egui::Context, path: &Path, now: Instant) {
    ctx.data_mut(|data| {
        let seen = data.get_temp_mut_or_default::<HashMap<PathBuf, Instant>>(visible_id());
        seen.insert(path.to_path_buf(), now);
        seen.retain(|_, at| now.duration_since(*at) < Duration::from_secs(5));
    });
}

fn visible_recently(ctx: &egui::Context, path: &Path) -> bool {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<HashMap<PathBuf, Instant>>(visible_id())
            .get(path)
            .is_some_and(|at| at.elapsed() < Duration::from_secs(2))
    })
}

/// Pages the texture window forward: drops what fell behind, materializes a
/// few frames ahead. A loop wrap rebases the window instead of mixing cycles.
fn maintain_window(ctx: &egui::Context, path: &Path, playing: &mut Playing, position: Duration) {
    if position < playing.last_position {
        playing.window.clear();
    }
    playing.last_position = position;
    let resident: Vec<usize> = playing.window.iter().map(|(index, _)| *index).collect();
    let plan = window_plan(
        &playing.source.starts,
        playing.source.total,
        &resident,
        position,
    );
    for _ in 0..plan.drop {
        playing.window.pop_front();
    }
    for index in plan.load {
        let Some(image) = load_frame(&playing.source, index) else {
            break;
        };
        let name = format!("{}#{index}", path.display());
        playing
            .window
            .push_back((index, ctx.load_texture(name, image, TextureOptions::LINEAR)));
    }
}

/// The resident texture showing at a playhead position, with how long it stays.
fn window_frame(playing: &Playing, position: Duration) -> Option<(TextureHandle, Duration)> {
    let mut chosen: Option<(usize, TextureHandle)> = None;
    for (index, texture) in &playing.window {
        if playing.source.starts[*index] <= position {
            chosen = Some((*index, texture.clone()));
        } else {
            break;
        }
    }
    let (index, texture) = chosen?;
    let end = playing
        .source
        .starts
        .get(index + 1)
        .copied()
        .unwrap_or(playing.source.total);
    Some((texture, end.saturating_sub(position)))
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
            Some(decoded) if decoded.source.frame_count() > 0 => Entry::Ready(Playing {
                source: decoded.source,
                window: VecDeque::new(),
                last_position: Duration::ZERO,
                started: Instant::now(),
                last_drawn: Instant::now(),
            }),
            _ => Entry::Failed(Instant::now()),
        };
        // Replacing an entry must not leak its spool file.
        if let Some(old) = entries.insert(arrived_path, entry) {
            forget_spool(&old);
        }
    }
    // Remove idle and least-recently-used animations.
    let now = Instant::now();
    note_visible(ctx, path, now);
    // Snapshot spool files first: pruning drops the entries that own them.
    let spools: Vec<(PathBuf, PathBuf)> = entries
        .iter()
        .filter_map(|(entry_path, entry)| Some((entry_path.clone(), spool_of(entry)?)))
        .collect();
    entries.retain(|_, entry| match entry {
        Entry::Ready(playing) => now.duration_since(playing.last_drawn) < IDLE,
        Entry::Failed(at) | Entry::Decoding(at) => now.duration_since(*at) < RETRY_AFTER,
    });
    // What pruning took off the map takes its spool file with it.
    for (entry_path, spool) in spools {
        if !entries.contains_key(&entry_path) {
            let _ = std::fs::remove_file(spool);
        }
    }
    let mut resident: usize = entries.values().map(entry_bytes).sum();
    while resident > MAX_RESIDENT_BYTES {
        let candidates: Vec<(PathBuf, Instant)> = entries
            .iter()
            .filter_map(|(entry_path, entry)| Some((entry_path.clone(), entry_touched(entry)?)))
            .collect();
        let Some(victim) = pick_victim(&candidates, path, &|candidate| {
            visible_recently(ctx, candidate)
        }) else {
            break;
        };
        let freed = entries.get(&victim).map(entry_bytes).unwrap_or(0);
        if let Some(entry) = entries.remove(&victim) {
            forget_spool(&entry);
        }
        resident = resident.saturating_sub(freed);
    }
    match entries.get_mut(path) {
        Some(Entry::Ready(playing)) => {
            playing.last_drawn = now;
            let elapsed = now.duration_since(playing.started);
            let total = playing.source.total;
            let position = Duration::from_nanos((elapsed.as_nanos() % total.as_nanos()) as u64);
            maintain_window(ctx, path, playing, position);
            match window_frame(playing, position) {
                Some((texture, until_next)) => {
                    if playing.source.frame_count() > 1 {
                        ctx.request_repaint_after(until_next.max(Duration::from_millis(10)));
                    }
                    Frame::Ready(texture)
                }
                None => {
                    // The first paint is still materializing.
                    ctx.request_repaint_after(Duration::from_millis(16));
                    Frame::Pending
                }
            }
        }
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
    let mut sink = BudgetSink::new();
    for frame in frames {
        let frame = frame.ok()?;
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        let delay = Duration::from_millis(u64::from(numerator / denominator.max(1)).max(20));
        let image = frame.into_buffer();
        sink.push(to_color_image(&image), delay);
    }
    sink.finish().map(|source| Decoded { source })
}

/// Decodes animated WebP with libwebp. It returns complete canvas frames,
/// unlike the `image` decoder, which did not apply frame disposal correctly.
fn decode_webp(path: &Path) -> Option<Decoded> {
    let bytes = std::fs::read(path).ok()?;
    let decoder = webp_animation::Decoder::new(&bytes).ok()?;
    let (width, height) = decoder.dimensions();
    let mut previous = 0i64;
    let mut sink = BudgetSink::new();
    for frame in decoder.into_iter() {
        let image = image::RgbaImage::from_raw(width, height, frame.data().to_vec())?;
        let delay = (i64::from(frame.timestamp()) - previous).max(20) as u64;
        previous = i64::from(frame.timestamp());
        sink.push(to_color_image(&image), Duration::from_millis(delay));
    }
    // Single-frame files use the static-image path.
    sink.finish()
        .filter(|source| source.frame_count() > 1)
        .map(|source| Decoded { source })
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
    let mut delays: std::collections::VecDeque<Duration> = std::collections::VecDeque::new();
    let mut sink = BudgetSink::new();
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
            if let Some((image, delay)) = frame_of(&yuv, delay) {
                sink.push(image, delay);
            }
        }
    }
    if let Ok(rest) = decoder.flush_remaining() {
        for yuv in &rest {
            let delay = delays.pop_front().unwrap_or(Duration::from_millis(66));
            if let Some((image, delay)) = frame_of(yuv, delay) {
                sink.push(image, delay);
            }
        }
    }
    sink.finish().map(|source| Decoded { source })
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
    let delay = Duration::from_millis(1000 / u64::from(fps));
    let mut buffer = vec![0u8; (out_width * out_height * 4) as usize];
    let mut sink = BudgetSink::new();
    loop {
        if stdout.read_exact(&mut buffer).is_err() {
            break;
        }
        sink.push(
            ColorImage::from_rgba_unmultiplied([out_width as usize, out_height as usize], &buffer),
            delay,
        );
    }
    let _ = child.kill();
    let _ = child.wait();
    sink.finish().map(|source| Decoded { source })
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
        assert_eq!(decoded.source.frame_count(), 2);
        let second = decoded.image(1).expect("second frame");
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
        assert_eq!(decoded.source.frame_count(), 2);
        assert_eq!(decoded.source.delay(0), Duration::from_millis(100));
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
        assert_eq!(decoded.source.frame_count(), 200);
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
        assert_eq!(decoded.source.frame_count(), 160);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_sticker_sized_webp_keeps_its_tail() {
        // The audit's case: 200 frames at 512 wide must all survive, with
        // the last frame and the total duration intact.
        use webp_animation::prelude::*;
        let side = 512u32;
        let mut encoder = Encoder::new((side, side)).expect("encoder");
        for index in 0..200i32 {
            let shade = (index % 251) as u8;
            // Opaque pixels: the shade goes in RGB, never in alpha.
            let mut frame = vec![255u8; (side * side * 4) as usize];
            for pixel in frame.as_chunks_mut::<4>().0 {
                pixel[0] = shade;
                pixel[1] = shade;
                pixel[2] = shade;
            }
            encoder.add_frame(&frame, index * 50).expect("frame");
        }
        let webp = encoder.finalize(200 * 50).expect("finalizes");
        let dir = std::env::temp_dir().join(format!("zapfast-512-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("long.webp");
        std::fs::write(&path, &webp).expect("writes");
        let decoded = decode(&path).expect("decodes");
        assert_eq!(decoded.source.frame_count(), 200);
        // Display caps at 320 wide; the tail shade survives within lossy drift.
        let last = decoded.image(199).expect("last frame");
        assert_eq!(last.size, [320, 320]);
        for pixel in &last.pixels {
            for channel in [pixel.r(), pixel.g(), pixel.b()] {
                assert!(
                    (channel as i16 - 199).abs() <= 12,
                    "tail shade drifts: {pixel:?}"
                );
            }
        }
        assert!(decoded.source.total >= Duration::from_millis(9_900));
        if let Storage::Spool { file, .. } = &decoded.source.storage {
            let _ = std::fs::remove_file(file);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_tall_animation_pages_its_tail_from_disk() {
        // Verticals grow past 320 tall (only the width is capped): 200
        // frames at 160 by 400 exceed RAM and must page from the spool.
        let dir = std::env::temp_dir().join(format!("zapfast-tall-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("tall.gif");
        {
            let file = std::fs::File::create(&path).expect("file");
            let mut encoder = image::codecs::gif::GifEncoder::new(file);
            encoder
                .set_repeat(image::codecs::gif::Repeat::Infinite)
                .expect("repeat");
            for shade in 0..200u8 {
                let frame = image::Frame::from_parts(
                    image::RgbaImage::from_pixel(160, 400, image::Rgba([shade, 10, 200, 255])),
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(50, 1),
                );
                encoder.encode_frame(frame).expect("frame");
            }
        }
        let decoded = decode(&path).expect("decodes");
        assert_eq!(decoded.source.frame_count(), 200);
        let Storage::Spool { file, .. } = &decoded.source.storage else {
            panic!("a 50 MB animation belongs on disk, not in RAM");
        };
        assert!(file.is_file(), "the spool file backs the tail");
        // The tail reads back with its shade: frame 199 wears 199.
        let last = decoded.image(199).expect("last frame");
        assert_eq!(last.size, [160, 400]);
        assert!(
            last.pixels
                .iter()
                .all(|pixel| *pixel == egui::Color32::from_rgb(199, 10, 200))
        );
        let _ = std::fs::remove_file(file);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_window_prefetches_and_sheds_without_gaps() {
        // Ten frames, 100 ms apart.
        let starts: Vec<Duration> = (0..10).map(|i| Duration::from_millis(i * 100)).collect();
        let total = Duration::from_millis(1000);
        // An empty window loads forward from the playhead, bounded by prefetch.
        let plan = window_plan(&starts, total, &[], Duration::from_millis(250));
        assert_eq!(plan.drop, 0);
        assert_eq!(plan.load, vec![2, 3, 4, 5, 6, 7]);
        // A longer run sheds what fell behind and extends ahead, twelve
        // textures per tick at most.
        let long: Vec<Duration> = (0..100).map(|i| Duration::from_millis(i * 100)).collect();
        let long_total = Duration::from_millis(10_000);
        let resident: Vec<usize> = (0..10).collect();
        let plan = window_plan(&long, long_total, &resident, Duration::from_millis(3000));
        assert_eq!(plan.drop, 9, "only the current picture is sacred");
        assert_eq!(plan.load.len(), UPLOAD_PER_TICK);
        assert_eq!(plan.load[0], 10);
    }

    #[test]
    fn victims_spare_visible_animations() {
        let now = Instant::now();
        let old = |secs: u64| now - Duration::from_secs(secs);
        let a = PathBuf::from("a");
        let b = PathBuf::from("b");
        let c = PathBuf::from("c");
        let candidates = vec![
            (a.clone(), old(30)),
            (b.clone(), old(20)),
            (c.clone(), old(10)),
        ];
        // Nothing visible: the stalest non-current goes.
        assert_eq!(
            pick_victim(&candidates, &PathBuf::from("z"), &|_| false),
            Some(a.clone())
        );
        // The current path is never it.
        assert_eq!(pick_victim(&candidates, &a, &|_| false), Some(b.clone()));
        // Visible ones are spared while anything else can go.
        let some_visible = |p: &Path| p == b.as_path() || p == c.as_path();
        assert_eq!(
            pick_victim(&candidates, &PathBuf::from("z"), &some_visible),
            Some(a.clone())
        );
        // Everything visible: the stalest visible yields to hold the budget.
        assert_eq!(
            pick_victim(&candidates, &PathBuf::from("z"), &|_| true),
            Some(a.clone())
        );
    }

    #[test]
    fn two_big_visible_animations_do_not_thrash() {
        // Two 200-frame animations page from disk; both stay Ready across
        // ticks while visible, with small texture windows and no
        // decode-evict-decode cycle.
        let ctx = egui::Context::default();
        let paths = ["thrash-a.gif", "thrash-b.gif"];
        let mut spool_files = Vec::new();
        {
            let cache = super::cache(&ctx);
            let mut entries = cache.0.lock().unwrap();
            for name in paths {
                let mut sink = BudgetSink::new();
                for index in 0..200 {
                    let shade = (index % 251) as u8;
                    let image = ColorImage::new(
                        [320, 320],
                        vec![egui::Color32::from_rgb(shade, shade, shade); 320 * 320],
                    );
                    sink.push(image, Duration::from_millis(50));
                }
                let source = sink.finish().expect("source");
                assert!(
                    matches!(source.storage, Storage::Spool { .. }),
                    "big enough to spill"
                );
                if let Storage::Spool { file, .. } = &source.storage {
                    spool_files.push(file.clone());
                }
                entries.insert(
                    PathBuf::from(name),
                    Entry::Ready(Playing {
                        source,
                        window: VecDeque::new(),
                        last_position: Duration::ZERO,
                        started: Instant::now(),
                        last_drawn: Instant::now(),
                    }),
                );
            }
        }
        // Alternate ticks on both, the way a chat with two stickers paints.
        for round in 0..6 {
            for name in paths {
                let at = round as f64 * 2.0 + if name == paths[0] { 0.0 } else { 0.05 };
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        time: Some(at),
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(400.0, 400.0),
                        )),
                        ..Default::default()
                    },
                    |ui| {
                        let rect = egui::Rect::from_min_size(
                            egui::pos2(0.0, 0.0),
                            egui::vec2(100.0, 100.0),
                        );
                        let _ = super::frame(ui, Path::new(name), rect);
                    },
                );
                output.textures_delta.clear();
            }
        }
        let cache = super::cache(&ctx);
        let entries = cache.0.lock().unwrap();
        for name in paths {
            match entries.get(Path::new(name)) {
                Some(Entry::Ready(playing)) => {
                    assert!(
                        playing.window.len() <= 2 * UPLOAD_PER_TICK,
                        "window stays small"
                    );
                }
                None => panic!("{name} must stay Ready, got missing"),
                Some(_) => panic!("{name} must stay Ready, got not ready"),
            }
        }
        assert!(
            !entries
                .values()
                .any(|entry| matches!(entry, Entry::Decoding(_))),
            "no re-decode cycle"
        );
        drop(entries);
        for file in spool_files {
            let _ = std::fs::remove_file(file);
        }
    }

    #[test]
    fn the_resident_budget_counts_display_bytes() {
        let image = ColorImage::new([10, 20], vec![egui::Color32::BLACK; 200]);
        assert_eq!(image_bytes(&image), 800);
        let source = AnimSource {
            starts: vec![Duration::ZERO],
            total: Duration::from_millis(50),
            width: 10,
            height: 20,
            storage: Storage::Ram(vec![image]),
        };
        let entry = Entry::Ready(Playing {
            source,
            window: VecDeque::new(),
            last_position: Duration::ZERO,
            started: Instant::now(),
            last_drawn: Instant::now(),
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
        assert_eq!(decoded.source.frame_count(), 5);
        assert_eq!(decoded.source.delay(0), Duration::from_millis(100));
        assert_eq!(decoded.image(0).expect("first frame").size, [64, 48]);
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
            decoded.source.frame_count(),
            decoded.image(0).map(|image| image.size),
            decoded.source.delay(0),
            started.elapsed()
        );
        assert!(decoded.source.frame_count() > 0);
    }
}
