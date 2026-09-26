//! Shortcuts, account, linking, contact, and chat dialogs.

use egui::{Align, CornerRadius, Frame, Layout, Margin, Sense, Stroke, pos2, vec2};

use crate::app::App;
use crate::model::{Action, Dialog};
use crate::theme::{self, Icon};

use super::widgets;

pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(dialog) = app.dialog.clone() else {
        return;
    };
    let palette = app.palette;
    let frame = Frame::new()
        .fill(palette.overlay)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS + 4))
        .inner_margin(Margin::same(22))
        .shadow(egui::epaint::Shadow {
            offset: [0, 12],
            blur: 40,
            spread: 0,
            color: palette.shadow,
        });
    let response = egui::Modal::new(egui::Id::new("dialog"))
        .frame(frame)
        .backdrop_color(palette.shadow)
        .show(ctx, |ui| {
            ui.set_width(match dialog {
                Dialog::Shortcuts => 540.0,
                Dialog::About => 380.0,
                Dialog::ConfirmUnlink => 380.0,
                Dialog::PairWithPhone => 380.0,
                Dialog::NewContact => 380.0,
                Dialog::ChatInfo(_) => 360.0,
                Dialog::Forward { .. } => 420.0,
                Dialog::ConfirmSticker { .. } => 340.0,
                Dialog::PeekSticker { .. } => 320.0,
                Dialog::StickerPackView { .. } => 420.0,
                Dialog::FileInfo(_) => 400.0,
                Dialog::ConfirmDeleteMany { .. } => 420.0,
                Dialog::CreatePoll(_) => 420.0,
            });
            ui.spacing_mut().item_spacing.y = 8.0;
            match dialog {
                Dialog::CreatePoll(chat) => super::polls::create(app, ui, &chat),
                Dialog::Shortcuts => shortcuts(app, ui),
                Dialog::About => about(app, ui),
                Dialog::ConfirmUnlink => confirm_unlink(app, ui),
                Dialog::PairWithPhone => pair_with_phone(app, ui),
                Dialog::NewContact => new_contact(app, ui),
                Dialog::ChatInfo(id) => chat_info(app, ui, &id),
                Dialog::Forward { chat, messages } => forward(app, ui, &chat, &messages),
                Dialog::ConfirmSticker { path } => confirm_sticker(app, ui, &path),
                Dialog::PeekSticker { path } => peek_sticker(app, ui, &path),
                Dialog::StickerPackView {
                    name,
                    publisher,
                    dir,
                    stickers,
                } => sticker_pack_view(app, ui, &name, &publisher, &dir, &stickers),
                Dialog::FileInfo(info) => file_info(app, ui, &info),
                Dialog::ConfirmDeleteMany { ids, revocable, .. } => {
                    confirm_delete_many(app, ui, ids, revocable)
                }
            }
        });
    if response.should_close() {
        app.actions.push(Action::CloseDialog);
    }
}

/// Copy and buttons for the delete-many dialog, kept pure so tests lock
/// the split between revoking for everyone and deleting locally.
pub(crate) struct DeleteManyCopy {
    pub title: String,
    pub note: String,
    /// Whether "Delete for everyone" is offered at all.
    pub revocable: bool,
    /// Label of the local-delete button.
    pub local_label: &'static str,
}

pub(crate) fn delete_many_copy(total: usize, revocable: usize) -> DeleteManyCopy {
    let revocable = revocable.min(total);
    let title = format!("Delete {} messages?", total.max(1));
    let (note, local_label) = if revocable == 0 {
        (
            format!(
                "These {total} messages are too old or not yours to revoke. They will be deleted for you."
            ),
            "Delete",
        )
    } else if revocable == total {
        (
            format!("These {total} messages will be deleted for everyone."),
            "Delete for me",
        )
    } else {
        (
            format!(
                "{revocable} of {total} are still recent enough to delete for everyone; the rest will be deleted for you."
            ),
            "Delete for me",
        )
    };
    DeleteManyCopy {
        title,
        note,
        revocable: revocable > 0,
        local_label,
    }
}

