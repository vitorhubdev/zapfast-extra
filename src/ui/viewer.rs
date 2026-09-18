//! Full-window viewer for a chat's pictures, stickers, and PDFs.

use std::path::Path;
use std::time::Duration;

use egui::{Align, Color32, CornerRadius, CursorIcon, Key, Layout, Rect, Sense, Vec2, pos2, vec2};

use crate::app::App;
use crate::model::{Action, ViewerKind};
use crate::theme::{self, Icon, Palette};

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
                    if ui.is_rect_visible(placed) {
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
    let uri = format!(
        "bytes://poster-{}-{id}",
        chat.chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
    );
    let seen = ctx.data_mut(|data| {
        let mut known = data
            .get_temp_mut_or_default::<std::collections::HashSet<String>>(egui::Id::new(
                "video-posters",
            ))
            .clone();
        let fresh = known.insert(uri.clone());
        data.insert_temp(egui::Id::new("video-posters"), known);
        fresh
    });
    if seen {
        ctx.include_bytes(uri.clone(), bytes.to_vec());
    }
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

/// Playback controls under the playing video.
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
) {
    let palette = app.palette;
    let bar = Rect::from_center_size(
        pos2(rect.center().x, rect.bottom() - 46.0),
        vec2((rect.width() - 56.0).min(560.0), 40.0),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(bar), |ui| {
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
            theme::text(
                ui,
                crate::util::duration(position.as_secs().min(u64::from(u32::MAX)) as u32),
                theme::regular(13.0),
                palette.secondary,
            );
            let total_secs = total.as_secs().max(1);
            let mut fraction = (position.as_secs_f32() / total_secs as f32).clamp(0.0, 1.0);
            let slider = ui.add_sized(
                vec2((ui.available_width() - 300.0).max(60.0), 22.0),
                egui::Slider::new(&mut fraction, 0.0..=1.0).show_value(false),
            );
            if slider.drag_stopped() {
                actions.push(Action::VideoSeek(fraction));
            }
            theme::text(
                ui,
                crate::util::duration(total_secs.min(u64::from(u32::MAX)) as u32),
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
            if theme::icon_button(ui, icon, 16.0, palette.dim, palette.text, "Mute (M)").clicked() {
                actions.push(Action::VideoMuteToggle);
            }
            let loud = ui.add_sized(
                vec2(90.0, 22.0),
                egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false),
            );
            if loud.changed() {
                actions.push(Action::VideoVolume(volume.clamp(0.0, 1.0)));
            }
            if loud.drag_stopped() {
                actions.push(Action::SettingsChanged);
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
        });
    });
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
fn image_surface(ui: &mut egui::Ui, ctx: &egui::Context, path: &Path, size: Vec2) -> Surface {
    let uri = crate::util::image_uri(path);
    match egui::Image::new(&uri).load_for_size(ctx, size) {
        Ok(egui::load::TexturePoll::Ready { texture }) => Surface::Ready {
            id: texture.id,
            size: texture.size,
            animated: animated(path),
        },
        Ok(egui::load::TexturePoll::Pending { .. }) => Surface::Pending,
        Err(_) => {
            // Drop the failed entry so the next frame tries again.
            ui.ctx().forget_image(&uri);
            Surface::Failed
        }
    }
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
}
