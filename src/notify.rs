//! Desktop notifications when the app is hidden, unfocused, or on another chat.
//!
//! Delivery uses the platform notification service. Each notification runs on
//! its own thread because delivery and click handling can block.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[cfg(target_os = "windows")]
mod windows;

#[cfg(any(target_os = "macos", test))]
const MACOS_APPLICATION_ID: &str = "me.paolino.fastsapp";

#[cfg(target_os = "macos")]
fn macos_application_ready() -> bool {
    static READY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *READY.get_or_init(|| {
        // The library's implicit default looks up an app named "use_default"
        // through AppleScript, which opens macOS's application chooser.
        match notify_rust::set_application(MACOS_APPLICATION_ID) {
            Ok(()) => true,
            Err(error) => {
                log::debug!("could not initialize notification application: {error}");
                false
            }
        }
    })
}

/// Cancellation is registered before delivery starts, so reading a chat while
/// its notification is still being delivered cannot leave a stale notification.
#[derive(Default)]
pub struct Notifications {
    /// Pending deliveries per chat, each tagged with its message id so one
    /// deleted message cancels only its own notification.
    pending: std::collections::HashMap<String, Vec<(String, tokio::sync::oneshot::Sender<()>)>>,
}

/// Identifies which chat/message a notification opens when clicked.
/// Grouping these keeps `show`/`deliver` under Clippy's argument limit.
#[derive(Clone, Debug)]
pub struct NotificationTarget {
    pub chat: String,
    pub message: String,
    pub opened: Arc<Mutex<Vec<(String, String)>>>,
}

impl NotificationTarget {
    pub fn new(chat: String, message: String, opened: Arc<Mutex<Vec<(String, String)>>>) -> Self {
        Self {
            chat,
            message,
            opened,
        }
    }
}

impl Notifications {
    fn register(&mut self, chat: &str, message: &str) -> tokio::sync::oneshot::Receiver<()> {
        self.pending.retain(|_, entries| {
            entries.retain(|(_, entry)| !entry.is_closed());
            !entries.is_empty()
        });
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        self.pending
            .entry(chat.to_owned())
            .or_default()
            .push((message.to_owned(), cancel));
        cancelled
    }

    pub fn clear(&mut self, chat: &str) {
        if let Some(entries) = self.pending.remove(chat) {
            for (_, cancel) in entries {
                let _ = cancel.send(());
            }
        }
    }

    /// Cancels the pending notification of one deleted message, if any.
    /// Delivered OS notifications cannot be retracted; this only stops
    /// one that has not gone out yet.
    pub fn clear_message(&mut self, chat: &str, message: &str) {
        if let Some(entries) = self.pending.get_mut(chat) {
            // Dropping the sender resolves the delivery wait, which closes
            // an already shown notification or stops a pending one.
            entries.retain(|(id, _)| id != message);
            if entries.is_empty() {
                self.pending.remove(chat);
            }
        }
    }

    pub fn clear_all(&mut self) {
        self.pending.clear();
    }

    /// Shows a notification; platform delivery runs outside the interface thread.
    pub fn show(
        &mut self,
        title: String,
        body: String,
        picture: Option<PathBuf>,
        target: NotificationTarget,
        wake: impl Fn() + Send + 'static,
    ) {
        let cancelled = self.register(&target.chat, &target.message);
        let spawned = std::thread::Builder::new()
            .name("notification".into())
            .spawn(move || deliver(&title, &body, picture.as_deref(), target, wake, cancelled));
        if let Err(error) = spawned {
            log::debug!("no thread for a notification: {error}");
        }
    }
}

/// Builds the notification title and body, including the group sender.
pub fn lines(chat_name: &str, is_group: bool, sender: &str, summary: &str) -> (String, String) {
    let body = if is_group {
        format!("{sender}: {summary}")
    } else {
        summary.to_owned()
    };
    (chat_name.to_owned(), body)
}

#[cfg(target_os = "linux")]
fn deliver(
    title: &str,
    body: &str,
    picture: Option<&std::path::Path>,
    target: NotificationTarget,
    wake: impl Fn() + Send + 'static,
    mut cancelled: tokio::sync::oneshot::Receiver<()>,
) {
    let NotificationTarget {
        chat,
        message,
        opened,
    } = target;
    if !matches!(
        cancelled.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ) {
        return;
    }
    let mut notification = notify_rust::Notification::new();
    notification
        .appname("Vespera")
        .summary(title)
        .body(body)
        .icon("zapfast")
        .action("default", "Open");
    if let Some(picture) = picture {
        notification.image_path(&picture.to_string_lossy());
    }
    match notification.show() {
        Ok(handle) => {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    handle.close();
                    log::debug!("no notification action runtime: {error}");
                    return;
                }
            };
            runtime.block_on(async {
                tokio::select! {
                    biased;
                    _ = &mut cancelled => handle.close_async().await,
                    _ = handle.wait_for_action_async(|action| {
                        if matches!(action, notify_rust::NotificationResponse::Default) {
                            opened
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .push((chat, message));
                            wake();
                        }
                    }) => {}
                }
            });
        }
        Err(error) => log::debug!("no notification: {error}"),
    }
}