fn confirm_delete_many(app: &mut App, ui: &mut egui::Ui, ids: Vec<String>, revocable: usize) {
    let palette = app.palette;
    let copy = delete_many_copy(ids.len(), revocable);
    title(ui, app, &copy.title);
    theme::paragraph(ui, &copy.note, theme::regular(13.5), palette.text);
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if copy.revocable && danger_button(ui, app, "Delete for everyone") {
                app.actions.push(Action::DeleteMany {
                    ids: ids.clone(),
                    for_everyone: true,
                });
            }
            if theme::pill_button(ui, &palette, copy.local_label, !copy.revocable).clicked() {
                app.actions.push(Action::DeleteMany {
                    ids: ids.clone(),
                    for_everyone: false,
                });
            }
            if theme::pill_button(ui, &palette, "Cancel", false).clicked() {
                app.actions.push(Action::CloseDialog);
            }
        });
    });
}

fn confirm_sticker(app: &mut App, ui: &mut egui::Ui, path: &std::path::Path) {
    let palette = app.palette;
    title(ui, app, "Send this sticker?");
    let side = 180.0;
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
        if ui.is_rect_visible(rect) {
            ui.painter().rect_filled(rect, 8.0, palette.surface);
            widgets::picture(ui, path, rect.shrink(8.0));
        }
    });
    let exists = path.exists();
    if !exists {
        theme::paragraph(
            ui,
            "This sticker file is gone, so there is nothing to send.",
            theme::regular(13.0),
            palette.secondary,
        );
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let send = ui.add_enabled_ui(exists, |ui| {
                theme::pill_button(ui, &palette, "Send sticker", true).clicked()
            });
            if send.inner {
                app.actions.push(Action::SendSticker(path.to_path_buf()));
            }
            if theme::pill_button(ui, &palette, "Cancel", false).clicked() {
                app.actions.push(Action::CloseDialog);
            }
        });
    });
}

/// Shows one sticker bigger, with a way to keep it.
/// Lists what is known about one attachment.
fn file_info(app: &mut App, ui: &mut egui::Ui, info: &crate::model::FileInfo) {
    let palette = app.palette;
    title(ui, app, "File info");
    theme::paragraph(ui, &info.title, theme::medium(14.0), palette.text);
    ui.add_space(4.0);
    for (label, value) in &info.rows {
        ui.horizontal(|ui| {
            ui.set_width(360.0);
            ui.vertical(|ui| {
                ui.set_width(120.0);
                theme::text(ui, label, theme::regular(12.5), palette.secondary);
            });
            ui.vertical(|ui| {
                ui.set_width(230.0);
                theme::paragraph(ui, value, theme::regular(12.5), palette.text);
            });
        });
    }
    if let Some(note) = &info.note {
        ui.add_space(6.0);
        theme::paragraph(ui, note, theme::regular(12.5), palette.danger);
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if theme::pill_button(ui, &palette, "Close", false).clicked() {
                app.actions.push(Action::CloseDialog);
            }
        });
    });
}

fn peek_sticker(app: &mut App, ui: &mut egui::Ui, path: &std::path::Path) {
    let palette = app.palette;
    title(ui, app, "Sticker");
    let side = 240.0;
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
        if ui.is_rect_visible(rect) {
            ui.painter().rect_filled(rect, 10.0, palette.surface);
            widgets::picture(ui, path, rect.shrink(10.0));
        }
    });
    ui.add_space(6.0);
    // A copy that already lives in the saved folder needs no button.
    let saved = path.starts_with(app.dirs.saved_sticker_dir());
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if theme::pill_button(ui, &palette, "Close", false).clicked() {
                app.actions.push(Action::CloseDialog);
            }
            if saved {
                theme::text(ui, "Saved", theme::regular(13.0), palette.secondary);
            } else if theme::pill_button(ui, &palette, "Save sticker", true).clicked() {
                app.actions.push(Action::SaveSticker(path.to_path_buf()));
                app.actions.push(Action::CloseDialog);
            }
        });
    });
}

