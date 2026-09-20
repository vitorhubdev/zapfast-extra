//! Keyboard shortcuts.

use egui::{Key, Modifiers};

use crate::app::App;
use crate::model::{Action, Dialog, Page};

/// Whether a viewer slider holds the keyboard, so the arrows adjust it.
fn viewer_control_focused(ctx: &egui::Context) -> bool {
    ctx.data(|data| {
        data.get_temp::<bool>(crate::ui::viewer::control_focus_id())
            .unwrap_or(false)
    })
}

/// Keys the media viewer owns: browsing, zooming, and closing.
fn viewer_keys(app: &mut App, ctx: &egui::Context) {
    // A focused progress or volume slider owns the arrows; browsing yields.
    let slider_focused = viewer_control_focused(ctx);
    const STEP: f32 = 1.3;
    let pdf = app
        .viewer
        .as_ref()
        .and_then(|viewer| viewer.current())
        .is_some_and(|item| item.kind == crate::model::ViewerKind::Pdf);
    let video = app
        .viewer
        .as_ref()
        .and_then(|viewer| viewer.current())
        .is_some_and(|item| item.kind == crate::model::ViewerKind::Video);
    let mut actions = Vec::new();
    // While a page number is being typed, the arrows and the page keys belong
    // to that field and not to the document.
    let typing_page = ctx.memory(|memory| memory.has_focus(crate::ui::viewer::page_field_id()));
    ctx.input_mut(|input| {
        let mut key = |modifiers: Modifiers, key: Key, action: Action| {
            if input.consume_key(modifiers, key) {
                actions.push(action);
            }
        };
        // A PDF walks its pages with all four arrows, as the buttons in its
        // bar do; the other files are walked as pictures.
        if pdf {
            if !typing_page {
                key(Modifiers::NONE, Key::ArrowDown, Action::ViewerPage(1));
                key(Modifiers::NONE, Key::ArrowUp, Action::ViewerPage(-1));
                key(Modifiers::NONE, Key::ArrowRight, Action::ViewerPage(1));
                key(Modifiers::NONE, Key::ArrowLeft, Action::ViewerPage(-1));
                key(Modifiers::NONE, Key::PageDown, Action::ViewerPage(10));
                key(Modifiers::NONE, Key::PageUp, Action::ViewerPage(-10));
                key(Modifiers::NONE, Key::Home, Action::ViewerPageTo(1));
                key(Modifiers::NONE, Key::End, Action::ViewerPageTo(usize::MAX));
                key(Modifiers::NONE, Key::R, Action::ViewerRotate);
            }
        } else {
            // A focused slider adjusts itself with the arrows instead.
            if !slider_focused {
                key(Modifiers::NONE, Key::ArrowRight, Action::ViewerStep(1));
                key(Modifiers::NONE, Key::ArrowLeft, Action::ViewerStep(-1));
                key(Modifiers::NONE, Key::ArrowDown, Action::ViewerStep(1));
                key(Modifiers::NONE, Key::ArrowUp, Action::ViewerStep(-1));
            }
        }
        if video {
            key(Modifiers::NONE, Key::Space, Action::VideoToggle);
        }
        if video {
            key(Modifiers::NONE, Key::M, Action::VideoMuteToggle);
        }
        key(
            Modifiers::NONE,
            Key::Plus,
            Action::ViewerZoom {
                factor: STEP,
                anchor: (0.0, 0.0),
            },
        );
        key(
            Modifiers::NONE,
            Key::Equals,
            Action::ViewerZoom {
                factor: STEP,
                anchor: (0.0, 0.0),
            },
        );
        key(
            Modifiers::NONE,
            Key::Minus,
            Action::ViewerZoom {
                factor: 1.0 / STEP,
                anchor: (0.0, 0.0),
            },
        );
        key(Modifiers::NONE, Key::Num0, Action::ViewerFit);
        key(Modifiers::NONE, Key::F, Action::ViewerFit);
        key(Modifiers::NONE, Key::Escape, Action::CloseViewer);
    });
    app.actions.extend(actions);
}

