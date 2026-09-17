//! Full-window viewer for a chat's pictures, stickers, and PDFs.

use std::path::Path;

use egui::{Align, Color32, CornerRadius, CursorIcon, Layout, Rect, Sense, Vec2, pos2, vec2};

use crate::app::App;
use crate::model::{Action, ViewerKind};
use crate::theme::{self, Icon, Palette};

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

/// Draws the title, the counter, and the controls over the picture.
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
    } else if kind == ViewerKind::Sticker {
        "Sticker".to_owned()
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
            ui.label(
                egui::RichText::new(counter)
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
    let bar = Rect::from_center_size(
        pos2(rect.center().x, rect.bottom() - 46.0),
        vec2(rect.width() - 56.0, 40.0),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(bar), |ui| {
        ui.horizontal(|ui| {
            let earlier = if pdf { "Previous page" } else { "Previous" };
            let later = if pdf { "Next page" } else { "Next" };
            let entries = [earlier, later, "Zoom out", "Zoom in", "Fit", "Save a copy"];
            let spacing = ui.spacing().item_spacing.x;
            let total = entries
                .iter()
                .map(|label| theme::soft_button_width(ui, label, true))
                .sum::<f32>()
                + 160.0
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
        "Scroll to zoom · drag to move · ↑ ↓ or ← → to turn the page · Esc to close"
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