fn sticker_pack_view(
    app: &mut App,
    ui: &mut egui::Ui,
    name: &str,
    publisher: &str,
    dir: &std::path::Path,
    stickers: &[std::path::PathBuf],
) {
    let palette = app.palette;
    title(ui, app, name);
    if !publisher.trim().is_empty() {
        theme::paragraph(ui, publisher, theme::regular(12.5), palette.secondary);
    }
    egui::ScrollArea::vertical()
        .id_salt("pack-view")
        .max_height(320.0)
        .show(ui, |ui| {
            egui::Grid::new("pack-grid")
                .num_columns(4)
                .spacing([6.0, 6.0])
                .show(ui, |ui| {
                    for (index, path) in stickers.iter().enumerate() {
                        let (rect, _) = ui.allocate_exact_size(vec2(72.0, 72.0), Sense::hover());
                        if ui.is_rect_visible(rect) {
                            ui.painter().rect_filled(rect, 8.0, palette.surface);
                            widgets::picture(ui, path, rect.shrink(6.0));
                        }
                        if index % 4 == 3 {
                            ui.end_row();
                        }
                    }
                });
        });
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if theme::pill_button(ui, &palette, "Close", false).clicked() {
                app.actions.push(Action::CloseDialog);
            }
            if theme::pill_button(ui, &palette, "Add to my packs", true).clicked() {
                app.actions.push(Action::AddStickerPack {
                    dir: dir.to_path_buf(),
                    name: name.to_owned(),
                });
                app.actions.push(Action::CloseDialog);
            }
        });
    });
}
fn forward(app: &mut App, ui: &mut egui::Ui, from_chat: &str, messages: &[String]) {
    let palette = app.palette;
    title(
        ui,
        app,
        &if messages.len() == 1 {
            "Forward message".to_owned()
        } else {
            format!("Forward {} messages", messages.len())
        },
    );
    theme::paragraph(
        ui,
        "WhatsApp allows up to 5 chats at once, or 1 chat for frequently forwarded messages.",
        theme::regular(12.5),
        palette.secondary,
    );
    ui.add_space(4.0);
    let width = ui.available_width();
    let search = super::widgets::search_field(
        ui,
        &palette,
        egui::Id::new("forward-search"),
        &mut app.forward_search,
        "Search chats",
        width,
    );
    if ui.memory(|memory| memory.focused().is_none()) {
        search.request_focus();
    }
    ui.add_space(4.0);

    let needle = app.forward_search.trim().to_lowercase();
    let mut chats: Vec<_> = app
        .chats
        .iter()
        .filter(|chat| chat.kind != crate::model::ChatKind::Broadcast && !chat.read_only)
        .filter(|chat| {
            needle.is_empty()
                || app.chat_title(chat).to_lowercase().contains(&needle)
                || chat.phone().is_some_and(|phone| phone.contains(&needle))
        })
        .cloned()
        .collect();
    chats.sort_by_key(|chat| std::cmp::Reverse(chat.last_activity));

    let row_height = 52.0;
    let max_height = (ui.ctx().content_rect().height() - 220.0).clamp(row_height * 3.0, 420.0);
    egui::ScrollArea::vertical()
        .id_salt("forward-chats")
        .max_height(max_height)
        .auto_shrink([false, true])
        .show_rows(ui, row_height, chats.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for chat in &chats[range] {
                let title = app.chat_title(chat);
                let (rect, response) =
                    ui.allocate_exact_size(vec2(ui.available_width(), row_height), Sense::click());
                if ui.is_rect_visible(rect) {
                    if response.hovered() {
                        ui.painter().rect_filled(rect, 8.0, palette.surface_hover);
                    }
                    let avatar = egui::Rect::from_center_size(
                        pos2(rect.left() + 23.0, rect.center().y),
                        egui::Vec2::splat(38.0),
                    );
                    let picture = app.avatar(&chat.id);
                    super::widgets::paint_avatar(
                        ui,
                        &palette,
                        avatar,
                        &title,
                        &chat.id,
                        picture.as_deref(),
                    );
                    let line = super::widgets::line(
                        ui,
                        &title,
                        theme::medium(14.5),
                        palette.text,
                        rect.width() - 62.0,
                        1,
                    );
                    line.paint(
                        ui,
                        pos2(rect.left() + 50.0, rect.center().y - line.size().y / 2.0),
                        palette.text,
                    );
                    ui.painter().hline(
                        (rect.left() + 50.0)..=rect.right(),
                        rect.bottom() - 0.5,
                        Stroke::new(1.0, palette.outline),
                    );
                    let box_center = pos2(rect.right() - 20.0, rect.center().y);
                    if app.forward_to.contains(&chat.id) {
                        ui.painter().circle_filled(box_center, 10.0, palette.accent);
                        Icon::Check.image(egui::Color32::WHITE, 12.0).paint_at(
                            ui,
                            egui::Rect::from_center_size(box_center, egui::Vec2::splat(12.0)),
                        );
                    } else {
                        ui.painter()
                            .circle_stroke(box_center, 9.0, Stroke::new(1.5, palette.dim));
                    }
                }
                if response
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    if let Some(known) = app.forward_to.iter().position(|id| id == &chat.id) {
                        app.forward_to.remove(known);
                    } else if app.forward_to.len() < crate::model::FORWARD_CHAT_LIMIT {
                        app.forward_to.push(chat.id.clone());
                    } else {
                        app.toast("WhatsApp allows up to 5 chats per forward");
                    }
                }
            }
        });
    if chats.is_empty() {
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            theme::text(
                ui,
                "No writable chats found",
                theme::regular(13.5),
                palette.secondary,
            );
        });
        ui.add_space(12.0);
    }
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        theme::text(
            ui,
            format!(
                "{} of {} chats",
                app.forward_to.len(),
                crate::model::FORWARD_CHAT_LIMIT
            ),
            theme::regular(13.0),
            palette.secondary,
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if theme::pill_button(ui, &palette, "Forward", true).clicked()
                && !app.forward_to.is_empty()
            {
                app.actions.push(Action::ForwardMany {
                    from_chat: from_chat.to_owned(),
                    messages: messages.to_vec(),
                    to_chats: app.forward_to.clone(),
                });
            }
        });
    });
}

