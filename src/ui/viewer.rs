//! Full-window viewer for a chat's pictures, stickers, and PDFs.

use std::path::Path;
use std::time::{Duration, Instant};

use egui::{Align, Color32, CornerRadius, CursorIcon, Key, Layout, Rect, Sense, Vec2, pos2, vec2};

use crate::app::App;
use crate::model::{Action, VideoScrub, ViewerKind};
use crate::theme::{self, Icon, Palette};

use super::conversation::thumb_revision;
use super::widgets;

/// Space the fitted picture leaves around itself.
const INSET: f32 = 64.0;
/// How much one wheel notch changes the zoom.
const WHEEL_STEP: f32 = 1.15;
/// How much the zoom buttons change it.
const BUTTON_STEP: f32 = 1.3;
/// Zoom a double click jumps to.
const DOUBLE_CLICK_ZOOM: f32 = 2.5;
/// Largest amount the fitted view enlarges a small file by.
const MAX_FIT: f32 = 2.5;
/// Near-black backdrop behind the picture.
const BACKDROP: Color32 = Color32::from_rgba_premultiplied(6, 10, 9, 245);

/// What the viewer has to paint.
enum Surface {
    Ready {
        id: egui::TextureId,
        size: Vec2,
        /// The file carries frames, so keep decoding them.
        animated: bool,
    },
    Pending,
    Failed,
}

/// Full-window playback for the chat video on screen: the viewer opens a video
/// playing, with sound, and offers the system player only for files the in-process
/// decoder cannot read.
fn video_view(
    app: &mut App,
    ctx: &egui::Context,
    screen: Rect,
    path: &Path,
    index: usize,
    count: usize,
) {
    if !app.video.is_active(path) && app.video.refusal(path).is_none() {
        // Opening a video starts it, with sound, like the phone does.
        app.actions.push(Action::VideoToggle);
    }
    // Fresh frame: no control holds the arrows until the bar says so below.
    ctx.data_mut(|data| data.insert_temp(control_focus_id(), false));
    let palette = app.palette;
    app.video
        .set_output(app.settings.video_volume, app.settings.video_muted);
    // The sender's poster shows until the first frame is decoded.
    let poster: Option<(String, String, Vec<u8>)> = app.viewer.as_ref().and_then(|viewer| {
        let id = viewer.current()?.message.clone();
        let row = app.conversations.get(&viewer.chat)?.message(&id)?;
        Some((viewer.chat.clone(), id, row.thumbnail.clone()?))
    });
    let state = app.video.poll(ctx, path);
    let mut actions = Vec::new();
    let title = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Video".to_owned());
    egui::Area::new(egui::Id::new("viewer"))
        .fixed_pos(screen.min)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            ui.set_width(screen.width());
            ui.set_height(screen.height());
            let (rect, backdrop) = ui.allocate_exact_size(screen.size(), Sense::click_and_drag());
            ui.painter().rect_filled(rect, CornerRadius::ZERO, BACKDROP);
            let area = usable_area(rect);
            match &state {
                crate::video::State::Showing {
                    texture,
                    size,
                    position,
                    total,
                    playing,
                    finished,
                    seeking,
                } => {
                    let natural = if size.x > 0.0 && size.y > 0.0 {
                        size
                    } else {
                        &vec2(4.0, 3.0)
                    };
                    let placed = Rect::from_center_size(
                        area.center(),
                        *natural * fit_scale(*natural, area.size()),
                    );
                    // A fresh scrub preview takes the picture while the
                    // drag lasts; otherwise the live frame stays put.
                    let previewed = paint_scrub_preview(app, ui, ctx, path, placed);
                    if ui.is_rect_visible(placed) && !previewed {
                        ui.painter().image(
                            texture.id(),
                            placed,
                            Rect::from_min_max(egui::Pos2::ZERO, pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    }
                    // The picture itself plays or pauses on click, like the phone.
                    if ui
                        .interact(placed, ui.id().with("video"), Sense::click())
                        .on_hover_cursor(CursorIcon::PointingHand)
                        .clicked()
                    {
                        actions.push(Action::VideoToggle);
                    }
                    if *seeking && ui.is_rect_visible(placed) {
                        // The jump landed instantly on its keyframe; this says the live
                        // picture is still catching up.
                        let chip =
                            Rect::from_min_size(placed.min + vec2(10.0, 10.0), vec2(86.0, 24.0));
                        ui.painter().rect_filled(
                            chip,
                            chip.height() / 2.0,
                            Color32::from_black_alpha(140),
                        );
                        ui.painter().text(
                            chip.center(),
                            egui::Align2::CENTER_CENTER,
                            "Seeking",
                            theme::regular(12.0),
                            Color32::WHITE,
                        );
                    }
                    video_bar(
                        app,
                        ui,
                        rect,
                        path,
                        *position,
                        *total,
                        *playing,
                        *finished,
                        &mut actions,
                    );
                }
                crate::video::State::Loading => {
                    // The sender's poster fills the wait for the first frame.
                    if poster
                        .as_ref()
                        .and_then(|(chat, id, bytes)| video_poster(ui, ctx, area, chat, id, bytes))
                        .is_none()
                    {
                        theme::paint_spinner(
                            ui,
                            Rect::from_center_size(area.center(), Vec2::splat(48.0)),
                            34.0,
                            palette.accent,
                        );
                    }
                }
                crate::video::State::Unsupported(why) => {
                    video_unsupported(ui, &palette, area, path, why, &mut actions);
                }
            }
            if backdrop.clicked() {
                actions.push(Action::CloseViewer);
            }
            video_head(ui, &palette, rect, &title, index, count, &mut actions);
        });
    app.actions.extend(actions);
}

/// Title, counter, and close button over the playing video.
/// Registers raw poster bytes with the image loader once and paints them.
///
/// Returns where the poster landed, so the wait for the first decoded frame
/// shows the sender's picture instead of a spinner.
fn video_poster(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    area: Rect,
    chat: &str,
    id: &str,
    bytes: &[u8],
) -> Option<Rect> {
    let uri = poster_uri(ctx, chat, id, bytes);
    let texture = match egui::Image::new(&uri).load_for_size(ctx, area.size()) {
        Ok(egui::load::TexturePoll::Ready { texture }) => texture,
        _ => return None,
    };
    let placed = widgets::picture_rect(area, texture.size);
    egui::Image::new(&uri).paint_at(ui, placed);
    Some(placed)
}
/// The loader URI of a poster's bytes, registering them a single time.
fn poster_uri(ctx: &egui::Context, chat: &str, id: &str, bytes: &[u8]) -> String {
    // The revision pins the bytes: an upgraded poster registers under a
    // new address instead of losing to the first image this viewer saw.
    let uri = format!(
        "bytes://poster-{}-{id}-{}",
        chat.chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>(),
        thumb_revision(bytes)
    );
    // Budgeted registration like chat thumbnails: re-registers after a sweep.
    crate::image_cache::include(ctx, uri.clone(), bytes);
    uri
}
fn video_head(
    ui: &mut egui::Ui,
    palette: &Palette,
    rect: Rect,
    title: &str,
    index: usize,
    count: usize,
    actions: &mut Vec<Action>,
) {
    let top = Rect::from_min_max(
        rect.min + vec2(28.0, 20.0),
        pos2(rect.right() - 28.0, rect.top() + 56.0),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(top), |ui| {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(title)
                    .font(theme::medium(14.0))
                    .color(palette.text),
            );
            ui.label(
                egui::RichText::new(format!("{} of {count}", index + 1))
                    .font(theme::regular(13.0))
                    .color(palette.secondary),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if theme::icon_button(
                    ui,
                    Icon::CircleX,
                    18.0,
                    palette.dim,
                    palette.text,
                    "Close (Esc)",
                )
                .clicked()
                {
                    actions.push(Action::CloseViewer);
                }
            });
        });
    });
}