pub fn handle(app: &mut App, ctx: &egui::Context) {
    // The media viewer owns the keyboard while it is on screen.
    if app.viewer.is_some() {
        viewer_keys(app, ctx);
        return;
    }
    let mut actions = Vec::new();
    ctx.input_mut(|input| {
        let mut key = |modifiers: Modifiers, key: Key, action: Action| {
            if input.consume_key(modifiers, key) {
                actions.push(action);
            }
        };
        key(Modifiers::COMMAND, Key::F, Action::FocusSearch);
        key(Modifiers::COMMAND, Key::K, Action::FocusSearch);
        if app.page == Page::Chats
            && app.open_chat.is_some()
            && app.dialog.is_none()
            && !app.show_update
            && app.recording.is_none()
        {
            key(Modifiers::COMMAND, Key::L, Action::FocusComposer);
        }
        key(Modifiers::COMMAND, Key::B, Action::ToggleSidebar);
        key(Modifiers::COMMAND, Key::Comma, Action::Open(Page::Settings));
        key(Modifiers::COMMAND, Key::Q, Action::Quit);
        key(Modifiers::COMMAND, Key::W, Action::CloseWindow);
        key(
            Modifiers::COMMAND,
            Key::Slash,
            Action::ShowDialog(Dialog::Shortcuts),
        );
        key(Modifiers::COMMAND, Key::Plus, Action::ZoomBy(0.1));
        key(Modifiers::COMMAND, Key::Equals, Action::ZoomBy(0.1));
        key(Modifiers::COMMAND, Key::Minus, Action::ZoomBy(-0.1));
        key(Modifiers::COMMAND, Key::Num0, Action::ResetZoom);
        key(Modifiers::COMMAND, Key::End, Action::ScrollToBottom);
    });
    // Escape cancels the topmost state. Menus handle Escape themselves.
    let menu_open = egui::Popup::is_any_open(ctx);
    let search_focused = ctx.memory(|memory| memory.has_focus(egui::Id::new("chat-search")));
    let escape =
        !menu_open && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
    if escape {
        if app.show_update {
            actions.push(Action::CloseUpdate);
        } else if app.dialog.is_some() {
            actions.push(Action::CloseDialog);
        } else if app.chat_search_open {
            actions.push(Action::CloseChatSearch);
        } else if !app.selected.is_empty() {
            actions.push(Action::ClearSelection);
        } else if app.recording.is_some() {
            actions.push(Action::CancelRecording);
        } else if app.picker.is_some() {
            actions.push(Action::ClosePicker);
        } else if app.emoji_start.is_some() {
            actions.push(Action::CloseEmojiSuggestions);
        } else if app.mention_start.is_some() {
            actions.push(Action::CloseMentions);
        } else if !app.pending.is_empty() {
            actions.push(Action::ClearPending);
        } else if app.editing.is_some() {
            actions.push(Action::CancelEdit);
        } else if app.reply_to.is_some() {
            actions.push(Action::CancelReply);
        } else if app.page == Page::Settings {
            actions.push(Action::Open(Page::Chats));
        } else if search_focused || !app.search.is_empty() {
            if !app.search.is_empty() {
                actions.push(Action::Search(String::new()));
            }
            if app.open_chat.is_some() {
                actions.push(Action::FocusComposer);
            }
        } else if app.open_chat.is_some() {
            actions.push(Action::CloseChat);
        }
    }
    // Enter sends a recording because the text field is hidden.
    if app.recording.is_some()
        && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter))
    {
        actions.push(Action::SendRecording);
    }
    // Alt+Up/Down switches chats without leaving the composer.
    let step = ctx.input_mut(|input| {
        if input.consume_key(Modifiers::ALT, Key::ArrowDown) {
            1
        } else if input.consume_key(Modifiers::ALT, Key::ArrowUp) {
            -1
        } else {
            0
        }
    });
    if step != 0 {
        let visible = app.visible_chats();
        if !visible.is_empty() {
            let current = app
                .open_chat
                .as_ref()
                .and_then(|open| visible.iter().position(|chat| chat.id == *open));
            let next = match current {
                Some(index) => (index as i64 + step).rem_euclid(visible.len() as i64) as usize,
                None => 0,
            };
            let next = visible[next].id.clone();
            app.scroll_chat_into_view = Some(next.clone());
            actions.push(Action::OpenChat(next));
        }
    }
    app.actions.extend(actions);
}

/// Shortcuts shown in the help dialog.
pub const SHORTCUTS: &[(&str, &str)] = &[
    ("Ctrl+F / Ctrl+K", "Search chats"),
    ("Ctrl+L", "Focus the message input"),
    ("Alt+↑ / Alt+↓", "Previous / next chat"),
    ("Enter", "Send (Shift+Enter for a new line)"),
    (
        "Escape",
        "Dismiss the current action, return from search, or close the chat",
    ),
    ("Ctrl+V", "Paste text, or send a picture from the clipboard"),
    ("Ctrl+B", "Show or hide the chat list"),
    ("Ctrl+End", "Jump to the newest message"),
    ("Ctrl+,", "Settings"),
    ("Ctrl++ / Ctrl+-", "Zoom in / out"),
    ("Ctrl+0", "Reset zoom"),
    ("Ctrl+/", "This list"),
    ("Ctrl+W", "Close the window (ZapExt remains in the tray)"),
    ("Ctrl+Q", "Quit"),
];