fn title(ui: &mut egui::Ui, app: &mut App, label: &str) {
    let palette = app.palette;
    ui.horizontal(|ui| {
        theme::text(ui, label, theme::bold(18.0), palette.text);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if theme::icon_button(ui, Icon::X, 16.0, palette.secondary, palette.text, "Close")
                .clicked()
            {
                app.actions.push(Action::CloseDialog);
            }
        });
    });
    ui.add_space(4.0);
}

fn shortcuts(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    title(ui, app, "Keyboard shortcuts");
    // Reserve enough width for the longest shortcut before laying out the grid.
    let keys_width = super::keys::SHORTCUTS
        .iter()
        .map(|(keys, _)| {
            ui.painter()
                .layout_no_wrap(
                    super::keys::label(keys),
                    theme::semibold(13.0),
                    egui::Color32::WHITE,
                )
                .size()
                .x
        })
        .fold(0.0, f32::max);
    egui::Grid::new("shortcuts")
        .num_columns(2)
        .min_col_width(keys_width)
        .spacing([18.0, 8.0])
        .show(ui, |ui| {
            for (keys, what) in super::keys::SHORTCUTS {
                theme::text(
                    ui,
                    super::keys::label(keys),
                    theme::semibold(13.0),
                    palette.text,
                );
                theme::text(ui, *what, theme::regular(13.0), palette.secondary);
                ui.end_row();
            }
        });
}

fn about(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    title(ui, app, "About");
    ui.horizontal(|ui| {
        let (logo, _) = ui.allocate_exact_size(egui::Vec2::splat(44.0), egui::Sense::hover());
        theme::logo(ui, logo.center(), 44.0);
        ui.vertical(|ui| {
            theme::text(ui, "ZapExt", theme::bold(17.0), palette.text);
            theme::text(
                ui,
                format!("Version {}", crate::updates::zapext_version()),
                theme::regular(13.0),
                palette.secondary,
            );
        });
    });
    ui.add_space(6.0);
    theme::paragraph(
        ui,
        "ZapExt is a community fork of ZapFast, a native WhatsApp client written in Rust with egui. It connects through whatsapp-rust, and messages are end-to-end encrypted on this device.",
        theme::regular(13.0),
        palette.text,
    );
    theme::paragraph(
        ui,
        "This is an unofficial client. Using it may be against WhatsApp's terms of service and could get an account suspended.",
        theme::regular(12.5),
        palette.secondary,
    );
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if theme::link(ui, "Source code", theme::medium(13.0), palette.link).clicked() {
            app.actions
                .push(Action::OpenUrl(env!("CARGO_PKG_REPOSITORY").to_owned()));
        }
        theme::text(ui, "·", theme::regular(13.0), palette.dim);
        if theme::link(ui, "whatsapp-rust", theme::medium(13.0), palette.link).clicked() {
            app.actions.push(Action::OpenUrl(
                "https://github.com/oxidezap/whatsapp-rust".to_owned(),
            ));
        }
    });
    ui.add_space(10.0);
    if app.update_checking {
        theme::text(
            ui,
            "Checking for updates…",
            theme::regular(13.0),
            palette.secondary,
        );
    } else if theme::pill_button(ui, &palette, "Check for updates", false).clicked() {
        app.actions.push(Action::CheckUpdatesNow);
    }
}

