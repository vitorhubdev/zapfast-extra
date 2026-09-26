//! Channel bridge between the UI and asynchronous runtime.
//!
//! A dedicated tokio runtime owns the WhatsApp connection, archive, and media
//! work. Commands and events cross channels, and events wake the UI.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::model::{Chat, ChatId, Contact, Message, PollDraft, StickerPack};
use crate::paths::AppDirs;

mod read_sync;
pub(crate) mod sticker_import;
mod worker;

/// Phone-link state.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkStatus {
    Starting,
    /// Waiting for QR scanning or pairing-code acceptance.
    Unlinked {
        qr: Option<String>,
        pair_code: Option<String>,
        pairing_phone: Option<String>,
    },
    Connecting,
    Connected,
    /// Connection dropped and automatic reconnection is active.
    Disconnected {
        reason: String,
    },
    /// Device unlinked by the phone.
    LoggedOut,
    Failed(String),
}

impl LinkStatus {
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// Oldest loaded message timestamp and id used as a page boundary.
pub type PageKey = (i64, String);

#[derive(Clone, Debug)]
pub struct CreatedPoll {
    pub id: String,
    pub secret: Vec<u8>,
    pub creator: String,
    pub recipients: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum Command {
    RefreshPoll {
        chat: ChatId,
        message: String,
    },
    PollHistoryFailed {
        chat: ChatId,
        message: String,
        requested: std::time::Instant,
    },
    CreatePoll {
        chat: ChatId,
        draft: PollDraft,
    },
    PollCreated {
        chat: ChatId,
        draft: PollDraft,
        result: Result<CreatedPoll, String>,
    },
    VotePoll {
        chat: ChatId,
        message: String,
        choices: Vec<usize>,
    },
    PollVoted {
        chat: ChatId,
        message: String,
        choices: Vec<usize>,
        at: i64,
        result: Result<String, String>,
    },
    PollDecoded {
        vote: crate::archive::PollVote,
        choices: Option<Vec<usize>>,
    },
    SendText {
        chat: ChatId,
        text: String,
        quoting: Option<String>,
        mentions: Vec<String>,
    },
    /// Forwards an archived message to another chat.
    Forward {
        from_chat: ChatId,
        message: String,
        to_chat: ChatId,
    },
    /// Internal bulk-forward receipt, with sent counts.
    Forwarded {
        messages: usize,
        chats: usize,
    },
    /// Forwards selected messages to several chats, paced like a person
    /// tapping through them. The worker enforces WhatsApp's destination caps.
    ForwardMany {
        from_chat: ChatId,
        messages: Vec<String>,
        to_chats: Vec<ChatId>,
    },
    /// Updates our typing state in a chat.
    Composing {
        chat: ChatId,
        composing: bool,
    },
    /// Marks a visible chat read and optionally sends receipts.
    MarkRead {
        chat: ChatId,
        receipts: bool,
    },
    /// Result of a private read-state update to the other linked devices.
    ReadSyncFinished {
        chat: ChatId,
        through: i64,
        success: bool,
    },
    /// Loads archived chat messages before an optional boundary.
    LoadChat {
        chat: ChatId,
        before: Option<PageKey>,
    },
    /// Requests messages before the archive's earliest message.
    FetchOlder(ChatId),
    Download {
        chat: ChatId,
        message: String,
    },
    /// Requests a profile picture; `full` selects the info-dialog size.
    FetchAvatar {
        id: String,
        full: bool,
    },
    /// Loads archived messages from `id` through the current page.
    LoadUntil {
        chat: ChatId,
        id: String,
        before: PageKey,
    },
    /// Searches visible archived message text.
    SearchMessages {
        query: String,
    },
    /// A finished background search, applied only when no newer query
    /// replaced it. Keeps slow queries off the serial worker loop.
    SearchReady {
        generation: u64,
        query: String,
        chat: Option<ChatId>,
        hits: Result<Vec<Message>, String>,
    },
    /// Creates an archive chat before its first message is sent.
    EnsureChat {
        chat: ChatId,
        name: String,
    },
    /// Internal result for a failed phone-history request.
    OlderFailed {
        chat: ChatId,
        error: String,
    },
    /// Internal group-metadata failure.
    GroupInfoFailed {
        chat: ChatId,
        /// Whether the server refusal is permanent.
        permanent: bool,
    },
    EditText {
        chat: ChatId,
        id: String,
        text: String,
        mentions: Vec<String>,
    },
    Revoke {
        chat: ChatId,
        id: String,
    },
    DeleteLocal {
        chat: ChatId,
        id: String,
    },
    /// Selects and sends files with the desktop picker.
    PickFiles(ChatId),
    /// Sends files with the caption on the first.
    SendFiles {
        chat: ChatId,
        paths: Vec<PathBuf>,
        caption: Option<String>,
        mentions: Vec<String>,
    },
    /// Sends a clipboard image as straight-alpha RGBA.
    SendImage {
        chat: ChatId,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        caption: Option<String>,
        mentions: Vec<String>,
    },
    /// Syncs chat mute state. `Some(0)` is indefinite and `None` unmutes.
    SetMuted(ChatId, Option<i64>),
    /// Normalizes, encodes, and sends mono 48 kHz push-to-talk audio.
    SendVoice {
        chat: ChatId,
        samples: Vec<f32>,
        quoting: Option<String>,
    },
    /// Sends a played receipt for a voice message.
    MarkPlayed {
        chat: ChatId,
        message: String,
        sender: String,
        receipts: bool,
    },
    /// Sends a WebP sticker.
    SendSticker {
        chat: ChatId,
        path: PathBuf,
        quoting: Option<String>,
    },
    /// Saves a sticker file.
    SaveSticker {
        path: PathBuf,
    },
    /// Removes a saved sticker.
    ForgetSticker {
        path: PathBuf,
    },
    /// Selects and imports a .wastickers or zip archive.
    PickStickerArchive,
    /// Deletes an imported pack directory.
    DeleteStickerPack {
        dir: PathBuf,
    },
    /// Internal pack-import result. An empty error means the picker was canceled.
    StickerPackImported {
        result: Result<String, String>,
    },
    /// Downloads a sticker pack shared in a chat for preview.
    ViewStickerPack {
        chat: ChatId,
        message: String,
    },
    /// Internal received-pack preview result.
    StickerPackViewed {
        result: Result<(StickerPack, String), String>,
    },
    /// Copies a previewed pack into the packs folder.
    AddStickerPack {
        dir: PathBuf,
        name: String,
    },
    /// Saves a name through contact sync. `first_name` is the short display
    /// name; `to_phone` also adds it to the phone's address book.
    SaveContact {
        id: String,
        full_name: String,
        first_name: Option<String>,
        to_phone: bool,
    },
    /// Internal contact-save result.
    ContactSaved {
        id: String,
        name: String,
        error: Option<String>,
    },
    /// Checks a number, optionally saves it, and opens its chat.
    NewContact {
        phone: String,
        full_name: Option<String>,
        first_name: Option<String>,
        to_phone: bool,
    },
    /// Internal number-lookup result.
    ContactChecked {
        phone: String,
        full_name: Option<String>,
        first_name: Option<String>,
        to_phone: bool,
        registered: bool,
    },
    /// Loads recent and saved stickers for the picker.
    RecentStickers,
    /// Internal notice that new sticker previews were written.
    StickerThumbsReady,
    /// Marks a sticker as a favourite, or clears the mark.
    FavoriteSticker {
        path: PathBuf,
    },
    /// Internal result of pushing one favorite change to the phone.
    FavoritePushed {
        hash: String,
        updated_at: i64,
        result: Result<Vec<u8>, String>,
    },
    /// Internal notice that the favorite push queue drained.
    FavoritesPushed,
    /// Internal result of fetching a phone favorite file.
    FavoriteFetched {
        hash: String,
        result: Result<PathBuf, String>,
    },
    /// Searches the open chat's messages.
    SearchChat {
        chat: ChatId,
        query: String,
    },
    /// Renders one page of a PDF for the media viewer.
    RenderPdfPage {
        path: PathBuf,
        page: usize,
        width: u32,
    },
    /// Drops the PDF kept open for the viewer.
    ForgetPdf,
    /// Builds small previews of every page of the PDF on screen.
    PdfThumbs {
        path: PathBuf,
    },
    /// Collects the details of one attachment for the info dialog.
    FileInfo {
        chat: ChatId,
        message: String,
    },
    /// Puts a picture on the system clipboard.
    CopyImage {
        path: PathBuf,
    },
    /// Internal result of a clipboard copy.
    ImageCopied {
        name: String,
        error: Option<String>,
    },
    /// Manual update check from the About dialog. Unlike the daily check it
    /// always reports back, so the button never spins forever.
    CheckUpdatesNow {
        channel: crate::updates::Channel,
    },
    /// Evicts a cached file that never decodes, clears its archive record
    /// and fetches it again. Saved stickers and imported packs are the
    /// user's own files and are never touched.
    HealSticker {
        path: PathBuf,
    },
    /// Deletes a thumbnail that never decodes so it is rebuilt locally.
    /// The original sticker file is never touched.
    HealStickerThumb {
        path: PathBuf,
    },
    /// Internal thumbnail-rebuild result from a blocking task.
    ThumbHealFinished {
        path: PathBuf,
        ok: bool,
    },
    React {
        chat: ChatId,
        message: String,
        emoji: String,
    },
    SetArchived(ChatId, bool),
    /// Internal chat-setting sync result from a phone-mutation task, bound to
    /// the intent revision it attempted.
    ChatSyncFlushed {
        chat: ChatId,
        rev: i64,
        ok: bool,
    },
    SetPinned(ChatId, bool),
    PairWithPhone(String),
    /// Unlinks the device remotely and locally.
    Unlink,
    Reconnect,
    Shutdown,
    /// Internal send result.
    Sent {
        chat: ChatId,
        id: String,
        error: Option<String>,
    },
    /// Internal attachment-download result.
    Downloaded {
        chat: ChatId,
        id: String,
        result: Result<PathBuf, String>,
    },
    /// Internal recent-sticker download result.
    StickerFetched {
        hash: String,
        result: Result<PathBuf, String>,
    },
    /// Internal generated video-poster result.
    VideoPreview {
        chat: ChatId,
        id: String,
        preview: Option<Vec<u8>>,
        /// Real length in seconds, when the file told it.
        seconds: Option<u32>,
    },
    /// Internal profile-picture result.
    AvatarFetched {
        id: String,
        full: bool,
        path: Option<PathBuf>,
    },
    /// Internal retryable profile-picture failure.
    AvatarFailed {
        id: String,
        full: bool,
    },
    /// Internal account about-text result.
    MeInfo {
        about: Option<String>,
    },
    /// Internal file-picker result.
    Picked {
        chat: ChatId,
        paths: Vec<PathBuf>,
    },
    /// Asks for a destination and saves a copy of a file the app shows.
    SaveCopy {
        from: PathBuf,
    },
    /// Internal result of a save-copy request.
    CopySaved {
        saved: Option<PathBuf>,
        error: Option<String>,
    },
    /// Internal rendered PDF page.
    PdfPage {
        path: PathBuf,
        page: usize,
        width: u32,
        result: Result<crate::pdf::Page, String>,
    },
    /// Internal finished PDF previews, oldest page first.
    PdfThumbsReady {
        path: PathBuf,
        files: Vec<PathBuf>,
    },
    /// Internal uploaded attachment ready for archiving and sending.
    Outbound {
        chat: ChatId,
        row: Box<Message>,
        raw: Vec<u8>,
    },
    /// Internal send audience. The sender waits for it to be archived.
    GroupRecipients {
        chat: ChatId,
        id: String,
        recipients: Vec<String>,
        lids: Vec<(String, String)>,
        stored: tokio::sync::mpsc::UnboundedSender<bool>,
    },
    /// Internal group metadata result.
    GroupInfo {
        chat: ChatId,
        name: Option<String>,
        participants: Vec<String>,
        read_only: bool,
        community: bool,
        ephemeral_expiration: Option<u32>,
        ephemeral_setting_timestamp: Option<i64>,
    },
    /// Internal pairing-code result.
    PairCode {
        result: Result<String, String>,
    },
    /// Internal account read-receipt setting.
    ReceiptsPrivacy {
        disabled: bool,
    },
    /// Ask GitHub whether a newer release exists.
    CheckForUpdates {
        channel: crate::updates::Channel,
    },
    InspectUpdate,
    DownloadUpdate {
        release: crate::updates::Release,
        source: crate::updates::Source,
        channel: crate::updates::Channel,
    },
    InstallUpdate {
        prepared: Box<crate::updates::install::Prepared>,
        arguments: Vec<String>,
    },
}

#[derive(Debug)]
pub enum Event {
    PollCreated {
        chat: ChatId,
        error: Option<String>,
    },
    PollVoted {
        chat: ChatId,
        message: String,
        error: Option<String>,
    },
    Link(LinkStatus),
    /// Linked account identity.
    Me {
        id: String,
        name: Option<String>,
        about: Option<String>,
    },
    /// Full chat list, newest first.
    Chats(Vec<Chat>),
    ChatUpdated(Box<Chat>),
    /// Chat messages in ascending order. `older` prepends them; `complete`
    /// means the archive has no earlier rows.
    Messages {
        chat: ChatId,
        messages: Vec<Message>,
        older: bool,
        complete: bool,
    },
    MessageUpdated(Box<Message>),
    /// Files selected for the composer.
    Picked {
        chat: ChatId,
        paths: Vec<PathBuf>,
    },
    /// Result of SaveCopy: where the copy went, or why it failed. A cancelled
    /// dialog reports neither.
    CopySaved {
        saved: Option<PathBuf>,
        error: Option<String>,
    },
    /// Live incoming message for desktop notification.
    Incoming {
        chat: ChatId,
        message: Box<Message>,
    },
    Contacts(Vec<Contact>),
    /// Message search results with their query, newest first.
    SearchHits {
        query: String,
        messages: Vec<Message>,
    },
    Typing {
        chat: ChatId,
        sender: String,
        composing: bool,
    },
    Presence {
        id: String,
        online: bool,
        last_seen: Option<i64>,
    },
    Avatar {
        id: String,
        full: bool,
        path: Option<PathBuf>,
    },
    MessageDeleted {
        chat: ChatId,
        id: String,
    },
    /// A chat deleted on a linked device vanished locally: drop its row,
    /// cached conversation, and open state when it was showing.
    ChatRemoved {
        chat: ChatId,
    },
    /// A chat cleared on a linked device lost its messages through a
    /// timestamp: drop the cached conversation so it reloads from the
    /// archive.
    ChatCleared {
        chat: ChatId,
        through: i64,
    },
    /// Explicit result of one requested thumbnail rebuild: the sticker
    /// path and whether its thumbnail is ready. The picker applies this
    /// instead of inferring completion from the file.
    StickerThumb {
        path: PathBuf,
        ok: bool,
    },
    /// Saved stickers, imported packs, and recent stickers for the picker.
    Stickers {
        saved: Vec<PathBuf>,
        packs: Vec<StickerPack>,
        recent: Vec<PathBuf>,
        /// Sticker files the reader marked as favourites, newest first.
        favorites: Vec<PathBuf>,
        /// Emoji tags read from sticker files, for search.
        emojis: Vec<(PathBuf, Vec<String>)>,
    },
    /// A sticker pack shared in a chat, downloaded for preview, or why not.
    StickerPackPreview(Result<(StickerPack, String), String>),
    /// In-chat search hits, or why the search failed.
    ChatSearch {
        chat: ChatId,
        query: String,
        hits: Result<Vec<Message>, String>,
    },
    /// A rendered PDF page, or why it could not be rendered.
    PdfPage {
        path: PathBuf,
        page: usize,
        width: u32,
        result: Result<crate::pdf::Page, String>,
    },
    /// Small previews of a PDF's pages, oldest first, for the viewer strip.
    PdfThumbs {
        path: PathBuf,
        files: Vec<PathBuf>,
    },
    /// Details of one attachment, or why they could not be collected.
    FileInfo {
        chat: ChatId,
        message: String,
        result: Result<crate::model::FileInfo, String>,
    },
    /// Result of copying a picture, with the name it was copied from.
    CopyImage {
        name: String,
        error: Option<String>,
    },
    Media {
        chat: ChatId,
        message: String,
        result: Result<PathBuf, String>,
    },
    /// Link-time history sync state.
    Syncing(bool),
    /// Reported history-sync percentage.
    SyncProgress(u32),
    /// Phone-history result. `more` indicates whether another request may help.
    OlderFetched {
        chat: ChatId,
        more: bool,
    },
    /// Whether account privacy disables direct-chat read receipts.
    ReceiptsPrivacy {
        disabled: bool,
    },
    /// Number lookup succeeded and its chat can open.
    ContactReady {
        id: String,
        name: Option<String>,
    },
    /// Informational toast message.
    Info(String),
    /// A newer release than this build exists.
    UpdateAvailable {
        version: String,
        url: String,
    },
    /// A manual check found nothing newer.
    UpdateUpToDate,
    /// A manual check failed, with the reason.
    UpdateCheckFailed(String),
    UpdateSupport(Result<crate::updates::install::Installation, String>),
    UpdateProgress {
        received: u64,
        total: u64,
    },
    UpdateDownloaded(Result<Box<crate::updates::install::Prepared>, String>),
    UpdateInstalling(Result<(), String>),
    Error(String),
}

/// Cross-thread window wake handle.
#[derive(Clone, Default)]
pub struct Waker(Arc<std::sync::Mutex<Option<egui::Context>>>);

impl Waker {
    pub fn attach(&self, ctx: &egui::Context) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(ctx.clone());
    }

