//! Application state and the frame loop.
//!
//! Views queue [`Action`]s while drawing. The app applies them after the frame
//! and processes backend events.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::audio::{Player, Recorder};
use crate::backend::{Backend, Command, Event, LinkStatus, Waker};
use crate::model::{
    Action, Chat, ChatId, Contact, Content, Delivery, Dialog, Media, MediaState, Message, Page,
    PickerTab, StickerPack, Toast, ToastKind, VideoScrub, Viewer, ViewerItem, ViewerKind,
};
use crate::paths::AppDirs;
use crate::settings::{Settings, ThemeChoice};
use crate::single_instance::{ControlCommand, Guard};
use crate::theme::{self, Palette};
use crate::tray::{TrayCommand, TrayService};

/// Initial and incremental message-page size.
pub const PAGE: usize = 60;
/// Minimum delay between phone history requests.
const PHONE_COOLDOWN: Duration = Duration::from_secs(6);
/// WhatsApp message-edit window.
pub const EDIT_WINDOW: Duration = Duration::from_secs(15 * 60);
/// WhatsApp revoke-for-everyone window.
pub const REVOKE_WINDOW: Duration = Duration::from_secs(2 * 24 * 60 * 60);

/// Pause after which a trackpad gesture selects a new axis.
const SCROLL_GESTURE_GAP: Duration = Duration::from_millis(150);
/// Linux trackpad scroll multiplier.
const TRACKPAD_SCALE: f32 = 1.8;
/// Trackpad glide decay, minimum start speed, and stop speed.
const GLIDE_DECAY: f32 = 0.35;
const GLIDE_START: f32 = 120.0;
const GLIDE_STOP: f32 = 40.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrollAxis {
    Horizontal,
    Vertical,
}
/// Delay after the last keystroke before clearing typing state.
const COMPOSING_TIMEOUT: Duration = Duration::from_secs(4);
/// Typing-state timeout when no stop event arrives.
const TYPING_TIMEOUT: Duration = Duration::from_secs(12);
/// Pause after the last keystroke before the in-chat search runs.
const CHAT_SEARCH_PAUSE: Duration = Duration::from_millis(250);
/// Extra width a PDF page may be short of before it is rendered again.
const PDF_SHARP_ENOUGH: u32 = 200;

/// Uploaded scrub preview picture: the file and drag generation it was
/// decoded for, the fraction it shows, and its texture slot.
pub(crate) struct PreviewSlot {
    pub(crate) path: PathBuf,
    pub(crate) generation: u64,
    pub(crate) seq: u64,
    pub(crate) fraction: f32,
    pub(crate) pts: Duration,
    pub(crate) approximate: bool,
    pub(crate) texture: egui::TextureHandle,
}

/// Loaded chat history and paging state.
#[derive(Default)]
pub struct Conversation {
    pub messages: Vec<Message>,
    /// Every resident id, so merges dedupe without rebuilding a set from
    /// the whole history on each page, and removals drop one key.
    pub(crate) ids: HashSet<String>,
    /// Last merge or open: eviction drops the stalest inactive chats first.
    pub(crate) touched: Option<Instant>,
    /// Whether the local archive has no earlier messages.
    pub complete: bool,
    pub loading_older: bool,
    /// Whether the initial page was requested.
    pub requested: bool,
    /// When the initial page was last asked for. A page that never answers
    /// is asked again instead of leaving an opened chat blank.
    pub requested_at: Option<Instant>,
    /// Whether a phone history request is active.
    pub fetching_phone: bool,
    /// Whether phone history is exhausted or unavailable.
    pub phone_exhausted: bool,
    /// Last phone response time for request throttling.
    pub phone_answered: Option<Instant>,
    /// Consecutive empty phone responses used for backoff.
    pub phone_misses: u32,
    /// Whether messages arrived after the latest phone request.
    pub phone_delivered: bool,
}

impl Conversation {
    fn merge(&mut self, incoming: Vec<Message>, older: bool) {
        self.touched = Some(Instant::now());
        if older {
            self.merge_older_page(incoming);
        } else {
            for message in incoming {
                self.upsert_live(message);
            }
        }
    }

    /// Merges one older page (ascending, as the archive returns) in front
    /// without a full resort: linear in both runs, page-first on timestamp
    /// ties because older pages carry smaller rowids.
    fn merge_older_page(&mut self, page: Vec<Message>) {
        let fresh: Vec<Message> = page
            .into_iter()
            .filter(|message| !self.ids.contains(&message.id))
            .collect();
        if fresh.is_empty() {
            return;
        }
        for message in &fresh {
            self.ids.insert(message.id.clone());
        }
        let mut merged = Vec::with_capacity(self.messages.len() + fresh.len());
        let mut old = std::mem::take(&mut self.messages).into_iter().peekable();
        let mut new = fresh.into_iter().peekable();
        loop {
            match (old.peek(), new.peek()) {
                (Some(o), Some(n)) if n.timestamp < o.timestamp => {
                    merged.push(new.next().expect("peeked"));
                }
                (Some(o), Some(n)) if n.timestamp == o.timestamp => {
                    merged.push(new.next().expect("peeked"));
                }
                (Some(_), Some(_)) => {
                    merged.push(old.next().expect("peeked"));
                }
                (Some(_), None) => {
                    merged.push(old.next().expect("peeked"));
                }
                (None, Some(_)) => {
                    merged.push(new.next().expect("peeked"));
                }
                (None, None) => break,
            }
        }
        self.messages = merged;
    }

    /// Upserts one live message: replacement in place, or binary search by
    /// timestamp for a new id. No full resort either way.
    fn upsert_live(&mut self, message: Message) {
        if self.ids.contains(&message.id)
            && let Some(position) = self.messages.iter().position(|m| m.id == message.id)
        {
            if self.messages[position].timestamp == message.timestamp {
                self.messages[position] = message;
                return;
            }
            self.messages.remove(position);
        }
        // Upper bound: a live arrival is the newest of its second.
        let position = self
            .messages
            .partition_point(|m| m.timestamp <= message.timestamp);
        self.ids.insert(message.id.clone());
        self.messages.insert(position, message);
    }

    /// Forgets one resident message from every structure at once.
    fn forget(&mut self, id: &str) {
        self.ids.remove(id);
        self.messages.retain(|message| message.id != id);
    }

    /// Keeps only the newest `keep` messages, pruning the id set with
    /// them. Older history still pages from the archive: callers reset
    /// the reload flags so reopening refetches what left the RAM.
    fn trim_older(&mut self, keep: usize) {
        if self.messages.len() > keep {
            let drop = self.messages.len() - keep;
            for message in self.messages.drain(..drop) {
                self.ids.remove(&message.id);
            }
        }
    }

    /// Forgets every message at or below `through`, returning the removed
    /// ids for composer and selection cleanup.
    fn forget_range(&mut self, through: i64) -> std::collections::HashSet<String> {
        let mut removed = std::collections::HashSet::new();
        self.messages.retain(|message| {
            let gone = message.timestamp <= through;
            if gone {
                removed.insert(message.id.clone());
            }
            !gone
        });
        for id in &removed {
            self.ids.remove(id);
        }
        removed
    }

    pub fn message_mut(&mut self, id: &str) -> Option<&mut Message> {
        self.messages.iter_mut().find(|message| message.id == id)
    }