fn confirm_unlink(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    title(ui, app, "Unlink this computer?");
    theme::paragraph(
        ui,
        "This removes the device from WhatsApp and deletes the chats stored here. You can link again with a new code.",
        theme::regular(13.5),
        palette.text,
    );
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if danger_button(ui, app, "Unlink") {
                app.actions.push(Action::Unlink);
            }
            if theme::pill_button(ui, &palette, "Cancel", false).clicked() {
                app.actions.push(Action::CloseDialog);
            }
        });
    });
}

fn pair_with_phone(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    title(ui, app, "Link with a phone number");
    theme::paragraph(
        ui,
        "Enter the WhatsApp phone number with its country code. Do not include a plus sign or leading zero. You will get a code to enter on the phone.",
        theme::regular(13.0),
        palette.secondary,
    );
    ui.add_space(4.0);
    let id = egui::Id::new("pair-phone");
    let submit = ui.memory(|memory| memory.has_focus(id))
        && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
    Frame::new()
        .fill(palette.surface)
        .corner_radius(CornerRadius::same(theme::RADIUS))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // Set the row height from the field instead of the shorter label.
                ui.set_min_height(
                    ui.ctx()
                        .fonts_mut(|fonts| fonts.row_height(&theme::regular(16.0)))
                        + 4.0,
                );
                theme::text(ui, "+", theme::semibold(16.0), palette.secondary);
                let response = ui.add(
                    egui::TextEdit::singleline(&mut app.pair_phone)
                        .id(id)
                        .hint_text(
                            egui::RichText::new("15551234567")
                                .color(palette.dim)
                                .font(theme::regular(16.0)),
                        )
                        .font(theme::regular(16.0))
                        .text_color(palette.text)
                        .frame(egui::Frame::NONE)
                        .desired_width(f32::INFINITY),
                );
                if !response.has_focus() && app.pair_phone.is_empty() {
                    response.request_focus();
                }
            });
        });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let ready = app.pair_phone.chars().filter(char::is_ascii_digit).count() >= 7;
            if (theme::pill_button(ui, &palette, "Get a code", ready).clicked() || submit) && ready
            {
                app.actions
                    .push(Action::PairWithPhone(app.pair_phone.clone()));
            }
            if theme::pill_button(ui, &palette, "Cancel", false).clicked() {
                app.actions.push(Action::CloseDialog);
            }
        });
    });
}