    pub fn detach(&self) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    pub fn wake(&self) {
        if let Some(ctx) = self.0.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            ctx.request_repaint();
        }
    }

    /// Schedules a delayed repaint.
    pub fn wake_after(&self, delay: std::time::Duration) {
        if let Some(ctx) = self.0.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            ctx.request_repaint_after(delay);
        }
    }
}

/// UI handle to the backend runtime.
pub struct Backend {
    commands: mpsc::UnboundedSender<Command>,
    events: std::sync::mpsc::Receiver<Event>,
    thread: Option<std::thread::JoinHandle<()>>,
    offline: bool,
    #[cfg(any(test, feature = "demo"))]
    demo_commands: Option<std::sync::Mutex<Vec<Command>>>,
}

impl Backend {
    pub fn spawn(dirs: AppDirs, waker: Waker) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("vespera-runtime")
            .enable_all()
            .build()
            .expect("unable to start the async runtime");
        let worker_commands = command_tx.clone();
        let thread = std::thread::Builder::new()
            .name("vespera-backend".to_string())
            .spawn(move || {
                runtime.block_on(async move {
                    worker::run(dirs, event_tx, worker_commands, command_rx, waker).await;
                });
                runtime.shutdown_timeout(Duration::from_secs(3));
            })
            .expect("unable to start the backend thread");