    pub fn message(&self, id: &str) -> Option<&Message> {
        self.messages.iter().find(|message| message.id == id)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Presence {
    pub online: bool,
    pub last_seen: Option<i64>,
}

pub struct App {
    pub dirs: AppDirs,
    pub settings: Settings,
    settings_dirty: bool,
    last_settings_save: Instant,
    pub backend: Backend,
    pub palette: Palette,
    pub custom_themes: theme::custom::Catalog,
    applied_dark: Option<bool>,
    zoom_applied: bool,

    pub link: LinkStatus,
    /// Whether link-time history sync is active.
    pub syncing: bool,
    pub sync_percent: Option<u32>,
    pub me: Option<String>,
    pub me_name: Option<String>,
    /// Account about text.
    pub me_about: Option<String>,

    /// Chats ordered by latest activity.
    pub chats: Vec<Chat>,
    pub contacts: HashMap<String, Contact>,
    pub conversations: HashMap<ChatId, Conversation>,
    pub open_chat: Option<ChatId>,
    /// Chat row to reveal after keyboard navigation.
    pub scroll_chat_into_view: Option<ChatId>,
    /// Composer drafts by chat.
    pub drafts: HashMap<ChatId, String>,
    draft_mentions: HashMap<ChatId, Vec<ComposerMention>>,
    pub composer: String,
    composer_mentions: Vec<ComposerMention>,
    /// Byte offset of the `:` starting the active emoji query.
    pub emoji_start: Option<usize>,
    /// Keyboard-highlighted emoji in suggestions or the full picker.
    pub emoji_selected: usize,
    /// Byte offset of the `@` starting the active mention query.
    pub mention_start: Option<usize>,
    /// Keyboard-highlighted member in the mention suggestions.
    pub mention_selected: usize,
    /// Reply target in the open chat.
    pub reply_to: Option<String>,
    /// Multi-selected message ids in the open chat. Non-empty means the
    /// selection bar is showing instead of the plain composer row.
    pub selected: Vec<String>,
    /// Message from which the next Shift-click range begins.
    selection_anchor: Option<String>,
    /// Outgoing message being edited.
    pub editing: Option<String>,
    composing: bool,
    last_keystroke: Option<Instant>,
    pub search: String,
    /// Message search results, newest first.
    pub search_hits: Vec<Message>,
    /// Active typers and their latest event time by chat.
    pub typing: HashMap<ChatId, Vec<(String, Instant)>>,
    pub presence: HashMap<String, Presence>,
    /// Whether account privacy disables direct-chat read receipts.
    pub account_receipts_off: bool,
    avatars: HashMap<String, Option<PathBuf>>,
    avatar_requests: HashSet<String>,
    /// Full-size profile pictures for info dialogs.
    avatars_full: HashMap<String, Option<PathBuf>>,
    avatar_full_requests: HashSet<String>,
    /// Whether files are being dragged over the window.
    pub dropping: bool,
    /// Paste-gesture state, linked by event order and origin, never by image
    /// contents or a time window. paste_armed: an image Paste was handled
    /// and its key release is still to come; that release ends the gesture
    /// quietly, armed optimistically inside the same event fold so a
    /// release sharing the Paste frame still sees it. paste_typed_v: a
    /// plain V press (no command) was seen, so a later bare release is
    /// typing, not a paste. The integration consumes every Ctrl+V press,
    /// with or without clipboard text, so no press event ever opens an
    /// image-only gesture; only the release is delivered. paste_ctrl_held:
    /// Ctrl latch folded from the ordered modifier/key events; a keyboard
    /// Paste arms exactly when Ctrl was down for it, while a menu Paste
    /// never does.
    paste_armed: bool,
    paste_typed_v: bool,
    paste_ctrl_held: bool,
    /// Open emoji, GIF, or sticker picker tab.
    pub picker: Option<PickerTab>,
    /// Full-window viewer over the open chat's pictures and stickers.
    pub viewer: Option<Viewer>,
    /// Whether the search bar inside the open chat is showing.
    pub chat_search_open: bool,
    /// The PDF page the worker last rendered, waiting to be uploaded.
    pub pdf_page: Option<crate::pdf::Page>,
    /// Small previews of the open PDF's pages, oldest first.
    pub pdf_thumbs: Option<(PathBuf, Vec<PathBuf>)>,
    /// Path, page and width of the uploaded PDF texture.
    pub pdf_texture: Option<(PathBuf, usize, u32, egui::TextureHandle)>,
    /// What the worker is rendering right now.
    pub pdf_rendering: Option<(PathBuf, usize, u32)>,
    /// Why the last render failed.
    pub pdf_error: Option<String>,
    /// Width of the viewer window, reported by the view for PDF rendering.
    pub viewer_view_width: f32,
    /// Text typed into the in-chat search bar.
    pub chat_search: String,
    /// Hits of the last answered in-chat search, newest first.
    pub chat_search_hits: Vec<Message>,
    /// Query the last batch of hits answers.
    pub chat_search_query: String,
    /// When the in-chat search text last changed, for the debounce.
    pub chat_search_at: Option<Instant>,
    /// Whether the in-chat search field should take focus.
    pub chat_search_focus: bool,
    /// Picker anchor at the composer button.
    pub picker_anchor: Option<egui::Rect>,
    pub picker_search: String,
    /// Sticker picker search text.
    pub sticker_search: String,
    /// Whether the newly opened picker should focus search.
    pub picker_focus: bool,
    /// Attachments pending in the composer.
    pub pending: Vec<Pending>,
    /// In-chat audio player.
    pub player: Player,
    /// Plays the video open in the viewer. Only one plays at a time.
    pub video: crate::video::Player,
    /// Scrub drag in progress over the open video, if any.
    pub video_scrub: Option<VideoScrub>,
    /// Background scrub-preview decoder shared by all drags.
    pub previewer: crate::video::Previewer,
    /// Uploaded preview picture: file, drag generation and texture.
    pub(crate) video_preview: Option<PreviewSlot>,
    /// Active voice recorder.
    pub recording: Option<Recorder>,
    /// Voice messages with a sent played receipt.
    played_told: HashSet<String>,
    /// Message bodies registered for transcript copy formatting.
    pub copy_rows: std::sync::Arc<std::sync::Mutex<Vec<crate::transcript::Row>>>,
    /// Previous message-list rect used by the selection hook.
    pub selection_view: std::sync::Arc<std::sync::Mutex<Option<egui::Rect>>>,
    pub stickers: Vec<PathBuf>,
    /// Saved stickers, newest first.
    pub stickers_saved: Vec<PathBuf>,
    /// Sticker files marked as favourites, newest first.
    pub stickers_favorites: Vec<PathBuf>,
    /// Imported sticker packs, newest first.
    pub sticker_packs: Vec<StickerPack>,
    /// Emoji tags by sticker file, for picker search.
    pub stickers_emojis: HashMap<PathBuf, Vec<String>>,
    /// Whether the sticker list is loading.
    pub stickers_pending: bool,
    /// Undrained thumbnail rebuild results from the worker: the sticker
    /// path and whether its thumbnail is ready. The picker applies them.
    pub sticker_thumb_results: Vec<(PathBuf, bool)>,
    /// Whether a sticker pack import is active.
    pub sticker_import_pending: bool,
    scroll_lock: Option<(ScrollAxis, Instant)>,
    scroll_from_trackpad: bool,
    scroll_history: egui::util::History<egui::Vec2>,
    scroll_accum: egui::Vec2,
    glide: Option<egui::Vec2>,
    scroll_last_event: Option<Instant>,

    pub page: Page,
    pub dialog: Option<Dialog>,
    /// Chat filter in the forwarding destination dialog.
    pub forward_search: String,
    /// Chats ticked in the forwarding destination dialog.
    pub forward_to: Vec<ChatId>,
    pub poll_draft: crate::model::PollDraft,
    pub poll_creating: bool,
    pub poll_voting: HashSet<(ChatId, String)>,
    /// Contact-name editor buffers.
    pub contact_edit: Option<(String, String)>,
    /// New-contact buffers and lookup state.
    pub new_contact_phone: String,
    pub new_contact_name: String,
    pub new_contact_last: String,
    pub new_contact_pending: bool,
    /// Phone number entered for pairing.
    pub pair_phone: String,
    pub sidebar_visible: bool,
    pub show_archived: bool,
    /// Show channels and community/announcement containers instead of normal chats.
    pub show_channels: bool,
    pub toasts: Vec<Toast>,
    pub actions: Vec<Action>,
    /// A newer release than this build, once GitHub has said so.
    pub update: Option<crate::updates::Release>,
    last_update_check: Option<Instant>,
    /// A manual update check is waiting for the worker's answer.
    pub update_checking: bool,
    pub show_update: bool,
    pub update_download: crate::updates::DownloadState,
    pub update_support: Option<Result<crate::updates::install::Installation, String>>,
    update_inspecting: bool,
    pub update_arguments: Vec<String>,
    /// Whether to scroll the conversation to its newest message.
    pub scroll_to_bottom: bool,
    /// Whether the conversation was at the bottom last frame.
    pub at_bottom: bool,
    /// Message id to scroll into view.
    pub scroll_anchor: Option<String>,
    /// Last measured message-body height per chat, without the top fill.
    /// Used to pin short conversations to the composer.
    pub chat_body_height: HashMap<ChatId, f32>,
    pub focus_composer: bool,
    pub focus_search: bool,
    pub quit_requested: bool,
    pub window_focused: bool,
    /// Cross-thread window repaint handle.
    waker: Waker,
    tray: Option<TrayService>,
    /// Whether the app is running without a window.
    pub window_hidden: bool,
    /// Whether window close should keep the process running.
    pub hide_intent: bool,
    /// Whether a headless app should create a window.
    pub wants_show: bool,
    /// Deadline and next attempt for bringing the window to the front.
    #[cfg(target_os = "windows")]
    raise_deadline: Option<Instant>,
    #[cfg(target_os = "windows")]
    raise_next: Instant,
    /// Requests received from later launches.
    control_commands: Option<std::sync::Arc<std::sync::Mutex<Vec<ControlCommand>>>>,
    /// Chat and message ids from clicked notifications.
    notification_opens: std::sync::Arc<std::sync::Mutex<Vec<(ChatId, String)>>>,
    notifications: crate::notify::Notifications,
}

/// Attachment pending in the composer.
pub enum Pending {
    /// Clipboard image as straight-alpha RGBA and optional preview.
    Picture {
        width: usize,
        height: usize,
        rgba: std::sync::Arc<Vec<u8>>,
        texture: Option<egui::TextureHandle>,
    },
    File(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ComposerMention {
    id: String,
    name: String,
}

impl Pending {
    /// Whether the composer can preview the file as an image.
    pub fn is_picture_file(path: &std::path::Path) -> bool {
        mime_guess2::from_path(path)
            .first()
            .is_some_and(|mime| mime.type_() == "image")
    }
}

/// Process-level app services.
#[derive(Clone, Copy, Debug)]
pub struct AppOptions {
    /// Registers the system-tray item.
    pub tray: bool,
}

impl Default for AppOptions {
    fn default() -> Self {
        Self { tray: true }
    }
}

impl App {
    pub fn new(waker: &Waker, dirs: AppDirs, settings: Settings, options: AppOptions) -> Self {
        let backend = Backend::spawn(dirs.clone(), waker.clone());
        let mut app = Self::with_backend(dirs, settings, backend, waker.clone());
        app.custom_themes.enable_desktop_themes();
        app.load_custom_themes();
        if options.tray {
            let waker = waker.clone();
            app.tray = TrayService::spawn(move || waker.wake());
        }
        app
    }

    /// Single-instance guard used by later launches.
    pub fn set_remote_control(&mut self, guard: &Guard) {
        self.control_commands = Some(guard.commands());
    }

    /// Creates a disconnected app and event sender for demos and tests.
    pub fn headless(dirs: AppDirs, settings: Settings) -> (Self, std::sync::mpsc::Sender<Event>) {
        let (backend, events) = Backend::detached();
        (
            Self::with_backend(dirs, settings, backend, Waker::default()),
            events,
        )
    }

    fn with_backend(dirs: AppDirs, settings: Settings, backend: Backend, waker: Waker) -> Self {
        let palette = settings
            .cached_palette()
            .unwrap_or_else(|| match settings.theme {
                ThemeChoice::Light => Palette::light(),
                _ => Palette::dark(),
            });
        let open_chat = settings.last_chat.clone();
        Self {
            dirs,
            settings,
            settings_dirty: false,
            last_settings_save: Instant::now(),
            backend,
            palette,
            custom_themes: theme::custom::Catalog::default(),
            applied_dark: None,
            zoom_applied: false,
            link: LinkStatus::Starting,
            syncing: false,
            sync_percent: None,
            me: None,
            me_name: None,
            me_about: None,
            chats: Vec::new(),
            contacts: HashMap::new(),
            conversations: HashMap::new(),
            open_chat,
            scroll_chat_into_view: None,
            drafts: HashMap::new(),
            draft_mentions: HashMap::new(),
            composer: String::new(),
            composer_mentions: Vec::new(),
            emoji_start: None,
            emoji_selected: 0,
            mention_start: None,
            mention_selected: 0,
            reply_to: None,
            selected: Vec::new(),
            selection_anchor: None,
            editing: None,
            composing: false,
            last_keystroke: None,
            search: String::new(),
            search_hits: Vec::new(),
            typing: HashMap::new(),
            presence: HashMap::new(),
            account_receipts_off: false,
            avatars: HashMap::new(),
            avatar_requests: HashSet::new(),
            avatars_full: HashMap::new(),
            avatar_full_requests: HashSet::new(),
            dropping: false,
            paste_armed: false,
            paste_typed_v: false,
            paste_ctrl_held: false,
            picker: None,
            viewer: None,
            chat_search_open: false,
            pdf_page: None,
            pdf_texture: None,
            pdf_rendering: None,
            pdf_thumbs: None,
            pdf_error: None,
            viewer_view_width: 0.0,
            chat_search: String::new(),
            chat_search_hits: Vec::new(),
            chat_search_query: String::new(),
            chat_search_at: None,
            chat_search_focus: false,
            picker_anchor: None,
            picker_search: String::new(),
            sticker_search: String::new(),
            picker_focus: false,
            pending: Vec::new(),
            player: Player::new(waker.clone()),
            video: crate::video::Player::default(),
            video_scrub: None,
            previewer: crate::video::Previewer::default(),
            video_preview: None,
            recording: None,
            played_told: HashSet::new(),
            copy_rows: Default::default(),
            selection_view: Default::default(),
            stickers: Vec::new(),
            stickers_saved: Vec::new(),
            stickers_favorites: Vec::new(),
            sticker_packs: Vec::new(),
            stickers_emojis: HashMap::new(),
            stickers_pending: false,
            sticker_thumb_results: Vec::new(),
            sticker_import_pending: false,
            scroll_lock: None,
            scroll_from_trackpad: false,
            scroll_history: egui::util::History::new(2..16, 0.1),
            scroll_accum: egui::Vec2::ZERO,
            glide: None,
            scroll_last_event: None,
            page: Page::Chats,
            dialog: None,
            forward_search: String::new(),
            forward_to: Vec::new(),
            poll_draft: Default::default(),
            poll_creating: false,
            poll_voting: HashSet::new(),
            contact_edit: None,
            new_contact_phone: String::new(),
            new_contact_name: String::new(),
            new_contact_last: String::new(),
            new_contact_pending: false,
            pair_phone: String::new(),
            sidebar_visible: true,
            show_archived: false,
            show_channels: false,
            toasts: Vec::new(),
            actions: Vec::new(),
            update: None,
            last_update_check: None,
            update_checking: false,
            show_update: false,
            update_download: Default::default(),
            update_support: None,
            update_inspecting: false,
            update_arguments: Vec::new(),
            scroll_to_bottom: true,
            at_bottom: true,
            scroll_anchor: None,
            chat_body_height: HashMap::new(),
            focus_composer: false,
            focus_search: false,
            quit_requested: false,
            window_focused: false,
            waker,
            tray: None,
            window_hidden: false,
            hide_intent: false,
            wants_show: false,
            #[cfg(target_os = "windows")]
            raise_deadline: None,
            #[cfg(target_os = "windows")]
            raise_next: Instant::now(),
            control_commands: None,
            notification_opens: Default::default(),
            notifications: Default::default(),
        }
    }

    /// Asks for the window to reach the front, and keeps asking while it
    /// comes up: Windows refuses the first request while another program
    /// holds the foreground, and a fresh window does not exist yet.
    fn request_raise(&mut self) {
        #[cfg(target_os = "windows")]
        {
            let now = Instant::now();
            self.raise_deadline = Some(now + crate::winfocus::PATIENCE);
            self.raise_next = now;
        }
    }

    /// Repeats the request while the window is coming up.
    fn pump_raise(&mut self) {
        #[cfg(target_os = "windows")]
        {
            let Some(deadline) = self.raise_deadline else {
                return;
            };
            let now = Instant::now();
            if now >= deadline {
                self.raise_deadline = None;
                return;
            }
            if now >= self.raise_next {
                self.raise_next = now + crate::winfocus::RETRY;
                crate::winfocus::raise();
            }
            self.waker.wake_after(crate::winfocus::RETRY);
        }
    }

    /// Updates the linked app while no window exists.
    pub fn window_gone(&mut self) {
        self.window_hidden = true;
        self.window_focused = false;
        self.hide_intent = false;
        self.wants_show = false;
        if let Some(tray) = &mut self.tray {
            tray.hidden();
        }
    }

    /// Whether window close keeps the app in the tray.
    pub fn hides_to_tray(&self) -> bool {
        self.tray.is_some() && self.settings.keep_running_in_background
    }

    fn handle_tray(&mut self) {
        let Some(commands) = self.tray.as_ref().map(TrayService::drain_commands) else {
            return;
        };
        for command in commands {
            match command {
                TrayCommand::Show => self.actions.push(Action::ShowWindow),
                TrayCommand::ShowHide => self.actions.push(if self.window_hidden {
                    Action::ShowWindow
                } else {
                    Action::HideWindow
                }),
                TrayCommand::Quit => self.actions.push(Action::Quit),
            }
        }
    }

    fn handle_control_commands(&mut self) {
        let Some(queue) = &self.control_commands else {
            return;
        };
        let commands: Vec<ControlCommand> =
            std::mem::take(&mut *queue.lock().unwrap_or_else(|p| p.into_inner()));
        for command in commands {
            match command {
                ControlCommand::Show => self.actions.push(Action::ShowWindow),
                ControlCommand::ReloadThemes => self.actions.push(Action::ReloadThemes),
            }
        }
    }

    /// Opens chats from clicked notifications, creating a window when needed.
    fn handle_notification_opens(&mut self) {
        let opened: Vec<(ChatId, String)> = std::mem::take(
            &mut *self
                .notification_opens
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
        );
        for (chat, message) in opened {
            self.actions.push(Action::OpenMessage { chat, message });
            self.actions.push(Action::ShowWindow);
        }
    }

    /// Sends a desktop notification for an unseen incoming message.
    fn maybe_notify(&mut self, chat_id: &str, message: &Message) {
        if !self.settings.notifications {
            return;
        }
        let Some(chat) = self.chat(chat_id) else {
            return;
        };
        let now = crate::util::now();
        // Skip muted chats and delayed reconnect backlogs.
        if chat.unread == 0 || chat.archived || chat.muted(now) || now - message.timestamp > 60 {
            return;
        }
        let reading = !self.window_hidden
            && self.window_focused
            && self.page == Page::Chats
            && self.open_chat.as_deref() == Some(chat_id);
        if reading {
            return;
        }
        let (name, is_group) = (self.chat_title(chat), chat.is_group());
        let sender = self.display_name_or(&message.sender, message.sender_name.as_deref());
        let (title, body) =
            crate::notify::lines(&name, is_group, &sender, &self.message_text(message));
        // Prefer the chat picture, then the sender picture. Cached files work
        // before the chat list loads; new requests help later notifications.
        let sender = message.sender.clone();
        let picture = self
            .avatar(chat_id)
            .or_else(|| self.cached_avatar(chat_id))
            .or_else(|| self.avatar(&sender))
            .or_else(|| self.cached_avatar(&sender));
        let waker = self.waker.clone();
        self.notifications.show(
            title,
            body,
            picture,
            crate::notify::NotificationTarget::new(
                chat_id.to_owned(),
                message.id.clone(),
                std::sync::Arc::clone(&self.notification_opens),
            ),
            move || waker.wake(),
        );
    }

    /// Initializes a newly created window.
    pub fn attach(&mut self, ctx: &egui::Context) {
        // Register transcript copy formatting once per egui context.
        ctx.add_plugin(crate::transcript::CopyAnnotator {
            rows: std::sync::Arc::clone(&self.copy_rows),
        });
        ctx.data_mut(|data| {
            data.insert_temp(
                egui::Id::new("copy-rows"),
                std::sync::Arc::clone(&self.copy_rows),
            );
        });
        ctx.add_plugin(crate::ui::conversation::SelectionLeash::new(
            std::sync::Arc::clone(&self.selection_view),
        ));
        crate::theme::install(ctx);
        // Use a faster wheel speed for short chat rows.
        ctx.options_mut(|options| options.input_options.line_scroll_speed = 120.0);
        // Load and index the color emoji font outside the frame loop.
        std::thread::Builder::new()
            .name("emoji-font".into())
            .spawn(crate::emoji::warm_up)
            .ok();
        self.applied_dark = None;
        self.zoom_applied = false;
        self.window_hidden = false;
        self.hide_intent = false;
        self.wants_show = false;
        self.refocus_composer(ctx);
        if let Some(tray) = &mut self.tray {
            tray.attach();
        }
        #[cfg(target_os = "macos")]
        crate::macos::attach(ctx);
    }

    pub fn is_connected(&self) -> bool {
        self.link.is_connected()
    }

    /// Whether the device has linked data, including while offline.
    pub fn is_linked(&self) -> bool {
        matches!(
            self.link,
            LinkStatus::Connected | LinkStatus::Connecting | LinkStatus::Disconnected { .. }
        ) || (!self.chats.is_empty() && !matches!(self.link, LinkStatus::LoggedOut))
    }

    pub fn chat(&self, id: &str) -> Option<&Chat> {
        self.chats.iter().find(|chat| chat.id == id)
    }

    pub fn chat_mut(&mut self, id: &str) -> Option<&mut Chat> {
        self.chats.iter_mut().find(|chat| chat.id == id)
    }

    pub fn current_chat(&self) -> Option<&Chat> {
        self.open_chat.as_deref().and_then(|id| self.chat(id))
    }

    /// Resolves an address-book, push, phone-number, or fallback name.
    pub fn display_name(&self, id: &str) -> String {
        self.display_name_or(id, None)
    }

    /// Resolves a consistent display name using settings and an optional
    /// message-provided fallback. Our own id becomes "You".
    pub fn display_name_or(&self, id: &str, hint: Option<&str>) -> String {
        if self.me.as_deref() == Some(id) {
            return "You".to_owned();
        }
        self.person_name(id, hint)
    }

    /// Resolves a mention name without replacing our own name with "You".
    pub fn mention_name(&self, id: &str) -> String {
        if self.me.as_deref() == Some(id) {
            return self
                .me_name
                .clone()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "You".to_owned());
        }
        self.person_name(id, None)
    }

    /// Resolves the chat-list title.
    pub fn chat_title(&self, chat: &Chat) -> String {
        if chat.is_group() || self.me.as_deref() == Some(chat.id.as_str()) {
            return chat.name.clone();
        }
        self.person_name(&chat.id, None)
    }

    fn person_name(&self, id: &str, hint: Option<&str>) -> String {
        let contact = self.contacts.get(id);
        let present = |name: Option<&str>| name.filter(|name| !name.is_empty()).map(str::to_owned);
        let saved = present(contact.and_then(|contact| contact.full_name.as_deref()));
        let called = present(contact.and_then(|contact| contact.push_name.as_deref()))
            .or_else(|| present(hint));
        let (first, second) = if self.settings.names_from_contacts {
            (saved, called.map(|name| format!("~{name}")))
        } else {
            (called, saved)
        };
        if let Some(name) = first.or(second) {
            return name;
        }
        if let Some(chat) = self.chat(id)
            && !chat.name.is_empty()
            && !chat.name.chars().all(|c| c.is_ascii_digit())
        {
            // Names stored before the Brazilian grouping landed keep the old
            // generic shape; normalize them on display instead of migrating
            // every archived row.
            return crate::util::display_phone_name(&chat.name);
        }
        match crate::model::phone_of(id) {
            Some(digits) => crate::util::phone(digits),
            None => "Unknown".to_owned(),
        }
    }

    /// Resolves message mentions for markup.
    pub fn mention_list(&self, message: &Message) -> Vec<crate::markup::Mention> {
        message
            .mentions
            .iter()
            .map(|mention| crate::markup::Mention {
                user: mention.user.clone(),
                name: self.mention_name(&mention.id),
                id: mention.id.clone(),
            })
            .collect()
    }

    /// Resolves `@user` tokens in previews without mention metadata.
    pub fn resolve_mention_tokens(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find('@') {
            out.push_str(&rest[..at]);
            out.push('@');
            let after = &rest[at + 1..];
            let digits = after
                .char_indices()
                .find(|(_, c)| !c.is_ascii_digit())
                .map_or(after.len(), |(index, _)| index);
            let id = format!("{}@s.whatsapp.net", &after[..digits]);
            let known = digits >= 5
                && (self.me.as_deref() == Some(id.as_str())
                    || self.contacts.contains_key(&id)
                    || self.chat(&id).is_some());
            if known {
                out.push_str(&self.mention_name(&id));
                rest = &after[digits..];
            } else {
                rest = after;
            }
        }
        out.push_str(rest);
        out
    }

    /// One-line plain-text message summary with resolved mentions.
    pub fn message_text(&self, message: &Message) -> String {
        match &message.content {
            Content::Text { text, .. } => crate::markup::plain(text, &self.mention_list(message)),
            _ => self.resolve_mention_tokens(&message.summary()),
        }
    }

    /// Whether a direct chat uses a saved address-book name.
    pub fn is_saved_contact(&self, id: &str) -> bool {
        self.contacts.get(id).is_some_and(|contact| {
            contact
                .full_name
                .as_deref()
                .is_some_and(|name| !name.is_empty())
        })
    }

    /// Group members sorted by name, then phone number, with our id last.
    pub fn participant_list(&self, chat: &Chat) -> Vec<(String, String)> {
        let me = self.me.as_deref();
        let mut named = Vec::new();
        let mut numbers = Vec::new();
        for id in chat
            .participants
            .iter()
            .filter(|id| Some(id.as_str()) != me)
        {
            let name = self.display_name(id);
            if name.starts_with('+') || name == "Unknown" {
                numbers.push((id.clone(), name));
            } else {
                named.push((id.clone(), name));
            }
        }
        named.sort_by_key(|(_, name)| name.trim_start_matches('~').to_lowercase());
        numbers.sort_by(|a, b| a.1.cmp(&b.1));
        named.extend(numbers);
        if let Some(me) = me
            && chat.participants.iter().any(|id| id == me)
        {
            named.push((me.to_owned(), "You".to_owned()));
        }
        named
    }

    /// Group members matching the active composer mention query.
    pub fn mention_candidates(&self, chat: &Chat, query: &str) -> Vec<(String, String)> {
        if !chat.is_group() {
            return Vec::new();
        }
        let needle = query.trim().to_lowercase();
        let digits: String = query.chars().filter(char::is_ascii_digit).collect();
        self.participant_list(chat)
            .into_iter()
            .filter(|(id, name)| {
                if self.me.as_deref() == Some(id) {
                    return false;
                }
                if needle.is_empty() {
                    return true;
                }
                name.trim_start_matches('~')
                    .to_lowercase()
                    .contains(&needle)
                    || (!digits.is_empty()
                        && id
                            .split('@')
                            .next()
                            .is_some_and(|user| user.contains(&digits)))
            })
            .collect()
    }

    pub fn participant_names(&self, chat: &Chat) -> String {
        let me = self.me.as_deref();
        let mut names = Vec::new();
        let mut numbers = Vec::new();
        for id in chat
            .participants
            .iter()
            .filter(|id| Some(id.as_str()) != me)
        {
            let name = self.display_name(id);
            if name.starts_with('+') || name == "Unknown" {
                numbers.push(name);
            } else {
                let name = name.trim_start_matches('~');
                names.push(name.split_whitespace().next().unwrap_or(name).to_owned());
            }
        }
        names.sort_by_key(|name| name.to_lowercase());
        names.dedup();
        numbers.sort();
        numbers.dedup();
        names.extend(numbers);
        if chat.participants.iter().any(|id| Some(id.as_str()) == me) {
            names.push("You".to_owned());
        }
        names.join(", ")
    }

    /// Visible chats filtered by search and archive state, with pinned first.
    pub fn visible_chats(&self) -> Vec<&Chat> {
        let needle = crate::util::search_key(self.search.trim());
        let mut chats: Vec<&Chat> = self
            .chats
            .iter()
            .filter(|chat| chat.archived == self.show_archived || !needle.is_empty())
            .filter(|chat| {
                !needle.is_empty() || chat.is_channel_or_community() == self.show_channels
            })
            .filter(|chat| {
                needle.is_empty()
                    || crate::util::search_key(&self.chat_title(chat)).contains(&needle)
                    || chat
                        .phone()
                        .is_some_and(|phone| crate::util::phone_matches(phone, self.search.trim()))
                    || chat.last.as_ref().is_some_and(|last| {
                        crate::util::search_key(&last.summary).contains(&needle)
                    })
            })
            .collect();
        chats.sort_by(|a, b| {
            b.pinned.cmp(&a.pinned).then_with(|| {
                if a.pinned && b.pinned {
                    b.pinned_at.cmp(&a.pinned_at).then(a.id.cmp(&b.id))
                } else {
                    b.last_activity.cmp(&a.last_activity).then(a.id.cmp(&b.id))
                }
            })
        });
        chats
    }

    /// Matching individual contacts without an existing chat, sorted by name.
    pub fn matching_contacts(&self) -> Vec<&Contact> {
        let needle = crate::util::search_key(self.search.trim());
        if needle.is_empty() {
            return Vec::new();
        }
        let mut contacts: Vec<&Contact> =
            self.contacts
                .values()
                .filter(|contact| crate::model::phone_of(&contact.id).is_some())
                .filter(|contact| self.me.as_deref() != Some(contact.id.as_str()))
                .filter(|contact| !self.chats.iter().any(|chat| chat.id == contact.id))
                .filter(|contact| {
                    contact
                        .display_name()
                        .is_some_and(|name| crate::util::search_key(name).contains(&needle))
                        || contact.id.split('@').next().is_some_and(|phone| {
                            crate::util::phone_matches(phone, self.search.trim())
                        })
                })
                .collect();
        contacts
            .sort_by_key(|contact| contact.display_name().unwrap_or(&contact.id).to_lowercase());
        contacts.truncate(15);
        contacts
    }

    /// Archived chats in the current Chats/Channels tab, so the row count
    /// matches what opening Archived will list.
    pub fn archived_count(&self) -> usize {
        self.chats
            .iter()
            .filter(|chat| chat.archived && chat.is_channel_or_community() == self.show_channels)
            .count()
    }

    pub fn unread_total(&self) -> u32 {
        self.chats
            .iter()
            .filter(|chat| !chat.archived && !chat.muted(crate::util::now()))
            .map(|chat| chat.unread)
            .sum()
    }

    /// Returns or requests a cached profile picture.
    fn cached_avatar(&self, id: &str) -> Option<PathBuf> {
        let path = self.dirs.avatar_file(id, false);
        path.metadata()
            .ok()
            .filter(|metadata| metadata.len() > 0)
            .map(|_| path)
    }

    /// Registers an existing profile picture, used by demo data.
    pub fn adopt_avatar(&mut self, id: &str, path: PathBuf) {
        self.avatars.insert(id.to_owned(), Some(path));
    }

    pub fn avatar(&mut self, id: &str) -> Option<PathBuf> {
        if let Some(known) = self.avatars.get(id) {
            return known.clone();
        }
        if self.avatar_requests.insert(id.to_owned()) {
            self.backend.send(Command::FetchAvatar {
                id: id.to_owned(),
                full: false,
            });
        }
        None
    }

    /// Returns or requests a full-size profile picture.
    pub fn avatar_full(&mut self, id: &str) -> Option<PathBuf> {
        if let Some(known) = self.avatars_full.get(id) {
            return known.clone();
        }
        if self.avatar_full_requests.insert(id.to_owned()) {
            self.backend.send(Command::FetchAvatar {
                id: id.to_owned(),
                full: true,
            });
        }
        None
    }

    /// Whether an outgoing message is still editable.
    pub fn can_edit(&self, message: &Message) -> bool {
        message.from_me
            && matches!(message.content, Content::Text { .. })
            && crate::util::now() - message.timestamp <= EDIT_WINDOW.as_secs() as i64
    }

    /// Whether an outgoing message can still be revoked for everyone.
    pub fn can_revoke(&self, message: &Message) -> bool {
        message.from_me
            && !matches!(message.content, Content::Revoked)
            && crate::util::now() - message.timestamp <= REVOKE_WINDOW.as_secs() as i64
    }

    /// Active typers in a chat as id and display name.
    pub fn typing_in(&self, chat: &str) -> Vec<(String, String)> {
        self.typing
            .get(chat)
            .map(|typers| {
                typers
                    .iter()
                    .map(|(sender, _)| (sender.clone(), self.display_name(sender)))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn handle_events(&mut self) {
        for event in self.backend.poll() {
            match event {
                Event::Link(status) => self.handle_link(status),
                Event::Me { id, name, about } => {
                    self.me = Some(id);
                    self.me_name = name;
                    self.me_about = about;
                }
                Event::Chats(chats) => {
                    let now = crate::util::now();
                    for chat in &chats {
                        if chat.unread == 0 || chat.archived || chat.muted(now) {
                            self.notifications.clear(&chat.id);
                        }
                    }
                    self.chats = chats;
                    if let Some(open) = self.open_chat.clone() {
                        if self.chat(&open).is_none() {
                            self.open_chat = None;
                        } else {
                            // Show archived messages immediately, including offline.
                            self.ensure_loaded(&open);
                        }
                    }
                }
                Event::ChatUpdated(chat) => self.handle_chat_updated(*chat),
                Event::Messages {
                    chat,
                    messages,
                    older,
                    complete,
                } => {
                    let conversation = self.conversations.entry(chat.clone()).or_default();
                    let was_empty = conversation.messages.is_empty();
                    if older && !messages.is_empty() {
                        conversation.phone_delivered = true;
                    }
                    conversation.merge(messages, older);
                    if older {
                        conversation.loading_older = false;
                        conversation.complete = complete;
                    } else if was_empty {
                        conversation.complete = complete;
                    }
                    if !older {
                        // Preloaded pages arrive without an explicit open.
                        conversation.requested = true;
                        conversation.requested_at = Some(Instant::now());
                    }
                    // Request phone history when sync created a chat without messages.
                    let bare = !older && complete && conversation.messages.is_empty();
                    if self.open_chat.as_deref() == Some(chat.as_str()) {
                        if !older && (self.at_bottom || was_empty) {
                            self.scroll_to_bottom = true;
                        }
                        if bare {
                            self.fetch_older(&chat);
                        }
                        // After the first page, load toward a pending search anchor once.
                        if !older
                            && let Some(anchor) = self.scroll_anchor.clone()
                            && let Some(conversation) = self.conversations.get_mut(&chat)
                            && conversation.message(&anchor).is_none()
                            && !conversation.loading_older
                            && let Some(oldest) = conversation.messages.first()
                        {
                            conversation.loading_older = true;
                            self.backend.send(Command::LoadUntil {
                                chat,
                                id: anchor,
                                before: (oldest.timestamp, oldest.id.clone()),
                            });
                        }
                    }
                }
                Event::SearchHits { query, messages } => {
                    if query == self.search.trim() {
                        self.search_hits = messages;
                    }
                }
                Event::Incoming { chat, message } => self.maybe_notify(&chat, &message),
                Event::Picked { chat, paths } => {
                    if self.open_chat.as_deref() == Some(chat.as_str()) {
                        self.stage_files(paths);
                    }
                }
                Event::CopySaved { saved, error } => {
                    if let Some(path) = saved {
                        self.toast(format!("Saved to {}", path.display()));
                    } else if let Some(error) = error {
                        self.toast_error(format!("Could not save the copy: {error}"));
                    }
                }
                Event::ChatSearch { chat, query, hits } => {
                    // Only the current chat and query may fill the panel.
                    if self.chat_search_open
                        && self.open_chat.as_deref() == Some(chat.as_str())
                        && query == self.chat_search_query
                    {
                        match hits {
                            Ok(hits) => self.chat_search_hits = hits,
                            Err(error) => {
                                self.chat_search_hits.clear();
                                self.toast_error(format!("Could not search the chat: {error}"));
                            }
                        }
                    }
                }
                Event::CopyImage { name, error } => match error {
                    Some(error) => self.toast_error(format!("Could not copy the picture: {error}")),
                    None => {
                        let _ = name;
                        self.toast("Picture copied to the clipboard");
                    }
                },

                Event::FileInfo {
                    chat,
                    message,
                    result,
                } => {
                    // The dialog belongs to the message it was asked about.
                    if self.open_chat.as_deref() == Some(chat.as_str()) {
                        match result {
                            Ok(info) => self.dialog = Some(Dialog::FileInfo(Box::new(info))),
                            Err(error) => {
                                self.toast_error(format!("Could not read the details: {error}"));
                            }
                        }
                    }
                    let _ = message;
                }
                Event::PdfPage {
                    path,
                    page,
                    width,
                    result,
                } => {
                    // Ignore an answer that is no longer being waited for.
                    if self.pdf_rendering.as_ref() == Some(&(path.clone(), page, width)) {
                        self.pdf_rendering = None;
                        match result {
                            Ok(rendered) => {
                                if let Some(viewer) = self.viewer.as_mut() {
                                    viewer.pdf_pages = rendered.pages;
                                    // A remembered page may outrun a changed file.
                                    viewer.pdf_page =
                                        viewer.pdf_page.min(rendered.pages.saturating_sub(1));
                                }
                                self.pdf_error = None;
                                self.pdf_page = Some(rendered);
                            }
                            Err(error) => {
                                // A remembered page may outrun a file that changed
                                // on disk: fall back to the first page instead of
                                // leaving the error on screen.
                                if error == "That page is not in the document"
                                    && let Some(viewer) = self.viewer.as_mut()
                                    && viewer.pdf_page > 0
                                {
                                    viewer.pdf_page = 0;
                                    self.pdf_error = None;
                                } else {
                                    self.pdf_error = Some(error);
                                }
                            }
                        }
                    }
                }
                Event::PdfThumbs { path, files } => {
                    // Late previews for a file already left behind are dropped.
                    let current = self.viewer.as_ref().and_then(|viewer| viewer.current());
                    if current.is_some_and(|item| item.kind == ViewerKind::Pdf && item.path == path)
                    {
                        self.pdf_thumbs = Some((path, files));
                    }
                }
                Event::PollCreated { chat, error } => {
                    self.poll_creating = false;
                    if let Some(error) = error {
                        self.toast_error(error);
                    } else if self.dialog == Some(Dialog::CreatePoll(chat)) {
                        self.dialog = None;
                        self.poll_draft = Default::default();
                    }
                }
                Event::PollVoted {
                    chat,
                    message,
                    error,
                } => {
                    self.poll_voting.remove(&(chat, message));
                    if let Some(error) = error {
                        self.toast_error(error);
                    }
                }
                Event::MessageUpdated(message) => {
                    let message = *message;
                    if let Some(conversation) = self.conversations.get_mut(&message.chat)
                        && let Some(existing) = conversation.message_mut(&message.id)
                    {
                        let state = existing.content.media().map(|media| media.state.clone());
                        *existing = message;
                        if let (Some(state), Some(media)) = (state, existing.content.media_mut()) {
                            media.state = state;
                        }
                    }
                }
                Event::Contacts(contacts) => {
                    for contact in contacts {
                        self.contacts.insert(contact.id.clone(), contact);
                    }
                }
                Event::Typing {
                    chat,
                    sender,
                    composing,
                } => {
                    let typers = self.typing.entry(chat).or_default();
                    typers.retain(|(who, _)| *who != sender);
                    if composing {
                        typers.push((sender, Instant::now()));
                    }
                }
                Event::Presence {
                    id,
                    online,
                    last_seen,
                } => {
                    self.presence.insert(id, Presence { online, last_seen });
                }
                Event::Avatar { id, full, path } => {
                    if full {
                        self.avatar_full_requests.remove(&id);
                        self.avatars_full.insert(id, path);
                    } else {
                        self.avatar_requests.remove(&id);
                        self.avatars.insert(id, path);
                    }
                }
                Event::StickerThumb { path, ok } => {
                    self.sticker_thumb_results.push((path, ok));
                }
                Event::Stickers {
                    saved,
                    packs,
                    recent,
                    favorites,
                    emojis,
                } => {
                    self.stickers_saved = saved;
                    self.sticker_packs = packs;
                    self.stickers = recent;
                    self.stickers_favorites = favorites;
                    self.stickers_emojis = emojis.into_iter().collect();
                    self.stickers_pending = false;
                    self.sticker_import_pending = false;
                }
                Event::StickerPackPreview(result) => match result {
                    Ok((pack, publisher)) => {
                        self.dialog = Some(Dialog::StickerPackView {
                            name: pack.name.clone(),
                            publisher,
                            dir: pack.dir.clone(),
                            stickers: pack.stickers.clone(),
                        });
                    }
                    Err(error) => {
                        self.toast_error(error);
                    }
                },
                Event::MessageDeleted { chat, id } => {
                    self.invalidate_message(&chat, &id);
                }
                Event::ChatRemoved { chat } => {
                    self.invalidate_chat(&chat);
                }
                Event::ChatCleared { chat, through } => {
                    self.invalidate_chat_range(&chat, through);
                    if self.open_chat.as_deref() == Some(chat.as_str())
                        && !self.conversations.contains_key(&chat)
                    {
                        self.ensure_loaded(&chat);
                    }
                }
                Event::Media {
                    chat,
                    message,
                    result,
                } => self.handle_media(&chat, &message, result),
                Event::Syncing(syncing) => {
                    if self.syncing && !syncing {
                        self.toast("History loaded");
                    }
                    self.syncing = syncing;
                    if !syncing {
                        self.sync_percent = None;
                    }
                }
                Event::SyncProgress(percent) => self.sync_percent = Some(percent),
                Event::OlderFetched { chat, more } => {
                    let conversation = self.conversations.entry(chat).or_default();
                    conversation.fetching_phone = false;
                    conversation.phone_exhausted = !more;
                    conversation.phone_answered = Some(Instant::now());
                    if conversation.phone_delivered {
                        conversation.phone_misses = 0;
                    } else {
                        conversation.phone_misses = (conversation.phone_misses + 1).min(7);
                    }
                    conversation.phone_delivered = false;
                    // Page the archive again after phone history arrives.
                    conversation.complete = false;
                }
                Event::ReceiptsPrivacy { disabled } => self.account_receipts_off = disabled,
                Event::ContactReady { id, name } => {
                    self.new_contact_pending = false;
                    if self.dialog == Some(Dialog::NewContact) {
                        self.dialog = None;
                    }
                    let name = name
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| crate::util::phone(&id));
                    self.actions.push(Action::StartChat { id, name });
                }
                Event::Info(message) => self.toast(message),
                Event::UpdateAvailable { version, url } => {
                    let notice = crate::updates::Release { version, url };
                    if self.update.as_ref() != Some(&notice) {
                        self.toast(format!("ZapExt {} is available", notice.version));
                    }
                    self.update = Some(notice);
                    self.update_checking = false;
                }
                Event::UpdateUpToDate => {
                    self.update_checking = false;
                    self.toast("You're on the latest version");
                }
                Event::UpdateCheckFailed(error) => {
                    self.update_checking = false;
                    self.toast_error(format!("Could not check for updates: {error}"));
                }
                Event::UpdateSupport(result) => {
                    self.update_support = Some(result);
                    self.update_inspecting = false;
                    self.maybe_download_update();
                }
                Event::UpdateProgress { received, total } => {
                    self.update_download =
                        crate::updates::DownloadState::Downloading { received, total };
                }
                Event::UpdateDownloaded(result) => {
                    self.update_download = match result {
                        Ok(prepared) => crate::updates::DownloadState::Ready(prepared),
                        Err(error) => crate::updates::DownloadState::Failed(error),
                    };
                }
                Event::UpdateInstalling(result) => match result {
                    Ok(()) => self.actions.push(Action::Quit),
                    Err(error) => {
                        self.update_download = crate::updates::DownloadState::Failed(error)
                    }
                },
                Event::Error(message) => {
                    self.sticker_import_pending = false;
                    self.new_contact_pending = false;
                    self.toast_error(message);
                }
            }
        }
    }

    fn handle_link(&mut self, status: LinkStatus) {
        match &status {
            LinkStatus::Connected => {
                for conversation in self.conversations.values_mut() {
                    for message in &mut conversation.messages {
                        if let Content::Poll { state, .. } = &mut message.content {
                            state.refresh_needed = true;
                            state.refreshing = false;
                        }
                    }
                }
                if matches!(self.link, LinkStatus::Disconnected { .. }) {
                    self.toast("Back online");
                }
                self.dialog = match self.dialog.take() {
                    Some(Dialog::PairWithPhone) => None,
                    other => other,
                };
                if let Some(open) = self.open_chat.clone() {
                    self.ensure_loaded(&open);
                }
            }
            LinkStatus::LoggedOut => {
                self.poll_voting.clear();
                self.poll_creating = false;
                self.poll_draft = Default::default();
                self.notifications.clear_all();
                self.chats.clear();
                self.conversations.clear();
                self.contacts.clear();
                self.avatars.clear();
                self.open_chat = None;
                self.toast_error("This device was unlinked from your phone");
            }
            LinkStatus::Failed(message) => self.toast_error(message.clone()),
            _ => {}
        }
        self.link = status;
    }

    fn handle_chat_updated(&mut self, chat: Chat) {
        let is_open =
            self.open_chat.as_deref() == Some(chat.id.as_str()) && self.page == Page::Chats;
        let mut chat = chat;
        if chat.unread == 0 || chat.archived || chat.muted(crate::util::now()) {
            self.notifications.clear(&chat.id);
        }
        if is_open && chat.unread > 0 && self.window_focused && !self.window_hidden {
            chat.unread = 0;
            self.mark_read(&chat.id);
        }
        match self.chats.iter_mut().find(|known| known.id == chat.id) {
            Some(existing) => *existing = chat,
            None => self.chats.push(chat),
        }
        self.chats
            .sort_by_key(|chat| std::cmp::Reverse(chat.last_activity));
    }

    fn handle_media(&mut self, chat: &str, id: &str, result: Result<PathBuf, String>) {
        let Some(message) = self
            .conversations
            .get_mut(chat)
            .and_then(|conversation| conversation.message_mut(id))
        else {
            return;
        };
        let Some(media) = message.content.media_mut() else {
            return;
        };
        match result {
            Ok(path) => {
                media.path = Some(path);
                media.state = MediaState::Idle;
            }
            Err(error) => {
                // Show expired-file failures in the bubble, not as a toast.
                let notice = if error.contains("403") || error.contains("404") {
                    "No longer available on WhatsApp's servers".to_owned()
                } else {
                    error
                };
                log::warn!("download failed: {notice}");
                media.state = MediaState::Failed(notice);
            }
        }
    }

    /// Single invalidation layer for one removed message: SQLite is the
    /// truth, every in-memory projection of that id passes through here.
    /// Cheap by construction, one comparison per cached row, and nothing
    /// is reloaded: a repeated delete still clears a stale screen.
    fn invalidate_message(&mut self, chat: &str, id: &str) {
        if let Some(conversation) = self.conversations.get_mut(chat) {
            conversation.forget(id);
        }
        self.search_hits
            .retain(|message| !(message.chat == chat && message.id == id));
        if self.open_chat.as_deref() == Some(chat) {
            self.chat_search_hits.retain(|message| message.id != id);
            if self.reply_to.as_deref() == Some(id) {
                self.reply_to = None;
            }
            if self.editing.as_deref() == Some(id) {
                self.editing = None;
                self.composer.clear();
            }
            self.selected.retain(|known| known != id);
        }
        // The viewer walks archive media, so a deleted picture must go
        // even when the conversation cache never held it.
        let close_viewer = if let Some(viewer) = self.viewer.as_mut() {
            if viewer.chat != chat {
                false
            } else {
                let current = viewer.current().map(|item| item.message.clone());
                viewer.items.retain(|item| item.message != id);
                // The open item survives by identity, not by number: with
                // [A, B, C] on B, deleting A keeps B on screen instead of
                // sliding to C. Only a deleted open item closes the viewer.
                match current {
                    Some(message) => {
                        match viewer.items.iter().position(|item| item.message == message) {
                            Some(index) => {
                                viewer.index = index;
                                false
                            }
                            // The open item itself is gone: close instead of
                            // sliding to a neighbour nobody chose.
                            None => true,
                        }
                    }
                    None if viewer.items.is_empty() => true,
                    None => {
                        viewer.index = viewer.index.min(viewer.items.len() - 1);
                        false
                    }
                }
            }
        } else {
            false
        };
        if close_viewer {
            // Effective shutdown, not just a hidden window: the decoder,
            // its task and the sound really stop, like a normal close.
            self.stop_media(Some(id));
        } else if self.player.playing_message() == Some(id) {
            // A voice note has no viewer item: still its own sound.
            self.player.stop();
        }
        self.notifications.clear_message(chat, id);
    }

    /// Single invalidation layer for a cleared range: drops only messages
    /// at or below `through`, keeps anything newer in memory, and never
    /// forces a full reload. The archive already dropped the same range.
    fn invalidate_chat_range(&mut self, chat: &str, through: i64) {
        // Membership by set: a big clear with a big selection must not
        // turn quadratic.
        let removed = self
            .conversations
            .get_mut(chat)
            .map(|conversation| conversation.forget_range(through))
            .unwrap_or_default();
        // Global search may hold rows of this chat from any query.
        self.search_hits
            .retain(|message| !(message.chat == chat && message.timestamp <= through));
        if self.open_chat.as_deref() == Some(chat) {
            self.chat_search_hits
                .retain(|message| message.timestamp > through);
            if self
                .reply_to
                .as_ref()
                .is_some_and(|id| removed.contains(id))
            {
                self.reply_to = None;
            }
            if self.editing.as_ref().is_some_and(|id| removed.contains(id)) {
                self.editing = None;
                self.composer.clear();
            }
            self.selected.retain(|known| !removed.contains(known));
            if self
                .selection_anchor
                .as_ref()
                .is_some_and(|anchor| removed.contains(anchor))
            {
                self.selection_anchor = self.selected.last().cloned();
            }
            // The open item survives by identity: a partial clear that
            // keeps it repoints the index instead of jumping to the first
            // picture. Only a cleared open item closes the viewer, with
            // effective shutdown like a normal close.
            let viewer_closed = if self
                .viewer
                .as_ref()
                .is_some_and(|viewer| viewer.chat == chat)
            {
                let current = self
                    .viewer
                    .as_ref()
                    .and_then(|viewer| viewer.current())
                    .map(|item| item.message.clone());
                let items = self.viewer_items(chat);
                match current
                    .and_then(|message| items.iter().position(|item| item.message == message))
                {
                    Some(index) => {
                        if let Some(viewer) = self.viewer.as_mut() {
                            viewer.items = items;
                            viewer.index = index;
                        }
                        false
                    }
                    None => true,
                }
            } else {
                false
            };
            if viewer_closed {
                self.stop_media(None);
            }
        }
        if self
            .player
            .playing_message()
            .is_some_and(|message| removed.contains(message))
        {
            // A voice note has no viewer item: still its own sound.
            self.player.stop();
        }
        self.notifications.clear(chat);
    }

    /// Single invalidation layer for a removed chat: every projection of
    /// the chat goes, the open view steps back, pending notifications die.
    fn invalidate_chat(&mut self, chat: &str) {
        self.chats.retain(|row| row.id != chat);
        // Voice has no viewer item: learn its message before the rows go.
        // A note playing in the open chat belongs to it by construction.
        let playing = self.player.playing_message().map(str::to_owned);
        let voice_gone = playing.is_some_and(|message| {
            self.open_chat.as_deref() == Some(chat)
                || self.conversations.get(chat).is_some_and(|conversation| {
                    conversation.messages.iter().any(|row| row.id == message)
                })
        });
        self.conversations.remove(chat);
        self.search_hits.retain(|message| message.chat != chat);
        self.notifications.clear(chat);
        if self
            .viewer
            .as_ref()
            .is_some_and(|viewer| viewer.chat == chat)
        {
            // Effective shutdown, not just a hidden window.
            self.stop_media(None);
        }
        if voice_gone {
            self.player.stop();
        }
        if self.open_chat.as_deref() == Some(chat) {
            self.open_chat = None;
            self.composer.clear();
            self.editing = None;
            self.reply_to = None;
            self.selected.clear();
            self.selection_anchor = None;
            self.chat_search_open = false;
            self.chat_search_hits.clear();
        }
    }

    /// Cancels a scrub drag, resuming playback only when the clip played
    /// before the drag held it. No jump happens here; the preview slot
    /// and its generation retire with the drag. Returns whether a drag
    /// was active.
    fn cancel_video_scrub(&mut self) -> bool {
        let Some(scrub) = self.video_scrub.take() else {
            return false;
        };
        self.previewer.cancel(&scrub.path);
        self.video_preview = None;
        if scrub.was_playing
            && let Some(item) = self.viewer.as_ref().and_then(|viewer| viewer.current())
        {
            let path = item.path.clone();
            let (video, player) = (&mut self.video, &mut self.player);
            if let Err(error) = video.toggle(&path, &mut || player.stop()) {
                self.toast_error(error);
            }
        }
        true
    }

    /// Drops a scrub drag without seeking or resuming: stepping to
    /// another file or opening a new viewer retires the drag and its
    /// generation, leaving playback to the new view.
    fn drop_video_scrub(&mut self) {
        if let Some(scrub) = self.video_scrub.take() {
            self.previewer.cancel(&scrub.path);
        }
        self.video_preview = None;
    }

    /// Effective media shutdown shared by the viewer close and the delete
    /// invalidation: the video decoder and its task stand down, rendered
    /// PDF pages leave memory, and the worker drops the document. A voice
    /// note stops only when the message it belongs to is gone, passed as
    /// `voice_message`; a plain close passes nothing and never stills
    /// unrelated sound.
    fn stop_media(&mut self, voice_message: Option<&str>) {
        // A scrub never survives its video: dropping the state and its
        // generation retires every pending preview with it.
        self.drop_video_scrub();
        self.viewer = None;
        self.forget_pdf();
        self.video.stop();
        if voice_message.is_some_and(|id| self.player.playing_message() == Some(id)) {
            self.player.stop();
        }
        // The document does not stay in memory once it is closed.
        self.backend.send(Command::ForgetPdf);
    }

    fn ensure_loaded(&mut self, chat: &str) {
        let conversation = self.conversations.entry(chat.to_owned()).or_default();
        if !conversation.requested {
            conversation.requested = true;
            conversation.requested_at = Some(Instant::now());
            self.backend.send(Command::LoadChat {
                chat: chat.to_owned(),
                before: None,
            });
        }
    }
    /// Re-asks for pages that never answered, so an opened chat cannot stay
    /// blank when its first read was lost on a busy worker.
    fn retry_missing_pages(&mut self) {
        /// How long a requested page may take before it is asked again.
        const PATIENCE: Duration = Duration::from_secs(8);
        let mut retry = Vec::new();
        let now = Instant::now();
        for (chat, conversation) in self.conversations.iter_mut() {
            let waiting = conversation.requested
                && conversation.messages.is_empty()
                && !conversation.loading_older
                && !conversation.fetching_phone;
            if waiting
                && conversation
                    .requested_at
                    .is_none_or(|at| now.duration_since(at) >= PATIENCE)
            {
                conversation.requested_at = Some(now);
                retry.push(chat.clone());
            }
        }
        for chat in retry {
            log::warn!("no page arrived for {chat}; asking again");
            self.backend.send(Command::LoadChat { chat, before: None });
        }
    }

    pub fn load_older(&mut self, chat: &str) {
        let Some(conversation) = self.conversations.get_mut(chat) else {
            return;
        };
        if conversation.loading_older {
            return;
        }
        let Some(oldest) = conversation.messages.first() else {
            return;
        };
        if conversation.complete {
            self.fetch_older(chat);
            return;
        }
        conversation.loading_older = true;
        let before = (oldest.timestamp, oldest.id.clone());
        self.scroll_anchor = Some(oldest.id.clone());
        self.backend.send(Command::LoadChat {
            chat: chat.to_owned(),
            before: Some(before),
        });
    }

    /// Requests older phone history when available and outside the cooldown.
    pub fn fetch_older(&mut self, chat: &str) {
        let Some(conversation) = self.conversations.get_mut(chat) else {
            return;
        };
        if conversation.fetching_phone || conversation.phone_exhausted {
            return;
        }
        // Back off after empty responses. Only a connected phone can answer.
        if !matches!(self.link, LinkStatus::Connected) {
            return;
        }
        let cooldown =
            (PHONE_COOLDOWN * 2u32.pow(conversation.phone_misses)).min(Duration::from_secs(600));
        if conversation
            .phone_answered
            .is_some_and(|answered| answered.elapsed() < cooldown)
        {
            return;
        }
        conversation.fetching_phone = true;
        self.scroll_anchor = conversation
            .messages
            .first()
            .map(|oldest| oldest.id.clone());
        self.backend.send(Command::FetchOlder(chat.to_owned()));
    }

    /// Whether a message can still be revoked for everyone: ours, intact,
    /// and inside WhatsApp's delete window.
    pub fn revocable(&self, chat: &str, id: &str) -> bool {
        self.conversations
            .get(chat)
            .and_then(|conversation| conversation.message(id))
            .is_some_and(|message| {
                message.from_me
                    && !matches!(message.content, Content::Revoked)
                    && crate::util::now() - message.timestamp <= REVOKE_WINDOW.as_secs() as i64
            })
    }

    /// Marks a message revoked locally and asks the phone to revoke it.
    fn revoke_message(&mut self, chat: &str, id: &str) {
        if let Some(message) = self
            .conversations
            .get_mut(chat)
            .and_then(|conversation| conversation.message_mut(id))
        {
            message.content = Content::Revoked;
        }
        self.backend.send(Command::Revoke {
            chat: chat.to_owned(),
            id: id.to_owned(),
        });
    }

    /// Drops a message from the local conversation and the archive.
    fn delete_message_local(&mut self, chat: &str, id: &str) {
        if let Some(conversation) = self.conversations.get_mut(chat) {
            conversation.forget(id);
        }
        self.backend.send(Command::DeleteLocal {
            chat: chat.to_owned(),
            id: id.to_owned(),
        });
    }

    fn mark_read(&mut self, chat: &str) {
        self.notifications.clear(chat);
        if let Some(known) = self.chat_mut(chat) {
            known.unread = 0;
        }
        // Clear local unread state regardless of receipt settings.
        self.backend.send(Command::MarkRead {
            chat: chat.to_owned(),
            receipts: self.settings.send_read_receipts,
        });
    }

    fn open_chat(&mut self, id: ChatId) {
        if self.open_chat.as_deref() != Some(id.as_str()) {
            // A search belongs to the chat it was typed in.
            self.chat_search_open = false;
            self.chat_search.clear();
            self.chat_search_hits.clear();
            self.chat_search_query.clear();
            self.chat_search_at = None;
            self.forget_pdf();
        }
        if self.open_chat.as_deref() != Some(id.as_str()) {
            if let Some(previous) = self.open_chat.take() {
                let draft = std::mem::take(&mut self.composer);
                // Discard an unfinished edit instead of keeping it as a draft.
                if self.editing.take().is_some() || draft.trim().is_empty() {
                    self.drafts.remove(&previous);
                    self.draft_mentions.remove(&previous);
                    self.composer_mentions.clear();
                } else {
                    self.drafts.insert(previous.clone(), draft);
                    self.draft_mentions.insert(
                        previous.clone(),
                        std::mem::take(&mut self.composer_mentions),
                    );
                }
                self.stop_composing(&previous);
            }
            self.composer = self.drafts.remove(&id).unwrap_or_default();
            self.composer_mentions = self.draft_mentions.remove(&id).unwrap_or_default();
            self.reply_to = None;
            self.selected.clear();
            self.selection_anchor = None;
            self.editing = None;
        }
        self.emoji_start = None;
        self.mention_start = None;
        self.open_chat = Some(id.clone());
        self.page = Page::Chats;
        self.scroll_to_bottom = true;
        self.at_bottom = true;
        self.focus_composer = true;
        self.ensure_loaded(&id);
        if self
            .conversations
            .get(&id)
            .is_some_and(|conversation| conversation.complete && conversation.messages.is_empty())
        {
            self.fetch_older(&id);
        }
        if self.chat(&id).is_some_and(|chat| chat.unread > 0) {
            self.mark_read(&id);
        }
        if self.settings.last_chat.as_deref() != Some(id.as_str()) {
            self.settings.last_chat = Some(id);
            self.mark_settings_dirty();
        }
        // The previous chat is inactive now: enforce the budget before its
        // pages accumulate without bound.
        self.trim_inactive_chats();
    }

    /// Messages kept for an inactive chat: reopening shows recent history
    /// instantly, older pages reload from the archive on demand.
    const INACTIVE_CHAT_RETAINED: usize = 120;
    /// Inactive conversations kept resident at all: beyond this the stalest
    /// entries leave the RAM entirely and reopen from the archive.
    const INACTIVE_CHATS_KEPT: usize = 10;

    /// Enforces the resident-history budget: every inactive chat keeps its
    /// newest rows, and only the most recently touched inactive chats stay
    /// resident at all. Trimmed chats reset their reload flags so reopening
    /// refetches from the archive; drafts, selection and the open chat are
    /// untouched. Total resident history stays bounded no matter how many
    /// chats the session visits.
    fn trim_inactive_chats(&mut self) {
        if let Some(open) = self.open_chat.clone()
            && let Some(conversation) = self.conversations.get_mut(&open)
        {
            conversation.touched = Some(Instant::now());
        }
        let open = self.open_chat.clone();
        for (id, conversation) in self.conversations.iter_mut() {
            if Some(id.as_str()) != open.as_deref() {
                conversation.trim_older(Self::INACTIVE_CHAT_RETAINED);
                conversation.complete = false;
                conversation.requested = false;
                conversation.loading_older = false;
            }
        }
        let allowed = Self::INACTIVE_CHATS_KEPT + usize::from(open.is_some());
        if self.conversations.len() > allowed {
            let mut idle: Vec<(Option<Instant>, ChatId)> = self
                .conversations
                .iter()
                .filter(|(id, _)| Some(id.as_str()) != open.as_deref())
                .map(|(id, conversation)| (conversation.touched, id.clone()))
                .collect();
            idle.sort_by_key(|(touched, _)| *touched);
            for (_, id) in idle.into_iter().take(self.conversations.len() - allowed) {
                self.conversations.remove(&id);
            }
        }
    }

    /// Identifies a PDF for its remembered page: file name plus size on disk.
    ///
    /// Cache names already carry the chat and message, so this survives restarts
    /// without pointing at anything personal.
    fn pdf_key(path: &Path) -> String {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
        format!("{name}:{size}")
    }
    /// The chat's pictures and PDFs that are on disk, oldest first.
    ///
    /// The viewer walks this list, so it only holds files it can actually
    /// show and keeps the message each one came from.
    pub(crate) fn viewer_items(&self, chat: &str) -> Vec<ViewerItem> {
        let Some(conversation) = self.conversations.get(chat) else {
            return Vec::new();
        };
        conversation
            .messages
            .iter()
            .filter_map(|message| {
                let (media, kind) = match &message.content {
                    Content::Image { media, .. } => (media, ViewerKind::Picture),
                    // A picture sent as a file is still a picture.
                    Content::Document { media, .. } if media.mime.starts_with("image/") => {
                        (media, ViewerKind::Picture)
                    }
                    Content::Document { media, .. } if media.mime == "application/pdf" => {
                        (media, ViewerKind::Pdf)
                    }
                    // Videos with a file play in the viewer; anything else
                    // keeps its old behaviour.
                    Content::Video { media, gif, .. } if !gif => (media, ViewerKind::Video),
                    _ => return None,
                };
                let path = media.path.as_ref().filter(|path| path.is_file())?;
                Some(ViewerItem {
                    message: message.id.clone(),
                    path: path.clone(),
                    kind,
                })
            })
            .collect()
    }

    /// Opens the media viewer on one picture or sticker of a chat.
    pub fn open_viewer(&mut self, chat: &str, message: &str) {
        self.drop_video_scrub(); // A new viewer retires any drag from the old video.
        let items = self.viewer_items(chat);
        if items.is_empty() {
            return;
        }
        let index = items
            .iter()
            .position(|item| item.message == message)
            .unwrap_or(items.len() - 1);
        self.viewer = Some(Viewer {
            chat: chat.to_owned(),
            items,
            index,
            zoom: 1.0,
            offset: (0.0, 0.0),
            pdf_page: 0,
            pdf_pages: 0,
            pdf_rotate: 0,
        });
        self.restore_pdf_page();
        self.request_pdf_thumbs();
    }
    /// Reopens a PDF on the page where the reader left it.
    fn restore_pdf_page(&mut self) {
        let path = match self.viewer.as_ref().and_then(|viewer| viewer.current()) {
            Some(item) if item.kind == ViewerKind::Pdf => item.path.clone(),
            _ => return,
        };
        // The count is still unknown here; turning pages clamps it later.
        if let Some(page) = self.settings.pdf_pages.get(&Self::pdf_key(&path))
            && let Some(viewer) = self.viewer.as_mut()
        {
            viewer.pdf_page = *page;
        }
    }
    /// Remembers the page of the PDF on screen, capped so the setting stays small.
    fn remember_pdf_page(&mut self) {
        let Some(viewer) = self.viewer.as_ref() else {
            return;
        };
        let Some(item) = viewer.current() else {
            return;
        };
        if item.kind != ViewerKind::Pdf {
            return;
        }
        // The count is unknown until the first page renders; clamp then.
        let page = viewer.pdf_page.min(viewer.pdf_pages.saturating_sub(1));
        self.settings
            .pdf_pages
            .insert(Self::pdf_key(&item.path), page);
        while self.settings.pdf_pages.len() > 100 {
            // Drop an arbitrary old entry; order does not matter here.
            if let Some(first) = self.settings.pdf_pages.keys().next().cloned() {
                self.settings.pdf_pages.remove(&first);
            } else {
                break;
            }
        }
        self.mark_settings_dirty();
    }

    /// Returns keyboard focus to the open conversation when no search or
    /// overlay is active.
    fn refocus_composer(&mut self, ctx: &egui::Context) {
        let search_focused = ctx.memory(|memory| memory.has_focus(egui::Id::new("chat-search")));
        if self.page == Page::Chats
            && self.dialog.is_none()
            && self.viewer.is_none()
            && !self.chat_search_open
            && self.picker.is_none()
            && self.recording.is_none()
            && self.open_chat.is_some()
            && self.search.trim().is_empty()
            && !self.focus_search
            && !search_focused
        {
            self.focus_composer = true;
        }
    }

    /// Updates typing state after composer changes.
    pub fn note_keystroke(&mut self) {
        self.last_keystroke = Some(Instant::now());
        if !self.composing
            && self.settings.send_typing
            && let Some(chat) = self.open_chat.clone()
        {
            self.composing = true;
            self.backend.send(Command::Composing {
                chat,
                composing: true,
            });
        }
    }

    fn stop_composing(&mut self, chat: &str) {
        if self.composing {
            self.composing = false;
            self.backend.send(Command::Composing {
                chat: chat.to_owned(),
                composing: false,
            });
        }
        self.last_keystroke = None;
    }

    /// Whether a send to this chat must not start. Newsletters are
    /// refused by address whether or not they are listed. Other chats
    /// missing from the list keep working (confirm dialogs, races with the
    /// chat list), and the worker re-checks known chats downstream.
    fn send_blocked(&self, chat: &ChatId) -> bool {
        if chat.ends_with("@newsletter") {
            return true;
        }
        self.chat(chat)
            .is_some_and(|chat| !crate::model::can_send(chat))
    }

    fn send_blocked_toast(&mut self, chat: &ChatId) -> bool {
        let blocked = self.send_blocked(chat);
        if blocked {
            self.toast_error("This chat cannot send messages");
        }
        blocked
    }

    fn send_text(&mut self, chat: ChatId, text: String, quoting: Option<String>) {
        if self.send_blocked_toast(&chat) {
            return;
        }
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        let (text, mentions) = self.encode_composer_mentions(&chat, text);
        self.emoji_start = None;
        self.mention_start = None;
        self.stop_composing(&chat);
        if let Some(id) = self.editing.take() {
            if let Some(message) = self
                .conversations
                .get_mut(&chat)
                .and_then(|conversation| conversation.message_mut(&id))
            {
                message.content = Content::text(text.clone());
                message.edited = true;
                message.mentions = mention_refs(&mentions);
            }
            self.backend.send(Command::EditText {
                chat,
                id,
                text,
                mentions,
            });
            return;
        }
        self.backend.send(Command::SendText {
            chat,
            text,
            quoting,
            mentions,
        });
        self.scroll_to_bottom = true;
        self.at_bottom = true;
    }

    /// Replaces selected display-name mentions with WhatsApp's `@user`
    /// tokens and returns the JIDs for message context.
    fn encode_composer_mentions(&mut self, chat: &str, mut text: String) -> (String, Vec<String>) {
        let participants = self
            .chat(chat)
            .map(|chat| chat.participants.clone())
            .unwrap_or_default();
        let selected = std::mem::take(&mut self.composer_mentions);
        let mut mentions = Vec::new();
        for mention in selected {
            if !participants.iter().any(|id| id == &mention.id) {
                continue;
            }
            let Some(user) = mention.id.split('@').next().filter(|user| !user.is_empty()) else {
                continue;
            };
            let shown = format!("@{}", mention.name);
            if let Some(at) = find_named_mention(&text, &shown) {
                text.replace_range(at..at + shown.len(), &format!("@{user}"));
                if !mentions.iter().any(|id| id == &mention.id) {
                    mentions.push(mention.id);
                }
            }
        }
        // Preserve mentions in an edited draft that already contains wire
        // tokens, even when it did not originate in this composer session.
        for id in participants {
            let Some(user) = id.split('@').next().filter(|user| !user.is_empty()) else {
                continue;
            };
            if contains_mention_token(&text, user) && !mentions.iter().any(|known| known == &id) {
                mentions.push(id);
            }
        }
        (text, mentions)
    }

    /// Adds files to the open chat's composer.
    fn stage_files(&mut self, paths: Vec<PathBuf>) {
        let Some(chat) = self.open_chat.clone() else {
            self.toast_error("Open a chat first");
            return;
        };
        if self.send_blocked_toast(&chat) {
            return;
        }
        for path in paths {
            self.pending.push(Pending::File(path));
        }
        self.focus_composer = true;
    }

    /// Sends pending files, attaching the caption to the first.
    fn send_pending(&mut self, chat: ChatId, caption: String) {
        if self.send_blocked_toast(&chat) {
            return;
        }
        let caption = caption.trim().to_owned();
        let (caption, mentions) = self.encode_composer_mentions(&chat, caption);
        let caption = Some(caption).filter(|text| !text.is_empty());
        let mut caption = caption;
        let mut mentions = mentions;
        self.emoji_start = None;
        self.mention_start = None;
        let mut files = Vec::new();
        for item in std::mem::take(&mut self.pending) {
            match item {
                Pending::Picture {
                    width,
                    height,
                    rgba,
                    ..
                } => {
                    self.backend.send(Command::SendImage {
                        chat: chat.clone(),
                        width: width as u32,
                        height: height as u32,
                        rgba: std::sync::Arc::try_unwrap(rgba).unwrap_or_else(|arc| (*arc).clone()),
                        caption: caption.take(),
                        mentions: std::mem::take(&mut mentions),
                    });
                }
                Pending::File(path) => files.push(path),
            }
        }
        if !files.is_empty() {
            self.backend.send(Command::SendFiles {
                chat,
                paths: files,
                caption: caption.take(),
                mentions,
            });
        }
        self.reply_to = None;
        self.scroll_to_bottom = true;
        self.at_bottom = true;
    }

    #[allow(dead_code)]
    fn send_files(&mut self, paths: Vec<PathBuf>) {
        let Some(chat) = self.open_chat.clone() else {
            self.toast_error("Open a chat first");
            return;
        };
        if paths.is_empty() {
            return;
        }
        self.toast(format!(
            "Sending {} file{}…",
            paths.len(),
            if paths.len() == 1 { "" } else { "s" }
        ));
        self.backend.send(Command::SendFiles {
            chat,
            paths,
            caption: None,
            mentions: Vec::new(),
        });
        self.scroll_to_bottom = true;
        self.at_bottom = true;
    }

    fn tick(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        if self.composing
            && let Some(last) = self.last_keystroke
            && now.duration_since(last) > COMPOSING_TIMEOUT
            && let Some(chat) = self.open_chat.clone()
        {
            self.stop_composing(&chat);
        }
        for typers in self.typing.values_mut() {
            typers.retain(|(_, since)| now.duration_since(*since) < TYPING_TIMEOUT);
        }
        self.typing.retain(|_, typers| !typers.is_empty());
        self.toasts
            .retain(|toast| toast.created.elapsed() < Duration::from_millis(3200));
        if self.settings.check_for_updates
            && !self.backend.is_offline()
            && self
                .last_update_check
                .is_none_or(|at| at.elapsed() >= crate::updates::CHECK_INTERVAL)
        {
            self.last_update_check = Some(now);
            self.backend.send(Command::CheckForUpdates);
        }
        self.maybe_download_update();
        if self.settings_dirty && self.last_settings_save.elapsed() > Duration::from_secs(2) {
            self.save_settings();
        }
        if !self.typing.is_empty() || self.composing {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        self.sync_pdf_view();
        self.poll_chat_search();
        self.pump_raise();
    }

    /// Runs the in-chat search once the typing in its field pauses.
    /// Asks the worker for the PDF page the viewer is showing.
    ///
    /// The request follows the page and the zoom, and is skipped while the
    /// same page is already sharp enough or on its way.
    fn sync_pdf_view(&mut self) {
        let Some(viewer) = self.viewer.as_ref() else {
            return;
        };
        let Some(item) = viewer.current() else {
            return;
        };
        if item.kind != ViewerKind::Pdf {
            return;
        }
        let path = item.path.clone();
        let page = viewer.pdf_page;
        let width = crate::pdf::render_width(self.viewer_view_width, viewer.zoom);
        let sharp = self
            .pdf_texture
            .as_ref()
            .is_some_and(|(known, known_page, known_width, _)| {
                *known == path
                    && *known_page == page
                    && width <= known_width.saturating_add(PDF_SHARP_ENOUGH)
            });
        if sharp {
            return;
        }
        let target = (path, page, width);
        if self.pdf_rendering.as_ref() == Some(&target) {
            return;
        }
        self.pdf_error = None;
        self.pdf_rendering = Some(target.clone());
        self.backend.send(Command::RenderPdfPage {
            path: target.0,
            page: target.1,
            width: target.2,
        });
    }

    /// Drops the rendered page and its texture.
    pub fn forget_pdf(&mut self) {
        self.pdf_page = None;
        self.pdf_texture = None;
        self.pdf_rendering = None;
        self.pdf_error = None;
        self.pdf_thumbs = None;
    }
    /// Asks the worker for the open PDF's page previews, unless they are here.
    fn request_pdf_thumbs(&mut self) {
        let Some(viewer) = self.viewer.as_ref() else {
            return;
        };
        let Some(item) = viewer.current() else {
            return;
        };
        if item.kind != ViewerKind::Pdf {
            return;
        }
        if self
            .pdf_thumbs
            .as_ref()
            .is_some_and(|(known, _)| known == &item.path)
        {
            return;
        }
        self.pdf_thumbs = None;
        self.backend.send(Command::PdfThumbs {
            path: item.path.clone(),
        });
    }
    /// Runs the in-chat search once the typing in its field pauses.
    fn poll_chat_search(&mut self) {
        let Some(chat) = self.open_chat.clone() else {
            return;
        };
        if !self.chat_search_open {
            return;
        }
        let Some(at) = self.chat_search_at else {
            return;
        };
        if at.elapsed() < CHAT_SEARCH_PAUSE {
            // Come back when the pause is over instead of every frame.
            self.waker
                .wake_after(CHAT_SEARCH_PAUSE.saturating_sub(at.elapsed()));
            return;
        }
        self.chat_search_at = None;
        let query = self.chat_search.trim().to_owned();
        self.chat_search_query = query.clone();
        if query.is_empty() {
            self.chat_search_hits.clear();
            return;
        }
        self.backend.send(Command::SearchChat { chat, query });
    }

    fn inspect_update(&mut self) {
        if self.update_support.is_none() && !self.update_inspecting {
            self.update_inspecting = true;
            self.backend.send(Command::InspectUpdate);
        }
    }

    fn maybe_download_update(&mut self) {
        if !self.settings.check_for_updates
            || !self.settings.download_updates_automatically
            || self.update.is_none()
            || !matches!(self.update_download, crate::updates::DownloadState::Idle)
        {
            return;
        }
        self.inspect_update();
        if matches!(self.update_support, Some(Ok(_))) {
            self.download_update();
        }
    }

    fn download_update(&mut self) {
        if !matches!(
            self.update_download,
            crate::updates::DownloadState::Idle | crate::updates::DownloadState::Failed(_)
        ) || !matches!(self.update_support, Some(Ok(_)))
        {
            return;
        }
        if let Some(release) = self.update.clone() {
            self.update_download = crate::updates::DownloadState::Downloading {
                received: 0,
                total: 0,
            };
            self.backend.send(Command::DownloadUpdate {
                release,
                source: crate::updates::Source::GitHub,
            });
        }
    }

    pub fn mark_settings_dirty(&mut self) {
        self.settings_dirty = true;
    }

    fn save_settings(&mut self) {
        self.settings_dirty = false;
        self.last_settings_save = Instant::now();
        if let Err(error) = self.settings.save(&self.dirs.settings_file()) {
            log::warn!("could not save settings: {error}");
        }
    }

    pub fn load_custom_themes(&mut self) {
        self.custom_themes.start(
            self.dirs.config.join("themes"),
            self.settings.custom_theme.clone(),
            &self.waker,
        );
    }

    fn poll_custom_themes(&mut self) {
        if self.custom_themes.needs_reload() {
            self.load_custom_themes();
        }
        if !self.custom_themes.poll() {
            return;
        }
        let mut changed = false;
        if let Some(filename) = &self.settings.custom_theme
            && let Some(theme) = self.custom_themes.find(filename)
            && self.settings.custom_theme_cache.as_ref() != Some(theme)
        {
            self.settings.custom_theme_cache = Some(theme.clone());
            changed = true;
        }
        if self.custom_themes.follows_omarchy() {
            if let Some(theme) = self.custom_themes.system_theme()
                && self.settings.system_theme_cache.as_ref() != Some(theme)
            {
                self.settings.system_theme_cache = Some(theme.clone());
                changed = true;
            }
        } else if self.settings.system_theme_cache.take().is_some() {
            changed = true;
        }
        if changed {
            self.mark_settings_dirty();
        }
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        let preference = self.settings.cached_palette().map_or_else(
            || match self.settings.theme {
                ThemeChoice::Dark => egui::ThemePreference::Dark,
                ThemeChoice::Light => egui::ThemePreference::Light,
                ThemeChoice::System => egui::ThemePreference::System,
            },
            |palette| {
                if palette.dark {
                    egui::ThemePreference::Dark
                } else {
                    egui::ThemePreference::Light
                }
            },
        );
        ctx.set_theme(preference);
        // Use the same preference for our palette and egui's native controls.
        let dark = ctx.theme() == egui::Theme::Dark;
        let palette = self.settings.cached_palette().unwrap_or_else(|| {
            if dark {
                Palette::dark()
            } else {
                Palette::light()
            }
        });
        if self.applied_dark.is_none() || self.palette != palette {
            self.palette = palette;
            crate::theme::apply(ctx, &self.palette);
            self.applied_dark = Some(dark);
        }
        if !self.zoom_applied {
            ctx.set_zoom_factor(self.settings.zoom);
            self.zoom_applied = true;
        }
    }

    fn apply_actions(&mut self, ctx: &egui::Context) {
        let mut actions = std::mem::take(&mut self.actions);
        while !actions.is_empty() {
            for action in actions.drain(..) {
                self.apply(action, ctx);
            }
            actions = std::mem::take(&mut self.actions);
        }
    }

    fn apply(&mut self, action: Action, ctx: &egui::Context) {
        match action {
            Action::Open(page) => {
                let opens_chats = page == Page::Chats;
                self.page = page;
                self.dialog = None;
                self.emoji_start = None;
                self.mention_start = None;
                if opens_chats {
                    self.refocus_composer(ctx);
                }
            }
            Action::OpenChat(id) => self.open_chat(id),
            Action::StartChat { id, name } => {
                if self.chat(&id).is_none() {
                    self.chats.push(Chat::new(id.clone(), name.clone()));
                    self.backend.send(Command::EnsureChat {
                        chat: id.clone(),
                        name,
                    });
                }
                self.open_chat(id);
            }
            Action::OpenMessage { chat, message } => {
                self.open_chat(chat.clone());
                // Keep the search result, not the chat end, in view.
                self.scroll_to_bottom = false;
                self.at_bottom = false;
                crate::ui::conversation::flash_message(ctx, &message);
                self.scroll_anchor = Some(message.clone());
                let conversation = self.conversations.entry(chat.clone()).or_default();
                if conversation.message(&message).is_none()
                    && !conversation.loading_older
                    && let Some(oldest) = conversation.messages.first()
                {
                    // Load older archive pages toward the search result.
                    conversation.loading_older = true;
                    self.backend.send(Command::LoadUntil {
                        chat,
                        id: message,
                        before: (oldest.timestamp, oldest.id.clone()),
                    });
                }
            }
            Action::CloseChat => {
                self.viewer = None;
                self.forget_pdf();
                if let Some(chat) = self.open_chat.take() {
                    self.stop_composing(&chat);
                    let draft = std::mem::take(&mut self.composer);
                    if self.editing.take().is_none() && !draft.trim().is_empty() {
                        self.drafts.insert(chat.clone(), draft);
                        self.draft_mentions
                            .insert(chat, std::mem::take(&mut self.composer_mentions));
                    } else {
                        self.composer_mentions.clear();
                    }
                }
                self.reply_to = None;
                self.selected.clear();
                self.selection_anchor = None;
                self.emoji_start = None;
                self.mention_start = None;
                // Nobody is open: every conversation is inactive budget now.
                self.trim_inactive_chats();
            }
            Action::SendText {
                chat,
                text,
                quoting,
            } => {
                self.send_text(chat, text, quoting);
                self.reply_to = None;
            }
            Action::RefreshPoll { chat, message } => {
                if let Some(row) = self
                    .conversations
                    .get_mut(&chat)
                    .and_then(|chat| chat.message_mut(&message))
                    && let Content::Poll { state, .. } = &mut row.content
                {
                    state.refreshing = true;
                }
                self.backend.send(Command::RefreshPoll { chat, message });
            }
            Action::CreatePoll { chat, draft } => {
                if !self.poll_creating {
                    match draft.validated() {
                        Ok(draft) => {
                            self.poll_creating = true;
                            self.backend.send(Command::CreatePoll { chat, draft });
                        }
                        Err(error) => self.toast_error(error),
                    }
                }
            }
            Action::VotePoll {
                chat,
                message,
                choices,
            } => {
                if self.poll_voting.insert((chat.clone(), message.clone())) {
                    self.backend.send(Command::VotePoll {
                        chat,
                        message,
                        choices,
                    });
                }
            }
            Action::Composing { chat, composing } => {
                if composing {
                    self.note_keystroke();
                } else {
                    self.stop_composing(&chat);
                }
            }
            Action::MarkRead(chat) => self.mark_read(&chat),
            Action::LoadOlder(chat) => self.load_older(&chat),
            Action::FetchOlder(chat) => self.fetch_older(&chat),
            Action::Download { chat, message } => {
                if let Some(media) = self
                    .conversations
                    .get_mut(&chat)
                    .and_then(|conversation| conversation.message_mut(&message))
                    .and_then(|message| message.content.media_mut())
                {
                    media.state = MediaState::Downloading;
                }
                self.backend.send(Command::Download { chat, message });
            }
            Action::OpenFile(path) => {
                if let Err(error) = open::that_detached(&path) {
                    self.toast_error(format!("Could not open {}: {error}", path.display()));
                }
            }
            Action::ShowInFolder(path) => {
                if let Err(error) = crate::util::reveal_in_folder(&path) {
                    self.toast_error(error);
                }
            }
            Action::OpenViewer { chat, message } => {
                self.open_viewer(&chat, &message);
            }
            Action::ViewerStep(step) => {
                // The page of the file left behind is kept before the view moves on.
                self.remember_pdf_page();
                if let Some(viewer) = self.viewer.as_mut() {
                    viewer.step(step);
                }
                self.video.stop();
                self.drop_video_scrub();
                // A new PDF opens where its reader left it.
                self.restore_pdf_page();
                self.request_pdf_thumbs();
            }
            Action::ViewerPage(step) => {
                if let Some(viewer) = self.viewer.as_mut() {
                    viewer.page_by(step);
                }
                self.remember_pdf_page();
            }
            Action::ViewerPageTo(page) => {
                if let Some(viewer) = self.viewer.as_mut() {
                    viewer.page_to(page);
                }
                self.remember_pdf_page();
            }
            Action::ViewerRotate => {
                if let Some(viewer) = self.viewer.as_mut() {
                    viewer.pdf_rotate = (viewer.pdf_rotate + 1) % 4;
                }
                // The texture holds the old orientation; the rotated pixels
                // upload over it on the next frame.
                self.pdf_texture = None;
            }
            Action::VideoToggle => {
                if let Some(item) = self.viewer.as_ref().and_then(|viewer| viewer.current()) {
                    let path = item.path.clone();
                    let (video, player) = (&mut self.video, &mut self.player);
                    if let Err(error) = video.toggle(&path, &mut || player.stop()) {
                        self.toast_error(error);
                    }
                }
            }
            Action::VideoSeek(fraction) => {
                if let Some(item) = self.viewer.as_ref().and_then(|viewer| viewer.current())
                    && let Err(error) = self.video.seek(&item.path, fraction)
                {
                    self.toast_error(error);
                }
            }
            Action::VideoScrubCancel => {
                self.cancel_video_scrub();
            }
            Action::VideoVolume(volume) => {
                let volume = volume.clamp(0.0, 1.0);
                self.settings.video_volume = volume;
                if volume > 0.01 {
                    self.settings.video_muted = false;
                }
                self.video.set_output(volume, self.settings.video_muted);
            }
            Action::VideoMuteToggle => {
                self.settings.video_muted = !self.settings.video_muted;
                self.video
                    .set_output(self.settings.video_volume, self.settings.video_muted);
                self.mark_settings_dirty();
            }
            Action::ViewerZoom { factor, anchor } => {
                if let Some(viewer) = self.viewer.as_mut() {
                    viewer.zoom_by(factor, anchor);
                }
            }
            Action::ViewerPan((x, y)) => {
                if let Some(viewer) = self.viewer.as_mut() {
                    viewer.offset = (viewer.offset.0 + x, viewer.offset.1 + y);
                }
            }
            Action::ViewerFit => {
                if let Some(viewer) = self.viewer.as_mut() {
                    viewer.zoom = 1.0;
                    viewer.offset = (0.0, 0.0);
                }
            }
            Action::CloseViewer => {
                // Escape mid-drag restores the pre-drag state instead of
                // closing the viewer.
                if !self.cancel_video_scrub() {
                    self.remember_pdf_page();
                    // A plain close never stops a voice note: only a deleted
                    // message stills its own sound.
                    self.stop_media(None);
                }
            }
            Action::ToggleChatSearch => {
                self.chat_search_open = !self.chat_search_open;
                if self.chat_search_open {
                    self.chat_search_focus = true;
                    self.chat_search_at = Some(Instant::now());
                } else {
                    self.chat_search.clear();
                    self.chat_search_hits.clear();
                    self.chat_search_query.clear();
                    self.refocus_composer(ctx);
                }
            }
            Action::ChatSearch(query) => {
                if query != self.chat_search {
                    self.chat_search = query;
                    self.chat_search_at = Some(Instant::now());
                }
            }
            Action::CloseChatSearch => {
                self.chat_search_open = false;
                self.chat_search.clear();
                self.chat_search_hits.clear();
                self.chat_search_query.clear();
                self.chat_search_at = None;
                self.refocus_composer(ctx);
            }
            Action::SaveCopy(path) => self.backend.send(Command::SaveCopy { from: path }),
            Action::CycleAudioSpeed(message) => {
                let speed = crate::audio::next_speed(self.player.speed_of(&message));
                self.player.set_speed(&message, speed);
                self.waker.wake();
            }
            Action::OpenUrl(url) => ctx.open_url(egui::OpenUrl::new_tab(url)),
            Action::CopyText(text) => {
                ctx.copy_text(text);
                self.toast("Copied");
            }
            Action::Reply(id) => {
                self.reply_to = Some(id);
                self.focus_composer = true;
            }
            Action::CancelReply => self.reply_to = None,
            Action::Forward {
                from_chat,
                message,
                to_chat,
            } => {
                if self.send_blocked_toast(&to_chat) {
                    return;
                }
                self.backend.send(Command::Forward {
                    from_chat,
                    message,
                    to_chat,
                });
                self.dialog = None;
                self.forward_search.clear();
            }
            Action::ForwardMany {
                from_chat,
                messages,
                to_chats,
            } => {
                let allowed: Vec<ChatId> = to_chats
                    .into_iter()
                    .filter(|id| !self.send_blocked(id))
                    .collect();
                if allowed.is_empty() {
                    self.toast_error("None of these chats can receive forwards");
                    return;
                }
                self.backend.send(Command::ForwardMany {
                    from_chat,
                    messages,
                    to_chats: allowed,
                });
                self.dialog = None;
                self.forward_search.clear();
                self.selected.clear();
                self.selection_anchor = None;
            }
            Action::ToggleSelect(id) => {
                self.selection_anchor = Some(id.clone());
                if let Some(known) = self.selected.iter().position(|known| known == &id) {
                    self.selected.remove(known);
                    if self.selected.is_empty() {
                        self.selection_anchor = None;
                    }
                } else {
                    self.selected.push(id);
                }
            }
            Action::SelectRange(id) => {
                let Some(anchor) = self.selection_anchor.clone() else {
                    self.selected.push(id.clone());
                    self.selection_anchor = Some(id);
                    return;
                };
                let Some(chat) = self.open_chat.as_deref() else {
                    return;
                };
                let Some(conversation) = self.conversations.get(chat) else {
                    return;
                };
                let position = |needle: &str| {
                    conversation
                        .messages
                        .iter()
                        .position(|message| message.id == needle)
                };
                if let (Some(from), Some(to)) = (position(&anchor), position(&id)) {
                    let (start, end) = if from <= to { (from, to) } else { (to, from) };
                    for message in &conversation.messages[start..=end] {
                        if !matches!(message.content, Content::Revoked)
                            && !self.selected.iter().any(|known| known == &message.id)
                        {
                            self.selected.push(message.id.clone());
                        }
                    }
                    self.selected
                        .sort_by_key(|selected| position(selected).unwrap_or(usize::MAX));
                }
                self.selection_anchor = Some(id);
            }
            Action::ClearSelection => {
                self.selected.clear();
                self.selection_anchor = None;
            }
            Action::Edit(id) => {
                let text = self
                    .open_chat
                    .as_deref()
                    .and_then(|chat| self.conversations.get(chat))
                    .and_then(|conversation| conversation.message(&id))
                    .and_then(|message| match &message.content {
                        Content::Text { text, .. } => Some(text.clone()),
                        _ => None,
                    });
                if let Some(text) = text {
                    self.editing = Some(id);
                    self.reply_to = None;
                    self.composer = text;
                    self.composer_mentions.clear();
                    self.emoji_start = None;
                    self.mention_start = None;
                    self.focus_composer = true;
                }
            }
            Action::CancelEdit => {
                if self.editing.take().is_some() {
                    self.composer.clear();
                    self.composer_mentions.clear();
                    self.emoji_start = None;
                    self.mention_start = None;
                }
            }
            Action::DeleteForEveryone(id) => {
                if let Some(chat) = self.open_chat.clone() {
                    self.revoke_message(&chat, &id);
                }
            }
            Action::DeleteForMe(id) => {
                if let Some(chat) = self.open_chat.clone() {
                    self.delete_message_local(&chat, &id);
                }
            }
            Action::DeleteMany { ids, for_everyone } => {
                if let Some(chat) = self.open_chat.clone() {
                    let mut revoked = 0usize;
                    let mut local = 0usize;
                    for id in &ids {
                        if for_everyone && self.revocable(&chat, id) {
                            self.revoke_message(&chat, id);
                            revoked += 1;
                        } else {
                            self.delete_message_local(&chat, id);
                            local += 1;
                        }
                    }
                    self.selected.clear();
                    self.selection_anchor = None;
                    self.dialog = None;
                    match (revoked, local) {
                        (0, 0) => {}
                        (revoked, 0) => self.toast(format!("Deleted {revoked} for everyone")),
                        (0, local) => self.toast(format!("Deleted {local} for you")),
                        _ => self.toast(format!("Deleted {revoked} for everyone, {local} for you")),
                    }
                }
            }
            Action::Attach => {
                if let Some(chat) = self.open_chat.clone()
                    && !self.send_blocked_toast(&chat)
                {
                    self.backend.send(Command::PickFiles(chat));
                }
            }
            Action::SendFiles(paths) => self.stage_files(paths),
            Action::SendPending { chat, caption } => self.send_pending(chat, caption),
            Action::RemovePending(index) => {
                if index < self.pending.len() {
                    self.pending.remove(index);
                }
            }
            Action::ClearPending => self.pending.clear(),
            Action::PlayVoice { message, path } => self.play_voice(message, path),
            Action::SeekVoice {
                message,
                path,
                fraction,
            } => {
                if let Err(error) = self.player.seek(&message, &path, fraction) {
                    self.toast_error(error);
                }
            }
            Action::StartRecording => {
                if let Some(chat) = self.open_chat.clone()
                    && !self.send_blocked_toast(&chat)
                    && self.recording.is_none()
                {
                    self.recording = Some(Recorder::start(self.waker.clone()));
                }
            }
            Action::CancelRecording => {
                self.recording = None;
                self.refocus_composer(ctx);
            }
            Action::SendRecording => {
                self.send_recording();
                self.refocus_composer(ctx);
            }
            Action::SetMuted(chat, until) => {
                if let Some(known) = self.chat_mut(&chat) {
                    known.muted_until = until;
                }
                self.backend.send(Command::SetMuted(chat, until));
            }
            Action::TogglePicker(tab) => {
                self.emoji_start = None;
                self.mention_start = None;
                if self.picker == Some(tab) {
                    self.picker = None;
                    self.refocus_composer(ctx);
                } else {
                    self.picker = Some(tab);
                    self.picker_search.clear();
                    self.sticker_search.clear();
                    // Reopening the picker returns to this tab, even after a restart.
                    self.settings.picker_tab = tab;
                    self.actions.push(Action::SettingsChanged);
                    self.picker_focus = tab == PickerTab::Emoji;
                    self.emoji_selected = 0;
                    if matches!(tab, PickerTab::Stickers | PickerTab::Favorites) {
                        self.stickers_pending = self.stickers.is_empty()
                            && self.stickers_saved.is_empty()
                            && self.sticker_packs.is_empty();
                        self.backend.send(Command::RecentStickers);
                    }
                }
            }
            Action::ClosePicker => {
                self.picker = None;
                self.refocus_composer(ctx);
            }
            Action::InsertEmoji(emoji) => {
                self.insert_in_composer(ctx, &emoji);
                self.remember_emoji(&emoji);
                self.focus_composer = true;
            }
            Action::InsertEmojiCompletion { emoji, start, end } => {
                let starts_with_colon = start
                    .checked_add(1)
                    .is_some_and(|after| self.composer.get(start..after) == Some(":"));
                if starts_with_colon
                    && start <= end
                    && self.composer.is_char_boundary(start)
                    && self.composer.is_char_boundary(end)
                {
                    self.composer.replace_range(start..end, &emoji);
                    let cursor = self.composer[..start].chars().count() + emoji.chars().count();
                    self.set_composer_cursor(ctx, cursor);
                    self.remember_emoji(&emoji);
                    self.focus_composer = true;
                }
                self.emoji_start = None;
            }
            Action::CloseEmojiSuggestions => {
                self.emoji_start = None;
                self.focus_composer = true;
            }
            Action::InsertMention {
                id,
                name,
                start,
                end,
            } => {
                let member = self.current_chat().is_some_and(|chat| {
                    chat.is_group() && chat.participants.iter().any(|known| known == &id)
                });
                let mention_at = start
                    .checked_add(1)
                    .is_some_and(|after| self.composer.get(start..after) == Some("@"));
                if member
                    && start <= end
                    && self.composer.is_char_boundary(start)
                    && self.composer.is_char_boundary(end)
                    && mention_at
                {
                    let mention = format!("@{name}");
                    let inserted = format!("{mention} ");
                    self.composer.replace_range(start..end, &inserted);
                    self.composer_mentions.push(ComposerMention { id, name });
                    let cursor = self.composer[..start + inserted.len()].chars().count();
                    self.set_composer_cursor(ctx, cursor);
                    self.focus_composer = true;
                }
                self.emoji_start = None;
                self.mention_start = None;
            }
            Action::CloseMentions => self.mention_start = None,
            Action::SaveSticker(path) => {
                self.backend.send(Command::SaveSticker { path });
                self.toast("Sticker saved");
            }
            Action::HealSticker { path } => {
                self.backend.send(Command::HealSticker { path });
            }
            Action::HealStickerThumb { path } => {
                self.backend.send(Command::HealStickerThumb { path });
            }
            Action::ForgetSticker(path) => {
                self.backend.send(Command::ForgetSticker { path });
            }
            Action::PickStickerArchive => {
                self.sticker_import_pending = true;
                self.backend.send(Command::PickStickerArchive);
            }
            Action::DeleteStickerPack(dir) => {
                self.backend.send(Command::DeleteStickerPack { dir });
            }
            Action::FavoriteSticker(path) => {
                self.backend.send(Command::FavoriteSticker { path });
            }
            Action::ViewStickerPack { chat, message } => {
                self.backend
                    .send(Command::ViewStickerPack { chat, message });
            }
            Action::AddStickerPack { dir, name } => {
                self.backend.send(Command::AddStickerPack { dir, name });
            }
            Action::PeekSticker(path) => {
                self.dialog = Some(Dialog::PeekSticker { path });
            }
            Action::ShowFileInfo { chat, message } => {
                self.backend.send(Command::FileInfo { chat, message });
            }
            Action::CopyImage(path) => {
                self.backend.send(Command::CopyImage { path });
            }
            Action::SendSticker(path) => {
                if let Some(chat) = self.open_chat.clone() {
                    if self.send_blocked_toast(&chat) {
                        return;
                    }
                    let quoting = self.reply_to.take();
                    self.backend.send(Command::SendSticker {
                        chat,
                        path,
                        quoting,
                    });
                    self.dialog = None;
                    self.picker = None;
                    self.scroll_to_bottom = true;
                    self.at_bottom = true;
                    self.refocus_composer(ctx);
                }
            }
            Action::PasteImage {
                width,
                height,
                rgba,
            } => {
                // Stage the files so the user can add a caption.
                if let Some(chat) = self.open_chat.clone()
                    && !self.send_blocked_toast(&chat)
                {
                    self.pending.push(Pending::Picture {
                        width,
                        height,
                        rgba: std::sync::Arc::new(rgba),
                        texture: None,
                    });
                    self.focus_composer = true;
                }
            }
            Action::React {
                chat,
                message,
                emoji,
            } => self.backend.send(Command::React {
                chat,
                message,
                emoji,
            }),
            Action::SetArchived(chat, archived) => {
                if let Some(known) = self.chat_mut(&chat) {
                    known.archived = archived;
                }
                if archived && self.open_chat.as_deref() == Some(chat.as_str()) {
                    self.actions.push(Action::CloseChat);
                }
                self.backend.send(Command::SetArchived(chat, archived));
            }
            Action::SetPinned(chat, pinned) => {
                if let Some(known) = self.chat_mut(&chat) {
                    known.pinned = pinned;
                    known.pinned_at = if pinned {
                        jiff::Timestamp::now().as_millisecond()
                    } else {
                        0
                    };
                }
                self.backend.send(Command::SetPinned(chat, pinned));
            }
            Action::ShowDialog(dialog) => {
                self.emoji_start = None;
                self.mention_start = None;
                if matches!(&dialog, Dialog::CreatePoll(_)) && !self.poll_creating {
                    self.poll_draft = Default::default();
                }
                if matches!(&dialog, Dialog::Forward { .. }) {
                    self.forward_search.clear();
                    self.forward_to.clear();
                }
                if dialog == Dialog::PairWithPhone {
                    self.pair_phone.clear();
                }
                if dialog == Dialog::NewContact {
                    self.new_contact_phone.clear();
                    self.new_contact_name.clear();
                    self.new_contact_last.clear();
                    self.new_contact_pending = false;
                }
                self.contact_edit = None;
                self.dialog = Some(dialog);
            }
            Action::CloseDialog => {
                self.dialog = None;
                self.forward_search.clear();
                self.contact_edit = None;
                self.refocus_composer(ctx);
            }
            Action::EditContact(prefill) => {
                self.contact_edit = Some(crate::util::split_name(&prefill));
            }
            Action::SaveContact { id, first, last } => {
                self.contact_edit = None;
                let (full_name, first_name) = compose_name(&first, &last);
                let Some(full_name) = full_name else {
                    return;
                };
                self.backend.send(Command::SaveContact {
                    id,
                    full_name,
                    first_name,
                    to_phone: self.settings.save_contacts_to_phone,
                });
            }
            Action::NewContact { phone, first, last } => {
                self.new_contact_pending = true;
                let (full_name, first_name) = compose_name(&first, &last);
                self.backend.send(Command::NewContact {
                    phone,
                    full_name,
                    first_name,
                    to_phone: self.settings.save_contacts_to_phone,
                });
            }
            Action::ToggleSidebar => self.sidebar_visible = !self.sidebar_visible,
            Action::FocusSearch => {
                self.sidebar_visible = true;
                self.page = Page::Chats;
                self.focus_composer = false;
                self.focus_search = true;
                self.emoji_start = None;
                self.mention_start = None;
            }
            Action::FocusComposer => {
                self.focus_search = false;
                self.focus_composer = true;
            }
            Action::ScrollToBottom => self.scroll_to_bottom = true,
            Action::ScrollTo(id) => {
                self.scroll_to_bottom = false;
                crate::ui::conversation::flash_message(ctx, &id);
                let Some(chat) = self.open_chat.clone() else {
                    return;
                };
                let conversation = self.conversations.entry(chat.clone()).or_default();
                if conversation.message(&id).is_none()
                    && !conversation.loading_older
                    && let Some(oldest) = conversation.messages.first()
                {
                    // Load older archive pages toward the target.
                    conversation.loading_older = true;
                    self.backend.send(Command::LoadUntil {
                        chat,
                        id: id.clone(),
                        before: (oldest.timestamp, oldest.id.clone()),
                    });
                }
                self.scroll_anchor = Some(id);
            }
            Action::Search(text) => {
                self.search = text;
                let query = self.search.trim().to_owned();
                if query.is_empty() {
                    self.search_hits.clear();
                } else {
                    self.backend.send(Command::SearchMessages { query });
                }
            }
            Action::ShowUpdate => {
                self.show_update = self.update.is_some();
                self.inspect_update();
            }
            Action::CloseUpdate => self.show_update = false,
            Action::CheckUpdatesNow => {
                if self.update_checking {
                    return;
                }
                if self.backend.is_offline() {
                    self.toast("Connect to check for updates");
                    return;
                }
                self.update_checking = true;
                self.last_update_check = Some(Instant::now());
                self.backend.send(Command::CheckUpdatesNow);
            }
            Action::DownloadUpdate => self.download_update(),
            Action::InstallUpdate => {
                if matches!(
                    self.update_download,
                    crate::updates::DownloadState::Ready(_)
                ) {
                    let crate::updates::DownloadState::Ready(prepared) = std::mem::replace(
                        &mut self.update_download,
                        crate::updates::DownloadState::Installing,
                    ) else {
                        unreachable!()
                    };
                    self.backend.send(Command::InstallUpdate {
                        prepared,
                        arguments: self.update_arguments.clone(),
                    });
                }
            }
            Action::SetTheme(choice) => {
                self.settings.theme = choice;
                self.settings.custom_theme = None;
                self.settings.custom_theme_cache = None;
                self.mark_settings_dirty();
                self.apply_theme(ctx);
            }
            Action::SetCustomTheme(filename) => {
                if let Some(theme) = self.custom_themes.find(&filename) {
                    self.settings.custom_theme_cache = Some(theme.clone());
                    self.settings.custom_theme = Some(filename);
                    self.mark_settings_dirty();
                    self.apply_theme(ctx);
                }
            }
            Action::ReloadThemes => self.load_custom_themes(),
            Action::OpenThemesFolder => {
                let directory = self.dirs.config.join("themes");
                std::thread::spawn(move || {
                    if std::fs::create_dir_all(&directory).is_ok() {
                        let _ = open::that(directory);
                    }
                });
            }
            Action::HideShortcutHints => {
                self.settings.show_shortcut_hints = false;
                self.mark_settings_dirty();
            }
            Action::SettingsChanged => self.mark_settings_dirty(),
            Action::ZoomBy(delta) => {
                self.settings.zoom = (self.settings.zoom + delta).clamp(0.6, 2.0);
                self.zoom_applied = false;
                self.mark_settings_dirty();
            }
            Action::ResetZoom => {
                self.settings.zoom = 1.0;
                self.zoom_applied = false;
                self.mark_settings_dirty();
            }
            Action::PairWithPhone(phone) => {
                let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
                if digits.len() < 7 {
                    self.toast_error(
                        "Enter the phone number with its country code, using digits only",
                    );
                } else {
                    self.backend.send(Command::PairWithPhone(digits));
                }
            }
            Action::Unlink => {
                self.dialog = None;
                self.backend.send(Command::Unlink);
            }
            Action::Reconnect => self.backend.send(Command::Reconnect),
            Action::Quit => {
                self.quit_requested = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Action::ShowWindow => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                if self.window_hidden {
                    // The headless loop in `main` will create the window.
                    self.wants_show = true;
                } else {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                self.request_raise();
            }
            Action::HideWindow => {
                if self.tray.is_some() {
                    self.hide_intent = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            // Route through the configured window-close behavior.
            Action::CloseWindow => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        }
    }

    pub fn toast(&mut self, message: impl Into<String>) {
        self.toasts.push(Toast {
            message: message.into(),
            kind: ToastKind::Info,
            created: Instant::now(),
        });
        self.toasts.truncate(4);
    }

    pub fn toast_error(&mut self, message: impl Into<String>) {
        let message = message.into();
        log::warn!("{message}");
        self.toasts.push(Toast {
            message,
            kind: ToastKind::Error,
            created: Instant::now(),
        });
    }

    /// Processes app state shared by windowed and headless modes.
    pub fn background_frame(&mut self, ctx: &egui::Context) {
        // Events are drained before frame_ui observes focus. Losing focus in
        // this frame must take effect before an incoming chat update can read it.
        if self.window_hidden || ctx.input(|input| input.viewport().focused) == Some(false) {
            self.window_focused = false;
        }
        self.handle_tray();
        #[cfg(target_os = "macos")]
        self.actions
            .extend(crate::macos::drain(ctx, self.window_hidden));
        self.handle_control_commands();
        self.poll_custom_themes();
        self.handle_notification_opens();
        self.handle_events();
        self.tick(ctx);
        self.tick_audio();
        self.retry_missing_pages();
        self.apply_actions(ctx);
    }

    /// Polls audio state and schedules repaints while it changes.
    fn tick_audio(&mut self) {
        if let Err(error) = self.player.poll() {
            self.toast_error(error);
        }
        // The answer is taken either way: a clip that ended while the setting
        // was off must not start playing when it is turned back on.
        if let Some(finished) = self.player.take_finished()
            && self.settings.play_next_audio
        {
            self.play_next_audio(&finished);
        }
        if let Some(error) = self.recording.as_ref().and_then(Recorder::failure) {
            self.recording = None;
            self.toast_error(format!("Could not record: {error}"));
        }
        if self.player.is_playing() || self.recording.is_some() {
            self.waker.wake_after(Duration::from_millis(40));
        }
    }

    /// Plays or pauses audio and sends the first played receipt when needed.
    fn play_voice(&mut self, message: String, path: PathBuf) {
        // A voice note and a video never play over each other.
        self.video.stop();
        if let Err(error) = self.player.toggle(&message, &path) {
            self.toast_error(error);
            return;
        }
        self.tell_played(message);
    }

    /// Continues with the next audio message that has a file, like the
    /// phone's voice notes: playback walks forward only, one clip at a time,
    /// and text in between is passed over.
    fn play_next_audio(&mut self, finished: &str) {
        let Some(chat) = self.open_chat.clone() else {
            return;
        };
        let next = self
            .conversations
            .get(&chat)
            .and_then(|conversation| next_audio_after(&conversation.messages, finished));
        if let Some((message, path)) = next {
            self.play_voice(message, path);
        }
    }

    fn tell_played(&mut self, message: String) {
        let Some(chat) = self.open_chat.clone() else {
            return;
        };
        if self.played_told.contains(&message) {
            return;
        }
        let Some(row) = self
            .conversations
            .get(&chat)
            .and_then(|conversation| conversation.message(&message))
        else {
            return;
        };
        if row.from_me {
            return;
        }
        let sender = row.sender.clone();
        self.played_told.insert(message.clone());
        self.backend.send(Command::MarkPlayed {
            chat,
            message,
            sender,
            receipts: self.settings.send_read_receipts,
        });
    }

    /// Stops and sends a recording unless it is under one second.
    fn send_recording(&mut self) {
        let Some(recorder) = self.recording.take() else {
            return;
        };
        let Some(chat) = self.open_chat.clone() else {
            return;
        };
        if self.send_blocked_toast(&chat) {
            return;
        }
        match recorder.finish() {
            Ok(samples) if samples.len() < crate::voice::RATE as usize / 2 => {}
            Ok(samples) => {
                let quoting = self.reply_to.take();
                self.backend.send(Command::SendVoice {
                    chat,
                    samples,
                    quoting,
                });
            }
            Err(error) => self.toast_error(format!("Could not record: {error}")),
        }
    }

    pub fn frame_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.copy_rows
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        *self
            .selection_view
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
        self.apply_theme(ctx);
        let focused = ctx.input(|input| input.viewport().focused.unwrap_or(true));
        let regained_focus = focused && !self.window_focused;
        // Mark messages received while hidden as read on window return.
        if regained_focus
            && self.page == Page::Chats
            && let Some(open) = self.open_chat.clone()
            && self.chat(&open).is_some_and(|chat| chat.unread > 0)
        {
            self.mark_read(&open);
        }
        if regained_focus {
            self.refocus_composer(ctx);
        }
        self.window_focused = focused;
        // Close the window and continue headless when background mode is enabled.
        if ctx.input(|input| input.viewport().close_requested())
            && !self.quit_requested
            && self.hides_to_tray()
        {
            self.hide_intent = true;
        }
        self.lock_scroll_axis(ctx);
        self.take_drops_and_pastes(ctx);
        crate::ui::show(self, ui);
        self.apply_actions(ctx);
        // Release the image caches of everything that scrolled away.
        crate::image_cache::sweep(ctx);
        if !self.toasts.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }

    /// Inserts text at the composer cursor or end.
    fn insert_in_composer(&mut self, ctx: &egui::Context, text: &str) {
        let id = egui::Id::new("composer-text");
        let at = egui::TextEdit::load_state(ctx, id)
            .and_then(|state| state.cursor.char_range())
            .map(|range| range.primary.index.0)
            .unwrap_or_else(|| self.composer.chars().count());
        let at = at.min(self.composer.chars().count());
        let byte = self
            .composer
            .char_indices()
            .nth(at)
            .map_or(self.composer.len(), |(byte, _)| byte);
        self.composer.insert_str(byte, text);
        self.set_composer_cursor(ctx, at + text.chars().count());
    }

    fn set_composer_cursor(&self, ctx: &egui::Context, at: usize) {
        let id = egui::Id::new("composer-text");
        if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(at),
                )));
            egui::TextEdit::store_state(ctx, id, state);
        }
    }

    fn remember_emoji(&mut self, emoji: &str) {
        self.settings.recent_emoji.retain(|known| known != emoji);
        self.settings.recent_emoji.insert(0, emoji.to_owned());
        self.settings.recent_emoji.truncate(36);
        self.mark_settings_dirty();
    }

    /// Handles dropped files and pasted images for the open chat.
    fn take_drops_and_pastes(&mut self, ctx: &egui::Context) {
        let (dropped, hovering) = ctx.input(|input| {
            let dropped: Vec<PathBuf> = input
                .raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .collect();
            let hovering = !input.raw.hovered_files.is_empty();
            (dropped, hovering)
        });
        self.dropping = hovering && self.open_chat.is_some();
        if !dropped.is_empty() {
            self.actions.push(Action::SendFiles(dropped));
        }
        self.take_image_paste(ctx, clipboard_image);
    }

    fn take_image_paste(
        &mut self,
        ctx: &egui::Context,
        mut read_image: impl FnMut() -> Option<(usize, usize, Vec<u8>)>,
    ) {
        // Fold the frame events in delivery order: a release that shares its
        // frame with the next Paste must still see the armed flag, and a
        // Paste must see the Ctrl state from before its own frame churn.
        // Only origins and counts are decided here; the clipboard is read
        // once below, and only when an intent survived the guards.
        let (intents, focused) = ctx.input(|input| {
            let mut intents: u32 = 0;
            let mut armed = self.paste_armed;
            let mut typed = self.paste_typed_v;
            let mut ctrl = self.paste_ctrl_held;
            if !input.focused {
                // A gesture cannot span a focus loss: modifiers may have been
                // released outside, so no release belongs to an old Paste.
                self.paste_armed = false;
                self.paste_typed_v = false;
                self.paste_ctrl_held = false;
                return (0, false);
            }
            for event in &input.events {
                match event {
                    egui::Event::ModifiersChanged(modifiers) => {
                        ctrl = modifiers.command;
                    }
                    egui::Event::Key {
                        key: egui::Key::V,
                        pressed: true,
                        modifiers,
                        ..
                    } if !modifiers.command => {
                        // A V press without command is typing: the
                        // integration consumes every Ctrl+V press, so a
                        // plain press always reaches the field as text. A
                        // later bare release ends this typing, never a
                        // paste. Ctrl-first releases lost their command
                        // by now; the missing typed V tells them apart.
                        typed = true;
                    }
                    egui::Event::Paste(_) => {
                        // A new gesture epoch: earlier typing cannot leak
                        // into it. The arm is optimistic inside this fold
                        // so a release sharing the frame still suppresses.
                        intents += 1;
                        typed = false;
                        armed = ctrl;
                    }
                    egui::Event::Key {
                        key: egui::Key::V,
                        pressed: false,
                        ..
                    } => {
                        let suppress = armed;
                        armed = false;
                        let was_typed = typed;
                        typed = false;
                        if !suppress && !was_typed {
                            intents += 1;
                        }
                    }
                    egui::Event::Key { modifiers, .. } => {
                        ctrl = modifiers.command;
                    }
                    _ => {}
                }
            }
            self.paste_armed = armed;
            self.paste_typed_v = typed;
            self.paste_ctrl_held = ctrl;
            (intents, true)
        });
        // Handle image paste only when the composer or no field has focus.
        let composing = ctx.memory(|memory| {
            memory.has_focus(egui::Id::new("composer-text")) || memory.focused().is_none()
        });
        // Decide an image paste before the composer TextEdit sees the
        // gesture: an accepted image consumes the textual Paste event so
        // only the attachment lands. Anything else flows through untouched.
        if intents > 0
            && focused
            && composing
            && let Some(chat) = self.open_chat.clone()
            && !self.send_blocked(&chat)
            && let Some(image) = read_image()
        {
            // A browser can offer both pixels and its source URL. Consume the
            // text before the composer sees it, keeping any existing caption.
            ctx.input_mut(|input| {
                input
                    .events
                    .retain(|event| !matches!(event, egui::Event::Paste(_)))
            });
            for _ in 0..intents {
                self.actions.push(Action::PasteImage {
                    width: image.0,
                    height: image.1,
                    rgba: image.2.clone(),
                });
            }
        }
    }

    /// Locks trackpad scrolling to one axis, scales Linux deltas, and adds glide.
    fn lock_scroll_axis(&mut self, ctx: &egui::Context) {
        let (raw, from_trackpad, ended) = ctx.input(|input| {
            let mut sum = egui::Vec2::ZERO;
            let mut pointish = false;
            let mut ended = false;
            for event in &input.events {
                if let egui::Event::MouseWheel {
                    unit, delta, phase, ..
                } = event
                {
                    sum += *delta;
                    pointish |= *unit == egui::MouseWheelUnit::Point;
                    ended |= matches!(phase, egui::TouchPhase::End | egui::TouchPhase::Cancel);
                }
            }
            (sum, pointish, ended)
        });
        let now = Instant::now();
        if raw != egui::Vec2::ZERO {
            self.scroll_from_trackpad = from_trackpad;
        }
        let trackpad_here = cfg!(target_os = "linux") && self.scroll_from_trackpad;
        if trackpad_here {
            ctx.input_mut(|input| input.smooth_scroll_delta *= TRACKPAD_SCALE);
        }
        if trackpad_here && raw != egui::Vec2::ZERO {
            self.glide = None;
            self.scroll_accum += raw * TRACKPAD_SCALE;
            self.scroll_history
                .add(ctx.input(|input| input.time), self.scroll_accum);
            self.scroll_last_event = Some(now);
            ctx.request_repaint_after(Duration::from_millis(60));
        } else if raw != egui::Vec2::ZERO || ctx.input(|input| input.pointer.any_down()) {
            self.glide = None;
            self.scroll_history.clear();
            self.scroll_last_event = None;
        }
        let quiet = self
            .scroll_last_event
            .is_some_and(|at| now.duration_since(at).as_secs_f32() > 0.15);
        if ended || quiet {
            let mut velocity = self.scroll_history.velocity().unwrap_or(egui::Vec2::ZERO);
            if let Some((axis, _)) = self.scroll_lock {
                match axis {
                    ScrollAxis::Horizontal => velocity.y = 0.0,
                    ScrollAxis::Vertical => velocity.x = 0.0,
                }
            }
            self.glide = (velocity.length() > GLIDE_START).then_some(velocity);
            self.scroll_history.clear();
            self.scroll_accum = egui::Vec2::ZERO;
            self.scroll_last_event = None;
        }
        if let Some(velocity) = self.glide {
            if raw == egui::Vec2::ZERO {
                let dt = ctx.input(|input| input.stable_dt).clamp(0.001, 0.05);
                ctx.input_mut(|input| input.smooth_scroll_delta += velocity * dt);
                let slower = velocity * (-dt / GLIDE_DECAY).exp();
                self.glide = (slower.length() > GLIDE_STOP).then_some(slower);
            }
            ctx.request_repaint();
        }
        let held = self
            .scroll_lock
            .filter(|(_, at)| now.duration_since(*at) < SCROLL_GESTURE_GAP)
            .map(|(axis, _)| axis);
        let moved = raw != egui::Vec2::ZERO;
        let axis = match held {
            Some(axis) => axis,
            None if moved && raw.x.abs() > raw.y.abs() * 1.2 => ScrollAxis::Horizontal,
            None if moved => ScrollAxis::Vertical,
            None => {
                self.scroll_lock = None;
                return;
            }
        };
        if moved {
            self.scroll_lock = Some((axis, now));
        }
        ctx.input_mut(|input| match axis {
            ScrollAxis::Horizontal => input.smooth_scroll_delta.y = 0.0,
            ScrollAxis::Vertical => input.smooth_scroll_delta.x = 0.0,
        });
    }

    pub fn save_state(&mut self) {
        if self.settings_dirty {
            self.save_settings();
        }
    }

    pub fn shutdown(&mut self) {
        self.save_state();
        self.backend.shutdown();
    }

    /// Returns attachment state for a loaded message.
    pub fn media_of(&self, chat: &str, id: &str) -> Option<&Media> {
        self.conversations.get(chat)?.message(id)?.content.media()
    }
}

/// Detects paste from the key release. egui consumes the press and emits a
/// `Paste` event only for text, so image paste has no key-press event.
/// Builds WhatsApp's full and short contact names. A first name is required.
fn compose_name(first: &str, last: &str) -> (Option<String>, Option<String>) {
    let first = first.trim();
    let last = last.trim();
    if first.is_empty() && last.is_empty() {
        return (None, None);
    }
    let full = if last.is_empty() {
        first.to_owned()
    } else if first.is_empty() {
        last.to_owned()
    } else {
        format!("{first} {last}")
    };
    let short = (!first.is_empty()).then(|| first.to_owned());
    (Some(full), short)
}

fn contains_mention_token(text: &str, user: &str) -> bool {
    let token = format!("@{user}");
    let mut rest = text;
    while let Some(at) = rest.find(&token) {
        let after = &rest[at + token.len()..];
        if after
            .chars()
            .next()
            .is_none_or(|character| !character.is_ascii_digit())
        {
            return true;
        }
        rest = &rest[at + 1..];
    }
    false
}

fn find_named_mention(text: &str, token: &str) -> Option<usize> {
    text.match_indices(token).find_map(|(at, _)| {
        let after = &text[at + token.len()..];
        after
            .chars()
            .next()
            .is_none_or(|character| !character.is_alphanumeric())
            .then_some(at)
    })
}

fn mention_refs(ids: &[String]) -> Vec<crate::model::MentionRef> {
    ids.iter()
        .filter_map(|id| {
            let user = id.split('@').next()?.to_owned();
            (!user.is_empty()).then(|| crate::model::MentionRef {
                user,
                id: id.clone(),
            })
        })
        .collect()
}

/// Whether the clipboard asks for a paste: either the integration delivered
/// a Paste event (text, or image+text whose Ctrl+V press was consumed) or
/// the Ctrl+V key release arrived. Image-only presses carry no Paste event;
/// they attach on the release below instead.
pub fn wants_paste(input: &egui::InputState) -> bool {
    input.events.iter().any(|event| {
        matches!(event, egui::Event::Paste(_))
            || matches!(
                event,
                egui::Event::Key {
                    key: egui::Key::V,
                    pressed: false,
                    modifiers,
                    ..
                } if modifiers.command
            )
    })
}

/// The next audio message with a file after the one that just finished.
fn next_audio_after(messages: &[Message], finished: &str) -> Option<(String, PathBuf)> {
    let position = messages.iter().position(|message| message.id == finished)?;
    messages[position + 1..]
        .iter()
        .find_map(|message| match &message.content {
            Content::Audio { media, .. } => media
                .path
                .as_ref()
                .map(|path| (message.id.clone(), path.clone())),
            _ => None,
        })
}

/// Clipboard image as width, height, and straight-alpha RGBA.
fn clipboard_image() -> Option<(usize, usize, Vec<u8>)> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let image = clipboard.get_image().ok()?;
    if image.width == 0 || image.height == 0 {
        return None;
    }
    Some((image.width, image.height, image.bytes.into_owned()))
}

impl Delivery {
    /// Whether an outgoing message is still pending.
    pub fn in_flight(self) -> bool {
        matches!(self, Delivery::Pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Content;

    fn app() -> App {
        let root = std::env::temp_dir().join(format!("zapfast-app-{}", std::process::id()));
        App::headless(AppDirs::under(&root), Settings::default()).0
    }

    #[test]
    fn the_viewer_walks_the_pictures_that_are_on_disk() {
        let dir = std::env::temp_dir().join(format!("zapfast-viewer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let chat = "1@s.whatsapp.net";
        let mut app = app();
        let ctx = egui::Context::default();
        app.chats.push(Chat::new(chat.into(), "Ada".into()));
        let first = dir.join("first.png");
        let second = dir.join("second.png");
        std::fs::write(&first, b"png").expect("writes");
        std::fs::write(&second, b"png").expect("writes");
        let missing = dir.join("gone.png");
        let media = |path: PathBuf| Media {
            mime: "image/png".to_owned(),
            size: 3,
            width: Some(10),
            height: Some(10),
            path: Some(path),
            state: MediaState::Idle,
        };
        let mut photo = message(chat, "first", 10);
        photo.content = Content::Image {
            caption: None,
            media: media(first.clone()),
        };
        let mut sticker = message(chat, "second", 20);
        sticker.content = Content::Sticker {
            media: media(second.clone()),
            animated: false,
        };
        // An image that arrived as a document still opens in the viewer.
        let mut sent_png = message(chat, "png-doc", 25);
        sent_png.content = Content::Document {
            media: Media {
                mime: "image/png".to_owned(),
                size: 3,
                width: None,
                height: None,
                path: Some(second.clone()),
                state: MediaState::Idle,
            },
            file_name: "shot.png".to_owned(),
            caption: None,
            pages: Some(0),
        };
        let mut broken = message(chat, "third", 30);
        broken.content = Content::Image {
            caption: None,
            media: media(missing),
        };
        let clip = dir.join("clip.mp4");
        std::fs::write(&clip, b"mp4").expect("writes");
        let mut video = message(chat, "fourth", 40);
        video.content = Content::Video {
            caption: None,
            media: Media {
                mime: "video/mp4".to_owned(),
                size: 5,
                width: Some(480),
                height: Some(850),
                path: Some(clip),
                state: MediaState::Idle,
            },
            seconds: Some(64),
            gif: false,
        };
        let mut gif = message(chat, "fifth", 50);
        gif.content = Content::Video {
            caption: None,
            media: Media {
                mime: "video/mp4".to_owned(),
                size: 5,
                width: Some(480),
                height: Some(850),
                path: Some(second.clone()),
                state: MediaState::Idle,
            },
            seconds: Some(6),
            gif: true,
        };
        app.conversations.insert(
            chat.into(),
            Conversation {
                requested: true,
                complete: true,
                messages: vec![photo, sticker, sent_png, broken, video, gif],
                ..Default::default()
            },
        );

        app.apply(
            Action::OpenViewer {
                chat: chat.into(),
                message: "first".into(),
            },
            &ctx,
        );
        let viewer = app.viewer.as_ref().expect("the viewer opens");
        assert_eq!(
            viewer.items.len(),
            3,
            "a sticker and a GIF stay out of the viewer and a file that is gone is not offered"
        );
        assert_eq!(viewer.index, 0);
        assert_eq!(viewer.items[0].kind, ViewerKind::Picture);
        assert_eq!(viewer.items[2].kind, ViewerKind::Video);

        // Walking stops at either end and a new picture starts fitted.
        app.apply(Action::ViewerStep(-1), &ctx);
        assert_eq!(app.viewer.as_ref().unwrap().index, 0);
        app.apply(
            Action::ViewerZoom {
                factor: 4.0,
                anchor: (50.0, 0.0),
            },
            &ctx,
        );
        assert!(app.viewer.as_ref().unwrap().zoom > 1.0);
        app.apply(Action::ViewerStep(1), &ctx);
        let viewer = app.viewer.as_ref().unwrap();
        assert_eq!(viewer.index, 1);
        assert_eq!(viewer.zoom, 1.0);
        assert_eq!(viewer.offset, (0.0, 0.0));
        app.apply(Action::ViewerStep(1), &ctx);
        assert_eq!(
            app.viewer.as_ref().unwrap().index,
            2,
            "walking stops at the last item"
        );

        // The zoom stays between its limits.
        app.apply(
            Action::ViewerZoom {
                factor: 1_000.0,
                anchor: (0.0, 0.0),
            },
            &ctx,
        );
        assert_eq!(app.viewer.as_ref().unwrap().zoom, Viewer::MAX_ZOOM);
        app.apply(
            Action::ViewerZoom {
                factor: 0.000_1,
                anchor: (0.0, 0.0),
            },
            &ctx,
        );
        assert_eq!(app.viewer.as_ref().unwrap().zoom, Viewer::MIN_ZOOM);
        app.apply(Action::ViewerFit, &ctx);
        assert_eq!(app.viewer.as_ref().unwrap().zoom, 1.0);

        // Closing a chat takes the viewer with it.
        app.apply(Action::CloseChat, &ctx);
        assert!(app.viewer.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn opening_the_picker_remembers_its_tab() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.apply(Action::TogglePicker(PickerTab::Stickers), &ctx);
        assert_eq!(app.picker, Some(PickerTab::Stickers));
        assert_eq!(app.settings.picker_tab, PickerTab::Stickers);
        // Closing keeps the memory; reopening returns to it.
        app.apply(Action::TogglePicker(PickerTab::Stickers), &ctx);
        assert_eq!(app.picker, None);
        assert_eq!(app.settings.picker_tab, PickerTab::Stickers);
        // The choice survives a settings round trip; old files open on emoji.
        let back: Settings =
            serde_json::from_str(&serde_json::to_string(&app.settings).unwrap()).unwrap();
        assert_eq!(back.picker_tab, PickerTab::Stickers);
        let old: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(old.picker_tab, PickerTab::Emoji);
    }

    #[test]
    fn selection_toggles_and_clears() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.open_chat = Some("c".to_owned());
        app.apply(Action::ToggleSelect("m1".into()), &ctx);
        app.apply(Action::ToggleSelect("m2".into()), &ctx);
        assert_eq!(app.selected, vec!["m1".to_owned(), "m2".to_owned()]);
        app.apply(Action::ToggleSelect("m1".into()), &ctx);
        assert_eq!(app.selected, vec!["m2".to_owned()]);
        app.apply(Action::ClearSelection, &ctx);
        assert!(app.selected.is_empty());
        app.apply(Action::ToggleSelect("m1".into()), &ctx);
        app.apply(Action::CloseChat, &ctx);
        assert!(app.selected.is_empty());
    }

    #[test]
    fn deleting_many_splits_revocable_from_local_only() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.open_chat = Some("c".to_owned());
        let now = crate::util::now();
        let mut fresh = message("c", "fresh", now - 60);
        fresh.from_me = true;
        let mut old = message("c", "old", now - REVOKE_WINDOW.as_secs() as i64 - 60);
        old.from_me = true;
        let incoming = message("c", "incoming", now - 60);
        app.conversations
            .entry("c".to_owned())
            .or_default()
            .messages = vec![fresh, old, incoming];
        app.apply(
            Action::DeleteMany {
                ids: vec!["fresh".into(), "old".into(), "incoming".into()],
                for_everyone: true,
            },
            &ctx,
        );
        let conversation = app.conversations.get("c").expect("chat");
        assert!(
            conversation
                .messages
                .iter()
                .any(|row| row.id == "fresh" && matches!(row.content, Content::Revoked))
        );
        assert!(
            !conversation
                .messages
                .iter()
                .any(|row| row.id == "old" || row.id == "incoming")
        );
        assert!(app.selected.is_empty());
        assert!(app.dialog.is_none());
    }

    #[test]
    fn manual_update_check_reports_all_outcomes() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, events) =
            App::headless(AppDirs::under(directory.path()), Settings::default());
        let ctx = egui::Context::default();
        app.apply(Action::CheckUpdatesNow, &ctx);
        // Offline: tapping explains instead of spinning forever.
        assert!(!app.update_checking);
        assert!(
            app.toasts
                .iter()
                .any(|toast| toast.message.contains("Connect to check"))
        );
        // Pretend a check is in flight; every worker answer resolves it.
        app.update_checking = true;
        events.send(Event::UpdateUpToDate).unwrap();
        app.background_frame(&ctx);
        assert!(!app.update_checking);
        assert!(
            app.toasts
                .iter()
                .any(|toast| toast.message.contains("latest version"))
        );
        app.update_checking = true;
        events
            .send(Event::UpdateCheckFailed("offline".to_owned()))
            .unwrap();
        app.background_frame(&ctx);
        assert!(!app.update_checking);
        assert!(
            app.toasts
                .iter()
                .any(|toast| matches!(toast.kind, ToastKind::Error))
        );
        app.update_checking = true;
        events
            .send(Event::UpdateAvailable {
                version: "9.9.9".into(),
                url: "https://example.com".into(),
            })
            .unwrap();
        app.background_frame(&ctx);
        assert!(!app.update_checking);
        assert_eq!(
            app.update.as_ref().map(|notice| notice.version.as_str()),
            Some("9.9.9")
        );
    }

    #[test]
    fn sending_a_sticker_closes_its_confirm_dialog() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.open_chat = Some("chat".to_owned());
        app.dialog = Some(Dialog::ConfirmSticker {
            path: std::path::PathBuf::from("sticker.webp"),
        });
        app.apply(
            Action::SendSticker(std::path::PathBuf::from("sticker.webp")),
            &ctx,
        );
        assert!(app.dialog.is_none());
        assert_eq!(app.picker, None);
    }

    #[test]
    fn failed_poll_requests_keep_the_draft_and_clear_pending_controls() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, events) =
            App::headless(AppDirs::under(directory.path()), Settings::default());
        let ctx = egui::Context::default();
        let draft = crate::model::PollDraft {
            question: "Lunch?".into(),
            options: vec!["Pizza".into(), "Pasta".into()],
            multiple: false,
        };
        app.dialog = Some(Dialog::CreatePoll("chat".into()));
        app.poll_draft = draft.clone();
        app.apply(
            Action::CreatePoll {
                chat: "chat".into(),
                draft: draft.clone(),
            },
            &ctx,
        );
        assert!(app.poll_creating);
        events
            .send(Event::PollCreated {
                chat: "chat".into(),
                error: Some("Could not send".into()),
            })
            .unwrap();
        app.background_frame(&ctx);
        assert!(!app.poll_creating);
        assert_eq!(app.poll_draft, draft);
        assert!(app.dialog.is_some());
        app.apply(
            Action::VotePoll {
                chat: "chat".into(),
                message: "poll".into(),
                choices: vec![0],
            },
            &ctx,
        );
        assert_eq!(app.poll_voting.len(), 1);
        events
            .send(Event::PollVoted {
                chat: "chat".into(),
                message: "poll".into(),
                error: Some("Could not vote".into()),
            })
            .unwrap();
        app.background_frame(&ctx);
        assert!(app.poll_voting.is_empty());
    }

    #[test]
    fn follow_system_retains_the_os_theme_between_platform_events() {
        let mut app = app();
        app.settings.theme = ThemeChoice::System;
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        for theme in [egui::Theme::Light, egui::Theme::Dark] {
            input.system_theme = Some(theme);
            // Native input preserves the OS preference when taking each frame.
            for _ in 0..2 {
                let mut output = ctx.run_ui(input.take(), |_| app.apply_theme(&ctx));
                output.textures_delta.clear();
                assert_eq!(app.palette.dark, theme == egui::Theme::Dark);
                assert_eq!(ctx.theme(), theme);
            }
        }
    }

    #[test]
    fn custom_theme_cache_survives_a_missing_file_and_follows_system_updates() {
        use crate::theme::custom::{Catalog, CustomTheme};
        let mut app = app();
        let ctx = egui::Context::default();
        let mut first = CustomTheme {
            filename: "mine.json".into(),
            palette: Palette::dark(),
        };
        first.palette.accent = egui::Color32::RED;
        app.custom_themes = Catalog::from_themes(vec![first.clone()]);
        app.apply(Action::SetCustomTheme(first.filename.clone()), &ctx);
        assert_eq!(app.palette.accent, egui::Color32::RED);
        // Cached selection remains usable while the file is temporarily missing.
        app.custom_themes = Catalog::default();
        app.settings =
            serde_json::from_str(&serde_json::to_string(&app.settings).unwrap()).unwrap();
        app.apply_theme(&ctx);
        assert_eq!(app.palette, first.palette);
        app.apply(Action::SetTheme(ThemeChoice::System), &ctx);
        assert!(app.settings.custom_theme.is_none());
        let mut system = first;
        system.filename = "omarchy.json".into();
        system.palette.accent = egui::Color32::GREEN;
        app.custom_themes
            .load_system_test(Some(system.clone()), true);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.settings.system_theme_cache.as_ref() != Some(&system) {
            app.poll_custom_themes();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        app.apply_theme(&ctx);
        assert_eq!(app.palette.accent, egui::Color32::GREEN);
        app.apply(Action::SetTheme(ThemeChoice::Light), &ctx);
        assert_eq!(app.palette, Palette::light());
    }

    #[test]
    fn automatic_updates_require_opt_in_and_explicit_restart() {
        use crate::updates::{
            DownloadState,
            install::{Installation, Kind, Prepared},
        };
        let mut app = app();
        let ctx = egui::Context::default();
        app.update = Some(crate::updates::Release {
            version: "99.0.0".into(),
            url: "https://github.com/crmne/zapfast/releases/latest".into(),
        });
        app.update_support = Some(Err("Use your package manager".into()));
        app.settings.download_updates_automatically = true;
        app.maybe_download_update();
        assert!(matches!(app.update_download, DownloadState::Idle));
        let installation = Installation {
            executable: PathBuf::from("/fixture/zapfast"),
            kind: Kind::Portable,
        };
        app.update_support = Some(Ok(installation.clone()));
        app.settings.download_updates_automatically = false;
        app.maybe_download_update();
        assert!(matches!(app.update_download, DownloadState::Idle));
        app.settings.download_updates_automatically = true;
        app.maybe_download_update();
        assert!(matches!(
            app.update_download,
            DownloadState::Downloading { .. }
        ));
        app.update_download = DownloadState::Ready(Box::new(Prepared {
            installation,
            directory: "/fixture/staging".into(),
            payload: "/fixture/staging/next".into(),
            sha256: String::new(),
            version: "99.0.0".into(),
        }));
        app.maybe_download_update();
        assert!(matches!(app.update_download, DownloadState::Ready(_)));
        assert!(!app.quit_requested);
        app.apply(Action::InstallUpdate, &ctx);
        assert!(matches!(app.update_download, DownloadState::Installing));
        assert!(!app.quit_requested, "wait for the helper before closing");
    }

    #[test]
    fn a_closed_window_does_not_read_new_messages_in_the_last_chat() {
        let mut app = app();
        let mut chat = Chat::new("peer@s.whatsapp.net".into(), "Peer".into());
        app.open_chat = Some(chat.id.clone());
        app.window_focused = true;
        app.window_gone();
        assert!(!app.window_focused);
        chat.unread = 2;
        app.handle_chat_updated(chat.clone());
        assert_eq!(app.chat(&chat.id).unwrap().unread, 2);
        // Focus left over from a window callback is insufficient while hidden.
        app.window_focused = true;
        app.handle_chat_updated(chat.clone());
        assert_eq!(app.chat(&chat.id).unwrap().unread, 2);
        app.window_hidden = false;
        app.handle_chat_updated(chat.clone());
        assert_eq!(app.chat(&chat.id).unwrap().unread, 0);
    }

    #[test]
    fn losing_focus_takes_effect_before_processing_an_incoming_chat_update() {
        let root = std::env::temp_dir().join("zapfast-focus-test");
        let (mut app, events) = App::headless(AppDirs::under(&root), Settings::default());
        let mut chat = Chat::new("peer@s.whatsapp.net".into(), "Peer".into());
        chat.unread = 1;
        app.open_chat = Some(chat.id.clone());
        app.window_focused = true;
        events
            .send(Event::ChatUpdated(Box::new(chat.clone())))
            .unwrap();
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .focused = Some(false);
        let mut output = ctx.run_ui(input, |ui| app.background_frame(ui.ctx()));
        output.textures_delta.clear();
        assert_eq!(app.chat(&chat.id).unwrap().unread, 1);
    }

    #[test]
    fn read_receipt_preference_applies_to_both_reading_and_voice_playback() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let chat = "peer@s.whatsapp.net";
        app.open_chat = Some(chat.into());
        app.conversations
            .entry(chat.into())
            .or_default()
            .merge(vec![message(chat, "voice", 100)], false);
        app.settings.send_read_receipts = false;
        app.mark_read(chat);
        assert!(matches!(
            commands.try_recv().unwrap(),
            Command::MarkRead {
                receipts: false,
                ..
            }
        ));
        app.tell_played("voice".into());
        assert!(matches!(
            commands.try_recv().unwrap(),
            Command::MarkPlayed {
                receipts: false,
                ..
            }
        ));
        app.settings.send_read_receipts = true;
        app.played_told.clear();
        app.tell_played("voice".into());
        assert!(matches!(
            commands.try_recv().unwrap(),
            Command::MarkPlayed { receipts: true, .. }
        ));
    }

    fn message(chat: &str, id: &str, timestamp: i64) -> Message {
        Message {
            id: id.into(),
            chat: chat.into(),
            sender: chat.into(),
            sender_name: None,
            from_me: false,
            timestamp,
            content: Content::text(id),
            status: Delivery::None,
            delivered_at: None,
            read_at: None,
            quoted: None,
            reactions: Vec::new(),
            edited: false,
            mentions: Vec::new(),
            forwarded: false,
            thumbnail: None,
        }
    }

    #[test]
    fn conversations_merge_pages_without_duplicates() {
        let mut conversation = Conversation::default();
        conversation.merge(vec![message("c", "b", 2), message("c", "c", 3)], false);
        conversation.merge(vec![message("c", "a", 1), message("c", "b", 2)], true);
        let ids: Vec<&str> = conversation
            .messages
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn a_rendered_pdf_page_reaches_the_viewer_and_stale_ones_are_dropped() {
        let dir = std::env::temp_dir().join(format!("zapfast-pdf-view-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let file = dir.join("notes.pdf");
        std::fs::write(&file, b"%PDF-1.4").expect("writes");
        let chat = "1@s.whatsapp.net";
        let home = tempfile::tempdir().unwrap();
        let (mut app, events) = App::headless(AppDirs::under(home.path()), Settings::default());
        let ctx = egui::Context::default();
        app.chats.push(Chat::new(chat.into(), "Ada".into()));
        let mut row = message(chat, "doc", 10);
        row.content = Content::Document {
            media: Media {
                mime: "application/pdf".to_owned(),
                size: 8,
                width: None,
                height: None,
                path: Some(file.clone()),
                state: MediaState::Idle,
            },
            file_name: "notes.pdf".to_owned(),
            caption: None,
            pages: None,
        };
        app.conversations.insert(
            chat.into(),
            Conversation {
                requested: true,
                complete: true,
                messages: vec![row],
                ..Default::default()
            },
        );

        app.apply(
            Action::OpenViewer {
                chat: chat.into(),
                message: "doc".into(),
            },
            &ctx,
        );
        let viewer = app.viewer.as_ref().expect("the viewer opens");
        assert_eq!(viewer.items[0].kind, ViewerKind::Pdf);
        assert_eq!(viewer.pdf_page, 0);

        // The worker answers the request that is in flight.
        let page = |page: usize, pages: usize, width: u32| crate::pdf::Page {
            page,
            pages,
            width,
            height: width * 2,
            rgba: vec![0; 8],
        };
        app.pdf_rendering = Some((file.clone(), 0, 900));
        events
            .send(Event::PdfPage {
                path: file.clone(),
                page: 0,
                width: 900,
                result: Ok(page(0, 3, 900)),
            })
            .unwrap();
        app.handle_events();
        assert_eq!(app.pdf_page.as_ref().map(|page| page.width), Some(900));
        assert!(app.pdf_rendering.is_none());
        assert_eq!(app.viewer.as_ref().unwrap().pdf_pages, 3);

        // An answer for a width nobody waits for is ignored.
        events
            .send(Event::PdfPage {
                path: file.clone(),
                page: 1,
                width: 200,
                result: Ok(page(1, 3, 200)),
            })
            .unwrap();
        app.handle_events();
        assert_eq!(app.pdf_page.as_ref().map(|page| page.page), Some(0));

        // Pages stop at either end.
        app.apply(Action::ViewerPage(1), &ctx);
        assert_eq!(app.viewer.as_ref().unwrap().pdf_page, 1);
        app.apply(Action::ViewerPage(9), &ctx);
        assert_eq!(app.viewer.as_ref().unwrap().pdf_page, 2);
        app.apply(Action::ViewerPage(-9), &ctx);
        assert_eq!(app.viewer.as_ref().unwrap().pdf_page, 0);
        // A typed page number is counted from one and stops at either end.
        app.apply(Action::ViewerPageTo(2), &ctx);
        assert_eq!(app.viewer.as_ref().unwrap().pdf_page, 1);
        app.apply(Action::ViewerPageTo(99), &ctx);
        assert_eq!(app.viewer.as_ref().unwrap().pdf_page, 2);
        app.apply(Action::ViewerPageTo(0), &ctx);
        assert_eq!(app.viewer.as_ref().unwrap().pdf_page, 0);
        // Turning the page turns the sheet a quarter at a time and back again.
        app.apply(Action::ViewerRotate, &ctx);
        assert_eq!(app.viewer.as_ref().unwrap().pdf_rotate, 1);
        for _ in 0..3 {
            app.apply(Action::ViewerRotate, &ctx);
        }
        assert_eq!(app.viewer.as_ref().unwrap().pdf_rotate, 0);
        // The page is remembered across closing and reopening.
        app.apply(Action::ViewerPageTo(3), &ctx);
        assert_eq!(app.viewer.as_ref().unwrap().pdf_page, 2);
        app.apply(Action::CloseViewer, &ctx);
        assert!(app.settings.pdf_pages.values().any(|page| *page == 2));
        app.apply(
            Action::OpenViewer {
                chat: chat.into(),
                message: "doc".into(),
            },
            &ctx,
        );
        assert_eq!(
            app.viewer.as_ref().unwrap().pdf_page,
            2,
            "the reader returns where they left"
        );

        // Closing leaves nothing behind.
        app.apply(Action::CloseViewer, &ctx);
        assert!(app.viewer.is_none());
        assert!(app.pdf_page.is_none() && app.pdf_texture.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn the_in_chat_search_opens_fills_and_closes() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, events) =
            App::headless(AppDirs::under(directory.path()), Settings::default());
        let ctx = egui::Context::default();
        let chat = "1@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Ada".into()));
        app.open_chat = Some(chat.into());

        app.apply(Action::ToggleChatSearch, &ctx);
        assert!(app.chat_search_open);
        assert!(app.chat_search_focus, "the field takes focus");
        app.apply(Action::ChatSearch("engine".into()), &ctx);
        assert!(app.chat_search_at.is_some(), "the run is scheduled");

        // The worker answers the query that is on screen.
        app.chat_search_query = "engine".to_owned();
        events
            .send(Event::ChatSearch {
                chat: chat.into(),
                query: "engine".into(),
                hits: Ok(vec![message(chat, "hit", 5)]),
            })
            .unwrap();
        app.handle_events();
        assert_eq!(app.chat_search_hits.len(), 1);

        // An answer for a query the reader moved past is dropped.
        events
            .send(Event::ChatSearch {
                chat: chat.into(),
                query: "older".into(),
                hits: Ok(Vec::new()),
            })
            .unwrap();
        app.handle_events();
        assert_eq!(app.chat_search_hits.len(), 1);

        // Another chat starts without this search.
        app.chats
            .push(Chat::new("2@s.whatsapp.net".into(), "Grace".into()));
        app.apply(
            Action::OpenMessage {
                chat: "2@s.whatsapp.net".into(),
                message: "m".into(),
            },
            &ctx,
        );
        assert!(!app.chat_search_open, "a new chat closes the search");

        app.apply(Action::CloseChatSearch, &ctx);
        assert!(!app.chat_search_open);
        assert!(app.chat_search_hits.is_empty() && app.chat_search.is_empty());
    }

    #[test]
    fn autoplay_takes_the_next_audio_that_has_a_file() {
        let dir = std::env::temp_dir().join(format!("zapfast-autoplay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let voice = dir.join("voice.ogg");
        std::fs::write(&voice, b"OggS").expect("writes");
        let audio = |path: Option<PathBuf>| Content::Audio {
            media: Media {
                mime: "audio/ogg".to_owned(),
                size: 4,
                width: None,
                height: None,
                path,
                state: MediaState::Idle,
            },
            seconds: Some(3),
            voice_note: true,
            waveform: Vec::new(),
        };
        let mut first = message("c", "first", 10);
        first.content = audio(Some(voice.clone()));
        let text = message("c", "text", 20);
        let mut missing = message("c", "missing", 25);
        missing.content = audio(None);
        let mut second = message("c", "second", 30);
        second.content = audio(Some(voice.clone()));
        let list = vec![first, text, missing, second];

        let next = next_audio_after(&list, "first").expect("the next audio");
        assert_eq!(
            next.0, "second",
            "text and a file-less clip are passed over"
        );
        assert_eq!(next.1, voice);
        assert!(next_audio_after(&list, "second").is_none());
        assert!(next_audio_after(&list, "missing").is_some());
        assert!(next_audio_after(&list, "unknown").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_speed_button_walks_the_supported_speeds() {
        let mut app = app();
        let ctx = egui::Context::default();
        let message = "voice-1".to_owned();
        assert_eq!(app.player.speed_of(&message), 1.0);
        for expected in [1.5, 2.0, 1.0] {
            app.apply(Action::CycleAudioSpeed(message.clone()), &ctx);
            assert_eq!(app.player.speed_of(&message), expected);
        }
        // Another clip keeps its own speed.
        assert_eq!(app.player.speed_of("voice-2"), 1.0);
    }

    #[test]
    fn a_search_hit_opens_its_chat_at_the_message() {
        let mut app = app();
        let ctx = egui::Context::default();
        let chat = "1@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Ada".into()));
        let conversation = Conversation {
            requested: true,
            complete: true,
            messages: vec![message(chat, "old", 10)],
            ..Default::default()
        };
        app.conversations.insert(chat.into(), conversation);
        app.apply(
            Action::OpenMessage {
                chat: chat.into(),
                message: "old".into(),
            },
            &ctx,
        );
        assert_eq!(app.open_chat.as_deref(), Some(chat));
        assert_eq!(app.scroll_anchor.as_deref(), Some("old"));
        assert!(!app.scroll_to_bottom, "aims at the hit, not the end");
    }

    #[test]
    fn clearing_the_search_clears_its_hits() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.search_hits.push(message("1@s.whatsapp.net", "m", 1));
        app.apply(Action::Search(String::new()), &ctx);
        assert!(app.search_hits.is_empty());
    }

    #[test]
    fn matching_contacts_are_people_not_yet_talked_to() {
        let mut app = app();
        app.me = Some("490000000000@s.whatsapp.net".into());
        let contact = |id: &str, name: &str| crate::model::Contact {
            id: id.into(),
            full_name: Some(name.into()),
            push_name: None,
        };
        // Exclude contacts that already have chats.
        app.contacts.insert(
            "491700000001@s.whatsapp.net".into(),
            contact("491700000001@s.whatsapp.net", "Ada Lovelace"),
        );
        app.chats.push(Chat::new(
            "491700000001@s.whatsapp.net".into(),
            "Ada Lovelace".into(),
        ));
        // Include contacts without chats.
        app.contacts.insert(
            "491700000002@s.whatsapp.net".into(),
            contact("491700000002@s.whatsapp.net", "Adele Goldberg"),
        );
        // Exclude groups and our own id.
        app.contacts
            .insert("12345@g.us".into(), contact("12345@g.us", "Adventurers"));
        app.contacts.insert(
            "490000000000@s.whatsapp.net".into(),
            contact("490000000000@s.whatsapp.net", "Adah Me"),
        );
        app.search = "ad".into();
        let names: Vec<&str> = app
            .matching_contacts()
            .iter()
            .filter_map(|contact| contact.display_name())
            .collect();
        assert_eq!(names, vec!["Adele Goldberg"]);
        // Match phone-number digits.
        app.search = "491700000002".into();
        assert_eq!(app.matching_contacts().len(), 1);
        app.search = String::new();
        assert!(app.matching_contacts().is_empty());
    }

    #[test]
    fn visible_chats_pin_first_and_filter() {
        let mut app = app();
        let mut a = Chat::new("1@s.whatsapp.net".into(), "Ada".into());
        a.last_activity = 10;
        let mut b = Chat::new("2@s.whatsapp.net".into(), "Bob".into());
        b.last_activity = 20;
        let mut c = Chat::new("3@s.whatsapp.net".into(), "Cy".into());
        c.last_activity = 5;
        c.pinned = true;
        let mut d = Chat::new("4@s.whatsapp.net".into(), "Dee".into());
        d.archived = true;
        app.chats = vec![b, a, c, d];
        let names: Vec<&str> = app
            .visible_chats()
            .iter()
            .map(|chat| chat.name.as_str())
            .collect();
        assert_eq!(names, vec!["Cy", "Bob", "Ada"]);
        app.search = "ad".into();
        let names: Vec<&str> = app
            .visible_chats()
            .iter()
            .map(|chat| chat.name.as_str())
            .collect();
        assert_eq!(names, vec!["Ada"]);
    }

    #[test]
    fn archived_count_follows_the_current_tab() {
        let mut app = app();
        let mut archived_chat = Chat::new("1@s.whatsapp.net".into(), "Ada".into());
        archived_chat.archived = true;
        let mut archived_other = Chat::new("2@s.whatsapp.net".into(), "Bob".into());
        archived_other.archived = true;
        let mut archived_channel = Chat::new("3@newsletter".into(), "News".into());
        archived_channel.archived = true;
        app.chats = vec![archived_chat, archived_other, archived_channel];

        app.show_channels = false;
        assert_eq!(app.archived_count(), 2);
        app.show_archived = true;
        assert_eq!(app.visible_chats().len(), 2);

        app.show_archived = false;
        app.show_channels = true;
        assert_eq!(app.archived_count(), 1);
        app.show_archived = true;
        assert_eq!(app.visible_chats().len(), 1);
    }

    #[test]
    fn channel_sends_are_refused_before_any_command() {
        let mut app = app();
        app.chats
            .push(Chat::new("1@newsletter".into(), "News".into()));
        app.chats
            .push(Chat::new("1@s.whatsapp.net".into(), "Ada".into()));
        let channel: ChatId = "1@newsletter".into();
        let direct: ChatId = "1@s.whatsapp.net".into();
        assert!(app.send_blocked(&channel));
        assert!(!app.send_blocked(&direct));
        // Chats missing from the list keep working: confirm dialogs and
        // list races send before the row exists. Unknown newsletters stay
        // refused by address.
        assert!(!app.send_blocked(&"unknown@s.whatsapp.net".into()));
        assert!(app.send_blocked(&"unknown@newsletter".into()));
    }

    // Intentional pastes are modelled at the application level below:
    // every Ctrl+V or menu gesture below drives the real
    // take_image_paste frame by frame, with the system clipboard
    // replaced by an injected reader.
    fn paste_release() -> egui::Event {
        egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        }
    }

    fn paste_press_plain() -> egui::Event {
        egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn paste_release_plain() -> egui::Event {
        egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    fn ten_by_ten(fill: u8) -> (usize, usize, Vec<u8>) {
        (10, 10, vec![fill; 10 * 10 * 4])
    }

    fn paste_app() -> (App, egui::Context) {
        let mut app = app();
        app.open_chat = Some("1@s.whatsapp.net".into());
        app.chats
            .push(Chat::new("1@s.whatsapp.net".into(), "Ada".into()));
        app.composer = "caption".into();
        let ctx = egui::Context::default();
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("composer-text")));
        paste_frame(&mut app, &ctx, vec![], true, None);
        (app, ctx)
    }

    fn paste_frame(
        app: &mut App,
        ctx: &egui::Context,
        mut events: Vec<egui::Event>,
        command: bool,
        image: Option<(usize, usize, Vec<u8>)>,
    ) {
        events.insert(
            0,
            egui::Event::ModifiersChanged(if command {
                egui::Modifiers::COMMAND
            } else {
                egui::Modifiers::NONE
            }),
        );
        let mut output = ctx.run_ui(
            egui::RawInput {
                events,
                ..Default::default()
            },
            |ui| {
                app.take_image_paste(ui.ctx(), || image.clone());
                ui.add(
                    egui::TextEdit::singleline(&mut app.composer)
                        .id(egui::Id::new("composer-text")),
                );
                app.apply_actions(ui.ctx());
            },
        );
        output.textures_delta.clear();
    }

    #[test]
    fn two_ctrl_v_image_gestures_attach_twice() {
        let (mut app, ctx) = paste_app();
        let image = ten_by_ten(7);
        // Image-only presses deliver no Key event: empty frames stand in
        // for them, releases do the attaching.
        paste_frame(&mut app, &ctx, vec![], true, Some(image.clone()));
        paste_frame(
            &mut app,
            &ctx,
            vec![paste_release()],
            true,
            Some(image.clone()),
        );
        assert_eq!(app.pending.len(), 1);
        paste_frame(&mut app, &ctx, vec![], true, Some(image.clone()));
        paste_frame(
            &mut app,
            &ctx,
            vec![paste_release()],
            true,
            Some(image.clone()),
        );
        assert_eq!(app.pending.len(), 2, "a second Ctrl+V must attach again");
    }

    #[test]
    fn paste_event_then_release_attaches_once() {
        let (mut app, ctx) = paste_app();
        let image = ten_by_ten(7);
        paste_frame(
            &mut app,
            &ctx,
            vec![egui::Event::Paste("https://example.com/pic.png".into())],
            true,
            Some(image.clone()),
        );
        assert_eq!(app.pending.len(), 1);
        assert_eq!(
            app.composer, "caption",
            "the source URL must not leak into the draft"
        );
        paste_frame(&mut app, &ctx, vec![paste_release()], true, Some(image));
        assert_eq!(
            app.pending.len(),
            1,
            "the release of the same gesture must not duplicate"
        );
    }

    #[test]
    fn menu_paste_then_shortcut_attaches_twice() {
        let (mut app, ctx) = paste_app();
        let image = ten_by_ten(7);
        paste_frame(
            &mut app,
            &ctx,
            vec![egui::Event::Paste("https://example.com/pic.png".into())],
            false,
            Some(image.clone()),
        );
        assert_eq!(app.pending.len(), 1);
        // The shortcut press delivers nothing either; only its release
        // may attach.
        paste_frame(&mut app, &ctx, vec![], true, Some(image.clone()));
        paste_frame(&mut app, &ctx, vec![paste_release()], true, Some(image));
        assert_eq!(
            app.pending.len(),
            2,
            "menu paste must not swallow the next shortcut"
        );
    }

    #[test]
    fn same_edges_different_middle_attaches_twice() {
        let (mut app, ctx) = paste_app();
        let mut middle = ten_by_ten(7);
        middle.2[200] = 9;
        paste_frame(
            &mut app,
            &ctx,
            vec![paste_release()],
            true,
            Some(ten_by_ten(7)),
        );
        assert_eq!(app.pending.len(), 1);
        paste_frame(&mut app, &ctx, vec![paste_release()], true, Some(middle));
        assert_eq!(
            app.pending.len(),
            2,
            "content identity must not gate intent"
        );
    }

    #[test]
    fn consecutive_release_only_pastes_attach_twice() {
        let (mut app, ctx) = paste_app();
        let image = ten_by_ten(7);
        paste_frame(
            &mut app,
            &ctx,
            vec![paste_release()],
            true,
            Some(image.clone()),
        );
        assert_eq!(app.pending.len(), 1);
        paste_frame(&mut app, &ctx, vec![paste_release()], true, Some(image));
        assert_eq!(
            app.pending.len(),
            2,
            "a release-only gesture must not arm suppression"
        );
    }

    fn unfocused_frame(
        app: &mut App,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        calls: &mut usize,
    ) {
        // Overlay open: the window lost focus, like under Win+Shift+S.
        // Releases and pastes delivered here must die with the frame.
        let mut output = ctx.run_ui(
            egui::RawInput {
                focused: false,
                events,
                ..Default::default()
            },
            |ui| {
                app.take_image_paste(ui.ctx(), || {
                    *calls += 1;
                    None
                });
                ui.add(
                    egui::TextEdit::singleline(&mut app.composer)
                        .id(egui::Id::new("composer-text")),
                );
                app.apply_actions(ui.ctx());
            },
        );
        output.textures_delta.clear();
    }

    #[test]
    fn unfocused_frames_read_nothing_and_attach_nothing() {
        let (mut app, ctx) = paste_app();
        let mut calls = 0;
        unfocused_frame(&mut app, &ctx, vec![paste_release()], &mut calls);
        unfocused_frame(
            &mut app,
            &ctx,
            vec![egui::Event::Paste("shot".into())],
            &mut calls,
        );
        unfocused_frame(&mut app, &ctx, vec![], &mut calls);
        assert_eq!(calls, 0, "no clipboard touch without focus");
        assert!(app.pending.is_empty());
        assert!(!app.paste_armed && !app.paste_typed_v && !app.paste_ctrl_held);
    }

    #[test]
    fn release_outside_then_return_pastes_once_without_dup() {
        let (mut app, ctx) = paste_app();
        let image = ten_by_ten(7);
        // Ctrl let go outside while the overlay owns the keys.
        let mut calls = 0;
        unfocused_frame(&mut app, &ctx, vec![paste_release()], &mut calls);
        assert!(app.pending.is_empty());
        // A fresh Ctrl+V after return attaches exactly once.
        paste_frame(&mut app, &ctx, vec![], true, Some(image.clone()));
        paste_frame(&mut app, &ctx, vec![paste_release()], true, Some(image));
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn large_print_paste_cost_is_bounded_and_reported() {
        let (mut app, ctx) = paste_app();
        // 4K screenshot: 3840 by 2160 RGBA is about 33 MB per copy.
        let big = (3840, 2160, vec![7u8; 3840 * 2160 * 4]);
        let start = std::time::Instant::now();
        paste_frame(
            &mut app,
            &ctx,
            vec![egui::Event::Paste("shot".into())],
            true,
            Some(big),
        );
        eprintln!("paste-4k single intent on UI thread: {:?}", start.elapsed());
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn blocked_reader_blocks_the_calling_frame() {
        // Structural proof, not a freeze repro: whatever stalls the
        // reader stalls the frame by the same amount.
        let (mut app, ctx) = paste_app();
        let image = ten_by_ten(7);
        let start = std::time::Instant::now();
        let mut output = ctx.run_ui(
            egui::RawInput {
                events: vec![
                    egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                    egui::Event::Paste("shot".into()),
                ],
                ..Default::default()
            },
            |ui| {
                app.take_image_paste(ui.ctx(), || {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    Some(image.clone())
                });
                ui.add(
                    egui::TextEdit::singleline(&mut app.composer)
                        .id(egui::Id::new("composer-text")),
                );
                app.apply_actions(ui.ctx());
            },
        );
        output.textures_delta.clear();
        eprintln!("blocked reader held the frame: {:?}", start.elapsed());
        assert!(start.elapsed() >= std::time::Duration::from_millis(300));
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn text_search_switch_and_empty_clipboard_stay_correct() {
        let (mut app, ctx) = paste_app();
        paste_frame(
            &mut app,
            &ctx,
            vec![egui::Event::Paste("hello".into())],
            true,
            None,
        );
        assert!(app.pending.is_empty(), "pure text never stages a picture");
        assert!(
            app.composer.contains("hello"),
            "pure text still reaches the draft"
        );
        // Every keyboard Paste is followed by its own key release, which
        // ends that text gesture and clears the flag for the next one.
        paste_frame(&mut app, &ctx, vec![paste_release()], true, None);
        assert!(app.pending.is_empty());
        let image = ten_by_ten(7);
        paste_frame(
            &mut app,
            &ctx,
            vec![paste_release()],
            true,
            Some(image.clone()),
        );
        assert_eq!(app.pending.len(), 1);
        app.chats
            .push(Chat::new("2@s.whatsapp.net".into(), "Bob".into()));
        app.open_chat = Some("2@s.whatsapp.net".into());
        paste_frame(
            &mut app,
            &ctx,
            vec![paste_release()],
            true,
            Some(image.clone()),
        );
        assert_eq!(
            app.pending.len(),
            2,
            "switching chats must not carry suppression over"
        );
        paste_frame(&mut app, &ctx, vec![paste_release()], true, None);
        assert_eq!(app.pending.len(), 2, "an empty clipboard stages nothing");
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("search-field")));
        paste_frame(&mut app, &ctx, vec![paste_release()], true, Some(image));
        assert_eq!(
            app.pending.len(),
            2,
            "a focused search field keeps the image out"
        );
    }

    #[test]
    fn newsletter_paste_attaches_nothing() {
        let (mut app, ctx) = paste_app();
        app.chats
            .push(Chat::new("1@newsletter".into(), "News".into()));
        app.open_chat = Some("1@newsletter".into());
        let image = ten_by_ten(7);
        paste_frame(
            &mut app,
            &ctx,
            vec![egui::Event::Paste("https://example.com/pic.png".into())],
            true,
            Some(image.clone()),
        );
        paste_frame(&mut app, &ctx, vec![paste_release()], true, Some(image));
        assert!(app.pending.is_empty(), "channels refuse pasted pictures");
    }

    #[test]
    fn removed_chat_closes_open_state() {
        let root =
            std::env::temp_dir().join(format!("zapfast-removed-chat-{}", std::process::id()));
        let (mut app, events) = App::headless(AppDirs::under(&root), Settings::default());
        app.chats
            .push(Chat::new("1@s.whatsapp.net".into(), "Ada".into()));
        app.chats
            .push(Chat::new("2@s.whatsapp.net".into(), "Bob".into()));
        app.open_chat = Some("1@s.whatsapp.net".into());
        app.composer = "draft".into();
        app.editing = Some("m1".into());
        app.chat_search_open = true;
        app.chat_search_hits = vec![message("1@s.whatsapp.net", "m1", 10)];
        app.conversations
            .entry("1@s.whatsapp.net".into())
            .or_default()
            .merge(vec![message("1@s.whatsapp.net", "m1", 10)], false);
        events
            .send(Event::ChatRemoved {
                chat: "1@s.whatsapp.net".into(),
            })
            .expect("sends");
        app.handle_events();
        assert_eq!(app.chats.len(), 1);
        assert_eq!(app.chats[0].id, "2@s.whatsapp.net");
        assert!(app.open_chat.is_none(), "the open chat is gone");
        assert!(app.composer.is_empty(), "no draft for a missing chat");
        assert!(app.editing.is_none());
        assert!(!app.chat_search_open);
        assert!(app.chat_search_hits.is_empty());
        assert!(!app.conversations.contains_key("1@s.whatsapp.net"));
    }

    #[test]
    fn cleared_chat_filters_range_and_keeps_newer() {
        let root =
            std::env::temp_dir().join(format!("zapfast-cleared-chat-{}", std::process::id()));
        let (mut app, events) = App::headless(AppDirs::under(&root), Settings::default());
        app.chats
            .push(Chat::new("1@s.whatsapp.net".into(), "Ada".into()));
        app.open_chat = Some("1@s.whatsapp.net".into());
        app.conversations
            .entry("1@s.whatsapp.net".into())
            .or_default()
            .merge(
                vec![
                    message("1@s.whatsapp.net", "m1", 10),
                    message("1@s.whatsapp.net", "m2", 100),
                ],
                false,
            );
        events
            .send(Event::ChatCleared {
                chat: "1@s.whatsapp.net".into(),
                through: 50,
            })
            .expect("sends");
        app.handle_events();
        assert_eq!(app.chats.len(), 1, "the chat stays listed");
        assert_eq!(app.open_chat.as_deref(), Some("1@s.whatsapp.net"));
        let conversation = app
            .conversations
            .get("1@s.whatsapp.net")
            .expect("newer messages stay cached");
        assert_eq!(
            conversation
                .messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            vec!["m2"],
            "only the cleared range goes"
        );
        assert!(
            !conversation.requested,
            "no reload is requested for a filtered cache"
        );
    }

    #[test]
    fn deleted_message_leaves_every_projection() {
        let root =
            std::env::temp_dir().join(format!("zapfast-deleted-everywhere-{}", std::process::id()));
        let (mut app, events) = App::headless(AppDirs::under(&root), Settings::default());
        app.chats
            .push(Chat::new("1@s.whatsapp.net".into(), "Ada".into()));
        app.open_chat = Some("1@s.whatsapp.net".into());
        app.conversations
            .entry("1@s.whatsapp.net".into())
            .or_default()
            .merge(
                vec![
                    message("1@s.whatsapp.net", "m1", 10),
                    message("1@s.whatsapp.net", "m2", 20),
                ],
                false,
            );
        app.search_hits = vec![
            message("1@s.whatsapp.net", "m1", 10),
            message("2@s.whatsapp.net", "m1", 10),
        ];
        app.chat_search_open = true;
        app.chat_search_hits = vec![message("1@s.whatsapp.net", "m1", 10)];
        app.reply_to = Some("m1".into());
        app.editing = Some("m1".into());
        app.composer = "draft".into();
        app.selected = vec!["m1".into(), "m2".into()];
        app.viewer = Some(Viewer {
            chat: "1@s.whatsapp.net".into(),
            items: vec![
                ViewerItem {
                    message: "m1".into(),
                    path: "/tmp/m1".into(),
                    kind: ViewerKind::Picture,
                },
                ViewerItem {
                    message: "m2".into(),
                    path: "/tmp/m2".into(),
                    kind: ViewerKind::Picture,
                },
            ],
            index: 1,
            zoom: 1.0,
            offset: (0.0, 0.0),
            pdf_page: 0,
            pdf_pages: 0,
            pdf_rotate: 0,
        });
        events
            .send(Event::MessageDeleted {
                chat: "1@s.whatsapp.net".into(),
                id: "m1".into(),
            })
            .expect("sends");
        app.handle_events();
        let ids: Vec<&str> = app
            .conversations
            .get("1@s.whatsapp.net")
            .expect("chat stays")
            .messages
            .iter()
            .map(|message| message.id.as_str())
            .collect();
        assert_eq!(ids, vec!["m2"]);
        assert_eq!(app.search_hits.len(), 1);
        assert_eq!(app.search_hits[0].chat.as_str(), "2@s.whatsapp.net");
        assert!(app.chat_search_hits.is_empty());
        assert!(app.reply_to.is_none());
        assert!(app.editing.is_none());
        assert!(app.composer.is_empty());
        assert_eq!(app.selected, vec!["m2".to_owned()]);
        let viewer = app.viewer.as_ref().expect("viewer survives");
        assert_eq!(viewer.items.len(), 1);
        assert_eq!(viewer.index, 0);
        events
            .send(Event::MessageDeleted {
                chat: "1@s.whatsapp.net".into(),
                id: "m2".into(),
            })
            .expect("sends");
        app.handle_events();
        assert!(app.viewer.is_none(), "viewer closes on its last item");
    }
    #[test]
    fn deleted_message_in_other_chat_keeps_open_state() {
        let root =
            std::env::temp_dir().join(format!("zapfast-deleted-elsewhere-{}", std::process::id()));
        let (mut app, events) = App::headless(AppDirs::under(&root), Settings::default());
        app.open_chat = Some("1@s.whatsapp.net".into());
        app.reply_to = Some("m9".into());
        app.editing = Some("m9".into());
        app.selected = vec!["m9".into()];
        app.chat_search_open = true;
        app.chat_search_hits = vec![message("1@s.whatsapp.net", "m9", 10)];
        app.conversations
            .entry("2@s.whatsapp.net".into())
            .or_default()
            .merge(vec![message("2@s.whatsapp.net", "m1", 10)], false);
        app.search_hits = vec![message("2@s.whatsapp.net", "m1", 10)];
        events
            .send(Event::MessageDeleted {
                chat: "2@s.whatsapp.net".into(),
                id: "m1".into(),
            })
            .expect("sends");
        app.handle_events();
        assert!(
            app.conversations
                .get("2@s.whatsapp.net")
                .expect("preloaded")
                .messages
                .is_empty()
        );
        assert!(app.search_hits.is_empty());
        assert_eq!(app.reply_to.as_deref(), Some("m9"));
        assert_eq!(app.editing.as_deref(), Some("m9"));
        assert_eq!(app.selected, vec!["m9".to_owned()]);
        assert_eq!(app.chat_search_hits.len(), 1);
    }
    #[test]
    fn deleted_missing_row_still_clears_stale_screen() {
        let root =
            std::env::temp_dir().join(format!("zapfast-deleted-stale-{}", std::process::id()));
        let (mut app, events) = App::headless(AppDirs::under(&root), Settings::default());
        app.open_chat = Some("1@s.whatsapp.net".into());
        app.conversations
            .entry("1@s.whatsapp.net".into())
            .or_default()
            .merge(vec![message("1@s.whatsapp.net", "m1", 10)], false);
        events
            .send(Event::MessageDeleted {
                chat: "1@s.whatsapp.net".into(),
                id: "m1".into(),
            })
            .expect("sends");
        app.handle_events();
        assert!(
            app.conversations
                .get("1@s.whatsapp.net")
                .expect("chat stays")
                .messages
                .is_empty()
        );
        events
            .send(Event::MessageDeleted {
                chat: "1@s.whatsapp.net".into(),
                id: "m1".into(),
            })
            .expect("sends");
        app.handle_events();
    }
    #[test]
    fn cleared_thousands_with_big_selection_stay_synchronous() {
        // Five thousand rows, five thousand selected, reply and edit
        // inside the cleared range: membership by set keeps the clear
        // linear instead of quadratic.
        let root =
            std::env::temp_dir().join(format!("zapfast-cleared-many-{}", std::process::id()));
        let (mut app, events) = App::headless(AppDirs::under(&root), Settings::default());
        app.open_chat = Some("1@s.whatsapp.net".into());
        let rows: Vec<Message> = (1..=5000)
            .map(|n| message("1@s.whatsapp.net", &format!("m{n}"), n))
            .collect();
        app.conversations
            .entry("1@s.whatsapp.net".into())
            .or_default()
            .merge(rows, false);
        app.search_hits = (1..=5000)
            .map(|n| message("1@s.whatsapp.net", &format!("m{n}"), n))
            .collect();
        app.chat_search_hits = (1..=5000)
            .map(|n| message("1@s.whatsapp.net", &format!("m{n}"), n))
            .collect();
        app.selected = (1..=5000).map(|n| format!("m{n}")).collect();
        app.reply_to = Some("m100".into());
        app.editing = Some("m200".into());
        app.composer = "draft".into();
        events
            .send(Event::ChatCleared {
                chat: "1@s.whatsapp.net".into(),
                through: 2500,
            })
            .expect("sends");
        app.handle_events();
        assert_eq!(
            app.conversations
                .get("1@s.whatsapp.net")
                .expect("chat stays")
                .messages
                .len(),
            2500
        );
        assert_eq!(app.search_hits.len(), 2500);
        assert_eq!(app.chat_search_hits.len(), 2500);
        assert_eq!(app.selected.len(), 2500, "only survivors stay selected");
        assert!(app.reply_to.is_none(), "cleared reply goes");
        assert!(app.editing.is_none(), "cleared edit goes");
        assert!(app.composer.is_empty());
    }

    #[test]
    fn deleted_video_message_stops_media_but_keeps_unrelated_voice() {
        let root =
            std::env::temp_dir().join(format!("zapfast-deleted-media-{}", std::process::id()));
        let (mut app, events) = App::headless(AppDirs::under(&root), Settings::default());
        app.open_chat = Some("1@s.whatsapp.net".into());
        // Viewer open on the video message; a voice note plays for
        // another message. Loading needs no audio device, only a path.
        app.viewer = Some(Viewer {
            chat: "1@s.whatsapp.net".into(),
            items: vec![ViewerItem {
                message: "m1".into(),
                path: "/tmp/m1.mp4".into(),
                kind: ViewerKind::Video,
            }],
            index: 0,
            zoom: 1.0,
            offset: (0.0, 0.0),
            pdf_page: 0,
            pdf_pages: 0,
            pdf_rotate: 0,
        });
        app.player
            .toggle("m2", std::path::Path::new("/tmp/voice-m2.ogg"))
            .expect("loads");
        assert_eq!(app.player.playing_message(), Some("m2"));
        // Deleting the video closes its viewer with effective shutdown
        // while the unrelated voice keeps playing.
        events
            .send(Event::MessageDeleted {
                chat: "1@s.whatsapp.net".into(),
                id: "m1".into(),
            })
            .expect("sends");
        app.handle_events();
        assert!(app.viewer.is_none());
        assert_eq!(app.player.playing_message(), Some("m2"));
        // Deleting the voice message stills its own sound.
        events
            .send(Event::MessageDeleted {
                chat: "1@s.whatsapp.net".into(),
                id: "m2".into(),
            })
            .expect("sends");
        app.handle_events();
        assert_eq!(app.player.playing_message(), None);
    }
    #[test]
    fn viewer_keeps_the_open_item_by_identity() {
        let root = std::env::temp_dir().join(format!("zapfast-viewer-id-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("creates");
        let (mut app, events) = App::headless(AppDirs::under(&root), Settings::default());
        app.open_chat = Some("1@s.whatsapp.net".into());
        let picture = |id: &str, timestamp: i64| {
            let path = root.join(format!("{id}.mp4"));
            std::fs::write(&path, b"bytes").expect("writes");
            let mut message = message("1@s.whatsapp.net", id, timestamp);
            message.content = Content::Video {
                caption: None,
                media: Media {
                    mime: "video/mp4".into(),
                    size: 5,
                    width: None,
                    height: None,
                    path: Some(path),
                    state: Default::default(),
                },
                seconds: None,
                gif: false,
            };
            message
        };
        app.conversations
            .entry("1@s.whatsapp.net".into())
            .or_default()
            .merge(
                vec![picture("m1", 10), picture("m2", 20), picture("m3", 30)],
                false,
            );
        // [m1, m2, m3] showing m2: deleting m1 must keep m2, not slide.
        app.open_viewer("1@s.whatsapp.net", "m2");
        assert_eq!(app.viewer.as_ref().map(|viewer| viewer.index), Some(1));
        events
            .send(Event::MessageDeleted {
                chat: "1@s.whatsapp.net".into(),
                id: "m1".into(),
            })
            .expect("sends");
        app.handle_events();
        let viewer = app.viewer.as_ref().expect("viewer survives");
        assert_eq!(viewer.items.len(), 2);
        assert_eq!(
            viewer.current().map(|item| item.message.as_str()),
            Some("m2")
        );
        assert_eq!(viewer.index, 0);
        // Deleting past the open item changes nothing on screen.
        events
            .send(Event::MessageDeleted {
                chat: "1@s.whatsapp.net".into(),
                id: "m3".into(),
            })
            .expect("sends");
        app.handle_events();
        let viewer = app.viewer.as_ref().expect("viewer survives");
        assert_eq!(
            viewer.current().map(|item| item.message.as_str()),
            Some("m2")
        );
        // Deleting the open item itself closes instead of sliding.
        events
            .send(Event::MessageDeleted {
                chat: "1@s.whatsapp.net".into(),
                id: "m2".into(),
            })
            .expect("sends");
        app.handle_events();
        assert!(app.viewer.is_none());
    }
    #[test]
    fn scrub_cancel_step_and_close_retire_without_a_jump() {
        let root = std::env::temp_dir().join(format!("zapfast-scrub-app-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("creates");
        let (mut app, _events) = App::headless(AppDirs::under(&root), Settings::default());
        app.open_chat = Some("1@s.whatsapp.net".into());
        let clip = |id: &str, timestamp: i64| {
            let path = root.join(format!("{id}.mp4"));
            std::fs::write(&path, b"bytes").expect("writes");
            let mut body = message("1@s.whatsapp.net", id, timestamp);
            body.content = Content::Video {
                caption: None,
                media: Media {
                    mime: "video/mp4".into(),
                    size: 5,
                    width: None,
                    height: None,
                    path: Some(path),
                    state: Default::default(),
                },
                seconds: None,
                gif: false,
            };
            body
        };
        app.conversations
            .entry("1@s.whatsapp.net".into())
            .or_default()
            .merge(vec![clip("m1", 10), clip("m2", 20)], false);
        app.open_viewer("1@s.whatsapp.net", "m1");
        let ctx = egui::Context::default();
        let path = app
            .viewer
            .as_ref()
            .and_then(|viewer| viewer.current())
            .map(|item| item.path.clone())
            .expect("m1 opens");
        let held = |app: &mut App, path: &std::path::Path| {
            let generation = app.previewer.begin(path);
            app.video_scrub = Some(VideoScrub {
                path: path.to_path_buf(),
                generation,
                was_playing: false,
                target: 0.8,
            });
        };
        held(&mut app, &path);
        app.apply(Action::VideoScrubCancel, &ctx);
        assert!(app.video_scrub.is_none(), "cancel retires the drag");
        assert!(app.viewer.is_some(), "cancel never closes the viewer");
        held(&mut app, &path);
        app.apply(Action::ViewerStep(1), &ctx);
        assert!(app.video_scrub.is_none(), "stepping retires the drag");
        assert_eq!(
            app.viewer
                .as_ref()
                .and_then(|viewer| viewer.current())
                .map(|item| item.message.as_str()),
            Some("m2"),
            "the view moves on",
        );
        let next = app
            .viewer
            .as_ref()
            .and_then(|viewer| viewer.current())
            .map(|item| item.path.clone())
            .expect("m2 shows");
        held(&mut app, &next);
        app.apply(Action::CloseViewer, &ctx);
        assert!(app.video_scrub.is_none(), "escape retires the drag first");
        assert!(app.viewer.is_some(), "the first escape keeps the viewer");
        app.apply(Action::CloseViewer, &ctx);
        assert!(app.viewer.is_none(), "the second escape closes");
    }
    #[test]
    fn scrub_commit_with_real_decoder_holds_then_lands() {
        let root = std::env::temp_dir().join(format!("zapfast-scrubflow-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("creates");
        let (mut app, _events) = App::headless(AppDirs::under(&root), Settings::default());
        app.open_chat = Some("1@s.whatsapp.net".into());
        let path = root.join("m1.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y"])
            .args(["-f", "lavfi", "-i", "testsrc2=s=320x240:d=6:r=10"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=6"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .args(["-profile:v", "baseline", "-bf", "0"])
            .args(["-g", "10", "-keyint_min", "10", "-sc_threshold", "0"])
            .args(["-c:a", "aac", "-shortest", "-movflags", "+faststart"])
            .arg(&path)
            .status()
            .is_ok_and(|status| status.success());
        if !made {
            // GitHub runners do not all ship an H.264/libx264 encoder.
            // Keep this as a real decoder test wherever the codec is available.
            return;
        }
        let mut first = message("1@s.whatsapp.net", "m1", 10);
        first.content = Content::Video {
            caption: None,
            media: Media {
                mime: "video/mp4".into(),
                size: 5,
                width: None,
                height: None,
                path: Some(path.clone()),
                state: Default::default(),
            },
            seconds: None,
            gif: false,
        };
        app.conversations
            .entry("1@s.whatsapp.net".into())
            .or_default()
            .merge(vec![first], false);
        app.open_viewer("1@s.whatsapp.net", "m1");
        let ctx = egui::Context::default();
        app.apply(Action::VideoToggle, &ctx);
        let deadline = Instant::now() + Duration::from_secs(20);
        let total = loop {
            match app.video.poll(&ctx, &path) {
                crate::video::State::Showing { playing, total, .. } if playing => break total,
                crate::video::State::Showing { .. } => {}
                crate::video::State::Loading => {}
                crate::video::State::Unsupported(why) => panic!("the fixture plays: {why}"),
            }
            assert!(Instant::now() < deadline, "playback starts");
            std::thread::sleep(Duration::from_millis(20));
        };
        app.apply(Action::VideoToggle, &ctx);
        let generation = app.previewer.begin(&path);
        app.video_scrub = Some(VideoScrub {
            path: path.clone(),
            generation,
            was_playing: true,
            target: 0.75,
        });
        let frozen = match app.video.poll(&ctx, &path) {
            crate::video::State::Showing {
                position, playing, ..
            } => {
                assert!(!playing, "the drag holds playback");
                position
            }
            _ => panic!("the held clip keeps showing"),
        };
        std::thread::sleep(Duration::from_millis(150));
        let still = match app.video.poll(&ctx, &path) {
            crate::video::State::Showing { position, .. } => position,
            _ => panic!("the held clip keeps showing"),
        };
        assert_eq!(frozen, still, "a held preview advances nothing");
        let target = total.mul_f32(0.75);
        app.apply(Action::VideoSeek(0.75), &ctx);
        app.apply(Action::VideoToggle, &ctx);
        let start = Instant::now();
        let landed = loop {
            match app.video.poll(&ctx, &path) {
                crate::video::State::Showing {
                    position,
                    seeking,
                    playing,
                    ..
                } => {
                    assert!(playing, "the commit resumes");
                    if seeking {
                        assert_eq!(position, target, "the clock holds the target mid-seek");
                    } else {
                        break start.elapsed().as_millis();
                    }
                }
                crate::video::State::Loading => {}
                crate::video::State::Unsupported(why) => panic!("the fixture plays: {why}"),
            }
            assert!(start.elapsed() < Duration::from_secs(25), "the jump lands");
            std::thread::sleep(Duration::from_millis(20));
        };
        eprintln!("scrub-commit landed_ms={landed}");
        app.apply(Action::VideoToggle, &ctx);
        let generation = app.previewer.begin(&path);
        app.video_scrub = Some(VideoScrub {
            path: path.clone(),
            generation,
            was_playing: false,
            target: 0.25,
        });
        app.apply(Action::VideoSeek(0.25), &ctx);
        let parked = total.mul_f32(0.25);
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            match app.video.poll(&ctx, &path) {
                crate::video::State::Showing {
                    position,
                    seeking,
                    playing,
                    ..
                } => {
                    assert!(!playing, "a parked commit never resumes");
                    if !seeking {
                        let gap = position.abs_diff(parked);
                        assert!(gap < Duration::from_millis(200), "the parked jump lands");
                        break;
                    }
                }
                crate::video::State::Loading => {}
                crate::video::State::Unsupported(why) => panic!("the fixture plays: {why}"),
            }
            assert!(Instant::now() < deadline, "the parked jump lands");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    #[test]
    fn scrub_lifecycle_with_real_decoder_stops_cleanly() {
        let root = std::env::temp_dir().join(format!("zapfast-scrublife-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("creates");
        let (mut app, _events) = App::headless(AppDirs::under(&root), Settings::default());
        app.open_chat = Some("1@s.whatsapp.net".into());
        let real = |id: &str| {
            let path = root.join(format!("{id}.mp4"));
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
            // GitHub runners do not all ship an H.264/libx264 encoder.
            // Keep this as a real decoder test wherever the codec is available.
            return;
        }
            let mut body = message("1@s.whatsapp.net", id, 10);
            body.content = Content::Video {
                caption: None,
                media: Media {
                    mime: "video/mp4".into(),
                    size: 5,
                    width: None,
                    height: None,
                    path: Some(path.clone()),
                    state: Default::default(),
                },
                seconds: None,
                gif: false,
            };
            (path, body)
        };
        let (first_path, first) = real("m1");
        let (_, second) = real("m2");
        app.conversations
            .entry("1@s.whatsapp.net".into())
            .or_default()
            .merge(vec![first, second], false);
        app.open_viewer("1@s.whatsapp.net", "m1");
        let ctx = egui::Context::default();
        app.apply(Action::VideoToggle, &ctx);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match app.video.poll(&ctx, &first_path) {
                crate::video::State::Showing { playing, .. } if playing => break,
                crate::video::State::Showing { .. } => {}
                crate::video::State::Loading => {}
                crate::video::State::Unsupported(why) => panic!("the fixture plays: {why}"),
            }
            assert!(Instant::now() < deadline, "playback starts");
            std::thread::sleep(Duration::from_millis(20));
        }
        app.apply(Action::VideoToggle, &ctx);
        let generation = app.previewer.begin(&first_path);
        app.video_scrub = Some(VideoScrub {
            path: first_path.clone(),
            generation,
            was_playing: true,
            target: 0.5,
        });
        app.apply(Action::VideoScrubCancel, &ctx);
        assert!(app.video_scrub.is_none(), "escape retires the drag");
        assert!(app.viewer.is_some(), "escape never closes the viewer");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match app.video.poll(&ctx, &first_path) {
                crate::video::State::Showing { playing, .. } if playing => break,
                crate::video::State::Showing { .. } => {}
                crate::video::State::Loading => {}
                crate::video::State::Unsupported(why) => panic!("the fixture plays: {why}"),
            }
            assert!(Instant::now() < deadline, "escape resumes playback");
            std::thread::sleep(Duration::from_millis(20));
        }
        app.apply(Action::VideoToggle, &ctx);
        let generation = app.previewer.begin(&first_path);
        app.video_scrub = Some(VideoScrub {
            path: first_path.clone(),
            generation,
            was_playing: false,
            target: 0.5,
        });
        app.apply(Action::ViewerStep(1), &ctx);
        assert!(app.video_scrub.is_none(), "stepping retires the drag");
        assert_eq!(
            app.viewer
                .as_ref()
                .and_then(|viewer| viewer.current())
                .map(|item| item.message.as_str()),
            Some("m2"),
            "the view moves on",
        );
        assert!(
            !app.video.is_active(&first_path),
            "the old decode stands down"
        );
        let second_path = app
            .viewer
            .as_ref()
            .and_then(|viewer| viewer.current())
            .map(|item| item.path.clone())
            .expect("m2 shows");
        let generation = app.previewer.begin(&second_path);
        app.video_scrub = Some(VideoScrub {
            path: second_path.clone(),
            generation,
            was_playing: false,
            target: 0.5,
        });
        app.apply(Action::CloseViewer, &ctx);
        assert!(
            app.video_scrub.is_none(),
            "the first close retires the drag"
        );
        assert!(app.viewer.is_some(), "the first close keeps the viewer");
        app.apply(Action::CloseViewer, &ctx);
        assert!(app.viewer.is_none(), "the second close leaves");
        assert!(
            !app.video.is_active(&second_path),
            "closing stills the decoder"
        );
    }
    #[test]
    fn cleared_range_repoints_the_open_item_by_identity() {
        let root =
            std::env::temp_dir().join(format!("zapfast-viewer-clear-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("creates");
        let (mut app, events) = App::headless(AppDirs::under(&root), Settings::default());
        app.open_chat = Some("1@s.whatsapp.net".into());
        let picture = |id: &str, timestamp: i64| {
            let path = root.join(format!("{id}.mp4"));
            std::fs::write(&path, b"bytes").expect("writes");
            let mut message = message("1@s.whatsapp.net", id, timestamp);
            message.content = Content::Video {
                caption: None,
                media: Media {
                    mime: "video/mp4".into(),
                    size: 5,
                    width: None,
                    height: None,
                    path: Some(path),
                    state: Default::default(),
                },
                seconds: None,
                gif: false,
            };
            message
        };
        app.conversations
            .entry("1@s.whatsapp.net".into())
            .or_default()
            .merge(
                vec![picture("m1", 10), picture("m2", 100), picture("m3", 200)],
                false,
            );
        app.open_viewer("1@s.whatsapp.net", "m2");
        // A partial clear keeps the open picture by identity.
        events
            .send(Event::ChatCleared {
                chat: "1@s.whatsapp.net".into(),
                through: 50,
            })
            .expect("sends");
        app.handle_events();
        let viewer = app.viewer.as_ref().expect("viewer survives");
        assert_eq!(viewer.items.len(), 2);
        assert_eq!(
            viewer.current().map(|item| item.message.as_str()),
            Some("m2")
        );
        assert_eq!(viewer.index, 0);
        // Clearing the open picture closes with effective shutdown.
        events
            .send(Event::ChatCleared {
                chat: "1@s.whatsapp.net".into(),
                through: 150,
            })
            .expect("sends");
        app.handle_events();
        assert!(app.viewer.is_none());
    }

    #[test]
    fn resident_history_stays_bounded_across_many_chats() {
        let root = std::env::temp_dir().join(format!("zapfast-bounded-{}", std::process::id()));
        let (mut app, events) = App::headless(AppDirs::under(&root), Settings::default());
        // Thirty chats with two hundred messages each, opened in turn.
        for n in 0..30 {
            let chat = format!("{n}@s.whatsapp.net");
            app.chats.push(Chat::new(chat.clone(), "Peer".into()));
            let rows: Vec<Message> = (1..=200)
                .map(|m| message(&chat, &format!("m{m}"), m as i64))
                .collect();
            app.conversations
                .entry(chat.clone())
                .or_default()
                .merge(rows, false);
            app.open_chat(chat);
        }
        let total: usize = app.conversations.values().map(|c| c.messages.len()).sum();
        assert!(
            total <= 200 + 10 * 120,
            "bounded no matter the visits: {total}"
        );
        assert!(app.conversations.len() <= 11);
        // A trimmed survivor refetches on reopen...
        let kept = app
            .conversations
            .get("25@s.whatsapp.net")
            .expect("recent stays");
        assert_eq!(kept.messages.len(), 120);
        assert!(!kept.requested, "reopen reloads");
        // ...and an evicted chat rebuilds through the real event path.
        assert!(!app.conversations.contains_key("0@s.whatsapp.net"));
        app.open_chat("0@s.whatsapp.net".into());
        assert!(app.conversations.contains_key("0@s.whatsapp.net"));
        let rows: Vec<Message> = (141..=200)
            .map(|m| message("0@s.whatsapp.net", &format!("m{m}"), m as i64))
            .collect();
        events
            .send(Event::Messages {
                chat: "0@s.whatsapp.net".into(),
                messages: rows,
                older: false,
                complete: true,
            })
            .expect("sends");
        app.handle_events();
        assert_eq!(
            app.conversations
                .get("0@s.whatsapp.net")
                .expect("rebuilt")
                .messages
                .len(),
            60
        );
    }

    #[test]
    fn live_replace_moves_changed_timestamps() {
        let mut conversation = Conversation::default();
        conversation.merge(
            vec![
                message("c", "a", 1),
                message("c", "b", 2),
                message("c", "c", 3),
            ],
            true,
        );
        // Same id, new timestamp: reinserted in order, never duplicated.
        conversation.merge(vec![message("c", "b", 5)], false);
        let ids: Vec<&str> = conversation
            .messages
            .iter()
            .map(|message| message.id.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "c", "b"]);
        conversation.merge(vec![message("c", "b", 5)], false);
        assert_eq!(conversation.messages.len(), 3);
    }

    #[test]
    fn same_second_messages_keep_every_row() {
        let mut conversation = Conversation::default();
        let page: Vec<Message> = (0..5)
            .map(|n| message("c", &format!("m{n}"), 100))
            .collect();
        conversation.merge(page, true);
        assert_eq!(conversation.messages.len(), 5);
        assert_eq!(conversation.ids.len(), 5);
        conversation.merge(vec![message("c", "live", 100)], false);
        assert_eq!(conversation.messages.len(), 6);
        assert_eq!(conversation.ids.len(), 6);
    }

    #[test]
    fn overlapping_pages_dedupe_without_holes() {
        let mut conversation = Conversation::default();
        let first: Vec<Message> = (1..=60)
            .map(|n| message("c", &format!("m{n}"), n as i64))
            .collect();
        conversation.merge(first, true);
        let second: Vec<Message> = (40..=100)
            .map(|n| message("c", &format!("m{n}"), n as i64))
            .collect();
        conversation.merge(second, true);
        assert_eq!(conversation.messages.len(), 100);
        assert_eq!(conversation.ids.len(), 100);
        let ids: Vec<String> = conversation.messages.iter().map(|m| m.id.clone()).collect();
        let expected: Vec<String> = (1..=100).map(|n| format!("m{n}")).collect();
        assert_eq!(ids, expected);
    }

    #[test]
    fn clear_during_load_keeps_newer_and_drops_ids() {
        let mut conversation = Conversation::default();
        let rows: Vec<Message> = (1..=10)
            .map(|n| message("c", &format!("m{n}"), n as i64))
            .collect();
        conversation.merge(rows, true);
        let removed = conversation.forget_range(5);
        assert_eq!(removed.len(), 5);
        assert_eq!(conversation.messages.len(), 5);
        assert_eq!(conversation.ids.len(), 5);
        assert!(conversation.message("m3").is_none());
        assert!(conversation.message("m8").is_some());
    }

    #[test]
    fn delete_local_drops_the_resident_id() {
        let root = std::env::temp_dir().join(format!("zapfast-delete-id-{}", std::process::id()));
        let (mut app, _events) = App::headless(AppDirs::under(&root), Settings::default());
        let chat = String::from("c@s.whatsapp.net");
        app.conversations.entry(chat.clone()).or_default().merge(
            vec![message(&chat, "gone", 1), message(&chat, "kept", 2)],
            true,
        );
        app.delete_message_local(&chat, "gone");
        let conversation = app.conversations.get(&chat).expect("chat stays");
        assert!(conversation.message("gone").is_none());
        assert!(!conversation.ids.contains("gone"));
        assert!(conversation.message("kept").is_some());
    }

    #[test]
    fn deleted_event_clears_open_closed_and_archived_views() {
        let root =
            std::env::temp_dir().join(format!("zapfast-deleted-views-{}", std::process::id()));
        let (mut app, events) = App::headless(AppDirs::under(&root), Settings::default());
        for chat in ["a@s.whatsapp.net", "c@s.whatsapp.net"] {
            let mut known = Chat::new(chat.into(), "Peer".into());
            if chat.starts_with("c") {
                known.archived = true;
            }
            app.chats.push(known);
            app.conversations.entry(chat.to_owned()).or_default().merge(
                vec![message(chat, "gone", 1), message(chat, "kept", 2)],
                true,
            );
        }
        app.open_chat = Some("a@s.whatsapp.net".into());
        for (chat, id) in [
            ("a@s.whatsapp.net", "gone"),
            ("b@s.whatsapp.net", "gone"),
            ("c@s.whatsapp.net", "gone"),
        ] {
            events
                .send(Event::MessageDeleted {
                    chat: chat.into(),
                    id: id.into(),
                })
                .expect("sends");
        }
        app.handle_events();
        for chat in ["a@s.whatsapp.net", "c@s.whatsapp.net"] {
            let conversation = app.conversations.get(chat).expect("chat stays");
            assert!(conversation.message("gone").is_none());
            assert!(conversation.message("kept").is_some());
        }
    }

    #[test]
    fn ctrl_released_before_v_still_pastes() {
        // The integration consumes every Ctrl+V press: the press frame
        // carries nothing, Ctrl goes up, and the release arrives without
        // command. Only the missing typed V tells it apart from typing.
        let (mut app, ctx) = paste_app();
        let image = ten_by_ten(7);
        paste_frame(&mut app, &ctx, vec![], true, Some(image.clone()));
        assert!(app.pending.is_empty(), "the press frame carries nothing");
        paste_frame(
            &mut app,
            &ctx,
            vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)],
            false,
            Some(image.clone()),
        );
        assert!(app.pending.is_empty(), "Ctrl going up pastes nothing yet");
        paste_frame(
            &mut app,
            &ctx,
            vec![paste_release_plain()],
            false,
            Some(image),
        );
        assert_eq!(
            app.pending.len(),
            1,
            "the release without typed V is its own gesture"
        );
    }

    #[test]
    fn release_then_paste_same_frame_counts_once_each() {
        let (mut app, ctx) = paste_app();
        let image = ten_by_ten(7);
        paste_frame(
            &mut app,
            &ctx,
            vec![egui::Event::Paste("https://example.com/first.png".into())],
            true,
            Some(image.clone()),
        );
        assert_eq!(app.pending.len(), 1);
        // The previous release and the next Paste share one frame: the
        // release must still see the armed flag before the Paste re-arms.
        paste_frame(
            &mut app,
            &ctx,
            vec![
                paste_release(),
                egui::Event::Paste("https://example.com/second.png".into()),
            ],
            true,
            Some(image.clone()),
        );
        assert_eq!(app.pending.len(), 2);
        paste_frame(&mut app, &ctx, vec![paste_release()], true, Some(image));
        assert_eq!(
            app.pending.len(),
            2,
            "the second release ends its own gesture"
        );
    }

    #[test]
    fn paste_and_release_same_frame_counts_once() {
        // The release belongs to the Paste right before it in the same
        // frame: the arm must advance inside the fold, not after it.
        let (mut app, ctx) = paste_app();
        let image = ten_by_ten(7);
        paste_frame(
            &mut app,
            &ctx,
            vec![
                egui::Event::Paste("https://example.com/pic.png".into()),
                paste_release(),
            ],
            true,
            Some(image.clone()),
        );
        assert_eq!(app.pending.len(), 1, "one gesture, one attachment");
        paste_frame(&mut app, &ctx, vec![paste_release()], true, Some(image));
        assert_eq!(
            app.pending.len(),
            2,
            "the next release is a gesture of its own"
        );
    }

    #[test]
    fn failed_image_read_attaches_nothing() {
        // A Paste whose clipboard has no image must flow to the draft,
        // and its release must end the gesture quietly either way.
        let (mut app, ctx) = paste_app();
        paste_frame(
            &mut app,
            &ctx,
            vec![egui::Event::Paste("hello".into())],
            true,
            None,
        );
        assert!(app.pending.is_empty());
        assert!(
            app.composer.contains("hello"),
            "text still reaches the draft"
        );
        paste_frame(&mut app, &ctx, vec![paste_release()], true, None);
        assert!(app.pending.is_empty(), "no image, no attachment");
        let image = ten_by_ten(7);
        paste_frame(&mut app, &ctx, vec![paste_release()], true, Some(image));
        assert_eq!(app.pending.len(), 1, "later pastes still work");
    }

    #[test]
    fn typed_v_then_shortcut_still_pastes() {
        // Typing v completes first: its release ends the typing, so a
        // later shortcut starts clean and attaches.
        let (mut app, ctx) = paste_app();
        let image = ten_by_ten(7);
        paste_frame(
            &mut app,
            &ctx,
            vec![paste_press_plain()],
            false,
            Some(image.clone()),
        );
        paste_frame(
            &mut app,
            &ctx,
            vec![paste_release_plain()],
            false,
            Some(image.clone()),
        );
        assert!(app.pending.is_empty(), "typing alone attaches nothing");
        paste_frame(&mut app, &ctx, vec![], true, Some(image.clone()));
        paste_frame(&mut app, &ctx, vec![paste_release()], true, Some(image));
        assert_eq!(app.pending.len(), 1, "the shortcut survives typing");
    }

    #[test]
    fn held_v_with_late_ctrl_pastes_nothing() {
        // Press V without Ctrl, hold it, press Ctrl, release V: no V press
        // ever happened under Ctrl, so the release ends the typing hold
        // instead of inventing a shortcut.
        let (mut app, ctx) = paste_app();
        let image = ten_by_ten(7);
        paste_frame(
            &mut app,
            &ctx,
            vec![paste_press_plain()],
            false,
            Some(image.clone()),
        );
        paste_frame(&mut app, &ctx, vec![], true, Some(image.clone()));
        paste_frame(
            &mut app,
            &ctx,
            vec![paste_release_plain()],
            true,
            Some(image.clone()),
        );
        assert!(
            app.pending.is_empty(),
            "no shortcut without a press under Ctrl"
        );
        // And the typing flag is spent: a later real shortcut still works.
        paste_frame(&mut app, &ctx, vec![], true, Some(image.clone()));
        paste_frame(&mut app, &ctx, vec![paste_release()], true, Some(image));
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn modifier_churn_inside_frame_keeps_keyboard_origin() {
        // Ctrl goes up inside the Paste frame, after the Paste event:
        // the origin is the latch at Paste time, not the end state.
        let (mut app, ctx) = paste_app();
        let image = ten_by_ten(7);
        paste_frame(
            &mut app,
            &ctx,
            vec![
                egui::Event::Paste("https://example.com/pic.png".into()),
                egui::Event::ModifiersChanged(egui::Modifiers::NONE),
            ],
            true,
            Some(image.clone()),
        );
        assert_eq!(app.pending.len(), 1);
        paste_frame(&mut app, &ctx, vec![paste_release()], true, Some(image));
        assert_eq!(
            app.pending.len(),
            1,
            "the release belongs to the armed Paste"
        );
    }

    #[test]
    fn plain_v_typing_pastes_nothing() {
        let (mut app, ctx) = paste_app();
        let image = ten_by_ten(7);
        paste_frame(
            &mut app,
            &ctx,
            vec![paste_press_plain()],
            false,
            Some(image.clone()),
        );
        paste_frame(
            &mut app,
            &ctx,
            vec![paste_release_plain()],
            false,
            Some(image.clone()),
        );
        assert!(app.pending.is_empty(), "typing v is not a paste gesture");
        paste_frame(&mut app, &ctx, vec![], true, Some(image.clone()));
        paste_frame(&mut app, &ctx, vec![paste_release()], true, Some(image));
        assert_eq!(app.pending.len(), 1, "a later shortcut still works");
    }

    #[test]
    fn switch_between_press_and_release_attaches_in_current_chat() {
        let (mut app, ctx) = paste_app();
        let image = ten_by_ten(7);
        // The press itself is consumed by the integration: this frame is
        // what an image-only Ctrl+V really delivers before the release.
        paste_frame(&mut app, &ctx, vec![], true, Some(image.clone()));
        app.chats
            .push(Chat::new("2@s.whatsapp.net".into(), "Bob".into()));
        app.open_chat = Some("2@s.whatsapp.net".into());
        paste_frame(&mut app, &ctx, vec![paste_release()], true, Some(image));
        assert_eq!(app.pending.len(), 1, "the release stages for the open chat");
    }

    #[test]
    fn pinned_order_survives_new_messages_and_legacy_pin_ties() {
        let mut app = app();
        for (id, pin, activity) in [("a", 100, 999), ("b", 200, 1), ("c", 0, 0), ("d", 0, 900)] {
            let mut chat = Chat::new(id.into(), id.into());
            chat.pinned = true;
            chat.pinned_at = pin;
            chat.last_activity = activity;
            app.chats.push(chat);
        }
        let order = |app: &App| {
            app.visible_chats()
                .iter()
                .map(|chat| chat.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&app), ["b", "a", "c", "d"]);
        app.chats[0].last_activity = 10_000;
        app.chats[2].last_activity = 20_000;
        assert_eq!(order(&app), ["b", "a", "c", "d"]);
    }

    #[test]
    fn chat_and_contact_search_ignore_composed_and_decomposed_accents() {
        let mut app = app();
        app.chats
            .push(Chat::new("1@s.whatsapp.net".into(), "Ángel".into()));
        let contact = Contact {
            id: "2@s.whatsapp.net".into(),
            full_name: Some("A\u{301}ngel".into()),
            push_name: None,
        };
        app.contacts.insert(contact.id.clone(), contact);
        for query in ["angel", "ÁNGEL", "A\u{301}ngel"] {
            app.search = query.into();
            assert_eq!(app.visible_chats().len(), 1, "{query}");
            assert_eq!(app.matching_contacts().len(), 1, "{query}");
        }
        assert_eq!(app.chats[0].name, "Ángel");
        app.search = "bob".into();
        assert!(app.visible_chats().is_empty());
        assert!(app.matching_contacts().is_empty());
    }

    #[test]
    fn closing_a_chat_preserves_its_text_draft() {
        let mut app = app();
        let id = "1@s.whatsapp.net";
        app.chats.push(Chat::new(id.into(), "Ada".into()));
        app.open_chat(id.into());
        app.composer = "unfinished message".into();
        app.actions.push(Action::CloseChat);
        app.apply_actions(&egui::Context::default());
        assert!(app.open_chat.is_none());
        app.open_chat(id.into());
        assert_eq!(app.composer, "unfinished message");
    }

    #[test]
    fn opening_a_chat_keeps_drafts_apart() {
        let mut app = app();
        app.chats
            .push(Chat::new("1@s.whatsapp.net".into(), "Ada".into()));
        app.chats
            .push(Chat::new("2@s.whatsapp.net".into(), "Bob".into()));
        app.open_chat("1@s.whatsapp.net".into());
        app.composer = "hello ada".into();
        app.open_chat("2@s.whatsapp.net".into());
        assert_eq!(app.composer, "");
        app.open_chat("1@s.whatsapp.net".into());
        assert_eq!(app.composer, "hello ada");
        assert_eq!(app.settings.last_chat.as_deref(), Some("1@s.whatsapp.net"));
    }

    #[test]
    fn selected_mentions_become_wire_tokens_and_context_jids() {
        let mut app = app();
        let chat_id = "123@g.us";
        let member = "491702222222@s.whatsapp.net";
        let mut chat = Chat::new(chat_id.into(), "Group".into());
        chat.participants.push(member.into());
        app.chats.push(chat);
        app.composer_mentions.push(ComposerMention {
            id: member.into(),
            name: "Mira Example".into(),
        });

        let (text, mentions) = app.encode_composer_mentions(chat_id, "hello @Mira Example".into());

        assert_eq!(text, "hello @491702222222");
        assert_eq!(mentions, vec![member]);
    }

    #[test]
    fn existing_wire_mentions_survive_an_edit() {
        let mut app = app();
        let chat_id = "123@g.us";
        let member = "491702222222@s.whatsapp.net";
        let mut chat = Chat::new(chat_id.into(), "Group".into());
        chat.participants.push(member.into());
        app.chats.push(chat);

        let (text, mentions) = app.encode_composer_mentions(chat_id, "still @491702222222!".into());

        assert_eq!(text, "still @491702222222!");
        assert_eq!(mentions, vec![member]);
    }

    #[test]
    fn editing_a_selected_name_drops_its_mention() {
        let mut app = app();
        let chat_id = "123@g.us";
        let member = "491702222222@s.whatsapp.net";
        let mut chat = Chat::new(chat_id.into(), "Group".into());
        chat.participants.push(member.into());
        app.chats.push(chat);
        app.composer_mentions.push(ComposerMention {
            id: member.into(),
            name: "Mira".into(),
        });

        let (text, mentions) = app.encode_composer_mentions(chat_id, "hello @Miranda".into());

        assert_eq!(text, "hello @Miranda");
        assert!(mentions.is_empty());
    }

    #[test]
    fn dismissing_shortcut_hints_persists_and_focusing_keeps_the_draft() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.composer = "Unsent draft".into();
        app.focus_search = true;
        app.apply(Action::HideShortcutHints, &ctx);
        assert!(!app.settings.show_shortcut_hints);
        assert!(app.settings_dirty);
        app.apply(Action::FocusComposer, &ctx);
        assert!(app.focus_composer);
        assert!(!app.focus_search);
        assert_eq!(app.composer, "Unsent draft");
    }

    #[test]
    fn returning_to_a_conversation_refocuses_the_composer() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.open_chat = Some("1@s.whatsapp.net".into());
        app.page = Page::Settings;

        app.apply(Action::Open(Page::Chats), &ctx);

        assert!(app.focus_composer);
    }

    #[test]
    fn recreating_the_window_refocuses_the_composer() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.open_chat = Some("1@s.whatsapp.net".into());

        app.attach(&ctx);

        assert!(app.focus_composer);
    }

    #[test]
    fn returning_to_a_conversation_does_not_interrupt_search() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.open_chat = Some("1@s.whatsapp.net".into());
        app.page = Page::Settings;
        app.search = "ada".into();

        app.apply(Action::Open(Page::Chats), &ctx);

        assert!(!app.focus_composer);

        app.search.clear();
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("chat-search")));
        app.dialog = Some(Dialog::About);
        app.apply(Action::CloseDialog, &ctx);
        assert!(!app.focus_composer);
    }

    #[test]
    fn names_fall_back_from_contacts_to_phones() {
        let mut app = app();
        app.contacts.insert(
            "1@s.whatsapp.net".into(),
            Contact {
                id: "1@s.whatsapp.net".into(),
                full_name: Some("Ada".into()),
                push_name: None,
            },
        );
        assert_eq!(app.display_name("1@s.whatsapp.net"), "Ada");
        assert_eq!(
            app.display_name("393331234567@s.whatsapp.net"),
            "+39 333 123 456 7"
        );
        assert_eq!(app.display_name("42@lid"), "Unknown");
        app.contacts.insert(
            "42@lid".into(),
            Contact {
                id: "42@lid".into(),
                full_name: None,
                push_name: Some("Bob".into()),
            },
        );
        assert_eq!(app.display_name("42@lid"), "~Bob");
        app.me = Some("42@lid".into());
        assert_eq!(app.display_name("42@lid"), "You");
    }
}

#[cfg(test)]
mod name_tests {
    use super::*;
    use crate::model::{Contact, Content, Delivery, MentionRef};

    fn app() -> App {
        let root = std::env::temp_dir().join(format!("zapfast-names-{}", std::process::id()));
        let (mut app, _events) = App::headless(AppDirs::under(&root), Settings::default());
        app.me = Some("15550001111@s.whatsapp.net".into());
        app.me_name = Some("Carmine".into());
        app.contacts.insert(
            "1@s.whatsapp.net".into(),
            Contact {
                id: "1@s.whatsapp.net".into(),
                full_name: Some("Ada Lovelace".into()),
                push_name: Some("Ada".into()),
            },
        );
        app.contacts.insert(
            "2@s.whatsapp.net".into(),
            Contact {
                id: "2@s.whatsapp.net".into(),
                full_name: None,
                push_name: Some("Bob".into()),
            },
        );
        app
    }

    #[test]
    fn the_setting_picks_the_source_and_the_other_fills_in() {
        let mut app = app();
        assert_eq!(app.display_name("1@s.whatsapp.net"), "Ada Lovelace");
        assert_eq!(app.display_name("2@s.whatsapp.net"), "~Bob");
        app.settings.names_from_contacts = false;
        assert_eq!(app.display_name("1@s.whatsapp.net"), "Ada");
        assert_eq!(app.display_name("2@s.whatsapp.net"), "Bob");
        assert_eq!(
            app.display_name_or("3@s.whatsapp.net", Some("Cy")),
            "Cy",
            "a name the message carried, for someone unknown"
        );
    }

    #[test]
    fn mentions_use_our_own_name_and_previews_resolve_tokens() {
        let app = app();
        assert_eq!(app.mention_name("15550001111@s.whatsapp.net"), "Carmine");
        assert_eq!(app.display_name("15550001111@s.whatsapp.net"), "You");
        assert_eq!(
            app.resolve_mention_tokens("palestra oggi? @15550001111 e @1 ?"),
            "palestra oggi? @Carmine e @1 ?",
            "a short number is not a mention"
        );
        let message = Message {
            id: "m".into(),
            chat: "1@s.whatsapp.net".into(),
            sender: "1@s.whatsapp.net".into(),
            sender_name: None,
            from_me: false,
            timestamp: 0,
            content: Content::text("ciao @15550001111"),
            status: Delivery::None,
            delivered_at: None,
            read_at: None,
            quoted: None,
            reactions: Vec::new(),
            edited: false,
            mentions: vec![MentionRef {
                user: "15550001111".into(),
                id: "15550001111@s.whatsapp.net".into(),
            }],
            forwarded: false,
            thumbnail: None,
        };
        assert_eq!(app.message_text(&message), "ciao @Carmine");
    }
}