fn new_contact(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    title(ui, app, "New contact");
    theme::paragraph(
        ui,
        "Enter a phone number with its country code, without a plus sign or leading zero. Add a name to save the contact, or leave it blank to open the chat. WhatsApp uses the first name as the display name.",
        theme::regular(13.0),
        palette.secondary,
    );
    ui.add_space(4.0);
    let boxed =
        |ui: &mut egui::Ui, plus: bool, inner: &mut dyn FnMut(&mut egui::Ui) -> egui::Response| {
            Frame::new()
                .fill(palette.surface)
                .corner_radius(CornerRadius::same(theme::RADIUS))
                .inner_margin(Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        // Use the field height to align the plus sign.
                        ui.set_min_height(
                            ui.ctx()
                                .fonts_mut(|fonts| fonts.row_height(&theme::regular(16.0)))
                                + 4.0,
                        );
                        if plus {
                            theme::text(ui, "+", theme::semibold(16.0), palette.secondary);
                        }
                        inner(ui)
                    })
                    .inner
                })
                .inner
        };
    macro_rules! edit {
        ($buffer:expr, $salt:literal, $hint:literal, $width:expr) => {
            egui::TextEdit::singleline($buffer)
                .id(egui::Id::new($salt))
                .hint_text(
                    egui::RichText::new($hint)
                        .color(palette.dim)
                        .font(theme::regular(16.0)),
                )
                .font(theme::regular(16.0))
                .text_color(palette.text)
                .frame(egui::Frame::NONE)
                .desired_width($width)
        };
    }
    let phone_empty = app.new_contact_phone.is_empty();
    let phone_field = boxed(ui, true, &mut |ui| {
        let response = ui.add(edit!(
            &mut app.new_contact_phone,
            "new-contact-phone",
            "15551234567",
            f32::INFINITY
        ));
        if !response.has_focus() && phone_empty {
            response.request_focus();
        }
        response
    });
    ui.add_space(4.0);
    let (first_field, last_field) = ui
        .horizontal(|ui| {
            let half = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0 - 24.0;
            let first = boxed(ui, false, &mut |ui| {
                ui.add(edit!(
                    &mut app.new_contact_name,
                    "new-contact-first",
                    "First name",
                    half
                ))
            });
            let last = boxed(ui, false, &mut |ui| {
                ui.add(edit!(
                    &mut app.new_contact_last,
                    "new-contact-last",
                    "Surname",
                    half
                ))
            });
            (first, last)
        })
        .inner;
    let digits: String = app
        .new_contact_phone
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    let ready = digits.len() >= 7 && !app.new_contact_pending;
    let named = !app.new_contact_name.trim().is_empty() || !app.new_contact_last.trim().is_empty();
    let submitted =
        (phone_field.lost_focus() || first_field.lost_focus() || last_field.lost_focus())
            && ui.input(|input| input.key_pressed(egui::Key::Enter));
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if app.new_contact_pending {
            theme::spinner(ui, 16.0, palette.accent);
            theme::text(
                ui,
                "Checking the number…",
                theme::regular(12.5),
                palette.secondary,
            );
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let save = theme::pill_button(ui, &palette, "Save contact", ready && named).clicked();
            let message = theme::pill_button(ui, &palette, "Message", ready && !named).clicked();
            if theme::pill_button(ui, &palette, "Cancel", false).clicked() {
                app.actions.push(Action::CloseDialog);
            }
            // Enter saves a named contact or opens an unnamed chat.
            if ready && (save || (submitted && named)) {
                app.actions.push(Action::NewContact {
                    phone: digits.clone(),
                    first: app.new_contact_name.trim().to_owned(),
                    last: app.new_contact_last.trim().to_owned(),
                });
            } else if ready && (message || submitted) {
                app.actions.push(Action::NewContact {
                    phone: digits.clone(),
                    first: String::new(),
                    last: String::new(),
                });
            }
        });
    });
}

