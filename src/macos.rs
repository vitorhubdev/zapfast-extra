//! Main-thread AppKit chrome and application menus. The menu outlives windows,
//! just like the link and tray; reopening replaces only its repaint callback.

use std::cell::RefCell;
use std::sync::Mutex;

use objc2_app_kit::{NSApplication, NSView, NSWindowButton};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem as Native, Submenu};

use crate::model::{Action, Dialog, Page};

thread_local! {
    static MENU: RefCell<Option<Menu>> = const { RefCell::new(None) };
}
static EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static REPAINT: Mutex<Option<egui::Context>> = Mutex::new(None);

fn item(id: &str, text: &str, shortcut: Option<&str>) -> MenuItem {
    MenuItem::with_id(
        id,
        text,
        true,
        shortcut.map(|key| key.parse().expect("menu shortcut")),
    )
}

fn build_menu() -> tray_icon::menu::Result<Menu> {
    let menu = Menu::new();
    let app = Submenu::new("ZapExt", true);
    app.append_items(&[
        &item("about", "About ZapExt", None),
        &Native::separator(),
        &item("settings", "Settings…", Some("Super+Comma")),
        &Native::separator(),
        &Native::services(None),
        &Native::separator(),
        &Native::hide(Some("Hide ZapExt")),
        &Native::hide_others(None),
        &Native::show_all(None),
        &Native::separator(),
        &item("quit", "Quit ZapExt", Some("Super+KeyQ")),
    ])?;
    let file = Submenu::new("File", true);
    file.append_items(&[
        &item("new", "New Contact…", Some("Super+KeyN")),
        &Native::separator(),
        &item("close", "Close Window", Some("Super+KeyW")),
    ])?;
    let edit = Submenu::new("Edit", true);
    // Winit's view is not an NSTextView: AppKit's copy:/undo: selectors
    // cannot edit egui text. Send the same events as its keyboard shortcuts.
    edit.append_items(&[
        &item("undo", "Undo", Some("Super+KeyZ")),
        &item("redo", "Redo", Some("Super+Shift+KeyZ")),
        &Native::separator(),
        &item("cut", "Cut", Some("Super+KeyX")),
        &item("copy", "Copy", Some("Super+KeyC")),
        &item("paste", "Paste", Some("Super+KeyV")),
        &item("select-all", "Select All", Some("Super+KeyA")),
        &Native::separator(),
        &item("search", "Find…", Some("Super+KeyF")),
    ])?;
    let view = Submenu::new("View", true);
    view.append_items(&[
        &item("sidebar", "Toggle Sidebar", Some("Super+KeyB")),
        &Native::separator(),
        &item("zoom-in", "Zoom In", Some("Super+Equal")),
        &item("zoom-out", "Zoom Out", Some("Super+Minus")),
        &item("zoom-reset", "Actual Size", Some("Super+Digit0")),
        &Native::separator(),
        &Native::fullscreen(None),
    ])?;
    let window = Submenu::new("Window", true);
    window.append_items(&[
        &Native::minimize(None),
        &Native::maximize(Some("Zoom")),
        &Native::separator(),
        &item("show-window", "Show ZapExt", None),
    ])?;
    let help = Submenu::new("Help", true);
    help.append_items(&[
        &item("shortcuts", "Keyboard Shortcuts", Some("Super+Slash")),
        &item("help", "ZapExt Help", None),
    ])?;
    menu.append_items(&[&app, &file, &edit, &view, &window, &help])?;
    window.set_as_windows_menu_for_nsapp();
    help.set_as_help_menu_for_nsapp();
    Ok(menu)
}

pub fn detach() {
    *REPAINT.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

pub fn attach(ctx: &egui::Context) {
    // Layout tests use headless contexts on test threads, without an NSApp.
    if objc2::MainThreadMarker::new().is_none() {
        return;
    }
    *REPAINT.lock().unwrap_or_else(|p| p.into_inner()) = Some(ctx.clone());
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if native_edit(&event.id.0) {
            return;
        }
        EVENTS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(event.id.0);
        if let Some(ctx) = &*REPAINT.lock().unwrap_or_else(|p| p.into_inner()) {
            ctx.request_repaint();
        }
    }));
    MENU.with_borrow_mut(|menu| {
        if menu.is_none() {
            match build_menu() {
                Ok(created) => *menu = Some(created),
                Err(error) => log::warn!("could not create the application menu: {error}"),
            }
        }
        if let Some(menu) = menu {
            menu.init_for_nsapp();
        }
    });
    // Demo windows have no tray to activate the app. This also brings a newly
    // recreated window forward after a menu command while running headless.
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}

/// A native file dialog runs a modal loop, so its text fields must receive
/// editing commands immediately instead of leaving them queued for egui.
fn native_edit(id: &str) -> bool {
    use objc2::sel;
    let selector = match id {
        "copy" => sel!(copy:),
        "cut" => sel!(cut:),
        "paste" => sel!(paste:),
        "undo" => sel!(undo:),
        "redo" => sel!(redo:),
        "select-all" => sel!(selectAll:),
        _ => return false,
    };
    let Some(main) = objc2::MainThreadMarker::new() else {
        return false;
    };
    let app = NSApplication::sharedApplication(main);
    let Some(responder) = app.keyWindow().and_then(|window| window.firstResponder()) else {
        return false;
    };
    // The pinned winit version installs WinitView as its first responder.
    if responder.class().name().to_bytes() == b"WinitView" {
        return false;
    }
    // SAFETY: standard AppKit editing selectors; nil target walks the native
    // responder chain, and these actions accept a nil sender.
    unsafe {
        app.sendAction_to_from(selector, None, None);
    }
    true
}