/// Uses Command and Option labels on macOS.
pub fn label(keys: &str) -> String {
    if cfg!(target_os = "macos") {
        keys.replace("Ctrl", "⌘").replace("Alt", "⌥")
    } else {
        keys.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrows_yield_to_a_focused_slider() {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::headless(
            crate::paths::AppDirs::under(root.path()),
            crate::settings::Settings::default(),
        )
        .0;
        let item = |message: &str, path: &str| crate::model::ViewerItem {
            message: message.to_owned(),
            path: std::path::PathBuf::from(path),
            kind: crate::model::ViewerKind::Video,
        };
        app.viewer = Some(crate::model::Viewer {
            chat: "chat".to_owned(),
            items: vec![item("a", "a.mp4"), item("b", "b.mp4")],
            index: 0,
            zoom: 1.0,
            offset: (0.0, 0.0),
            pdf_page: 0,
            pdf_pages: 0,
            pdf_rotate: 0,
        });
        let ctx = egui::Context::default();
        fn press(app: &mut App, ctx: &egui::Context) {
            app.actions.clear();
            let mut output = ctx.run_ui(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key: Key::ArrowRight,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: Modifiers::NONE,
                    }],
                    ..Default::default()
                },
                |ui| handle(app, ui.ctx()),
            );
            output.textures_delta.clear();
        }
        press(&mut app, &ctx);
        assert!(
            app.actions
                .iter()
                .any(|action| matches!(action, Action::ViewerStep(1))),
            "arrows browse with no control focused"
        );
        // A focused progress slider keeps the arrows for itself.
        ctx.data_mut(|data| data.insert_temp(crate::ui::viewer::control_focus_id(), true));
        press(&mut app, &ctx);
        assert!(
            !app.actions
                .iter()
                .any(|action| matches!(action, Action::ViewerStep(_))),
            "the file on screen stays put while a slider is focused"
        );
    }

    #[test]
    fn arrows_follow_a_real_focused_slider() {
        // A genuine slider, focused through the real control id: the arrows
        // adjust it instead of stepping media, exactly like the viewer bar.
        let root = tempfile::tempdir().unwrap();
        let mut app = App::headless(
            crate::paths::AppDirs::under(root.path()),
            crate::settings::Settings::default(),
        )
        .0;
        let item = |message: &str, path: &str| crate::model::ViewerItem {
            message: message.to_owned(),
            path: std::path::PathBuf::from(path),
            kind: crate::model::ViewerKind::Video,
        };
        app.viewer = Some(crate::model::Viewer {
            chat: "chat".to_owned(),
            items: vec![item("a", "a.mp4"), item("b", "b.mp4")],
            index: 0,
            zoom: 1.0,
            offset: (0.0, 0.0),
            pdf_page: 0,
            pdf_pages: 0,
            pdf_rotate: 0,
        });
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 400.0));
        // Pass one: draw the slider inside a stable id scope and keep it.
        let slider = std::cell::Cell::new(egui::Id::NULL);
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ui| {
                ui.push_id("probe", |ui| {
                    let mut value = 0.5;
                    slider.set(ui.add(egui::Slider::new(&mut value, 0.0..=1.0)).id);
                });
            },
        );
        output.textures_delta.clear();
        assert_ne!(slider.get(), egui::Id::NULL);
        // Pass two: a genuine Tab moves keyboard focus onto the slider, the
        // gesture the audit asked for. Focus state is read out of the pass;
        // the production wiring raises the flag outside, where writes rest.
        let key_event = |key: Key| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        let focused = std::cell::Cell::new(false);
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![key_event(Key::Tab)],
                ..Default::default()
            },
            |ui| {
                ui.push_id("probe", |ui| {
                    let mut value = 0.5;
                    focused.set(ui.add(egui::Slider::new(&mut value, 0.0..=1.0)).has_focus());
                });
            },
        );
        output.textures_delta.clear();
        assert!(focused.get(), "Tab focuses the real slider");
        // Pass three: the slider is drawn again under the same id, so it
        // keeps its real focus, and the flag follows the live widget state
        // exactly like the viewer bar raises it. Nothing is hand-set, and
        // its value is carried between passes the way the viewer carries
        // the playback position.
        let value = std::cell::Cell::new(0.5f32);
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ui| {
                ui.push_id("probe", |ui| {
                    let mut current = value.get();
                    let response = ui.add(egui::Slider::new(&mut current, 0.0..=1.0));
                    value.set(current);
                    if response.has_focus() {
                        ui.data_mut(|data| {
                            data.insert_temp(crate::ui::viewer::control_focus_id(), true)
                        });
                    }
                    focused.set(response.has_focus());
                });
            },
        );
        output.textures_delta.clear();
        assert!(focused.get(), "the slider keeps its real focus");
        // Pass four: shortcuts run before the viewer draws, exactly as the
        // application orders them, and the focused slider takes the arrow
        // for itself: its value moves and no media step is queued.
        app.actions.clear();
        let before = value.get();
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![key_event(Key::ArrowRight)],
                ..Default::default()
            },
            |ui| {
                handle(&mut app, ui.ctx());
                ui.push_id("probe", |ui| {
                    let mut current = value.get();
                    let response = ui.add(egui::Slider::new(&mut current, 0.0..=1.0));
                    value.set(current);
                    if response.has_focus() {
                        ui.data_mut(|data| {
                            data.insert_temp(crate::ui::viewer::control_focus_id(), true)
                        });
                    }
                });
            },
        );
        output.textures_delta.clear();
        assert!(
            value.get() > before,
            "the arrow adjusts the slider: {before} -> {}",
            value.get()
        );
        assert!(
            !app.actions
                .iter()
                .any(|action| matches!(action, Action::ViewerStep(_))),
            "a focused slider keeps the arrows"
        );
    }

    fn escape(app: &mut App, ctx: &egui::Context) {
        let mut output = ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| handle(app, ui.ctx()),
        );
        output.textures_delta.clear();
    }

    #[test]
    fn focus_input_shortcut_only_targets_an_available_composer() {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::headless(
            crate::paths::AppDirs::under(root.path()),
            crate::settings::Settings::default(),
        )
        .0;
        let ctx = egui::Context::default();
        for (page, chat, dialog, expected) in [
            (Page::Chats, Some("fixture"), None, true),
            (Page::Chats, None, None, false),
            (Page::Settings, Some("fixture"), None, false),
            (Page::Chats, Some("fixture"), Some(Dialog::Shortcuts), false),
        ] {
            app.page = page;
            app.open_chat = chat.map(str::to_owned);
            app.dialog = dialog;
            app.actions.clear();
            let mut output = ctx.run_ui(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key: Key::L,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: Modifiers::COMMAND,
                    }],
                    ..Default::default()
                },
                |ui| handle(&mut app, ui.ctx()),
            );
            output.textures_delta.clear();
            assert_eq!(
                matches!(app.actions.as_slice(), [Action::FocusComposer]),
                expected
            );
        }
    }

    #[test]
    fn escape_returns_from_search_and_reply_before_closing_the_chat() {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::headless(
            crate::paths::AppDirs::under(root.path()),
            crate::settings::Settings::default(),
        )
        .0;
        app.page = Page::Chats;
        app.open_chat = Some("fixture".into());
        let ctx = egui::Context::default();
        app.reply_to = Some("reply".into());
        escape(&mut app, &ctx);
        assert!(matches!(app.actions.as_slice(), [Action::CancelReply]));
        app.actions.clear();
        app.reply_to = None;
        app.search = "Ada".into();
        escape(&mut app, &ctx);
        assert!(
            matches!(app.actions.as_slice(), [Action::Search(text), Action::FocusComposer] if text.is_empty())
        );
        app.actions.clear();
        app.search.clear();
        escape(&mut app, &ctx);
        assert!(matches!(app.actions.as_slice(), [Action::CloseChat]));
    }

    #[test]
    fn escape_closes_the_update_before_touching_an_unfinished_message() {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::headless(
            crate::paths::AppDirs::under(root.path()),
            crate::settings::Settings::default(),
        )
        .0;
        app.show_update = true;
        app.page = Page::Settings;
        app.reply_to = Some("reply-fixture".into());
        app.pending
            .push(crate::app::Pending::File("unsent.png".into()));
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| handle(&mut app, ui.ctx()),
        );
        output.textures_delta.clear();
        assert!(matches!(app.actions.as_slice(), [Action::CloseUpdate]));
    }
}