/// Bar position for a playback position, in full float precision. Whole
/// seconds would snap a click at seventy percent to the wrong second; the
/// fraction keeps the jump where the pointer asked for it.
pub(crate) fn seek_fraction(position: Duration, total: Duration) -> f32 {
    if total.is_zero() {
        return 0.0;
    }
    (position.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0)
}

/// Playback controls under the playing video.
/// What one slider frame means during a scrub drag. Preview and the
/// definitive jump stay separate: motion only previews, release jumps
/// once, Escape jumps nowhere.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum BarSeek {
    None,
    BeginDrag,
    Preview(f32),
    Commit(f32),
    Click(f32),
}

/// Reads one slider frame: release commits exactly once, motion
/// previews, a click jumps at once. Pure so drag sequencing stays
/// testable without a window.
pub(crate) fn bar_seek_action(
    started: bool,
    dragged: bool,
    stopped: bool,
    changed: bool,
    scrubbing: bool,
    fraction: f32,
) -> BarSeek {
    if stopped && scrubbing {
        return BarSeek::Commit(fraction);
    }
    if stopped {
        return BarSeek::Click(fraction);
    }
    if changed && !dragged && scrubbing {
        return BarSeek::Preview(fraction);
    }
    if changed && !dragged {
        return BarSeek::Click(fraction);
    }
    if started && !scrubbing {
        return BarSeek::BeginDrag;
    }
    if dragged && scrubbing {
        return BarSeek::Preview(fraction);
    }
    BarSeek::None
}