fn edit_event(id: &str) -> Option<egui::Event> {
    match id {
        "copy" => return Some(egui::Event::Copy),
        "cut" => return Some(egui::Event::Cut),
        "paste" => {
            return Some(egui::Event::Paste(
                arboard::Clipboard::new()
                    .ok()
                    .and_then(|mut clipboard| clipboard.get_text().ok())
                    .unwrap_or_default(),
            ));
        }
        _ => {}
    }
    let (key, shift) = match id {
        "undo" => (egui::Key::Z, false),
        "redo" => (egui::Key::Z, true),
        "select-all" => (egui::Key::A, false),
        _ => return None,
    };
    Some(egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers {
            command: true,
            mac_cmd: true,
            shift,
            ..Default::default()
        },
    })
}

fn action(id: &str, hidden: bool) -> Option<Action> {
    Some(match id {
        "about" => Action::ShowDialog(Dialog::About),
        "settings" => Action::Open(Page::Settings),
        "new" => Action::ShowDialog(Dialog::NewContact),
        "close" => Action::CloseWindow,
        "quit" => Action::Quit,
        "search" => Action::FocusSearch,
        "sidebar" => Action::ToggleSidebar,
        "zoom-in" => Action::ZoomBy(0.1),
        "zoom-out" => Action::ZoomBy(-0.1),
        "zoom-reset" => Action::ResetZoom,
        "shortcuts" => Action::ShowDialog(Dialog::Shortcuts),
        "help" => Action::OpenUrl("https://zapfast.rocks/using-zapfast/".into()),
        "show-window" => Action::ShowWindow,
        // These two ids are shared with the native tray menu.
        "show" => {
            if hidden {
                Action::ShowWindow
            } else {
                Action::HideWindow
            }
        }
        _ => return None,
    })
}

pub fn drain(ctx: &egui::Context, hidden: bool) -> Vec<Action> {
    let events = std::mem::take(&mut *EVENTS.lock().unwrap_or_else(|p| p.into_inner()));
    let mut actions = Vec::new();
    for id in events {
        if let Some(event) = edit_event(&id) {
            if !hidden {
                ctx.input_mut(|input| input.events.push(event));
                if id == "paste" {
                    // Image paste follows the same key-release path as Cmd+V.
                    ctx.input_mut(|input| {
                        input.events.push(egui::Event::Key {
                            key: egui::Key::V,
                            physical_key: None,
                            pressed: false,
                            repeat: false,
                            modifiers: egui::Modifiers {
                                command: true,
                                mac_cmd: true,
                                ..Default::default()
                            },
                        })
                    });
                }
            }
        } else if let Some(action) = action(&id, hidden) {
            if hidden
                && matches!(
                    action,
                    Action::ShowDialog(_) | Action::Open(_) | Action::FocusSearch
                )
            {
                actions.push(Action::ShowWindow);
            }
            actions.push(action);
        }
    }
    actions
}

/// Reapply after resizing, zooming, or recreating the native window. AppKit
/// restores standard button positions during its own window layout passes.
pub fn update_window(frame: &eframe::Frame, ctx: &egui::Context, linked: bool) {
    if ctx.input(|input| input.viewport().fullscreen.unwrap_or(false)) {
        return;
    }
    let Ok(handle) = frame.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    // SAFETY: eframe supplies a live NSView and this runs on its main thread.
    let view = unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() };
    let Some(window) = view.window() else { return };
    let Some(close) = window.standardWindowButton(NSWindowButton::CloseButton) else {
        return;
    };
    // SAFETY: standard buttons and their retained parent views belong to this
    // live window; AppKit is accessed only from eframe's main-thread callback.
    let Some(parent) = (unsafe { close.superview() }) else {
        return;
    };
    let Some(container) = (unsafe { parent.superview() }) else {
        return;
    };
    let height = if linked {
        60.0 * f64::from(ctx.zoom_factor())
    } else {
        28.0
    };
    let mut rect = container.frame();
    rect.size.height = height;
    rect.origin.y = window.frame().size.height - height;
    if container.frame() != rect {
        container.setFrame(rect);
    }
    let mut parent_rect = parent.frame();
    parent_rect.origin.y = 0.0;
    parent_rect.size.height = height;
    if parent.frame() != parent_rect {
        parent.setFrame(parent_rect);
    }
    for (index, kind) in [
        NSWindowButton::CloseButton,
        NSWindowButton::MiniaturizeButton,
        NSWindowButton::ZoomButton,
    ]
    .into_iter()
    .enumerate()
    {
        if let Some(button) = window.standardWindowButton(kind) {
            let mut origin = button.frame().origin;
            origin.x = 16.0 + index as f64 * 20.0;
            // Convert from the content top to the button parent's coordinates.
            origin.y = parent.frame().size.height - (height + button.frame().size.height) / 2.0;
            if button.frame().origin != origin {
                button.setFrameOrigin(origin);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_uses_the_apps_close_quit_and_edit_paths() {
        assert!(matches!(action("close", false), Some(Action::CloseWindow)));
        assert!(matches!(action("quit", true), Some(Action::Quit)));
        assert!(matches!(action("show", true), Some(Action::ShowWindow)));
        assert!(matches!(edit_event("copy"), Some(egui::Event::Copy)));
        assert!(
            matches!(edit_event("redo"), Some(egui::Event::Key { key: egui::Key::Z, modifiers, .. }) if modifiers.command && modifiers.shift)
        );
        for shortcut in [
            "Super+KeyN",
            "Super+Comma",
            "Super+Equal",
            "Super+Digit0",
            "Super+Shift+KeyZ",
        ] {
            shortcut
                .parse::<tray_icon::menu::accelerator::Accelerator>()
                .unwrap();
        }
    }
}