#[cfg(target_os = "windows")]
fn deliver(
    title: &str,
    body: &str,
    picture: Option<&std::path::Path>,
    target: NotificationTarget,
    wake: impl Fn() + Send + 'static,
    mut cancelled: tokio::sync::oneshot::Receiver<()>,
) {
    let NotificationTarget {
        chat,
        message,
        opened,
    } = target;
    if !matches!(
        cancelled.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ) {
        return;
    }
    let activated = move || {
        opened
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((chat.clone(), message.clone()));
        wake();
    };
    if let Err(error) = windows::show(title, body, picture, activated) {
        log::debug!("no Windows notification: {error}");
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn deliver(
    title: &str,
    body: &str,
    picture: Option<&std::path::Path>,
    _target: NotificationTarget,
    _wake: impl Fn() + Send + 'static,
    mut cancelled: tokio::sync::oneshot::Receiver<()>,
) {
    // Never fall back to application discovery, including for unbundled builds.
    #[cfg(target_os = "macos")]
    if !macos_application_ready() {
        return;
    }
    if !matches!(
        cancelled.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ) {
        return;
    }
    let mut notification = notify_rust::Notification::new();
    notification.appname("Vespera").summary(title).body(body);
    // Windows uses the image; macOS always uses the app icon.
    if let Some(picture) = picture {
        notification.image_path(&picture.to_string_lossy());
    }
    if let Err(error) = notification.show() {
        log::debug!("no notification: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_notification_identity_matches_the_packaged_application() {
        let plist = include_str!("../packaging/macos/Info.plist");
        assert!(plist.contains(&format!(
            "<key>CFBundleIdentifier</key><string>{MACOS_APPLICATION_ID}</string>"
        )));
    }

    #[test]
    fn reading_cancels_delivered_and_pending_notifications_for_only_that_chat() {
        let mut notifications = Notifications::default();
        let mut first = notifications.register("a", "m1");
        let mut second = notifications.register("a", "m2");
        let mut other = notifications.register("b", "m3");
        notifications.clear("a");
        assert_eq!(first.try_recv(), Ok(()));
        assert_eq!(second.try_recv(), Ok(()));
        assert_eq!(
            other.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        );
        let mut next = notifications.register("a", "m4");
        assert_eq!(
            next.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        );
        notifications.clear_all();
        assert!(other.try_recv().is_err());
        assert_eq!(
            next.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        );
    }

    #[test]
    fn expired_notifications_do_not_accumulate() {
        let mut notifications = Notifications::default();
        drop(notifications.register("a", "m1"));
        let _next = notifications.register("b", "m2");
        assert!(!notifications.pending.contains_key("a"));
    }

    #[test]
    fn deleting_one_message_cancels_only_its_notification() {
        let mut notifications = Notifications::default();
        let mut first = notifications.register("a", "m1");
        let mut second = notifications.register("a", "m2");
        notifications.clear_message("a", "m1");
        assert_eq!(
            first.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        );
        assert_eq!(
            second.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        );
        notifications.clear_message("missing", "m1");
        notifications.clear_message("a", "missing");
    }

    /// Shows a test notification with an optional cached picture:
    /// `cargo test --all-features shows_one -- --ignored --nocapture`.
    #[test]
    #[ignore = "shows a real notification"]
    fn shows_one_on_this_desktop() {
        let picture = std::fs::read_dir(crate::paths::AppDirs::discover().avatar_cache_dir())
            .ok()
            .and_then(|entries| entries.flatten().map(|entry| entry.path()).next());
        let mut notifications = Notifications::default();
        notifications.show(
            "Ada Lovelace".into(),
            "A test from Vespera, with a picture".into(),
            picture,
            NotificationTarget::new("test".into(), "test-message".into(), Default::default()),
            || {},
        );
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    #[test]
    fn notification_target_keeps_chat_and_message_together() {
        let opened: Arc<Mutex<Vec<(String, String)>>> = Default::default();
        let target = NotificationTarget::new("chat-1".into(), "msg-1".into(), Arc::clone(&opened));
        assert_eq!(target.chat, "chat-1");
        assert_eq!(target.message, "msg-1");
        target
            .opened
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((target.chat.clone(), target.message.clone()));
        assert_eq!(
            *opened.lock().unwrap_or_else(|p| p.into_inner()),
            vec![("chat-1".to_owned(), "msg-1".to_owned())]
        );
    }

    #[test]
    fn notification_lines_cover_groups_directs_and_empty_summaries() {
        assert_eq!(
            lines("Rust Berlin", true, "Mira", "Save me a seat"),
            ("Rust Berlin".to_owned(), "Mira: Save me a seat".to_owned())
        );
        assert_eq!(
            lines("Ada Lovelace", false, "Ada Lovelace", "Photo"),
            ("Ada Lovelace".to_owned(), "Photo".to_owned())
        );
        // Empty summary still yields a usable title/body pair.
        assert_eq!(
            lines("Chat", false, "Someone", ""),
            ("Chat".to_owned(), String::new())
        );
        assert_eq!(
            lines("Group", true, "", "hi"),
            ("Group".to_owned(), ": hi".to_owned())
        );
    }

    #[test]
    fn a_group_names_the_sender_and_a_chat_does_not() {
        assert_eq!(
            lines("Rust Berlin", true, "Mira", "Save me a seat"),
            ("Rust Berlin".to_owned(), "Mira: Save me a seat".to_owned())
        );
        assert_eq!(
            lines("Ada Lovelace", false, "Ada Lovelace", "Photo"),
            ("Ada Lovelace".to_owned(), "Photo".to_owned())
        );
    }
}