/// Paints the scrub preview over the live picture when it shows the
/// drag destination. Anything else keeps the last live frame: no black
/// screen while the new preview travels. Returns whether it painted.
fn paint_scrub_preview(
    app: &mut App,
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    path: &Path,
    placed: Rect,
) -> bool {
    let Some(scrub) = app.video_scrub.as_ref().filter(|scrub| scrub.path == path) else {
        return false;
    };
    if let Some(ready) = app.previewer.poll(path, scrub.generation) {
        let upload = app.video_preview.as_ref().is_none_or(|slot| {
            slot.path != ready.path
                || slot.generation != ready.generation
                || slot.seq != ready.seq
                || slot.approximate != ready.approximate
        });
        if upload {
            let texture = match app.video_preview.as_mut() {
                Some(slot) => {
                    slot.texture
                        .set(ready.image.clone(), egui::TextureOptions::LINEAR);
                    slot.texture.clone()
                }
                None => ctx.load_texture(
                    "video-preview",
                    ready.image.clone(),
                    egui::TextureOptions::LINEAR,
                ),
            };
            app.video_preview = Some(crate::app::PreviewSlot {
                path: ready.path.clone(),
                generation: ready.generation,
                seq: ready.seq,
                fraction: ready.fraction,
                pts: ready.pts,
                approximate: ready.approximate,
                texture,
            });
        }
    }
    let fresh = app.video_preview.as_ref().is_some_and(|slot| {
        slot.path == *path
            && slot.generation == scrub.generation
            && (slot.fraction - scrub.target).abs() <= 0.015
    });
    if fresh
        && ui.is_rect_visible(placed)
        && let Some(slot) = app.video_preview.as_ref()
    {
        ui.painter().image(
            slot.texture.id(),
            placed,
            Rect::from_min_max(egui::Pos2::ZERO, pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        return true;
    }
    false
}

/// Thumbnail with the drag time above the bar, never over the controls.
/// A stale or missing picture shows the time with a quiet standby mark.
fn paint_preview_thumb(app: &mut App, ui: &mut egui::Ui, bar: Rect, path: &Path, total: Duration) {
    let Some(scrub) = app.video_scrub.as_ref().filter(|scrub| scrub.path == path) else {
        return;
    };
    let palette = app.palette;
    let x = (bar.min.x + scrub.target * bar.width()).clamp(bar.min.x + 60.0, bar.max.x - 60.0);
    let thumb = Rect::from_center_size(pos2(x, bar.min.y - 52.0), vec2(120.0, 68.0));
    let target = total.mul_f32(scrub.target);
    let fresh = app.video_preview.as_ref().is_some_and(|slot| {
        slot.path == *path
            && slot.generation == scrub.generation
            && (slot.fraction - scrub.target).abs() <= 0.015
    });
    if fresh && let Some(slot) = app.video_preview.as_ref() {
        ui.painter().rect_filled(thumb, 6.0, Color32::BLACK);
        ui.painter().image(
            slot.texture.id(),
            thumb.shrink(2.0),
            Rect::from_min_max(egui::Pos2::ZERO, pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
    // The knob and clock follow the drag target at once; the thumbnail names
    // both the chosen destination and the decoded frame instant. An early
    // keyframe approximate shows both times so an approximate never reads as
    // exact. The definitive jump still uses the target through VideoSeek.
    let slot_info = app.video_preview.as_ref().filter(|slot| {
        slot.path == *path
            && slot.generation == scrub.generation
            && (slot.fraction - scrub.target).abs() <= 0.015
    });
    let target_text = crate::util::duration(target.as_secs().min(u64::from(u32::MAX)) as u32);
    let label = match slot_info {
        Some(slot)
            if !slot.approximate && slot.pts.abs_diff(target) <= Duration::from_millis(500) =>
        {
            target_text
        }
        Some(slot) => {
            let frame_text =
                crate::util::duration(slot.pts.as_secs().min(u64::from(u32::MAX)) as u32);
            format!("{target_text} · frame {frame_text}")
        }
        None => format!("{target_text} …"),
    };
    ui.painter().text(
        pos2(x, bar.min.y - 8.0),
        egui::Align2::CENTER_CENTER,
        label,
        theme::regular(12.0),
        palette.text,
    );
}

#[allow(clippy::too_many_arguments)]
fn video_bar(
    app: &mut App,
    ui: &mut egui::Ui,
    rect: Rect,
    path: &Path,
    position: Duration,
    total: Duration,
    playing: bool,
    finished: bool,
    actions: &mut Vec<Action>,
) -> egui::Response {
    let palette = app.palette;
    let bar = Rect::from_center_size(
        pos2(rect.center().x, rect.bottom() - 46.0),
        vec2((rect.width() - 56.0).min(560.0), 40.0),
    );
    let progress = ui
        .scope_builder(egui::UiBuilder::new().max_rect(bar), |ui| {
            ui.horizontal(|ui| {
                let icon = if playing { Icon::Pause } else { Icon::Play };
                let hint = if finished {
                    "Play again"
                } else if playing {
                    "Pause (Space)"
                } else {
                    "Play (Space)"
                };
                if theme::icon_button(ui, icon, 16.0, palette.dim, palette.text, hint).clicked() {
                    actions.push(Action::VideoToggle);
                }
                // The ball and the clock follow the drag destination at once,
                // while the held player underneath stays where it was.
                let scrubbing = app
                    .video_scrub
                    .as_ref()
                    .is_some_and(|scrub| scrub.path == path);
                let mut fraction = app
                    .video_scrub
                    .as_ref()
                    .filter(|scrub| scrub.path == path)
                    .map(|scrub| scrub.target)
                    .unwrap_or_else(|| seek_fraction(position, total));
                let shown = app
                    .video_scrub
                    .as_ref()
                    .filter(|scrub| scrub.path == path)
                    .map(|scrub| total.mul_f32(scrub.target))
                    .unwrap_or(position);
                theme::text(
                    ui,
                    crate::util::duration(shown.as_secs().min(u64::from(u32::MAX)) as u32),
                    theme::regular(13.0),
                    palette.secondary,
                );
                let slider = ui.add_sized(
                    vec2((ui.available_width() - 300.0).max(60.0), 22.0),
                    egui::Slider::new(&mut fraction, 0.0..=1.0).show_value(false),
                );
                // Motion previews, release jumps once, a click jumps at once.
                match bar_seek_action(
                    slider.drag_started(),
                    slider.dragged(),
                    slider.drag_stopped(),
                    slider.changed(),
                    scrubbing,
                    fraction,
                ) {
                    BarSeek::None => {}
                    BarSeek::BeginDrag => {
                        let was_playing = playing && !finished;
                        if was_playing {
                            actions.push(Action::VideoToggle);
                        }
                        let generation = app.previewer.begin(path);
                        app.video_scrub = Some(VideoScrub {
                            path: path.to_path_buf(),
                            generation,
                            was_playing,
                            target: fraction,
                        });
                        app.previewer.request(path, generation, fraction, total);
                    }
                    BarSeek::Preview(target) => {
                        if let Some(scrub) =
                            app.video_scrub.as_mut().filter(|scrub| scrub.path == path)
                        {
                            scrub.target = target;
                        }
                        if let Some(scrub) =
                            app.video_scrub.as_ref().filter(|scrub| scrub.path == path)
                        {
                            app.previewer.request(path, scrub.generation, target, total);
                        }
                    }
                    BarSeek::Commit(target) => {
                        let held = app
                            .video_scrub
                            .as_ref()
                            .filter(|scrub| scrub.path == path)
                            .map(|scrub| (scrub.target, scrub.was_playing));
                        let (target, resume) = held.unwrap_or((target, false));
                        app.video_scrub = None;
                        app.previewer.cancel(path);
                        app.video_preview = None;
                        actions.push(Action::VideoSeek(target));
                        if resume {
                            actions.push(Action::VideoToggle);
                        }
                    }
                    BarSeek::Click(target) => {
                        actions.push(Action::VideoSeek(target));
                    }
                }
                paint_preview_thumb(app, ui, bar, path, total);
                theme::text(
                    ui,
                    crate::util::duration(total.as_secs().max(1).min(u64::from(u32::MAX)) as u32),
                    theme::regular(13.0),
                    palette.secondary,
                );
                // The output level lives here, beside the picture it belongs to.
                let (mut volume, muted) = (app.settings.video_volume, app.settings.video_muted);
                let icon = if muted || volume <= 0.01 {
                    Icon::VolumeX
                } else {
                    Icon::Volume
                };
                if theme::icon_button(ui, icon, 16.0, palette.dim, palette.text, "Mute (M)")
                    .clicked()
                {
                    actions.push(Action::VideoMuteToggle);
                }
                let loud = ui.add_sized(
                    vec2(90.0, 22.0),
                    egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false),
                );
                if loud.changed() {
                    actions.push(Action::VideoVolume(volume.clamp(0.0, 1.0)));
                    // Clicks and arrow keys have no drag to stop on: save at once.
                    if !loud.dragged() {
                        actions.push(Action::SettingsChanged);
                    }
                }
                if loud.drag_stopped() {
                    actions.push(Action::SettingsChanged);
                }
                // A focused slider owns the arrow keys: flag it so media
                // browsing yields until focus moves on.
                if slider.has_focus() || loud.has_focus() {
                    ui.data_mut(|data| data.insert_temp(control_focus_id(), true));
                }
                if theme::soft_button(ui, &palette, Some(Icon::Download), "Save a copy", false)
                    .clicked()
                {
                    actions.push(Action::SaveCopy(path.to_path_buf()));
                }
                if theme::soft_button(ui, &palette, Some(Icon::ExternalLink), "Default app", false)
                    .on_hover_text("Open in the default app")
                    .clicked()
                {
                    actions.push(Action::OpenFile(path.to_path_buf()));
                }
                slider
            })
            .inner
        })
        .inner;
    let hint = Rect::from_min_max(
        rect.min + vec2(28.0, 0.0),
        pos2(rect.right() - 28.0, rect.bottom() - 12.0),
    );
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(hint)
            .layout(Layout::bottom_up(Align::Min)),
        |ui| {
            theme::text(
                ui,
                "Space plays or pauses \u{b7} M mutes \u{b7} drag the bar to jump \u{b7} Esc to close",
                theme::regular(12.0),
                palette.dim,
            );
        },
    );
    progress
}

/// What the viewer shows for a video it cannot play in-process.
fn video_unsupported(
    ui: &mut egui::Ui,
    palette: &Palette,
    area: Rect,
    path: &Path,
    why: &str,
    actions: &mut Vec<Action>,
) {
    ui.scope_builder(
        egui::UiBuilder::new().max_rect(Rect::from_center_size(
            area.center(),
            vec2(area.width().min(420.0), 160.0),
        )),
        |ui| {
            ui.vertical_centered(|ui| {
                theme::paragraph(ui, why, theme::regular(14.0), palette.secondary);
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 220.0).max(0.0) / 2.0);
                    if theme::soft_button(
                        ui,
                        palette,
                        Some(Icon::ExternalLink),
                        "Open in the default app",
                        false,
                    )
                    .clicked()
                    {
                        actions.push(Action::OpenFile(path.to_path_buf()));
                    }
                });
            });
        },
    );
}

pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(viewer) = app.viewer.as_ref() else {
        return;
    };
    let Some(item) = viewer.current() else {
        return;
    };
    let palette = app.palette;
    let path = item.path.clone();
    let kind = item.kind;
    let zoom = viewer.zoom;
    let offset = vec2(viewer.offset.0, viewer.offset.1);
    let index = viewer.index;
    let count = viewer.items.len();
    let page = viewer.pdf_page;
    let pages = viewer.pdf_pages;
    let mut actions = Vec::new();
    let screen = ctx.content_rect();
    if kind == ViewerKind::Video {
        // Videos play with their soundtrack instead of showing a still.
        video_view(app, ctx, screen, &path, index, count);
        return;
    }
    // Fresh frame: pictures and PDFs own no sliders, so nothing holds arrows.
    ctx.data_mut(|data| data.insert_temp(control_focus_id(), false));
    // A rendered PDF page arrives as raw pixels; the texture is made here,
    // on the thread that owns the graphics context.
    if kind == ViewerKind::Pdf {
        upload_page(app, ctx, &path, page);
    }
    egui::Area::new(egui::Id::new("viewer"))
        .fixed_pos(screen.min)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            ui.set_width(screen.width());
            ui.set_height(screen.height());
            let (rect, backdrop) = ui.allocate_exact_size(screen.size(), Sense::click_and_drag());
            ui.painter().rect_filled(rect, CornerRadius::ZERO, BACKDROP);
            let area = usable_area(rect);
            // A PDF with previews gives up a strip on the left: every page
            // one click away, with the open one marked.
            let area = match kind {
                ViewerKind::Pdf => pdf_area(app, ui, area, &path, page, &mut actions),
                _ => area,
            };
            // Remember how wide the view is, so the worker rasterises the
            // page at the size it will be shown at.
            app.viewer_view_width = area.width() * ctx.pixels_per_point();
            let surface = match kind {
                ViewerKind::Pdf => pdf_surface(app, &path, page),
                _ => image_surface(ui, ctx, &path, area.size()),
            };
            let mut image_rect = None;
            match surface {
                Surface::Ready { id, size, animated } => {
                    let natural = if size.x > 0.0 && size.y > 0.0 {
                        size
                    } else {
                        vec2(4.0, 3.0)
                    };
                    let shown = natural * fit_scale(natural, area.size()) * zoom;
                    let center = area.center() + offset;
                    let placed = Rect::from_center_size(center, shown);
                    image_rect = Some(placed);
                    let response =
                        ui.interact(placed, ui.id().with("picture"), Sense::click_and_drag());
                    if ui.is_rect_visible(placed) {
                        // Animated stickers and GIFs keep playing here.
                        let frame = animated
                            .then(|| crate::animation::frame(ui, &path, placed))
                            .and_then(|frame| match frame {
                                crate::animation::Frame::Ready(texture) => Some(texture.id()),
                                _ => None,
                            });
                        ui.painter().image(
                            frame.unwrap_or(id),
                            placed,
                            Rect::from_min_max(egui::Pos2::ZERO, pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    }
                    if response.dragged() {
                        let delta = response.drag_delta();
                        actions.push(Action::ViewerPan((delta.x, delta.y)));
                    }
                    let pointer = ui.input(|input| input.pointer.hover_pos());
                    let hovering = pointer.is_some_and(|pos| placed.contains(pos));
                    if hovering {
                        let scroll = ui.input(|input| input.smooth_scroll_delta.y);
                        if scroll != 0.0 {
                            let anchor = pointer
                                .map(|pos| (pos.x - center.x, pos.y - center.y))
                                .unwrap_or((0.0, 0.0));
                            actions.push(Action::ViewerZoom {
                                factor: WHEEL_STEP.powf(scroll / 40.0),
                                anchor,
                            });
                        }
                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    }
                    if response.double_clicked() {
                        if zoom > 1.01 {
                            actions.push(Action::ViewerFit);
                        } else {
                            actions.push(Action::ViewerZoom {
                                factor: DOUBLE_CLICK_ZOOM,
                                anchor: (0.0, 0.0),
                            });
                        }
                    }
                    // The right button offers the file actions where the
                    // picture is, as it does everywhere else in the app.
                    egui::Popup::context_menu(&response)
                        .width(widgets::menu_width(
                            ui,
                            &["Copy to clipboard", "Save a copy…"],
                            true,
                        ))
                        .frame(widgets::menu_frame(&palette))
                        .show(|ui| {
                            if widgets::menu_item(
                                ui,
                                &palette,
                                Some(Icon::Copy),
                                "Copy to clipboard",
                            ) {
                                actions.push(Action::CopyImage(path.clone()));
                            }
                            if widgets::menu_item(
                                ui,
                                &palette,
                                Some(Icon::Download),
                                "Save a copy…",
                            ) {
                                actions.push(Action::SaveCopy(path.clone()));
                            }
                        });
                }
                Surface::Pending => {
                    theme::paint_spinner(
                        ui,
                        Rect::from_center_size(area.center(), Vec2::splat(48.0)),
                        34.0,
                        palette.accent,
                    );
                }
                Surface::Failed => {
                    let message = if kind == ViewerKind::Pdf {
                        app.pdf_error
                            .clone()
                            .unwrap_or_else(|| "This page could not be rendered.".to_owned())
                    } else {
                        "This picture could not be displayed.".to_owned()
                    };
                    ui.scope_builder(
                        egui::UiBuilder::new().max_rect(Rect::from_center_size(
                            area.center(),
                            vec2(area.width(), 60.0),
                        )),
                        |ui| {
                            ui.vertical_centered(|ui| {
                                theme::paragraph(
                                    ui,
                                    message,
                                    theme::regular(14.0),
                                    palette.secondary,
                                );
                            });
                        },
                    );
                }
            }
            if backdrop.clicked() && !ui.rect_contains_pointer(image_rect.unwrap_or(Rect::NOTHING))
            {
                actions.push(Action::CloseViewer);
            }
            chrome(
                ui,
                &palette,
                rect,
                index,
                count,
                zoom,
                kind,
                page,
                pages,
                &path,
                &mut actions,
            );
        });
    app.actions.extend(actions);
}

/// Turns the pixels the worker rendered into a texture, once per page.
fn upload_page(app: &mut App, ctx: &egui::Context, path: &Path, page: usize) {
    let Some(rendered) = app.pdf_page.as_ref() else {
        return;
    };
    if rendered.page != page {
        return;
    }
    let turns = app
        .viewer
        .as_ref()
        .filter(|viewer| {
            viewer
                .current()
                .is_some_and(|item| item.kind == ViewerKind::Pdf)
        })
        .map(|viewer| viewer.pdf_rotate)
        .unwrap_or(0);
    let rendered = rendered.rotated(turns);
    // A sharper render of the page on screen replaces the one in memory; an
    // older, coarser one is left alone.
    if let Some((known, known_page, known_width, _)) = app.pdf_texture.as_ref()
        && known == path
        && *known_page == page
        && *known_width >= rendered.width
    {
        return;
    }
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [rendered.width as usize, rendered.height as usize],
        &rendered.rgba,
    );
    let texture = ctx.load_texture(
        format!("pdf-{}-{page}", path.display()),
        image,
        egui::TextureOptions::LINEAR,
    );
    app.pdf_texture = Some((path.to_path_buf(), page, rendered.width, texture));
}

/// The uploaded texture of the PDF page on screen, when it is ready.
fn pdf_surface(app: &App, path: &Path, page: usize) -> Surface {
    match app.pdf_texture.as_ref() {
        Some((known, known_page, _, texture)) if known == path && *known_page == page => {
            let [width, height] = texture.size();
            Surface::Ready {
                id: texture.id(),
                size: vec2(width as f32, height as f32),
                animated: false,
            }
        }
        _ if app.pdf_error.is_some() => Surface::Failed,
        _ => Surface::Pending,
    }
}

