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
        file: SpoolFile,
    },
}

/// A spool file that deletes itself. Sources own their tails through this
/// type, so eviction, replacement, pruning and shutdown all clean up with
/// no call site remembering to.
struct SpoolFile {
    path: PathBuf,
}

impl AsRef<Path> for SpoolFile {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for SpoolFile {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl Drop for SpoolFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
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
#[cfg(test)]
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
            // Pixels are stored premultiplied (ColorImage::as_raw), so they
            // must come back the same way: unmultiplied would darken every
            // soft edge twice.
            Some(ColorImage::from_rgba_premultiplied(
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

fn spool_file_name() -> PathBuf {
    let id = SPOOL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = std::ffi::OsString::from("zapfast-anim-");
    name.push(std::process::id().to_string());
    name.push("-");
    name.push(id.to_string());
    name.push(".bin");
    PathBuf::from(name)
}

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
fn open_spill(parent: Option<&Path>) -> Option<SpillWriter> {
    let path = match parent {
        Some(dir) => dir.join(spool_file_name()),
        None => spool_path(),
    };
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
    /// Sealed: no more frames accepted, the recorded prefix plays as is.
    sealed: bool,
    /// A spilled write failed: the on-disk tail cannot be trusted.
    failed: bool,
    /// Where spill files go; tests point it at a dead end to fail opens.
    spill_parent: Option<PathBuf>,
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
            sealed: false,
            failed: false,
            spill_parent: None,
        }
    }

    /// Files one display frame, spilling past the RAM budget to disk.
    /// False means stop feeding: sealed (frame cap, unopenable spill) or
    /// failed (a spilled write broke the on-disk tail). Either way the
    /// recorded prefix stays aligned; only failure aborts the decode.
    fn push(&mut self, image: ColorImage, delay: Duration) -> bool {
        if self.sealed || self.failed {
            return false;
        }
        if self.delays.len() >= MAX_SOURCE_FRAMES {
            if !self.warned {
                self.warned = true;
                log::warn!("animation capped at {MAX_SOURCE_FRAMES} frames");
            }
            self.sealed = true;
            return false;
        }
        if self.head.is_empty() {
            self.width = image.size[0];
            self.height = image.size[1];
        }
        let cost = image.size[0] * image.size[1] * 4;
        if self.spill.is_none() && !self.head.is_empty() && self.spent + cost > MAX_ANIM_BYTES {
            match open_spill(self.spill_parent.as_deref()) {
                Some(spill) => self.spill = Some(spill),
                // No spill, no tail: the recorded head plays as is.
                None => log::warn!("animation spool unavailable, keeping the head"),
            }
            if self.spill.is_none() {
                self.sealed = true;
                return false;
            }
        }
        match self.spill.as_mut() {
            Some(spill) => {
                if spill.file.write_all(image.as_raw()).is_err() {
                    self.failed = true;
                    return false;
                }
            }
            None => {
                self.spent += cost;
                self.head.push(image);
            }
        }
        self.delays.push(delay);
        true
    }

    fn finish(mut self) -> Option<AnimSource> {
        if self.failed {
            // A spilled write broke the on-disk tail: nothing recorded
            // past it can be trusted, so the attempt is abandoned whole.
            if let Some(spill) = self.spill.take() {
                drop(spill.file);
                let _ = std::fs::remove_file(spill.path);
            }
            return None;
        }
        if self.delays.is_empty() {
            if let Some(spill) = self.spill.take() {
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
        let storage = match self.spill.take() {
            Some(mut spill) => {
                // Flush before the player starts seeking through it.
                // A short tail aborts instead of shipping half a spool.
                if spill.file.flush().is_err() {
                    drop(spill.file);
                    let _ = std::fs::remove_file(spill.path);
                    return None;
                }
                drop(spill.file);
                Storage::Spool {
                    head: std::mem::take(&mut self.head),
                    file: SpoolFile { path: spill.path },
                }
            }
            None => Storage::Ram(std::mem::take(&mut self.head)),
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

impl Drop for BudgetSink {
    fn drop(&mut self) {
        // An abandoned decode (app closing mid-spill, a dropped delivery)
        // must not leave its partial file behind. Close first: Windows
        // cannot delete an open file.
        if let Some(spill) = self.spill.take() {
            drop(spill.file);
            let _ = std::fs::remove_file(spill.path);
        }
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

/// One spooled frame requested by the interface thread. The tail index
/// counts from the spool start, not from the animation start.
struct FrameJob {
    file: PathBuf,
    width: usize,
    height: usize,
    tail: usize,
}

#[derive(Clone)]
struct FrameReady {
    file: PathBuf,
    tail: usize,
    image: Option<ColorImage>,
}

struct PrefetchWorker {
    jobs: std::sync::mpsc::Sender<FrameJob>,
    // The queue lock is held only for non-blocking drains.
    done: Mutex<std::sync::mpsc::Receiver<FrameReady>>,
}

static PREFETCH_WORKER: std::sync::OnceLock<PrefetchWorker> = std::sync::OnceLock::new();

/// The single background pager for spooled tails. One thread serves every
/// viewer; results are keyed by file and index, so no per-viewer routing
/// is needed. A job reads one frame: even a stalled disk only delays its
/// own tiny read, never the interface.
fn prefetch_worker() -> &'static PrefetchWorker {
    PREFETCH_WORKER.get_or_init(|| {
        let (jobs_tx, jobs_rx) = std::sync::mpsc::channel::<FrameJob>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<FrameReady>();
        std::thread::Builder::new()
            .name("anim-prefetch".into())
            .spawn(move || {
                while let Ok(job) = jobs_rx.recv() {
                    let image = read_spool_frame(&job.file, job.width, job.height, job.tail);
                    let _ = done_tx.send(FrameReady {
                        file: job.file,
                        tail: job.tail,
                        image,
                    });
                }
            })
            .expect("animation prefetch thread spawns");
        PrefetchWorker {
            jobs: jobs_tx,
            done: Mutex::new(done_rx),
        }
    })
}

/// Reads one spooled tail frame straight from disk.
fn read_spool_frame(file: &Path, width: usize, height: usize, tail: usize) -> Option<ColorImage> {
    let frame_bytes = width * height * 4;
    let mut input = BufReader::new(File::open(file).ok()?);
    input
        .seek(SeekFrom::Start(tail as u64 * frame_bytes as u64))
        .ok()?;
    let mut pixels = vec![0u8; frame_bytes];
    input.read_exact(&mut pixels).ok()?;
    // Same premultiplied roundtrip as the synchronous path below.
    Some(ColorImage::from_rgba_premultiplied(
        [width, height],
        &pixels,
    ))
}

fn pending_id() -> egui::Id {
    egui::Id::new("anim-prefetch-pending")
}

fn stash_id() -> egui::Id {
    egui::Id::new("anim-prefetch-stash")
}

/// What a spooled lookup found: ready, permanently missing, or still queued.
enum TakeReady {
    Ready(ColorImage),
    Missing,
    Pending,
}

/// Takes one spooled frame without blocking: completed prefetches, the
/// stash of other animations results, or a fresh queue slot. Corrupt tails
/// report Missing once instead of spinning the window forever.
fn take_ready(ctx: &egui::Context, file: &Path, tail: usize) -> TakeReady {
    let mut outcome: Option<TakeReady> = None;
    ctx.data_mut(|data| {
        let stash = data.get_temp_mut_or_default::<Vec<FrameReady>>(stash_id());
        if stash.len() > 256 {
            stash.clear();
        }
        if let Some(pos) = stash
            .iter()
            .position(|ready| ready.file.as_path() == file && ready.tail == tail)
        {
            let ready = stash.remove(pos);
            let pending =
                data.get_temp_mut_or_default::<HashMap<(PathBuf, usize), Instant>>(pending_id());
            pending.remove(&(file.to_path_buf(), tail));
            outcome = Some(match ready.image {
                Some(image) => TakeReady::Ready(image),
                None => TakeReady::Missing,
            });
            return;
        }
        let worker = prefetch_worker();
        let done = worker
            .done
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while let Ok(ready) = done.try_recv() {
            if ready.file.as_path() == file && ready.tail == tail {
                let pending = data
                    .get_temp_mut_or_default::<HashMap<(PathBuf, usize), Instant>>(pending_id());
                pending.remove(&(file.to_path_buf(), tail));
                outcome = Some(match ready.image {
                    Some(image) => TakeReady::Ready(image),
                    None => TakeReady::Missing,
                });
            } else if data
                .get_temp_mut_or_default::<Vec<FrameReady>>(stash_id())
                .len()
                <= 256
            {
                data.get_temp_mut_or_default::<Vec<FrameReady>>(stash_id())
                    .push(ready);
            }
        }
    });
    outcome.unwrap_or(TakeReady::Pending)
}

/// Queues one tail frame for background loading, once at a time. Entries
/// older than two seconds requeue: their job either landed elsewhere or
/// died with a retired viewer, and a duplicate read is harmless.
fn enqueue_prefetch(ctx: &egui::Context, file: &Path, width: usize, height: usize, tail: usize) {
    let key = (file.to_path_buf(), tail);
    let mut send = false;
    ctx.data_mut(|data| {
        let pending =
            data.get_temp_mut_or_default::<HashMap<(PathBuf, usize), Instant>>(pending_id());
        pending.retain(|_, at| at.elapsed() < Duration::from_secs(2));
        if pending.len() > 128 {
            pending.clear();
        }
        if pending.contains_key(&key) {
            return;
        }
        pending.insert(key.clone(), Instant::now());
        send = true;
    });
    if send {
        // The worker only exits with the process; a failed send would mean
        // it died, which its panic-free loop cannot do.
        let _ = prefetch_worker().jobs.send(FrameJob {
            file: key.0,
            width,
            height,
            tail: key.1,
        });
    }
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
    let mut spent = 0;
    for index in plan.load {
        if spent >= UPLOAD_PER_TICK {
            ctx.request_repaint_after(Duration::from_millis(16));
            break;
        }
        match materialize(ctx, &playing.source, index) {
            Material::Ready(image) => {
                let name = format!("{}#{index}", path.display());
                playing
                    .window
                    .push_back((index, ctx.load_texture(name, image, TextureOptions::LINEAR)));
                spent += 1;
            }
            // A corrupt tail leaves a hole and pages on: one bad frame must
            // not wedge the whole window.
            Material::Missing => {}
            // Still on its way: wait for the painter instead of spinning.
            Material::Pending => {
                ctx.request_repaint_after(Duration::from_millis(16));
                break;
            }
        }
    }
}

/// What one planned index gave: pixels, a hole, or patience.
enum Material {
    Ready(ColorImage),
    Missing,
    Pending,
}

/// Materializes one frame for upload. RAM heads copy inline; spooled tails
/// arrive on the background pager, so a busy disk never blocks the tick.
fn materialize(ctx: &egui::Context, source: &AnimSource, index: usize) -> Material {
    match &source.storage {
        Storage::Ram(images) => match images.get(index).cloned() {
            Some(image) => Material::Ready(image),
            None => Material::Missing,
        },
        Storage::Spool { head, file } => {
            if let Some(image) = head.get(index).cloned() {
                return Material::Ready(image);
            }
            let tail = index - head.len();
            match take_ready(ctx, file, tail) {
                TakeReady::Ready(image) => Material::Ready(image),
                TakeReady::Missing => Material::Missing,
                TakeReady::Pending => {
                    enqueue_prefetch(ctx, file, source.width, source.height, tail);
                    Material::Pending
                }
            }
        }
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
        // Replacing an entry drops the old one, and its spool with it.
        entries.insert(arrived_path, entry);
    }
    // Remove idle and least-recently-used animations.
    let now = Instant::now();
    note_visible(ctx, path, now);
    entries.retain(|_, entry| match entry {
        Entry::Ready(playing) => now.duration_since(playing.last_drawn) < IDLE,
        Entry::Failed(at) | Entry::Decoding(at) => now.duration_since(*at) < RETRY_AFTER,
    });
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
        // Dropping the victim deletes its spool file with it.
        let _ = entries.remove(&victim);
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
        // Sealed, capped or failed: stop decoding instead of processing
        // frames that will never be stored.
        if !sink.push(to_color_image(&image), delay) {
            break;
        }
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
        if !sink.push(to_color_image(&image), Duration::from_millis(delay)) {
            break;
        }
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
            if let Some((image, delay)) = frame_of(&yuv, delay)
                && !sink.push(image, delay)
            {
                break;
            }
        }
    }
    if let Ok(rest) = decoder.flush_remaining() {
        for yuv in &rest {
            let delay = delays.pop_front().unwrap_or(Duration::from_millis(66));
            if let Some((image, delay)) = frame_of(yuv, delay)
                && !sink.push(image, delay)
            {
                break;
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
        if !sink.push(
            ColorImage::from_rgba_unmultiplied([out_width as usize, out_height as usize], &buffer),
            delay,
        ) {
            break;
        }
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
        assert!(file.path.is_file(), "the spool file backs the tail");
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
                    spool_files.push(file.path.clone());
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
        // Alternate ticks on both in real time, the way a chat with two
        // stickers paints. Sleeping between rounds advances the playheads
        // on the wall clock, so windows page forward for real.
        for _ in 0..6 {
            for name in paths {
                let mut output = ctx.run_ui(
                    egui::RawInput {
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
            std::thread::sleep(Duration::from_millis(100));
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
                    assert!(!playing.window.is_empty(), "pages paint through the worker");
                    assert!(
                        playing.last_position > Duration::ZERO,
                        "playheads advance on the wall clock"
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
    fn spooled_frames_keep_their_transparency() {
        // Premultiplied bytes must roundtrip without darkening soft edges,
        // in memory and through a real spool file.
        let pixels = vec![
            egui::Color32::from_rgba_unmultiplied(200, 100, 50, 128),
            egui::Color32::from_rgba_unmultiplied(10, 20, 30, 0),
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 255),
            egui::Color32::from_rgba_unmultiplied(0, 0, 0, 77),
        ];
        let image = ColorImage::new([2, 2], pixels.clone());
        let back = ColorImage::from_rgba_premultiplied([2, 2], image.as_raw());
        assert_eq!(back.pixels, pixels);
        let dir = tempfile::tempdir().expect("dir");
        let file = dir.path().join("spool.bin");
        std::fs::write(&file, image.as_raw()).expect("writes");
        let source = AnimSource {
            starts: vec![Duration::ZERO],
            total: Duration::from_millis(50),
            width: 2,
            height: 2,
            storage: Storage::Spool {
                head: Vec::new(),
                file: SpoolFile { path: file.clone() },
            },
        };
        assert_eq!(load_frame(&source, 0).expect("frame").pixels, pixels);
    }

    #[test]
    fn the_frame_cap_seals_instead_of_skewing() {
        // Past 4096 frames the sink stops recording: the sealed prefix
        // stays aligned, every recorded index loads.
        let mut sink = BudgetSink::new();
        let image = ColorImage::new([4, 4], vec![egui::Color32::BLACK; 16]);
        let mut accepted = 0;
        for _ in 0..MAX_SOURCE_FRAMES + 100 {
            if sink.push(image.clone(), Duration::from_millis(50)) {
                accepted += 1;
            }
        }
        assert_eq!(accepted, MAX_SOURCE_FRAMES);
        let source = sink.finish().expect("sealed prefix plays");
        assert_eq!(source.frame_count(), MAX_SOURCE_FRAMES);
        assert!(load_frame(&source, MAX_SOURCE_FRAMES - 1).is_some());
    }

    #[test]
    fn an_unopenable_spool_seals_the_head() {
        // Nowhere to spill: the RAM head plays as is, aligned and whole.
        let mut sink = BudgetSink::new();
        sink.spill_parent = Some(PathBuf::from("/nonexistent-dir-xyz-123"));
        let image = ColorImage::new([320, 320], vec![egui::Color32::BLACK; 320 * 320]);
        let mut accepted = 0;
        for _ in 0..50 {
            if sink.push(image.clone(), Duration::from_millis(50)) {
                accepted += 1;
            }
        }
        assert!(
            accepted > 0 && accepted < 50,
            "seals at the budget: {accepted}"
        );
        let source = sink.finish().expect("head plays");
        assert_eq!(source.frame_count(), accepted);
        assert!(matches!(source.storage, Storage::Ram(_)));
        for index in 0..accepted {
            assert!(load_frame(&source, index).is_some(), "index {index} loads");
        }
    }

    #[test]
    fn a_failed_spool_write_aborts_the_decode() {
        // Forty good frames spill, then the disk breaks: the attempt is
        // abandoned whole instead of skewing every later index.
        let dir = tempfile::tempdir().expect("dir");
        let mut sink = BudgetSink::new();
        let image = ColorImage::new([320, 320], vec![egui::Color32::BLACK; 320 * 320]);
        for _ in 0..45 {
            assert!(sink.push(image.clone(), Duration::from_millis(50)));
        }
        let spilled = sink.spill.as_ref().expect("spilling by now").path.clone();
        assert!(spilled.is_file(), "spool exists mid-decode");
        // Break the disk under it with a read-only handle.
        let broken = dir.path().join("broken.bin");
        std::fs::write(&broken, b"x").expect("writes");
        let mut permissions = std::fs::metadata(&broken).expect("meta").permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&broken, permissions).expect("readonly");
        let read_only = File::open(&broken).expect("opens read-only");
        sink.spill.as_mut().expect("spill").file = BufWriter::new(read_only);
        assert!(!sink.push(image.clone(), Duration::from_millis(50)));
        assert!(sink.finish().is_none(), "half a spool never ships");
        assert!(!spilled.exists(), "the partial spool is deleted");
    }

    #[test]
    fn dropping_a_source_deletes_its_spool() {
        // RAII: eviction, replacement, pruning and shutdown clean up with
        // no call site remembering to.
        let mut sink = BudgetSink::new();
        let image = ColorImage::new([320, 320], vec![egui::Color32::BLACK; 320 * 320]);
        for _ in 0..50 {
            let _ = sink.push(image.clone(), Duration::from_millis(50));
        }
        let source = sink.finish().expect("source");
        let spilled = match &source.storage {
            Storage::Spool { file, .. } => file.path.clone(),
            Storage::Ram(_) => panic!("50 full frames must spill"),
        };
        assert!(spilled.is_file());
        drop(source);
        assert!(!spilled.exists(), "dropping deletes the spool");
    }

    #[test]
    fn abandoning_a_decode_deletes_its_partial_spool() {
        let mut sink = BudgetSink::new();
        let image = ColorImage::new([320, 320], vec![egui::Color32::BLACK; 320 * 320]);
        for _ in 0..50 {
            let _ = sink.push(image.clone(), Duration::from_millis(50));
        }
        let spilled = sink.spill.as_ref().expect("spilling").path.clone();
        assert!(spilled.is_file());
        drop(sink);
        assert!(!spilled.exists(), "abandoned partial spool is deleted");
    }

    #[test]
    fn spooled_frames_arrive_without_blocking_the_interface() {
        // A spool-backed source plus the real worker: the first tick only
        // queues, a later tick paints, and the interface thread never reads.
        let mut sink = BudgetSink::new();
        for index in 0..60 {
            let shade = (index % 251) as u8;
            let image = ColorImage::new(
                [320, 320],
                vec![egui::Color32::from_rgb(shade, shade, shade); 320 * 320],
            );
            let _ = sink.push(image, Duration::from_millis(50));
        }
        let source = sink.finish().expect("source");
        assert!(matches!(source.storage, Storage::Spool { .. }));
        let spool_path = match &source.storage {
            Storage::Spool { file, .. } => file.path.clone(),
            Storage::Ram(_) => unreachable!(),
        };
        let ctx = egui::Context::default();
        let path = Path::new("prefetch.gif");
        let mut playing = Playing {
            source,
            window: VecDeque::new(),
            last_position: Duration::ZERO,
            started: Instant::now(),
            last_drawn: Instant::now(),
        };
        // The RAM head paints on the first tick; the spooled tail arrives
        // through the worker on later ticks.
        maintain_window(&ctx, path, &mut playing, Duration::from_millis(100));
        assert_eq!(playing.window[0].0, 2, "lands on the playhead frame");
        assert!(playing.window.len() <= UPLOAD_PER_TICK);
        assert!(spool_path.exists(), "the tail lives on disk");
        // Deep into the spooled tail: ticks queue, the worker delivers,
        // later ticks paint without reading on this thread.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            maintain_window(&ctx, path, &mut playing, Duration::from_millis(2500));
            if playing.window.iter().any(|(index, _)| *index >= 40) {
                break;
            }
            assert!(Instant::now() < deadline, "tail frames arrive");
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(playing);
        assert!(!spool_path.exists(), "test source cleans its spool");
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