fn chat_info(app: &mut App, ui: &mut egui::Ui, id: &str) {
    let palette = app.palette;
    // Group members may not have an existing chat.
    let chat = app
        .chat(id)
        .cloned()
        .unwrap_or_else(|| crate::model::Chat::new(id.to_owned(), app.display_name(id)));
    let has_chat = app.chat(id).is_some();
    let name = app.chat_title(&chat);
    title(ui, app, if chat.is_group() { "Group" } else { "Contact" });
    // Scale the photo and member list to fit the window.
    let window = ui.ctx().content_rect().height();
    let photo = (window * 0.34).clamp(120.0, 240.0);
    let picture = app.avatar_full(id).or_else(|| app.avatar(id));
    let mine = app.me.as_deref() == Some(id);
    let editable = chat.phone().is_some() && !mine;
    // Saving or cancelling leaves the editor buffer checked out.
    let mut editing = app.contact_edit.take().filter(|_| editable);
    let mut saved = None;
    ui.vertical_centered(|ui| {
        super::widgets::avatar(ui, &palette, &name, id, photo, picture.as_deref());
        ui.add_space(6.0);
        if let Some((first, last)) = editing.as_mut() {
            let mut submit = false;
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - 288.0).max(0.0) / 2.0);
                let name_field = |ui: &mut egui::Ui, buffer: &mut String, salt: &str, hint| {
                    Frame::new()
                        .fill(palette.surface)
                        .corner_radius(CornerRadius::same(theme::RADIUS))
                        .inner_margin(Margin::symmetric(10, 5))
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::singleline(buffer)
                                    .id(egui::Id::new(salt))
                                    .hint_text(
                                        egui::RichText::new(hint)
                                            .color(palette.dim)
                                            .font(theme::semibold(15.0)),
                                    )
                                    .font(theme::semibold(15.0))
                                    .text_color(palette.text)
                                    .frame(Frame::NONE)
                                    .desired_width(108.0),
                            )
                        })
                        .inner
                };
                let first_field = name_field(ui, first, "contact-first", "First name");
                let last_field = name_field(ui, last, "contact-last", "Surname");
                if ui.memory(|memory| memory.focused().is_none()) {
                    first_field.request_focus();
                }
                submit = (first_field.lost_focus() || last_field.lost_focus())
                    && ui.input(|input| input.key_pressed(egui::Key::Enter));
                if theme::icon_button(
                    ui,
                    Icon::Check,
                    18.0,
                    palette.secondary,
                    palette.accent,
                    "Save name (Enter)",
                )
                .clicked()
                {
                    submit = true;
                }
            });
            if submit && !(first.trim().is_empty() && last.trim().is_empty()) {
                saved = Some((first.trim().to_owned(), last.trim().to_owned()));
            }
        } else {
            super::widgets::selectable_rich_text(ui, &name, theme::bold(19.0), palette.text);
        }
        if let Some(phone) = chat.phone() {
            theme::selectable_text(
                ui,
                crate::util::phone(phone),
                theme::regular(13.5),
                palette.secondary,
            );
        }
        if chat.is_group() && !chat.participants.is_empty() {
            theme::text(
                ui,
                format!("{} members", chat.participants.len()),
                theme::regular(13.5),
                palette.secondary,
            );
        }
        if let Some(presence) = app.presence.get(id) {
            let status = if presence.online {
                "online".to_owned()
            } else if let Some(seen) = presence.last_seen {
                format!("last seen {}", crate::util::chat_stamp(seen).to_lowercase())
            } else {
                String::new()
            };
            if !status.is_empty() {
                theme::text(ui, status, theme::regular(12.5), palette.dim);
            }
        }
    });
    if let Some((first, last)) = saved {
        editing = None;
        app.actions.push(Action::SaveContact {
            id: id.to_owned(),
            first,
            last,
        });
    }
    app.contact_edit = editing;
    ui.add_space(8.0);
    if chat.is_group() && !chat.participants.is_empty() {
        let members = app.participant_list(&chat);
        theme::text(
            ui,
            format!("Members ({})", members.len()),
            theme::medium(12.5),
            palette.secondary,
        );
        ui.add_space(4.0);
        // Limit the visible rows because groups can have thousands of members.
        let row_height = 30.0;
        let rows = ((window - photo - 300.0) / row_height)
            .floor()
            .clamp(2.0, 8.0);
        let mut open = None;
        egui::ScrollArea::vertical()
            .id_salt("members")
            .max_height(row_height * rows)
            .auto_shrink([false, true])
            .show_rows(ui, row_height, members.len(), |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for (member, name) in &members[range] {
                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), row_height),
                        egui::Sense::click(),
                    );
                    if ui.is_rect_visible(rect) {
                        if response.hovered() {
                            ui.painter().rect_filled(rect, 6.0, palette.surface_hover);
                        }
                        let picture = app.avatar(member);
                        let avatar = egui::Rect::from_center_size(
                            egui::pos2(rect.left() + 16.0, rect.center().y),
                            egui::Vec2::splat(24.0),
                        );
                        super::widgets::paint_avatar(
                            ui,
                            &palette,
                            avatar,
                            name.trim_start_matches('~'),
                            member,
                            picture.as_deref(),
                        );
                        let line = super::widgets::line(
                            ui,
                            name,
                            theme::regular(13.0),
                            palette.text,
                            rect.width() - 40.0,
                            1,
                        );
                        line.paint(
                            ui,
                            egui::pos2(rect.left() + 34.0, rect.center().y - line.size().y / 2.0),
                            palette.text,
                        );
                    }
                    if response
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        open = Some(member.clone());
                    }
                }
            });
        if let Some(member) = open
            && Some(member.as_str()) != app.me.as_deref()
        {
            app.actions
                .push(Action::ShowDialog(Dialog::ChatInfo(member)));
        }
        ui.add_space(6.0);
    }
    if let Some(until) = chat.muted_until {
        theme::text(
            ui,
            if until == 0 {
                "Muted".to_owned()
            } else {
                format!("Muted until {}", crate::util::chat_stamp(until))
            },
            theme::regular(12.5),
            palette.secondary,
        );
    }
    ui.add_space(4.0);
    // Center the available actions below the picture.
    let mut buttons: Vec<(Icon, &str, Vec<Action>)> = Vec::new();
    if !chat.is_group() && !mine {
        buttons.push((
            Icon::MessageCircle,
            "Message",
            vec![
                Action::StartChat {
                    id: chat.id.clone(),
                    name: name.clone(),
                },
                Action::CloseDialog,
            ],
        ));
    }
    if editable {
        let known = app
            .contacts
            .get(id)
            .and_then(|contact| contact.full_name.as_deref())
            .is_some_and(|full| !full.is_empty());
        buttons.push((
            Icon::User,
            if known { "Rename" } else { "Add to contacts" },
            vec![Action::EditContact(name.trim_start_matches('~').to_owned())],
        ));
    }
    if let Some(phone) = chat.phone() {
        buttons.push((
            Icon::Copy,
            "Copy number",
            vec![Action::CopyText(format!("+{phone}"))],
        ));
    }
    if has_chat {
        let muted = chat.muted(crate::util::now());
        buttons.push(if muted {
            (
                Icon::Bell,
                "Unmute",
                vec![Action::SetMuted(chat.id.clone(), None)],
            )
        } else {
            (
                Icon::BellOff,
                "Mute",
                vec![Action::SetMuted(chat.id.clone(), Some(0))],
            )
        });
        buttons.push((
            if chat.pinned { Icon::PinOff } else { Icon::Pin },
            if chat.pinned { "Unpin" } else { "Pin" },
            vec![Action::SetPinned(chat.id.clone(), !chat.pinned)],
        ));
        buttons.push((
            Icon::Archive,
            if chat.archived {
                "Unarchive"
            } else {
                "Archive"
            },
            vec![
                Action::SetArchived(chat.id.clone(), !chat.archived),
                Action::CloseDialog,
            ],
        ));
    }
    let spacing = ui.spacing().item_spacing.x;
    let available = ui.available_width();
    let mut fired: Option<Vec<Action>> = None;
    let mut start = 0;
    while start < buttons.len() {
        // Fit and center as many buttons as each row allows.
        let mut end = start;
        let mut total = 0.0;
        while end < buttons.len() {
            let width = theme::soft_button_width(ui, buttons[end].1, true);
            let grown = if end == start {
                width
            } else {
                total + spacing + width
            };
            if end > start && grown > available {
                break;
            }
            total = grown;
            end += 1;
        }
        ui.horizontal(|ui| {
            ui.add_space((available - total).max(0.0) / 2.0);
            for (icon, label, actions) in &buttons[start..end] {
                if theme::soft_button(ui, &palette, Some(*icon), label, false).clicked() {
                    fired = Some(actions.clone());
                }
            }
        });
        start = end;
    }
    if let Some(actions) = fired {
        app.actions.extend(actions);
    }
}