/// Loads a picture or sticker through the image loader.
fn image_surface(_ui: &mut egui::Ui, ctx: &egui::Context, path: &Path, size: Vec2) -> Surface {
    let uri = crate::util::image_uri(path);
    crate::image_cache::touch(ctx, &uri);
    // A permanently broken file stays failed for a while instead of burning
    // a decode on every frame; the cooldown retries quietly on its own.
    if let Some(left) = image_cooling_down(ctx, &uri) {
        // Wake up when the cooldown ends so the retry actually runs.
        ctx.request_repaint_after(left.max(Duration::from_millis(500)));
        return Surface::Failed;
    }
    // The cooldown expired, if there ever was one: drop the loader cached
    // error once and read the file again, so a file fixed on disk recovers
    // without restarting the app.
    if clear_image_failure(ctx, &uri) {
        ctx.forget_image(&uri);
    }
    match egui::Image::new(&uri).load_for_size(ctx, size) {
        Ok(egui::load::TexturePoll::Ready { texture }) => Surface::Ready {
            id: texture.id,
            size: texture.size,
            animated: animated(path),
        },
        Ok(egui::load::TexturePoll::Pending { .. }) => Surface::Pending,
        Err(_) => {
            remember_image_failure(ctx, &uri);
            Surface::Failed
        }
    }
}

/// How long a broken picture waits before the viewer tries it again.
const IMAGE_RETRY_AFTER: Duration = Duration::from_secs(30);

/// How long until this picture may be tried again, if it broke recently.
fn image_cooling_down(ctx: &egui::Context, uri: &str) -> Option<Duration> {
    ctx.data_mut(|data| {
        let failures = data.get_temp_mut_or_default::<std::collections::HashMap<String, Instant>>(
            image_failures_id(),
        );
        cooldown_left(failures, uri, Instant::now())
    })
}

/// How long until a broken picture may be tried again: the remaining
/// cooldown, or nothing when it never broke or the wait already passed.
fn cooldown_left(
    failures: &std::collections::HashMap<String, Instant>,
    uri: &str,
    now: Instant,
) -> Option<Duration> {
    failures
        .get(uri)
        .and_then(|at| IMAGE_RETRY_AFTER.checked_sub(now.duration_since(*at)))
}

/// Forgets one recorded failure; true when there was one to forget.
fn clear_image_failure(ctx: &egui::Context, uri: &str) -> bool {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<std::collections::HashMap<String, Instant>>(
            image_failures_id(),
        )
        .remove(uri)
        .is_some()
    })
}

/// Where broken pictures and their failure instants live.
fn image_failures_id() -> egui::Id {
    egui::Id::new("viewer-image-failures")
}

/// Remembers a broken picture so the next frames skip it for a while.
fn remember_image_failure(ctx: &egui::Context, uri: &str) {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<std::collections::HashMap<String, Instant>>(
            image_failures_id(),
        )
        .insert(uri.to_owned(), Instant::now());
    });
}

/// One editable page number in the top bar.
#[derive(Clone, Default)]
struct PageField {
    text: String,
}

/// The id of the page field, so the keyboard leaves it alone while it is
/// being typed in.
pub fn page_field_id() -> egui::Id {
    egui::Id::new("viewer-page-field")
}

/// The id of the flag a focused viewer control sets, so the arrow keys
/// adjust it instead of browsing media.
pub fn control_focus_id() -> egui::Id {
    egui::Id::new("viewer-control-focus")
}