        Self {
            commands: command_tx,
            events: event_rx,
            thread: Some(thread),
            offline: false,
            #[cfg(any(test, feature = "demo"))]
            demo_commands: None,
        }
    }

    /// Creates a disconnected backend and event sender for demos and tests.
    pub fn detached() -> (Self, std::sync::mpsc::Sender<Event>) {
        let (command_tx, _command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        (
            Self {
                commands: command_tx,
                events: event_rx,
                thread: None,
                offline: true,
                #[cfg(any(test, feature = "demo"))]
                demo_commands: None,
            },
            event_tx,
        )
    }

    /// Records commands without a runtime or network connection.
    #[cfg(test)]
    pub(crate) fn recording() -> (Self, mpsc::UnboundedReceiver<Command>) {
        let (mut backend, _) = Self::detached();
        let (commands, inbox) = mpsc::unbounded_channel();
        backend.commands = commands;
        backend.offline = false;
        (backend, inbox)
    }

    /// Disables commands except shutdown.
    pub fn set_offline(&mut self, offline: bool) {
        self.offline = offline;
    }

    pub fn is_offline(&self) -> bool {
        self.offline
    }

    pub fn send(&self, command: Command) {
        if self.offline && !matches!(command, Command::Shutdown) {
            #[cfg(any(test, feature = "demo"))]
            if let Some(commands) = &self.demo_commands {
                commands
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(command);
            }
            return;
        }
        let _ = self.commands.send(command);
    }

    /// Captures real UI commands for an offline demo's local responder.
    #[cfg(any(test, feature = "demo"))]
    pub(crate) fn record_demo_commands(&mut self) {
        assert!(self.offline && self.thread.is_none());
        self.demo_commands = Some(Default::default());
    }

    #[cfg(any(test, feature = "demo"))]
    pub(crate) fn take_demo_commands(&self) -> Vec<Command> {
        self.demo_commands
            .as_ref()
            .map_or_else(Vec::new, |commands| {
                std::mem::take(&mut *commands.lock().unwrap_or_else(|p| p.into_inner()))
            })
    }

    pub fn poll(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }

    pub fn shutdown(&mut self) {
        self.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updates::{
        Release, Source,
        install::{Installation, Kind, Prepared},
    };

    fn release() -> Release {
        Release {
            version: "99.0.0".into(),
            url: "https://github.com/vitorhubdev/vespera-extra/releases/latest".into(),
        }
    }

    fn prepared() -> Box<Prepared> {
        Box::new(Prepared {
            installation: Installation {
                executable: std::path::PathBuf::from("/demo/vespera"),
                kind: Kind::Portable,
            },
            directory: "/demo/staging".into(),
            payload: "/demo/staging/next".into(),
            sha256: String::new(),
            version: "99.0.0".into(),
        })
    }

    #[test]
    fn detached_backends_never_run_update_commands() {
        let (mut backend, _events) = Backend::detached();
        assert!(backend.is_offline());
        // Without a recorder the commands are dropped outright.
        backend.send(Command::DownloadUpdate {
            release: release(),
            source: Source::GitHub,
            channel: crate::updates::Channel::Stable,
        });
        backend.send(Command::InstallUpdate {
            prepared: prepared(),
            arguments: Vec::new(),
        });
        assert!(backend.take_demo_commands().is_empty());
        // With a recorder they stay inside the process for the local responder.
        backend.record_demo_commands();
        backend.send(Command::DownloadUpdate {
            release: release(),
            source: Source::GitHub,
            channel: crate::updates::Channel::Stable,
        });
        backend.send(Command::InstallUpdate {
            prepared: prepared(),
            arguments: Vec::new(),
        });
        assert_eq!(backend.take_demo_commands().len(), 2);
    }
}