/// A filled button for a destructive action.
fn danger_button(ui: &mut egui::Ui, app: &mut App, label: &str) -> bool {
    let palette = app.palette;
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        theme::semibold(13.0),
        egui::Color32::WHITE,
    );
    let size = galley.size() + egui::vec2(36.0, 16.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if response.hovered() {
            palette.danger.gamma_multiply(0.85)
        } else {
            palette.danger
        };
        ui.painter().rect_filled(rect, rect.height() / 2.0, fill);
        ui.painter().galley(
            rect.center() - galley.size() / 2.0,
            galley,
            egui::Color32::WHITE,
        );
    }
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_dialog_splits_revoke_and_local() {
        let all = delete_many_copy(5, 5);
        assert_eq!(all.title, "Delete 5 messages?");
        assert_eq!(all.note, "These 5 messages will be deleted for everyone.");
        assert!(all.revocable);
        assert_eq!(all.local_label, "Delete for me");

        let none = delete_many_copy(3, 0);
        assert!(!none.revocable);
        assert_eq!(none.local_label, "Delete");
        assert!(
            none.note.contains("too old or not yours"),
            "unrevocable says why: {}",
            none.note
        );

        let mixed = delete_many_copy(5, 2);
        assert!(mixed.revocable);
        assert_eq!(mixed.local_label, "Delete for me");
        assert!(
            mixed.note.starts_with("2 of 5"),
            "mixed names both parts: {}",
            mixed.note
        );

        // An inconsistent caller cannot offer revoking more than everything.
        let clamped = delete_many_copy(2, 9);
        assert!(clamped.revocable);
        assert_eq!(clamped.local_label, "Delete for me");
    }
}