/// The page number as a field the reader can type in.
///
/// Shows the page on screen while it is not being edited; typing a number
/// and pressing Enter answers with the page to jump to.
fn page_field(ui: &mut egui::Ui, palette: &Palette, page: usize, pages: usize) -> Option<usize> {
    let id = page_field_id();
    let mut state = ui.data_mut(|data| data.get_temp_mut_or_default::<PageField>(id).clone());
    if !ui.memory(|memory| memory.has_focus(id)) {
        // Not being edited: the field shows the page on screen.
        state.text = (page + 1).to_string();
    }
    // The field is drawn like the app's other fields: a rounded surface with
    // the text editor kept frameless inside it.
    let (rect, _) = ui.allocate_exact_size(vec2(40.0, 22.0), Sense::hover());
    ui.painter().rect_filled(rect, 6.0, palette.surface);
    let field = rect.shrink2(vec2(7.0, 3.0));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(field)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let response = child.add(
        egui::TextEdit::singleline(&mut state.text)
            .id(id)
            .font(theme::regular(13.0))
            .text_color(palette.text)
            .horizontal_align(Align::Center)
            .frame(egui::Frame::NONE)
            .desired_width(field.width()),
    );
    let entered = response.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter));
    let asked = entered
        .then(|| state.text.trim().parse::<usize>().ok())
        .flatten()
        .filter(|asked| (1..=pages).contains(asked));
    if entered {
        // Focus goes away, so the field shows the page it moved to.
        ui.memory_mut(|memory| memory.surrender_focus(id));
    }
    ui.data_mut(|data| data.insert_temp(id, state));
    asked
}
/// Draws the title, the counter, and the controls over the picture.
/// The page strip on the left of an open PDF, or the untouched area.
///
/// Every preview is one click away from its page, and the open page carries
/// the accent border so the reader always knows where they are.
fn pdf_area(
    app: &mut App,
    ui: &mut egui::Ui,
    area: Rect,
    path: &Path,
    page: usize,
    actions: &mut Vec<Action>,
) -> Rect {
    let palette = app.palette;
    let thumbs = match app.pdf_thumbs.as_ref() {
        Some((known, files)) if known == path && files.len() > 1 => files.clone(),
        _ => return area,
    };
    let wide = 84.0;
    let strip = Rect::from_min_max(area.min, pos2(area.min.x + wide, area.max.y));
    let cell = vec2(wide - 12.0, 92.0);
    ui.scope_builder(egui::UiBuilder::new().max_rect(strip), |ui| {
        egui::ScrollArea::vertical()
            .id_salt("pdf-thumbs")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (index, file) in thumbs.iter().enumerate() {
                    let (tile, response) = ui.allocate_exact_size(cell, Sense::click());
                    if ui.is_rect_visible(tile) {
                        ui.painter().rect_filled(tile, 6.0, palette.surface);
                        if index == page {
                            ui.painter().rect_stroke(
                                tile,
                                6.0,
                                egui::Stroke::new(2.0, palette.accent),
                                egui::StrokeKind::Outside,
                            );
                        }
                        let inner = tile.shrink(5.0);
                        crate::image_cache::touch(ui.ctx(), &crate::util::image_uri(file));
                        let image = egui::Image::new(crate::util::image_uri(file));
                        if let Ok(egui::load::TexturePoll::Ready { texture }) =
                            image.load_for_size(ui.ctx(), inner.size())
                        {
                            image.paint_at(ui, widgets::picture_rect(inner, texture.size));
                        }
                        theme::text(
                            ui,
                            format!("{}", index + 1),
                            theme::regular(11.0),
                            palette.secondary,
                        );
                    }
                    if response.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                        actions.push(Action::ViewerPageTo(index + 1));
                    }
                }
            });
    });
    Rect::from_min_max(pos2(area.min.x + wide + 8.0, area.min.y), area.max)
}
#[allow(clippy::too_many_arguments)]
fn chrome(
    ui: &mut egui::Ui,
    palette: &Palette,
    rect: Rect,
    index: usize,
    count: usize,
    zoom: f32,
    kind: ViewerKind,
    page: usize,
    pages: usize,
    path: &Path,
    actions: &mut Vec<Action>,
) {
    let pdf = kind == ViewerKind::Pdf;
    let title = if pdf {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "PDF".to_owned())
    } else {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Picture".to_owned())
    };
    let counter = if pdf && pages > 0 {
        format!("page {} of {pages}", page + 1)
    } else {
        format!("{} of {count}", index + 1)
    };
    let top = Rect::from_min_max(
        rect.min + vec2(28.0, 20.0),
        pos2(rect.right() - 28.0, rect.top() + 56.0),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(top), |ui| {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(&title)
                    .font(theme::medium(14.0))
                    .color(palette.text),
            );
            if pdf && pages > 1 {
                // The number itself is a field: type a page and press Enter.
                ui.label(
                    egui::RichText::new("page")
                        .font(theme::regular(13.0))
                        .color(palette.secondary),
                );
                if let Some(asked) = page_field(ui, palette, page, pages) {
                    actions.push(Action::ViewerPageTo(asked));
                }
                ui.label(
                    egui::RichText::new(format!("of {pages}"))
                        .font(theme::regular(13.0))
                        .color(palette.secondary),
                );
            } else {
                ui.label(
                    egui::RichText::new(&counter)
                        .font(theme::regular(13.0))
                        .color(palette.secondary),
                );
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if theme::icon_button(
                    ui,
                    Icon::CircleX,
                    18.0,
                    palette.dim,
                    palette.text,
                    "Close (Esc)",
                )
                .clicked()
                {
                    actions.push(Action::CloseViewer);
                }
            });
        });
    });
    let bar = Rect::from_center_size(
        pos2(rect.center().x, rect.bottom() - 46.0),
        vec2(rect.width() - 56.0, 40.0),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(bar), |ui| {
        ui.horizontal(|ui| {
            let earlier = if pdf { "Previous page" } else { "Previous" };
            let later = if pdf { "Next page" } else { "Next" };
            let entries = [
                earlier,
                later,
                "Zoom out",
                "Zoom in",
                "Fit",
                "Copy",
                "Save a copy",
            ];
            let spacing = ui.spacing().item_spacing.x;
            let total = entries
                .iter()
                .map(|label| theme::soft_button_width(ui, label, true))
                .sum::<f32>()
                + 170.0
                + spacing * (entries.len() as f32 + 1.0);
            ui.add_space(((bar.width() - total) / 2.0).max(0.0));
            if theme::soft_button(ui, palette, Some(Icon::ChevronLeft), earlier, false)
                .on_hover_text(if pdf {
                    "Earlier page (↑)"
                } else {
                    "Earlier picture (←)"
                })
                .clicked()
            {
                actions.push(if pdf {
                    Action::ViewerPage(-1)
                } else {
                    Action::ViewerStep(-1)
                });
            }
            if theme::soft_button(ui, palette, Some(Icon::ChevronRight), later, false)
                .on_hover_text(if pdf {
                    "Later page (↓)"
                } else {
                    "Later picture (→)"
                })
                .clicked()
            {
                actions.push(if pdf {
                    Action::ViewerPage(1)
                } else {
                    Action::ViewerStep(1)
                });
            }
            if theme::icon_button(
                ui,
                Icon::Minus,
                14.0,
                palette.dim,
                palette.text,
                "Zoom out (-)",
            )
            .clicked()
            {
                actions.push(Action::ViewerZoom {
                    factor: 1.0 / BUTTON_STEP,
                    anchor: (0.0, 0.0),
                });
            }
            theme::text(
                ui,
                format!("{}%", (zoom * 100.0).round()),
                theme::regular(13.0),
                palette.secondary,
            );
            if theme::icon_button(
                ui,
                Icon::Plus,
                14.0,
                palette.dim,
                palette.text,
                "Zoom in (+)",
            )
            .clicked()
            {
                actions.push(Action::ViewerZoom {
                    factor: BUTTON_STEP,
                    anchor: (0.0, 0.0),
                });
            }
            if theme::soft_button(ui, palette, Some(Icon::Maximize), "Fit", zoom <= 1.01).clicked()
            {
                actions.push(Action::ViewerFit);
            }
            if pdf
                && theme::soft_button(ui, palette, Some(Icon::RotateCw), "Rotate", false)
                    .on_hover_text("Turn the page (R)")
                    .clicked()
            {
                actions.push(Action::ViewerRotate);
            }
            if theme::soft_button(ui, palette, Some(Icon::Copy), "Copy", false)
                .on_hover_text("Copy the picture to the clipboard")
                .clicked()
            {
                actions.push(Action::CopyImage(path.to_path_buf()));
            }
            if theme::soft_button(ui, palette, Some(Icon::Download), "Save a copy", false).clicked()
            {
                actions.push(Action::SaveCopy(path.to_path_buf()));
            }
            if theme::soft_button(
                ui,
                palette,
                Some(Icon::ExternalLink),
                "Open in the default app",
                false,
            )
            .clicked()
            {
                actions.push(Action::OpenFile(path.to_path_buf()));
            }
        });
    });
    let hint = Rect::from_min_max(
        rect.min + vec2(28.0, 0.0),
        pos2(rect.right() - 28.0, rect.bottom() - 12.0),
    );
    let text = if pdf {
        "Scroll to zoom · drag to move · arrows turn the page · PageUp/PageDown skip ten · Esc to close"
    } else {
        "Scroll to zoom · drag to move · arrows to browse · Esc to close"
    };
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(hint)
            .layout(Layout::bottom_up(Align::Min)),
        |ui| {
            theme::text(ui, text, theme::regular(12.0), palette.dim);
        },
    );
}

/// Whether the file may carry frames worth decoding.
fn usable_area(rect: Rect) -> Rect {
    let inset = INSET
        .min(rect.width() / 3.0)
        .min(rect.height() / 3.0)
        .max(0.0);
    rect.shrink(inset)
}

/// Whether the file may carry frames worth decoding.
fn animated(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "webp" | "gif" | "mp4"
            )
        })
}

/// Scale that fits a picture inside the window.
///
/// A small file grows so the window is used, but only up to a point: past
/// that the pixels stop carrying information, and the reader has the zoom
/// controls for a closer look.
fn fit_scale(natural: Vec2, available: Vec2) -> f32 {
    if natural.x <= 0.0 || natural.y <= 0.0 {
        return 1.0;
    }
    (available.x / natural.x)
        .min(available.y / natural.y)
        .min(MAX_FIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_fits_the_window_without_blowing_up_thin_air() {
        assert_eq!(fit_scale(vec2(100.0, 50.0), vec2(400.0, 300.0)), MAX_FIT);
        assert_eq!(fit_scale(vec2(160.0, 160.0), vec2(400.0, 300.0)), 1.875);
        assert_eq!(fit_scale(vec2(800.0, 400.0), vec2(400.0, 300.0)), 0.5);
        assert_eq!(fit_scale(vec2(200.0, 800.0), vec2(400.0, 300.0)), 0.375);
        assert_eq!(fit_scale(Vec2::ZERO, vec2(400.0, 300.0)), 1.0);
    }

    #[test]
    fn only_moving_formats_are_decoded_frame_by_frame() {
        assert!(animated(Path::new("sticker.webp")));
        assert!(animated(Path::new("loop.GIF")));
        assert!(animated(Path::new("clip.mp4")));
        assert!(!animated(Path::new("photo.jpg")));
        assert!(!animated(Path::new("notes.pdf")));
        assert!(!animated(Path::new("notes")));
    }

    #[test]
    fn a_click_lands_on_fractions_of_a_second() {
        // Seventy percent of ten and a half seconds is 7.35 seconds in.
        let total = Duration::from_millis(10_500);
        let clicked = seek_fraction(Duration::from_millis(7_350), total);
        assert!((clicked - 0.7).abs() < 0.000_1, "lands at {clicked}");
        // Whole seconds would have snapped the same click to 0.666.
        assert!(clicked > 0.69);
        assert_eq!(seek_fraction(Duration::ZERO, total), 0.0);
        assert_eq!(seek_fraction(total, total), 1.0);
        assert_eq!(seek_fraction(Duration::from_secs(99), total), 1.0);
        assert_eq!(seek_fraction(Duration::from_secs(1), Duration::ZERO), 0.0);
    }

    #[test]
    fn drag_previews_motion_and_commits_once_on_release() {
        // Press starts the hold, motion only previews, release jumps
        // once, Escape jumps nowhere.
        assert_eq!(
            bar_seek_action(false, false, false, false, false, 0.2),
            BarSeek::None
        );
        assert_eq!(
            bar_seek_action(true, false, false, false, false, 0.2),
            BarSeek::BeginDrag
        );
        assert_eq!(
            bar_seek_action(false, true, false, true, true, 0.5),
            BarSeek::Preview(0.5)
        );
        assert_eq!(
            bar_seek_action(false, false, true, true, true, 0.8),
            BarSeek::Commit(0.8)
        );
    }

    #[test]
    fn click_and_keys_jump_at_once_without_a_hold() {
        assert_eq!(
            bar_seek_action(false, false, false, true, false, 0.7),
            BarSeek::Click(0.7)
        );
        // A release nobody held behaves like the old direct jump.
        assert_eq!(
            bar_seek_action(false, false, true, true, false, 0.7),
            BarSeek::Click(0.7)
        );
        // Arrow nudges during a hold adjust the preview, never jump.
        assert_eq!(
            bar_seek_action(false, false, false, true, true, 0.4),
            BarSeek::Preview(0.4)
        );
    }

    #[test]
    fn a_fixed_picture_gets_a_real_retry() {
        let mut failures = std::collections::HashMap::new();
        let now = Instant::now();
        assert_eq!(cooldown_left(&failures, "x", now), None);
        failures.insert("x".to_owned(), now - Duration::from_secs(10));
        let left = cooldown_left(&failures, "x", now).expect("still cooling");
        assert!(left <= Duration::from_secs(20) && !left.is_zero());
        // After thirty seconds the wait has passed: retry for real.
        failures.insert("x".to_owned(), now - Duration::from_secs(31));
        assert_eq!(cooldown_left(&failures, "x", now), None);
    }

    #[test]
    fn a_replaced_picture_recovers_without_restart() {
        // Invalid bytes fail and cool down; a valid file plus an expired
        // cooldown rereads and paints, without restarting anything.
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("swap.png");
        std::fs::write(&path, b"not a picture").expect("writes");
        let ctx = egui::Context::default();
        // The harness context needs the real file and image loaders.
        egui_extras::install_image_loaders(&ctx);
        let area = egui::vec2(200.0, 200.0);
        let tag = std::cell::Cell::new(9u8);
        let show = |ctx: &egui::Context| {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, area)),
                    ..Default::default()
                },
                |ui| {
                    let ctx = ui.ctx().clone();
                    tag.set(match image_surface(ui, &ctx, &path, area) {
                        Surface::Ready { .. } => 0,
                        Surface::Pending => 1,
                        Surface::Failed => 2,
                    });
                },
            );
            output.textures_delta.clear();
        };
        // Pump the loader: invalid bytes settle on Failed, then stay there.
        for _ in 0..20 {
            show(&ctx);
            if tag.get() != 1 {
                break;
            }
        }
        // Pump the loader with real time: file reads and decodes run on
        // background threads. Invalid bytes settle on Failed, then stay.
        let deadline = Instant::now() + Duration::from_secs(10);
        while tag.get() == 1 && Instant::now() < deadline {
            show(&ctx);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(tag.get(), 2, "invalid bytes fail");
        show(&ctx);
        assert_eq!(tag.get(), 2, "cooling down, no hot retry");
        // Swap in a valid picture and expire the cooldown the way time does.
        image::RgbaImage::from_pixel(8, 8, image::Rgba([9, 10, 11, 255]))
            .save(&path)
            .expect("saves");
        ctx.data_mut(|data| {
            let failures = data
                .get_temp_mut_or_default::<std::collections::HashMap<String, Instant>>(
                    image_failures_id(),
                );
            for at in failures.values_mut() {
                *at = Instant::now() - IMAGE_RETRY_AFTER - Duration::from_secs(1);
            }
        });
        // Re-arm the pump: the tag still says Failed from the cooldown.
        tag.set(1);
        let deadline = Instant::now() + Duration::from_secs(10);
        while tag.get() == 1 && Instant::now() < deadline {
            show(&ctx);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(tag.get(), 0, "the swapped file paints");
    }

    #[test]
    fn real_slider_drag_previews_then_commits_once() {
        use crate::paths::AppDirs;
        use crate::settings::Settings;

        let root = std::env::temp_dir().join(format!("zapfast-scrub-{}", std::process::id()));
        let (mut app, _events) = App::headless(AppDirs::under(&root), Settings::default());
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        let path = root.join("clip.mp4");
        let total = Duration::from_secs(100);
        let start = Duration::from_secs(10);
        let press = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let at = |bar: egui::Rect, fraction: f32| {
            egui::pos2(bar.min.x + fraction * bar.width(), bar.center().y)
        };
        let seeks = |actions: &[Action]| {
            actions
                .iter()
                .filter(|action| matches!(action, Action::VideoSeek(_)))
                .count()
        };
        let toggles = |actions: &[Action]| {
            actions
                .iter()
                .filter(|action| matches!(action, Action::VideoToggle))
                .count()
        };
        let mut run = |events: Vec<egui::Event>| {
            let mut out: Option<(egui::Rect, Vec<Action>, Option<VideoScrub>)> = None;
            let input = egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                let mut actions = Vec::new();
                let response = video_bar(
                    &mut app,
                    ui,
                    screen,
                    &path,
                    start,
                    total,
                    true,
                    false,
                    &mut actions,
                );
                out = Some((response.rect, actions, app.video_scrub.clone()));
            });
            output.textures_delta.clear();
            out.expect("the bar draws every frame")
        };
        let (bar, first, scrub) = run(Vec::new());
        assert!(first.is_empty(), "an idle bar queues nothing");
        assert!(scrub.is_none(), "no drag is held yet");
        let ball = at(bar, 0.1);
        let (_, pressed, scrub) = run(vec![egui::Event::PointerMoved(ball), press(ball, true)]);
        assert_eq!(seeks(&pressed), 0, "grabbing the ball never jumps");
        assert_eq!(toggles(&pressed), 1, "the drag holds playback");
        let held = scrub.expect("a drag is held");
        assert!(held.was_playing, "resume stays armed");
        assert!(held.target >= 0.0, "the hold starts near the ball");
        assert!(held.target < 0.3, "grabbing never jumps across the bar");
        for fraction in [0.7f32, 0.3, 0.8] {
            let (_, moved, scrub) = run(vec![egui::Event::PointerMoved(at(bar, fraction))]);
            assert_eq!(seeks(&moved), 0, "motion only previews");
            assert_eq!(toggles(&moved), 0, "motion holds quietly");
            let target = scrub.expect("the hold lasts").target;
            assert!(
                (target - fraction).abs() < 0.06,
                "the ball follows the pointer"
            );
        }
        let outside = screen.min + egui::vec2(10.0, 10.0);
        let release = vec![egui::Event::PointerMoved(outside), press(outside, false)];
        let (_, released, scrub) = run(release);
        assert_eq!(seeks(&released), 1, "release jumps exactly once");
        assert_eq!(toggles(&released), 1, "playback resumes after the jump");
        let target = released
            .iter()
            .find_map(|action| match action {
                Action::VideoSeek(target) => Some(*target),
                _ => None,
            })
            .expect("release jumps");
        assert!(
            (target - 0.8).abs() < 0.06,
            "the jump lands where the drag ended"
        );
        assert!(scrub.is_none(), "the drag retires on release");
    }

    #[test]
    fn click_jumps_at_once_without_a_hold() {
        use crate::paths::AppDirs;
        use crate::settings::Settings;

        let root = std::env::temp_dir().join(format!("zapfast-click-{}", std::process::id()));
        let (mut app, _events) = App::headless(AppDirs::under(&root), Settings::default());
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        let path = root.join("clip.mp4");
        let total = Duration::from_secs(100);
        let start = Duration::from_secs(10);
        let press = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let mut run = |events: Vec<egui::Event>| {
            let mut out: Option<(egui::Rect, Vec<Action>, Option<VideoScrub>)> = None;
            let input = egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                let mut actions = Vec::new();
                let response = video_bar(
                    &mut app,
                    ui,
                    screen,
                    &path,
                    start,
                    total,
                    false,
                    false,
                    &mut actions,
                );
                out = Some((response.rect, actions, app.video_scrub.clone()));
            });
            output.textures_delta.clear();
            out.expect("the bar draws every frame")
        };
        let (bar, _, _) = run(Vec::new());
        let spot = egui::pos2(bar.min.x + 0.7 * bar.width(), bar.center().y);
        let (_, down, held) = run(vec![egui::Event::PointerMoved(spot), press(spot, true)]);
        let (_, up, kept) = run(vec![egui::Event::PointerMoved(spot), press(spot, false)]);
        assert!(
            down.iter()
                .filter(|action| matches!(action, Action::VideoSeek(_)))
                .count()
                == 0,
            "a click jumps on release"
        );
        assert!(held.is_some(), "a press parks a transient drag");
        assert!(kept.is_none(), "no drag survives the release");
        let mut jumps = down
            .iter()
            .chain(up.iter())
            .filter_map(|action| match action {
                Action::VideoSeek(target) => Some(*target),
                _ => None,
            });
        let mut count = 0;
        for jump in jumps.by_ref() {
            count += 1;
            assert!((jump - 0.7).abs() < 0.1, "the click lands where asked");
        }
        assert_eq!(count, 1, "a click jumps exactly once");
        assert!(
            !down
                .iter()
                .any(|action| matches!(action, Action::VideoToggle))
        );
        assert!(
            !up.iter()
                .any(|action| matches!(action, Action::VideoToggle))
        );
    }
    #[test]
    fn pointer_move_to_displayed_preview_reports_latency() {
        use crate::paths::AppDirs;
        use crate::settings::Settings;
        let root =
            std::env::temp_dir().join(format!("zapfast-previewshown-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("creates");
        let path = root.join("shown.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y"])
            .args(["-f", "lavfi", "-i", "testsrc2=s=320x240:d=6:r=10"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .args(["-profile:v", "baseline", "-bf", "0"])
            .args(["-g", "10", "-keyint_min", "10", "-sc_threshold", "0"])
            .args(["-an", "-movflags", "+faststart"])
            .arg(&path)
            .status()
            .is_ok_and(|status| status.success());
        if !made {
            eprintln!("skipped: ffmpeg unavailable for fixtures");
            return;
        }
        let total = Duration::from_secs(6);
        let start_at = Duration::from_millis(600);
        let (mut app, _events) = App::headless(AppDirs::under(&root), Settings::default());
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        let picture = egui::Rect::from_center_size(screen.center(), egui::vec2(400.0, 300.0));
        let press = |pos: egui::Pos2, pressed: bool| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let at = |bar: egui::Rect, fraction: f32| {
            egui::pos2(bar.min.x + fraction * bar.width(), bar.center().y)
        };
        let mut run = |events: Vec<egui::Event>| {
            let mut out: Option<(egui::Rect, Vec<Action>, Option<VideoScrub>, bool)> = None;
            let input = egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                let mut actions = Vec::new();
                let response = video_bar(
                    &mut app,
                    ui,
                    screen,
                    &path,
                    start_at,
                    total,
                    false,
                    false,
                    &mut actions,
                );
                let shown = paint_scrub_preview(&mut app, ui, &ctx, &path, picture);
                out = Some((response.rect, actions, app.video_scrub.clone(), shown));
            });
            output.textures_delta.clear();
            out.expect("the bar draws every frame")
        };
        let (bar, _, _, _) = run(Vec::new());
        let ball = at(bar, 0.1);
        let (_, pressed, scrub, _) = run(vec![egui::Event::PointerMoved(ball), press(ball, true)]);
        assert!(pressed.is_empty(), "grabbing parks without jumping");
        assert!(scrub.is_some(), "a drag is held");
        let mut times_ms: Vec<u128> = Vec::new();
        for fraction in [0.2f32, 0.8, 0.4, 0.6] {
            let start = Instant::now();
            let (_, moved, scrub, _) = run(vec![egui::Event::PointerMoved(at(bar, fraction))]);
            assert!(moved.is_empty(), "motion only previews");
            assert!(scrub.is_some(), "the hold lasts");
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let (_, _, scrub, shown) = run(Vec::new());
                let held = scrub.expect("the hold lasts");
                assert!(
                    (held.target - fraction).abs() < 0.06,
                    "the knob leads the picture"
                );
                if shown {
                    break;
                }
                assert!(Instant::now() < deadline, "the preview shows");
            }
            times_ms.push(start.elapsed().as_millis());
        }
        times_ms.sort_unstable();
        let pct = |q: f64| {
            times_ms[((times_ms.len() as f64 * q).floor() as usize).min(times_ms.len() - 1)]
        };
        eprintln!(
            "move-to-displayed samples={} p50={}ms p95={}ms raw={times_ms:?}",
            times_ms.len(),
            pct(0.5),
            pct(0.95)
        );
    }
}
