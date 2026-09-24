//! Tokio worker for WhatsApp, the archive, attachments, and profile pictures.
//!
//! Messages are archived before reaching the UI. Privacy ids (`@lid`) are
//! canonicalized to phone-number ids as soon as their mapping is known.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use whatsapp_rust::download::MediaType;
use whatsapp_rust::media::{
    AudioOptions, DocumentOptions, ImageOptions, VideoOptions, audio_message, document_message,
    image_message, video_message,
};
use whatsapp_rust::pair_code::PairCodeOptions;
use whatsapp_rust::prelude::{
    Bot, BotHandle, Client, Jid, MessageBuilderExt, MessageExt, MessageField, SendOptions,
    SqliteStore, wa,
};
use whatsapp_rust::schemas;
use whatsapp_rust::send::RevokeType;
use whatsapp_rust::types::events as wa_events;
use whatsapp_rust::types::message::{MessageInfo, MessageSource};
use whatsapp_rust::types::presence::{ChatPresence, ReceiptType};
use whatsapp_rust::upload::UploadOptions;
use whatsapp_rust::wacore::download::Downloadable;
use whatsapp_rust::wacore::history_sync::{HistorySyncStream, MAX_DECOMPRESSED};
use whatsapp_rust::wacore::store::DevicePropsOverride;
use whatsapp_rust::wacore_binary::jid::JidExt;
use whatsapp_rust::waproto::buffa::Message as _;
use whatsapp_rust::{MediaRetryResult, MediaReuploadRequest};

mod link_watch;
mod poll_history;
mod polls;

use super::{Command, Event, LinkStatus, Waker, read_sync::ReadSync};
use crate::app::PAGE;
use crate::archive::Archive;
use crate::model::{
    Chat, ChatId, ChatKind, Contact, Content, Delivery, LinkPreview, Media, MentionRef, Message,
    Quoted, Reaction,
};
use crate::paths::AppDirs;

/// Delay after the last history chunk before sync is complete.
const SYNC_QUIET: Duration = Duration::from_secs(20);
/// Profile-picture cache lifetime.
const AVATAR_FRESH: Duration = Duration::from_secs(24 * 60 * 60);
const AVATAR_MISS_FRESH: Duration = Duration::from_secs(5 * 60);
/// Failed avatar tries before the worker stops retrying. Giving up reports
/// the cached photo when one is still stored, or absence when there is not.
const AVATAR_MAX_FAILURES: u32 = 3;

/// Retry state of one deferred or failed profile-picture request. Attempts
/// counts failed tries; the entry is removed only on success, absence, or
/// give-up, never when a retry is dispatched.
#[derive(Clone, Copy)]
struct AvatarRetry {
    attempts: u32,
    next_retry: Instant,
    in_flight: bool,
}

impl Default for AvatarRetry {
    fn default() -> Self {
        Self {
            attempts: 0,
            next_retry: Instant::now(),
            in_flight: false,
        }
    }
}

/// What one avatar retry entry needs on a tick.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AvatarDue {
    /// A try is already running or the deadline has not passed.
    Wait,
    /// Start another try.
    Dispatch,
    /// The failure cap is hit: report the kept photo or absence and drop
    /// the entry.
    GiveUp,
}

/// Decides one avatar retry entry without touching any state.
fn avatar_due(retry: &AvatarRetry, now: Instant) -> AvatarDue {
    if retry.in_flight || now < retry.next_retry {
        AvatarDue::Wait
    } else if retry.attempts >= AVATAR_MAX_FAILURES {
        AvatarDue::GiveUp
    } else {
        AvatarDue::Dispatch
    }
}

/// Wait before retrying a failed avatar lookup, by failure count.
fn avatar_backoff(failures: u32) -> Duration {
    match failures {
        0 => Duration::ZERO,
        1 => Duration::from_secs(30),
        2 => Duration::from_secs(2 * 60),
        _ => Duration::from_secs(10 * 60),
    }
}
/// Phone history-request timeout.
const PHONE_PATIENCE: Duration = Duration::from_secs(30);
/// Phone history-request batch size.
const PHONE_BATCH: i32 = 50;
/// `HistorySync.sync_type` for on-demand history responses.
const ON_DEMAND: i32 = 6;
/// Maximum attachment-preview dimension.
const THUMBNAIL_SIDE: u32 = 96;
/// Concurrent downloads across media and stickers.
///
/// A picker page fills in from a few streams instead of a burst, so the
/// server never sees a spike and the tiles arrive progressively.
const DOWNLOAD_SLOTS: usize = 4;
/// Stickers fetched per picker round.
///
/// The next round starts as soon as one of these lands, so a large library
/// fills in without ever asking for everything at once.
const STICKER_ROUND: usize = 10;
/// Quiet attempts for one sticker before the picker leaves it alone.
const STICKER_TRIES: u32 = 3;
/// Hits an in-chat search reports at most.
const CHAT_SEARCH_LIMIT: usize = 60;

/// Pause between bulk-forwarded messages. One paced stream through the same
/// single-forward path never looks like a burst to the server.
const FORWARD_PACE: Duration = Duration::from_millis(250);

/// Quiet repeats of a media download before the bubble reports a failure.
///
/// Network hiccups are common and invisible to the reader: the picture keeps
/// its loading state while these run, and only the last failure is shown.
const MEDIA_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(8),
    Duration::from_secs(20),
];

/// Retry wait while an archive intent stays queued without a client.
const OFFLINE_SYNC_RETRY: Duration = Duration::from_secs(30);

/// Bounds one pump tick across every chat.
const MAX_SYNC_DISPATCH_PER_ROUND: usize = 4;

/// Bounds simultaneous archive-sync flights across every chat. Direct
/// callers share this ceiling with the pump through prepare_sync.
const MAX_SYNC_IN_FLIGHT_TOTAL: usize = 8;

/// Background searches running at once. A third query waits coalesced:
/// the panel only ever shows the newest answer, so intermediate queries
/// die by generation instead of spawning unbounded blocking tasks.
const MAX_SEARCH_IN_FLIGHT: usize = 2;

/// A reserved archive-sync dispatch: decided centrally, then launched.
#[derive(Debug, Clone)]
struct SyncJob {
    chat: ChatId,
    archived: bool,
    rev: i64,
}

/// Backoff between archive-sync attempts while the link stays up: 5 s, 15 s,
/// 45 s, then 300 s up to the eighth failure. None stops scheduling: the
/// queue survives for reconnects and fresh intents.
fn retry_delay(attempts: u8) -> Option<Duration> {
    match attempts {
        1 => Some(Duration::from_secs(5)),
        2 => Some(Duration::from_secs(15)),
        3 => Some(Duration::from_secs(45)),
        4..=8 => Some(Duration::from_secs(300)),
        _ => None,
    }
}

fn account_allows_receipts(
    settings: &whatsapp_rust::wacore::iq::privacy::PrivacySettingsResponse,
) -> bool {
    use whatsapp_rust::wacore::iq::privacy::{PrivacyCategory, PrivacyValue};
    matches!(
        settings.get_value(&PrivacyCategory::ReadReceipts),
        Some(PrivacyValue::All)
    )
}

/// The library's persisted privacy value is refreshed during connection setup,
/// which can finish after messages arrive and does not track later phone edits.
/// Check the account before disclosing a read/play; an unavailable setting is
/// not permission to send a receipt. Chat-state sync does not use this gate.
async fn receipts_allowed(
    client: &Client,
    jid: &Jid,
    commands: &mpsc::UnboundedSender<Command>,
) -> bool {
    if jid.is_group() {
        return true;
    }
    match client.fetch_privacy_settings().await {
        Ok(settings) => {
            let allowed = account_allows_receipts(&settings);
            let _ = commands.send(Command::ReceiptsPrivacy { disabled: !allowed });
            allowed
        }
        Err(error) => {
            log::debug!("receipt withheld: account privacy unavailable: {error}");
            false
        }
    }
}

/// Downloadable recent sticker from the phone.
struct PhoneSticker(wa::StickerMetadata);

impl Downloadable for PhoneSticker {
    fn direct_path(&self) -> Option<&str> {
        self.0.direct_path.as_deref()
    }

    fn media_key(&self) -> Option<&[u8]> {
        self.0.media_key.as_deref()
    }

    fn file_enc_sha256(&self) -> Option<&[u8]> {
        self.0.file_enc_sha256.as_deref()
    }

    fn file_sha256(&self) -> Option<&[u8]> {
        self.0.file_sha256.as_deref()
    }

    fn file_length(&self) -> Option<u64> {
        self.0.file_length
    }

    fn app_info(&self) -> MediaType {
        MediaType::Sticker
    }
}

/// Whether a failed download is worth repeating quietly.
///
/// Expired media (403/404/410, already re-requested once) and the terminal
/// message the downloader reports for it are final. Anything else can be a
/// dropped connection or a busy server and deserves another try.
fn retriable_download(error: &str) -> bool {
    const FINAL: [&str; 6] = [
        "403",
        "404",
        "410",
        "No longer available",
        // Without the keys in the archived message no attempt can succeed.
        "keys are missing",
        // A message with no direct path (a view-once payload, for example)
        // cannot be fetched here at all.
        "Missing direct_path",
    ];
    !FINAL.iter().any(|code| error.contains(code))
}

/// App version in WhatsApp device-property format.
/// Uses the ZapExt fork version so new pairings show the fork identity;
/// existing pairings keep their old name until relinking (see README Files).
fn app_version() -> wa::device_props::AppVersion {
    let mut parts = crate::updates::zapext_version()
        .split('.')
        .map(|part| part.parse::<u32>().ok());
    wa::device_props::AppVersion {
        primary: parts.next().flatten(),
        secondary: parts.next().flatten(),
        tertiary: parts.next().flatten(),
        ..Default::default()
    }
}

/// Whether a recorded sticker file is still on disk.
///
/// The cache directory can be cleared between runs, so a recorded path whose
/// file is gone counts as missing and the sticker is fetched again instead of
/// staying invisible in the picker.
fn sticker_file(path: &Option<PathBuf>) -> bool {
    path.as_ref().is_some_and(|path| path.exists())
}

/// The phone's stickers that still need a file, in fetch order.
///
/// A recorded path whose file is gone counts as missing, a sticker already in
/// flight is skipped, one that keeps failing is left alone for now, and the
/// round is capped so the picker fills in steadily instead of asking for a
/// whole library at once.
fn stickers_to_fetch(
    stickers: Vec<crate::archive::PhoneSticker>,
    busy: &HashSet<String>,
    tries: &HashMap<String, u32>,
) -> Vec<crate::archive::PhoneSticker> {
    stickers
        .into_iter()
        .filter(|sticker| !sticker_file(&sticker.path))
        .filter(|sticker| !busy.contains(&sticker.hash))
        .filter(|sticker| tries.get(&sticker.hash).copied().unwrap_or(0) < STICKER_TRIES)
        .take(STICKER_ROUND)
        .collect()
}

/// Stable sticker hash across messages and the phone's recent list.
fn sticker_hash(sha256: Option<&[u8]>, enc_sha256: Option<&[u8]>) -> Option<String> {
    let bytes = sha256
        .filter(|bytes| !bytes.is_empty())
        .or(enc_sha256.filter(|bytes| !bytes.is_empty()))?;
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub async fn run(
    dirs: AppDirs,
    events: std::sync::mpsc::Sender<Event>,
    commands: mpsc::UnboundedSender<Command>,
    mut inbox: mpsc::UnboundedReceiver<Command>,
    waker: Waker,
) {
    let archive = loop {
        let path = dirs.archive_db();
        let opened = tokio::task::spawn_blocking(move || Archive::open(&path)).await;
        match opened {
            Ok(Ok(archive)) => break archive,
            result => {
                let error = match result {
                    Ok(Err(error)) => format!("{error:#}"),
                    Err(_) => "Archive unlock worker failed".to_owned(),
                    Ok(Ok(_)) => unreachable!(),
                };
                log::error!("could not unlock the message archive: {error}");
                let _ = events.send(Event::Link(LinkStatus::Failed(error)));
                waker.wake();
                // Do not connect with a disposable archive: history is replayed
                // only once and would be lost if the keyring were locked.
                loop {
                    match inbox.recv().await {
                        Some(Command::Reconnect) => break,
                        Some(Command::Shutdown) | None => return,
                        _ => {}
                    }
                }
            }
        }
    };
    let (wa_sender, wa_events) = mpsc::unbounded_channel();
    let mut worker = Worker {
        dirs,
        events,
        commands,
        waker,
        archive,
        client: None,
        handle: None,
        wa_sender,
        me_pn: None,
        me_lid: None,
        me_name: None,
        me_about: None,
        lid_to_pn: HashMap::new(),
        contacts: HashMap::new(),
        status: LinkStatus::Starting,
        pairing_phone: None,
        pair_code: None,
        qr: None,
        syncing: false,
        sync_deadline: None,
        group_info_requested: HashSet::new(),
        group_info_queue: std::collections::VecDeque::new(),
        group_info_tries: HashMap::new(),
        group_info_retry: Vec::new(),
        presence_subscribed: HashSet::new(),
        pending_older: HashMap::new(),
        older_warned: HashSet::new(),
        pending_avatars: HashMap::new(),
        sticker_fetches: HashSet::new(),
        sticker_downloads: HashSet::new(),
        sticker_give_up: HashSet::new(),
        download_retries: HashMap::new(),
        update_checker: crate::updates::Checker::new(),
        link_watch: Default::default(),
        sync_attempts: HashMap::new(),
        sync_in_flight: HashMap::new(),
        sync_aliases: HashMap::new(),
        sync_dispatched: HashMap::new(),
        search_generation: 0,
        search_in_flight: 0,
        search_pending: None,
        sync_retry_at: HashMap::new(),
        media_gc: Vec::new(),
        media_gc_retry: Vec::new(),
        #[cfg(any(test, feature = "demo"))]
        sync_sink: None,
        inflight_downloads: HashSet::new(),
        download_slots: Arc::new(tokio::sync::Semaphore::new(DOWNLOAD_SLOTS)),
        sticker_tries: HashMap::new(),
        favorites_pushing: false,
        favorites_again: false,
        favorite_fetches: HashSet::new(),
        emoji_cache: HashMap::new(),
        favorites_migrated: false,
        thumb_tries: HashMap::new(),
        thumb_heals: HashMap::new(),
        cache_swept: false,
        pdf: Arc::new(std::sync::Mutex::new(crate::pdf::Reader::default())),
        pdf_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        read_sync: ReadSync::default(),
        poll_decrypting: 0,
        poll_history: Default::default(),
        poll_sending: HashSet::new(),
    };
    worker.load_state();
    worker.backfill();
    worker.relocate_media();
    worker.rekey_known_chats();
    worker.preload_recent();
    worker.start_bot().await;
    let mut wa_events = wa_events;
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        let deadline = worker.sync_deadline;
        tokio::select! {
            command = inbox.recv() => {
                match command {
                    Some(Command::Shutdown) | None => break,
                    Some(command) => worker.handle_command(command).await,
                }
            }
            Some(event) = wa_events.recv() => worker.handle_wa_event(event).await,
            _ = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                worker.sync_deadline = None;
                worker.set_syncing(false);
                worker.emit_chats();
            }
            _ = tick.tick() => {
                worker.watch_link();
                worker.pump_chat_sync();
                worker.expire_older_requests();
                worker.retry_avatars();
                worker.pump_thumb_heals();
                worker.pump_group_info();
                worker.pump_read_sync();
                worker.pump_poll_votes();
                worker.pump_poll_history();
                worker.pump_cache();
                worker.pump_media_gc();
            }
        }
    }
    worker.stop_bot().await;
}

/// Sticker emoji tags by file, with the size and time they were read at.
type EmojiTags = HashMap<PathBuf, ((u64, Option<std::time::SystemTime>), Vec<String>)>;
struct Worker {
    read_sync: ReadSync,
    poll_decrypting: usize,
    poll_history: poll_history::Requests,
    poll_sending: HashSet<(ChatId, String)>,
    dirs: AppDirs,
    events: std::sync::mpsc::Sender<Event>,
    commands: mpsc::UnboundedSender<Command>,
    waker: Waker,
    archive: Archive,
    client: Option<Arc<Client>>,
    handle: Option<BotHandle>,
    wa_sender: mpsc::UnboundedSender<Arc<wa_events::Event>>,
    me_pn: Option<String>,
    me_lid: Option<String>,
    me_name: Option<String>,
    me_about: Option<String>,
    /// Privacy-id user part to phone-number user part.
    lid_to_pn: HashMap<String, String>,
    contacts: HashMap<String, Contact>,
    status: LinkStatus,
    pairing_phone: Option<String>,
    pair_code: Option<String>,
    qr: Option<String>,
    syncing: bool,
    sync_deadline: Option<Instant>,
    /// Groups queued or already requested. Queries are rate-limited.
    group_info_requested: HashSet<String>,
    /// Pending group metadata queue.
    group_info_queue: std::collections::VecDeque<String>,
    /// Group metadata attempt counts.
    group_info_tries: HashMap<String, u32>,
    /// Next retry time for failed group metadata requests.
    group_info_retry: Vec<(Instant, String)>,
    presence_subscribed: HashSet<String>,
    /// Pending phone-history request time and boundary by chat.
    pending_older: HashMap<ChatId, (Instant, super::PageKey)>,
    /// Chats already notified about a phone-history timeout.
    older_warned: HashSet<ChatId>,
    /// Deferred profile-picture requests and their retry state.
    pending_avatars: HashMap<(String, bool), AvatarRetry>,
    /// Active recent-sticker downloads by hash.
    sticker_fetches: HashSet<String>,
    /// Active chat-sticker downloads by chat and message id.
    sticker_downloads: HashSet<(ChatId, String)>,
    /// Chat stickers that used up their quiet retries this run.
    sticker_give_up: HashSet<(ChatId, String)>,
    /// Silent media retries per chat and message id.
    download_retries: HashMap<(ChatId, String), u32>,
    /// Notices a link that stays open after a sleep but carries nothing.
    link_watch: link_watch::LinkWatch,
    /// Update-listing checks: timeout, one flight at a time and an ETag
    /// cache shared across automatic and manual checks.
    update_checker: crate::updates::Checker,
    /// Failed archive-sync rounds per chat and revision. A fresh user intent
    /// drops every key of its chat; the fourth quiet failure of one revision
    /// surfaces a visible error once.
    sync_attempts: HashMap<(String, i64), u8>,
    /// Test transport: accepted dispatches land here instead of spawning a task.
    #[cfg(any(test, feature = "demo"))]
    sync_sink: Option<std::sync::mpsc::Sender<SyncJob>>,
    /// Archive-sync tasks currently in flight, keyed by their own chat and
    /// revision. Two ids of one conversation keep two tasks across a
    /// privacy-id migration; each completion settles exactly its revision,
    /// and a new dispatch waits until none remain.
    sync_in_flight: HashMap<(String, i64), ()>,
    /// Old chat id to canonical id while its dispatched tasks still fly.
    /// Completions resolve through it without consuming it, so a second
    /// task reporting under the same old id still finds its way home.
    sync_aliases: HashMap<String, String>,
    /// Newest search query issued: older background answers die on arrival
    /// instead of repainting the panel with stale hits.
    search_generation: u64,
    /// Background searches running now. At most two run at once; a third
    /// query waits coalesced instead of spawning unbounded tasks.
    search_in_flight: usize,
    /// Newest query that arrived while two searches already flew.
    search_pending: Option<(Option<ChatId>, String, usize)>,
    /// What each flying task was sent to do: (value, intent time) by
    /// (chat, revision). An echo carrying the dispatched value is the
    /// acknowledgement of that revision, never proof about a newer
    /// queued intent; only the echo own revision may settle it.
    sync_dispatched: HashMap<(String, i64), (bool, i64)>,
    /// Next retry time per chat for unconfirmed archive intents.
    sync_retry_at: HashMap<String, Instant>,
    /// Attachment files freed by deletions, reclaimed in batches on the
    /// tick instead of on the delete path. A path queued here belonged to
    /// a removed row, so no new message can reference it again; the flush
    /// still re-checks the live references before deleting anything.
    media_gc: Vec<std::path::PathBuf>,
    /// Files whose removal failed once: one more round, then the startup
    /// sweep owns the orphan instead of the tick warning forever.
    media_gc_retry: Vec<std::path::PathBuf>,
    /// Downloads already running per chat and message id. A second request
    /// for the same file does not spawn another fetch; the first
    /// completion notifies the bubble through the Downloaded command.
    inflight_downloads: HashSet<(ChatId, String)>,
    /// Limits how many downloads run at once, media and stickers alike.
    download_slots: Arc<tokio::sync::Semaphore>,
    /// Failed sticker fetches by hash, so a hopeless one is left alone.
    sticker_tries: HashMap<String, u32>,
    /// Favorite sync pushes in flight: a new change waits for the drain.
    favorites_pushing: bool,
    /// Another favorite change arrived while one pushed.
    favorites_again: bool,
    /// Favorite stickers being fetched from the phone, by content hash.
    favorite_fetches: HashSet<String>,
    /// Sticker emoji tags by file, with the size and time they were read at.
    emoji_cache: EmojiTags,
    /// Whether path-based favorites were migrated to content hashes.
    favorites_migrated: bool,
    /// Sticker previews that failed to build, so they are not retried forever.
    thumb_tries: HashMap<PathBuf, u32>,
    /// Requested thumbnail rebuilds with their retry state.
    thumb_heals: HashMap<PathBuf, ThumbHeal>,
    /// Whether the app's own cache folders were swept this run.
    cache_swept: bool,
    /// The PDF the viewer has open, kept parsed between pages.
    pdf: Arc<std::sync::Mutex<crate::pdf::Reader>>,
    /// Counts viewer render requests, so a stale prefetch stands down.
    pdf_generation: Arc<std::sync::atomic::AtomicU64>,
}

/// Decoded history chunk waiting to be canonicalized and archived.
struct ParsedHistory {
    chats: Vec<ParsedChat>,
    push_names: Vec<(String, String)>,
    lids: Vec<(String, String)>,
    /// Recent phone stickers included with history sync.
    stickers: Vec<wa::StickerMetadata>,
}

struct ParsedChat {
    id: String,
    name: Option<String>,
    unread: Option<u32>,
    /// Outer None means the history chunk omitted the archived flag.
    archived: Option<bool>,
    pinned_at: Option<i64>,
    /// Outer None means the history chunk omitted mute metadata.
    muted_until: Option<Option<i64>>,
    ephemeral_expiration: Option<u32>,
    ephemeral_setting_timestamp: Option<i64>,
    last_activity: i64,
    pn_jid: Option<String>,
    lid_jid: Option<String>,
    /// Whether the phone reports more available history.
    more_on_phone: Option<bool>,
    messages: Vec<ParsedMessage>,
    revoked: Vec<String>,
    poll_updates: Vec<HistoryPollUpdate>,
}

struct HistoryPollUpdate {
    id: String,
    sender: Option<String>,
    from_me: bool,
    timestamp: i64,
    update: wa::message::PollUpdateMessage,
}

struct ParsedMessage {
    id: String,
    sender: Option<String>,
    from_me: bool,
    push_name: Option<String>,
    timestamp: i64,
    content: Content,
    status: Delivery,
    quoted: Option<Quoted>,
    reactions: Vec<(Option<String>, bool, String)>,
    mentions: Vec<String>,
    forwarded: bool,
    thumbnail: Option<Vec<u8>>,
    raw: Vec<u8>,
    poll_secret: Option<Vec<u8>>,
    poll_votes: Vec<wa::PollUpdate>,
}

/// Blocking byte fetcher behind avatar downloads, injectable in tests.
type AvatarFetch = std::sync::Arc<dyn Fn(&str) -> Result<Vec<u8>, String> + Send + Sync>;

/// Rebuild state of one requested sticker thumbnail. Attempts counts failed
/// rebuilds; the entry lives until success, give-up, or logout.
struct ThumbHeal {
    attempts: u32,
    next_retry: Instant,
    in_flight: bool,
}

/// Failed thumbnail rebuilds before the worker reports failure.
const THUMB_HEAL_MAX_FAILURES: u32 = 3;

/// Thumbnail rebuilds running at once. Each rebuild reads, decodes, and
/// encodes on a blocking thread; the cap keeps a sticker flood from
/// starving the worker that answers every other command.
const THUMB_HEAL_SLOTS: usize = 2;

/// Wait before retrying a failed thumbnail rebuild, by failure count.
fn thumb_heal_backoff(failures: u32) -> Duration {
    match failures {
        0 => Duration::ZERO,
        1 => Duration::from_secs(10),
        2 => Duration::from_secs(60),
        _ => Duration::from_secs(5 * 60),
    }
}

/// Whether a picture reference may go to a plain HTTP client: only absolute
/// HTTP(S) URLs. Relative CDN direct paths are refused without touching the
/// network.
fn is_fetchable_avatar_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Whether downloaded bytes may become the cached avatar: non-empty and
/// decoding as an image.
fn validate_avatar_bytes(bytes: &[u8]) -> bool {
    !bytes.is_empty() && image::load_from_memory(bytes).is_ok()
}

impl Worker {
    fn ephemeral_expiration(&self, chat: &str) -> Option<u32> {
        self.archive
            .ephemeral_expiration(chat)
            .ok()
            .flatten()
            .filter(|expiration| *expiration > 0)
    }

    fn default_ephemeral_expiration(&self) -> Option<u32> {
        self.archive
            .meta("default_ephemeral_expiration")
            .ok()
            .flatten()
            .and_then(|value| value.parse().ok())
    }

    fn apply_ephemeral(&self, chat: &str, message: &mut wa::Message) -> Option<u32> {
        apply_ephemeral_expiration(message, self.ephemeral_expiration(chat))
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
        self.waker.wake();
    }

    /// Applies one confirmed remote archive state transactionally: flag, order
    /// marker, and intent cleanup persist together. Budget maps and UI update
    /// only after commit; on error the queue stays for recovery and a warning
    /// shows instead of a phantom acceptance.
    fn accept_remote_archive(&mut self, chat: &str, remote: bool, remote_ms: i64) {
        match self.archive.apply_remote_archive(chat, remote, remote_ms) {
            Ok(()) => {
                self.purge_sync_budget(chat);
                // The queue is gone: no flying task may claim its echo
                // against a future intent anymore.
                self.sync_dispatched.retain(|(id, _), _| id != chat);
                self.emit_chat(chat);
            }
            Err(error) => {
                log::warn!("could not apply a remote archive state: {error}");
                self.emit(Event::Error(
                    "Could not apply the archive change.".to_owned(),
                ));
            }
        }
    }

    /// Sends one queued archive intent, explicit signals only. Returns whether
    /// a task started. The tick uses the clock-controlled variant instead.
    fn push_archive_sync(&mut self, chat: &str) -> bool {
        self.push_archive_sync_at(chat, Instant::now(), true)
    }

    fn push_archive_sync_at(&mut self, chat: &str, now: Instant, force: bool) -> bool {
        let job = self.prepare_sync(chat, now, force);
        self.launch_sync(job, now)
    }

    /// Central dispatch decision for every caller: queued revision, one task
    /// per chat, intent budget, deadline unless forced, and a global ceiling
    /// on simultaneous flights. Reserves the winner in flight without sending.
    /// What the task flying for one chat was sent to do: its revision with
    /// the dispatched (value, intent time). Nothing when the bookkeeping
    /// never saw a dispatch, like tasks tests fly by hand.
    fn dispatched_sync(&self, chat: &str) -> Option<(i64, bool, i64)> {
        self.sync_in_flight
            .keys()
            .find(|(id, _)| id == chat)
            .and_then(|key| {
                self.sync_dispatched
                    .get(key)
                    .map(|(value, updated)| (key.1, *value, *updated))
            })
    }
    fn prepare_sync(&mut self, chat: &str, now: Instant, force: bool) -> Option<SyncJob> {
        let Ok(Some((archived, updated, rev))) = self.archive.queued_chat_sync(chat, "archived")
        else {
            return None;
        };
        if self.sync_in_flight.keys().any(|(id, _)| id == chat) {
            return None;
        }
        if !self.sync_budget_open(chat, rev) {
            return None;
        }
        if !force {
            let due = self.sync_retry_at.get(chat).is_none_or(|at| *at <= now);
            if !due {
                return None;
            }
        }
        if self.sync_in_flight.len() >= MAX_SYNC_IN_FLIGHT_TOTAL {
            return None;
        }
        self.sync_in_flight.insert((chat.to_owned(), rev), ());
        // Bind the acknowledgement to the revision actually sent: its echo
        // may be stamped later than a newer local intent without outranking
        // it, and only its own completion may settle it.
        self.sync_dispatched
            .insert((chat.to_owned(), rev), (archived, updated));
        Some(SyncJob {
            chat: chat.to_owned(),
            archived,
            rev,
        })
    }

    /// Runs a reserved job: spawns the mutation, or defers offline with a
    /// scheduled retry. Only a started task counts toward the round cap.
    fn launch_sync(&mut self, job: Option<SyncJob>, now: Instant) -> bool {
        let Some(job) = job else {
            return false;
        };
        // Test transport: count the accepted dispatch without spawning.
        #[cfg(any(test, feature = "demo"))]
        if let Some(sink) = &self.sync_sink {
            let _ = sink.send(job);
            return true;
        }
        let (Some(client), Some(jid)) = (self.client.clone(), Self::jid_of(&job.chat)) else {
            self.sync_in_flight.remove(&(job.chat.clone(), job.rev));
            self.sync_dispatched.remove(&(job.chat.clone(), job.rev));
            self.sync_retry_at
                .insert(job.chat, now + OFFLINE_SYNC_RETRY);
            return false;
        };
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let result = if job.archived {
                client.chat_actions().archive_chat(&jid, None).await
            } else {
                client.chat_actions().unarchive_chat(&jid, None).await
            };
            if let Err(error) = &result {
                log::warn!("an archive change did not reach the phone: {error}");
            }
            let _ = commands.send(Command::ChatSyncFlushed {
                chat: job.chat,
                rev: job.rev,
                ok: result.is_ok(),
            });
        });
        true
    }

    /// Whether another attempt may fly for this revision: nine failures spend
    /// the budget until the next explicit user intent resets it.
    fn sync_budget_open(&self, chat: &str, rev: i64) -> bool {
        self.sync_attempts
            .get(&(chat.to_owned(), rev))
            .is_none_or(|attempts| *attempts < 9)
    }

    fn pump_chat_sync(&mut self) {
        self.pump_chat_sync_at(Instant::now())
    }

    fn pump_chat_sync_at(&mut self, now: Instant) {
        let pending = self.archive.pending_chat_syncs().unwrap_or_default();
        let mut dispatched = 0;
        for (chat, setting) in pending {
            if setting != "archived" {
                continue;
            }
            if self.push_archive_sync_at(&chat, now, false) {
                dispatched += 1;
                if dispatched >= MAX_SYNC_DISPATCH_PER_ROUND {
                    break;
                }
            }
        }
    }

    /// Syncs a chat setting to the phone without blocking the worker.
    fn tell_phone<F, Fut>(&self, chat: &str, call: F)
    where
        F: FnOnce(Arc<Client>, Jid) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        let (Some(client), Some(jid)) = (self.client.clone(), Self::jid_of(chat)) else {
            return;
        };
        let chat = chat.to_owned();
        tokio::spawn(async move {
            if let Err(error) = call(client, jid).await {
                log::warn!("the phone was not told about {chat}: {error}");
            }
        });
    }

    fn emit_chats(&self) {
        match self.archive.chats() {
            Ok(mut chats) => {
                // Early preference sync can create an empty privacy-id row.
                // Once mapped, its preferences live on the canonical chat.
                chats.retain(|chat| chat.last.is_some() || self.canonical_str(&chat.id) == chat.id);
                for chat in &mut chats {
                    self.polish_chat(chat);
                }
                self.emit(Event::Chats(chats));
            }
            Err(error) => log::warn!("could not list chats: {error}"),
        }
    }

    fn emit_chat(&self, id: &str) {
        if let Ok(Some(mut chat)) = self.archive.chat(id) {
            self.polish_chat(&mut chat);
            self.emit(Event::ChatUpdated(Box::new(chat)));
        }
    }

    /// Resolves phone numbers in chat-row previews.
    fn polish_chat(&self, chat: &mut Chat) {
        if let Some(last) = chat.last.as_mut() {
            last.summary = self.pn_tokens(&last.summary);
        }
    }

    fn emit_message(&self, chat: &str, id: &str) {
        if let Ok(Some(mut message)) = self.archive.message(chat, id) {
            self.polish(&mut message);
            self.emit(Event::MessageUpdated(Box::new(message)));
        }
    }

    /// Reconnects a link that the machine slept under, or that has received
    /// nothing for longer than a working one can. See `link_watch`.
    fn watch_link(&mut self) {
        let client = self
            .client
            .clone()
            .filter(|_| matches!(self.status, LinkStatus::Connected));
        let frames = client.as_ref().map(|client| client.stats().frames_received);
        let verdict = self.link_watch.check(
            std::time::Instant::now(),
            std::time::SystemTime::now(),
            frames,
        );
        let Some(client) = client else {
            return;
        };
        match verdict {
            link_watch::Verdict::Healthy => return,
            link_watch::Verdict::Slept(asleep) => {
                log::info!(
                    "link: resumed after {} s asleep, reconnecting",
                    asleep.as_secs()
                );
            }
            link_watch::Verdict::Silent(quiet) => {
                log::warn!(
                    "link: nothing received for {} s, reconnecting",
                    quiet.as_secs()
                );
            }
        }
        self.set_status(LinkStatus::Connecting);
        tokio::spawn(async move { client.reconnect_immediately().await });
    }

    fn set_status(&mut self, status: LinkStatus) {
        if self.status != status {
            log::info!("link: {status:?}");
            self.status = status.clone();
            self.emit(Event::Link(status));
        }
    }

    fn set_syncing(&mut self, syncing: bool) {
        if self.syncing != syncing {
            self.syncing = syncing;
            self.emit(Event::Syncing(syncing));
        }
    }

    fn unlinked(&self) -> LinkStatus {
        LinkStatus::Unlinked {
            qr: self.qr.clone(),
            pair_code: self.pair_code.clone(),
            pairing_phone: self.pairing_phone.clone(),
        }
    }

    /// Canonical id used for our account.
    fn me(&self) -> String {
        self.me_pn
            .clone()
            .or_else(|| self.me_lid.clone())
            .unwrap_or_else(|| "me".to_owned())
    }

    fn is_me(&self, id: &str) -> bool {
        self.me_pn.as_deref() == Some(id) || self.me_lid.as_deref() == Some(id)
    }

    fn load_state(&mut self) {
        self.me_pn = self.archive.meta("me_pn").ok().flatten();
        self.me_lid = self.archive.meta("me_lid").ok().flatten();
        self.me_name = self.archive.meta("me_name").ok().flatten();
        self.me_about = self.archive.meta("me_about").ok().flatten();
        if let Ok(lids) = self.archive.lids() {
            self.lid_to_pn = lids.into_iter().collect();
        }
        if let Ok(contacts) = self.archive.contacts() {
            self.contacts = contacts
                .into_iter()
                .map(|contact| (contact.id.clone(), contact))
                .collect();
        }
        if let Some(id) = self.me_pn.clone().or_else(|| self.me_lid.clone()) {
            self.emit(Event::Me {
                id,
                name: self.me_name.clone(),
                about: self.me_about.clone(),
            });
        }
        self.emit(Event::Contacts(self.contacts.values().cloned().collect()));
        self.emit_chats();
    }

    /// Re-derives archived rows from raw protobufs after parser changes. Also
    /// repairs moved attachment paths or clears missing files for redownload.
    fn relocate_media(&mut self) {
        let dir = self.dirs.media_cache_dir();
        let rows = match self.archive.media_paths() {
            Ok(rows) => rows,
            Err(error) => {
                log::warn!("could not list attachments: {error}");
                return;
            }
        };
        let (mut moved, mut forgotten) = (0, 0);
        for (chat, id, path) in rows {
            if path.exists() {
                continue;
            }
            let candidate = path.file_name().map(|name| dir.join(name));
            match candidate.filter(|candidate| candidate.exists()) {
                Some(candidate) => {
                    if self.archive.set_media_path(&chat, &id, &candidate).is_ok() {
                        moved += 1;
                    }
                }
                None => {
                    if self.archive.clear_media_path(&chat, &id).is_ok() {
                        forgotten += 1;
                    }
                }
            }
        }
        if moved + forgotten > 0 {
            log::info!(
                "attachments: {moved} re-pointed to {}, {forgotten} to fetch again",
                dir.display()
            );
        }
    }

    fn backfill(&mut self) {
        const VERSION: &str = "2";
        if self.archive.meta("derived").ok().flatten().as_deref() == Some(VERSION) {
            return;
        }
        let rows = match self.archive.rows_with_raw() {
            Ok(rows) => rows,
            Err(error) => {
                log::warn!("could not read the archive for re-deriving: {error}");
                return;
            }
        };
        let started = Instant::now();
        let mut updated = 0;
        for (chat, id, raw) in rows {
            let Ok(message) = wa::Message::decode_from_slice(&raw) else {
                continue;
            };
            let base = message.get_base_message();
            let Some(mut content) = classify(base) else {
                continue;
            };
            let Ok(Some(existing)) = self.archive.message(&chat, &id) else {
                continue;
            };
            if matches!(existing.content, Content::Revoked) {
                continue;
            }
            if let (Some(new), Some(old)) = (content.media_mut(), existing.content.media()) {
                new.path = old.path.clone();
            }
            let mentions = self.mentions_of(&mentioned_of(base));
            let thumbnail = thumbnail_of(base);
            if self
                .archive
                .set_derived(
                    &chat,
                    &id,
                    &content,
                    &mentions,
                    thumbnail.as_deref(),
                    forwarded_of(base),
                )
                .is_ok()
            {
                updated += 1;
            }
        }
        let _ = self.archive.set_meta("derived", VERSION);
        if updated > 0 {
            log::info!(
                "re-derived {updated} archived messages in {:.1?}",
                started.elapsed()
            );
            self.emit_chats();
        }
    }

    async fn start_bot(&mut self) {
        let path = self.dirs.session_db();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let store = match SqliteStore::new(&path.to_string_lossy()).await {
            Ok(store) => store,
            Err(error) => {
                self.set_status(LinkStatus::Failed(format!(
                    "Could not open the device store: {error}"
                )));
                return;
            }
        };
        let sender = self.wa_sender.clone();
        let bot = Bot::builder()
            .with_backend(store)
            // WhatsApp reads the linked-device name, version, and icon at pairing.
            .with_device_props(
                DevicePropsOverride::new()
                    .with_os("ZapExt")
                    .with_version(app_version())
                    .with_platform_type(wa::device_props::PlatformType::DESKTOP),
            )
            .on_event(move |event, _client| {
                let sender = sender.clone();
                async move {
                    let _ = sender.send(event);
                }
            })
            .build()
            .await;
        match bot {
            Ok(bot) => {
                let handle = bot.spawn();
                self.client = Some(handle.client());
                self.handle = Some(handle);
                self.set_status(LinkStatus::Connecting);
            }
            Err(error) => self.set_status(LinkStatus::Failed(format!(
                "Could not start WhatsApp: {error}"
            ))),
        }
    }

    fn refresh_legacy_preferences(&self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        match self.archive.take_preferences_refresh() {
            Ok(true) => {
                tokio::spawn(async move {
                    use whatsapp_rust::{WAPatchName, sync_task::MajorSyncTask};
                    // The protocol library owns collection locking, full
                    // snapshots, and bounded retries across reconnects. Do
                    // not reset its store or add an application retry loop.
                    for name in [WAPatchName::RegularLow, WAPatchName::RegularHigh] {
                        client
                            .process_sync_task(MajorSyncTask::AppStateSync {
                                name,
                                full_sync: true,
                            })
                            .await;
                    }
                });
            }
            Ok(false) => {}
            Err(error) => log::warn!("could not schedule chat preference recovery: {error}"),
        }
    }

    async fn stop_bot(&mut self) {
        self.client = None;
        if let Some(handle) = self.handle.take()
            && tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
                .await
                .is_err()
        {
            log::warn!("the WhatsApp connection did not stop in time");
        }
    }

    // --- ids -------------------------------------------------------------

    fn learn_lid(&mut self, lid: &str, pn: &str) {
        if lid.is_empty() || pn.is_empty() {
            return;
        }
        if self.lid_to_pn.get(lid).is_some_and(|known| known == pn) {
            return;
        }
        self.lid_to_pn.insert(lid.to_owned(), pn.to_owned());
        let mut changed = false;
        match self.archive.put_lid(lid, pn) {
            Ok(true) => changed = true,
            Ok(false) => {}
            Err(error) => log::warn!("could not remember an id mapping: {error}"),
        }
        // Rows filed before this mapping was known keep the old id. Moving
        // them now keeps the sidebar and the open chat reading one id.
        match self
            .archive
            .rekey_chat(&format!("{lid}@lid"), &format!("{pn}@s.whatsapp.net"))
        {
            Ok(true) => {
                log::info!("chat {pn} re-filed under its phone number");
                self.adopt_rekeyed_chat(&format!("{lid}@lid"), &format!("{pn}@s.whatsapp.net"));
                changed = true;
            }
            Ok(false) => {}
            Err(error) => {
                log::warn!("could not re-file chat {pn}: {error}; will retry");
                // Keep the retry path open: without the mapping the next
                // learn repeats the migration instead of skipping it.
                self.lid_to_pn.remove(lid);
            }
        }
        if changed {
            self.emit_chats();
        }
    }

    /// Drops every attempt counter of a chat whose intent is gone, however it
    /// went: converged echo, superseding phone state, or fresh user intent.
    fn purge_sync_budget(&mut self, chat: &str) {
        self.sync_attempts.retain(|(id, _), _| id != chat);
    }

    /// Moves sync bookkeeping across a privacy-id migration. A task still
    /// flying for the old id keeps flying under the canonical id instead of
    /// racing a second dispatch: its completion settles the survivor.
    fn adopt_rekeyed_chat(&mut self, from: &str, to: &str) {
        if from == to {
            return;
        }
        // Every task keeps its own identity under the canonical id: two
        // pre-existing flights stay two, the global ceiling keeps counting
        // real tasks, and each completion settles exactly its revision.
        // Revisions never repeat, so no two tasks share one key.
        let flying: Vec<i64> = self
            .sync_in_flight
            .keys()
            .filter(|(id, _)| id == from)
            .map(|(_, rev)| *rev)
            .collect();
        for rev in flying.iter() {
            self.sync_in_flight.remove(&(from.to_owned(), *rev));
            self.sync_in_flight.insert((to.to_owned(), *rev), ());
            // The acknowledgement binding rides along: an echo of the moved
            // task still settles only its own revision under the new id.
            if let Some(sent) = self.sync_dispatched.remove(&(from.to_owned(), *rev)) {
                self.sync_dispatched.insert((to.to_owned(), *rev), sent);
            }
        }
        if !flying.is_empty() {
            self.sync_aliases.insert(from.to_owned(), to.to_owned());
        }
        // Budgets ride with revisions, so nothing transfers: the moved intent
        // takes a fresh revision on write and starts with a clean budget. Only
        // dead keys are pruned here.
        self.sync_attempts.retain(|(id, _), _| id != from);
        let survived = self
            .archive
            .queued_chat_sync(to, "archived")
            .ok()
            .flatten()
            .is_some();
        if survived {
            if !self.sync_retry_at.contains_key(to) {
                if let Some(at) = self.sync_retry_at.remove(from) {
                    self.sync_retry_at.insert(to.to_owned(), at);
                }
            } else {
                self.sync_retry_at.remove(from);
            }
            // Not a fresh click: respect the inherited deadline like the tick.
            self.push_archive_sync_at(to, Instant::now(), false);
        } else {
            self.sync_retry_at.remove(from);
        }
    }

    fn learn_pair(&mut self, a: &Jid, b: &Jid) {
        if a.is_lid() && b.is_pn() {
            self.learn_lid(a.user_base(), b.user_base());
        } else if a.is_pn() && b.is_lid() {
            self.learn_lid(b.user_base(), a.user_base());
        }
    }

    fn learn_source(&mut self, source: &MessageSource) {
        if let Some(alt) = &source.sender_alt {
            let sender = source.sender.clone();
            self.learn_pair(&sender, alt);
        }
        if let Some(alt) = &source.recipient_alt {
            let chat = source
                .recipient
                .clone()
                .unwrap_or_else(|| source.chat.clone());
            self.learn_pair(&chat, alt);
        }
    }

    /// Returns the archive id for a JID, resolving known privacy ids.
    fn canonical(&self, jid: &Jid) -> String {
        if jid.is_lid()
            && let Some(pn) = self.lid_to_pn.get(jid.user_base())
        {
            let pn = format!("{pn}@s.whatsapp.net");
            return if self.is_me(&pn) { self.me() } else { pn };
        }
        let id = jid.to_non_ad_string();
        if self.is_me(&id) {
            return self.me();
        }
        id
    }

    fn canonical_str(&self, id: &str) -> String {
        match id.parse::<Jid>() {
            Ok(jid) => self.canonical(&jid),
            Err(_) => id.to_owned(),
        }
    }
    /// The other id a chat may be filed under while a privacy mapping settles.
    ///
    /// Rows written before a LID-to-number mapping was known keep the old id,
    /// so reads check both until the background rekey finishes the move.
    fn alt_chat_id(&self, chat: &str) -> Option<String> {
        if let Some(lid) = chat.strip_suffix("@lid") {
            let pn = self.lid_to_pn.get(lid)?;
            return Some(format!("{pn}@s.whatsapp.net"));
        }
        if let Some(pn) = chat.strip_suffix("@s.whatsapp.net") {
            let lid = self.lid_to_pn.iter().find(|(_, known)| *known == pn)?.0;
            return Some(format!("{lid}@lid"));
        }
        None
    }

    fn jid_of(id: &str) -> Option<Jid> {
        id.parse().ok()
    }

    // --- names -----------------------------------------------------------

    fn contact_name(&self, id: &str) -> Option<String> {
        self.contacts.get(id).and_then(Contact::label)
    }

    /// Resolves a name for a quote or mention.
    fn name_for(&self, id: &str) -> Option<String> {
        if self.is_me(id) || id == self.me() {
            return Some("You".to_owned());
        }
        if let Some(name) = self.contact_name(id) {
            return Some(name);
        }
        crate::model::phone_of(id).map(crate::util::phone)
    }

    /// Returns the best current chat name.
    ///
    /// WhatsApp shows the address-book name first, then the profile name its
    /// owner chose (marked with a tilde), and only then the number itself.
    fn chat_name(&self, id: &str, push_name: Option<&str>) -> String {
        if id == self.me() {
            return "You".to_owned();
        }
        if let Some(name) = self
            .contacts
            .get(id)
            .and_then(|contact| contact.full_name.clone())
            .filter(|name| !name.is_empty())
        {
            return name;
        }
        if let Some(name) = push_name
            .filter(|name| !name.is_empty())
            .or_else(|| self.contacts.get(id)?.push_name.as_deref())
            .filter(|name| !name.is_empty())
        {
            return format!("~{name}");
        }
        if let Some(digits) = crate::model::phone_of(id) {
            return crate::util::phone(digits);
        }
        fallback_name(id)
    }

    fn remember_push_name(&mut self, id: &str, push_name: &str) {
        if push_name.is_empty() || id == self.me() {
            return;
        }
        let contact = self
            .contacts
            .entry(id.to_owned())
            .or_insert_with(|| Contact {
                id: id.to_owned(),
                full_name: None,
                push_name: None,
            });
        if contact.push_name.as_deref() == Some(push_name) {
            return;
        }
        contact.push_name = Some(push_name.to_owned());
        let contact = contact.clone();
        if let Err(error) = self.archive.upsert_contact(&contact) {
            log::warn!("could not save a contact: {error}");
        }
        self.emit(Event::Contacts(vec![contact]));
        self.refresh_chat_name(id);
    }

    /// Replaces a fallback chat name when a better one is known.
    fn refresh_chat_name(&mut self, id: &str) {
        let Ok(Some(chat)) = self.archive.chat(id) else {
            return;
        };
        if chat.kind == ChatKind::Group {
            return;
        }
        let name = self.chat_name(id, None);
        if name != chat.name {
            let _ = self.archive.rename_chat(id, &name);
            self.emit_chat(id);
        }
    }

    fn ensure_chat(&mut self, id: &str, push_name: Option<&str>) {
        match self.archive.chat(id) {
            Ok(Some(chat)) => {
                if chat.kind != ChatKind::Group {
                    let name = self.chat_name(id, push_name);
                    if name != chat.name {
                        let _ = self.archive.rename_chat(id, &name);
                    }
                }
            }
            Ok(None) => {
                let name = self.chat_name(id, push_name);
                if let Err(error) = self.archive.ensure_chat(id, &name) {
                    log::warn!("could not create chat {id}: {error}");
                }
            }
            Err(error) => log::warn!("could not read chat {id}: {error}"),
        }
        if ChatKind::from_id(id) == ChatKind::Group {
            self.request_group_info(id, false);
        }
    }

    /// Queues a group metadata request, at the front when `force` is true.
    fn request_group_info(&mut self, id: &str, force: bool) {
        if force {
            self.group_info_requested.remove(id);
        } else {
            let known = self.archive.chat(id).ok().flatten().is_some_and(|chat| {
                chat.name != fallback_name(id) && !chat.participants.is_empty()
            });
            if known {
                return;
            }
        }
        // A forced request must not leave an older entry behind: without this
        // the same group would be queried twice on the next tick, which is
        // exactly the burst the per-tick limit exists to avoid.
        if force {
            self.group_info_queue.retain(|queued| queued != id);
        }
        if !self.group_info_requested.insert(id.to_owned()) {
            return;
        }
        if force {
            self.group_info_queue.push_front(id.to_owned());
        } else {
            self.group_info_queue.push_back(id.to_owned());
        }
    }

    /// Group metadata retry delay.
    fn group_retry_delay(tries: u32) -> Duration {
        Duration::from_secs(30 * 2u64.pow(tries.saturating_sub(1).min(5)))
            .min(Duration::from_secs(600))
    }

    /// Sends a limited number of group metadata requests per tick.
    fn pump_group_info(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = {
            let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.group_info_retry)
                .into_iter()
                .partition(|(at, _)| *at <= now);
            self.group_info_retry = later;
            due.into_iter().map(|(_, id)| id).collect()
        };
        for id in due {
            if self.group_info_requested.insert(id.clone()) {
                self.group_info_queue.push_back(id);
            }
        }
        for _ in 0..2 {
            let Some(id) = self.group_info_queue.pop_front() else {
                return;
            };
            // A late failure can requeue a group deleted in the meantime.
            if self.archive.removal_point(&id).ok().flatten().is_some()
                && self.archive.chat(&id).ok().flatten().is_none()
            {
                self.group_info_requested.remove(&id);
                continue;
            }
            self.query_group_info(&id);
        }
    }

    /// Schedules metadata retry with backoff, or stops on permanent failure.
    fn handle_failed_group(&mut self, chat: String, permanent: bool) {
        self.group_info_requested.remove(&chat);
        if permanent {
            self.group_info_tries.remove(&chat);
        } else {
            let tries = self.group_info_tries.entry(chat.clone()).or_insert(0);
            *tries += 1;
            if *tries <= 7 {
                self.group_info_retry
                    .push((Instant::now() + Self::group_retry_delay(*tries), chat));
            }
        }
    }

    /// Requests metadata for one group.
    fn query_group_info(&mut self, id: &str) {
        let (Some(client), Some(jid)) = (self.client.clone(), Self::jid_of(id)) else {
            // Requeue until the link is available.
            self.group_info_requested.remove(id);
            let tries = self.group_info_tries.entry(id.to_owned()).or_insert(0);
            *tries += 1;
            self.group_info_retry.push((
                Instant::now() + Self::group_retry_delay(*tries),
                id.to_owned(),
            ));
            return;
        };
        let commands = self.commands.clone();
        let chat = id.to_owned();
        let me: Vec<String> = [self.me_pn.clone(), self.me_lid.clone()]
            .into_iter()
            .flatten()
            .collect();
        let lids = self.lid_to_pn.clone();
        tokio::spawn(async move {
            match client.groups().get_metadata(&jid).await {
                Ok(metadata) => {
                    let canonical = |jid: &Jid| -> String {
                        if jid.is_lid()
                            && let Some(pn) = lids.get(jid.user_base())
                        {
                            return format!("{pn}@s.whatsapp.net");
                        }
                        jid.to_non_ad_string()
                    };
                    let mut participants = Vec::new();
                    let mut admin = false;
                    for participant in &metadata.participants {
                        let id = participant
                            .phone_number
                            .as_ref()
                            .map(canonical)
                            .unwrap_or_else(|| canonical(&participant.jid));
                        let mine = me.contains(&id)
                            || participant
                                .lid
                                .as_ref()
                                .is_some_and(|lid| me.contains(&lid.to_non_ad_string()))
                            || me.contains(&participant.jid.to_non_ad_string());
                        if mine && participant.is_admin() {
                            admin = true;
                        }
                        participants.push(id);
                    }
                    let _ = commands.send(Command::GroupInfo {
                        chat,
                        name: (!metadata.subject.is_empty()).then(|| metadata.subject.clone()),
                        participants,
                        read_only: metadata.is_announcement && !admin,
                        community: metadata.is_parent_group,
                        // GroupEphemeralSettings carries a trigger mode, not a
                        // timestamp; a zero setting timestamp keeps later
                        // authoritative updates (protocol messages) able to
                        // override the value fetched here.
                        ephemeral_expiration: metadata
                            .ephemeral
                            .as_ref()
                            .and_then(|value| value.expiration),
                        ephemeral_setting_timestamp: None,
                    });
                }
                Err(error) => {
                    let text = error.to_string();
                    // Missing, forbidden, and unauthorized groups do not retry.
                    let permanent = ["item-not-found", "forbidden", "not-authorized"]
                        .iter()
                        .any(|word| text.contains(word));
                    log::warn!("no metadata for {chat}: {text}");
                    let _ = commands.send(Command::GroupInfoFailed { chat, permanent });
                }
            }
        });
    }

    // --- WhatsApp events -------------------------------------------------

    async fn handle_wa_event(&mut self, event: Arc<wa_events::Event>) {
        use wa_events::Event as E;
        match &*event {
            E::PairingQrCode(qr) => {
                self.qr = Some(qr.code.clone());
                let status = self.unlinked();
                self.set_status(status);
            }
            E::PairingCode(code) => {
                self.pair_code = Some(code.code.clone());
                let status = self.unlinked();
                self.set_status(status);
            }
            E::PairingCodeError(error) => {
                self.pair_code = None;
                self.pairing_phone = None;
                self.emit(Event::Error(format!(
                    "Could not link by phone number: {}",
                    error.error
                )));
                let status = self.unlinked();
                self.set_status(status);
            }
            E::PairingQrCodesExhausted(exhausted) => {
                self.qr = None;
                let status = self.unlinked();
                self.set_status(status);
                if exhausted.disconnected
                    && let Some(client) = self.client.clone()
                {
                    tokio::spawn(async move { client.reconnect_immediately().await });
                }
            }
            E::PairSuccess(pair) => {
                self.qr = None;
                self.pair_code = None;
                self.pairing_phone = None;
                self.remember_identity(Some(pair.id.clone()), Some(pair.lid.clone()), None);
                self.set_status(LinkStatus::Connecting);
            }
            E::Connected(_) => {
                let (pn, lid, name) = match &self.client {
                    Some(client) => (client.pn(), client.lid(), Some(client.push_name())),
                    None => (None, None, None),
                };
                self.remember_identity(pn, lid, name);
                self.set_status(LinkStatus::Connected);
                self.refresh_legacy_preferences();
                self.retry_avatars();
                self.pump_read_sync();
                self.poll_history.reconnect(Instant::now());
                let _ = self.archive.retry_poll_votes();
                self.pump_poll_votes();
                // Reconcile archive intents the phone never confirmed, bounded
                // per round like every other pump caller.
                self.pump_chat_sync();
                // Favorite changes the phone has not seen go out, and phone
                // favorites whose files never arrived are fetched again.
                self.push_favorites();
                self.fetch_missing_favorites();
                if let Some(client) = self.client.clone() {
                    let me = self.me_pn.clone().and_then(|pn| Self::jid_of(&pn));
                    let commands = self.commands.clone();
                    tokio::spawn(async move {
                        if let Err(error) = client.presence().set_available().await {
                            log::debug!("presence not announced: {error}");
                        }
                        // whatsapp-rust also enforces the account privacy setting.
                        match client.fetch_privacy_settings().await {
                            Ok(settings) => {
                                let disabled = !account_allows_receipts(&settings);
                                let _ = commands.send(Command::ReceiptsPrivacy { disabled });
                            }
                            Err(error) => log::debug!("privacy settings not fetched: {error}"),
                        }
                        if let Some(me) = me {
                            match client
                                .contacts()
                                .get_user_info(std::slice::from_ref(&me))
                                .await
                            {
                                Ok(info) => {
                                    let about = info
                                        .get(&me)
                                        .and_then(|info| info.status.clone())
                                        .filter(|about| !about.is_empty());
                                    let _ = commands.send(Command::MeInfo { about });
                                }
                                Err(error) => log::debug!("own info not fetched: {error}"),
                            }
                        }
                    });
                    // Mutations committed while offline (delete-for-me,
                    // clears, archive) have no other way back: each fresh
                    // connection asks the chat collections incrementally.
                    // Version-based, so an empty answer costs one handshake;
                    // failures stay a log line, retries belong to the next
                    // connection, never to a loop here.
                    let resync = self.client.clone();
                    tokio::spawn(async move {
                        let Some(resync) = resync else { return };
                        use whatsapp_rust::{AppStateResyncMode, WAPatchName};
                        match resync
                            .resync_app_state(
                                [
                                    WAPatchName::RegularHigh,
                                    WAPatchName::RegularLow,
                                    WAPatchName::Regular,
                                ],
                                AppStateResyncMode::Incremental,
                            )
                            .await
                        {
                            Ok(report) => {
                                if !report.all_synced() {
                                    log::warn!("app-state resync incomplete");
                                }
                            }
                            Err(error) => log::debug!("app-state resync skipped: {error}"),
                        }
                    });
                }
            }
            E::Disconnected(disconnected) => {
                if matches!(self.status, LinkStatus::Connected | LinkStatus::Connecting) {
                    self.set_status(LinkStatus::Disconnected {
                        reason: disconnected.reason.to_string(),
                    });
                }
            }
            E::LoggedOut(_) => self.on_logged_out().await,
            E::ConnectFailure(failure) => {
                if !failure.reason.is_logged_out() {
                    let detail = failure
                        .message
                        .as_ref()
                        .map(|message| format!(": {message}"))
                        .unwrap_or_default();
                    self.emit(Event::Error(format!(
                        "WhatsApp connection failed ({:?}){detail}",
                        failure.reason
                    )));
                }
            }
            E::StreamReplaced(_) => {
                self.emit(Event::Error(
                    "Another WhatsApp Web session replaced this one".to_owned(),
                ));
            }
            E::TemporaryBan(ban) => {
                self.set_status(LinkStatus::Failed(format!(
                    "WhatsApp has temporarily blocked this account ({:?})",
                    ban.code
                )));
            }
            E::ClientOutdated(_) => {
                self.set_status(LinkStatus::Failed(
                    "WhatsApp rejected this version of ZapExt. Update the app".to_owned(),
                ));
            }
            E::Messages(batch) => {
                for inbound in batch.messages.iter() {
                    self.ingest(&inbound.message, &inbound.info);
                }
            }
            E::UndecryptableMessage(undecryptable) => {
                self.ingest_undecryptable(&undecryptable.info);
            }
            E::Receipt(receipt) => self.on_receipt(receipt),
            E::ChatPresence(presence) => {
                self.learn_source(&presence.source);
                // Match WhatsApp: only other participants appear as typing,
                // including when our presence arrives from a linked device.
                if self.is_me(&self.canonical(&presence.source.sender)) {
                    return;
                }
                self.emit(Event::Typing {
                    chat: self.canonical(&presence.source.chat),
                    sender: self.canonical(&presence.source.sender),
                    composing: matches!(presence.state, ChatPresence::Composing),
                });
            }
            E::Presence(presence) => {
                self.emit(Event::Presence {
                    id: self.canonical(&presence.from),
                    online: !presence.unavailable,
                    last_seen: presence.last_seen.map(|when| when.timestamp()),
                });
            }
            E::ContactUpdate(update) => self.on_contact_update(update),
            E::GroupUpdate(update) => {
                let chat = self.canonical(&update.group_jid);
                if let whatsapp_rust::wacore::stanza::groups::GroupNotificationAction::Ephemeral {
                    expiration,
                    ..
                } = &update.action
                {
                    self.ensure_chat(&chat, None);
                    let timestamp = update.timestamp.timestamp();
                    let accepted = self
                        .archive
                        .set_ephemeral(&chat, *expiration, timestamp)
                        .unwrap_or(false);
                    log::debug!(
                        target: "zapfast::disappearing",
                        "group timer update: duration={expiration}s timestamp={timestamp} accepted={accepted}"
                    );
                    if accepted {
                        self.emit_chat(&chat);
                    }
                }
                self.request_group_info(&chat, true);
            }
            E::ArchiveUpdate(update) => {
                let chat = self.canonical(&update.jid);
                let remote = update.action.archived.unwrap_or(false);
                let remote_ms = update.timestamp.timestamp_millis();
                let queued = self
                    .archive
                    .queued_chat_sync(&chat, "archived")
                    .ok()
                    .flatten();
                let flying = self.sync_in_flight.keys().any(|(id, _)| id == &chat);
                // Central staleness gate on the persisted accepted order: an
                // older echo cannot flip the state back, with or without queue.
                let seen = self
                    .archive
                    .sync_order(&chat)
                    .ok()
                    .flatten()
                    .map_or(i64::MIN, |(ms, _)| ms);
                if remote_ms <= seen {
                    return;
                }
                match (queued, flying) {
                    // Converged only with no older operation still flying: an echo
                    // cannot confirm an intent it may predate.
                    (Some((value, _, _)), false) if value == remote => {
                        self.accept_remote_archive(&chat, remote, remote_ms);
                    }
                    // Our older operation still flies. Its echo (same value) is
                    // quiet, but a newer genuine phone change is applied now so
                    // the completion cannot silently keep the old state.
                    (Some((value, updated, _)), true) if value != remote && remote_ms > updated => {
                        // A mutation is stamped when the phone runs it, which
                        // may postdate a newer local intent: the echo of our
                        // own in-flight revision settles only that revision.
                        // It marks the order observed without flipping the
                        // flag or dropping the surviving queued intent.
                        if self
                            .dispatched_sync(&chat)
                            .is_some_and(|(_, dispatched, _)| remote == dispatched)
                        {
                            if let Err(error) =
                                self.archive.record_sync_order(&chat, remote_ms, value)
                            {
                                log::warn!("could not record an archive echo order: {error}");
                            }
                        } else {
                            self.accept_remote_archive(&chat, remote, remote_ms);
                        }
                    }
                    // An agreeing echo while our operation flies: remember its
                    // order so a delayed older echo cannot win later. The queue
                    // stays put and nothing repaints; the completion records
                    // its own time with a newest-wins policy and can never
                    // move the marker back.
                    (Some((value, _, _)), true) if value == remote => {
                        if let Err(error) = self.archive.record_sync_order(&chat, remote_ms, remote)
                        {
                            log::warn!("could not record an archive echo order: {error}");
                        }
                    }
                    (Some(_), true) => {}
                    // A newer local intent outranks this remote state: keep it,
                    // flushing now only when no backoff is pending.
                    (Some((_, updated_ms, _)), false) if updated_ms > remote_ms => {
                        self.push_archive_sync_at(&chat, Instant::now(), false);
                    }
                    // The phone is newer: apply it and drop the stale intent.
                    _ => {
                        self.accept_remote_archive(&chat, remote, remote_ms);
                    }
                }
            }
            E::PinUpdate(update) => {
                let chat = self.canonical(&update.jid);
                self.ensure_chat(&chat, None);
                let _ = self.archive.set_pinned_at(
                    &chat,
                    update.action.pinned.unwrap_or(false),
                    update.timestamp.timestamp_millis(),
                );
                self.emit_chat(&chat);
            }
            E::MuteUpdate(update) => {
                let chat = self.canonical(&update.jid);
                self.ensure_chat(&chat, None);
                let until = if update.action.muted.unwrap_or(false) {
                    Some(seconds(update.action.mute_end_timestamp.unwrap_or(0)))
                } else {
                    None
                };
                let _ =
                    self.archive
                        .set_muted_at(&chat, until, update.timestamp.timestamp_millis());
                self.emit_chat(&chat);
            }
            E::MarkChatAsReadUpdate(update) => {
                let chat = self.canonical(&update.jid);
                self.ensure_chat(&chat, None);
                if update.action.read.unwrap_or(true) {
                    let through = update
                        .action
                        .message_range
                        .as_option()
                        .and_then(|range| range.last_message_timestamp);
                    if let Some(through) = through {
                        let _ = self.archive.mark_read_through(&chat, seconds(through));
                    } else {
                        let _ = self.archive.mark_read(&chat);
                    }
                } else {
                    let _ = self.archive.finish_read_sync(&chat, i64::MAX);
                    let unread = self
                        .archive
                        .chat(&chat)
                        .ok()
                        .flatten()
                        .map_or(1, |row| row.unread.max(1));
                    let _ = self.archive.set_unread(&chat, unread);
                }
                self.emit_chat(&chat);
            }
            E::HistorySync(lazy) => self.on_history_sync(lazy).await,
            E::DisappearingModeChanged(update) => {
                // This is a contact's default for new conversations, not a
                // timer change in an existing chat. Per-chat changes arrive
                // as EPHEMERAL_SETTING or a typed group Ephemeral action.
                let id = self.canonical(&update.from);
                let timestamp = update.setting_timestamp.timestamp();
                if self.is_me(&id) {
                    let stored = self
                        .archive
                        .meta("default_ephemeral_setting_timestamp")
                        .ok()
                        .flatten()
                        .and_then(|value| value.parse::<i64>().ok())
                        .unwrap_or_default();
                    if timestamp >= stored {
                        let _ = self
                            .archive
                            .set_meta("default_ephemeral_expiration", &update.duration.to_string());
                        let _ = self.archive.set_meta(
                            "default_ephemeral_setting_timestamp",
                            &timestamp.to_string(),
                        );
                    }
                }
            }
            E::PictureUpdate(update) => {
                let id = self.canonical(&update.jid);
                let _ = std::fs::remove_file(self.avatar_file(&id, false));
                let _ = std::fs::remove_file(self.avatar_file(&id, true));
                if update.removed {
                    self.emit(Event::Avatar {
                        id: id.clone(),
                        full: false,
                        path: None,
                    });
                    self.emit(Event::Avatar {
                        id,
                        full: true,
                        path: None,
                    });
                } else {
                    self.fetch_avatar(id.clone(), false);
                    self.fetch_avatar(id, true);
                }
            }
            E::SelfPushNameUpdated(update) => {
                self.me_name = Some(update.new_name.clone());
                let _ = self.archive.set_meta("me_name", &update.new_name);
                self.emit(Event::Me {
                    id: self.me(),
                    name: self.me_name.clone(),
                    about: self.me_about.clone(),
                });
            }
            E::OfflineSyncCompleted(_) => self.emit_chats(),
            E::DeleteChatUpdate(update) => {
                log::info!("chat removal: received delete update");
                let chat = self.canonical(&update.jid);
                let through = removal_point(
                    update
                        .action
                        .message_range
                        .as_option()
                        .and_then(|range| range.last_message_timestamp),
                    update.timestamp.timestamp(),
                );
                self.remove_chat(&chat, through, update.delete_media);
            }
            E::ClearChatUpdate(update) => {
                log::info!("chat removal: received clear update");
                // The archive does not track which messages are starred, so
                // a clear that must preserve them cannot run: deleting the
                // range would destroy user-curated messages with no way
                // back, and the barrier would block their replay. Skip the
                // destructive step entirely instead of deleting more than
                // asked. Full starred support (state, StarUpdate order,
                // history snapshot) is a separate project.
                if !update.delete_starred {
                    log::info!("chat removal: keeping starred messages, clear skipped");
                    return;
                }
                let chat = self.canonical(&update.jid);
                let through = removal_point(
                    update
                        .action
                        .message_range
                        .as_option()
                        .and_then(|range| range.last_message_timestamp),
                    update.timestamp.timestamp(),
                );
                self.empty_chat(&chat, through, update.delete_media);
            }
            E::DeleteMessageForMeUpdate(update) => {
                log::info!("chat removal: received delete-for-me update");
                let chat = self.canonical(&update.chat_jid);
                // Tombstone first: a late history replay of the same id must
                // not resurrect the row this device just deleted.
                let now = whatsapp_rust::wacore::time::now_millis();
                match self
                    .archive
                    .delete_message_for_me(&chat, &update.message_id, now)
                {
                    Ok((deleted, media)) => {
                        // Files leave the critical path: the row is already
                        // gone and the tick reclaims what nothing references.
                        self.queue_media_gc(media);
                        // Idempotent invalidation: the archive may already be
                        // right while the interface still shows the message,
                        // so a repeated delete still clears the screen.
                        self.emit(Event::MessageDeleted {
                            chat: chat.clone(),
                            id: update.message_id.clone(),
                        });
                        self.emit_chat(&chat);
                        if !deleted {
                            log::info!("chat removal: repeated delete, interface invalidated");
                        }
                    }
                    Err(_error) => log::warn!("could not delete a message"),
                }
            }
            _ => {}
        }
    }

    /// Deletes a chat and stops everything that could still bring it back.
    fn remove_chat(&mut self, chat: &str, through: i64, delete_media: bool) {
        // A group we left would otherwise keep being asked for metadata and
        // log a failure for every attempt.
        self.group_info_queue.retain(|id| id != chat);
        self.group_info_retry.retain(|(_, id)| id != chat);
        self.group_info_requested.remove(chat);
        self.group_info_tries.remove(chat);
        match self.archive.remove_chat_through(chat, through, true) {
            Ok(removed) => {
                self.pending_older.remove(chat);
                // Files leave the critical path; the tick reclaims whatever
                // the phone allowed to drop once nothing references it.
                if delete_media {
                    self.queue_media_gc(removed.media);
                }
                // Idempotent invalidation against the authoritative state:
                // a replayed removal still clears a stale screen, while the
                // archive barrier keeps old history from coming back.
                if self.archive.chat(chat).ok().flatten().is_none() {
                    log::info!("chat removal: deleted cached chat");
                    self.emit(Event::ChatRemoved {
                        chat: chat.to_owned(),
                    });
                } else {
                    // Newer messages survived: clear the screen through the
                    // stored boundary, which never moves backwards.
                    let through = self
                        .archive
                        .removal_point(chat)
                        .ok()
                        .flatten()
                        .unwrap_or(through);
                    log::info!("chat removal: retained messages newer than deletion boundary");
                    self.emit(Event::ChatCleared {
                        chat: chat.to_owned(),
                        through,
                    });
                    self.emit_chat(chat);
                }
            }
            Err(_error) => log::warn!("could not delete a chat"),
        }
    }

    /// Empties a chat while keeping it listed.
    fn empty_chat(&mut self, chat: &str, through: i64, delete_media: bool) {
        match self.archive.remove_chat_through(chat, through, false) {
            Ok(removed) => {
                self.pending_older.remove(chat);
                // Files leave the critical path; the tick reclaims whatever
                // the phone allowed to drop once nothing references it.
                if delete_media {
                    self.queue_media_gc(removed.media);
                }
                // Idempotent invalidation through the stored boundary, which
                // never moves backwards: a replayed clear still clears a
                // stale screen.
                let through = self
                    .archive
                    .removal_point(chat)
                    .ok()
                    .flatten()
                    .unwrap_or(through);
                self.emit(Event::ChatCleared {
                    chat: chat.to_owned(),
                    through,
                });
                self.emit_chat(chat);
            }
            Err(_error) => log::warn!("could not clear a chat"),
        }
    }

    /// Defers freed attachment files to the tick: the message is already
    /// gone from the interface and the archive, and file deletion never
    /// blocks a delete event.
    /// Files reclaimed per tick: deletions stay off the event path, and one
    /// slow disk cannot stall the worker behind an unbounded queue.
    const MEDIA_GC_PER_TICK: usize = 64;
    fn queue_media_gc(&mut self, media: Vec<std::path::PathBuf>) {
        for path in media {
            // One entry per file: repeats from double deletes must not
            // grow the queue or pay the lookup twice.
            if !self.media_gc.contains(&path) {
                self.media_gc.push(path);
            }
        }
    }

    /// Reclaims queued attachment files whose references are all gone.
    /// One protection lookup per flush no matter how many deletes queued
    /// it, and a file still referenced by any survivor is never touched.
    /// Unprovable means keep everything: the startup cache sweep retries
    /// the orphans on a later run.
    fn pump_media_gc(&mut self) {
        if self.media_gc.is_empty() && self.media_gc_retry.is_empty() {
            return;
        }
        // A second chance first, then fresh work up to the round budget:
        // one slow disk cannot stall the worker behind an unbounded queue.
        let mut batch = std::mem::take(&mut self.media_gc_retry);
        let retried = batch.len();
        let fresh = self
            .media_gc
            .len()
            .min(Self::MEDIA_GC_PER_TICK.saturating_sub(retried));
        batch.extend(self.media_gc.drain(..fresh));
        // Unprovable means keep everything: a failed lookup or a damaged
        // favorites list is not evidence of absence, and the startup cache
        // sweep retries the orphans on a later run.
        let Some(live) = self.archive.protected_files() else {
            log::warn!("attachments: keeping removed files, references unprovable");
            batch.extend(std::mem::take(&mut self.media_gc));
            self.media_gc = batch;
            return;
        };
        for (index, path) in batch.into_iter().enumerate() {
            if live.contains(&path) {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) if index < retried => {
                    // Already failed once: drop it and let the startup
                    // sweep own the orphan instead of warning forever.
                    log::warn!("could not remove a cached attachment: {error}");
                }
                Err(_) => self.media_gc_retry.push(path),
            }
        }
    }

    /// Whether a message predates the deletion or clear of its chat.
    fn predates_removal(&self, chat: &str, timestamp: i64) -> bool {
        self.archive
            .removal_point(chat)
            .ok()
            .flatten()
            .is_some_and(|through| timestamp <= through)
    }

    fn remember_identity(&mut self, pn: Option<Jid>, lid: Option<Jid>, name: Option<String>) {
        if let Some(pn) = pn {
            let pn = pn.to_non_ad_string();
            let _ = self.archive.set_meta("me_pn", &pn);
            self.me_pn = Some(pn);
        }
        if let Some(lid) = lid {
            let lid = lid.to_non_ad_string();
            let _ = self.archive.set_meta("me_lid", &lid);
            self.me_lid = Some(lid);
        }
        if let (Some(pn), Some(lid)) = (self.me_pn.clone(), self.me_lid.clone())
            && let (Some(pn), Some(lid)) = (Self::jid_of(&pn), Self::jid_of(&lid))
        {
            self.learn_pair(&lid, &pn);
        }
        if let Some(name) = name.filter(|name| !name.is_empty()) {
            let _ = self.archive.set_meta("me_name", &name);
            self.me_name = Some(name);
        }
        self.emit(Event::Me {
            id: self.me(),
            name: self.me_name.clone(),
            about: self.me_about.clone(),
        });
    }

    async fn on_logged_out(&mut self) {
        self.stop_bot().await;
        if let Err(error) = self.archive.clear() {
            log::warn!("could not clear the archive: {error}");
        }
        self.lid_to_pn.clear();
        self.contacts.clear();
        self.group_info_requested.clear();
        self.group_info_queue.clear();
        self.group_info_tries.clear();
        self.group_info_retry.clear();
        self.presence_subscribed.clear();
        self.read_sync = ReadSync::default();
        self.poll_sending.clear();
        self.poll_history = Default::default();
        self.pending_older.clear();
        self.pending_avatars.clear();
        self.thumb_heals.clear();
        self.me_pn = None;
        self.me_lid = None;
        self.me_name = None;
        self.me_about = None;
        self.qr = None;
        self.pair_code = None;
        self.pairing_phone = None;
        self.set_syncing(false);
        let session = self.dirs.session_db();
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut path = session.clone().into_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir_all(self.dirs.avatar_cache_dir());
        let _ = std::fs::remove_dir_all(self.dirs.media_cache_dir());
        self.emit(Event::Chats(Vec::new()));
        self.set_status(LinkStatus::LoggedOut);
        // Recreate the store so the next connection starts linking.
        self.start_bot().await;
    }

    fn on_contact_update(&mut self, update: &wa_events::ContactUpdate) {
        if let (Some(lid), Some(pn)) = (&update.action.lid_jid, &update.action.pn_jid)
            && let (Some(lid), Some(pn)) = (Self::jid_of(lid), Self::jid_of(pn))
        {
            self.learn_pair(&lid, &pn);
        }
        let id = self.canonical(&update.jid);
        let name = update
            .action
            .full_name
            .clone()
            .or_else(|| update.action.first_name.clone())
            .filter(|name| !name.is_empty());
        let contact = self.contacts.entry(id.clone()).or_insert_with(|| Contact {
            id: id.clone(),
            full_name: None,
            push_name: None,
        });
        // A nameless update (a LID pairing, a status change) must not erase
        // the name the address book already gave us.
        if contact.full_name == name || (name.is_none() && contact.full_name.is_some()) {
            return;
        }
        contact.full_name = name;
        let contact = contact.clone();
        if let Err(error) = self.archive.upsert_contact(&contact) {
            log::warn!("could not save a contact: {error}");
        }
        self.emit(Event::Contacts(vec![contact]));
        self.refresh_chat_name(&id);
    }

    fn on_receipt(&mut self, receipt: &wa_events::Receipt) {
        self.learn_source(&receipt.source);
        let chat = self.canonical(&receipt.source.chat);
        log::debug!(
            "receipt {:?} from {} (chat {chat}, from me: {}, offline: {}) for {:?}",
            receipt.r#type,
            receipt.source.sender,
            receipt.source.is_from_me,
            receipt.offline,
            receipt.message_ids
        );
        let status = match receipt.r#type {
            ReceiptType::Delivered => Delivery::Delivered,
            // An inactive-device receipt still means delivered.
            ReceiptType::Inactive => Delivery::Delivered,
            ReceiptType::Read => Delivery::Read,
            ReceiptType::Played => Delivery::Played,
            ReceiptType::ReadSelf | ReceiptType::PlayedSelf => {
                // The receipt time is when the phone read, not the position
                // it read through. A delayed receipt must leave newer messages.
                for id in &receipt.message_ids {
                    if self
                        .archive
                        .message(&chat, id)
                        .ok()
                        .flatten()
                        .is_some_and(|message| !message.from_me)
                    {
                        let _ = self.archive.mark_read_to(&chat, id);
                    }
                }
                self.emit_chat(&chat);
                return;
            }
            // Own-device delivery counts as read only in the self chat.
            ReceiptType::Sender if chat == self.me() => Delivery::Read,
            _ => return,
        };
        let at = receipt.timestamp.timestamp();
        if ChatKind::from_id(&chat) == ChatKind::Group {
            let recipient = self.canonical(&receipt.source.sender);
            if self.is_me(&recipient) {
                return;
            }
            for id in &receipt.message_ids {
                if !self
                    .archive
                    .message(&chat, id)
                    .ok()
                    .flatten()
                    .is_some_and(|row| row.from_me)
                {
                    continue;
                }
                match self
                    .archive
                    .group_receipt(&chat, id, &recipient, status, at)
                {
                    Ok(true) => self.emit_message(&chat, id),
                    Ok(false) => {}
                    Err(error) => log::warn!("could not file a group receipt: {error}"),
                }
            }
            self.emit_chat(&chat);
            return;
        }
        let mut newest = 0;
        let mut changed = 0;
        for id in &receipt.message_ids {
            match self.archive.set_status(&chat, id, status, at) {
                Ok(true) => {
                    changed += 1;
                    self.emit_message(&chat, id);
                }
                Ok(false) => {}
                Err(error) => log::warn!("could not file a receipt for {id}: {error}"),
            }
            if let Ok(Some(message)) = self.archive.message(&chat, id) {
                newest = newest.max(message.timestamp);
            }
        }
        log::debug!(
            "receipt moved {changed} of {} messages in {chat} to {status:?}",
            receipt.message_ids.len()
        );
        // Read receipts advance all earlier messages.
        if status >= Delivery::Read
            && newest > 0
            && let Ok(ids) = self.archive.advance_statuses(&chat, newest, status, at)
        {
            for id in ids {
                self.emit_message(&chat, &id);
            }
        }
        self.emit_chat(&chat);
    }

    /// Returns raw mention tokens and canonical ids.
    fn mentions_of(&self, raw: &[String]) -> Vec<MentionRef> {
        raw.iter()
            .filter_map(|jid| {
                let user = jid.split('@').next()?.to_owned();
                if user.is_empty() {
                    return None;
                }
                Some(MentionRef {
                    user,
                    id: self.canonical_str(jid),
                })
            })
            .collect()
    }

    fn ingest(&mut self, message: &Arc<wa::Message>, info: &MessageInfo) {
        self.learn_source(&info.source);
        if info.source.chat.is_status_broadcast() {
            return;
        }
        let chat = self.canonical(&info.source.chat);
        let from_me = info.source.is_from_me;
        let sender = if from_me {
            self.me()
        } else {
            self.canonical(&info.source.sender)
        };
        let push_name = (!info.push_name.is_empty()).then(|| info.push_name.clone());
        let base = message.get_base_message();
        if let Some(expiration) = info.ephemeral_expiration
            && self
                .archive
                .ephemeral_expiration(&chat)
                .ok()
                .flatten()
                .is_none()
        {
            self.ensure_chat(&chat, push_name.as_deref());
            let _ = self.archive.set_ephemeral(&chat, expiration, 0);
        }

        if let Some(protocol) = base.protocol_message.as_option() {
            use wa::message::protocol_message::Type;
            if protocol.r#type == Some(Type::EPHEMERAL_SETTING) {
                if let Some(expiration) = protocol.ephemeral_expiration {
                    let timestamp = protocol
                        .ephemeral_setting_timestamp
                        .unwrap_or_else(|| info.timestamp.timestamp());
                    let used_fallback = protocol.ephemeral_setting_timestamp.is_none();
                    self.ensure_chat(&chat, push_name.as_deref());
                    let accepted = self
                        .archive
                        .set_ephemeral(&chat, expiration, timestamp)
                        .unwrap_or(false);
                    log::debug!(
                        target: "zapfast::disappearing",
                        "protocol timer update: duration={expiration}s timestamp={timestamp} fallback_timestamp={used_fallback} accepted={accepted}"
                    );
                    if accepted {
                        self.emit_chat(&chat);
                    }
                } else {
                    log::debug!(
                        target: "zapfast::disappearing",
                        "protocol timer update missing expiration"
                    );
                }
                return;
            }
            let Some(target) = protocol.key.as_option().and_then(|key| key.id.clone()) else {
                return;
            };
            match protocol.r#type {
                Some(Type::REVOKE) => {
                    if let Ok(true) =
                        self.archive
                            .set_content(&chat, &target, &Content::Revoked, false)
                    {
                        self.emit_message(&chat, &target);
                        self.emit_chat(&chat);
                    }
                }
                Some(Type::MESSAGE_EDIT) => {
                    if let Some(edited) = protocol.edited_message.as_option()
                        && let Some(mut content) = classify(edited.get_base_message())
                    {
                        // Preserve downloaded media when updating a caption.
                        if let Ok(Some(existing)) = self.archive.message(&chat, &target)
                            && let (Some(new), Some(old)) =
                                (content.media_mut(), existing.content.media())
                        {
                            new.path = old.path.clone();
                        }
                        if let Ok(true) = self.archive.set_content(&chat, &target, &content, true) {
                            self.emit_message(&chat, &target);
                            self.emit_chat(&chat);
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        if let Some(reaction) = base.reaction_message.as_option() {
            let Some(target) = reaction.key.as_option().and_then(|key| key.id.clone()) else {
                return;
            };
            let emoji = reaction.text.clone().unwrap_or_default();
            if let Ok(Some(updated)) = self
                .archive
                .set_reaction(&chat, &target, &sender, from_me, &emoji)
            {
                self.emit(Event::MessageUpdated(Box::new(updated)));
            }
            return;
        }
        if let Some(update) = base.poll_update_message.as_option() {
            self.ingest_poll_vote(
                &chat,
                &info.id,
                &info.source.sender.to_non_ad_string(),
                from_me,
                info.timestamp.timestamp(),
                update,
            );
            return;
        }
        let Some(content) = classify(base) else {
            return;
        };
        // A view-once payload is a one-shot that a linked device cannot fetch:
        // say what it is instead of offering a download that can only fail.
        let content = if message.is_view_once() {
            view_once_of(base, content)
        } else {
            content
        };
        let quoted = self.quoted_of(base);
        let mentions = self.mentions_of(&mentioned_of(base));
        let row = Message {
            id: info.id.clone(),
            chat: chat.clone(),
            sender,
            sender_name: if from_me { None } else { push_name.clone() },
            from_me,
            timestamp: info.timestamp.timestamp(),
            content,
            status: if from_me {
                Delivery::Sent
            } else {
                Delivery::None
            },
            delivered_at: None,
            read_at: None,
            quoted,
            reactions: Vec::new(),
            edited: false,
            mentions,
            forwarded: forwarded_of(base),
            thumbnail: thumbnail_of(base),
        };
        let is_poll = matches!(row.content, Content::Poll { .. });
        self.remember_poll(&row, message, &info.source.sender.to_non_ad_string(), None);
        self.store_message(row, Some(message.encode_to_vec()), push_name.as_deref());
        if is_poll {
            self.pump_poll_votes();
        }
    }

    fn ingest_undecryptable(&mut self, info: &MessageInfo) {
        self.learn_source(&info.source);
        if info.source.chat.is_status_broadcast() || info.source.is_from_me {
            return;
        }
        let chat = self.canonical(&info.source.chat);
        if self
            .archive
            .message(&chat, &info.id)
            .ok()
            .flatten()
            .is_some()
        {
            return;
        }
        let push_name = (!info.push_name.is_empty()).then(|| info.push_name.clone());
        let row = Message {
            id: info.id.clone(),
            chat,
            sender: self.canonical(&info.source.sender),
            sender_name: push_name.clone(),
            from_me: false,
            timestamp: info.timestamp.timestamp(),
            content: Content::Unsupported {
                what: "Waiting for this message. Open WhatsApp on your phone".to_owned(),
            },
            status: Delivery::None,
            delivered_at: None,
            read_at: None,
            quoted: None,
            reactions: Vec::new(),
            edited: false,
            mentions: Vec::new(),
            forwarded: false,
            thumbnail: None,
        };
        self.store_message(row, None, push_name.as_deref());
    }

    /// Archives a message and emits chat and row updates.
    fn store_message(&mut self, message: Message, raw: Option<Vec<u8>>, push_name: Option<&str>) {
        let chat = message.chat.clone();
        if self.predates_removal(&chat, message.timestamp) {
            // Deleted on a linked device: late history must not resurrect it.
            return;
        }
        // Deleted for this device: the tombstone blocks the row below, and
        // this guard blocks its unread count, bubble, and notification too.
        match self.archive.is_tombstoned(&chat, &message.id) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                log::warn!("could not check a tombstone: {error}");
                return;
            }
        }
        self.ensure_chat(&chat, if message.from_me { None } else { push_name });
        if let Some(push_name) = push_name
            && !message.from_me
        {
            let sender = message.sender.clone();
            self.remember_push_name(&sender, push_name);
        }
        let is_new = self
            .archive
            .message(&chat, &message.id)
            .ok()
            .flatten()
            .is_none();
        if let Err(error) = self.archive.insert_message(&message, raw.as_deref()) {
            log::warn!("could not store a message: {error}");
            return;
        }
        let unread = is_new
            && !message.from_me
            && self
                .archive
                .read_through(&chat)
                .ok()
                .flatten()
                // A new live message may share the read message's second. Its
                // distinct id already passed the duplicate check above.
                .is_none_or(|through| message.timestamp >= through);
        if unread {
            let _ = self.archive.bump_unread(&chat);
        } else if message.from_me
            && matches!(
                message.status,
                Delivery::Sent | Delivery::Delivered | Delivery::Read | Delivery::Played
            )
        {
            // A reply sent from the phone/another companion reads the preceding
            // conversation there. Replayed replies cannot clear newer arrivals.
            let _ = self.archive.mark_read_to(&chat, &message.id);
        }
        let mut stored = self
            .archive
            .message(&chat, &message.id)
            .ok()
            .flatten()
            .unwrap_or(message);
        self.polish(&mut stored);
        // Notify only for live incoming messages, not history replay.
        let incoming = (unread && !self.syncing).then(|| stored.clone());
        self.emit(Event::Messages {
            chat: chat.clone(),
            messages: vec![stored],
            older: false,
            complete: false,
        });
        self.emit_chat(&chat);
        if let Some(message) = incoming {
            self.emit(Event::Incoming {
                chat,
                message: Box::new(message),
            });
        }
    }

    fn quoted_of(&self, base: &wa::Message) -> Option<Quoted> {
        let context = context_of(base)?;
        let id = context.stanza_id.clone().filter(|id| !id.is_empty())?;
        let sender = context
            .participant
            .as_deref()
            .map(|participant| self.canonical_str(participant))
            .unwrap_or_default();
        let (summary, listed) = context
            .quoted_message
            .as_option()
            .map(|quoted| {
                let base = quoted.get_base_message();
                (
                    classify(base)
                        .map(|content| content.summary())
                        .unwrap_or_default(),
                    self.mentions_of(&mentioned_of(base)),
                )
            })
            .unwrap_or_default();
        // Recover quote mentions from `@user` tokens when metadata is missing.
        let summary = self.pn_tokens(&summary);
        let mentions = if listed.is_empty() {
            self.mention_tokens(&summary)
        } else {
            listed
        };
        Some(Quoted {
            sender_name: self.name_for(&sender),
            id,
            sender,
            summary,
            mentions,
        })
    }

    /// Replaces known privacy ids in `@user` tokens with phone-number ids.
    fn pn_tokens(&self, text: &str) -> String {
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
            match self.lid_to_pn.get(&after[..digits]) {
                Some(pn) if digits > 0 => {
                    out.push_str(pn);
                    rest = &after[digits..];
                }
                _ => rest = after,
            }
        }
        out.push_str(rest);
        out
    }

    /// Infers canonical mention ids from `@user` tokens.
    fn mention_tokens(&self, text: &str) -> Vec<MentionRef> {
        let mut found = Vec::new();
        let mut rest = text;
        while let Some(at) = rest.find('@') {
            let after = &rest[at + 1..];
            let digits = after
                .char_indices()
                .find(|(_, c)| !c.is_ascii_digit())
                .map_or(after.len(), |(index, _)| index);
            let user = &after[..digits];
            if digits >= 5 {
                let id = match self.lid_to_pn.get(user) {
                    Some(pn) => format!("{pn}@s.whatsapp.net"),
                    None => format!("{user}@s.whatsapp.net"),
                };
                let id = self.canonical_str(&id);
                if !found.iter().any(|known: &MentionRef| known.user == user) {
                    found.push(MentionRef {
                        user: user.to_owned(),
                        id,
                    });
                }
            }
            rest = after;
        }
        found
    }

    // --- history ---------------------------------------------------------

    async fn on_history_sync(&mut self, lazy: &wa_events::LazyHistorySync) {
        let on_demand =
            lazy.sync_type() == ON_DEMAND || lazy.peer_data_request_session_id().is_some();
        if !on_demand {
            self.sync_deadline = Some(Instant::now() + SYNC_QUIET);
            self.set_syncing(true);
            if let Some(progress) = lazy.progress() {
                self.emit(Event::SyncProgress(progress.min(100)));
            }
        }
        let compressed = lazy.compressed_bytes().clone();
        let parsed = tokio::task::spawn_blocking(move || parse_history(&compressed)).await;
        match parsed {
            Ok(Ok(parsed)) => {
                if on_demand {
                    log::info!(
                        "poll recovery: on-demand history received; chats={}, messages={}, standalone_votes={}",
                        parsed.chats.len(),
                        parsed
                            .chats
                            .iter()
                            .map(|chat| chat.messages.len())
                            .sum::<usize>(),
                        parsed
                            .chats
                            .iter()
                            .map(|chat| chat.poll_updates.len())
                            .sum::<usize>()
                    );
                }
                let filed = self.apply_history(parsed, !on_demand);
                if on_demand {
                    self.answer_older(filed);
                }
            }
            Ok(Err(error)) => {
                log::warn!("a history chunk could not be read: {error}");
                self.emit(Event::Error(format!(
                    "Could not read part of the chat history: {error}"
                )));
            }
            Err(error) => log::warn!("history parsing panicked: {error}"),
        }
        if !on_demand && lazy.progress().is_some_and(|progress| progress >= 100) {
            self.sync_deadline = Some(Instant::now() + Duration::from_secs(3));
        }
        self.emit_chats();
    }

    /// Archives a history chunk. `metadata` controls chat-state updates.
    /// Returns each chat's message count and whether the phone has more.
    fn apply_history(
        &mut self,
        parsed: ParsedHistory,
        metadata: bool,
    ) -> Vec<(ChatId, usize, Option<bool>)> {
        for (lid, pn) in &parsed.lids {
            if let (Some(lid), Some(pn)) = (Self::jid_of(lid), Self::jid_of(pn)) {
                self.learn_pair(&lid, &pn);
            }
        }
        if !parsed.stickers.is_empty() {
            log::info!(
                "the phone listed {} recently used stickers",
                parsed.stickers.len()
            );
        }
        for sticker in &parsed.stickers {
            let Some(hash) = sticker_hash(
                sticker.file_sha256.as_deref(),
                sticker.file_enc_sha256.as_deref(),
            ) else {
                continue;
            };
            if let Err(error) = self.archive.upsert_phone_sticker(
                &hash,
                &sticker.encode_to_vec(),
                seconds(sticker.last_sticker_sent_ts.unwrap_or(0)),
                sticker.weight.unwrap_or(0.0),
            ) {
                log::warn!("could not store sticker {hash}: {error}");
            }
        }
        for chat in &parsed.chats {
            if let (Some(lid), Some(pn)) = (&chat.lid_jid, &chat.pn_jid)
                && let (Some(lid), Some(pn)) = (Self::jid_of(lid), Self::jid_of(pn))
            {
                self.learn_pair(&lid, &pn);
            }
        }
        let mut filed = Vec::new();
        for (id, name) in &parsed.push_names {
            let id = self.canonical_str(id);
            self.remember_push_name(&id, name);
        }
        for mut chat in parsed.chats {
            let id = self.canonical_str(&chat.id);
            if id.ends_with("@broadcast") {
                continue;
            }
            let existing = self.archive.chat(&id).ok().flatten();
            if let Some(through) = self.archive.removal_point(&id).ok().flatten() {
                chat.messages.retain(|message| message.timestamp > through);
                // Nothing newer than the deletion: leave the chat deleted.
                if existing.is_none() && chat.messages.is_empty() {
                    continue;
                }
            }
            if metadata || existing.is_none() {
                let name = match chat.name.filter(|name| !name.is_empty()) {
                    Some(name) if ChatKind::from_id(&id) == ChatKind::Group => name,
                    Some(name) => {
                        // Prefer the phone's address-book name for direct chats.
                        let contact = self.contacts.entry(id.clone()).or_insert_with(|| Contact {
                            id: id.clone(),
                            full_name: None,
                            push_name: None,
                        });
                        if contact.full_name.is_none()
                            && !name
                                .chars()
                                .all(|c| c.is_ascii_digit() || c == '+' || c == ' ')
                        {
                            contact.full_name = Some(name.clone());
                            let contact = contact.clone();
                            let _ = self.archive.upsert_contact(&contact);
                            self.emit(Event::Contacts(vec![contact]));
                        }
                        self.chat_name(&id, None)
                    }
                    // A chunk without a name keeps the stored subject for
                    // chats that already exist, so a fallback never
                    // overwrites a real name. New chats still get the best
                    // name available; an explicit name always applies.
                    None => existing
                        .as_ref()
                        .map(|row| row.name.clone())
                        .unwrap_or_else(|| self.chat_name(&id, None)),
                };
                let mut row = Chat::new(id.clone(), name);
                row.last_activity = chat.last_activity;
                row.unread = existing.as_ref().map_or(0, |existing| existing.unread);
                // An omitted archived flag keeps the stored state; an
                // explicit value archives or unarchives. This mirrors how
                // pin and mute already treat absent metadata.
                row.archived = chat
                    .archived
                    .unwrap_or_else(|| existing.as_ref().is_some_and(|row| row.archived));
                row.pinned_at = chat
                    .pinned_at
                    .unwrap_or_else(|| existing.as_ref().map_or(0, |row| row.pinned_at));
                row.pinned = chat.pinned_at.map_or_else(
                    || existing.as_ref().is_some_and(|row| row.pinned),
                    |when| when > 0,
                );
                row.muted_until = chat
                    .muted_until
                    .unwrap_or_else(|| existing.as_ref().and_then(|row| row.muted_until));
                // An omitted archived flag now keeps the stored state (see
                // above), so reaching here with a cleared flag means the
                // phone explicitly unarchived. Logged without identifiers.
                if metadata
                    && chat.archived == Some(false)
                    && existing.as_ref().is_some_and(|known| known.archived)
                {
                    log::debug!("history sync explicitly unarchived a chat");
                }
                if let Err(error) = self.archive.upsert_chat(&row) {
                    log::warn!("could not store chat {id}: {error}");
                    continue;
                }
            }
            if let Some(expiration) = chat.ephemeral_expiration {
                let _ = self.archive.set_ephemeral(
                    &id,
                    expiration,
                    chat.ephemeral_setting_timestamp.unwrap_or_default(),
                );
            }
            if ChatKind::from_id(&id) == ChatKind::Group {
                self.request_group_info(&id, false);
            }
            let count = chat.messages.len();
            for message in chat.messages {
                let poll_creator = if message.from_me {
                    self.me()
                } else {
                    message.sender.clone().unwrap_or_else(|| chat.id.clone())
                };
                let sender = if message.from_me {
                    self.me()
                } else {
                    message
                        .sender
                        .as_deref()
                        .map(|sender| self.canonical_str(sender))
                        .unwrap_or_else(|| id.clone())
                };
                if let Some(push_name) = message.push_name.as_deref()
                    && !message.from_me
                {
                    self.remember_push_name(&sender, push_name);
                }
                let reactions = message
                    .reactions
                    .into_iter()
                    .map(|(who, from_me, emoji)| Reaction {
                        sender: if from_me {
                            self.me()
                        } else {
                            who.as_deref()
                                .map(|who| self.canonical_str(who))
                                .unwrap_or_else(|| id.clone())
                        },
                        from_me,
                        emoji,
                    })
                    .collect();
                let quoted = message.quoted.map(|quoted| {
                    let sender = self.canonical_str(&quoted.sender);
                    Quoted {
                        sender_name: self.name_for(&sender),
                        sender,
                        ..quoted
                    }
                });
                let mentions = self.mentions_of(&message.mentions);
                let row = Message {
                    id: message.id,
                    chat: id.clone(),
                    sender,
                    sender_name: if message.from_me {
                        None
                    } else {
                        message.push_name
                    },
                    from_me: message.from_me,
                    timestamp: message.timestamp,
                    content: message.content,
                    status: message.status,
                    delivered_at: None,
                    read_at: None,
                    quoted,
                    reactions,
                    edited: false,
                    mentions,
                    forwarded: message.forwarded,
                    thumbnail: message.thumbnail,
                };
                let mut poll_history_received = false;
                if matches!(row.content, Content::Poll { .. }) {
                    if let Ok(raw) = wa::Message::decode_from_slice(&message.raw) {
                        self.remember_poll(
                            &row,
                            &raw,
                            &poll_creator,
                            message.poll_secret.as_deref(),
                        );
                    }
                    poll_history_received = self.history_poll_votes(&row, &message.poll_votes);
                }
                if let Err(error) = self.archive.insert_message(&row, Some(&message.raw)) {
                    log::warn!("could not store a history message: {error}");
                }
                if matches!(row.content, Content::Poll { .. }) {
                    if poll_history_received {
                        let _ = self.archive.mark_poll_history(&id, &row.id);
                        self.poll_history.finish(&id, &row.id);
                    }
                    self.emit_message(&id, &row.id);
                }
            }
            for update in chat.poll_updates {
                let sender = if update.from_me {
                    self.me()
                } else {
                    update.sender.unwrap_or_else(|| chat.id.clone())
                };
                self.ingest_poll_vote(
                    &id,
                    &update.id,
                    &sender,
                    update.from_me,
                    update.timestamp,
                    &update.update,
                );
            }
            for revoked in chat.revoked {
                let _ = self
                    .archive
                    .set_content(&id, &revoked, &Content::Revoked, false);
            }
            if (metadata || existing.is_none())
                && let Some(snapshot_unread) = chat.unread
            {
                if snapshot_unread == 0 {
                    let _ = self.archive.mark_read_through(&id, chat.last_activity);
                } else {
                    let unread = self
                        .archive
                        .history_unread(&id, snapshot_unread)
                        .unwrap_or(0);
                    let unread = existing
                        .as_ref()
                        .map_or(unread, |existing| existing.unread.max(unread));
                    let _ = self.archive.set_unread(&id, unread);
                }
            }
            filed.push((id, count, chat.more_on_phone));
        }
        self.pump_poll_votes();
        self.pump_poll_history();
        for (id, _, _) in &filed {
            self.emit_chat(id);
        }
        filed
    }

    /// Completes pending requests covered by an on-demand history chunk.
    fn answer_older(&mut self, filed: Vec<(ChatId, usize, Option<bool>)>) {
        for (chat, count, more_on_phone) in filed {
            let more = count > 0 && more_on_phone != Some(false);
            let Some((_, (before_time, before_id))) = self.pending_older.remove(&chat) else {
                // Late responses are already archived; tell the app to page again.
                self.emit(Event::OlderFetched { chat, more });
                continue;
            };
            match self
                .archive
                .messages(&chat, Some((before_time, &before_id)), 500)
            {
                Ok(mut messages) => {
                    for message in &mut messages {
                        self.polish(message);
                    }
                    self.emit(Event::Messages {
                        chat: chat.clone(),
                        messages,
                        older: true,
                        complete: false,
                    })
                }
                Err(error) => log::warn!("could not read older messages: {error}"),
            }
            self.emit(Event::OlderFetched { chat, more });
        }
    }

    /// Times out unanswered phone-history requests.
    fn expire_older_requests(&mut self) {
        let expired: Vec<ChatId> = self
            .pending_older
            .iter()
            .filter(|(_, (asked, _))| asked.elapsed() > PHONE_PATIENCE)
            .map(|(chat, _)| chat.clone())
            .collect();
        for chat in expired {
            self.pending_older.remove(&chat);
            self.emit(Event::OlderFetched {
                chat: chat.clone(),
                more: true,
            });
            // Report the timeout once per chat; later retries back off silently.
            if self.older_warned.insert(chat) {
                self.emit(Event::Error(
                    "Your phone did not send older messages. Check that it is online".to_owned(),
                ));
            }
        }
    }

    fn fetch_older(&mut self, chat: ChatId) {
        if self.pending_older.contains_key(&chat) {
            return;
        }
        let (Some(client), Some(jid)) = (self.client.clone(), Self::jid_of(&chat)) else {
            // Offline requests retry after reconnection; the banner shows state.
            self.emit(Event::OlderFetched { chat, more: true });
            return;
        };
        // Chats without messages request history from the current time.
        let (id, from_me, timestamp) = match self.archive.oldest(&chat) {
            Ok(Some(oldest)) => (oldest.id, oldest.from_me, oldest.timestamp),
            _ => (String::new(), false, crate::util::now()),
        };
        self.pending_older
            .insert(chat.clone(), (Instant::now(), (timestamp, id.clone())));
        let commands = self.commands.clone();
        tokio::spawn(async move {
            if let Err(error) = client
                // Despite its `Ms` name, the protocol field takes Unix seconds.
                // https://github.com/tulir/whatsmeow/commit/54650307d891f89ab346a57953d316106caee371
                .fetch_message_history(&jid, &id, from_me, timestamp, PHONE_BATCH)
                .await
            {
                log::warn!("older messages not requested: {error}");
                let _ = commands.send(Command::OlderFailed {
                    chat: chat.clone(),
                    error: format!("Could not request older messages from your phone: {error}"),
                });
            }
        });
    }

    // --- commands --------------------------------------------------------

    async fn handle_command(&mut self, command: Command) {
        match command {
            Command::RefreshPoll { chat, message } => self.refresh_poll(chat, message),
            Command::PollHistoryFailed {
                chat,
                message,
                requested,
            } => {
                self.poll_history
                    .fail(&chat, &message, requested, Instant::now());
                self.emit_message(&chat, &message);
                self.pump_poll_history();
            }
            Command::CreatePoll { chat, draft } => self.create_poll(chat, draft),
            Command::PollCreated {
                chat,
                draft,
                result,
            } => self.poll_created(chat, draft, result),
            Command::VotePoll {
                chat,
                message,
                choices,
            } => self.vote_poll(chat, message, choices),
            Command::PollVoted {
                chat,
                message,
                choices,
                at,
                result,
            } => self.poll_voted(chat, message, choices, at, result),
            Command::PollDecoded { vote, choices } => self.poll_decoded(vote, choices),
            Command::SendText {
                chat,
                text,
                quoting,
                mentions,
            } => self.send_text(chat, text, quoting, mentions),
            Command::Forward {
                from_chat,
                message,
                to_chat,
            } => self.forward_message(from_chat, message, to_chat),
            Command::ForwardMany {
                from_chat,
                messages,
                to_chats,
            } => self.forward_many(from_chat, messages, to_chats),
            Command::Forwarded { messages, chats } => {
                let what = if messages == 1 {
                    "1 message".to_owned()
                } else {
                    format!("{messages} messages")
                };
                let whereto = if chats == 1 {
                    "1 chat".to_owned()
                } else {
                    format!("{chats} chats")
                };
                self.emit(Event::Info(format!("Forwarded {what} to {whereto}")));
            }
            Command::Composing { chat, composing } => {
                let (Some(client), Some(jid)) = (self.client.clone(), Self::jid_of(&chat)) else {
                    return;
                };
                tokio::spawn(async move {
                    let result = if composing {
                        client.chatstate().send_composing(&jid).await
                    } else {
                        client.chatstate().send_paused(&jid).await
                    };
                    if let Err(error) = result {
                        log::debug!("chat state not sent: {error}");
                    }
                });
            }
            Command::MarkRead { chat, receipts } => self.mark_read(chat, receipts),
            Command::ReadSyncFinished {
                chat,
                through,
                success,
            } => {
                if !self
                    .read_sync
                    .finish(&chat, through, success, Instant::now())
                {
                    return;
                }
                if success {
                    let _ = self.archive.finish_read_sync(&chat, through);
                    self.pump_read_sync();
                }
            }
            Command::LoadChat { chat, before } => self.load_chat(chat, before),
            Command::FetchOlder(chat) => self.fetch_older(chat),
            Command::LoadUntil { chat, id, before } => self.load_until(chat, id, before),
            Command::SearchMessages { query } => self.spawn_search(None, query, 50),
            Command::SearchReady {
                generation,
                query,
                chat,
                hits,
            } => self.apply_search(generation, query, chat, hits),
            Command::EnsureChat { chat, name } => {
                let is_new = self.archive.chat(&chat).ok().flatten().is_none();
                if let Err(error) = self.archive.ensure_chat(&chat, &name) {
                    log::warn!("could not create the chat: {error}");
                } else if is_new
                    && ChatKind::from_id(&chat) == ChatKind::Direct
                    && let Some(expiration) = self.default_ephemeral_expiration()
                {
                    let timestamp = self
                        .archive
                        .meta("default_ephemeral_setting_timestamp")
                        .ok()
                        .flatten()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or_default();
                    let _ = self.archive.set_ephemeral(&chat, expiration, timestamp);
                }
            }
            Command::Download { chat, message } => self.download(chat, message),
            Command::HealSticker { path } => self.heal_sticker(&path),
            Command::HealStickerThumb { path } => self.heal_sticker_thumb(&path),
            Command::ThumbHealFinished { path, ok } => {
                if ok {
                    self.thumb_heals.remove(&path);
                    self.emit(Event::StickerThumb { path, ok: true });
                } else if self.thumb_heals.contains_key(&path) {
                    let attempts = self
                        .thumb_heals
                        .get(&path)
                        .map(|heal| heal.attempts)
                        .unwrap_or_default();
                    self.fail_thumb_heal(path, attempts, Instant::now());
                }
            }
            Command::FetchAvatar { id, full } => self.fetch_avatar(id, full),
            Command::EditText {
                chat,
                id,
                text,
                mentions,
            } => self.edit_text(chat, id, text, mentions),
            Command::Revoke { chat, id } => self.revoke(chat, id),
            Command::DeleteLocal { chat, id } => {
                if let Ok(true) = self.archive.delete_message(&chat, &id) {
                    self.emit(Event::MessageDeleted {
                        chat: chat.clone(),
                        id,
                    });
                    self.emit_chat(&chat);
                }
            }
            Command::CopyImage { path } => {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let commands = self.commands.clone();
                tokio::task::spawn_blocking(move || {
                    let error = copy_image_to_clipboard(&path).err();
                    let _ = commands.send(Command::ImageCopied { name, error });
                });
            }
            Command::ImageCopied { name, error } => self.emit(Event::CopyImage { name, error }),

            Command::FileInfo { chat, message } => {
                let result = self.file_info(&chat, &message);
                self.emit(Event::FileInfo {
                    chat,
                    message,
                    result,
                });
            }
            Command::PickFiles(chat) => {
                let commands = self.commands.clone();
                tokio::task::spawn_blocking(move || {
                    let paths = rfd::FileDialog::new()
                        .set_title("Send to WhatsApp")
                        .pick_files()
                        .unwrap_or_default();
                    let _ = commands.send(Command::Picked { chat, paths });
                });
            }
            Command::Picked { chat, paths } => self.emit(Event::Picked { chat, paths }),
            Command::SaveCopy { from } => {
                let name = from
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "zapext-file".to_owned());
                let commands = self.commands.clone();
                tokio::task::spawn_blocking(move || {
                    // A cancelled dialog reports nothing.
                    let result = rfd::FileDialog::new()
                        .set_title("Save a copy")
                        .set_file_name(&name)
                        .save_file()
                        .map(|to| {
                            std::fs::copy(&from, &to)
                                .map(|_| to)
                                .map_err(|error| error.to_string())
                        });
                    let (saved, error) = match result {
                        Some(Ok(path)) => (Some(path), None),
                        Some(Err(error)) => (None, Some(error)),
                        None => (None, None),
                    };
                    let _ = commands.send(Command::CopySaved { saved, error });
                });
            }
            Command::CopySaved { saved, error } => self.emit(Event::CopySaved { saved, error }),
            Command::SendFiles {
                chat,
                paths,
                caption,
                mentions,
            } => {
                self.send_files(chat, paths, caption, mentions);
            }
            Command::SendImage {
                chat,
                width,
                height,
                rgba,
                caption,
                mentions,
            } => self.send_pasted_image(chat, width, height, rgba, caption, mentions),
            Command::Outbound { chat, row, raw } => self.outbound(chat, *row, raw),
            Command::SendSticker {
                chat,
                path,
                quoting,
            } => self.send_sticker(chat, path, quoting),
            Command::SaveSticker { path } => match self.save_sticker(&path) {
                Ok(()) => self.emit_stickers(),
                Err(error) => self.emit(Event::Error(format!("Could not save sticker: {error}"))),
            },
            Command::ForgetSticker { path } => {
                // Restrict deletion to files in the saved-sticker directory.
                if path.starts_with(self.dirs.saved_sticker_dir())
                    && std::fs::remove_file(&path).is_ok()
                {
                    self.emit_stickers();
                }
            }
            Command::PickStickerArchive => {
                let commands = self.commands.clone();
                let packs = self.packs_dir();
                tokio::task::spawn_blocking(move || {
                    let result = match rfd::FileDialog::new()
                        .set_title("Add a sticker pack")
                        .add_filter("Sticker packs", &["wastickers", "zip"])
                        .pick_file()
                    {
                        Some(path) => super::sticker_import::import_archive(&path, &packs),
                        // Ignore file-picker cancellation.
                        None => Err(String::new()),
                    };
                    let _ = commands.send(Command::StickerPackImported { result });
                });
            }
            Command::SaveContact {
                id,
                full_name,
                first_name,
                to_phone,
            } => {
                let (Some(client), Some(jid)) = (self.client.clone(), Self::jid_of(&id)) else {
                    self.emit(Event::Error("Not connected to WhatsApp".to_owned()));
                    return;
                };
                let commands = self.commands.clone();
                tokio::spawn(async move {
                    let error = client
                        .chat_actions()
                        .save_contact(&jid, Some(full_name.clone()), first_name, to_phone)
                        .await
                        .err()
                        .map(|error| error.to_string());
                    let _ = commands.send(Command::ContactSaved {
                        id,
                        name: full_name,
                        error,
                    });
                });
            }
            Command::ContactSaved { id, name, error } => {
                if let Some(error) = error {
                    self.emit(Event::Error(format!("Could not save contact: {error}")));
                    return;
                }
                let contact = Contact {
                    id: id.clone(),
                    full_name: Some(name.clone()),
                    push_name: None,
                };
                if let Err(error) = self.archive.upsert_contact(&contact) {
                    log::warn!("could not store the contact: {error}");
                }
                // Preserve the stored push name during contact updates.
                let stored = self.archive.contact(&id).ok().flatten().unwrap_or(contact);
                self.emit(Event::Contacts(vec![stored]));
                self.emit(Event::Info(format!("Added {name} to contacts")));
                self.emit_chat(&id);
            }
            Command::NewContact {
                phone,
                full_name,
                first_name,
                to_phone,
            } => {
                let Some(client) = self.client.clone() else {
                    self.emit(Event::Error("Not connected to WhatsApp".to_owned()));
                    return;
                };
                let commands = self.commands.clone();
                let jid = Jid::pn(&phone);
                tokio::spawn(async move {
                    // Use WhatsApp's registration check before opening the chat.
                    let registered = match client.contacts().is_on_whatsapp(&[jid]).await {
                        Ok(results) => results.iter().any(|result| result.is_registered),
                        Err(error) => {
                            log::debug!("number check failed, trusting the number: {error}");
                            true
                        }
                    };
                    let _ = commands.send(Command::ContactChecked {
                        phone,
                        full_name,
                        first_name,
                        to_phone,
                        registered,
                    });
                });
            }
            Command::ContactChecked {
                phone,
                full_name,
                first_name,
                to_phone,
                registered,
            } => {
                if !registered {
                    self.emit(Event::Error(format!(
                        "{} is not on WhatsApp",
                        crate::util::phone(&phone)
                    )));
                    return;
                }
                let id = format!("{phone}@s.whatsapp.net");
                if let Some(full_name) = full_name.clone() {
                    let _ = self.commands.send(Command::SaveContact {
                        id: id.clone(),
                        full_name,
                        first_name,
                        to_phone,
                    });
                }
                self.emit(Event::ContactReady {
                    id,
                    name: full_name,
                });
            }
            Command::StickerPackImported { result } => match result {
                Ok(name) => {
                    self.emit_stickers();
                    self.emit(Event::Info(format!("Added sticker pack \"{name}\"")));
                }
                Err(error) if error.is_empty() => self.emit_stickers(),
                Err(error) => {
                    self.emit(Event::Error(format!("Could not add sticker pack: {error}")))
                }
            },
            Command::DeleteStickerPack { dir } => {
                let root = self.packs_dir();
                if dir.starts_with(&root) && dir != root && std::fs::remove_dir_all(&dir).is_ok() {
                    self.emit_stickers();
                }
            }
            Command::ViewStickerPack { chat, message } => {
                self.view_sticker_pack(&chat, &message);
            }
            Command::StickerPackViewed { result } => {
                self.emit(Event::StickerPackPreview(result));
            }
            Command::AddStickerPack { dir, name } => {
                self.add_sticker_pack(&dir, &name);
            }
            Command::SendVoice {
                chat,
                samples,
                quoting,
            } => self.send_voice(chat, samples, quoting),
            Command::MarkPlayed {
                chat,
                message,
                sender,
                receipts,
            } => {
                if receipts {
                    self.mark_played(chat, message, sender);
                }
            }
            Command::ReceiptsPrivacy { disabled } => {
                self.emit(Event::ReceiptsPrivacy { disabled });
            }
            Command::InspectUpdate => {
                let events = self.events.clone();
                let waker = self.waker.clone();
                tokio::task::spawn_blocking(move || {
                    let result =
                        crate::updates::install::detect().map_err(|error| format!("{error:#}"));
                    let _ = events.send(Event::UpdateSupport(result));
                    waker.wake();
                });
            }
            Command::DownloadUpdate {
                release,
                source,
                channel,
            } => {
                let events = self.events.clone();
                let waker = self.waker.clone();
                tokio::task::spawn_blocking(move || {
                    let result =
                        crate::updates::download(&release, &source, channel, |received, total| {
                            let _ = events.send(Event::UpdateProgress { received, total });
                            waker.wake();
                        })
                        .map(Box::new)
                        .map_err(|error| format!("{error:#}"));
                    let _ = events.send(Event::UpdateDownloaded(result));
                    waker.wake();
                });
            }
            Command::InstallUpdate {
                prepared,
                arguments,
            } => {
                let events = self.events.clone();
                let waker = self.waker.clone();
                tokio::task::spawn_blocking(move || {
                    let result = crate::updates::install::handoff(&prepared, arguments)
                        .map_err(|error| format!("{error:#}"));
                    let _ = events.send(Event::UpdateInstalling(result));
                    waker.wake();
                });
            }
            Command::CheckForUpdates { channel } => {
                let checker = self.update_checker.clone();
                let events = self.events.clone();
                let waker = self.waker.clone();
                tokio::task::spawn_blocking(move || {
                    let endpoints = crate::updates::Source::GitHub.endpoints();
                    match checker.check(&endpoints, channel, crate::updates::zapext_version()) {
                        Some(crate::updates::CheckOutcome::Available(release)) => {
                            let _ = events.send(Event::UpdateAvailable {
                                version: release.version,
                                url: release.url,
                            });
                            waker.wake();
                        }
                        Some(_) => log::debug!("no newer release on this channel"),
                        None => log::debug!("update check already running"),
                    }
                });
            }
            Command::CheckUpdatesNow { channel } => {
                let checker = self.update_checker.clone();
                let events = self.events.clone();
                let waker = self.waker.clone();
                tokio::task::spawn_blocking(move || {
                    let endpoints = crate::updates::Source::GitHub.endpoints();
                    let outcome = checker
                        .check(&endpoints, channel, crate::updates::zapext_version())
                        .unwrap_or(crate::updates::CheckOutcome::Unavailable(
                            crate::updates::FetchError::Unexpected(
                                "An update check is already running".into(),
                            ),
                        ));
                    match outcome {
                        crate::updates::CheckOutcome::Available(release) => {
                            let _ = events.send(Event::UpdateAvailable {
                                version: release.version,
                                url: release.url,
                            });
                        }
                        crate::updates::CheckOutcome::UpToDate => {
                            let _ = events.send(Event::UpdateUpToDate);
                        }
                        crate::updates::CheckOutcome::Unavailable(error) => {
                            let _ = events.send(Event::UpdateCheckFailed(error.to_string()));
                        }
                    }
                    waker.wake();
                });
            }
            // handlers end
            Command::RecentStickers => {
                // Opening the picker is a fresh ask: failures get their
                // attempts back.
                self.sticker_give_up.clear();
                self.fetch_missing_stickers();
                self.emit_stickers();
            }
            Command::VideoPreview {
                chat,
                id,
                preview,
                seconds,
            } => {
                // One analysis answers length and poster together; either
                // may be missing while the other still applies.
                if preview.is_some() || seconds.is_some() {
                    let _ = self
                        .archive
                        .set_video_meta(&chat, &id, seconds, preview.as_deref());
                    self.emit_message(&chat, &id);
                }
            }
            Command::StickerThumbsReady => self.emit_stickers(),
            Command::FavoriteSticker { path } => {
                self.toggle_favorite_sticker(&path);
            }
            Command::FavoritePushed {
                hash,
                updated_at,
                result,
            } => {
                self.favorite_pushed(&hash, updated_at, result);
            }
            Command::FavoritesPushed => {
                self.favorites_pushing = false;
                if std::mem::take(&mut self.favorites_again) {
                    self.push_favorites();
                }
            }
            Command::FavoriteFetched { hash, result } => {
                self.favorite_fetched(&hash, result);
            }
            Command::SearchChat { chat, query } => {
                self.spawn_search(Some(chat), query, CHAT_SEARCH_LIMIT);
            }
            Command::RenderPdfPage { path, page, width } => {
                let commands = self.commands.clone();
                let reader = self.pdf.clone();
                let generation = self.pdf_generation.clone();
                // Only the newest request may pre-render the page after it.
                let pass = generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                tokio::task::spawn_blocking(move || {
                    let result = render_pdf_page(&reader, &path, page, width);
                    let _ = commands.send(Command::PdfPage {
                        path: path.clone(),
                        page,
                        width,
                        result,
                    });
                    // The next page is usually the one asked for next, so it
                    // is ready before the reader turns to it.
                    if generation.load(std::sync::atomic::Ordering::SeqCst) == pass {
                        let _ = render_pdf_page(&reader, &path, page + 1, width);
                    }
                });
            }
            Command::ForgetPdf => {
                // Nothing of the document stays in memory once the viewer is
                // closed, and a render still on its way will not refill it.
                // Generation-guarded: an older forget never clears a newer
                // document opened after it. The pass is captured now; the
                // blocking task clears only when no newer render or forget
                // moved the generation meanwhile.
                let pass = self
                    .pdf_generation
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                // Invalidation stays synchronous and immediate: newer renders
                // die by generation. The mutex wait and the clear itself
                // leave the main loop for a blocking task, so a slow
                // rasterization never stalls messages, sync, downloads or
                // retries behind it. The same Reader is reused while the
                // document stays open.
                let reader = self.pdf.clone();
                let generation = self.pdf_generation.clone();
                tokio::task::spawn_blocking(move || {
                    if generation.load(std::sync::atomic::Ordering::SeqCst) == pass {
                        reader
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .clear();
                    }
                });
            }
            Command::PdfPage {
                path,
                page,
                width,
                result,
            } => self.emit(Event::PdfPage {
                path,
                page,
                width,
                result,
            }),
            Command::PdfThumbs { path } => {
                let commands = self.commands.clone();
                let reader = self.pdf.clone();
                let generation = self.pdf_generation.clone();
                let thumbs = self.dirs.pdf_thumb_dir();
                let pass = generation.load(std::sync::atomic::Ordering::SeqCst);
                tokio::task::spawn_blocking(move || {
                    let _ = std::fs::create_dir_all(&thumbs);
                    let key = crate::pdf::thumb_key(&path);
                    let mut files = Vec::new();
                    let count = reader
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .pages(&path)
                        .unwrap_or(0)
                        .min(crate::pdf::THUMB_PAGES);
                    for page in 0..count {
                        // A newer document stands this loop down; whatever is
                        // already on disk shows up next time the file opens.
                        if generation.load(std::sync::atomic::Ordering::SeqCst) != pass {
                            return;
                        }
                        let target = thumbs.join(format!("{key}-{page:04}.png"));
                        if !target.is_file() {
                            let rendered = reader
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .thumb(&path, page, crate::pdf::THUMB_WIDTH);
                            let saved = rendered.ok().and_then(|rendered| {
                                let image = image::RgbaImage::from_raw(
                                    rendered.width,
                                    rendered.height,
                                    rendered.rgba,
                                )?;
                                image.save(&target).ok()?;
                                Some(())
                            });
                            if saved.is_none() {
                                continue;
                            }
                        }
                        files.push(target);
                    }
                    let _ = commands.send(Command::PdfThumbsReady { path, files });
                });
            }
            Command::PdfThumbsReady { path, files } => self.emit(Event::PdfThumbs { path, files }),
            Command::StickerFetched { hash, result } => {
                self.sticker_fetches.remove(&hash);
                match result {
                    Ok(path) => {
                        // The file name carries the hash the phone announced,
                        // but the identity the picker uses is the hash of the
                        // bytes. If they ever disagree, the bytes win, so the
                        // copy cannot show twice under two names.
                        let path = self.adopt_fetched_sticker(&hash, &path);
                        if let Err(error) = self.archive.set_sticker_path(&hash, &path) {
                            log::warn!("could not file sticker {hash}: {error}");
                        }
                    }
                    Err(error) => {
                        // Leave a sticker that keeps failing alone for now.
                        *self.sticker_tries.entry(hash.clone()).or_insert(0) += 1;
                        log::warn!("sticker {hash} could not be fetched: {error}");
                    }
                }
                self.fetch_missing_stickers();
                self.emit_stickers();
            }
            Command::MeInfo { about } => {
                self.me_about = about;
                match &self.me_about {
                    Some(about) => {
                        let _ = self.archive.set_meta("me_about", about);
                    }
                    None => {
                        let _ = self.archive.set_meta("me_about", "");
                    }
                }
                self.emit(Event::Me {
                    id: self.me(),
                    name: self.me_name.clone(),
                    about: self.me_about.clone(),
                });
            }
            Command::React {
                chat,
                message,
                emoji,
            } => self.react(chat, message, emoji),
            Command::SetArchived(chat, archived) => {
                // State and intent persist together: the interface never shows
                // an archive change the queue lost.
                let now = whatsapp_rust::wacore::time::now_millis();
                match self.archive.set_archived_queued(&chat, archived, now) {
                    Ok(_) => {
                        self.emit_chat(&chat);
                        // A fresh intent reopens the budget for every revision.
                        self.sync_attempts.retain(|(id, _), _| id != &chat);
                        self.sync_retry_at.remove(&chat);
                        self.push_archive_sync(&chat);
                    }
                    Err(error) => {
                        log::warn!("could not save an archive change: {error}");
                        // Repaint from storage truth: the optimistic interface
                        // change never persisted.
                        self.emit_chat(&chat);
                        self.emit(Event::Error(
                            "Could not save the archive change.".to_owned(),
                        ));
                    }
                }
            }
            Command::ChatSyncFlushed { chat, rev, ok } => {
                // A migrated task reports under its old id: settle the survivor.
                // The alias persists while either id may still report, so one
                // completion never consumes another revision way home.
                let chat = self.sync_aliases.get(&chat).cloned().unwrap_or(chat);
                // The acknowledgement binding dies with its task, settled or
                // stale: a later echo is judged by the persisted order.
                self.sync_dispatched.remove(&(chat.clone(), rev));
                // A completion only settles the revision it attempted: older
                // responses cannot erase or punish a newer intent.
                if !self.sync_in_flight.contains_key(&(chat.clone(), rev)) {
                    self.sync_attempts.remove(&(chat.clone(), rev));
                    return;
                }
                self.sync_in_flight.remove(&(chat.clone(), rev));
                if ok {
                    // One transaction records the order marker and removes
                    // exactly this intent: a crash between the two can never
                    // leave a concluded intent with no persisted order. Later
                    // echoes compare against the last accepted order, persisted.
                    match self.archive.complete_chat_sync(&chat, rev) {
                        Ok(_) => {
                            self.sync_attempts.remove(&(chat.clone(), rev));
                            self.sync_retry_at.remove(&chat);
                            // A newer intent queued mid-flight goes out on its
                            // own revision; a remaining flight still blocks it.
                            self.push_archive_sync(&chat);
                        }
                        Err(error) => {
                            log::warn!("could not record a completed archive sync: {error}");
                            // The phone already applied the change, so the
                            // intent stays queued and the tick resends the
                            // same idempotent value instead of pretending
                            // the conclusion persisted.
                            self.sync_retry_at
                                .insert(chat.clone(), Instant::now() + OFFLINE_SYNC_RETRY);
                            self.emit_chat(&chat);
                            self.emit(Event::Error(
                                "Could not save the archive change.".to_owned(),
                            ));
                        }
                    }
                } else if self
                    .archive
                    .queued_chat_sync(&chat, "archived")
                    .ok()
                    .flatten()
                    .is_some_and(|(_, _, current)| current > rev)
                {
                    // A superseded revision yields to the newer intent, which keeps
                    // its own attempt budget.
                    self.sync_attempts.remove(&(chat.clone(), rev));
                    self.push_archive_sync(&chat);
                } else {
                    let attempts = {
                        let counter = self.sync_attempts.entry((chat.clone(), rev)).or_insert(0);
                        *counter = (*counter + 1).min(9);
                        *counter
                    };
                    // Bounded and visible: after quiet rounds, say so once
                    // instead of diverging silently. The tick retries quietly.
                    if attempts == 4 {
                        self.emit(Event::Error(
                            "Could not sync the archive change to the phone.".to_owned(),
                        ));
                    }
                    if let Some(delay) = retry_delay(attempts) {
                        self.sync_retry_at
                            .insert(chat.clone(), Instant::now() + delay);
                    } else {
                        self.sync_retry_at.remove(&chat);
                    }
                }
            }
            Command::SetPinned(chat, pinned) => {
                let _ = self.archive.set_pinned(&chat, pinned);
                self.emit_chat(&chat);
                self.tell_phone(&chat, move |client, jid| async move {
                    if pinned {
                        client.chat_actions().pin_chat(&jid).await
                    } else {
                        client.chat_actions().unpin_chat(&jid).await
                    }
                    .map_err(|error| error.to_string())
                });
            }
            Command::SetMuted(chat, until) => {
                let _ = self.archive.set_muted(&chat, until);
                self.emit_chat(&chat);
                self.tell_phone(&chat, move |client, jid| async move {
                    match until {
                        None => client.chat_actions().unmute_chat(&jid).await,
                        Some(0) => client.chat_actions().mute_chat(&jid).await,
                        Some(seconds) => {
                            client
                                .chat_actions()
                                .mute_chat_until(&jid, seconds * 1000)
                                .await
                        }
                    }
                    .map_err(|error| error.to_string())
                });
            }
            Command::PairWithPhone(phone) => {
                let Some(client) = self.client.clone() else {
                    self.emit(Event::Error("Not connected to WhatsApp yet".to_owned()));
                    return;
                };
                self.pairing_phone = Some(phone.clone());
                self.pair_code = None;
                let status = self.unlinked();
                self.set_status(status);
                let commands = self.commands.clone();
                tokio::spawn(async move {
                    let result = client
                        .pair_with_code(PairCodeOptions {
                            phone_number: phone,
                            ..Default::default()
                        })
                        .await
                        .map_err(|error| error.to_string());
                    let _ = commands.send(Command::PairCode { result });
                });
            }
            Command::PairCode { result } => match result {
                Ok(code) => {
                    self.pair_code = Some(code);
                    let status = self.unlinked();
                    self.set_status(status);
                }
                Err(error) => {
                    self.pairing_phone = None;
                    self.emit(Event::Error(format!(
                        "Could not link by phone number: {error}"
                    )));
                    let status = self.unlinked();
                    self.set_status(status);
                }
            },
            Command::Unlink => {
                if let Some(client) = self.client.clone() {
                    client.logout().await;
                } else {
                    self.on_logged_out().await;
                }
            }
            Command::Reconnect => {
                if let Some(client) = self.client.clone() {
                    tokio::spawn(async move { client.reconnect_immediately().await });
                } else {
                    self.start_bot().await;
                }
            }
            Command::Shutdown => {}
            Command::OlderFailed { chat, error } => {
                self.pending_older.remove(&chat);
                self.emit(Event::OlderFetched { chat, more: true });
                self.emit(Event::Error(error));
            }
            Command::GroupInfoFailed { chat, permanent } => {
                self.handle_failed_group(chat, permanent);
            }
            Command::Sent { chat, id, error } => {
                if id.is_empty() {
                    // This is a command failure, not a failed message send.
                    if let Some(error) = error {
                        self.emit(Event::Error(error));
                    }
                    return;
                }
                let status = match &error {
                    Some(_) => Delivery::Failed,
                    None => Delivery::Sent,
                };
                let _ = self
                    .archive
                    .set_status(&chat, &id, status, crate::util::now());
                self.emit_message(&chat, &id);
                self.emit_chat(&chat);
                if let Some(error) = error {
                    self.emit(Event::Error(format!("Message not sent: {error}")));
                }
            }
            Command::Downloaded { chat, id, result } => {
                self.inflight_downloads.remove(&(chat.clone(), id.clone()));
                match &result {
                    Ok(path) => {
                        let _ = self.archive.set_media_path(&chat, &id, path);
                        self.download_retries.remove(&(chat.clone(), id.clone()));
                        self.analyze_video(&chat, &id);
                    }
                    Err(error) => {
                        let key = (chat.clone(), id.clone());
                        if retriable_download(error) && self.schedule_media_retry(&key) {
                            // Keep the bubble loading: a transient failure is
                            // repeated quietly, and the picture appears on its
                            // own without an error the reader has to dismiss.
                            return;
                        }
                        self.download_retries.remove(&key);
                    }
                }
                let for_picker = self.sticker_downloads.remove(&(chat.clone(), id.clone()));
                if for_picker && result.is_err() {
                    // The quiet retries are used up; do not queue it again
                    // until the picker is opened afresh.
                    self.sticker_give_up.insert((chat.clone(), id.clone()));
                }
                self.emit(Event::Media {
                    chat,
                    message: id,
                    result,
                });
                if for_picker {
                    // Keep the page filling in while the reader watches.
                    self.fetch_missing_stickers();
                    self.emit_stickers();
                }
            }
            Command::AvatarFetched { id, full, path } => {
                self.pending_avatars.remove(&(id.clone(), full));
                self.emit(Event::Avatar { id, full, path })
            }
            Command::AvatarFailed { id, full } => {
                // Count the failure and schedule the next try. The entry
                // survives: clearing it here would restart the count and
                // retry forever without ever reporting absence.
                let retry = self.pending_avatars.entry((id, full)).or_default();
                retry.attempts += 1;
                retry.in_flight = false;
                retry.next_retry = Instant::now() + avatar_backoff(retry.attempts);
            }
            Command::GroupRecipients {
                chat,
                id,
                recipients,
                lids,
                stored,
            } => {
                for (lid, pn) in lids {
                    self.learn_lid(&lid, &pn);
                }
                let saved = self.save_group_recipients(&chat, &id, &recipients);
                let _ = stored.send(saved);
            }
            Command::GroupInfo {
                chat,
                name,
                participants,
                read_only,
                community,
                ephemeral_expiration,
                ephemeral_setting_timestamp,
            } => {
                self.group_info_tries.remove(&chat);
                let _ =
                    self.archive
                        .set_group_info(&chat, name.as_deref(), &participants, read_only);
                let _ = self.archive.set_group_community(&chat, community);
                if let Some(expiration) = ephemeral_expiration {
                    let _ = self.archive.set_ephemeral(
                        &chat,
                        expiration,
                        ephemeral_setting_timestamp.unwrap_or_default(),
                    );
                }
                self.emit_chat(&chat);
            }
        }
    }

    /// Save the same audience the protocol library uses to encrypt the send.
    fn save_group_recipients(&self, chat: &str, id: &str, recipients: &[String]) -> bool {
        let recipients: Vec<_> = recipients
            .iter()
            .map(|id| self.canonical_str(id))
            .filter(|id| !self.is_me(id))
            .collect();
        match self
            .archive
            .snapshot_group_recipients(chat, id, &recipients)
        {
            Ok(()) => true,
            Err(error) => {
                log::warn!("could not save the group message audience: {error}");
                false
            }
        }
    }

    /// The reply context and the archived row for one quoted message.
    fn quote_of(&self, chat: &str, id: &str, jid: &Jid) -> Option<(wa::ContextInfo, Message)> {
        let raw = self.archive.raw(chat, id).ok().flatten()?;
        let quoted = wa::Message::decode_from_slice(&raw).ok()?;
        let row = self.archive.message(chat, id).ok().flatten()?;
        let sender = Self::jid_of(&row.sender).unwrap_or_else(|| jid.clone());
        let context = whatsapp_rust::wacore::proto_helpers::build_quote_context_with_info(
            row.id.clone(),
            &sender,
            jid,
            jid,
            &quoted,
        );
        Some((context, row))
    }

    /// The summary a reply shows for the message it answers.
    fn quoted_summary(&self, row: Message) -> Quoted {
        Quoted {
            mentions: row.mentions.clone(),
            id: row.id,
            sender_name: if row.from_me {
                Some("You".to_owned())
            } else {
                row.sender_name
                    .clone()
                    .or_else(|| self.name_for(&row.sender))
            },
            sender: row.sender,
            summary: row.content.summary(),
        }
    }

    fn send_text(
        &mut self,
        chat: ChatId,
        text: String,
        quoting: Option<String>,
        mentions: Vec<String>,
    ) {
        if !self.send_allowed(&chat) {
            log::debug!("refusing text send without proven capability");
            return;
        }
        let (Some(client), Some(jid)) = (self.client.clone(), Self::jid_of(&chat)) else {
            self.emit(Event::Error("Not connected to WhatsApp".to_owned()));
            return;
        };
        let mut quoted_row = None;
        let context = quoting.as_deref().and_then(|id| {
            let (context, row) = self.quote_of(&chat, id, &jid)?;
            quoted_row = Some(row);
            Some(context)
        });
        let mut message = outgoing_text(text.clone(), context, &mentions);
        let expiration = self.apply_ephemeral(&chat, &mut message);
        let mentions = self.mentions_of(&mentions);
        let id = client.generate_message_id();
        let row = Message {
            id: id.clone(),
            chat: chat.clone(),
            sender: self.me(),
            sender_name: None,
            from_me: true,
            timestamp: crate::util::now(),
            content: Content::text(text),
            status: Delivery::Pending,
            delivered_at: None,
            read_at: None,
            quoted: quoted_row.map(|row| self.quoted_summary(row)),
            reactions: Vec::new(),
            edited: false,
            mentions,
            forwarded: false,
            thumbnail: None,
        };
        self.store_message(row, Some(message.encode_to_vec()), None);
        tokio::spawn(send_outgoing(
            client,
            self.commands.clone(),
            chat,
            jid,
            id,
            message,
            expiration,
        ));
    }

    /// Whether an outgoing send may start for this chat. The address
    /// decides first: newsletters have no proven send path whether or not
    /// the archive knows them. Known chats follow the shared rule; new
    /// recipients are allowed only for directly supported types. A store
    /// failure denies rather than permits.
    fn send_allowed(&self, chat: &str) -> bool {
        if chat.ends_with("@newsletter") {
            return false;
        }
        Self::decide_send(self.archive.chat(chat), chat)
    }

    /// Applies one archive lookup to the send guard: known chats follow the
    /// shared rule, unknown chats only for supported types, and a store
    /// failure denies rather than permits.
    fn decide_send<E: std::fmt::Display>(lookup: Result<Option<Chat>, E>, chat: &str) -> bool {
        match lookup {
            Ok(Some(row)) => crate::model::can_send(&row),
            Ok(None) => Self::send_allowed_unknown(chat),
            Err(error) => {
                log::debug!("send guard could not read chat: {error}");
                false
            }
        }
    }

    /// Whether a send may start to a chat missing from the archive: only
    /// direct chats and groups, the types the app can open and send to.
    fn send_allowed_unknown(chat: &str) -> bool {
        matches!(
            crate::model::ChatKind::from_id(chat),
            crate::model::ChatKind::Direct | crate::model::ChatKind::Group
        )
    }

    fn forward_message(&mut self, from_chat: ChatId, message_id: String, to_chat: ChatId) {
        if !self.send_allowed(&to_chat) {
            log::debug!("refusing forward without proven capability");
            return;
        }
        let (Some(client), Some(jid)) = (self.client.clone(), Self::jid_of(&to_chat)) else {
            self.emit(Event::Error("Not connected to WhatsApp".to_owned()));
            return;
        };
        let Ok(Some(source)) = self.archive.message(&from_chat, &message_id) else {
            self.emit(Event::Error(
                "This message is not stored on this computer".to_owned(),
            ));
            return;
        };
        if !source.content.forwardable() {
            self.emit(Event::Error("This message cannot be forwarded".to_owned()));
            return;
        }
        let Ok(Some(raw)) = self.archive.raw(&from_chat, &message_id) else {
            self.emit(Event::Error(
                "The original message data is not available to forward".to_owned(),
            ));
            return;
        };
        let Ok(original) = wa::Message::decode_from_slice(&raw) else {
            self.emit(Event::Error(
                "The original message data could not be read".to_owned(),
            ));
            return;
        };
        // whatsapp-rust owns the forwarding rules: unwrap transient wrappers,
        // strip quote chains and secrets, and retain reusable media metadata.
        let (message, expiration) =
            outgoing_forward(&original, self.ephemeral_expiration(&to_chat));
        let id = client.generate_message_id();
        let mentions = self.mentions_of(&mentioned_of(&message));
        let thumbnail = thumbnail_of(&message).or_else(|| source.thumbnail.clone());
        let row = forwarded_row(
            source,
            to_chat.clone(),
            self.me(),
            id.clone(),
            crate::util::now(),
            mentions,
            thumbnail,
        );
        self.store_message(row, Some(message.encode_to_vec()), None);
        tokio::spawn(send_outgoing(
            client,
            self.commands.clone(),
            to_chat,
            jid,
            id,
            message,
            expiration,
        ));
    }

    /// Forwards selected messages to several chats, paced like a person
    /// tapping through them.
    ///
    /// Anything beyond WhatsApp's destination caps is cut with an
    /// explanation instead of fanning out. Every pair still travels the
    /// same single-forward path, one at a time.
    fn forward_many(&mut self, from_chat: ChatId, messages: Vec<String>, to_chats: Vec<ChatId>) {
        if messages.is_empty() || to_chats.is_empty() {
            return;
        }
        if self.client.is_none() {
            self.emit(Event::Error("Not connected to WhatsApp".to_owned()));
            return;
        }
        let frequently = messages
            .iter()
            .any(|id| is_frequently_forwarded(self.forwarding_score(&from_chat, id)));
        let to_chats = cap_destinations(to_chats, frequently);
        if to_chats.is_empty() {
            return;
        }
        if frequently {
            self.emit(Event::Info(
                "Frequently forwarded messages go to one chat at a time".to_owned(),
            ));
        }
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let mut first = true;
            for id in &messages {
                for to in &to_chats {
                    if !first {
                        tokio::time::sleep(FORWARD_PACE).await;
                    }
                    first = false;
                    let _ = commands.send(Command::Forward {
                        from_chat: from_chat.clone(),
                        message: id.clone(),
                        to_chat: to.clone(),
                    });
                }
            }
            let _ = commands.send(Command::Forwarded {
                messages: messages.len(),
                chats: to_chats.len(),
            });
        });
    }

    /// Forwarding score of an archived message, for the frequent-forward cap.
    /// Messages without stored data count as fresh; the send path still
    /// refuses whatever it cannot forward.
    fn forwarding_score(&self, chat: &str, id: &str) -> u32 {
        self.archive
            .raw(chat, id)
            .ok()
            .flatten()
            .and_then(|raw| wa::Message::decode_from_slice(&raw).ok())
            .map(|message| forwarding_score_of(message.get_base_message()))
            .unwrap_or(0)
    }

    fn mark_read(&mut self, chat: ChatId, receipts: bool) {
        let Ok(Some(row)) = self.archive.chat(&chat) else {
            return;
        };
        // Collect before advancing the archive's read position.
        let ids = if receipts {
            self.archive
                .unread_incoming(&chat, row.unread)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let _ = self.archive.mark_read(&chat);
        self.emit_chat(&chat);
        if row.unread == 0 {
            return;
        }
        let _ = self.archive.queue_read_sync(&chat);
        self.pump_read_sync();
        self.send_read_receipts(chat, ids);
    }

    fn pump_read_sync(&mut self) {
        if !matches!(self.status, LinkStatus::Connected) || !self.read_sync.ready(Instant::now()) {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        for (chat, through) in self.archive.pending_reads().unwrap_or_default() {
            let Some(jid) = Self::jid_of(&chat) else {
                continue;
            };
            if !self.read_sync.start(&chat, through, Instant::now()) {
                break;
            }
            let client = client.clone();
            let commands = self.commands.clone();
            tokio::spawn(async move {
                // This update is private to our devices, even with blue ticks
                // disabled. Keep the original position when retrying offline
                // reads, not the latest message received since the local read.
                let range = whatsapp_rust::message_range(through, None, Vec::new());
                let result = client
                    .chat_actions()
                    .mark_chat_as_read(&jid, true, Some(range))
                    .await;
                if let Err(error) = &result {
                    log::debug!("chat read state not synced: {error}");
                }
                let _ = commands.send(Command::ReadSyncFinished {
                    chat,
                    through,
                    success: result.is_ok(),
                });
            });
            // Every read-state write uses regular_low. A queue of spawned tasks
            // would each retry the same broken collection before we can back off.
            break;
        }
    }

    fn send_read_receipts(&self, chat: ChatId, ids: Vec<(String, String)>) {
        if ids.is_empty() {
            return;
        }
        let (Some(client), Some(jid)) = (self.client.clone(), Self::jid_of(&chat)) else {
            return;
        };
        let is_group = jid.is_group();
        let mut by_sender: HashMap<Option<String>, Vec<String>> = HashMap::new();
        for (id, sender) in ids {
            by_sender
                .entry(is_group.then_some(sender))
                .or_default()
                .push(id);
        }
        let commands = self.commands.clone();
        tokio::spawn(async move {
            if !receipts_allowed(&client, &jid, &commands).await {
                return;
            }
            for (sender, ids) in by_sender {
                let sender = sender.and_then(|sender| sender.parse::<Jid>().ok());
                let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
                if let Err(error) = client.mark_as_read(&jid, sender.as_ref(), &ids).await {
                    log::debug!("read receipt not sent: {error}");
                }
            }
        });
    }

    /// Refreshes stored quote ids and names with current mappings.
    fn polish(&self, message: &mut Message) {
        self.polish_poll(message);
        if let Some(quoted) = message.quoted.as_mut() {
            let sender = self.canonical_str(&quoted.sender);
            if sender != quoted.sender || quoted.sender_name.is_none() {
                quoted.sender_name = self.name_for(&sender);
                quoted.sender = sender;
            }
            quoted.summary = self.pn_tokens(&quoted.summary);
            for mention in &mut quoted.mentions {
                mention.id = self.canonical_str(&mention.id);
            }
            if quoted.mentions.is_empty() {
                quoted.mentions = self.mention_tokens(&quoted.summary);
            }
        }
        for mention in &mut message.mentions {
            mention.id = self.canonical_str(&mention.id);
        }
    }

    fn load_chat(&mut self, chat: ChatId, before: Option<super::PageKey>) {
        // A chat behind a privacy id may still have rows under its old id.
        // Reading both files the view under the asked id, so saved messages
        // appear while the background rekey finishes the move.
        let before_ref = before.as_ref().map(|(time, id)| (*time, id.as_str()));
        let mut merged = Vec::new();
        let mut complete = true;
        let mut failed: Option<String> = None;
        // The asked id first, so ties keep its rows before the old ones.
        let mut ids = vec![chat.clone()];
        if let Some(alt) = self.alt_chat_id(&chat)
            && alt != chat
        {
            ids.push(alt);
        }
        for id in ids {
            match self.archive.messages(&id, before_ref, PAGE + 1) {
                Ok(mut messages) => {
                    complete = complete && messages.len() <= PAGE;
                    if messages.len() > PAGE {
                        messages.remove(0);
                    }
                    merged.append(&mut messages);
                }
                Err(error) => {
                    failed = Some(error.to_string());
                    break;
                }
            }
        }
        if let Some(error) = failed {
            self.emit(Event::Error(format!("Could not read the chat: {error}")));
            return;
        }
        // Oldest first like a single read, one row per message id.
        merged.sort_by(|a, b| a.timestamp.cmp(&b.timestamp).then(a.id.cmp(&b.id)));
        let mut seen = std::collections::HashSet::new();
        merged.retain(|message| seen.insert(message.id.clone()));
        let complete = complete && merged.len() <= PAGE;
        // The extra row only says whether an older page exists.
        if merged.len() > PAGE {
            merged.remove(0);
        }
        for message in &mut merged {
            self.polish(message);
        }
        self.emit(Event::Messages {
            chat: chat.clone(),
            messages: merged,
            older: before.is_some(),
            complete,
        });
        if before.is_none() && ChatKind::from_id(&chat) == ChatKind::Group {
            // Force group metadata when opening a group.
            self.request_group_info(&chat, false);
        }
        if before.is_none()
            && ChatKind::from_id(&chat) == ChatKind::Direct
            && chat != self.me()
            && self.presence_subscribed.insert(chat.clone())
            && let (Some(client), Some(jid)) = (self.client.clone(), Self::jid_of(&chat))
        {
            tokio::spawn(async move {
                if let Err(error) = client.presence().subscribe(jid).await {
                    log::debug!("presence not subscribed: {error}");
                }
            });
        }
    }

    /// Rejects downloads that could never display, before they are filed.
    /// An empty file, or image bytes no decoder accepts, would otherwise sit
    /// in the cache showing an error tile until cleared by hand. Runs off the
    /// async runtime because decoding can take a moment on large files.
    fn validate_media_bytes(bytes: &[u8], mime: &str) -> Result<(), String> {
        if bytes.is_empty() {
            return Err("The download came back empty".to_owned());
        }
        if mime.starts_with("image/") && image::load_from_memory(bytes).is_err() {
            return Err("The download is not a readable picture".to_owned());
        }
        Ok(())
    }

    /// Absolute ceiling for one attachment download. Declared lengths only
    /// narrow this down; exceeding it is always an error with an explicit
    /// message, never a silent new restriction: WhatsApp media stays far
    /// below this bound.
    const DOWNLOAD_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
    /// Slack over the declared plaintext length for framing overhead and
    /// length lies in either direction.
    const DOWNLOAD_LENGTH_SLACK: u64 = 1024 * 1024;
    /// Total budget for one download including re-upload and retry. A
    /// backstop against hanging forever, not a speed target.
    const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

    /// Images at or below this size are fully decoded for validation;
    /// larger ones are checked by header only (format, dimensions). A
    /// full decode needs the whole file plus its bitmap in RAM, which no
    /// download validation may demand unboundedly.
    const IMAGE_FULL_DECODE_MAX_BYTES: u64 = 16 * 1024 * 1024;
    /// Largest image side the validator accepts, in pixels. Matches the
    /// JPEG ceiling; phone pictures never come close.
    const IMAGE_MAX_SIDE: u32 = 65535;
    /// Largest pixel count the validator accepts. Below what the default
    /// 512 MiB decoder allocation guard would still allow, so oversized
    /// dimensions fail here first with a clear message.
    const IMAGE_MAX_PIXELS: u64 = 100_000_000;

    fn download(&mut self, chat: ChatId, id: String) {
        let Some(client) = self.client.clone() else {
            self.emit(Event::Media {
                chat,
                message: id,
                result: Err("Not connected to WhatsApp".to_owned()),
            });
            return;
        };
        let raw = self.archive.raw(&chat, &id).ok().flatten();
        let Some(message) = raw.and_then(|raw| wa::Message::decode_from_slice(&raw).ok()) else {
            self.emit(Event::Media {
                chat,
                message: id,
                result: Err("Attachment download keys are missing".to_owned()),
            });
            return;
        };
        let base = message.get_base_message().clone();
        let (downloadable, mime, file_name): (Box<dyn Downloadable>, String, Option<String>) =
            if let Some(image) = base.image_message.as_option() {
                (
                    Box::new(image.clone()),
                    image.mimetype.clone().unwrap_or_default(),
                    None,
                )
            } else if let Some(video) = base
                .video_message
                .as_option()
                .or(base.ptv_message.as_option())
            {
                (
                    Box::new(video.clone()),
                    video.mimetype.clone().unwrap_or_default(),
                    None,
                )
            } else if let Some(audio) = base.audio_message.as_option() {
                (
                    Box::new(audio.clone()),
                    audio.mimetype.clone().unwrap_or_default(),
                    None,
                )
            } else if let Some(document) = base.document_message.as_option() {
                (
                    Box::new(document.clone()),
                    document.mimetype.clone().unwrap_or_default(),
                    document.file_name.clone(),
                )
            } else if let Some(sticker) = base.sticker_message.as_option() {
                (
                    Box::new(sticker.clone()),
                    sticker.mimetype.clone().unwrap_or_default(),
                    None,
                )
            } else {
                self.emit(Event::Media {
                    chat,
                    message: id,
                    result: Err("This message has no downloadable file".to_owned()),
                });
                return;
            };
        // Keep metadata needed for one media re-upload request and retry.
        let media_key = base
            .image_message
            .as_option()
            .and_then(|media| media.media_key.clone())
            .or_else(|| {
                base.video_message
                    .as_option()
                    .or(base.ptv_message.as_option())
                    .and_then(|media| media.media_key.clone())
            })
            .or_else(|| {
                base.audio_message
                    .as_option()
                    .and_then(|media| media.media_key.clone())
            })
            .or_else(|| {
                base.document_message
                    .as_option()
                    .and_then(|media| media.media_key.clone())
            })
            .or_else(|| {
                base.sticker_message
                    .as_option()
                    .and_then(|media| media.media_key.clone())
            })
            .unwrap_or_default();
        let jid = Self::jid_of(&chat);
        let row = self.archive.message(&chat, &id).ok().flatten();
        let is_from_me = row.as_ref().is_some_and(|row| row.from_me);
        let participant = match (&jid, &row) {
            (Some(jid), Some(row)) if jid.is_group() => Self::jid_of(&row.sender),
            _ => None,
        };
        let mut fresh_base = base;
        let mut refreshed = move |direct: String| -> Option<Box<dyn Downloadable>> {
            if let Some(media) = fresh_base.image_message.as_option_mut() {
                media.direct_path = Some(direct);
                media.url = None;
                return Some(Box::new(media.clone()));
            }
            if let Some(media) = fresh_base
                .video_message
                .as_option_mut()
                .or(fresh_base.ptv_message.as_option_mut())
            {
                media.direct_path = Some(direct);
                media.url = None;
                return Some(Box::new(media.clone()));
            }
            if let Some(media) = fresh_base.audio_message.as_option_mut() {
                media.direct_path = Some(direct);
                media.url = None;
                return Some(Box::new(media.clone()));
            }
            if let Some(media) = fresh_base.document_message.as_option_mut() {
                media.direct_path = Some(direct);
                media.url = None;
                return Some(Box::new(media.clone()));
            }
            if let Some(media) = fresh_base.sticker_message.as_option_mut() {
                media.direct_path = Some(direct);
                media.url = None;
                return Some(Box::new(media.clone()));
            }
            None
        };
        let dir = self.dirs.media_cache_dir();
        let commands = self.commands.clone();
        let slots = self.download_slots.clone();
        if !self.inflight_downloads.insert((chat.clone(), id.clone())) {
            // Already fetching this file: the running task notifies the
            // bubble when it finishes, so a second fetch would only
            // duplicate the bytes on the wire.
            return;
        }
        let limits = download_limits_for(downloadable.file_length());
        let final_path = media_path(&dir, &chat, &id, &mime, file_name.as_deref());
        let mut temp_os = final_path.clone().into_os_string();
        temp_os.push(".part");
        let temp_path = PathBuf::from(temp_os);
        tokio::spawn(async move {
            let _slot = slots.acquire_owned().await;
            // Stream into a temporary file next to the destination: RAM stays
            // flat regardless of attachment size, and a partial file is never
            // published. The guard removes the temporary file on every exit
            // that did not rename it away, including timeouts.
            let _temp = TempGuard::new(temp_path.clone());
            let deadline = tokio::time::Instant::now() + limits.timeout;
            let result =
                match fetch_to_temp(&client, &*downloadable, &dir, &temp_path, limits, deadline)
                    .await
                {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        let expired = is_expired_media_error(&error);
                        let error = error.to_string();
                        match (&jid, expired && !media_key.is_empty()) {
                            (Some(jid), true) => {
                                // Ask the phone to re-upload expired media, then retry once.
                                let request = MediaReuploadRequest {
                                    msg_id: &id,
                                    chat_jid: jid,
                                    media_key: &media_key,
                                    is_from_me,
                                    participant: participant.as_ref(),
                                };
                                match tokio::time::timeout_at(
                                    deadline,
                                    client.media_reupload().request(&request),
                                )
                                .await
                                {
                                    Ok(Ok(MediaRetryResult::Success { direct_path })) => {
                                        match refreshed(direct_path) {
                                            Some(again) => match fetch_to_temp(
                                                &client, &*again, &dir, &temp_path, limits,
                                                deadline,
                                            )
                                            .await
                                            {
                                                Ok(()) => Ok(()),
                                                Err(error) => Err(error.to_string()),
                                            },
                                            None => Err(error),
                                        }
                                    }
                                    Ok(Ok(_)) => {
                                        Err("No longer available on WhatsApp's servers".to_owned())
                                    }
                                    Ok(Err(error)) => {
                                        log::info!("media re-upload was not granted: {error}");
                                        Err("No longer available on WhatsApp's servers".to_owned())
                                    }
                                    Err(_) => Err(format!(
                                        "Re-upload request timed out after {} seconds",
                                        limits.timeout.as_secs()
                                    )),
                                }
                            }
                            _ => Err(error),
                        }
                    }
                };
            let result = match result {
                Ok(()) => {
                    let mime = mime.clone();
                    let temp_path = temp_path.clone();
                    let final_path = final_path.clone();
                    // Images validate from disk with bounded memory (headers
                    // always, full decode only when small); every other kind
                    // only needs a non-empty file, so large videos never
                    // cross through RAM here either.
                    let checked = if mime.starts_with("image/") {
                        let read_path = temp_path.clone();
                        let mime = mime.clone();
                        match tokio::task::spawn_blocking(move || {
                            validate_image_file(
                                &read_path,
                                &mime,
                                Worker::IMAGE_FULL_DECODE_MAX_BYTES,
                            )
                        })
                        .await
                        {
                            Ok(inner) => inner,
                            Err(join) => Err(join.to_string()),
                        }
                    } else {
                        match tokio::fs::metadata(&temp_path).await {
                            Ok(metadata) if metadata.len() > 0 => Ok(()),
                            Ok(_) => Err("The download came back empty".to_owned()),
                            Err(error) => Err(error.to_string()),
                        }
                    };
                    match checked {
                        Ok(()) => publish_download(&temp_path, &final_path)
                            .await
                            .map(|()| final_path),
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(error),
            };
            let _ = commands.send(Command::Downloaded { chat, id, result });
        });
    }

    /// Queues another quiet attempt of a download that failed.
    ///
    /// Returns true when the repeat was scheduled, so the interface stays in
    /// its loading state; false means the attempts are exhausted and the
    /// failure has to be reported.
    fn schedule_media_retry(&mut self, key: &(ChatId, String)) -> bool {
        let attempts = self.download_retries.entry(key.clone()).or_insert(0);
        if *attempts as usize >= MEDIA_RETRY_DELAYS.len() {
            return false;
        }
        let delay = MEDIA_RETRY_DELAYS[*attempts as usize];
        *attempts += 1;
        let (chat, message) = key.clone();
        let commands = self.commands.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = commands.send(Command::Download { chat, message });
        });
        true
    }

    /// Downloads missing recent and archived stickers for the picker.
    fn fetch_missing_stickers(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let phone = match self.archive.phone_stickers() {
            Ok(list) => list,
            Err(error) => {
                log::warn!("could not list the phone's stickers: {error}");
                Vec::new()
            }
        };
        let dir = self.dirs.sticker_cache_dir();
        for sticker in stickers_to_fetch(phone, &self.sticker_fetches, &self.sticker_tries) {
            self.sticker_fetches.insert(sticker.hash.clone());
            let Ok(meta) = wa::StickerMetadata::decode_from_slice(&sticker.raw) else {
                self.sticker_fetches.remove(&sticker.hash);
                continue;
            };
            let client = client.clone();
            let commands = self.commands.clone();
            let dir = dir.clone();
            let slots = self.download_slots.clone();
            let hash = sticker.hash;
            tokio::spawn(async move {
                // Wait for a slot: the picker fills in steadily instead of
                // asking the server for everything at once.
                let _slot = slots.acquire_owned().await;
                let result = async {
                    let bytes = client
                        .download(&PhoneSticker(meta))
                        .await
                        .map_err(|error| error.to_string())?;
                    tokio::fs::create_dir_all(&dir)
                        .await
                        .map_err(|error| error.to_string())?;
                    let path = dir.join(format!("{hash}.webp"));
                    tokio::fs::write(&path, &bytes)
                        .await
                        .map_err(|error| error.to_string())?;
                    Ok(path)
                }
                .await;
                let _ = commands.send(Command::StickerFetched { hash, result });
            });
        }
        match self.archive.stickers_without_file(STICKER_ROUND) {
            Ok(list) => {
                for (chat, id) in list {
                    if self.sticker_give_up.contains(&(chat.clone(), id.clone())) {
                        continue;
                    }
                    if self.sticker_downloads.insert((chat.clone(), id.clone())) {
                        self.download(chat, id);
                    }
                }
            }
            Err(error) => log::warn!("could not list unfetched stickers: {error}"),
        }
    }

    /// Learns a downloaded video's real length and a better poster.
    ///
    /// The phone's metadata and thumbnail stay on screen until this
    /// answers. The poster is rebuilt even when one arrived, because a
    /// 96 px phone thumbnail cannot carry a 440 px bubble; the length
    /// fills in whenever the message arrived with none (or a zero).
    fn analyze_video(&mut self, chat: &str, id: &str) {
        let ready = self.archive.message(chat, id).ok().flatten().filter(|row| {
            matches!(&row.content, Content::Video { gif: false, .. })
                && row
                    .content
                    .media()
                    .is_some_and(|media| media.path.is_some())
        });
        let Some(row) = ready else {
            return;
        };
        let path = row
            .content
            .media()
            .and_then(|media| media.path.clone())
            .expect("just checked");
        if !path.is_file() {
            return;
        }
        let commands = self.commands.clone();
        let (chat, id) = (chat.to_owned(), id.to_owned());
        tokio::task::spawn_blocking(move || {
            let analysis = crate::video::analyze(&path);
            let _ = commands.send(Command::VideoPreview {
                chat,
                id,
                preview: analysis.poster,
                seconds: analysis.seconds,
            });
        });
    }

    /// Fills length and poster for videos downloaded before this version
    /// learned to analyze them. One background task walks them in order,
    /// so an archive full of videos never storms the disk; each answer
    /// refreshes its own bubble without a new download.
    fn backfill_video_meta(&mut self) {
        let Ok(rows) = self.archive.videos_needing_meta() else {
            return;
        };
        let rows: Vec<_> = rows
            .into_iter()
            .filter(|(_, _, path)| path.is_file())
            .collect();
        if rows.is_empty() {
            return;
        }
        let commands = self.commands.clone();
        tokio::task::spawn_blocking(move || {
            for (chat, id, path) in rows {
                let analysis = crate::video::analyze(&path);
                if analysis.poster.is_some() || analysis.seconds.is_some() {
                    let _ = commands.send(Command::VideoPreview {
                        chat,
                        id,
                        preview: analysis.poster,
                        seconds: analysis.seconds,
                    });
                }
            }
        });
    }

    /// Evicts a cached file that never decodes and fetches it again.
    ///
    /// Phone-cache copies are cleared and re-downloaded through the sticker
    /// fetcher; chat-media copies are cleared and re-downloaded as
    /// attachments. Anything else (saved stickers, imported packs) is the
    /// user's own file and is left alone.
    fn heal_sticker(&mut self, path: &Path) {
        let _ = std::fs::remove_file(path);
        if path.starts_with(self.dirs.sticker_cache_dir()) {
            if let Some(hash) = path.file_stem().and_then(|stem| stem.to_str()) {
                let _ = self.archive.clear_sticker_path(hash);
            }
            self.fetch_missing_stickers();
            self.emit_stickers();
            return;
        }
        let Ok(list) = self.archive.media_paths() else {
            return;
        };
        let Some((chat, id, _)) = list.into_iter().find(|(_, _, known)| known == path) else {
            return;
        };
        let _ = self.archive.clear_media_path(&chat, &id);
        self.emit_message(&chat, &id);
        self.download(chat, id);
    }

    /// Deletes a thumbnail that never decodes so it is rebuilt from the
    /// original file. The original is never touched and nothing is
    /// downloaded: thumbnails are purely local derivatives.
    fn heal_sticker_thumb(&mut self, path: &Path) {
        // (Re)queue the rebuild without resetting past failures, so a
        // hopeless file still reaches give-up instead of looping forever.
        self.thumb_heals
            .entry(path.to_path_buf())
            .or_insert_with(|| ThumbHeal {
                attempts: 0,
                next_retry: Instant::now(),
                in_flight: false,
            });
        self.pump_thumb_heals();
    }

    /// Dispatches due thumbnail rebuilds to blocking tasks and returns at
    /// once: each result comes back as a command the worker applies later.
    /// A success deletes the request, a failure schedules the next try,
    /// and the cap reports failure once and deletes it. The interface
    /// applies these results instead of inferring completion from the file.
    fn pump_thumb_heals(&mut self) {
        self.pump_thumb_heals_with(std::sync::Arc::new(|thumbs: PathBuf, file: PathBuf| {
            crate::stickers::build_thumb(&thumbs, &file)
        }));
    }

    /// Same dispatch with an injectable build step, so tests can gate the
    /// tasks on a barrier instead of hoping a real decode stays slow.
    fn pump_thumb_heals_with(
        &mut self,
        build: std::sync::Arc<dyn Fn(PathBuf, PathBuf) -> Result<PathBuf, String> + Send + Sync>,
    ) {
        let now = Instant::now();
        let flying = self
            .thumb_heals
            .values()
            .filter(|heal| heal.in_flight)
            .count();
        let mut slots = THUMB_HEAL_SLOTS.saturating_sub(flying);
        if slots == 0 {
            return;
        }
        let thumbs = self.dirs.sticker_thumb_dir();
        let due: Vec<PathBuf> = self.thumb_heals.keys().cloned().collect();
        for path in due {
            if slots == 0 {
                break;
            }
            match self.thumb_heals.get(&path) {
                Some(heal) if !heal.in_flight && now >= heal.next_retry => {}
                _ => continue,
            }
            // A build validates any cached file first and regenerates when
            // it does not decode, so no delete is needed here. A path with
            // no content-hash name can never address a thumbnail: fail it
            // at once instead of scheduling hopeless retries.
            let Some(thumb) = crate::stickers::thumb_path(&thumbs, &path) else {
                self.thumb_heals.remove(&path);
                self.emit(Event::StickerThumb { path, ok: false });
                continue;
            };
            let _ = thumb;
            if let Some(heal) = self.thumb_heals.get_mut(&path) {
                heal.in_flight = true;
            } else {
                continue;
            }
            slots -= 1;
            let commands = self.commands.clone();
            let task_thumbs = thumbs.clone();
            let task_build = build.clone();
            tokio::task::spawn_blocking(move || {
                let result = task_build(task_thumbs, path.clone());
                if let Err(error) = &result {
                    log::debug!("thumbnail rebuild failed: {error}");
                }
                let _ = commands.send(Command::ThumbHealFinished {
                    path,
                    ok: result.is_ok(),
                });
            });
        }
    }

    /// Records one failed thumbnail rebuild: schedules the next try or,
    /// past the cap, reports failure once and drops the request.
    fn fail_thumb_heal(&mut self, path: PathBuf, attempts: u32, now: Instant) {
        if attempts >= THUMB_HEAL_MAX_FAILURES {
            self.thumb_heals.remove(&path);
            self.emit(Event::StickerThumb { path, ok: false });
        } else if let Some(heal) = self.thumb_heals.get_mut(&path) {
            heal.attempts = attempts + 1;
            heal.in_flight = false;
            heal.next_retry = now + thumb_heal_backoff(heal.attempts);
        }
    }

    /// Reclaims the app's own attachment cache, once per run.
    ///
    /// The archive is the list of what still matters: the file every message
    /// points at. Anything else in the folder is left over from a failed
    /// write, an interrupted download, or a message that is gone. Nothing
    /// the user saved lives there, so nothing of theirs can be lost.
    fn pump_cache(&mut self) {
        if self.cache_swept {
            return;
        }
        self.cache_swept = true;
        // Videos downloaded before analysis existed get their length and
        // poster now, without a new download.
        self.backfill_video_meta();
        let media = self.dirs.media_cache_dir();
        // One protection policy for every cleanup: message attachments,
        // sticker favorites, and cataloged copies. Unprovable means keep
        // everything, exactly like the direct removal path.
        let Some(protected) = self.archive.protected_files() else {
            log::warn!("attachments: keeping them all, references unprovable");
            return;
        };
        let mut keep = sweep_keep_set(protected, &[]);
        tokio::task::spawn_blocking(move || {
            // Interrupted publishes restore before the sweep: a backup
            // whose destination is missing is still the last valid copy,
            // and the sweep below would otherwise delete it as garbage.
            let (restored, preserved) = recover_interrupted_publishes(&media);
            if restored > 0 {
                log::info!("attachments: restored {restored} interrupted publishes");
            }
            // Backups that could not move back stay protected until the
            // next run retries them.
            keep.extend(sweep_keep_set(HashSet::new(), &preserved));
            let held = crate::cache::usage(&media);
            let freed = crate::cache::sweep(&media, &|path| {
                keep.contains(&path.to_string_lossy().into_owned())
            });
            if freed.files > 0 {
                let left = crate::cache::usage(&media);
                log::info!(
                    "attachments: reclaimed {freed} of {held}, {left} left in {}",
                    media.display()
                );
            }
        });
        self.sweep_stickers();
        // Page previews are tiny and keyed by file, but a removed document
        // must not keep its strip on disk forever.
        let thumbs = crate::cache::expire(
            &self.dirs.pdf_thumb_dir(),
            Duration::from_secs(30 * 24 * 3600),
        );
        if thumbs.files > 0 {
            log::info!("pdf previews: reclaimed {thumbs}");
        }
    }

    /// Reclaims sticker previews and phone copies nothing points at anymore.
    ///
    /// Previews are keyed by content hash, so one whose sticker file is gone
    /// (a removed pack, a healed copy, a favourite that moved) is rubbish. A
    /// phone copy the stickers table no longer names is a failed download or
    /// an entry from an older version. Saved stickers and packs are the
    /// user's own files and are never touched here.
    fn sweep_stickers(&self) {
        let dir = self.dirs.sticker_cache_dir();
        let thumbs = self.dirs.sticker_thumb_dir();
        let mut hashed: HashSet<String> = HashSet::new();
        for folder in [
            dir.clone(),
            self.dirs.saved_sticker_dir(),
            self.dirs.media_cache_dir(),
        ] {
            let Ok(entries) = std::fs::read_dir(&folder) else {
                continue;
            };
            for path in entries.flatten().map(|entry| entry.path()) {
                if let Some(id) = crate::stickers::id_of(&path) {
                    hashed.insert(id);
                }
            }
        }
        if let Ok(entries) = std::fs::read_dir(self.packs_dir()) {
            for pack in entries.flatten().map(|entry| entry.path()) {
                let Ok(files) = std::fs::read_dir(&pack) else {
                    continue;
                };
                for path in files.flatten().map(|entry| entry.path()) {
                    if let Some(id) = crate::stickers::id_of(&path) {
                        hashed.insert(id);
                    }
                }
            }
        }
        let thumbs_freed = crate::cache::sweep(&thumbs, &|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| hashed.contains(stem))
        });
        let refs: HashSet<String> = self
            .archive
            .sticker_file_refs()
            .unwrap_or_default()
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let copies_freed = crate::cache::sweep(&dir, &|path| {
            refs.contains(&path.to_string_lossy().into_owned())
        });
        if thumbs_freed.files + copies_freed.files > 0 {
            log::info!(
                "stickers: reclaimed {thumbs_freed} of previews and {copies_freed} of copies in {}",
                dir.display()
            );
        }
    }

    /// Files a freshly downloaded phone sticker under the hash of its bytes.
    ///
    /// The download lands under the hash the phone announced. When the bytes
    /// hash to something else, the file is renamed (or folded into the copy
    /// already there), so the picker never lists one picture twice.
    fn adopt_fetched_sticker(&self, hash: &str, path: &Path) -> PathBuf {
        let Ok(bytes) = std::fs::read(path) else {
            return path.to_path_buf();
        };
        let content = crate::stickers::hash_of(&bytes);
        if content == hash {
            return path.to_path_buf();
        }
        log::warn!("sticker {hash} arrived with unexpected bytes; filing by content");
        let filed = path.with_file_name(format!("{content}.webp"));
        if filed == path {
            return filed;
        }
        if filed.is_file() {
            let _ = std::fs::remove_file(path);
            return filed;
        }
        std::fs::rename(path, &filed).map_or_else(|_| path.to_path_buf(), |()| filed)
    }

    /// Moves chats filed under a privacy id to their phone number, once per start.
    ///
    /// Mappings learned in earlier runs already sit in the archive, but rows
    /// written before they were learned still carry the old id. Healing them
    /// at startup means opening a chat never reads an empty id.
    fn rekey_known_chats(&mut self) {
        let mut moved = 0;
        for (lid, pn) in self.lid_to_pn.clone() {
            match self
                .archive
                .rekey_chat(&format!("{lid}@lid"), &format!("{pn}@s.whatsapp.net"))
            {
                Ok(true) => {
                    self.adopt_rekeyed_chat(&format!("{lid}@lid"), &format!("{pn}@s.whatsapp.net"));
                    moved += 1;
                }
                Ok(false) => {}
                Err(error) => log::warn!("could not re-file chat {pn}: {error}"),
            }
        }
        if moved > 0 {
            log::info!("re-filed {moved} chats under their phone numbers");
            self.emit_chats();
        }
    }
    /// Reads the first page of the most recent chats into the new session.
    ///
    /// Opening a chat then paints saved messages at once instead of waiting
    /// for a page read. Text and attachment references come from the archive
    /// itself; files stay lazy and load when their bubble scrolls into view.
    fn preload_recent(&mut self) {
        /// Recent chats preloaded at startup.
        const PRELOAD_CHATS: usize = 15;
        let chats = match self.archive.chats() {
            Ok(chats) => chats,
            Err(error) => {
                log::warn!("could not preload chats: {error}");
                return;
            }
        };
        let mut filled = 0;
        for chat in chats.iter().take(PRELOAD_CHATS) {
            match self.archive.messages(&chat.id, None, PAGE + 1) {
                Ok(mut messages) => {
                    let complete = messages.len() <= PAGE;
                    if !complete {
                        messages.remove(0);
                    }
                    if messages.is_empty() {
                        continue;
                    }
                    for message in &mut messages {
                        self.polish(message);
                    }
                    filled += 1;
                    self.emit(Event::Messages {
                        chat: chat.id.clone(),
                        messages,
                        older: false,
                        complete,
                    });
                }
                Err(error) => log::warn!("could not preload {}: {error}", chat.id),
            }
        }
        if filled > 0 {
            log::info!("preloaded {filled} recent chats");
        }
    }
    /// Files a sticker copy in the app's own cache under its content hash.
    ///
    /// A sticker the user sent is filed with its message, under a name that
    /// carries the message id. Naming it after the hash of its bytes instead
    /// gives the picture one identity: the picker builds a single preview for
    /// it, however many times it was sent, and two copies of the same
    /// picture are never listed twice.
    fn adopt_sticker_file(&mut self, file: &Path) -> PathBuf {
        let Some(dir) = file.parent() else {
            return file.to_path_buf();
        };
        let filed = match crate::stickers::file_by_hash(dir, file) {
            Ok(filed) => filed,
            Err(error) => {
                log::debug!("could not file {} under its hash: {error}", file.display());
                return file.to_path_buf();
            }
        };
        if filed == file {
            return filed;
        }
        // A favourite that named the old file follows it, so it does not
        // drop out of the picker after the rename.
        let _ = self.archive.rename_sticker_favorite(file, &filed);
        // The archive still points at the old name: it follows the file.
        let Ok(rows) = self.archive.media_paths() else {
            return filed;
        };
        for (chat, id, known) in rows {
            if known == file && self.archive.set_media_path(&chat, &id, &filed).is_ok() {
                self.emit_message(&chat, &id);
            }
        }
        filed
    }
    /// Moves path-based favorites to content hashes once: the same picture
    /// favorited from a pack, the recents, or the saved stickers becomes one
    /// favorite instead of one entry per path. Missing files simply drop.
    fn migrate_sticker_favorites(&mut self) {
        if self.favorites_migrated {
            return;
        }
        self.favorites_migrated = true;
        if self
            .archive
            .meta("sticker_favorites_migrated_v1")
            .ok()
            .flatten()
            .is_some()
        {
            return;
        }
        for path in self.archive.sticker_favorites().unwrap_or_default() {
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let hash = crate::stickers::hash_of(&bytes);
            if self
                .archive
                .favorite_sticker(&hash)
                .ok()
                .flatten()
                .is_some()
            {
                continue;
            }
            let when = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |age| age.as_millis() as i64);
            let _ = self
                .archive
                .set_favorite_sticker(&hash, true, when, None, false);
        }
        let _ = self
            .archive
            .set_meta("sticker_favorites_migrated_v1", "complete");
    }
    /// Favorite hashes, newest first.
    fn favorite_hashes(&self) -> Vec<String> {
        self.archive.favorite_hashes().unwrap_or_default()
    }
    /// Where a favorite hash shows from: the saved copy, a pack member, a
    /// recent file, or the legacy path, whichever still exists. One picture
    /// stays one favorite however many copies exist, and losing one copy
    /// never hides the others.
    fn resolve_favorite(
        &self,
        hash: &str,
        packs: &[crate::model::StickerPack],
        recent: &[PathBuf],
    ) -> Option<PathBuf> {
        let saved = self.dirs.saved_sticker_dir().join(format!("{hash}.webp"));
        if saved.is_file() {
            return Some(saved);
        }
        if let Some(found) = packs
            .iter()
            .flat_map(|pack| &pack.stickers)
            .find(|path| crate::stickers::id_of(path).as_deref() == Some(hash))
            .filter(|path| path.is_file())
        {
            return Some(found.clone());
        }
        if let Some(found) = recent
            .iter()
            .find(|path| crate::stickers::id_of(path).as_deref() == Some(hash))
            .filter(|path| path.is_file())
        {
            return Some(found.clone());
        }
        self.archive
            .sticker_favorites()
            .unwrap_or_default()
            .into_iter()
            .find(|path| {
                path.is_file()
                    && std::fs::read(path)
                        .is_ok_and(|bytes| crate::stickers::hash_of(&bytes) == hash)
            })
    }
    /// Marks a sticker as a favorite, or clears the mark, by content hash.
    /// Files never move: packs keep their members and nothing is deleted.
    fn toggle_favorite_sticker(&mut self, path: &Path) {
        self.migrate_sticker_favorites();
        let Ok(bytes) = std::fs::read(path) else {
            log::warn!("could not favorite a sticker without its file");
            return;
        };
        let hash = crate::stickers::hash_of(&bytes);
        let favorite = !self
            .archive
            .favorite_sticker(&hash)
            .ok()
            .flatten()
            .is_some_and(|known| known.favorite);
        let now = crate::util::now().saturating_mul(1000);
        if let Err(error) = self
            .archive
            .set_favorite_sticker(&hash, favorite, now, None, false)
        {
            log::warn!("could not store the sticker favorite: {error}");
        }
        self.emit_stickers();
        self.push_favorites();
    }
    /// The emojis each sticker file is tagged with, read from its metadata
    /// once per file version.
    fn sticker_emojis(&mut self, paths: &[PathBuf]) -> HashMap<PathBuf, Vec<String>> {
        let mut found = HashMap::new();
        for path in paths {
            let Ok(metadata) = std::fs::metadata(path) else {
                continue;
            };
            let stamp = (metadata.len(), metadata.modified().ok());
            let emojis = match self.emoji_cache.get(path) {
                Some((seen, emojis)) if *seen == stamp => emojis.clone(),
                _ => {
                    let emojis = std::fs::read(path)
                        .map(|bytes| crate::sticker_meta::emojis(&bytes))
                        .unwrap_or_default();
                    self.emoji_cache
                        .insert(path.clone(), (stamp, emojis.clone()));
                    emojis
                }
            };
            if !emojis.is_empty() {
                found.insert(path.clone(), emojis);
            }
        }
        found
    }
    /// Sends every favorite change the phone has not seen, one at a time.
    /// Favorites saved before sync existed, or while offline, count too, so
    /// they reach the phone once after linking.
    fn push_favorites(&mut self) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if self.favorites_pushing {
            self.favorites_again = true;
            return;
        }
        let waiting = match self.archive.unpushed_favorite_stickers() {
            Ok(waiting) => waiting,
            Err(error) => {
                log::warn!("could not list favorite stickers to sync: {error}");
                return;
            }
        };
        if waiting.is_empty() {
            return;
        }
        let pushes: Vec<FavoritePush> = waiting
            .into_iter()
            .map(|(hash, state)| {
                let action = state
                    .action
                    .and_then(|raw| {
                        wa::sync_action_value::StickerAction::decode_from_slice(&raw).ok()
                    })
                    .or_else(|| self.sticker_references(&hash));
                let file = self
                    .resolve_favorite(&hash, &self.sticker_packs(), &[])
                    .filter(|path| path.is_file())
                    .or((!state.favorite)
                        .then(|| self.dirs.saved_sticker_dir().join(format!("{hash}.webp"))));
                FavoritePush {
                    file,
                    hash,
                    favorite: state.favorite,
                    updated_at: state.updated_at,
                    action,
                }
            })
            .collect();
        if pushes.is_empty() {
            return;
        }
        self.favorites_pushing = true;
        let commands = self.commands.clone();
        tokio::spawn(async move {
            for push in pushes {
                let (hash, updated_at) = (push.hash.clone(), push.updated_at);
                let result = Self::push_favorite(&client, push).await;
                let _ = commands.send(Command::FavoritePushed {
                    hash,
                    updated_at,
                    result,
                });
            }
            let _ = commands.send(Command::FavoritesPushed);
        });
    }
    /// Records the phone receipt of a favorite change. A late receipt for
    /// an older change never claims a newer one still waiting.
    fn favorite_pushed(&mut self, hash: &str, updated_at: i64, result: Result<Vec<u8>, String>) {
        match result {
            Ok(action) => {
                let _ = self
                    .archive
                    .favorite_sticker_pushed(hash, updated_at, Some(&action));
            }
            Err(error) => log::warn!("could not sync a favorite sticker: {error}"),
        }
    }
    /// CDN references for a sticker seen in a chat, so a favorite need not
    /// be uploaded again.
    fn sticker_references(&self, hash: &str) -> Option<wa::sync_action_value::StickerAction> {
        if let Ok(phone) = self.archive.phone_stickers()
            && let Some(sticker) = phone.into_iter().find(|sticker| sticker.hash == hash)
            && let Ok(meta) = wa::StickerMetadata::decode_from_slice(&sticker.raw)
            && meta.direct_path.is_some()
        {
            return Some(Self::action_of_metadata(&meta));
        }
        self.archive
            .sticker_message_raws(2000)
            .ok()?
            .into_iter()
            .filter_map(|raw| wa::Message::decode_from_slice(&raw).ok())
            .find_map(|message| {
                let sticker = message.get_base_message().sticker_message.as_option()?;
                let matches = sticker_hash(
                    sticker.file_sha256.as_deref(),
                    sticker.file_enc_sha256.as_deref(),
                )
                .as_deref()
                    == Some(hash);
                (matches && sticker.direct_path.is_some()).then(|| Self::action_of_message(sticker))
            })
    }
    /// Applies a favorite added or removed on the phone side: the phone
    /// change wins unless a change made here is still on its way to the
    /// phone and is newer. Ready for phone events and replayed syncs.
    /// No caller yet: the library does not deliver phone favorite events in
    /// this revision, so this waits for that support with its tests green.
    #[allow(dead_code)]
    fn apply_phone_favorite(
        &mut self,
        hash: &str,
        favorite: bool,
        stamped: i64,
        action: Option<Vec<u8>>,
    ) {
        if let Ok(Some(known)) = self.archive.favorite_sticker(hash)
            && !known.pushed
            && known.updated_at > stamped
        {
            log::info!("kept a newer favorite sticker change made here");
            return;
        }
        let at = if stamped > 0 {
            stamped
        } else {
            crate::util::now().saturating_mul(1000)
        };
        if let Err(error) =
            self.archive
                .set_favorite_sticker(hash, favorite, at, action.as_deref(), true)
        {
            log::warn!("could not record a favorite sticker: {error}");
        }
        self.emit_stickers();
        if favorite {
            self.fetch_favorite(hash.to_owned());
        }
    }
    /// Brings a favorite file in: from a copy of the same sticker already
    /// here, otherwise from the CDN. Missing references wait for the next
    /// connection instead of failing loudly.
    fn fetch_favorite(&mut self, hash: String) {
        let dir = self.dirs.saved_sticker_dir();
        let path = dir.join(format!("{hash}.webp"));
        if path.exists() || self.favorite_fetches.contains(&hash) {
            return;
        }
        if let Some(source) = self.local_sticker_copy(&hash) {
            match std::fs::read(&source) {
                Ok(bytes) if crate::stickers::hash_of(&bytes) == hash => {
                    if std::fs::write(&path, &bytes).is_ok() {
                        log::info!("favorite sticker copied from a local copy");
                        self.emit_stickers();
                        return;
                    }
                }
                Ok(_) => log::warn!("a favorite sticker copy did not match its hash"),
                Err(error) => log::warn!("could not copy a favorite sticker: {error}"),
            }
        }
        let Some(file_sha256) = crate::stickers::filehash_bytes(&hash) else {
            return;
        };
        let stored = self
            .archive
            .favorite_sticker(&hash)
            .ok()
            .flatten()
            .and_then(|known| known.action)
            .and_then(|raw| wa::sync_action_value::StickerAction::decode_from_slice(&raw).ok());
        let candidates: Vec<FavoriteDownload> = stored
            .into_iter()
            .chain(self.sticker_references(&hash))
            .filter(Self::fetchable)
            .map(|action| FavoriteDownload {
                action,
                file_sha256: file_sha256.clone(),
            })
            .collect();
        if candidates.is_empty() {
            log::warn!(
                "could not fetch a favorite sticker: no download references and no local copy"
            );
            return;
        }
        let Some(client) = self.client.clone() else {
            log::info!("a favorite sticker will be fetched once connected");
            return;
        };
        self.favorite_fetches.insert(hash.clone());
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let mut result = Err("no download references".to_owned());
            for download in &candidates {
                result = Self::download_favorite(&client, download, &dir, &path).await;
                if result.is_ok() {
                    break;
                }
            }
            let _ = commands.send(Command::FavoriteFetched { hash, result });
        });
    }
    /// Keeps a fetched favorite only when it is the sticker the phone named.
    fn favorite_fetched(&mut self, hash: &str, result: Result<PathBuf, String>) {
        self.favorite_fetches.remove(hash);
        match result {
            Ok(path) => {
                let matches = std::fs::read(&path)
                    .is_ok_and(|bytes| crate::stickers::hash_of(&bytes) == hash);
                if matches {
                    log::info!("favorite sticker fetched");
                } else {
                    log::warn!("a favorite sticker did not match its hash");
                    let _ = std::fs::remove_file(&path);
                }
                self.emit_stickers();
            }
            Err(error) => log::warn!(
                "could not fetch a favorite sticker; retrying on the next connection: {error}"
            ),
        }
    }
    /// Fetches phone favorites whose files never arrived, such as one whose
    /// download failed or that came while offline.
    fn fetch_missing_favorites(&mut self) {
        let dir = self.dirs.saved_sticker_dir();
        let missing: Vec<String> = match self.archive.favorite_hashes() {
            Ok(hashes) => hashes
                .into_iter()
                .filter(|hash| !dir.join(format!("{hash}.webp")).exists())
                .collect(),
            Err(error) => {
                log::warn!("could not list favorite stickers to fetch: {error}");
                return;
            }
        };
        if !missing.is_empty() {
            log::info!("fetching {} favorite stickers", missing.len());
        }
        for hash in missing {
            self.fetch_favorite(hash);
        }
    }
    /// A file here holding the sticker with this content hash: the phone
    /// recents, chat stickers, or a pack. Bytes always decide.
    fn local_sticker_copy(&self, hash: &str) -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(phone) = self.archive.phone_stickers() {
            candidates.extend(
                phone
                    .into_iter()
                    .filter(|sticker| sticker.hash == hash)
                    .filter_map(|sticker| sticker.path),
            );
        }
        candidates.extend(
            self.sticker_packs()
                .into_iter()
                .flat_map(|pack| pack.stickers)
                .filter(|path| {
                    path.file_stem()
                        .is_some_and(|stem| stem.to_string_lossy() == hash)
                }),
        );
        candidates.into_iter().find(|path| {
            std::fs::read(path).is_ok_and(|bytes| crate::stickers::hash_of(&bytes) == hash)
        })
    }
    /// Downloads a pack shared in a chat into the cache and shows it. A pack
    /// opened before shows again without downloading.
    fn view_sticker_pack(&mut self, chat: &str, message: &str) {
        let pack = self
            .archive
            .raw(chat, message)
            .ok()
            .flatten()
            .and_then(|raw| wa::Message::decode_from_slice(&raw).ok())
            .and_then(|message| {
                message
                    .get_base_message()
                    .sticker_pack_message
                    .as_option()
                    .cloned()
            });
        let Some(pack) = pack else {
            self.emit(Event::StickerPackPreview(Err(
                "This sticker pack is no longer available".to_owned(),
            )));
            return;
        };
        let name = pack.name.clone().unwrap_or_default();
        let publisher = pack.publisher.clone().unwrap_or_default();
        let id = pack
            .sticker_pack_id
            .clone()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| message.to_owned());
        let dir = self
            .dirs
            .sticker_cache_dir()
            .join("shared")
            .join(sanitize(&id));
        if let Some(cached) = self
            .sticker_packs()
            .into_iter()
            .find(|listed| listed.dir == dir)
        {
            self.emit(Event::StickerPackPreview(Ok((cached, publisher))));
            return;
        }
        let Some(client) = self.client.clone() else {
            self.emit(Event::StickerPackPreview(Err(
                "Connect to WhatsApp to open this sticker pack".to_owned(),
            )));
            return;
        };
        if pack.file_length.unwrap_or(0) > 64 * 1024 * 1024 {
            self.emit(Event::StickerPackPreview(Err(
                "This sticker pack is too large to open".to_owned(),
            )));
            return;
        }
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let result = Self::download_shared_pack(&client, &pack, &name, &publisher, &dir).await;
            let _ = commands.send(Command::StickerPackViewed { result });
        });
    }
    /// Copies a viewed pack into the packs folder, under its own name.
    fn add_sticker_pack(&mut self, dir: &Path, name: &str) {
        if !dir.starts_with(self.dirs.sticker_cache_dir()) {
            return;
        }
        match super::sticker_import::copy_pack(dir, &self.packs_dir(), name) {
            Ok(name) => {
                self.emit_stickers();
                self.emit(Event::Info(format!("Added sticker pack \"{name}\"")));
            }
            Err(error) => self.emit(Event::Error(format!("Could not add sticker pack: {error}"))),
        }
    }
    /// References from a sticker message, as a favorite action carries them.
    fn action_of_message(
        sticker: &wa::message::StickerMessage,
    ) -> wa::sync_action_value::StickerAction {
        wa::sync_action_value::StickerAction {
            url: sticker.url.clone(),
            file_enc_sha256: sticker.file_enc_sha256.clone(),
            media_key: sticker.media_key.clone(),
            mimetype: sticker.mimetype.clone(),
            height: sticker.height,
            width: sticker.width,
            direct_path: sticker.direct_path.clone(),
            file_length: sticker.file_length,
            is_lottie: sticker.is_lottie,
            is_avatar_sticker: sticker.is_avatar,
            ..Default::default()
        }
    }
    /// References from the phone recent-sticker list.
    fn action_of_metadata(sticker: &wa::StickerMetadata) -> wa::sync_action_value::StickerAction {
        wa::sync_action_value::StickerAction {
            url: sticker.url.clone(),
            file_enc_sha256: sticker.file_enc_sha256.clone(),
            media_key: sticker.media_key.clone(),
            mimetype: sticker.mimetype.clone(),
            height: sticker.height,
            width: sticker.width,
            direct_path: sticker.direct_path.clone(),
            file_length: sticker.file_length,
            is_lottie: sticker.is_lottie,
            is_avatar_sticker: sticker.is_avatar_sticker,
            ..Default::default()
        }
    }
    /// Whether an action carries enough to fetch the sticker from the CDN.
    fn fetchable(action: &wa::sync_action_value::StickerAction) -> bool {
        action
            .direct_path
            .as_deref()
            .is_some_and(|path| !path.is_empty())
            || (action.media_key.is_none()
                && action.url.as_deref().is_some_and(|url| !url.is_empty()))
    }
    /// A download error without the CDN paths and tokens it may quote.
    fn redacted(error: &str) -> String {
        error
            .split_whitespace()
            .map(|word| {
                if word.contains("://") || word.contains("/v/") || word.contains("oh=") {
                    "<link>"
                } else {
                    word
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
    /// Tells the phone about one favorite change, uploading the sticker first
    /// when the phone could not fetch it otherwise. Returns the references sent.
    async fn push_favorite(client: &Client, push: FavoritePush) -> Result<Vec<u8>, String> {
        let filehash = crate::stickers::filehash_of_hash(&push.hash).ok_or("not a sticker hash")?;
        let mut action = push.action.unwrap_or_default();
        if push.favorite && action.direct_path.is_none() {
            let Some(path) = push.file else {
                return Err("no file to upload for a new favorite".to_owned());
            };
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|error| error.to_string())?;
            if crate::stickers::hash_of(&bytes) != push.hash {
                return Err("the favorite file changed under its hash".to_owned());
            }
            let size = image::ImageReader::new(std::io::Cursor::new(&bytes))
                .with_guessed_format()
                .ok()
                .and_then(|reader| reader.into_dimensions().ok());
            let upload = client
                .upload(bytes, MediaType::Sticker, UploadOptions::default())
                .await
                .map_err(|error| error.to_string())?;
            action = wa::sync_action_value::StickerAction {
                url: Some(upload.url),
                file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
                media_key: Some(upload.media_key.to_vec()),
                mimetype: Some("image/webp".to_owned()),
                width: size.map(|(width, _)| width),
                height: size.map(|(_, height)| height),
                direct_path: Some(upload.direct_path),
                file_length: Some(upload.file_length),
                ..Default::default()
            };
        }
        action.is_favorite = Some(push.favorite);
        let encoded = action.encode_to_vec();
        let value = wa::SyncActionValue {
            sticker_action: MessageField::some(action),
            timestamp: Some(push.updated_at),
            ..Default::default()
        };
        client
            .send_app_state_action(&schemas::FAVORITE_STICKER, &[&filehash], &value)
            .await
            .map_err(|error| error.to_string())?;
        Ok(encoded)
    }
    /// Downloads one favorite into place, proving the bytes the phone named.
    async fn download_favorite(
        client: &Client,
        download: &FavoriteDownload,
        dir: &Path,
        path: &Path,
    ) -> Result<PathBuf, String> {
        let bytes = client
            .download(download)
            .await
            .map_err(|error| Self::redacted(&error.to_string()))?;
        if crate::stickers::hash_of(&bytes)
            != path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default()
        {
            return Err("a favorite sticker did not match its hash".to_owned());
        }
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|error| error.to_string())?;
        let staging = path.with_extension("part");
        tokio::fs::write(&staging, &bytes)
            .await
            .map_err(|error| error.to_string())?;
        tokio::fs::rename(&staging, path)
            .await
            .map_err(|error| error.to_string())?;
        Ok(path.to_path_buf())
    }

    /// Downloads a shared pack zip and unpacks it into the folder, stamping the listed emoji tags. Returns the pack and its publisher.
    async fn download_shared_pack(
        client: &Client,
        pack: &wa::message::StickerPackMessage,
        name: &str,
        publisher: &str,
        dir: &Path,
    ) -> Result<(crate::model::StickerPack, String), String> {
        let zip = client
            .download(&PackDownload {
                message: pack.clone(),
            })
            .await
            .map_err(|error| error.to_string())?;
        let stickers: Vec<(String, Vec<String>)> = pack
            .stickers
            .iter()
            .filter_map(|sticker| Some((sticker.file_name.clone()?, sticker.emojis.clone())))
            .collect();
        let tray = pack.tray_icon_file_name.clone();
        let files = super::sticker_import::extract_whatsapp_pack(
            &zip,
            &stickers,
            tray.as_deref(),
            name,
            dir,
        )
        .map_err(|error| error.to_string())?;
        Ok((
            crate::model::StickerPack {
                name: name.to_owned(),
                dir: dir.to_path_buf(),
                stickers: files,
            },
            publisher.to_owned(),
        ))
    }
    /// Returns distinct downloaded stickers by most recent use.
    fn emit_stickers(&mut self) {
        // Saved stickers from older versions carry plain file names, which
        // have no thumbnail and no shared identity. Filing them under their
        // content hash gives every copy one name, one preview, and one entry.
        for (before, after) in crate::stickers::adopt_dir(&self.dirs.saved_sticker_dir()) {
            let _ = self.archive.rename_sticker_favorite(&before, &after);
        }
        self.migrate_sticker_favorites();
        let saved = self.saved_stickers();
        let fav_hashes = self.favorite_hashes();
        // One picture, one place: a sticker already saved or favorited is not
        // offered again in a pack or under recents. The key is the content
        // hash, so every origin agrees on one identity per picture.
        let mut shown: HashSet<String> = saved
            .iter()
            .filter_map(|path| crate::stickers::id_of(path))
            .chain(fav_hashes.iter().cloned())
            .collect();
        let mut packs = Vec::new();
        for pack in self.sticker_packs() {
            let stickers: Vec<PathBuf> = pack
                .stickers
                .into_iter()
                .filter(|path| match crate::stickers::id_of(path) {
                    Some(id) => shown.insert(id),
                    None => true,
                })
                .collect();
            if !stickers.is_empty() {
                packs.push(crate::model::StickerPack { stickers, ..pack });
            }
        }
        let mut list: Vec<(i64, PathBuf)> = Vec::new();
        if let Ok(phone) = self.archive.phone_stickers() {
            for sticker in phone {
                if let Some(path) = sticker.path
                    && path.exists()
                {
                    // The file on disk wins over the stored hash: it is the
                    // same picture the packs and the saved stickers are keyed
                    // by, so all three lists finally agree.
                    let key = crate::stickers::id_of(&path).unwrap_or(sticker.hash);
                    if shown.insert(key) {
                        list.push((sticker.last_used, path));
                    }
                }
            }
        }
        match self.archive.recent_stickers(80) {
            Ok(rows) => {
                for sticker in rows {
                    let before = sticker.path.clone();
                    let path = self.adopt_sticker_file(&before);
                    // The adopted file carries the content hash in its name, so
                    // prefer it: our own sends have no raw message to read a
                    // hash from, and the stored one may be missing as well.
                    let key = crate::stickers::id_of(&path).or_else(|| {
                        sticker.raw.as_deref().and_then(|raw| {
                            wa::Message::decode_from_slice(raw)
                                .ok()
                                .and_then(|message| {
                                    let base = message.get_base_message();
                                    let sticker = base.sticker_message.as_option()?;
                                    sticker_hash(
                                        sticker.file_sha256.as_deref(),
                                        sticker.file_enc_sha256.as_deref(),
                                    )
                                })
                        })
                    });
                    // A row with no hash to read cannot be told apart from
                    // another, so its path stands in for it.
                    let key = key.unwrap_or_else(|| path.display().to_string());
                    if shown.insert(key) {
                        list.push((sticker.last_used, path));
                    }
                }
            }
            Err(error) => log::warn!("could not list stickers: {error}"),
        }
        list.sort_by_key(|(when, _)| std::cmp::Reverse(*when));
        let recent: Vec<PathBuf> = list.into_iter().map(|(_, path)| path).collect();
        let favorites: Vec<PathBuf> = fav_hashes
            .iter()
            .filter_map(|hash| self.resolve_favorite(hash, &self.sticker_packs(), &recent))
            .collect();
        self.build_missing_thumbs(&saved, &packs, &recent, &favorites);
        let listed: Vec<PathBuf> = saved
            .iter()
            .chain(packs.iter().flat_map(|pack| &pack.stickers))
            .chain(recent.iter())
            .chain(favorites.iter())
            .cloned()
            .collect();
        let emojis = self.sticker_emojis(&listed);
        self.emit(Event::Stickers {
            saved,
            packs,
            recent,
            favorites,
            emojis: emojis.into_iter().collect(),
        });
    }

    /// Builds the small previews the picker draws, off the interface thread.
    ///
    /// A sticker is a 512 px WebP, often animated, and decoding one per tile
    /// is what made the grid crawl. The preview is a 128 px static PNG, so
    /// the grid only ever decodes a handful of bytes per tile.
    fn build_missing_thumbs(
        &mut self,
        saved: &[PathBuf],
        packs: &[crate::model::StickerPack],
        recent: &[PathBuf],
        favorites: &[PathBuf],
    ) {
        let thumbs = self.dirs.sticker_thumb_dir();
        let files: Vec<PathBuf> = saved
            .iter()
            .cloned()
            .chain(packs.iter().flat_map(|pack| pack.stickers.iter().cloned()))
            .chain(recent.iter().cloned())
            .chain(favorites.iter().cloned())
            .filter(|path| {
                // Paths with an explicit heal request are rebuilt by the heal
                // pump, which reports each result, instead of this batch.
                !self.thumb_heals.contains_key(path)
                    && crate::stickers::thumb_path(&thumbs, path)
                        .is_some_and(|thumb| !thumb.is_file())
                    && self.thumb_tries.get(path).copied().unwrap_or(0) < 2
            })
            .collect();
        if files.is_empty() {
            return;
        }
        for file in &files {
            *self.thumb_tries.entry(file.clone()).or_insert(0) += 1;
        }
        let commands = self.commands.clone();
        tokio::task::spawn_blocking(move || {
            let mut built = 0;
            for file in files {
                if crate::stickers::build_thumb(&thumbs, &file).is_ok() {
                    built += 1;
                }
            }
            if built > 0 {
                let _ = commands.send(Command::StickerThumbsReady);
            }
        });
    }

    /// Collects everything the info dialog shows about one attachment.
    fn file_info(&self, chat: &str, id: &str) -> Result<crate::model::FileInfo, String> {
        let row = self
            .archive
            .message(chat, id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "That message is not in the archive".to_owned())?;
        let media = row
            .content
            .media()
            .ok_or_else(|| "That message carries no file".to_owned())?;
        let name = match &row.content {
            Content::Document { file_name, .. } => file_name.clone(),
            other => other.summary(),
        };
        let kind = crate::model::FileKind::of(&media.mime, &name);
        let mut rows = vec![
            ("Type".to_owned(), kind.label().to_owned()),
            (
                "MIME".to_owned(),
                if media.mime.is_empty() {
                    "unknown".to_owned()
                } else {
                    media.mime.clone()
                },
            ),
            ("Size".to_owned(), crate::util::bytes(media.size)),
        ];
        if let (Some(width), Some(height)) = (media.width, media.height) {
            rows.push(("Pixels".to_owned(), format!("{width} x {height}")));
        }
        if let Content::Video { seconds, .. } | Content::Audio { seconds, .. } = &row.content
            && let Some(seconds) = seconds
        {
            rows.push(("Length".to_owned(), crate::util::duration(*seconds)));
        }
        if let Content::Document {
            pages: Some(pages), ..
        } = &row.content
            && *pages > 0
        {
            rows.push(("Pages".to_owned(), pages.to_string()));
        }
        rows.push(("Sent".to_owned(), crate::util::moment_stamp(row.timestamp)));
        rows.push(("Message".to_owned(), id.to_owned()));
        if let Some(known) = self.archive.chat(chat).ok().flatten() {
            rows.push(("Chat".to_owned(), known.name));
        }
        match media.path.as_ref().filter(|path| path.is_file()) {
            Some(path) => {
                rows.push(("File".to_owned(), path.display().to_string()));
                if let Ok(metadata) = path.metadata() {
                    rows.push(("On disk".to_owned(), crate::util::bytes(metadata.len())));
                }
                if let Ok(hash) = sha256_of(path) {
                    rows.push(("SHA-256".to_owned(), hash));
                }
            }
            None => rows.push(("File".to_owned(), "Not downloaded yet".to_owned())),
        }
        Ok(crate::model::FileInfo {
            title: name,
            rows,
            note: kind.runs_code().then(|| {
                "This is a program. Save it and check where it came from before running it."
                    .to_owned()
            }),
        })
    }

    /// Root directory for imported sticker packs.
    fn packs_dir(&self) -> PathBuf {
        self.dirs.saved_sticker_dir().join("packs")
    }

    /// Returns imported packs, newest first, with files in pack order.
    ///
    /// An older folder is brought up to date on the way through: files are
    /// filed under their content hash and a manifest records the order.
    fn sticker_packs(&self) -> Vec<crate::model::StickerPack> {
        let Ok(entries) = std::fs::read_dir(self.packs_dir()) else {
            return Vec::new();
        };
        let mut packs: Vec<(std::time::SystemTime, crate::model::StickerPack)> = entries
            .flatten()
            .filter_map(|entry| {
                let dir = entry.path();
                if !dir.is_dir() {
                    return None;
                }
                let fallback = entry.file_name().to_string_lossy().into_owned();
                let (name, stickers) =
                    crate::stickers::adopt_pack(&dir, &fallback).unwrap_or((fallback, Vec::new()));
                if stickers.is_empty() {
                    return None;
                }
                let when = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                Some((
                    when,
                    crate::model::StickerPack {
                        name,
                        dir,
                        stickers,
                    },
                ))
            })
            .collect();
        packs.sort_by_key(|(when, _)| std::cmp::Reverse(*when));
        packs.into_iter().map(|(_, pack)| pack).collect()
    }

    /// Returns saved sticker files, newest first.
    fn saved_stickers(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.dirs.saved_sticker_dir()) else {
            return Vec::new();
        };
        let mut saved: Vec<(std::time::SystemTime, PathBuf)> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                path.extension()
                    .is_some_and(|extension| extension == "webp")
                    .then(|| {
                        let when = entry
                            .metadata()
                            .and_then(|metadata| metadata.modified())
                            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                        (when, path)
                    })
            })
            .collect();
        saved.sort_by_key(|(when, _)| std::cmp::Reverse(*when));
        saved.into_iter().map(|(_, path)| path).collect()
    }

    /// Saves a sticker under its content hash to deduplicate copies.
    fn save_sticker(&self, path: &Path) -> Result<(), String> {
        use sha2::{Digest, Sha256};
        let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
        let hash: String = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let dir = self.dirs.saved_sticker_dir();
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let target = dir.join(format!("{hash}.webp"));
        if !target.exists() {
            std::fs::write(&target, &bytes).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn ureq_avatar_fetch() -> AvatarFetch {
        std::sync::Arc::new(|url: &str| {
            ureq::get(url)
                .call()
                .and_then(|mut response| response.body_mut().read_to_vec())
                .map_err(|error| error.to_string())
        })
    }

    /// Downloads a picture URL through the given transport, validates the
    /// bytes, and stores them atomically. A refused reference, invalid
    /// image, or transport failure is an error: the caller retries later
    /// and keeps showing the last good photo meanwhile. Absence is never
    /// synthesized here.
    async fn download_and_store_avatar(
        fetch: &AvatarFetch,
        url: String,
        path: PathBuf,
    ) -> Result<PathBuf, String> {
        if !is_fetchable_avatar_url(&url) {
            return Err("refusing non-absolute avatar reference".to_owned());
        }
        let bytes = tokio::task::spawn_blocking({
            let fetch = fetch.clone();
            move || fetch(&url)
        })
        .await
        .map_err(|error| error.to_string())??;
        if !validate_avatar_bytes(&bytes) {
            return Err("downloaded avatar is not a readable image".to_owned());
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| error.to_string())?;
        }
        // Written through a temporary file so a reader never sees a half one.
        let staging = path.with_extension("part");
        tokio::fs::write(&staging, &bytes)
            .await
            .map_err(|error| error.to_string())?;
        tokio::fs::rename(&staging, &path)
            .await
            .map_err(|error| error.to_string())?;
        Ok(path)
    }

    fn avatar_file(&self, id: &str, full: bool) -> PathBuf {
        self.dirs.avatar_file(id, full)
    }

    fn fetch_avatar(&mut self, id: String, full: bool) {
        let path = self.avatar_file(&id, full);
        if let Ok(metadata) = std::fs::metadata(&path)
            && metadata
                .modified()
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| {
                    age < if metadata.len() > 0 {
                        AVATAR_FRESH
                    } else {
                        AVATAR_MISS_FRESH
                    }
                })
        {
            let path = (metadata.len() > 0).then_some(path);
            self.pending_avatars.remove(&(id.clone(), full));
            self.emit(Event::Avatar { id, full, path });
            return;
        }
        // Try both of our ids for our profile picture.
        let candidates: Vec<Jid> = if self.is_me(&id) || id == self.me() {
            [self.me_pn.clone(), self.me_lid.clone()]
                .into_iter()
                .flatten()
                .filter_map(|id| Self::jid_of(&id))
                .collect()
        } else {
            let mut candidates: Vec<Jid> = Self::jid_of(&id).into_iter().collect();
            if let Some(phone) = crate::model::phone_of(&id)
                && let Some(lid) = self
                    .lid_to_pn
                    .iter()
                    .find_map(|(lid, pn)| (pn == phone).then_some(lid))
                && let Some(jid) = Self::jid_of(&format!("{lid}@lid"))
                && !candidates.contains(&jid)
            {
                candidates.push(jid);
            }
            candidates
        };
        if candidates.is_empty() {
            self.pending_avatars.remove(&(id.clone(), full));
            self.emit(Event::Avatar {
                id,
                full,
                path: None,
            });
            return;
        }
        let connected = self
            .client
            .as_ref()
            .is_some_and(|client| client.is_connected());
        let Some(client) = self.client.clone().filter(|_| connected) else {
            // Defer profile-picture lookup until connected, keeping the
            // failure count and deadline; only the in-flight flag resets.
            self.pending_avatars
                .entry((id.clone(), full))
                .or_default()
                .in_flight = false;
            return;
        };
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let fetched = async {
                // Channels use the same contacts picture lookup: the library
                // answers it for newsletter JIDs, while the newsletter
                // metadata picture fields are CDN direct paths, not fetchable
                // URLs, so they must never reach a plain HTTP client.
                // Resolving a direct path would need media-host auth the app
                // does not negotiate; that stays a registered limitation.
                let mut picture = None;
                let mut failed = false;
                'lookup: for jid in &candidates {
                    for preview in [!full, false] {
                        match client.contacts().get_profile_picture(jid, preview).await {
                            Ok(Some(found)) => {
                                picture = Some(found);
                                break 'lookup;
                            }
                            Ok(None) => {}
                            Err(error) => {
                                log::debug!("picture lookup failed: {error}");
                                failed = true;
                            }
                        }
                    }
                }
                let Some(picture) = picture else {
                    return if failed {
                        Err("lookup failed".to_owned())
                    } else {
                        Ok(None)
                    };
                };
                // Validation, atomic store, and failure-as-error live in
                // one place: only a readable image marks the cache ready.
                let path =
                    Self::download_and_store_avatar(&Self::ureq_avatar_fetch(), picture.url, path)
                        .await?;
                Ok::<Option<PathBuf>, String>(Some(path))
            }
            .await;
            match fetched {
                Ok(path) => {
                    let _ = commands.send(Command::AvatarFetched { id, full, path });
                }
                Err(error) => {
                    log::debug!("no picture for {id} yet: {error}");
                    let _ = commands.send(Command::AvatarFailed { id, full });
                }
            }
        });
    }

    /// Retries deferred or failed profile-picture requests. Entries keep
    /// their failure count and deadline across ticks: only success,
    /// reported absence, or the failure cap removes them.
    fn retry_avatars(&mut self) {
        if !self
            .client
            .as_ref()
            .is_some_and(|client| client.is_connected())
        {
            return;
        }
        let now = Instant::now();
        let due: Vec<(String, bool)> = self.pending_avatars.keys().cloned().collect();
        for (id, full) in due {
            let Some(retry) = self.pending_avatars.get_mut(&(id.clone(), full)) else {
                continue;
            };
            match avatar_due(retry, now) {
                AvatarDue::Wait => continue,
                AvatarDue::GiveUp => {
                    self.give_up_avatar(id, full);
                    continue;
                }
                AvatarDue::Dispatch => {}
            }
            retry.in_flight = true;
            self.fetch_avatar(id, full);
        }
    }

    /// Reports a profile picture the worker stopped retrying: the cached
    /// photo when one is still on disk, or absence when there is nothing
    /// valid to show. Only success, proven absence, or an explicit removal
    /// clears a photo; exhausting retries never wipes the last good one.
    fn give_up_avatar(&mut self, id: String, full: bool) {
        self.pending_avatars.remove(&(id.clone(), full));
        let path = self.avatar_file(&id, full);
        let keep = std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 0);
        self.emit(Event::Avatar {
            id,
            full,
            path: keep.then_some(path),
        });
    }

    /// Searches visible archived message text on a dedicated read
    /// connection, off the serial loop: the loop keeps consuming
    /// messages, receipts, syncs and timers while the query runs.
    /// Only the newest query applies; older answers die by generation.
    /// The ceiling counts running blocking tasks, not stored answers:
    /// at most two queries execute at once, a third waits coalesced,
    /// and the generation gate drops every stale delivery.
    fn spawn_search(&mut self, chat: Option<ChatId>, query: String, limit: usize) {
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;
        if self.search_in_flight >= MAX_SEARCH_IN_FLIGHT {
            // Coalesce: only the newest waiting query survives; its
            // generation already outranks everything in flight.
            self.search_pending = Some((chat, query, limit));
            return;
        }
        self.launch_search(chat, query, limit, generation);
    }

    /// Starts one background search task for an already-numbered
    /// generation. Callers bumped the generation; pending relaunches reuse
    /// the newest number instead of bumping again.
    fn launch_search(
        &mut self,
        chat: Option<ChatId>,
        query: String,
        limit: usize,
        generation: u64,
    ) {
        self.search_in_flight = self.search_in_flight.saturating_add(1);
        let commands = self.commands.clone();
        let path = self.dirs.archive_db();
        tokio::task::spawn_blocking(move || {
            let hits = Self::search_archive(&path, chat.as_deref(), &query, limit);
            let _ = commands.send(Command::SearchReady {
                generation,
                query,
                chat,
                hits,
            });
        });
    }

    /// Applies one finished background search, unless a newer query already
    /// replaced it. Sender names resolve here, on the loop, exactly like the
    /// serial path did for global search; chat search never polished.
    fn apply_search(
        &mut self,
        generation: u64,
        query: String,
        chat: Option<ChatId>,
        hits: Result<Vec<Message>, String>,
    ) {
        // One background task finished: free its slot, then run the newest
        // waiting query if any. Stale answers also free a slot, so the
        // pending newest still launches even when an older task lands last.
        self.search_in_flight = self.search_in_flight.saturating_sub(1);
        if let Some((pending_chat, pending_query, pending_limit)) = self.search_pending.take() {
            let pending_generation = self.search_generation;
            self.launch_search(
                pending_chat,
                pending_query,
                pending_limit,
                pending_generation,
            );
        }
        if generation != self.search_generation {
            return;
        }
        match chat {
            None => match hits {
                Ok(mut messages) => {
                    // A message removed while the query flew must not
                    // repaint the panel: tombstones, clear barriers and
                    // missing rows all win over stale hits.
                    messages.retain(|message| self.search_hit_alive(message));
                    for message in &mut messages {
                        self.polish(message);
                    }
                    self.emit(Event::SearchHits { query, messages });
                }
                Err(error) => self.emit(Event::Error(format!("Could not search: {error}"))),
            },
            Some(chat) => {
                let hits = hits.map(|mut messages| {
                    messages.retain(|message| self.search_hit_alive(message));
                    messages
                });
                self.emit(Event::ChatSearch { chat, query, hits })
            }
        }
    }

    /// Whether one search hit may still paint: dropped when its message
    /// was tombstoned, cleared below the removal barrier, or deleted from
    /// the archive after the database query ran but before this answer
    /// applied. Database errors keep the hit rather than hiding a live
    /// message over a transient failure.
    fn search_hit_alive(&self, message: &Message) -> bool {
        if self
            .archive
            .is_tombstoned(&message.chat, &message.id)
            .unwrap_or(false)
        {
            return false;
        }
        if let Ok(Some(through)) = self.archive.removal_point(&message.chat)
            && message.timestamp <= through
        {
            return false;
        }
        match self.archive.message(&message.chat, &message.id) {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(_) => true,
        }
    }

    /// Runs one search on a dedicated read connection, off the serial loop.
    /// Same database, same keyring configuration, same fields, escaping,
    /// ordering and limits as the serial path it replaces.
    fn search_archive(
        path: &std::path::Path,
        chat: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Message>, String> {
        let archive = Archive::open(path).map_err(|error| format!("{error:#}"))?;
        match chat {
            Some(chat) => archive.search_messages_in(Some(chat), query, limit),
            None => archive.search_messages(query, limit),
        }
        .map_err(|error| error.to_string())
    }

    /// Loads archived messages needed to scroll to a quote.
    fn load_until(&mut self, chat: ChatId, id: String, before: super::PageKey) {
        let Ok(Some(target)) = self.archive.message(&chat, &id) else {
            self.emit(Event::Messages {
                chat: chat.clone(),
                messages: Vec::new(),
                older: true,
                complete: false,
            });
            self.emit(Event::Error(
                "This message is not stored on this computer".to_owned(),
            ));
            return;
        };
        match self
            .archive
            .messages_range(&chat, target.timestamp, (before.0, &before.1), 2000)
        {
            Ok(mut messages) => {
                for message in &mut messages {
                    self.polish(message);
                }
                self.emit(Event::Messages {
                    chat,
                    messages,
                    older: true,
                    complete: false,
                });
            }
            Err(error) => self.emit(Event::Error(format!("Could not read the chat: {error}"))),
        }
    }

    fn edit_text(&mut self, chat: ChatId, id: String, text: String, mentions: Vec<String>) {
        let (Some(client), Some(jid)) = (self.client.clone(), Self::jid_of(&chat)) else {
            self.emit(Event::Error("Not connected to WhatsApp".to_owned()));
            return;
        };
        let content = Content::text(text.clone());
        let mention_rows = self.mentions_of(&mentions);
        if let Ok(true) = self
            .archive
            .set_edited_text(&chat, &id, &content, &mention_rows)
        {
            self.emit_message(&chat, &id);
            self.emit_chat(&chat);
        }
        let mut message = outgoing_text(text, None, &mentions);
        self.apply_ephemeral(&chat, &mut message);
        let commands = self.commands.clone();
        tokio::spawn(async move {
            if let Err(error) = client.edit_message(jid, id.clone(), message).await {
                let _ = commands.send(Command::Sent {
                    chat,
                    id: String::new(),
                    error: Some(format!("Could not send the edit: {error}")),
                });
            }
        });
    }

    fn revoke(&mut self, chat: ChatId, id: String) {
        let (Some(client), Some(jid)) = (self.client.clone(), Self::jid_of(&chat)) else {
            self.emit(Event::Error("Not connected to WhatsApp".to_owned()));
            return;
        };
        if let Ok(true) = self
            .archive
            .set_content(&chat, &id, &Content::Revoked, false)
        {
            self.emit_message(&chat, &id);
            self.emit_chat(&chat);
        }
        let commands = self.commands.clone();
        tokio::spawn(async move {
            if let Err(error) = client.revoke_message(jid, id, RevokeType::Sender).await {
                let _ = commands.send(Command::Sent {
                    chat,
                    id: String::new(),
                    error: Some(format!(
                        "Could not delete the message for everyone: {error}"
                    )),
                });
            }
        });
    }

    fn send_files(
        &mut self,
        chat: ChatId,
        paths: Vec<PathBuf>,
        caption: Option<String>,
        mentions: Vec<String>,
    ) {
        if !self.send_allowed(&chat) {
            log::debug!("refusing files send without proven capability");
            return;
        }
        for (index, path) in paths.into_iter().enumerate() {
            let Some(client) = self.client.clone() else {
                self.emit(Event::Error("Not connected to WhatsApp".to_owned()));
                return;
            };
            let commands = self.commands.clone();
            let chat = chat.clone();
            let dir = self.dirs.media_cache_dir();
            let me = self.me();
            // Attach the caption to the first file.
            let caption = if index == 0 { caption.clone() } else { None };
            let mentions = if index == 0 {
                mentions.clone()
            } else {
                Vec::new()
            };
            tokio::spawn(async move {
                let outcome = async {
                    let bytes = tokio::fs::read(&path)
                        .await
                        .map_err(|error| format!("{}: {error}", path.display()))?;
                    let mime = mime_guess2::from_path(&path)
                        .first_or_octet_stream()
                        .to_string();
                    let file_name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned());
                    let prepared =
                        prepare_media(&client, bytes, &mime, file_name.as_deref(), false).await?;
                    file_outbound(&client, &chat, &me, &dir, prepared, caption, mentions).await
                }
                .await;
                match outcome {
                    Ok((row, raw)) => {
                        let _ = commands.send(Command::Outbound {
                            chat,
                            row: Box::new(row),
                            raw,
                        });
                    }
                    Err(error) => {
                        let _ = commands.send(Command::Sent {
                            chat,
                            id: String::new(),
                            error: Some(format!("Could not send the file: {error}")),
                        });
                    }
                }
            });
        }
    }

    fn send_pasted_image(
        &mut self,
        chat: ChatId,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
        caption: Option<String>,
        mentions: Vec<String>,
    ) {
        if !self.send_allowed(&chat) {
            log::debug!("refusing pasted-image send without proven capability");
            return;
        }
        let Some(client) = self.client.clone() else {
            self.emit(Event::Error("Not connected to WhatsApp".to_owned()));
            return;
        };
        let commands = self.commands.clone();
        let dir = self.dirs.media_cache_dir();
        let me = self.me();
        tokio::spawn(async move {
            let outcome = async {
                let encoded = tokio::task::spawn_blocking(move || {
                    let image = image::RgbaImage::from_raw(width, height, rgba)
                        .ok_or_else(|| "Clipboard image data is invalid".to_owned())?;
                    encode_jpeg(&image::DynamicImage::ImageRgba8(image), 88)
                })
                .await
                .map_err(|error| error.to_string())??;
                let prepared = prepare_media(&client, encoded, "image/jpeg", None, false).await?;
                file_outbound(&client, &chat, &me, &dir, prepared, caption, mentions).await
            }
            .await;
            match outcome {
                Ok((row, raw)) => {
                    let _ = commands.send(Command::Outbound {
                        chat,
                        row: Box::new(row),
                        raw,
                    });
                }
                Err(error) => {
                    let _ = commands.send(Command::Sent {
                        chat,
                        id: String::new(),
                        error: Some(format!("Could not send the picture: {error}")),
                    });
                }
            }
        });
    }

    /// Encodes and sends an OGG/Opus voice message with optional quote.
    fn send_voice(&mut self, chat: ChatId, samples: Vec<f32>, quoting: Option<String>) {
        if !self.send_allowed(&chat) {
            log::debug!("refusing voice send without proven capability");
            return;
        }
        let Some(client) = self.client.clone() else {
            self.emit(Event::Error("Not connected to WhatsApp".to_owned()));
            return;
        };
        let quote = quoting.as_deref().and_then(|id| {
            let raw = self.archive.raw(&chat, id).ok().flatten()?;
            let quoted = wa::Message::decode_from_slice(&raw).ok()?;
            let row = self.archive.message(&chat, id).ok().flatten()?;
            let jid = Self::jid_of(&chat)?;
            let sender = Self::jid_of(&row.sender).unwrap_or_else(|| jid.clone());
            let context = whatsapp_rust::wacore::proto_helpers::build_quote_context_with_info(
                row.id.clone(),
                &sender,
                &jid,
                &jid,
                &quoted,
            );
            let shown = Quoted {
                mentions: row.mentions.clone(),
                id: row.id,
                sender_name: if row.from_me {
                    Some("You".to_owned())
                } else {
                    row.sender_name
                        .clone()
                        .or_else(|| self.name_for(&row.sender))
                },
                sender: row.sender,
                summary: row.content.summary(),
            };
            Some((context, shown))
        });
        let (context, shown) = match quote {
            Some((context, shown)) => (Some(Box::new(context)), Some(shown)),
            None => (None, None),
        };
        let commands = self.commands.clone();
        let dir = self.dirs.media_cache_dir();
        let me = self.me();
        tokio::spawn(async move {
            let outcome = async {
                let (bytes, seconds, waveform) = tokio::task::spawn_blocking(move || {
                    let mut samples = samples;
                    crate::voice::normalize(&mut samples);
                    let seconds = (samples.len() as f64 / f64::from(crate::voice::RATE))
                        .round()
                        .max(1.0) as u32;
                    let waveform = crate::voice::waveform(&samples);
                    crate::voice::encode(&samples).map(|bytes| (bytes, seconds, waveform))
                })
                .await
                .map_err(|error| error.to_string())??;
                let prepared = prepare_voice(&client, bytes, seconds, waveform, context).await?;
                file_outbound(&client, &chat, &me, &dir, prepared, None, Vec::new()).await
            }
            .await;
            match outcome {
                Ok((mut row, raw)) => {
                    row.quoted = shown;
                    let _ = commands.send(Command::Outbound {
                        chat,
                        row: Box::new(row),
                        raw,
                    });
                }
                Err(error) => {
                    let _ = commands.send(Command::Sent {
                        chat,
                        id: String::new(),
                        error: Some(format!("Could not send the voice message: {error}")),
                    });
                }
            }
        });
    }

    /// Sends a played receipt for an incoming voice message.
    fn mark_played(&mut self, chat: ChatId, message: String, sender: String) {
        let (Some(client), Some(jid)) = (self.client.clone(), Self::jid_of(&chat)) else {
            return;
        };
        let sender = if jid.is_group() {
            sender.parse::<Jid>().ok()
        } else {
            None
        };
        let commands = self.commands.clone();
        tokio::spawn(async move {
            if !receipts_allowed(&client, &jid, &commands).await {
                return;
            }
            if let Err(error) = client
                .mark_as_played(&jid, sender.as_ref(), &[message.as_str()])
                .await
            {
                log::debug!("played receipt not sent: {error}");
            }
        });
    }

    /// Sends a WebP sticker.
    ///
    /// The local copy is filed and drawn before the upload starts: the bubble
    /// paints the sticker at once and only its tick waits for the server.
    fn send_sticker(&mut self, chat: ChatId, path: PathBuf, quoting: Option<String>) {
        if !self.send_allowed(&chat) {
            log::debug!("refusing sticker send without proven capability");
            return;
        }
        let Some(client) = self.client.clone() else {
            self.emit(Event::Error("Not connected to WhatsApp".to_owned()));
            return;
        };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.emit(Event::Error(format!("Could not read the sticker: {error}")));
                return;
            }
        };
        let Some(shape) = sticker_shape(&bytes) else {
            self.emit(Event::Error(
                "Could not read the sticker: it is not a picture".to_owned(),
            ));
            return;
        };
        let quote = quoting.as_deref().and_then(|id| {
            let jid = Self::jid_of(&chat)?;
            self.quote_of(&chat, id, &jid)
        });
        let quoted = quote
            .as_ref()
            .map(|(_, row)| self.quoted_summary(row.clone()));
        let context = quote.map(|(context, _)| context);
        let (animated, width, height) = shape;
        let id = client.generate_message_id();
        // Filed under the hash of its bytes, like every other sticker copy:
        // the same picture is one file, one preview, and one entry.
        let file = self
            .dirs
            .media_cache_dir()
            .join(format!("{}.webp", crate::stickers::hash_of(&bytes)));
        // The copy beside the archive is what the bubble, the reply bar, and
        // the viewer read while the upload is still on its way.
        let kept = std::fs::create_dir_all(self.dirs.media_cache_dir())
            .and_then(|()| std::fs::write(&file, &bytes));
        if let Err(error) = &kept {
            log::warn!("could not keep the sticker copy: {error}");
        }
        let mut content = Content::Sticker {
            media: media(
                Some(&"image/webp".to_owned()),
                Some(bytes.len() as u64),
                Some(width),
                Some(height),
            ),
            animated,
        };
        if let Some(media) = content.media_mut() {
            // The copy beside the archive is what the bubble, the reply bar,
            // and the viewer read. Without one the send still goes, and the
            // file the picker drew from stands in: a picture beats a bubble
            // that spins forever over a file nobody can read.
            media.path = Some(if kept.is_ok() { file } else { path });
        }
        let row = Message {
            id: id.clone(),
            chat: chat.clone(),
            sender: self.me(),
            sender_name: None,
            from_me: true,
            timestamp: crate::util::now(),
            content,
            status: Delivery::Pending,
            delivered_at: None,
            read_at: None,
            quoted,
            reactions: Vec::new(),
            edited: false,
            mentions: Vec::new(),
            forwarded: false,
            thumbnail: None,
        };
        self.store_message(row.clone(), None, None);
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let outcome = async {
                let prepared = prepare_sticker(&client, bytes, shape, context).await?;
                Ok::<_, String>(prepared.message.encode_to_vec())
            }
            .await;
            match outcome {
                Ok(raw) => {
                    let _ = commands.send(Command::Outbound {
                        chat,
                        row: Box::new(row),
                        raw,
                    });
                }
                Err(error) => {
                    let _ = commands.send(Command::Sent {
                        chat,
                        id,
                        error: Some(format!("Could not send the sticker: {error}")),
                    });
                }
            }
        });
    }

    /// Archives and sends an uploaded attachment message.
    fn outbound(&mut self, chat: ChatId, row: Message, raw: Vec<u8>) {
        let (Some(client), Some(jid)) = (self.client.clone(), Self::jid_of(&chat)) else {
            self.emit(Event::Error("Not connected to WhatsApp".to_owned()));
            return;
        };
        let Ok(mut message) = wa::Message::decode_from_slice(&raw) else {
            self.emit(Event::Error("Could not encode the attachment".to_owned()));
            return;
        };
        let expiration = self.apply_ephemeral(&chat, &mut message);
        let raw = message.encode_to_vec();
        let id = row.id.clone();
        self.store_message(row, Some(raw), None);
        tokio::spawn(send_outgoing(
            client,
            self.commands.clone(),
            chat,
            jid,
            id,
            message,
            expiration,
        ));
    }

    fn react(&mut self, chat: ChatId, id: String, emoji: String) {
        let (Some(client), Some(jid)) = (self.client.clone(), Self::jid_of(&chat)) else {
            self.emit(Event::Error("Not connected to WhatsApp".to_owned()));
            return;
        };
        let Ok(Some(target)) = self.archive.message(&chat, &id) else {
            return;
        };
        let me = self.me();
        if let Ok(Some(updated)) = self.archive.set_reaction(&chat, &id, &me, true, &emoji) {
            self.emit(Event::MessageUpdated(Box::new(updated)));
        }
        let key = wa::MessageKey {
            remote_jid: Some(chat.clone()),
            from_me: Some(target.from_me),
            id: Some(id),
            participant: (jid.is_group() && !target.from_me).then(|| target.sender.clone()),
        };
        tokio::spawn(async move {
            if let Err(error) = client.send_reaction(jid, key, &emoji).await {
                log::warn!("reaction not sent: {error}");
            }
        });
    }
}

// --- free helpers ----------------------------------------------------------

fn outgoing_forward(original: &wa::Message, expiration: Option<u32>) -> (wa::Message, Option<u32>) {
    let mut message = *original.get_base_message().prepare_for_forward();
    if let Some(mut context) = context_of(&message).cloned() {
        // A forward belongs to the destination chat. The library retains the
        // source timer, including when the destination has no timer at all.
        context.expiration = None;
        context.ephemeral_setting_timestamp = None;
        context.ephemeral_shared_secret = None;
        message.set_context_info(context);
    }
    let expiration = apply_ephemeral_expiration(&mut message, expiration);
    (message, expiration)
}

fn apply_ephemeral_expiration(message: &mut wa::Message, expiration: Option<u32>) -> Option<u32> {
    let expiration = expiration.filter(|expiration| *expiration > 0)?;
    message
        .set_ephemeral_expiration(expiration)
        .then_some(expiration)
}

async fn send_outgoing(
    client: Arc<Client>,
    commands: mpsc::UnboundedSender<Command>,
    chat: ChatId,
    jid: Jid,
    id: String,
    message: wa::Message,
    ephemeral_expiration: Option<u32>,
) {
    let result = async {
        if jid.is_group() {
            // Uses whatsapp-rust's send cache; only a miss queries the server,
            // exactly as encryption would. No separate burst of metadata queries.
            let group = client
                .groups()
                .query_info(&jid)
                .await
                .map_err(|error| error.to_string())?;
            let lids = group
                .participants
                .iter()
                .filter(|jid| jid.is_lid())
                .filter_map(|lid| {
                    group
                        .phone_jid_for_lid_user(lid.user_base())
                        .map(|pn| (lid.user_base().to_owned(), pn.user_base().to_owned()))
                })
                .collect();
            let recipients = group
                .participants
                .iter()
                .map(Jid::to_non_ad_string)
                .collect();
            let (stored, mut saved) = mpsc::unbounded_channel();
            commands
                .send(Command::GroupRecipients {
                    chat: chat.clone(),
                    id: id.clone(),
                    recipients,
                    lids,
                    stored,
                })
                .map_err(|_| "The application is shutting down".to_owned())?;
            if saved.recv().await != Some(true) {
                return Err("Could not save the group message recipients".to_owned());
            }
        }
        let mut options = SendOptions::default().with_message_id(id.clone());
        if let Some(expiration) = ephemeral_expiration {
            options = options.with_ephemeral_expiration(expiration);
        }
        client
            .send_message_with_options(jid, message, options)
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }
    .await;
    let _ = commands.send(Command::Sent {
        chat,
        id,
        error: result.err(),
    });
}

fn forwarded_row(
    mut source: Message,
    chat: ChatId,
    sender: String,
    id: String,
    timestamp: i64,
    mentions: Vec<MentionRef>,
    thumbnail: Option<Vec<u8>>,
) -> Message {
    source.id = id;
    source.chat = chat;
    source.sender = sender;
    source.sender_name = None;
    source.from_me = true;
    source.timestamp = timestamp;
    source.status = Delivery::Pending;
    source.delivered_at = None;
    source.read_at = None;
    source.quoted = None;
    source.reactions.clear();
    source.edited = false;
    source.mentions = mentions;
    source.forwarded = true;
    source.thumbnail = thumbnail;
    source
}

/// Fallback chat name from a phone number or bare id.
fn fallback_name(id: &str) -> String {
    match crate::model::phone_of(id) {
        Some(digits) => crate::util::phone(digits),
        None if ChatKind::from_id(id) == ChatKind::Group => "Group".to_owned(),
        None => id.split('@').next().unwrap_or(id).to_owned(),
    }
}

/// Normalizes WhatsApp timestamps to seconds.
fn seconds(timestamp: i64) -> i64 {
    if timestamp > 100_000_000_000 {
        timestamp / 1000
    } else {
        timestamp.max(0)
    }
}

/// Where a deleted or cleared chat ends: the last message the deleting
/// device knew about, or the moment of the action when it sent no range.
fn removal_point(last_message: Option<i64>, action: i64) -> i64 {
    last_message
        .filter(|timestamp| *timestamp > 0)
        .map_or(action, seconds)
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn extension_for(mime: &str, file_name: Option<&str>) -> String {
    if let Some(extension) = file_name
        .and_then(|name| Path::new(name).extension())
        .and_then(|extension| extension.to_str())
        .filter(|extension| !extension.is_empty() && extension.len() <= 8)
    {
        return extension.to_ascii_lowercase();
    }
    let mime = mime.split(';').next().unwrap_or(mime).trim();
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/3gpp" => "3gp",
        "audio/ogg" => "ogg",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "audio/aac" => "aac",
        "audio/wav" => "wav",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        _ => mime.rsplit('/').next().unwrap_or("bin"),
    }
    .to_owned()
}

fn media_path(dir: &Path, chat: &str, id: &str, mime: &str, file_name: Option<&str>) -> PathBuf {
    let extension = extension_for(mime, file_name);
    let stem = match file_name.and_then(|name| Path::new(name).file_stem()?.to_str()) {
        Some(name) => format!("{}-{}", sanitize(id), sanitize(name)),
        None => format!("{}-{}", sanitize(chat), sanitize(id)),
    };
    dir.join(format!("{stem}.{extension}"))
}

/// Budget for one attachment download: an effective byte cap and a total
/// deadline covering download, re-upload, and retry.
#[derive(Clone, Copy, Debug)]
struct DownloadLimits {
    max_bytes: u64,
    timeout: Duration,
}

/// Caps a download at its declared plaintext length plus slack, falling
/// back to the absolute ceiling when the length is unknown. Narrower than
/// the old unbounded fetch without changing what the app may download:
/// honest WhatsApp references always declare their length below the cap.
fn download_limits_for(declared: Option<u64>) -> DownloadLimits {
    let max_bytes = declared
        .map(|length| length.saturating_add(Worker::DOWNLOAD_LENGTH_SLACK))
        .unwrap_or(Worker::DOWNLOAD_MAX_BYTES)
        .min(Worker::DOWNLOAD_MAX_BYTES);
    DownloadLimits {
        max_bytes,
        timeout: Worker::DOWNLOAD_TIMEOUT,
    }
}

/// File sink that refuses to grow past a budget, for streaming downloads
/// that must never hold the whole attachment in RAM. Truncating back to
/// zero (what the library does before every retry) restores the budget.
struct LimitedFile {
    file: std::fs::File,
    max_bytes: u64,
    remaining: u64,
}

impl LimitedFile {
    fn create(path: &Path, max_bytes: u64) -> std::io::Result<Self> {
        Ok(Self {
            file: std::fs::File::create(path)?,
            max_bytes,
            remaining: max_bytes,
        })
    }
}

impl std::io::Write for LimitedFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.len() as u64 > self.remaining {
            return Err(std::io::Error::new(
                std::io::ErrorKind::QuotaExceeded,
                format!("attachment exceeds its {} byte budget", self.max_bytes),
            ));
        }
        let written = self.file.write(buf)?;
        self.remaining -= written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl std::io::Seek for LimitedFile {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.file.seek(pos)
    }
}

impl whatsapp_rust::wacore::download::DownloadWriter for LimitedFile {
    fn truncate(&mut self, len: u64) -> std::io::Result<()> {
        self.file.set_len(len)?;
        self.remaining = self.max_bytes.saturating_sub(len);
        Ok(())
    }
}

/// Removes a temporary download file on drop unless it was published away
/// by rename. Covers errors, timeouts, and cancelled tasks: a partial file
/// is never left behind to be mistaken for a complete attachment.
struct TempGuard {
    path: Option<PathBuf>,
}

impl TempGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Publishes a verified temporary download over its destination without
/// ever exposing a partial file. When a previous copy exists it is moved
/// aside first and restored if the final rename fails, so a failed publish
/// never destroys the last valid copy.
async fn publish_download(temp: &Path, dest: &Path) -> Result<(), String> {
    if tokio::fs::try_exists(dest)
        .await
        .map_err(|error| error.to_string())?
    {
        let mut backup = dest.as_os_str().to_owned();
        backup.push(".bak");
        let backup = PathBuf::from(backup);
        tokio::fs::rename(dest, &backup)
            .await
            .map_err(|error| error.to_string())?;
        if let Err(error) = tokio::fs::rename(temp, dest).await {
            let _ = tokio::fs::rename(&backup, dest).await;
            return Err(error.to_string());
        }
        let _ = tokio::fs::remove_file(&backup).await;
    } else {
        tokio::fs::rename(temp, dest)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Whether a download failure means the reference expired and the phone
/// should be asked to re-upload. Typed status first, message fallback for
/// errors whose chain carries no status.
fn is_expired_media_error(error: &anyhow::Error) -> bool {
    use whatsapp_rust::ErrorChainExt;
    if let Some(status) = error.http_status() {
        return matches!(status, 403 | 404 | 410);
    }
    let text = error.to_string();
    ["403", "404", "410"].iter().any(|code| text.contains(code))
}

/// Restores downloads whose publish was interrupted between moving the old
/// copy aside and renaming the temporary file over it. For every backup
/// file: a missing destination means the interruption happened mid-publish
/// and the backup is still the last valid copy, so it moves back; an
/// Builds the sweep keep set both the startup cleanup and its tests share:
/// proven references plus extra paths that must survive (backups whose
/// restore is still pending). Pure string mapping, no I/O.
fn sweep_keep_set(
    protected: std::collections::HashSet<PathBuf>,
    extra: &[PathBuf],
) -> HashSet<String> {
    protected
        .into_iter()
        .chain(extra.iter().cloned())
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

/// existing destination means the publish completed and only its cleanup
/// was missed, so the backup goes. A failed restore keeps the backup for
/// the next run instead of deleting anything. Returns how many backups
/// moved back, plus the backup paths that must survive the next cleanup
/// because their restore is still pending.
fn recover_interrupted_publishes(dir: &Path) -> (usize, Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            log::warn!("attachments: could not scan for interrupted publishes: {error}");
            return (0, Vec::new());
        }
    };
    let mut restored = 0;
    let mut preserved = Vec::new();
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(_) => continue,
        };
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(dest_name) = name.strip_suffix(".bak") else {
            continue;
        };
        let dest = path.with_file_name(dest_name);
        if !dest.exists() {
            match std::fs::rename(&path, &dest) {
                Ok(()) => {
                    restored += 1;
                    log::info!("attachments: restored an interrupted publish");
                }
                Err(error) => {
                    log::warn!("attachments: could not restore an interrupted publish: {error}");
                    preserved.push(path);
                }
            }
        } else if dest.is_file() {
            // The publish completed and only its cleanup was missed. A
            // destination that is not a file is not ours to judge: the
            // backup stays for a human to look at.
            if let Err(error) = std::fs::remove_file(&path) {
                log::warn!("attachments: could not clear a spent backup: {error}");
            }
        } else {
            log::warn!("attachments: backup kept, destination is not a file");
            preserved.push(path);
        }
    }
    (restored, preserved)
}

/// Validates a downloaded image straight from disk with bounded memory.
/// Headers (format, dimensions) always come from a header-only read that
/// never holds pixels; only small files pay for a full decode. Larger
/// files are accepted on headers alone, which the CDN authentication
/// already backs with a whole-file MAC.
fn validate_image_file(path: &Path, mime: &str, full_decode_max: u64) -> Result<(), String> {
    let size = std::fs::metadata(path)
        .map_err(|error| error.to_string())?
        .len();
    if size == 0 {
        return Err("The download came back empty".to_owned());
    }
    let not_picture = || "The download is not a readable picture".to_owned();
    // Explicit decoder limits on top of the 512 MiB allocation guard that
    // ships by default; the fields are public, only the struct itself is
    // non-exhaustive.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(Worker::IMAGE_MAX_SIDE);
    limits.max_image_height = Some(Worker::IMAGE_MAX_SIDE);
    let mut reader = image::ImageReader::open(path)
        .map_err(|_| not_picture())?
        .with_guessed_format()
        .map_err(|_| not_picture())?;
    reader.limits(limits);
    let (width, height) = reader.into_dimensions().map_err(|_| not_picture())?;
    if !image_dimensions_acceptable(width, height) {
        return Err(not_picture());
    }
    if size <= full_decode_max {
        let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
        Worker::validate_media_bytes(&bytes, mime)?;
    }
    Ok(())
}

/// Whether decoded dimensions fit the validator budget: nonzero, within
/// the side ceiling, and within the total pixel budget. Pure so the
/// boundaries stay testable without multi-hundred-megapixel fixtures.
fn image_dimensions_acceptable(width: u32, height: u32) -> bool {
    if width == 0 || height == 0 {
        return false;
    }
    if width > Worker::IMAGE_MAX_SIDE || height > Worker::IMAGE_MAX_SIDE {
        return false;
    }
    (width as u64) * (height as u64) <= Worker::IMAGE_MAX_PIXELS
}

/// Streams one attachment into a temporary file under the given budget and
/// deadline. RAM stays flat: the library decrypts straight into the sink
/// with small buffers, and the cap refuses exorbitant streams mid-write.
async fn fetch_to_temp(
    client: &Client,
    downloadable: &dyn Downloadable,
    dir: &Path,
    temp: &Path,
    limits: DownloadLimits,
    deadline: tokio::time::Instant,
) -> Result<(), anyhow::Error> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(anyhow::Error::from)?;
    let file = LimitedFile::create(temp, limits.max_bytes).map_err(anyhow::Error::from)?;
    match tokio::time::timeout_at(deadline, client.download_to_writer(downloadable, file)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(anyhow::anyhow!(
            "Download timed out after {} seconds",
            limits.timeout.as_secs()
        )),
    }
}

fn media(
    mime: Option<&String>,
    size: Option<u64>,
    width: Option<u32>,
    height: Option<u32>,
) -> Media {
    Media {
        mime: mime.cloned().unwrap_or_default(),
        size: size.unwrap_or(0),
        width,
        height,
        path: None,
        state: Default::default(),
    }
}

fn non_empty(text: &Option<String>) -> Option<String> {
    text.clone().filter(|text| !text.trim().is_empty())
}

/// Builds a text body with optional quote and mention context.
fn outgoing_text(
    text: String,
    mut context: Option<wa::ContextInfo>,
    mentions: &[String],
) -> wa::Message {
    if !mentions.is_empty() {
        context.get_or_insert_default().mentioned_jid = mentions.to_vec();
    }
    match context {
        Some(context) => wa::Message::text_with_context(text, context),
        None => wa::Message::text(text),
    }
}

/// Extracts quote and mention context from a message.
fn context_of(base: &wa::Message) -> Option<&wa::ContextInfo> {
    if let Some(text) = base.extended_text_message.as_option() {
        return text.context_info.as_option();
    }
    if let Some(image) = base.image_message.as_option() {
        return image.context_info.as_option();
    }
    if let Some(video) = base
        .video_message
        .as_option()
        .or(base.ptv_message.as_option())
    {
        return video.context_info.as_option();
    }
    if let Some(audio) = base.audio_message.as_option() {
        return audio.context_info.as_option();
    }
    if let Some(document) = base.document_message.as_option() {
        return document.context_info.as_option();
    }
    if let Some(sticker) = base.sticker_message.as_option() {
        return sticker.context_info.as_option();
    }
    if let Some(location) = base.location_message.as_option() {
        return location.context_info.as_option();
    }
    if let Some(location) = base.live_location_message.as_option() {
        return location.context_info.as_option();
    }
    if let Some(contact) = base.contact_message.as_option() {
        return contact.context_info.as_option();
    }
    if let Some(contacts) = base.contacts_array_message.as_option() {
        return contacts.context_info.as_option();
    }
    if let Some(poll) = base
        .poll_creation_message
        .as_option()
        .or(base.poll_creation_message_v2.as_option())
        .or(base.poll_creation_message_v3.as_option())
    {
        return poll.context_info.as_option();
    }
    None
}

/// Returns raw JIDs mentioned by a message.
fn mentioned_of(base: &wa::Message) -> Vec<String> {
    context_of(base)
        .map(|context| context.mentioned_jid.clone())
        .unwrap_or_default()
}

fn forwarded_of(base: &wa::Message) -> bool {
    context_of(base).is_some_and(|context| {
        context.is_forwarded.unwrap_or(false) || context.forwarding_score.unwrap_or(0) > 0
    })
}

/// Forwarding score of a message: how many chats it already passed through.
/// WhatsApp marks five or more as frequently forwarded.
fn forwarding_score_of(base: &wa::Message) -> u32 {
    context_of(base)
        .and_then(|context| context.forwarding_score)
        .unwrap_or(0)
}

/// Whether WhatsApp restricts a message to a single destination chat.
fn is_frequently_forwarded(score: u32) -> bool {
    score >= crate::model::FREQUENT_FORWARD_SCORE
}

/// Applies WhatsApp's destination caps: five chats, or one when any message
/// was forwarded many times. Keeps the dialog order and drops repeats.
fn cap_destinations(to_chats: Vec<ChatId>, frequently_forwarded: bool) -> Vec<ChatId> {
    let mut chats = to_chats;
    chats.truncate(if frequently_forwarded {
        1
    } else {
        crate::model::FORWARD_CHAT_LIMIT
    });
    let mut seen = HashSet::new();
    chats.retain(|chat| seen.insert(chat.clone()));
    chats
}

/// Finds the first web address when preview metadata omits its URL.
fn first_link(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|token| token.starts_with("http://") || token.starts_with("https://"))
        .map(|token| token.trim_end_matches(['.', ',', ')', ']']).to_owned())
}

/// Extracts the attachment or link-preview thumbnail.
fn thumbnail_of(base: &wa::Message) -> Option<Vec<u8>> {
    let bytes = if let Some(image) = base.image_message.as_option() {
        image.jpeg_thumbnail.clone()
    } else if let Some(video) = base
        .video_message
        .as_option()
        .or(base.ptv_message.as_option())
    {
        video.jpeg_thumbnail.clone()
    } else if let Some(document) = base.document_message.as_option() {
        document.jpeg_thumbnail.clone()
    } else if let Some(text) = base.extended_text_message.as_option() {
        text.jpeg_thumbnail.clone()
    } else {
        None
    };
    bytes.filter(|bytes| !bytes.is_empty())
}

/// Converts a protocol message to visible content, or `None` for internal traffic.
/// Turns a view-once payload into the note shown in its place.
/// Puts a picture on the system clipboard, as straight RGBA.
fn copy_image_to_clipboard(path: &Path) -> Result<(), String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("Could not read the picture: {error}"))?;
    let (width, height, rgba) = crate::stickers::clipboard_pixels(&bytes)
        .ok_or_else(|| "This picture could not be decoded".to_owned())?;
    let mut clipboard = arboard::Clipboard::new().map_err(|error| error.to_string())?;
    clipboard
        .set_image(arboard::ImageData {
            width: width as usize,
            height: height as usize,
            bytes: std::borrow::Cow::Owned(rgba),
        })
        .map_err(|error| error.to_string())
}

/// Streams a file through SHA-256 without holding all of it in memory.
fn sha256_of(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn view_once_of(base: &wa::Message, content: Content) -> Content {
    let what = match &content {
        Content::Video { .. } => "video",
        Content::Image { .. } => "photo",
        Content::Audio { .. } => "voice message",
        _ if base.video_message.is_set() => "video",
        _ if base.image_message.is_set() => "photo",
        _ => "message",
    };
    Content::ViewOnce {
        what: what.to_owned(),
    }
}

fn classify(base: &wa::Message) -> Option<Content> {
    if let Some(text) = base.text_content() {
        let preview = base.extended_text_message.as_option().and_then(|extended| {
            let title = non_empty(&extended.title);
            let description = non_empty(&extended.description);
            let has_picture = extended
                .jpeg_thumbnail
                .as_ref()
                .is_some_and(|bytes| !bytes.is_empty());
            if title.is_none() && description.is_none() && !has_picture {
                return None;
            }
            let url = non_empty(&extended.matched_text).or_else(|| first_link(text))?;
            let url = if url.contains("://") {
                url
            } else {
                format!("https://{url}")
            };
            Some(LinkPreview {
                url,
                title,
                description,
            })
        });
        return Some(Content::Text {
            text: text.to_owned(),
            preview,
        });
    }
    if let Some(image) = base.image_message.as_option() {
        return Some(Content::Image {
            caption: non_empty(&image.caption),
            media: media(
                image.mimetype.as_ref(),
                image.file_length,
                image.width,
                image.height,
            ),
        });
    }
    if let Some(video) = base
        .video_message
        .as_option()
        .or(base.ptv_message.as_option())
    {
        return Some(Content::Video {
            caption: non_empty(&video.caption),
            media: media(
                video.mimetype.as_ref(),
                video.file_length,
                video.width,
                video.height,
            ),
            // Zero means the phone sent no length: keep it unknown so the
            // bubble omits it until the downloaded file is analyzed.
            seconds: video.seconds.filter(|seconds| *seconds > 0),
            gif: video.gif_playback.unwrap_or(false),
        });
    }
    if let Some(audio) = base.audio_message.as_option() {
        return Some(Content::Audio {
            media: media(audio.mimetype.as_ref(), audio.file_length, None, None),
            seconds: audio.seconds,
            voice_note: audio.ptt.unwrap_or(false),
            waveform: audio.waveform.clone().unwrap_or_default(),
        });
    }
    if let Some(document) = base.document_message.as_option() {
        let file_name = non_empty(&document.file_name)
            .or_else(|| non_empty(&document.title))
            .unwrap_or_else(|| "Document".to_owned());
        return Some(Content::Document {
            media: media(document.mimetype.as_ref(), document.file_length, None, None),
            file_name,
            caption: non_empty(&document.caption),
            pages: document.page_count,
        });
    }
    if let Some(sticker) = base.sticker_message.as_option() {
        return Some(Content::Sticker {
            media: media(
                sticker.mimetype.as_ref(),
                sticker.file_length,
                sticker.width,
                sticker.height,
            ),
            animated: sticker.is_animated.unwrap_or(false),
        });
    }
    if let Some(location) = base.location_message.as_option() {
        return Some(Content::Location {
            latitude: location.degrees_latitude.unwrap_or(0.0),
            longitude: location.degrees_longitude.unwrap_or(0.0),
            name: non_empty(&location.name),
            address: non_empty(&location.address),
        });
    }
    if let Some(live) = base.live_location_message.as_option() {
        return Some(Content::Location {
            latitude: live.degrees_latitude.unwrap_or(0.0),
            longitude: live.degrees_longitude.unwrap_or(0.0),
            name: Some("Live location".to_owned()),
            address: None,
        });
    }
    if let Some(contact) = base.contact_message.as_option() {
        return Some(Content::Contact {
            display_name: non_empty(&contact.display_name).unwrap_or_else(|| "Contact".to_owned()),
            vcard: contact.vcard.clone().unwrap_or_default(),
        });
    }
    if let Some(contacts) = base.contacts_array_message.as_option() {
        let count = contacts.contacts.len();
        return Some(Content::Contact {
            display_name: non_empty(&contacts.display_name)
                .unwrap_or_else(|| format!("{count} contacts")),
            vcard: contacts
                .contacts
                .iter()
                .filter_map(|contact| contact.vcard.clone())
                .collect::<Vec<_>>()
                .join("\n"),
        });
    }
    if let Some(poll) = base
        .poll_creation_message
        .as_option()
        .or(base.poll_creation_message_v2.as_option())
        .or(base.poll_creation_message_v3.as_option())
    {
        return Some(Content::Poll {
            question: non_empty(&poll.name).unwrap_or_else(|| "Poll".to_owned()),
            state: crate::model::PollState {
                selectable: poll.selectable_options_count.unwrap_or(0) as usize,
                ..Default::default()
            },
            options: poll
                .options
                .iter()
                .map(|option| option.option_name.clone().unwrap_or_default())
                .collect(),
        });
    }
    let unsupported = |what: &str| {
        Some(Content::Unsupported {
            what: what.to_owned(),
        })
    };
    if base.album_message.is_set() {
        return None;
    }
    if base.group_invite_message.is_set() {
        return unsupported("group invite");
    }
    if base.event_message.is_set() {
        return unsupported("event");
    }
    if base.sticker_pack_message.is_set() {
        if let Some(pack) = base.sticker_pack_message.as_option() {
            return Some(Content::StickerPack {
                name: pack.name.clone().unwrap_or_default(),
                publisher: pack.publisher.clone().unwrap_or_default(),
                count: pack.stickers.len() as u32,
                caption: pack
                    .caption
                    .clone()
                    .filter(|caption| !caption.trim().is_empty()),
            });
        }
        return unsupported("sticker pack");
    }
    if base.interactive_message.is_set()
        || base.buttons_message.is_set()
        || base.list_message.is_set()
        || base.template_message.is_set()
        || base.buttons_response_message.is_set()
        || base.list_response_message.is_set()
        || base.interactive_response_message.is_set()
        || base.template_button_reply_message.is_set()
    {
        return unsupported("interactive message");
    }
    if base.product_message.is_set() || base.order_message.is_set() {
        return unsupported("product");
    }
    if base.send_payment_message.is_set()
        || base.request_payment_message.is_set()
        || base.payment_invite_message.is_set()
        || base.invoice_message.is_set()
    {
        return unsupported("payment");
    }
    if base.call_log_messsage.is_set() || base.scheduled_call_creation_message.is_set() {
        return unsupported("call");
    }
    if base.lottie_sticker_message.is_set() {
        return unsupported("animated sticker");
    }
    if base.poll_update_message.is_set()
        || base.enc_reaction_message.is_set()
        || base.enc_comment_message.is_set()
        || base.enc_event_response_message.is_set()
        || base.keep_in_chat_message.is_set()
        || base.pin_in_chat_message.is_set()
        || base.sender_key_distribution_message.is_set()
        || base
            .fast_ratchet_key_sender_key_distribution_message
            .is_set()
        || base.sticker_sync_rmr_message.is_set()
        || base.message_context_info.is_set()
        || base.device_sent_message.is_set()
        || base.placeholder_message.is_set()
        || base.secret_encrypted_message.is_set()
        || base.message_history_bundle.is_set()
        || base.message_history_notice.is_set()
        || base.bot_invoke_message.is_set()
    {
        return None;
    }
    if *base == wa::Message::default() {
        return None;
    }
    unsupported("message")
}

/// Uploaded attachment protobuf and archive content.
struct Prepared {
    message: wa::Message,
    content: Content,
    thumbnail: Option<Vec<u8>>,
    bytes: Vec<u8>,
    mime: String,
    file_name: Option<String>,
}

fn encode_jpeg(image: &image::DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality);
    encoder
        .encode_image(&image.to_rgb8())
        .map_err(|error| error.to_string())?;
    Ok(bytes)
}

/// Builds the pre-download attachment thumbnail.
fn thumbnail_jpeg(image: &image::DynamicImage) -> Option<Vec<u8>> {
    let small = image.thumbnail(THUMBNAIL_SIDE, THUMBNAIL_SIDE);
    encode_jpeg(&small, 60).ok()
}

/// Uploads a recording and builds a push-to-talk message with waveform.
async fn prepare_voice(
    client: &Client,
    bytes: Vec<u8>,
    seconds: u32,
    waveform: Vec<u8>,
    context: Option<Box<wa::ContextInfo>>,
) -> Result<Prepared, String> {
    let mime = "audio/ogg; codecs=opus".to_owned();
    let size = bytes.len() as u64;
    let upload = client
        .upload(bytes.clone(), MediaType::Audio, UploadOptions::default())
        .await
        .map_err(|error| error.to_string())?;
    let message = audio_message(
        upload,
        AudioOptions {
            mimetype: Some(mime.clone()),
            duration_seconds: Some(seconds),
            ptt: Some(true),
            waveform: Some(waveform.clone()),
            context_info: context,
        },
    );
    Ok(Prepared {
        message,
        content: Content::Audio {
            media: media(Some(&mime), Some(size), None, None),
            seconds: Some(seconds),
            voice_note: true,
            waveform,
        },
        thumbnail: None,
        bytes,
        mime,
        file_name: None,
    })
}

/// Uploads a file and builds its message. Images are encoded as JPEG.
async fn prepare_media(
    client: &Client,
    bytes: Vec<u8>,
    mime: &str,
    file_name: Option<&str>,
    gif: bool,
) -> Result<Prepared, String> {
    let kind = mime.split('/').next().unwrap_or_default();
    let is_picture = matches!(
        mime,
        "image/jpeg" | "image/png" | "image/webp" | "image/bmp" | "image/tiff"
    );
    if is_picture {
        let decoded = tokio::task::spawn_blocking({
            let bytes = bytes.clone();
            move || image::load_from_memory(&bytes).map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| error.to_string())??;
        let (width, height) = (decoded.width(), decoded.height());
        let jpeg = if mime == "image/jpeg" {
            bytes
        } else {
            encode_jpeg(&decoded, 88)?
        };
        let thumbnail = thumbnail_jpeg(&decoded);
        let upload = client
            .upload(jpeg.clone(), MediaType::Image, UploadOptions::default())
            .await
            .map_err(|error| error.to_string())?;
        let mut message = image_message(
            upload,
            ImageOptions {
                caption: None,
                mimetype: Some("image/jpeg".to_owned()),
                jpeg_thumbnail: thumbnail.clone(),
                context_info: None,
            },
        );
        if let Some(image) = message.image_message.as_option_mut() {
            image.width = Some(width);
            image.height = Some(height);
        }
        return Ok(Prepared {
            message,
            content: Content::Image {
                caption: None,
                media: media(
                    Some(&"image/jpeg".to_owned()),
                    Some(jpeg.len() as u64),
                    Some(width),
                    Some(height),
                ),
            },
            thumbnail,
            bytes: jpeg,
            mime: "image/jpeg".to_owned(),
            file_name: None,
        });
    }
    let size = bytes.len() as u64;
    let mime_owned = mime.to_owned();
    if kind == "video" {
        let upload = client
            .upload(bytes.clone(), MediaType::Video, UploadOptions::default())
            .await
            .map_err(|error| error.to_string())?;
        let message = video_message(
            upload,
            VideoOptions {
                mimetype: Some(mime_owned.clone()),
                gif_playback: Some(gif),
                ..Default::default()
            },
        );
        return Ok(Prepared {
            message,
            content: Content::Video {
                caption: None,
                media: media(Some(&mime_owned), Some(size), None, None),
                seconds: None,
                gif,
            },
            thumbnail: None,
            bytes,
            mime: mime_owned,
            file_name: file_name.map(str::to_owned),
        });
    }
    if kind == "audio" {
        let upload = client
            .upload(bytes.clone(), MediaType::Audio, UploadOptions::default())
            .await
            .map_err(|error| error.to_string())?;
        let message = audio_message(
            upload,
            AudioOptions {
                mimetype: Some(mime_owned.clone()),
                ptt: Some(false),
                ..Default::default()
            },
        );
        return Ok(Prepared {
            message,
            content: Content::Audio {
                media: media(Some(&mime_owned), Some(size), None, None),
                seconds: None,
                voice_note: false,
                waveform: Vec::new(),
            },
            thumbnail: None,
            bytes,
            mime: mime_owned,
            file_name: file_name.map(str::to_owned),
        });
    }
    let upload = client
        .upload(bytes.clone(), MediaType::Document, UploadOptions::default())
        .await
        .map_err(|error| error.to_string())?;
    let name = file_name.unwrap_or("file").to_owned();
    let message = document_message(
        upload,
        DocumentOptions {
            mimetype: Some(mime_owned.clone()),
            file_name: Some(name.clone()),
            title: Some(name.clone()),
            ..Default::default()
        },
    );
    Ok(Prepared {
        message,
        content: Content::Document {
            media: media(Some(&mime_owned), Some(size), None, None),
            file_name: name.clone(),
            caption: None,
            pages: None,
        },
        thumbnail: None,
        bytes,
        mime: mime_owned,
        file_name: Some(name),
    })
}

/// Renders one page, turning a panic inside the PDF parser into a message.
///
/// A damaged file must leave the viewer saying so, not waiting for an answer
/// that died with its task. Rendering the page after this one uses the same
/// call and simply ignores what comes back.
fn render_pdf_page(
    reader: &std::sync::Mutex<crate::pdf::Reader>,
    path: &Path,
    page: usize,
    width: u32,
) -> Result<crate::pdf::Page, String> {
    let reader = std::panic::AssertUnwindSafe(reader);
    std::panic::catch_unwind(move || {
        reader
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .render(path, page, width)
    })
    .map_err(|_| "This page could not be rendered: the file is damaged.".to_owned())?
}

/// Whether a sticker animates and how big it is, read from its own header.
fn sticker_shape(bytes: &[u8]) -> Option<(bool, u32, u32)> {
    let decoder = image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(bytes)).ok()?;
    let animated = decoder.has_animation();
    let (width, height) = image::ImageDecoder::dimensions(&decoder);
    Some((animated, width, height))
}

/// Uploads a WebP sticker and builds its message without a library builder.
async fn prepare_sticker(
    client: &Client,
    bytes: Vec<u8>,
    shape: (bool, u32, u32),
    context: Option<wa::ContextInfo>,
) -> Result<Prepared, String> {
    let (animated, width, height) = shape;
    let upload = client
        .upload(bytes.clone(), MediaType::Sticker, UploadOptions::default())
        .await
        .map_err(|error| error.to_string())?;
    let mut message = wa::Message {
        sticker_message: MessageField::some(wa::message::StickerMessage {
            url: Some(upload.url),
            direct_path: Some(upload.direct_path),
            media_key: Some(upload.media_key.to_vec()),
            file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
            file_sha256: Some(upload.file_sha256.to_vec()),
            file_length: Some(upload.file_length),
            mimetype: Some("image/webp".to_owned()),
            media_key_timestamp: Some(upload.media_key_timestamp),
            is_animated: Some(animated),
            width: Some(width),
            height: Some(height),
            ..Default::default()
        }),
        ..Default::default()
    };
    if let Some(context) = context {
        message.set_context_info(context);
    }
    Ok(Prepared {
        message,
        content: Content::Sticker {
            media: media(
                Some(&"image/webp".to_owned()),
                Some(bytes.len() as u64),
                Some(width),
                Some(height),
            ),
            animated,
        },
        thumbnail: None,
        bytes,
        mime: "image/webp".to_owned(),
        file_name: None,
    })
}

/// Copies a sent attachment to media storage and builds its archive row.
async fn file_outbound(
    client: &Client,
    chat: &str,
    me: &str,
    dir: &Path,
    mut prepared: Prepared,
    caption: Option<String>,
    mentions: Vec<String>,
) -> Result<(Message, Vec<u8>), String> {
    if let Some(caption) = caption.filter(|caption| !caption.trim().is_empty()) {
        match &mut prepared.content {
            Content::Image { caption: slot, .. }
            | Content::Video { caption: slot, .. }
            | Content::Document { caption: slot, .. } => *slot = Some(caption.clone()),
            _ => {}
        }
        if let Some(image) = prepared.message.image_message.as_option_mut() {
            image.caption = Some(caption.clone());
        }
        if let Some(video) = prepared.message.video_message.as_option_mut() {
            video.caption = Some(caption.clone());
        }
        if let Some(document) = prepared.message.document_message.as_option_mut() {
            document.caption = Some(caption);
        }
    }
    if !mentions.is_empty() {
        prepared.message.set_context_info(wa::ContextInfo {
            mentioned_jid: mentions.clone(),
            ..Default::default()
        });
    }
    let id = client.generate_message_id();
    let path = media_path(
        dir,
        chat,
        &id,
        &prepared.mime,
        prepared.file_name.as_deref(),
    );
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|error| error.to_string())?;
    tokio::fs::write(&path, &prepared.bytes)
        .await
        .map_err(|error| error.to_string())?;
    let mut content = prepared.content;
    if let Some(media) = content.media_mut() {
        media.path = Some(path);
    }
    let row = Message {
        id,
        chat: chat.to_owned(),
        sender: me.to_owned(),
        sender_name: None,
        from_me: true,
        timestamp: crate::util::now(),
        content,
        status: Delivery::Pending,
        delivered_at: None,
        read_at: None,
        quoted: None,
        reactions: Vec::new(),
        edited: false,
        mentions: mentions
            .into_iter()
            .filter_map(|id| {
                let user = id.split('@').next()?.to_owned();
                (!user.is_empty()).then_some(MentionRef { user, id })
            })
            .collect(),
        forwarded: false,
        thumbnail: prepared.thumbnail,
    };
    Ok((row, prepared.message.encode_to_vec()))
}

/// Decodes a history chunk off the worker thread.
fn parse_history(compressed: &[u8]) -> Result<ParsedHistory, String> {
    let mut stream = HistorySyncStream::new(compressed, MAX_DECOMPRESSED);
    let mut chats = Vec::new();
    loop {
        let conversation = match stream.next_conversation() {
            Ok(Some(conversation)) => conversation,
            Ok(None) => break,
            Err(error) => return Err(error.to_string()),
        };
        chats.push(parse_conversation(conversation));
    }
    let remainder = stream.remainder().map_err(|error| error.to_string())?;
    let push_names = remainder
        .pushnames
        .iter()
        .filter_map(|entry| Some((entry.id.clone()?, entry.pushname.clone()?)))
        .collect();
    let lids = remainder
        .phone_number_to_lid_mappings
        .iter()
        .filter_map(|entry| Some((entry.lid_jid.clone()?, entry.pn_jid.clone()?)))
        .collect();
    Ok(ParsedHistory {
        chats,
        push_names,
        lids,
        stickers: remainder.recent_stickers,
    })
}

fn parse_conversation(conversation: wa::Conversation) -> ParsedChat {
    let mut messages = Vec::new();
    let mut revoked = Vec::new();
    let mut poll_updates = Vec::new();
    let mut newest = 0;
    for entry in &conversation.messages {
        let Some(info) = entry.message.as_option() else {
            continue;
        };
        let Some(key) = info.key.as_option() else {
            continue;
        };
        let Some(id) = key.id.clone().filter(|id| !id.is_empty()) else {
            continue;
        };
        let Some(message) = info.message.as_option() else {
            continue;
        };
        let from_me = key.from_me.unwrap_or(false);
        let timestamp = info.message_timestamp.unwrap_or(0) as i64;
        newest = newest.max(timestamp);
        let base = message.get_base_message();
        if let Some(protocol) = base.protocol_message.as_option() {
            if protocol.r#type == Some(wa::message::protocol_message::Type::REVOKE)
                && let Some(target) = protocol.key.as_option().and_then(|key| key.id.clone())
            {
                revoked.push(target);
            }
            continue;
        }
        if base.reaction_message.is_set() {
            continue;
        }
        let sender = info
            .participant
            .clone()
            .or_else(|| key.participant.clone())
            .filter(|sender| !sender.is_empty())
            .or_else(|| key.remote_jid.clone());
        if let Some(update) = base.poll_update_message.as_option() {
            poll_updates.push(HistoryPollUpdate {
                id,
                sender,
                from_me,
                timestamp,
                update: update.clone(),
            });
            continue;
        }
        let Some(content) = classify(base) else {
            continue;
        };
        use wa::web_message_info::Status;
        let mut status = if from_me {
            match info.status {
                Some(Status::READ) => Delivery::Read,
                Some(Status::PLAYED) => Delivery::Played,
                Some(Status::DELIVERY_ACK) => Delivery::Delivered,
                Some(Status::SERVER_ACK) => Delivery::Sent,
                Some(Status::PENDING) => Delivery::Pending,
                Some(Status::ERROR) => Delivery::Failed,
                _ => Delivery::Sent,
            }
        } else {
            Delivery::None
        };
        // A group's individual receipts may be only a partial list. Only the
        // phone's aggregate status proves delivery/read for historical groups.
        if from_me
            && ChatKind::from_id(&conversation.id) != ChatKind::Group
            && status < Delivery::Read
        {
            if info
                .user_receipt
                .iter()
                .any(|receipt| receipt.read_timestamp.is_some())
            {
                status = Delivery::Read;
            } else if status < Delivery::Delivered
                && info
                    .user_receipt
                    .iter()
                    .any(|receipt| receipt.receipt_timestamp.is_some())
            {
                status = Delivery::Delivered;
            }
        }
        let quoted = context_of(base).and_then(|context| {
            let id = context.stanza_id.clone().filter(|id| !id.is_empty())?;
            Some(Quoted {
                mentions: Vec::new(),
                id,
                sender: context.participant.clone().unwrap_or_default(),
                sender_name: None,
                summary: context
                    .quoted_message
                    .as_option()
                    .and_then(|quoted| classify(quoted.get_base_message()))
                    .map(|content| content.summary())
                    .unwrap_or_default(),
            })
        });
        let reactions = info
            .reactions
            .iter()
            .filter_map(|reaction| {
                let text = reaction.text.clone().filter(|text| !text.is_empty())?;
                let key = reaction.key.as_option();
                let from_me = key.and_then(|key| key.from_me).unwrap_or(false);
                let who = key.and_then(|key| key.participant.clone());
                Some((who, from_me, text))
            })
            .collect();
        messages.push(ParsedMessage {
            id,
            sender,
            from_me,
            push_name: non_empty(&info.push_name),
            timestamp,
            content,
            status,
            quoted,
            reactions,
            mentions: mentioned_of(base),
            forwarded: forwarded_of(base),
            thumbnail: thumbnail_of(base),
            raw: message.encode_to_vec(),
            poll_secret: info.message_secret.clone(),
            poll_votes: info.poll_updates.clone(),
        });
    }
    let last_activity = conversation
        .conversation_timestamp
        .or(conversation.last_msg_timestamp)
        .map(|timestamp| timestamp as i64)
        .unwrap_or(0)
        .max(newest);
    use wa::conversation::EndOfHistoryTransferType as End;
    let more_on_phone = conversation
        .end_of_history_transfer_type
        .map(|end| match end {
            End::COMPLETE_BUT_MORE_MESSAGES_REMAIN_ON_PRIMARY
            | End::COMPLETE_ON_DEMAND_SYNC_BUT_MORE_MSG_REMAIN_ON_PRIMARY => true,
            End::COMPLETE_AND_NO_MORE_MESSAGE_REMAIN_ON_PRIMARY
            | End::COMPLETE_ON_DEMAND_SYNC_WITH_MORE_MSG_ON_PRIMARY_BUT_NO_ACCESS => false,
        });
    ParsedChat {
        id: conversation.id.clone(),
        name: non_empty(&conversation.display_name).or_else(|| non_empty(&conversation.name)),
        unread: conversation.unread_count,
        archived: conversation.archived,
        pinned_at: conversation.pinned.map(|when| i64::from(when) * 1000),
        muted_until: conversation.mute_end_time.map(|end| {
            // Zero explicitly clears a history mute; a wrapped -1 means
            // indefinite. Absence of the field must preserve existing state.
            (end != 0).then(|| seconds(end as i64))
        }),
        ephemeral_expiration: conversation.ephemeral_expiration,
        ephemeral_setting_timestamp: conversation.ephemeral_setting_timestamp,
        last_activity,
        pn_jid: conversation.pn_jid.clone(),
        lid_jid: conversation.lid_jid.clone(),
        more_on_phone,
        messages,
        revoked,
        poll_updates,
    }
}
/// A sticker pack zip shared in a chat, downloaded by its message references.
struct PackDownload {
    message: wa::message::StickerPackMessage,
}
impl Downloadable for PackDownload {
    fn direct_path(&self) -> Option<&str> {
        self.message.direct_path.as_deref()
    }
    fn media_key(&self) -> Option<&[u8]> {
        self.message.media_key.as_deref()
    }
    fn file_enc_sha256(&self) -> Option<&[u8]> {
        self.message.file_enc_sha256.as_deref()
    }
    fn file_sha256(&self) -> Option<&[u8]> {
        self.message.file_sha256.as_deref()
    }
    fn file_length(&self) -> Option<u64> {
        self.message.file_length
    }
    fn app_info(&self) -> MediaType {
        MediaType::StickerPack
    }
}

/// A favorite change on its way to the phone.
struct FavoritePush {
    hash: String,
    favorite: bool,
    updated_at: i64,
    /// Known CDN references, or none when the file must be uploaded first.
    action: Option<wa::sync_action_value::StickerAction>,
    /// The favorite file, uploaded when no references are known.
    file: Option<PathBuf>,
}
/// A favorite sticker the phone told us about, fetched by its references.
struct FavoriteDownload {
    action: wa::sync_action_value::StickerAction,
    file_sha256: Vec<u8>,
}
impl Downloadable for FavoriteDownload {
    fn direct_path(&self) -> Option<&str> {
        self.action.direct_path.as_deref()
    }
    fn media_key(&self) -> Option<&[u8]> {
        self.action.media_key.as_deref()
    }
    fn file_enc_sha256(&self) -> Option<&[u8]> {
        self.action.file_enc_sha256.as_deref()
    }
    fn file_sha256(&self) -> Option<&[u8]> {
        Some(&self.file_sha256)
    }
    fn file_length(&self) -> Option<u64> {
        self.action.file_length
    }
    fn app_info(&self) -> MediaType {
        MediaType::Sticker
    }
}

#[cfg(test)]
mod tests {
    use super::receipt_tests::worker;
    use super::*;
    use crate::model::MediaState;

    fn png_fixture() -> Vec<u8> {
        let mut out = Vec::new();
        let picture = image::RgbaImage::from_pixel(4, 4, image::Rgba([10, 200, 90, 255]));
        image::DynamicImage::ImageRgba8(picture)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("encodes");
        out
    }

    #[test]
    fn only_absolute_http_urls_reach_the_avatar_transport() {
        assert!(is_fetchable_avatar_url("https://cdn.example/pic.jpg"));
        assert!(is_fetchable_avatar_url("HTTP://cdn.example/pic.jpg"));
        assert!(!is_fetchable_avatar_url("/v/t61/pic.enc"));
        assert!(!is_fetchable_avatar_url("v/t61/pic.enc"));
        assert!(!is_fetchable_avatar_url(""));
        assert!(validate_avatar_bytes(&png_fixture()));
        assert!(!validate_avatar_bytes(b"not-an-image"));
        assert!(!validate_avatar_bytes(b""));
    }

    fn png_thumb_bytes() -> Vec<u8> {
        let mut out = Vec::new();
        let picture = image::RgbaImage::from_pixel(200, 100, image::Rgba([10, 200, 90, 255]));
        image::DynamicImage::ImageRgba8(picture)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("encodes");
        out
    }

    #[test]
    fn thumb_heal_backoff_grows_with_failures() {
        assert_eq!(thumb_heal_backoff(0), Duration::ZERO);
        assert_eq!(thumb_heal_backoff(1), Duration::from_secs(10));
        assert_eq!(thumb_heal_backoff(2), Duration::from_secs(60));
        assert_eq!(thumb_heal_backoff(3), Duration::from_secs(5 * 60));
        assert_eq!(thumb_heal_backoff(99), Duration::from_secs(5 * 60));
    }

    /// Reads the next internal command a blocking task sends back.
    async fn next_command(inbox: &mut mpsc::UnboundedReceiver<Command>) -> Command {
        tokio::time::timeout(Duration::from_secs(10), inbox.recv())
            .await
            .expect("task answers")
            .expect("channel open")
    }

    #[tokio::test]
    async fn thumb_heal_rebuilds_then_reports_success() {
        let (mut worker, events, mut inbox, _wa) = worker();
        let bytes = png_thumb_bytes();
        let hash = crate::stickers::hash_of(&bytes);
        let dir = std::env::temp_dir().join(format!("zapfast-thumbheal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        let file = dir.join(format!("{hash}.webp"));
        std::fs::write(&file, &bytes).expect("writes");
        worker.heal_sticker_thumb(&file);
        // The rebuild runs elsewhere: nothing is reported yet and the
        // worker thread is already free for other commands.
        assert!(
            worker
                .thumb_heals
                .get(&file)
                .is_some_and(|heal| heal.in_flight),
            "rebuild dispatched"
        );
        assert!(events.try_recv().is_err(), "no synchronous report");
        let finished = next_command(&mut inbox).await;
        assert!(
            matches!(&finished, Command::ThumbHealFinished { path, ok } if path == &file && *ok),
            "unexpected command: {finished:?}"
        );
        worker.handle_command(finished).await;
        assert!(!worker.thumb_heals.contains_key(&file));
        match events.try_recv().expect("reports success") {
            Event::StickerThumb { path, ok } => {
                assert_eq!(path, file);
                assert!(ok);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(events.try_recv().is_err(), "exactly one report");
        let thumb = crate::stickers::thumb_path(&worker.dirs.sticker_thumb_dir(), &file)
            .expect("thumb path");
        assert!(thumb.is_file(), "thumbnail rebuilt without network");
        let _ = std::fs::remove_file(&thumb);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn thumb_heal_dispatch_keeps_the_worker_answering() {
        let (mut worker, events, mut inbox, _wa) = worker();
        // Its own identity, so parallel suites never share its thumbnail.
        let mut bytes = Vec::new();
        let picture = image::RgbaImage::from_pixel(200, 100, image::Rgba([200, 30, 30, 255]));
        image::DynamicImage::ImageRgba8(picture)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("encodes");
        let hash = crate::stickers::hash_of(&bytes);
        let dir =
            std::env::temp_dir().join(format!("zapfast-thumbheal-busy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        let file = dir.join(format!("{hash}.webp"));
        std::fs::write(&file, &bytes).expect("writes");
        worker.heal_sticker_thumb(&file);
        assert!(
            worker
                .thumb_heals
                .get(&file)
                .is_some_and(|heal| heal.in_flight),
            "rebuild dispatched"
        );
        // While the rebuild runs elsewhere, other commands still process.
        let key = ("15550002222@s.whatsapp.net".to_owned(), false);
        worker
            .handle_command(Command::AvatarFailed {
                id: key.0.clone(),
                full: key.1,
            })
            .await;
        assert_eq!(worker.pending_avatars[&key].attempts, 1);
        // Then the rebuild result lands and reports success.
        let finished = next_command(&mut inbox).await;
        assert!(
            matches!(&finished, Command::ThumbHealFinished { path, ok } if path == &file && *ok),
            "unexpected command: {finished:?}"
        );
        worker.handle_command(finished).await;
        assert!(!worker.thumb_heals.contains_key(&file));
        match events.try_recv().expect("reports success") {
            Event::StickerThumb { path, ok } => {
                assert_eq!(path, file);
                assert!(ok);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(events.try_recv().is_err(), "exactly one report");
        let thumb = crate::stickers::thumb_path(&worker.dirs.sticker_thumb_dir(), &file)
            .expect("thumb path");
        assert!(thumb.is_file(), "thumbnail rebuilt without network");
        let _ = std::fs::remove_file(&thumb);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn thumb_heals_cap_two_and_answer_while_blocked() {
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        };
        use std::time::Duration;
        let (mut worker, _events, mut inbox, _wa) = worker();
        // Three due rebuilds with hash names; the files never matter
        // because the build step is injected below.
        let files: Vec<PathBuf> = (1..=3)
            .map(|n| std::env::temp_dir().join(format!("{n:064x}.webp")))
            .collect();
        for file in &files {
            worker.thumb_heals.insert(
                file.clone(),
                ThumbHeal {
                    attempts: 0,
                    next_retry: Instant::now() - Duration::from_secs(1),
                    in_flight: false,
                },
            );
        }
        // The first two builds report their entry and then hold until the
        // release sender is dropped; later builds pass straight through.
        // Timeouts turn a stall into a failure instead of a hung suite.
        let entered = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let build = {
            let entered = entered.clone();
            let release_rx = release_rx.clone();
            move |_thumbs: PathBuf, _file: PathBuf| -> Result<PathBuf, String> {
                if entered.fetch_add(1, Ordering::SeqCst) < 2 {
                    entered_tx.send(()).expect("test waits for entry");
                    // A dropped sender releases too; either way the task
                    // finishes and reports.
                    let _ = release_rx.lock().expect("receiver").recv();
                }
                Ok("done".into())
            }
        };
        worker.pump_thumb_heals_with(std::sync::Arc::new(build) as _);
        // Both tasks are inside the build step: the rendezvous proves the
        // overlap instead of hoping a decode stays slow.
        for _ in 0..2 {
            entered_rx
                .recv_timeout(Duration::from_secs(30))
                .expect("both builds entered");
        }
        let flying = worker
            .thumb_heals
            .values()
            .filter(|heal| heal.in_flight)
            .count();
        assert_eq!(flying, 2, "two rebuilds run at once");
        assert!(
            worker
                .thumb_heals
                .values()
                .filter(|heal| !heal.in_flight)
                .count()
                == 1,
            "the third request waits for a slot"
        );
        // While both builds are stuck, other commands still apply.
        let key = ("15550002222@s.whatsapp.net".to_owned(), false);
        assert!(inbox.try_recv().is_err(), "no build finished while held");
        worker
            .handle_command(Command::AvatarFailed {
                id: key.0.clone(),
                full: key.1,
            })
            .await;
        assert_eq!(worker.pending_avatars[&key].attempts, 1);
        drop(release_tx);
        // Both results land; handling them clears the finished requests.
        for _ in 0..2 {
            let finished = next_command(&mut inbox).await;
            assert!(
                matches!(&finished, Command::ThumbHealFinished { ok: true, .. }),
                "unexpected command: {finished:?}"
            );
            worker.handle_command(finished).await;
        }
        assert_eq!(worker.thumb_heals.len(), 1, "only the waiter remains");
        // The freed slots dispatch the waiter, which passes straight through.
        worker.pump_thumb_heals_with(std::sync::Arc::new(|_, _| Ok("done".into())) as _);
        let finished = next_command(&mut inbox).await;
        worker.handle_command(finished).await;
        assert!(worker.thumb_heals.is_empty(), "the waiter finished");
    }

    #[tokio::test]
    async fn thumb_heal_gives_up_after_capped_failures() {
        let (mut worker, events, mut inbox, _wa) = worker();
        // Hash-named but missing: every rebuild fails on the read.
        let dir =
            std::env::temp_dir().join(format!("zapfast-thumbheal-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        let file = dir.join(format!("{}.webp", "a".repeat(64)));
        let _ = std::fs::remove_file(&file);
        worker.heal_sticker_thumb(&file);
        // Dispatched to a task, not reported: the failure is still to come.
        assert!(
            worker
                .thumb_heals
                .get(&file)
                .is_some_and(|heal| heal.in_flight),
            "rebuild dispatched"
        );
        assert!(events.try_recv().is_err());
        // The first failure schedules a retry instead of reporting.
        let finished = next_command(&mut inbox).await;
        assert!(
            matches!(&finished, Command::ThumbHealFinished { path, ok } if path == &file && !ok),
            "unexpected command: {finished:?}"
        );
        worker.handle_command(finished).await;
        assert_eq!(worker.thumb_heals[&file].attempts, 1);
        assert!(!worker.thumb_heals[&file].in_flight);
        assert!(events.try_recv().is_err());
        // A retry before its deadline dispatches nothing.
        worker.pump_thumb_heals();
        assert_eq!(worker.thumb_heals[&file].attempts, 1);
        assert!(inbox.try_recv().is_err(), "nothing dispatched early");
        assert!(events.try_recv().is_err());
        // Failures keep scheduling retries until the cap...
        for expected in 2..=THUMB_HEAL_MAX_FAILURES {
            worker.thumb_heals.get_mut(&file).unwrap().next_retry =
                Instant::now() - Duration::from_secs(1);
            worker.pump_thumb_heals();
            let finished = next_command(&mut inbox).await;
            assert!(
                matches!(&finished, Command::ThumbHealFinished { path, ok } if path == &file && !ok),
                "unexpected command: {finished:?}"
            );
            worker.handle_command(finished).await;
            assert_eq!(worker.thumb_heals[&file].attempts, expected);
            assert!(!worker.thumb_heals[&file].in_flight);
            assert!(events.try_recv().is_err(), "no report yet");
        }
        // ...then the capped failure reports exactly once.
        worker.thumb_heals.get_mut(&file).unwrap().next_retry =
            Instant::now() - Duration::from_secs(1);
        worker.pump_thumb_heals();
        let finished = next_command(&mut inbox).await;
        worker.handle_command(finished).await;
        assert!(!worker.thumb_heals.contains_key(&file));
        match events.try_recv().expect("reports failure") {
            Event::StickerThumb { path, ok } => {
                assert_eq!(path, file);
                assert!(!ok);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(events.try_recv().is_err(), "exactly one report");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn avatar_backoff_grows_with_failures() {
        assert_eq!(avatar_backoff(0), Duration::ZERO);
        assert_eq!(avatar_backoff(1), Duration::from_secs(30));
        assert_eq!(avatar_backoff(2), Duration::from_secs(2 * 60));
        assert_eq!(avatar_backoff(3), Duration::from_secs(10 * 60));
        assert_eq!(avatar_backoff(99), Duration::from_secs(10 * 60));
    }

    #[test]
    fn avatar_retry_entries_wait_dispatch_or_give_up() {
        let now = Instant::now();
        let waiting = AvatarRetry {
            attempts: 1,
            next_retry: now + Duration::from_secs(60),
            in_flight: false,
        };
        assert_eq!(avatar_due(&waiting, now), AvatarDue::Wait);
        let flying = AvatarRetry {
            attempts: 1,
            next_retry: now - Duration::from_secs(1),
            in_flight: true,
        };
        assert_eq!(avatar_due(&flying, now), AvatarDue::Wait);
        let due = AvatarRetry {
            attempts: 2,
            next_retry: now - Duration::from_secs(1),
            in_flight: false,
        };
        assert_eq!(avatar_due(&due, now), AvatarDue::Dispatch);
        let capped = AvatarRetry {
            attempts: AVATAR_MAX_FAILURES,
            next_retry: now - Duration::from_secs(1),
            in_flight: false,
        };
        assert_eq!(avatar_due(&capped, now), AvatarDue::GiveUp);
    }

    #[tokio::test]
    async fn avatar_failures_accumulate_instead_of_restarting() {
        let (mut worker, events, _inbox, _wa) = worker();
        let id = "15550009999@s.whatsapp.net".to_owned();
        // Deferred while offline: the entry exists with nothing in flight.
        worker
            .handle_command(Command::FetchAvatar {
                id: id.clone(),
                full: false,
            })
            .await;
        let key = (id.clone(), false);
        assert_eq!(worker.pending_avatars[&key].attempts, 0);
        assert!(!worker.pending_avatars[&key].in_flight);
        // Consecutive failures accumulate instead of restarting at one, with
        // growing gaps between tries (30s, 2min, 10min).
        for expected in 1..=AVATAR_MAX_FAILURES {
            let before = Instant::now();
            worker
                .handle_command(Command::AvatarFailed {
                    id: id.clone(),
                    full: false,
                })
                .await;
            let retry = &worker.pending_avatars[&key];
            assert_eq!(retry.attempts, expected);
            assert!(!retry.in_flight);
            let backoff = avatar_backoff(expected);
            let wait = retry.next_retry.saturating_duration_since(before);
            assert!(
                wait >= backoff && wait <= backoff + Duration::from_secs(5),
                "failure {expected} waits {wait:?}, expected {backoff:?}"
            );
        }
        // Nothing reported yet: the cap decides on the next due tick.
        assert!(events.try_recv().is_err());
        // Past its deadline the capped entry is ready to report absence.
        worker.pending_avatars.get_mut(&key).unwrap().next_retry =
            Instant::now() - Duration::from_secs(1);
        assert_eq!(
            avatar_due(&worker.pending_avatars[&key], Instant::now()),
            AvatarDue::GiveUp
        );
        // A deferred fetch while offline keeps the count and deadline.
        worker
            .handle_command(Command::FetchAvatar {
                id: id.clone(),
                full: false,
            })
            .await;
        assert_eq!(worker.pending_avatars[&key].attempts, AVATAR_MAX_FAILURES);
        assert!(!worker.pending_avatars[&key].in_flight);
    }

    #[tokio::test]
    async fn avatar_download_stores_valid_images_atomically() {
        let dir = std::env::temp_dir().join(format!("zapfast-avatar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        let bytes = png_fixture();
        let path = dir.join("a.jpg");
        let fetch: AvatarFetch =
            std::sync::Arc::new(move |_: &str| Ok::<Vec<u8>, String>(bytes.clone()));
        let stored = Worker::download_and_store_avatar(
            &fetch,
            "https://cdn.example/pic.jpg".into(),
            path.clone(),
        )
        .await
        .expect("stores");
        assert_eq!(stored, path);
        assert_eq!(std::fs::read(&path).expect("reads"), png_fixture());
        assert!(
            !dir.join("a.part").exists(),
            "no staging file is left behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn avatar_download_refuses_relative_paths_without_network() {
        let dir = std::env::temp_dir().join(format!("zapfast-avatar-rel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        let path = dir.join("a.jpg");
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = called.clone();
        let fetch: AvatarFetch = std::sync::Arc::new(move |_: &str| {
            probe.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok::<Vec<u8>, String>(Vec::new())
        });
        let error =
            Worker::download_and_store_avatar(&fetch, "/v/t61/pic.enc".into(), path.clone())
                .await
                .expect_err("refuses");
        assert!(error.contains("non-absolute"), "{error}");
        assert!(
            !called.load(std::sync::atomic::Ordering::SeqCst),
            "the transport is never touched"
        );
        assert!(!path.exists(), "nothing is cached");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn avatar_download_failures_keep_the_last_good_photo() {
        let dir = std::env::temp_dir().join(format!("zapfast-avatar-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        let path = dir.join("a.jpg");
        let good = png_fixture();
        std::fs::write(&path, &good).expect("seeds");
        let bad: AvatarFetch =
            std::sync::Arc::new(|_: &str| Ok::<Vec<u8>, String>(b"not-an-image".to_vec()));
        let error = Worker::download_and_store_avatar(
            &bad,
            "https://cdn.example/pic.jpg".into(),
            path.clone(),
        )
        .await
        .expect_err("rejects");
        assert!(error.contains("readable"), "{error}");
        let down: AvatarFetch =
            std::sync::Arc::new(|_: &str| Err::<Vec<u8>, String>("boom".into()));
        Worker::download_and_store_avatar(
            &down,
            "https://cdn.example/pic.jpg".into(),
            path.clone(),
        )
        .await
        .expect_err("fails");
        assert_eq!(
            std::fs::read(&path).expect("reads"),
            good,
            "failures never overwrite the last good photo"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn avatar_give_up_keeps_the_last_good_photo() {
        let (mut worker, events, _inbox, _wa) = worker();
        let id = "15550009999@s.whatsapp.net".to_owned();
        // An older photo, as after successive update failures.
        let path = worker.avatar_file(&id, false);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("creates");
        std::fs::write(&path, png_fixture()).expect("seeds");
        worker.pending_avatars.insert(
            (id.clone(), false),
            AvatarRetry {
                attempts: AVATAR_MAX_FAILURES,
                next_retry: Instant::now() - Duration::from_secs(1),
                in_flight: false,
            },
        );
        worker.give_up_avatar(id.clone(), false);
        assert!(!worker.pending_avatars.contains_key(&(id.clone(), false)));
        match events.try_recv().expect("reports the kept photo") {
            Event::Avatar {
                id: got,
                full,
                path: kept,
            } => {
                assert_eq!(got, id);
                assert!(!full);
                assert_eq!(kept, Some(path.clone()));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(events.try_recv().is_err(), "exactly one report");
        assert_eq!(
            std::fs::read(&path).expect("reads"),
            png_fixture(),
            "the file is untouched"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn avatar_give_up_reports_absence_without_a_cached_photo() {
        let (mut worker, events, _inbox, _wa) = worker();
        let id = "15550008888@s.whatsapp.net".to_owned();
        let path = worker.avatar_file(&id, false);
        let _ = std::fs::remove_file(&path);
        worker.pending_avatars.insert(
            (id.clone(), false),
            AvatarRetry {
                attempts: AVATAR_MAX_FAILURES,
                next_retry: Instant::now() - Duration::from_secs(1),
                in_flight: false,
            },
        );
        worker.give_up_avatar(id.clone(), false);
        match events.try_recv().expect("reports absence") {
            Event::Avatar {
                id: got,
                full,
                path: kept,
            } => {
                assert_eq!(got, id);
                assert!(!full);
                assert_eq!(kept, None);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(events.try_recv().is_err(), "exactly one report");
    }
    #[tokio::test]
    async fn avatar_late_success_after_give_up_still_shows() {
        let (mut worker, events, _inbox, _wa) = worker();
        let id = "15550007777@s.whatsapp.net".to_owned();
        let path = worker.avatar_file(&id, false);
        let _ = std::fs::remove_file(&path);
        worker.pending_avatars.insert(
            (id.clone(), false),
            AvatarRetry {
                attempts: AVATAR_MAX_FAILURES,
                next_retry: Instant::now() - Duration::from_secs(1),
                in_flight: false,
            },
        );
        worker.give_up_avatar(id.clone(), false);
        assert!(events.try_recv().is_ok(), "absence reported");
        // A late download that finally lands is still a success: only
        // success, proven absence, or explicit removal clears a photo.
        worker
            .handle_command(Command::AvatarFetched {
                id: id.clone(),
                full: false,
                path: Some(path.clone()),
            })
            .await;
        match events.try_recv().expect("reports the late photo") {
            Event::Avatar {
                id: got,
                path: kept,
                ..
            } => {
                assert_eq!(got, id);
                assert_eq!(kept, Some(path));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(events.try_recv().is_err(), "exactly one report");
    }
    #[test]
    fn avatar_fetch_policy_locks_supported_formats() {
        // Only absolute HTTP(S) URLs may reach a plain HTTP client:
        // newsletter metadata carries CDN direct paths, which need
        // media-host auth the app does not negotiate.
        assert!(is_fetchable_avatar_url(
            "https://pps.whatsapp.net/photo.jpg"
        ));
        assert!(is_fetchable_avatar_url("http://pps.whatsapp.net/photo.jpg"));
        assert!(!is_fetchable_avatar_url("/media/direct/path"));
        assert!(!is_fetchable_avatar_url("mmg-fna.whatsapp.net/photo.jpg"));
        assert!(!is_fetchable_avatar_url(""));
        // Only non-empty readable images may mark the cache ready.
        assert!(validate_avatar_bytes(&png_fixture()));
        assert!(!validate_avatar_bytes(&[]));
        assert!(!validate_avatar_bytes(b"not a picture"));
    }
    fn phone_sticker(
        hash: &str,
        path: Option<PathBuf>,
        last_used: i64,
    ) -> crate::archive::PhoneSticker {
        crate::archive::PhoneSticker {
            hash: hash.to_owned(),
            raw: Vec::new(),
            last_used,
            path,
        }
    }
    #[tokio::test]
    async fn rapid_favorite_changes_collapse_to_one_unpushed_intent() {
        let (mut worker, _events, _inbox, _wa) = worker();
        let dir = worker.dirs.saved_sticker_dir();
        std::fs::create_dir_all(&dir).expect("creates");
        let file = dir.join("loose.webp");
        std::fs::write(&file, b"sun").expect("writes");
        let hash = crate::stickers::hash_of(b"sun");
        worker.toggle_favorite_sticker(&file);
        worker.toggle_favorite_sticker(&file);
        worker.toggle_favorite_sticker(&file);
        let stored = worker
            .archive
            .favorite_sticker(&hash)
            .expect("reads")
            .expect("row");
        assert!(stored.favorite, "three quick taps end favorited");
        assert!(!stored.pushed, "nothing told the phone yet");
        let waiting = worker.archive.unpushed_favorite_stickers().expect("lists");
        assert_eq!(waiting.len(), 1, "one intent, not three");
        assert_eq!(waiting[0].0, hash);
        worker.push_favorites();
        assert!(
            !worker.favorites_pushing,
            "offline pushes wait instead of failing"
        );
        let waiting = worker.archive.unpushed_favorite_stickers().expect("lists");
        assert_eq!(
            waiting.len(),
            1,
            "the intent survives for the next connection"
        );
    }
    #[tokio::test]
    async fn a_newer_local_change_beats_an_older_phone_change() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker
            .archive
            .set_favorite_sticker("aa", true, 30, None, false)
            .expect("stores");
        worker.apply_phone_favorite("aa", false, 10, None);
        let stored = worker
            .archive
            .favorite_sticker("aa")
            .expect("reads")
            .expect("row");
        assert!(
            stored.favorite && !stored.pushed,
            "the newer local change stands"
        );
        worker.apply_phone_favorite("aa", false, 40, None);
        let stored = worker
            .archive
            .favorite_sticker("aa")
            .expect("reads")
            .expect("row");
        assert!(
            !stored.favorite && stored.pushed,
            "the newer phone change applies"
        );
    }
    #[test]
    fn path_favorites_migrate_to_one_hash_and_follow_copies() {
        let (mut worker, _events, _inbox, _wa) = worker();
        let pack_dir = worker.packs_dir().join("frogs");
        std::fs::create_dir_all(&pack_dir).expect("creates");
        let cache = worker.dirs.sticker_cache_dir();
        std::fs::create_dir_all(&cache).expect("creates");
        let packed = pack_dir.join("000.webp");
        let cached = cache.join("msg-1.webp");
        std::fs::write(&packed, b"sun").expect("writes");
        std::fs::write(&cached, b"sun").expect("writes");
        let legacy = serde_json::to_string(&vec![packed.clone(), cached.clone()]).expect("json");
        worker
            .archive
            .set_meta("sticker_favorites", &legacy)
            .expect("seeds");
        worker.migrate_sticker_favorites();
        let hashes = worker.favorite_hashes();
        assert_eq!(hashes.len(), 1, "one picture is one favorite");
        let hash = &hashes[0];
        assert_eq!(hash, &crate::stickers::hash_of(b"sun"));
        std::fs::remove_file(&cached).expect("clears the cache copy");
        let resolved = worker.resolve_favorite(hash, &worker.sticker_packs(), &[]);
        let adopted = pack_dir.join(format!("{hash}.webp"));
        assert_eq!(
            resolved,
            Some(adopted),
            "the pack copy keeps the favorite alive"
        );
        worker.migrate_sticker_favorites();
        assert_eq!(worker.favorite_hashes().len(), 1, "migration runs once");
    }
    #[tokio::test]
    async fn emit_lists_one_favorite_with_pack_remainder_and_emojis() {
        let (mut worker, events_rx, _inbox, _wa) = worker();
        let pack_dir = worker.packs_dir().join("frogs");
        std::fs::create_dir_all(&pack_dir).expect("creates");
        let webp_of = |seed: u8| {
            let mut out = Vec::new();
            let picture = image::RgbaImage::from_pixel(8, 8, image::Rgba([seed, 20, 30, 255]));
            image::codecs::webp::WebPEncoder::new_lossless(&mut out)
                .encode(&picture, 8, 8, image::ExtendedColorType::Rgba8)
                .expect("encodes");
            out
        };
        let plain_a = webp_of(10);
        let info = crate::sticker_meta::StickerInfo {
            pack_name: "frogs".into(),
            emojis: vec!["\u{1F438}".into()],
            ..Default::default()
        };
        let tagged_a = crate::sticker_meta::write(&plain_a, &info).expect("tags");
        let plain_b = webp_of(20);
        std::fs::write(pack_dir.join("000.webp"), &tagged_a).expect("writes");
        std::fs::write(pack_dir.join("001.webp"), &plain_b).expect("writes");
        let hash_a = crate::stickers::hash_of(&tagged_a);
        let hash_b = crate::stickers::hash_of(&plain_b);
        worker
            .archive
            .set_favorite_sticker(&hash_a, true, 50, None, false)
            .expect("stores");
        worker.emit_stickers();
        let mut seen = None;
        while let Ok(event) = events_rx.try_recv() {
            if let Event::Stickers {
                favorites,
                packs,
                emojis,
                ..
            } = event
            {
                seen = Some((favorites, packs, emojis));
            }
        }
        let (favorites, packs, emojis) = seen.expect("stickers emitted");
        assert_eq!(favorites, vec![pack_dir.join(format!("{hash_a}.webp"))]);
        assert_eq!(packs.len(), 1);
        assert_eq!(
            packs[0].stickers,
            vec![pack_dir.join(format!("{hash_b}.webp"))]
        );
        assert_eq!(
            emojis
                .iter()
                .find(|(path, _)| path.ends_with(format!("{hash_a}.webp")))
                .map(|(_, tags)| tags.clone()),
            Some(vec!["\u{1F438}".to_owned()])
        );
    }
    #[tokio::test]
    async fn a_viewed_pack_can_be_kept() {
        let (mut worker, _events_rx, _inbox, _wa) = worker();
        let shared = worker.dirs.sticker_cache_dir().join("shared").join("abc");
        std::fs::create_dir_all(&shared).expect("creates");
        let mut out = Vec::new();
        let picture = image::RgbaImage::from_pixel(8, 8, image::Rgba([30, 20, 30, 255]));
        image::codecs::webp::WebPEncoder::new_lossless(&mut out)
            .encode(&picture, 8, 8, image::ExtendedColorType::Rgba8)
            .expect("encodes");
        std::fs::write(shared.join("000.webp"), &out).expect("writes");
        worker.add_sticker_pack(&shared, "Ducks");
        let kept = worker.packs_dir().join("Ducks");
        assert!(
            kept.join(format!("{}.webp", crate::stickers::hash_of(&out)))
                .is_file(),
            "the sticker is hash-filed"
        );
        assert!(
            kept.join(crate::stickers::PACK_MANIFEST).is_file(),
            "the manifest travels"
        );
        let packs = worker.sticker_packs();
        assert!(
            packs
                .iter()
                .any(|pack| pack.dir == kept && pack.stickers.len() == 1)
        );
    }

    #[test]
    fn a_cached_sticker_that_is_gone_is_fetched_again_and_stuck_ones_wait() {
        let dir = std::env::temp_dir().join(format!("zapfast-stickers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let kept = dir.join("kept.webp");
        std::fs::write(&kept, b"webp").expect("writes");
        let gone = dir.join("gone.webp");
        std::fs::write(&gone, b"webp").expect("writes");
        std::fs::remove_file(&gone).expect("removes");
        let list = vec![
            phone_sticker("kept", Some(kept), 3),
            phone_sticker("gone", Some(gone), 2),
            phone_sticker("fresh", None, 1),
        ];

        // A recorded file that is still there is left alone; the one whose
        // copy disappeared (a cleared cache) is fetched again.
        let picked = stickers_to_fetch(list.clone(), &HashSet::new(), &HashMap::new());
        let hashes: Vec<String> = picked.into_iter().map(|sticker| sticker.hash).collect();
        assert_eq!(hashes, vec!["gone".to_owned(), "fresh".to_owned()]);

        // One already in flight, and one that used up its tries, are skipped.
        let busy: HashSet<String> = ["gone".to_owned()].into_iter().collect();
        let picked = stickers_to_fetch(list.clone(), &busy, &HashMap::new());
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].hash, "fresh");
        let tries = HashMap::from([("fresh".to_owned(), STICKER_TRIES)]);
        let picked = stickers_to_fetch(list.clone(), &HashSet::new(), &tries);
        assert_eq!(
            picked
                .into_iter()
                .map(|sticker| sticker.hash)
                .collect::<Vec<_>>(),
            vec!["gone".to_owned()]
        );

        // A round never asks for a whole library at once.
        let many: Vec<_> = (0..40)
            .map(|index| phone_sticker(&format!("h{index}"), None, i64::from(index)))
            .collect();
        assert_eq!(
            stickers_to_fetch(many, &HashSet::new(), &HashMap::new()).len(),
            STICKER_ROUND
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_transient_download_failures_are_repeated() {
        assert!(retriable_download("connection reset by peer"));
        assert!(retriable_download("request timed out"));
        assert!(retriable_download("media could not be decrypted"));
        // Expired media is final: the phone was already asked to re-upload.
        assert!(!retriable_download(
            "No longer available on WhatsApp's servers"
        ));
        assert!(!retriable_download("HTTP 403"));
        assert!(!retriable_download("status 404"));
        assert!(!retriable_download("gone: 410"));
        assert!(!retriable_download("Attachment download keys are missing"));
    }

    #[test]
    fn empty_and_unreadable_downloads_are_rejected_before_filing() {
        assert!(Worker::validate_media_bytes(&[], "image/webp").is_err());
        assert!(Worker::validate_media_bytes(b"not a picture", "image/webp").is_err());
        assert!(Worker::validate_media_bytes(b"not a picture", "image/jpeg").is_err());
        // Non-image attachments are not the picture pipeline's business.
        assert!(Worker::validate_media_bytes(b"not a picture", "application/pdf").is_ok());
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(4, 4)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encodes");
        assert!(Worker::validate_media_bytes(&png, "image/png").is_ok());
    }

    #[test]
    fn five_hops_make_a_frequently_forwarded_message() {
        assert!(!is_frequently_forwarded(0));
        assert!(!is_frequently_forwarded(4));
        assert!(is_frequently_forwarded(5));
        assert!(is_frequently_forwarded(127));
    }

    #[test]
    fn forward_destinations_keep_five_chats_or_one_for_viral() {
        let chats: Vec<ChatId> = (0..7).map(|n| format!("{n}@s.whatsapp.net")).collect();
        let kept = cap_destinations(chats.clone(), false);
        assert_eq!(kept.len(), 5);
        assert_eq!(kept, chats[..5].to_vec(), "order is kept");
        assert_eq!(cap_destinations(chats, true).len(), 1);
        let dups = vec!["a@s.whatsapp.net".to_owned(), "a@s.whatsapp.net".to_owned()];
        assert_eq!(cap_destinations(dups, false).len(), 1);
    }

    #[test]
    fn healing_a_broken_copy_clears_it_for_redownload() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker
            .archive
            .ensure_chat("a@s.whatsapp.net", "A")
            .expect("chat");
        let dir = std::env::temp_dir().join(format!("zapfast-heal-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let broken = dir.join("broken.webp");
        std::fs::write(&broken, b"not a picture").expect("writes");
        let message = Message {
            id: "s1".into(),
            chat: "a@s.whatsapp.net".into(),
            sender: "a@s.whatsapp.net".into(),
            sender_name: None,
            from_me: false,
            timestamp: 10,
            content: Content::Sticker {
                media: Media {
                    mime: "image/webp".into(),
                    size: 13,
                    width: Some(512),
                    height: Some(512),
                    path: Some(broken.clone()),
                    state: MediaState::Idle,
                },
                animated: false,
            },
            status: Delivery::None,
            delivered_at: None,
            read_at: None,
            quoted: None,
            reactions: Vec::new(),
            edited: false,
            mentions: Vec::new(),
            forwarded: false,
            thumbnail: None,
        };
        worker
            .archive
            .insert_message(&message, None)
            .expect("inserted");
        worker.heal_sticker(&broken);
        assert!(!broken.exists(), "the broken copy is gone");
        let stored = worker
            .archive
            .message("a@s.whatsapp.net", "s1")
            .expect("reads")
            .expect("row");
        assert!(
            matches!(&stored.content, Content::Sticker { media, .. } if media.path.is_none()),
            "the archive record is cleared so the worker downloads it again"
        );
        // Unknown files are ignored instead of erroring.
        worker.heal_sticker(std::path::Path::new("/definitely/not/here.webp"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fallback_names_read_as_phones_or_ids() {
        assert_eq!(
            fallback_name("393331234567@s.whatsapp.net"),
            "+39 333 123 456 7"
        );
        assert_eq!(fallback_name("1-2@g.us"), "Group");
        assert_eq!(fallback_name("42@lid"), "42");
    }

    #[test]
    fn media_paths_keep_document_names_and_map_mimes() {
        let dir = Path::new("/cache");
        assert_eq!(
            media_path(dir, "1@s.whatsapp.net", "ABC", "image/jpeg", None),
            PathBuf::from("/cache/1_s_whatsapp_net-ABC.jpg")
        );
        assert_eq!(
            media_path(
                dir,
                "1@s.whatsapp.net",
                "ABC",
                "application/pdf",
                Some("tax return.pdf")
            ),
            PathBuf::from("/cache/ABC-tax_return.pdf")
        );
        assert_eq!(extension_for("audio/ogg; codecs=opus", None), "ogg");
        assert_eq!(extension_for("application/x-unknown", None), "x-unknown");
    }

    #[test]
    fn classification_covers_text_and_media() {
        let text = wa::Message::text("hello");
        assert_eq!(classify(&text), Some(Content::text("hello")));
        let image = wa::Message {
            image_message: whatsapp_rust::prelude::MessageField::some(wa::message::ImageMessage {
                caption: Some("look".into()),
                mimetype: Some("image/jpeg".into()),
                file_length: Some(10),
                width: Some(4),
                height: Some(3),
                jpeg_thumbnail: Some(vec![0xff, 0xd8]),
                ..Default::default()
            }),
            ..Default::default()
        };
        match classify(&image) {
            Some(Content::Image { caption, media }) => {
                assert_eq!(caption.as_deref(), Some("look"));
                assert_eq!(media.mime, "image/jpeg");
                assert_eq!((media.width, media.height), (Some(4), Some(3)));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(thumbnail_of(&image), Some(vec![0xff, 0xd8]));
        assert_eq!(classify(&wa::Message::default()), None);
    }
    #[test]
    fn a_shared_sticker_pack_reads_as_a_pack_in_the_chat() {
        let message = wa::Message {
            sticker_pack_message: MessageField::some(wa::message::StickerPackMessage {
                name: Some("Ducks".into()),
                publisher: Some("Ada".into()),
                caption: Some("  ".into()),
                stickers: vec![Default::default(); 3],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            classify(&message),
            Some(Content::StickerPack {
                name: "Ducks".into(),
                publisher: "Ada".into(),
                count: 3,
                caption: None,
            })
        );
    }

    #[test]
    fn link_previews_and_mentions_come_from_extended_text() {
        let message = wa::Message {
            extended_text_message: whatsapp_rust::prelude::MessageField::some(
                wa::message::ExtendedTextMessage {
                    text: Some("see spotifast.rocks @123456@lid".into()),
                    matched_text: Some("https://spotifast.rocks/".into()),
                    title: Some("spotifast.rocks".into()),
                    description: Some("Spotify, native and fast".into()),
                    context_info: whatsapp_rust::prelude::MessageField::some(wa::ContextInfo {
                        mentioned_jid: vec!["123456@lid".into()],
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ),
            ..Default::default()
        };
        match classify(&message) {
            Some(Content::Text { preview, .. }) => {
                let preview = preview.expect("preview");
                assert_eq!(preview.url, "https://spotifast.rocks/");
                assert_eq!(preview.title.as_deref(), Some("spotifast.rocks"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(mentioned_of(&message), vec!["123456@lid".to_owned()]);
    }

    #[test]
    fn outgoing_mentions_share_context_with_a_quote() {
        let mentions = vec!["491702222222@s.whatsapp.net".to_owned()];
        let message = outgoing_text(
            "hello @491702222222".to_owned(),
            Some(wa::ContextInfo {
                stanza_id: Some("quoted".to_owned()),
                ..Default::default()
            }),
            &mentions,
        );

        assert_eq!(message.text_content(), Some("hello @491702222222"));
        let context = context_of(&message).expect("text context");
        assert_eq!(context.stanza_id.as_deref(), Some("quoted"));
        assert_eq!(context.mentioned_jid, mentions);
    }

    #[test]
    fn missing_or_disabled_expiration_leaves_message_normal() {
        for expiration in [None, Some(0)] {
            let mut message = wa::Message::text("hello");
            assert_eq!(apply_ephemeral_expiration(&mut message, expiration), None);
            assert_eq!(message.get_ephemeral_expiration(), None);
        }
    }

    #[test]
    fn configured_expiration_is_added_to_text() {
        for expiration in [86_400, 604_800, 7_776_000] {
            let mut message = wa::Message::text("hello");
            assert_eq!(
                apply_ephemeral_expiration(&mut message, Some(expiration)),
                Some(expiration)
            );
            assert_eq!(message.get_ephemeral_expiration(), Some(expiration));
        }
    }

    #[test]
    fn ephemeral_reply_preserves_quote_context() {
        let mut message = outgoing_text(
            "reply".to_owned(),
            Some(wa::ContextInfo {
                stanza_id: Some("quoted".to_owned()),
                ..Default::default()
            }),
            &[],
        );

        apply_ephemeral_expiration(&mut message, Some(604_800));

        let context = context_of(&message).expect("context");
        assert_eq!(context.stanza_id.as_deref(), Some("quoted"));
        assert_eq!(context.expiration, Some(604_800));
    }

    #[test]
    fn ephemeral_media_preserves_caption() {
        let mut message = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                caption: Some("look".to_owned()),
                ..Default::default()
            }),
            ..Default::default()
        };

        apply_ephemeral_expiration(&mut message, Some(7_776_000));

        let image = message.image_message.as_option().expect("image");
        assert_eq!(image.caption.as_deref(), Some("look"));
        assert_eq!(
            image
                .context_info
                .as_option()
                .and_then(|info| info.expiration),
            Some(7_776_000)
        );
    }

    #[test]
    fn forwards_use_only_the_destination_timer() {
        let context = wa::ContextInfo {
            expiration: Some(7_776_000),
            ephemeral_setting_timestamp: Some(123),
            ephemeral_shared_secret: Some(vec![1, 2, 3]),
            is_forwarded: Some(true),
            forwarding_score: Some(2),
            ..Default::default()
        };
        let text = wa::Message::text_with_context("forward me", context.clone());
        let image = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                caption: Some("caption".into()),
                direct_path: Some("/media/path".into()),
                context_info: MessageField::some(context.clone()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let contacts = wa::Message {
            contacts_array_message: MessageField::some(wa::message::ContactsArrayMessage {
                context_info: MessageField::some(context),
                ..Default::default()
            }),
            ..Default::default()
        };
        for original in [text, image, contacts] {
            for timer in [None, Some(0), Some(86_400)] {
                let expected = timer.filter(|value| *value > 0);
                let (forward, expiration) = outgoing_forward(&original, timer);
                assert_eq!(expiration, expected);
                assert_eq!(forward.get_ephemeral_expiration(), expected);
                let context = context_of(&forward).unwrap();
                assert_eq!(context.expiration, expected);
                assert_eq!(context.ephemeral_setting_timestamp, None);
                assert_eq!(context.ephemeral_shared_secret, None);
                assert_eq!(context.is_forwarded, Some(true));
                assert_eq!(context.forwarding_score, Some(3));
                if let Some(image) = forward.image_message.as_option() {
                    assert_eq!(image.caption.as_deref(), Some("caption"));
                    assert_eq!(image.direct_path.as_deref(), Some("/media/path"));
                }
                assert_eq!(original.get_ephemeral_expiration(), Some(7_776_000));
            }
        }
    }

    #[test]
    fn forwarded_rows_keep_content_but_reset_conversation_state() {
        let source = Message {
            id: "source".into(),
            chat: "one@s.whatsapp.net".into(),
            sender: "one@s.whatsapp.net".into(),
            sender_name: Some("Ada".into()),
            from_me: false,
            timestamp: 10,
            content: Content::text("hello"),
            status: Delivery::Read,
            delivered_at: Some(11),
            read_at: Some(12),
            quoted: Some(Quoted {
                id: "quoted".into(),
                sender: "two@s.whatsapp.net".into(),
                sender_name: Some("Bob".into()),
                summary: "earlier".into(),
                mentions: Vec::new(),
            }),
            reactions: vec![Reaction {
                sender: "two@s.whatsapp.net".into(),
                from_me: false,
                emoji: "👍".into(),
            }],
            edited: true,
            mentions: Vec::new(),
            forwarded: false,
            thumbnail: Some(vec![1]),
        };
        let mention = MentionRef {
            user: "3".into(),
            id: "3@s.whatsapp.net".into(),
        };

        let forwarded = forwarded_row(
            source,
            "target@g.us".into(),
            "me@s.whatsapp.net".into(),
            "new".into(),
            20,
            vec![mention.clone()],
            Some(vec![2]),
        );

        assert_eq!(forwarded.id, "new");
        assert_eq!(forwarded.chat, "target@g.us");
        assert_eq!(forwarded.sender, "me@s.whatsapp.net");
        assert!(forwarded.from_me && forwarded.forwarded);
        assert_eq!(forwarded.timestamp, 20);
        assert_eq!(forwarded.status, Delivery::Pending);
        assert!(forwarded.delivered_at.is_none() && forwarded.read_at.is_none());
        assert!(forwarded.quoted.is_none() && forwarded.reactions.is_empty());
        assert!(!forwarded.edited);
        assert_eq!(forwarded.mentions, vec![mention]);
        assert_eq!(forwarded.thumbnail, Some(vec![2]));
        assert_eq!(forwarded.content, Content::text("hello"));
    }

    #[test]
    fn pictures_get_a_thumbnail_and_a_jpeg_body() {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            300,
            200,
            image::Rgba([200, 30, 30, 255]),
        ));
        let jpeg = encode_jpeg(&image, 80).expect("encodes");
        assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
        let thumbnail = thumbnail_jpeg(&image).expect("thumbnail");
        let small = image::load_from_memory(&thumbnail).expect("decodes");
        assert!(small.width() <= THUMBNAIL_SIDE && small.height() <= THUMBNAIL_SIDE);
    }

    #[test]
    fn millisecond_timestamps_are_normalised() {
        assert_eq!(seconds(1_700_000_000), 1_700_000_000);
        assert_eq!(seconds(1_700_000_000_000), 1_700_000_000);
        assert_eq!(seconds(-1), 0);
    }
}

#[cfg(test)]
mod receipt_tests {
    use super::*;
    use crate::model::{Content, Delivery, Message};

    const ME: &str = "15550001111@s.whatsapp.net";
    const PEER: &str = "4917663430455@s.whatsapp.net";
    const PEER_LID: &str = "167650256810092@lid";

    /// Creates a test worker with an in-memory archive and open channels.
    #[test]
    fn group_questions_wait_in_line() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker
            .archive
            .ensure_chat("1-1@g.us", "Group")
            .expect("chat");
        worker
            .archive
            .ensure_chat("2-2@g.us", "Group")
            .expect("chat");
        worker.request_group_info("1-1@g.us", false);
        worker.request_group_info("2-2@g.us", false);
        worker.request_group_info("1-1@g.us", false);
        assert_eq!(worker.group_info_queue.len(), 2, "asked once each");
        // Forced requests go to the front.
        worker.request_group_info("1-1@g.us", true);
        assert_eq!(
            worker.group_info_queue.front().map(String::as_str),
            Some("1-1@g.us")
        );
        assert_eq!(
            worker.group_info_queue.len(),
            2,
            "forcing replaces the older entry instead of duplicating it"
        );
        // Without a client, processing schedules a retry.
        worker.pump_group_info();
        assert!(worker.group_info_queue.is_empty() || worker.group_info_retry.len() >= 2);
        // Permanent failures are not requeued.
        worker.group_info_retry.clear();
        worker.handle_failed_group("gone@g.us".to_owned(), true);
        assert!(worker.group_info_retry.is_empty());
        // Retry transient failures after their delay.
        worker.handle_failed_group("busy@g.us".to_owned(), false);
        assert_eq!(worker.group_info_retry.len(), 1);
        assert_eq!(worker.group_info_tries.get("busy@g.us"), Some(&1));
    }

    pub(super) fn worker() -> (
        Worker,
        std::sync::mpsc::Receiver<Event>,
        mpsc::UnboundedReceiver<Command>,
        mpsc::UnboundedReceiver<Arc<wa_events::Event>>,
    ) {
        let (events, events_rx) = std::sync::mpsc::channel();
        let (commands, inbox) = mpsc::unbounded_channel();
        let (wa_sender, wa_events) = mpsc::unbounded_channel();
        // A shared process directory lets parallel avatar/cache tests see
        // each other's files. Keep a unique synthetic root for each worker.
        let root = tempfile::Builder::new()
            .prefix("zapfast-worker-test-")
            .tempdir()
            .expect("isolated worker directory")
            .keep();
        let worker = Worker {
            dirs: AppDirs::under(&root),
            events,
            commands,
            waker: Waker(Arc::new(std::sync::Mutex::new(None))),
            archive: Archive::in_memory().expect("archive"),
            client: None,
            handle: None,
            wa_sender,
            me_pn: Some(ME.to_owned()),
            me_lid: None,
            me_name: None,
            me_about: None,
            lid_to_pn: HashMap::new(),
            contacts: HashMap::new(),
            status: LinkStatus::Connected,
            pairing_phone: None,
            pair_code: None,
            qr: None,
            syncing: false,
            sync_deadline: None,
            group_info_requested: HashSet::new(),
            group_info_queue: std::collections::VecDeque::new(),
            group_info_tries: HashMap::new(),
            group_info_retry: Vec::new(),
            presence_subscribed: HashSet::new(),
            pending_older: HashMap::new(),
            older_warned: HashSet::new(),
            pending_avatars: HashMap::new(),
            sticker_fetches: HashSet::new(),
            sticker_downloads: HashSet::new(),
            sticker_give_up: HashSet::new(),
            download_retries: HashMap::new(),
            update_checker: crate::updates::Checker::new(),
            link_watch: Default::default(),
            sync_attempts: HashMap::new(),
            sync_in_flight: HashMap::new(),
            sync_aliases: HashMap::new(),
            sync_dispatched: HashMap::new(),
            search_generation: 0,
            search_in_flight: 0,
            search_pending: None,
            sync_retry_at: HashMap::new(),
            media_gc: Vec::new(),
            media_gc_retry: Vec::new(),
            #[cfg(any(test, feature = "demo"))]
            sync_sink: None,
            inflight_downloads: HashSet::new(),
            download_slots: Arc::new(tokio::sync::Semaphore::new(DOWNLOAD_SLOTS)),
            sticker_tries: HashMap::new(),
            favorites_pushing: false,
            favorites_again: false,
            favorite_fetches: HashSet::new(),
            emoji_cache: HashMap::new(),
            favorites_migrated: false,
            thumb_tries: HashMap::new(),
            thumb_heals: HashMap::new(),
            cache_swept: false,
            pdf: Arc::new(std::sync::Mutex::new(crate::pdf::Reader::default())),
            pdf_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            read_sync: ReadSync::default(),
            poll_decrypting: 0,
            poll_history: Default::default(),
            poll_sending: HashSet::new(),
        };
        (worker, events_rx, inbox, wa_events)
    }

    fn drain_pages(events: &std::sync::mpsc::Receiver<Event>) -> Vec<(ChatId, Vec<String>)> {
        let mut pages = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let Event::Messages { chat, messages, .. } = event {
                pages.push((chat, messages.iter().map(|row| row.id.clone()).collect()));
            }
        }
        pages
    }
    #[test]
    fn an_old_privacy_id_reads_as_its_number_then_moves_there() {
        let (mut worker, events, _inbox, _wa) = worker();
        // One row filed before its privacy mapping was known.
        worker.archive.ensure_chat(PEER_LID, "").expect("old row");
        let mut old = own_message("old", 10);
        old.chat = PEER_LID.into();
        old.from_me = false;
        old.sender = PEER_LID.into();
        worker.archive.insert_message(&old, None).expect("insert");
        worker.archive.ensure_chat(PEER, "Peer").expect("new row");
        let mut new = own_message("new", 20);
        new.chat = PEER.into();
        worker.archive.insert_message(&new, None).expect("insert");
        // The mapping is known but nothing moved yet: the asked id still
        // answers with both rows.
        worker
            .lid_to_pn
            .insert("167650256810092".into(), "4917663430455".into());
        worker.load_chat(PEER.into(), None);
        assert_eq!(
            drain_pages(&events),
            vec![(PEER.into(), vec!["old".to_owned(), "new".to_owned()])],
            "both ids read as one chat"
        );
        // Learning the mapping moves the old row under the number, and the
        // startup preload then fills the chat in one page.
        worker.lid_to_pn.remove("167650256810092");
        worker.learn_lid("167650256810092", "4917663430455");
        assert!(
            worker
                .archive
                .messages(PEER_LID, None, 10)
                .expect("old")
                .is_empty()
        );
        worker.preload_recent();
        assert_eq!(
            drain_pages(&events),
            vec![(PEER.into(), vec!["old".to_owned(), "new".to_owned()])],
            "one preloaded page for one chat"
        );
    }
    fn own_message(id: &str, timestamp: i64) -> Message {
        Message {
            id: id.into(),
            chat: PEER.into(),
            sender: ME.into(),
            sender_name: None,
            from_me: true,
            timestamp,
            content: Content::text("hi"),
            status: Delivery::Sent,
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

    fn receipt(chat: &str, ids: &[&str], kind: ReceiptType) -> wa_events::Receipt {
        let chat: Jid = chat.parse().expect("jid");
        wa_events::Receipt::builder()
            .message_ids(ids.iter().map(|id| (*id).to_owned()).collect())
            .source(MessageSource {
                chat: chat.clone(),
                sender: chat,
                ..Default::default()
            })
            .timestamp(whatsapp_rust::wacore::time::now_utc())
            .r#type(kind)
            .offline(false)
            .build()
    }

    #[test]
    fn group_checks_wait_for_every_recipient_and_do_not_read_earlier_messages() {
        let (mut worker, _events, _inbox, _wa) = worker();
        let group = "123-456@g.us";
        let other = "12025550123@s.whatsapp.net";
        worker.archive.ensure_chat(group, "Group").unwrap();
        worker
            .archive
            .set_group_info(
                group,
                None,
                &[ME.into(), PEER_LID.into(), other.into()],
                false,
            )
            .unwrap();
        for (id, timestamp) in [("old", 100), ("new", 200)] {
            worker.store_message(
                Message {
                    chat: group.into(),
                    ..own_message(id, timestamp)
                },
                None,
                None,
            );
            assert!(worker.save_group_recipients(
                group,
                id,
                &[ME.into(), PEER_LID.into(), other.into()]
            ));
        }
        let send = |worker: &mut Worker, sender: &str, kind| {
            let mut receipt = receipt(group, &["new"], kind);
            receipt.source.sender = sender.parse().unwrap();
            receipt.source.is_group = true;
            worker.on_receipt(&receipt);
        };
        let status =
            |worker: &Worker, id| worker.archive.message(group, id).unwrap().unwrap().status;
        send(&mut worker, PEER_LID, ReceiptType::Read);
        send(&mut worker, ME, ReceiptType::Read);
        send(&mut worker, "12025550999@s.whatsapp.net", ReceiptType::Read);
        assert_eq!(status(&worker, "new"), Delivery::Sent);
        // A new alias or device is not another reader. Learning a mapping after
        // the first receipt must also merge its saved audience entry.
        worker.learn_lid("167650256810092", "4917663430455");
        send(&mut worker, PEER, ReceiptType::Read);
        send(
            &mut worker,
            "4917663430455:2@s.whatsapp.net",
            ReceiptType::Read,
        );
        assert_eq!(status(&worker, "new"), Delivery::Sent);
        send(&mut worker, other, ReceiptType::Delivered);
        assert_eq!(status(&worker, "new"), Delivery::Delivered);
        // Departures and joins do not rewrite the message's original audience.
        worker
            .archive
            .set_group_info(group, None, &[ME.into(), PEER.into()], false)
            .unwrap();
        send(&mut worker, PEER, ReceiptType::Read);
        assert_eq!(status(&worker, "new"), Delivery::Delivered);
        send(&mut worker, other, ReceiptType::Read);
        assert_eq!(status(&worker, "new"), Delivery::Read);
        assert_eq!(status(&worker, "old"), Delivery::Sent);
        send(&mut worker, PEER, ReceiptType::Delivered);
        assert_eq!(status(&worker, "new"), Delivery::Read);
    }

    #[test]
    fn history_keeps_ephemeral_metadata() {
        let parsed = parse_conversation(wa::Conversation {
            id: PEER.into(),
            ephemeral_expiration: Some(7_776_000),
            ephemeral_setting_timestamp: Some(1_700_000_000),
            ..Default::default()
        });

        assert_eq!(parsed.ephemeral_expiration, Some(7_776_000));
        assert_eq!(parsed.ephemeral_setting_timestamp, Some(1_700_000_000));
    }

    #[test]
    fn protocol_timer_badge_follows_enable_disable_and_ignores_stale_updates() {
        let (mut worker, events, _inbox, _wa) = worker();
        for (expiration, setting_time, envelope_time, expected) in [
            (86_400, None, 200, Some(86_400)),
            (0, None, 300, None),
            (604_800, Some(250), 400, None),
        ] {
            let raw = wa::Message {
                protocol_message: MessageField::some(wa::message::ProtocolMessage {
                    r#type: Some(wa::message::protocol_message::Type::EPHEMERAL_SETTING),
                    ephemeral_expiration: Some(expiration),
                    ephemeral_setting_timestamp: setting_time,
                    ..Default::default()
                }),
                ..Default::default()
            };
            let info = MessageInfo {
                source: MessageSource {
                    chat: PEER.parse().unwrap(),
                    sender: PEER.parse().unwrap(),
                    ..Default::default()
                },
                timestamp: whatsapp_rust::wacore::time::from_secs(envelope_time).unwrap(),
                ..Default::default()
            };
            worker.ingest(&Arc::new(raw), &info);
            assert_eq!(
                worker
                    .archive
                    .chat(PEER)
                    .unwrap()
                    .unwrap()
                    .ephemeral_expiration,
                expected
            );
            assert_eq!(worker.ephemeral_expiration(PEER), expected);
        }
        let badges: Vec<_> = events
            .try_iter()
            .filter_map(|event| match event {
                Event::ChatUpdated(chat) if chat.id == PEER => Some(chat.ephemeral_expiration),
                _ => None,
            })
            .collect();
        assert!(badges.contains(&Some(86_400)));
        assert_eq!(badges.last(), Some(&None));
    }

    #[tokio::test]
    async fn group_timer_updates_work_before_history_and_keep_disable_versions() {
        let (mut worker, _events, _inbox, _wa) = worker();
        let group = "123-456@g.us";
        for (expiration, timestamp, expected) in [
            (86_400, 200, Some(86_400)),
            (0, 300, None),
            (604_800, 250, None),
        ] {
            let update = wa_events::GroupUpdate::builder()
                .group_jid(group.parse().unwrap())
                .timestamp(whatsapp_rust::wacore::time::from_secs(timestamp).unwrap())
                .is_lid_addressing_mode(false)
                .action(
                    whatsapp_rust::wacore::stanza::groups::GroupNotificationAction::Ephemeral {
                        expiration,
                        trigger: None,
                    },
                )
                .build();
            worker
                .handle_wa_event(Arc::new(wa_events::Event::GroupUpdate(update)))
                .await;
            assert_eq!(
                worker
                    .archive
                    .chat(group)
                    .unwrap()
                    .unwrap()
                    .ephemeral_expiration,
                expected
            );
        }
    }

    #[tokio::test]
    async fn default_timer_notifications_never_rewrite_existing_chat_timers() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.ensure_chat(PEER, None);
        worker.archive.set_ephemeral(PEER, 604_800, 100).unwrap();
        for (from, duration, timestamp) in [
            (PEER, 86_400, 200),
            (ME, 86_400, 200),
            (ME, 0, 300),
            (ME, 604_800, 250),
        ] {
            let update = wa_events::DisappearingModeChanged::builder()
                .from(from.parse().unwrap())
                .duration(duration)
                .setting_timestamp(whatsapp_rust::wacore::time::from_secs(timestamp).unwrap())
                .build();
            worker
                .handle_wa_event(Arc::new(wa_events::Event::DisappearingModeChanged(update)))
                .await;
        }
        assert_eq!(worker.ephemeral_expiration(PEER), Some(604_800));
        assert_eq!(worker.default_ephemeral_expiration(), Some(0));
        assert!(worker.archive.chat(ME).unwrap().is_none());
    }

    #[tokio::test]
    async fn own_typing_is_hidden_in_self_direct_and_group_chats() {
        let (mut worker, events, _inbox, _wa) = worker();
        let device = ME.replacen('@', ":2@", 1);
        let own_lid = "9000001@lid";
        worker.me_lid = Some(own_lid.into());
        for (chat, sender) in [ME, PEER, "123-456@g.us"]
            .into_iter()
            .flat_map(|chat| [ME, device.as_str(), own_lid, PEER].map(|sender| (chat, sender)))
        {
            let presence = wa_events::ChatPresenceUpdate::builder()
                .source(MessageSource {
                    chat: chat.parse().unwrap(),
                    sender: sender.parse().unwrap(),
                    is_group: chat.ends_with("@g.us"),
                    ..Default::default()
                })
                .state(ChatPresence::Composing)
                .media(whatsapp_rust::types::presence::ChatPresenceMedia::Text)
                .build();
            worker
                .handle_wa_event(Arc::new(wa_events::Event::ChatPresence(presence)))
                .await;
        }
        let senders: Vec<_> = events
            .try_iter()
            .filter_map(|event| match event {
                Event::Typing { sender, .. } => Some(sender),
                _ => None,
            })
            .collect();
        assert_eq!(senders, [PEER, PEER, PEER]);
    }

    #[test]
    fn partial_group_history_receipts_do_not_override_the_phone_aggregate() {
        use wa::web_message_info::Status;
        let parsed = |chat: &str, status| {
            parse_conversation(wa::Conversation {
                id: chat.into(),
                messages: vec![wa::HistorySyncMsg {
                    message: MessageField::some(wa::WebMessageInfo {
                        key: MessageField::some(wa::MessageKey {
                            id: Some("history".into()),
                            from_me: Some(true),
                            ..Default::default()
                        }),
                        message: MessageField::some(wa::Message {
                            conversation: Some("hello".into()),
                            ..Default::default()
                        }),
                        status: Some(status),
                        user_receipt: vec![wa::UserReceipt {
                            user_jid: PEER.into(),
                            read_timestamp: Some(123),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            })
        };
        assert_eq!(
            parsed("123-456@g.us", Status::SERVER_ACK).messages[0].status,
            Delivery::Sent
        );
        assert_eq!(
            parsed("123-456@g.us", Status::DELIVERY_ACK).messages[0].status,
            Delivery::Delivered
        );
        assert_eq!(
            parsed("123-456@g.us", Status::READ).messages[0].status,
            Delivery::Read
        );
        assert_eq!(
            parsed(PEER, Status::SERVER_ACK).messages[0].status,
            Delivery::Read
        );
    }

    fn incoming(id: &str, timestamp: i64) -> Message {
        Message {
            from_me: false,
            sender: PEER.into(),
            status: Delivery::None,
            ..own_message(id, timestamp)
        }
    }

    #[test]
    fn unknown_or_disabled_account_privacy_never_permits_receipts() {
        use whatsapp_rust::wacore::iq::privacy::{
            PrivacyCategory, PrivacySetting, PrivacySettingsResponse, PrivacyValue,
        };
        let mut settings = PrivacySettingsResponse {
            settings: Vec::new(),
        };
        assert!(!account_allows_receipts(&settings));
        settings.settings.push(PrivacySetting {
            category: PrivacyCategory::ReadReceipts,
            value: PrivacyValue::None,
        });
        assert!(!account_allows_receipts(&settings));
        settings.settings[0].value = PrivacyValue::All;
        assert!(account_allows_receipts(&settings));
        settings.settings[0].value = PrivacyValue::None;
        assert!(
            !account_allows_receipts(&settings),
            "a phone privacy change takes effect without reconnecting"
        );
    }

    fn unread(worker: &Worker) -> u32 {
        worker.archive.chat(PEER).unwrap().unwrap().unread
    }

    fn history(unread: u32) -> ParsedHistory {
        ParsedHistory {
            chats: vec![parse_conversation(wa::Conversation {
                id: PEER.into(),
                unread_count: Some(unread),
                conversation_timestamp: Some(200),
                ..Default::default()
            })],
            push_names: Vec::new(),
            lids: Vec::new(),
            stickers: Vec::new(),
        }
    }

    #[test]
    fn history_preserves_pin_time_and_distinguishes_missing_mute_metadata() {
        let chat = parse_conversation(wa::Conversation {
            id: PEER.into(),
            pinned: Some(1_700_000_000),
            mute_end_time: Some(1_800_000_000),
            ..Default::default()
        });
        assert_eq!(chat.pinned_at, Some(1_700_000_000_000));
        assert_eq!(chat.muted_until, Some(Some(1_800_000_000)));
        for (end, expected) in [
            (None, None),
            (Some(0), Some(None)),
            (Some(u64::MAX), Some(Some(0))),
        ] {
            let chat = parse_conversation(wa::Conversation {
                id: PEER.into(),
                mute_end_time: end,
                ..Default::default()
            });
            assert_eq!(chat.muted_until, expected);
        }
    }

    fn history_chunk(id: &str, archived: Option<bool>, name: Option<&str>) -> ParsedHistory {
        ParsedHistory {
            chats: vec![parse_conversation(wa::Conversation {
                id: id.into(),
                archived,
                display_name: name.map(str::to_owned),
                ..Default::default()
            })],
            push_names: Vec::new(),
            lids: Vec::new(),
            stickers: Vec::new(),
        }
    }

    fn seed_chat(worker: &Worker, id: &str, name: &str, archived: bool) {
        let mut chat = Chat::new(id.into(), name.into());
        chat.archived = archived;
        worker.archive.upsert_chat(&chat).expect("seeds");
    }

    fn stored_chat(worker: &Worker, id: &str) -> Chat {
        worker.archive.chat(id).unwrap().expect("stored")
    }

    #[test]
    fn history_without_archived_flag_keeps_the_stored_state() {
        let (mut worker, _events, _inbox, _wa) = worker();
        seed_chat(&worker, PEER, "Ada", true);
        // An omitted flag keeps the stored archived state.
        worker.apply_history(history_chunk(PEER, None, None), true);
        assert!(stored_chat(&worker, PEER).archived);
        // An explicit value archives or unarchives.
        worker.apply_history(history_chunk(PEER, Some(false), None), true);
        assert!(!stored_chat(&worker, PEER).archived);
        worker.apply_history(history_chunk(PEER, Some(true), None), true);
        assert!(stored_chat(&worker, PEER).archived);
        // A new chat without the flag defaults to unarchived.
        const NEWBIE: &str = "15550002222@s.whatsapp.net";
        worker.apply_history(history_chunk(NEWBIE, None, None), true);
        assert!(!stored_chat(&worker, NEWBIE).archived);
    }

    #[test]
    fn history_without_a_group_name_keeps_the_known_subject() {
        let (mut worker, _events, _inbox, _wa) = worker();
        const GROUP: &str = "120363111222333@g.us";
        seed_chat(&worker, GROUP, "Real Subject", false);
        // A nameless chunk keeps the stored subject, not the fallback.
        worker.apply_history(history_chunk(GROUP, None, None), true);
        assert_eq!(stored_chat(&worker, GROUP).name, "Real Subject");
        // Explicit renames still apply, even to the literal fallback.
        worker.apply_history(history_chunk(GROUP, None, Some("New Subject")), true);
        assert_eq!(stored_chat(&worker, GROUP).name, "New Subject");
        worker.apply_history(history_chunk(GROUP, None, Some("Group")), true);
        assert_eq!(stored_chat(&worker, GROUP).name, "Group");
        // A group genuinely named Group is not treated as nameless.
        worker.apply_history(history_chunk(GROUP, None, None), true);
        assert_eq!(stored_chat(&worker, GROUP).name, "Group");
        // A brand-new group without a name still gets the fallback.
        const FRESH: &str = "120363999888777@g.us";
        worker.apply_history(history_chunk(FRESH, None, None), true);
        assert_eq!(stored_chat(&worker, FRESH).name, "Group");
    }

    #[test]
    fn sends_require_a_supported_type_or_a_known_sendable_chat() {
        let (worker, _events, _inbox, _wa) = worker();
        // Unknown addresses: direct chats and groups may start, channels
        // and broadcast lists may not, all without touching the archive.
        assert!(Worker::send_allowed_unknown("15550001111@s.whatsapp.net"));
        assert!(Worker::send_allowed_unknown("120363111222333@g.us"));
        assert!(!Worker::send_allowed_unknown("55@newsletter"));
        assert!(!Worker::send_allowed_unknown("55@broadcast"));
        // A channel missing from the archive is still refused by address.
        assert!(!worker.send_allowed("55@newsletter"));
        // A new direct chat is allowed through.
        assert!(worker.send_allowed("15550002222@s.whatsapp.net"));
        // Known rows follow the shared rule: channels never, muted never.
        const CHANNEL: &str = "77@newsletter";
        seed_chat(&worker, CHANNEL, "News", false);
        assert!(!worker.send_allowed(CHANNEL));
        // Muted announcement state arrives through group info, the same
        // path production uses (upsert_chat does not persist read_only).
        seed_chat(&worker, "15550003333@s.whatsapp.net", "Muted", false);
        worker
            .archive
            .set_group_info("15550003333@s.whatsapp.net", None, &[], true)
            .expect("seeds");
        assert!(!worker.send_allowed("15550003333@s.whatsapp.net"));
        // One lookup decision, offline-testable: a store failure denies even
        // a new direct chat, while unknown types follow the same rule.
        assert!(!Worker::decide_send(
            Err::<Option<Chat>, String>("boom".to_owned()),
            "15550001111@s.whatsapp.net",
        ));
        assert!(Worker::decide_send(
            Ok::<Option<Chat>, String>(None),
            "15550001111@s.whatsapp.net",
        ));
        assert!(!Worker::decide_send(
            Ok::<Option<Chat>, String>(None),
            "55@newsletter",
        ));
        let channel = Chat::new("77@newsletter".into(), "News".into());
        assert!(!Worker::decide_send(
            Ok::<Option<Chat>, String>(Some(channel)),
            "77@newsletter",
        ));
    }

    #[tokio::test]
    async fn mute_and_pin_sync_before_history_survive_replays_and_unsetting() {
        let (mut worker, _events, _inbox, _wa) = worker();
        let time = whatsapp_rust::wacore::time::now_utc();
        for enabled in [true, false] {
            let mute = wa_events::MuteUpdate::builder()
                .jid(PEER.parse().unwrap())
                .timestamp(time)
                .from_full_sync(true)
                .action(Box::new(wa::sync_action_value::MuteAction {
                    muted: Some(enabled),
                    mute_end_timestamp: Some(-1),
                    ..Default::default()
                }))
                .build();
            let pin = wa_events::PinUpdate::builder()
                .jid(PEER.parse().unwrap())
                .timestamp(time)
                .from_full_sync(true)
                .action(Box::new(wa::sync_action_value::PinAction {
                    pinned: Some(enabled),
                }))
                .build();
            worker
                .handle_wa_event(Arc::new(wa_events::Event::MuteUpdate(mute)))
                .await;
            worker
                .handle_wa_event(Arc::new(wa_events::Event::PinUpdate(pin)))
                .await;
            let before = worker
                .archive
                .chat(PEER)
                .unwrap()
                .expect("sync creates the chat");
            assert_eq!(before.muted_until, enabled.then_some(0));
            assert_eq!(before.pinned, enabled);
            assert_eq!(
                before.pinned_at,
                if enabled { time.timestamp_millis() } else { 0 }
            );

            let mut stale = history(0);
            stale.chats[0].pinned_at = Some(if enabled { 0 } else { 123_000 });
            stale.chats[0].muted_until = Some(if enabled { None } else { Some(0) });
            worker.apply_history(stale, true);
            let after = worker.archive.chat(PEER).unwrap().unwrap();
            assert_eq!(after.muted_until, before.muted_until);
            assert_eq!(after.pinned, before.pinned);
            assert_eq!(after.pinned_at, before.pinned_at);
        }
    }

    #[test]
    fn early_privacy_id_mute_reaches_the_canonical_chat_without_a_duplicate() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.ensure_chat(PEER_LID, None);
        worker.archive.set_muted_at(PEER_LID, Some(0), 200).unwrap();
        worker.learn_lid("167650256810092", "4917663430455");
        let mut snapshot = history(0);
        snapshot.chats[0].pinned_at = Some(123_000);
        worker.apply_history(snapshot, true);
        let chat = worker.archive.chat(PEER).unwrap().unwrap();
        assert_eq!(chat.muted_until, Some(0));
        assert!(chat.pinned, "missing pin sync must not block history's pin");
        worker.emit_chats();
        let chats = events
            .try_iter()
            .filter_map(|event| match event {
                Event::Chats(chats) => Some(chats),
                _ => None,
            })
            .last()
            .unwrap();
        assert!(chats.iter().any(|chat| chat.id == PEER));
        assert!(!chats.iter().any(|chat| chat.id == PEER_LID));
        worker.archive.set_muted_at(PEER, None, 300).unwrap();
        worker
            .archive
            .put_lid("167650256810092", "4917663430455")
            .unwrap();
        assert_eq!(
            worker.archive.chat(PEER).unwrap().unwrap().muted_until,
            None
        );
    }

    #[test]
    fn history_without_mute_metadata_preserves_the_existing_history_value() {
        let (mut worker, _events, _inbox, _wa) = worker();
        let mut first = history(0);
        first.chats[0].muted_until = Some(Some(0));
        worker.apply_history(first, true);
        worker.apply_history(history(0), true);
        assert_eq!(
            worker.archive.chat(PEER).unwrap().unwrap().muted_until,
            Some(0)
        );
        let mut unmuted = history(0);
        unmuted.chats[0].muted_until = Some(None);
        worker.apply_history(unmuted, true);
        assert_eq!(
            worker.archive.chat(PEER).unwrap().unwrap().muted_until,
            None
        );
    }

    #[test]
    fn reading_without_blue_ticks_still_queues_private_sync_and_survives_history() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.store_message(incoming("a", 100), None, None);
        worker.store_message(incoming("b", 200), None, None);
        assert_eq!(unread(&worker), 2);
        worker.mark_read(PEER.into(), false);
        assert_eq!(unread(&worker), 0);
        assert_eq!(
            worker.archive.pending_reads().unwrap(),
            vec![(PEER.into(), 200)]
        );
        worker.apply_history(history(2), true);
        assert_eq!(
            unread(&worker),
            0,
            "stale history must not resurrect badges"
        );
        worker.store_message(incoming("late", 150), None, None);
        assert_eq!(unread(&worker), 0, "a delayed read message stays read");
        worker.store_message(incoming("new", 300), None, None);
        worker.apply_history(history(2), false);
        assert_eq!(
            unread(&worker),
            1,
            "paging old history preserves a new unread message"
        );
    }

    #[tokio::test]
    async fn a_failed_read_sync_stays_queued_until_it_succeeds() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.store_message(incoming("a", 100), None, None);
        worker.mark_read(PEER.into(), false);
        let now = Instant::now();
        assert!(worker.read_sync.start(PEER, 100, now));
        worker
            .handle_command(Command::ReadSyncFinished {
                chat: PEER.into(),
                through: 100,
                success: false,
            })
            .await;
        assert_eq!(
            worker.archive.pending_reads().unwrap(),
            vec![(PEER.into(), 100)]
        );
        assert!(!worker.read_sync.ready(Instant::now()));
        assert!(!worker.read_sync.start("another-chat", 200, Instant::now()));
        // A new local read stays queued while the shared collection backs off.
        worker.store_message(incoming("b", 200), None, None);
        worker.mark_read(PEER.into(), false);
        assert!(
            worker
                .read_sync
                .start(PEER, 100, now + Duration::from_secs(31))
        );
        worker
            .handle_command(Command::ReadSyncFinished {
                chat: PEER.into(),
                through: 100,
                success: true,
            })
            .await;
        assert_eq!(
            worker.archive.pending_reads().unwrap(),
            vec![(PEER.into(), 200)]
        );
        assert!(worker.read_sync.start(PEER, 200, Instant::now()));
        worker
            .handle_command(Command::ReadSyncFinished {
                chat: PEER.into(),
                through: 200,
                success: true,
            })
            .await;
        assert!(worker.archive.pending_reads().unwrap().is_empty());
        assert!(worker.read_sync.ready(Instant::now()));
    }

    #[test]
    fn replying_on_the_phone_reads_only_preceding_messages() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.store_message(incoming("old", 100), None, None);
        worker.store_message(incoming("new", 300), None, None);
        worker.store_message(
            Message {
                status: Delivery::Failed,
                ..own_message("failed", 400)
            },
            None,
            None,
        );
        assert_eq!(unread(&worker), 2, "a failed send does not read the chat");
        worker.store_message(own_message("reply", 200), None, None);
        assert_eq!(unread(&worker), 1);
        worker.store_message(own_message("reply2", 400), None, None);
        assert_eq!(unread(&worker), 0);
        worker.store_message(own_message("reply", 200), None, None);
        assert_eq!(worker.archive.read_through(PEER).unwrap(), Some(400));
        while events.try_recv().is_ok() {}
        worker.store_message(incoming("late", 150), None, None);
        assert_eq!(unread(&worker), 0);
        assert!(
            !events
                .try_iter()
                .any(|event| matches!(event, Event::Incoming { .. }))
        );
    }

    #[test]
    fn delayed_phone_receipts_preserve_newer_unread_messages() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.learn_lid("167650256810092", "4917663430455");
        worker.store_message(incoming("old", 100), None, None);
        worker.store_message(incoming("new", 300), None, None);
        worker.on_receipt(&receipt(PEER_LID, &["old"], ReceiptType::ReadSelf));
        assert_eq!(unread(&worker), 1);
        worker.on_receipt(&receipt(PEER_LID, &["unknown"], ReceiptType::ReadSelf));
        assert_eq!(
            unread(&worker),
            1,
            "an unknown receipt has no known read position"
        );
        worker.on_receipt(&receipt(PEER_LID, &["new"], ReceiptType::ReadSelf));
        assert_eq!(unread(&worker), 0);
    }

    #[test]
    fn rapid_messages_keep_distinct_read_positions_within_the_same_second() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.store_message(incoming("first", 100), None, None);
        worker.mark_read(PEER.into(), false);
        worker.store_message(incoming("second", 100), None, None);
        worker.store_message(incoming("third", 100), None, None);
        assert_eq!(unread(&worker), 2);
        worker.on_receipt(&receipt(PEER, &["first"], ReceiptType::ReadSelf));
        assert_eq!(unread(&worker), 2);
        worker.on_receipt(&receipt(PEER, &["second"], ReceiptType::ReadSelf));
        assert_eq!(unread(&worker), 1);
        assert_eq!(
            worker.archive.unread_incoming(PEER, 1).unwrap(),
            vec![("third".into(), PEER.into())]
        );
        worker.on_receipt(&receipt(PEER, &["third"], ReceiptType::ReadSelf));
        assert_eq!(unread(&worker), 0);
    }

    #[test]
    fn a_phone_history_snapshot_can_clear_stale_unread_counts() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.store_message(incoming("a", 100), None, None);
        worker.store_message(incoming("b", 200), None, None);
        worker.store_message(incoming("new", 300), None, None);
        worker.apply_history(history(0), true);
        assert_eq!(
            unread(&worker),
            1,
            "a read snapshot preserves later arrivals"
        );
        worker.apply_history(history(2), true);
        assert_eq!(
            unread(&worker),
            1,
            "older unread history cannot undo a read snapshot"
        );
    }

    #[tokio::test]
    async fn phone_read_updates_cover_their_range_even_before_history_arrives() {
        let (mut worker, _events, _inbox, _wa) = worker();
        let event = wa_events::MarkChatAsReadUpdate::builder()
            .jid(PEER.parse().unwrap())
            .timestamp(whatsapp_rust::wacore::time::now_utc())
            .from_full_sync(false)
            .action(Box::new(wa::sync_action_value::MarkChatAsReadAction {
                read: Some(true),
                message_range: MessageField::some(whatsapp_rust::message_range(
                    200,
                    None,
                    Vec::new(),
                )),
            }))
            .build();
        worker
            .handle_wa_event(Arc::new(wa_events::Event::MarkChatAsReadUpdate(event)))
            .await;
        worker.apply_history(history(2), true);
        worker.store_message(incoming("late", 100), None, None);
        worker.store_message(incoming("new", 300), None, None);
        assert_eq!(unread(&worker), 1);
    }

    #[test]
    fn a_read_receipt_from_the_peers_privacy_id_moves_our_messages() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "R").expect("chat");
        for (id, when) in [("A1", 100), ("A2", 200), ("A3", 300)] {
            worker
                .archive
                .insert_message(&own_message(id, when), None)
                .expect("stored");
        }
        worker.learn_lid("167650256810092", "4917663430455");
        worker.on_receipt(&receipt(PEER_LID, &["A2"], ReceiptType::Read));
        let status = |id: &str| {
            worker
                .archive
                .message(PEER, id)
                .expect("read")
                .expect("row")
                .status
        };
        assert_eq!(status("A2"), Delivery::Read, "the named message");
        assert_eq!(status("A1"), Delivery::Read, "and everything before it");
        assert_eq!(status("A3"), Delivery::Sent, "not what came after");
    }

    #[test]
    fn inactive_counts_as_delivered_and_sender_only_in_the_chat_with_ourselves() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "R").expect("chat");
        worker.archive.ensure_chat(ME, "Me").expect("chat");
        worker
            .archive
            .insert_message(&own_message("C1", 100), None)
            .expect("stored");
        let mut to_self = own_message("S1", 100);
        to_self.chat = ME.into();
        worker
            .archive
            .insert_message(&to_self, None)
            .expect("stored");
        worker.on_receipt(&receipt(PEER, &["C1"], ReceiptType::Inactive));
        assert_eq!(
            worker
                .archive
                .message(PEER, "C1")
                .expect("read")
                .expect("row")
                .status,
            Delivery::Delivered,
            "an inactive device still received it"
        );
        worker.on_receipt(&receipt(PEER, &["C1"], ReceiptType::Sender));
        assert_eq!(
            worker
                .archive
                .message(PEER, "C1")
                .expect("read")
                .expect("row")
                .status,
            Delivery::Delivered,
            "our own other device says nothing about the peer"
        );
        worker.on_receipt(&receipt(ME, &["S1"], ReceiptType::Sender));
        assert_eq!(
            worker
                .archive
                .message(ME, "S1")
                .expect("read")
                .expect("row")
                .status,
            Delivery::Read,
            "a message to ourselves is read once the phone has it"
        );
    }

    #[test]
    fn a_delivery_receipt_from_the_phone_number_moves_only_the_named_message() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "R").expect("chat");
        for (id, when) in [("B1", 100), ("B2", 200)] {
            worker
                .archive
                .insert_message(&own_message(id, when), None)
                .expect("stored");
        }
        worker.on_receipt(&receipt(PEER, &["B2"], ReceiptType::Delivered));
        let status = |id: &str| {
            worker
                .archive
                .message(PEER, id)
                .expect("read")
                .expect("row")
                .status
        };
        assert_eq!(status("B2"), Delivery::Delivered);
        assert_eq!(status("B1"), Delivery::Sent);
    }
    #[tokio::test]
    async fn archive_intent_survives_offline_and_converges_on_echo() {
        let (mut worker, _events, _inbox, _wa) = worker();
        assert!(worker.client.is_none(), "synthetic worker stays offline");
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker
            .handle_command(Command::SetArchived(PEER.into(), true))
            .await;
        assert!(
            worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived
        );
        assert!(
            matches!(
                worker
                    .archive
                    .queued_chat_sync(PEER, "archived")
                    .expect("queue"),
                Some((true, _, _))
            ),
            "offline intent stays queued"
        );
        // An agreeing echo, even older, converges and forgets the intent.
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, true, 1)))
            .await;
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_none()
        );
    }

    #[tokio::test]
    async fn newer_local_archive_intent_outranks_older_remote_state() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker.archive.set_archived(PEER, true).expect("local");
        worker
            .archive
            .queue_chat_sync(PEER, "archived", true, 3_000)
            .expect("intent");
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, false, 1_000)))
            .await;
        assert!(
            worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived,
            "local intent kept"
        );
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_some(),
            "intent kept"
        );
    }

    #[tokio::test]
    async fn older_remote_archive_state_replaces_a_stale_intent() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker.archive.set_archived(PEER, true).expect("local");
        worker
            .archive
            .queue_chat_sync(PEER, "archived", true, 1_000)
            .expect("intent");
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, false, 3_000)))
            .await;
        assert!(
            !worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived,
            "remote state applied"
        );
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_none()
        );
    }

    #[tokio::test]
    async fn sync_failures_surface_once_then_stay_quiet() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        let rev = worker
            .archive
            .queue_chat_sync(PEER, "archived", true, 1)
            .expect("intent");
        for _ in 0..4 {
            // Each counted failure answers a real attempt of this revision.
            worker.sync_in_flight.insert((PEER.into(), rev), ());
            worker
                .handle_command(Command::ChatSyncFlushed {
                    chat: PEER.into(),
                    rev,
                    ok: false,
                })
                .await;
        }
        let errors = ui_events(&events)
            .into_iter()
            .filter(|event| matches!(event, Event::Error(_)))
            .count();
        assert_eq!(errors, 1, "one visible failure, not four");
        worker.sync_in_flight.insert((PEER.into(), rev), ());
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev,
                ok: false,
            })
            .await;
        assert!(
            ui_events(&events)
                .iter()
                .all(|event| !matches!(event, Event::Error(_))),
            "stays quiet afterwards"
        );
        worker.sync_in_flight.insert((PEER.into(), rev), ());
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev,
                ok: true,
            })
            .await;
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_none()
        );
    }

    #[tokio::test]
    async fn tombstoned_replay_stays_off_screen_and_unread() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        let incoming = |id: &str, timestamp: i64| Message {
            chat: PEER.into(),
            sender: PEER.into(),
            sender_name: None,
            from_me: false,
            ..own_message(id, timestamp)
        };
        worker.store_message(incoming("m1", 100), None, None);
        assert_eq!(
            worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .unread,
            1
        );
        worker
            .handle_wa_event(Arc::new(delete_for_me_update(PEER, "m1", 150)))
            .await;
        // Deleting the only message recomputes the counters from survivors.
        assert_eq!(
            worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .unread,
            0
        );
        let _ = ui_events(&events);
        // A late replay of the tombstoned id stays out of storage, bubbles,
        // unread counts, and notifications alike.
        worker.store_message(incoming("m1", 100), None, None);
        assert!(worker.archive.message(PEER, "m1").expect("row").is_none());
        assert_eq!(
            worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .unread,
            0
        );
        assert!(ui_events(&events).is_empty(), "no bubble and no ping");
    }

    #[tokio::test]
    async fn delete_for_me_removes_one_row_and_blocks_its_replay() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        for (id, timestamp) in [("m1", 100), ("m2", 200)] {
            worker.store_message(
                Message {
                    chat: PEER.into(),
                    ..own_message(id, timestamp)
                },
                None,
                None,
            );
        }
        worker
            .handle_wa_event(Arc::new(delete_for_me_update(PEER, "m1", 150)))
            .await;
        assert!(worker.archive.message(PEER, "m1").expect("row").is_none());
        assert!(worker.archive.message(PEER, "m2").expect("row").is_some());
        let deleted = ui_events(&events)
            .into_iter()
            .filter(|event| matches!(event, Event::MessageDeleted { .. }))
            .count();
        assert_eq!(deleted, 1);
        // A late replay of the same id stays gone.
        worker.store_message(
            Message {
                chat: PEER.into(),
                ..own_message("m1", 100)
            },
            None,
            None,
        );
        assert!(
            worker.archive.message(PEER, "m1").expect("row").is_none(),
            "replay stays gone"
        );
        // Repeating the event still invalidates the interface: the archive
        // may already be right while the screen still shows the message.
        worker
            .handle_wa_event(Arc::new(delete_for_me_update(PEER, "m1", 150)))
            .await;
        assert_eq!(
            ui_events(&events)
                .iter()
                .filter(|event| matches!(event, Event::MessageDeleted { .. }))
                .count(),
            1,
            "repeat still invalidates"
        );
    }

    #[tokio::test]
    async fn delete_for_me_through_lid_survives_the_mapping() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER_LID, "Old").expect("chat");
        worker.store_message(
            Message {
                chat: PEER_LID.into(),
                ..own_message("m1", 100)
            },
            None,
            None,
        );
        // Mapping unknown: the delete files and invalidates under the LID.
        worker
            .handle_wa_event(Arc::new(delete_for_me_update(PEER_LID, "m1", 150)))
            .await;
        assert!(
            worker
                .archive
                .message(PEER_LID, "m1")
                .expect("row")
                .is_none()
        );
        // Learning the mapping moves the tombstone; a replay under the
        // number stays gone and the canonical chat converges.
        worker.learn_lid("167650256810092", "4917663430455");
        worker.store_message(
            Message {
                chat: PEER.into(),
                ..own_message("m1", 100)
            },
            None,
            None,
        );
        assert!(worker.archive.message(PEER, "m1").expect("row").is_none());
        assert!(
            ui_events(&events)
                .iter()
                .any(|event| matches!(event, Event::MessageDeleted { .. }))
        );
    }

    #[tokio::test]
    async fn removed_chat_always_invalidates_the_interface() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker.store_message(
            Message {
                chat: PEER.into(),
                ..own_message("m1", 100)
            },
            None,
            None,
        );
        worker
            .handle_wa_event(Arc::new(delete_update(PEER, 200)))
            .await;
        assert!(worker.archive.chat(PEER).expect("chat").is_none());
        // Repeating the removal still tells the interface: the archive may
        // be right while the screen is stale.
        worker
            .handle_wa_event(Arc::new(delete_update(PEER, 200)))
            .await;
        assert_eq!(
            ui_events(&events)
                .iter()
                .filter(|event| matches!(event, Event::ChatRemoved { .. }))
                .count(),
            2,
            "repeat still invalidates"
        );
    }
    #[tokio::test]
    async fn delete_for_me_keeps_files_a_survivor_still_references() {
        let root =
            std::env::temp_dir().join(format!("zapfast-delete-media-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let shared = root.join("shared.mp4");
        let lone = root.join("lone.mp4");
        std::fs::write(&shared, b"bytes").expect("file");
        std::fs::write(&lone, b"bytes").expect("file");
        let video = |id: &str, timestamp: i64, path: &std::path::Path| Message {
            chat: PEER.into(),
            content: Content::Video {
                caption: None,
                media: crate::model::Media {
                    mime: "video/mp4".into(),
                    size: 5,
                    width: None,
                    height: None,
                    path: Some(path.to_path_buf()),
                    state: Default::default(),
                },
                seconds: None,
                gif: false,
            },
            ..own_message(id, timestamp)
        };
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker.store_message(video("m1", 100, &shared), None, None);
        worker.store_message(video("m2", 200, &shared), None, None);
        worker.store_message(video("m3", 300, &lone), None, None);
        worker
            .handle_wa_event(Arc::new(delete_for_me_update(PEER, "m1", 150)))
            .await;
        // Files leave the critical path: still there until the tick.
        assert!(shared.exists(), "collection is deferred");
        worker.pump_media_gc();
        assert!(shared.exists(), "survivor still references it");
        worker
            .handle_wa_event(Arc::new(delete_for_me_update(PEER, "m2", 250)))
            .await;
        worker.pump_media_gc();
        assert!(!shared.exists(), "last reference deleted the file");
        worker
            .handle_wa_event(Arc::new(delete_for_me_update(PEER, "m3", 350)))
            .await;
        worker.pump_media_gc();
        assert!(!lone.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn stale_completion_cannot_erase_a_newer_intent() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        // Archive flies as revision 1; the user unarchives before it answers.
        worker
            .handle_command(Command::SetArchived(PEER.into(), true))
            .await;
        worker.sync_in_flight.insert((PEER.into(), 1), ());
        worker
            .handle_command(Command::SetArchived(PEER.into(), false))
            .await;
        // The late success of revision 1 must not clear revision 2.
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev: 1,
                ok: true,
            })
            .await;
        assert!(
            !worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived
        );
        assert!(
            matches!(
                worker
                    .archive
                    .queued_chat_sync(PEER, "archived")
                    .expect("queue"),
                Some((false, _, 2))
            ),
            "newer intent survives"
        );
        // Its own completion converges.
        worker.sync_in_flight.insert((PEER.into(), 2), ());
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev: 2,
                ok: true,
            })
            .await;
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_none()
        );
    }

    #[tokio::test]
    async fn old_failure_does_not_spend_the_new_intent_budget() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker
            .handle_command(Command::SetArchived(PEER.into(), true))
            .await;
        worker.sync_in_flight.insert((PEER.into(), 1), ());
        worker
            .handle_command(Command::SetArchived(PEER.into(), false))
            .await;
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev: 1,
                ok: false,
            })
            .await;
        assert!(
            !worker
                .sync_attempts
                .keys()
                .any(|(id, _)| id.as_str() == PEER),
            "superseded failure spends nothing"
        );
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_some(),
            "newer intent still queued"
        );
    }

    #[test]
    fn retry_delay_backs_off_then_stops() {
        use std::time::Duration;
        assert_eq!(retry_delay(0), None);
        assert_eq!(retry_delay(1), Some(Duration::from_secs(5)));
        assert_eq!(retry_delay(2), Some(Duration::from_secs(15)));
        assert_eq!(retry_delay(3), Some(Duration::from_secs(45)));
        assert_eq!(retry_delay(4), Some(Duration::from_secs(300)));
        assert_eq!(retry_delay(8), Some(Duration::from_secs(300)));
        assert_eq!(retry_delay(9), None);
    }

    #[tokio::test]
    async fn pump_retries_due_intents_without_new_input() {
        use std::time::{Duration, Instant};
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker
            .archive
            .queue_chat_sync(PEER, "archived", true, 1)
            .expect("intent");
        // A future deadline waits; a past one dispatches on the tick alone.
        let now = Instant::now();
        worker
            .sync_retry_at
            .insert(PEER.into(), now + Duration::from_secs(60));
        worker.pump_chat_sync_at(now);
        assert_eq!(
            worker.sync_retry_at.get(PEER),
            Some(&(now + Duration::from_secs(60)))
        );
        worker
            .sync_retry_at
            .insert(PEER.into(), now - Duration::from_secs(1));
        worker.pump_chat_sync_at(now);
        assert!(
            worker.sync_retry_at.get(PEER).is_some_and(|at| *at > now),
            "offline intent rescheduled, still queued"
        );
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_some()
        );
    }

    #[tokio::test]
    async fn rekey_adopts_sync_bookkeeping() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker
            .archive
            .queue_chat_sync(PEER_LID, "archived", true, 100)
            .expect("intent");
        let rev = worker
            .archive
            .queue_chat_sync(PEER_LID, "archived", true, 100)
            .expect("intent");
        worker.sync_attempts.insert((PEER_LID.into(), rev), 3);
        // A stale exhausted budget on the number must not leash the winner.
        worker.sync_attempts.insert((PEER.into(), 999), 9);
        worker.learn_lid("167650256810092", "4917663430455");
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER_LID, "archived")
                .expect("queue")
                .is_none()
        );
        let moved = worker
            .archive
            .queued_chat_sync(PEER, "archived")
            .expect("queue")
            .expect("intent follows the number");
        assert_ne!(moved.2, rev, "fresh revision on the move");
        assert!(
            !worker
                .sync_attempts
                .keys()
                .any(|(id, _)| id.as_str() == PEER_LID),
            "old keys pruned"
        );
        assert!(
            !worker.sync_attempts.contains_key(&(PEER.into(), moved.2)),
            "winner keeps a fresh budget despite the stale 9"
        );
        assert!(
            worker.sync_retry_at.contains_key(PEER),
            "survivor scheduled offline"
        );
    }

    #[tokio::test]
    async fn echo_with_in_flight_op_leaves_queue_alone() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker
            .handle_command(Command::SetArchived(PEER.into(), true))
            .await;
        worker.sync_in_flight.insert((PEER.into(), 1), ());
        let _ = ui_events(&events);
        // An agreeing echo cannot confirm an intent its own older
        // operation may predate: the completion reconciles instead.
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, true, 1)))
            .await;
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_some(),
            "queue kept while an op flies"
        );
        assert!(ui_events(&events).is_empty(), "echo stays quiet");
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev: 1,
                ok: true,
            })
            .await;
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_none()
        );
    }

    #[tokio::test]
    async fn archive_unarchive_archive_keeps_the_last_intent() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker
            .handle_command(Command::SetArchived(PEER.into(), true))
            .await;
        worker.sync_in_flight.insert((PEER.into(), 1), ());
        worker
            .handle_command(Command::SetArchived(PEER.into(), false))
            .await;
        worker
            .handle_command(Command::SetArchived(PEER.into(), true))
            .await;
        // A stale echo of the first archive must not touch the last one.
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, true, 1)))
            .await;
        assert!(
            worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived
        );
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_some()
        );
        // The late success of revision 1 settles nothing and spends nothing.
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev: 1,
                ok: true,
            })
            .await;
        assert!(
            worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived
        );
        assert!(
            !worker
                .sync_attempts
                .keys()
                .any(|(id, _)| id.as_str() == PEER),
            "no budget spent on superseded revisions"
        );
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_some()
        );
    }

    #[tokio::test]
    async fn exhausted_budget_sends_nothing_on_tick() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        let rev = worker
            .archive
            .queue_chat_sync(PEER, "archived", true, 1)
            .expect("intent");
        for _ in 0..300 {
            worker.sync_in_flight.insert((PEER.into(), rev), ());
            worker
                .handle_command(Command::ChatSyncFlushed {
                    chat: PEER.into(),
                    rev,
                    ok: false,
                })
                .await;
        }
        assert_eq!(
            worker.sync_attempts.get(&(PEER.into(), rev)),
            Some(&9),
            "budget saturates instead of overflowing"
        );
        worker.sync_retry_at.remove(PEER);
        let (sink, accepted) = std::sync::mpsc::channel();
        worker.sync_sink = Some(sink);
        for _ in 0..3 {
            worker.pump_chat_sync();
        }
        assert!(
            accepted.try_recv().is_err(),
            "exhausted budget sends nothing"
        );
        assert!(
            !worker.sync_in_flight.keys().any(|(id, _)| id == PEER),
            "no new flight"
        );
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_some(),
            "queue kept"
        );
    }

    #[tokio::test]
    async fn pump_dispatches_at_most_four_chats_per_round() {
        let (mut worker, _events, _inbox, _wa) = worker();
        for index in 0..6 {
            let chat = format!("c{index}@s.whatsapp.net");
            worker
                .archive
                .queue_chat_sync(&chat, "archived", true, index)
                .expect("intent");
        }
        let (sink, accepted) = std::sync::mpsc::channel();
        worker.sync_sink = Some(sink);
        worker.pump_chat_sync();
        let mut first_round = Vec::new();
        while let Ok(job) = accepted.try_recv() {
            first_round.push(job.chat);
        }
        assert_eq!(first_round.len(), 4, "one round dispatches four");
        for chat in &first_round {
            let rev = worker
                .archive
                .queued_chat_sync(chat, "archived")
                .expect("queue")
                .expect("pending")
                .2;
            worker
                .handle_command(Command::ChatSyncFlushed {
                    chat: chat.clone(),
                    rev,
                    ok: true,
                })
                .await;
        }
        worker.pump_chat_sync();
        let mut second_round = Vec::new();
        while let Ok(job) = accepted.try_recv() {
            second_round.push(job.chat);
        }
        assert_eq!(second_round.len(), 2, "the rest follows next round");
    }

    #[tokio::test]
    async fn stale_opposite_echo_keeps_the_newer_queued_intent() {
        // R1 = archive dispatched; R2 = unarchive queued before R1 runs.
        // The phone stamps R1 after R2: its echo must neither flip the
        // chat nor dequeue R2, which still has to converge.
        let (mut worker, events, _inbox, _wa) = worker();
        let (sink, accepted) = std::sync::mpsc::channel();
        worker.sync_sink = Some(sink);
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker
            .handle_command(Command::SetArchived(PEER.into(), true))
            .await;
        assert!(accepted.try_recv().is_ok(), "R1 dispatched");
        let rev1 = worker
            .sync_in_flight
            .keys()
            .find(|(id, _)| id.as_str() == PEER)
            .map(|(_, rev)| *rev)
            .expect("R1 flight");
        // R2 while R1 flies: queued, never sent yet.
        worker
            .handle_command(Command::SetArchived(PEER.into(), false))
            .await;
        assert!(accepted.try_recv().is_err(), "R2 waits for R1");
        let (_, updated2, rev2) = worker
            .archive
            .queued_chat_sync(PEER, "archived")
            .expect("queue")
            .expect("R2 intent");
        // R1 echo stamped after R2 intent time.
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, true, updated2 + 60_000)))
            .await;
        assert!(
            !worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived,
            "stale echo never flips the newer intent"
        );
        assert_eq!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .map(|(_, _, rev)| rev),
            Some(rev2),
            "R2 stays queued"
        );
        let _ = ui_events(&events);
        // R1 completion is stale against R2: the survivor goes out next.
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev: rev1,
                ok: true,
            })
            .await;
        assert!(accepted.try_recv().is_ok(), "R2 dispatched after R1");
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev: rev2,
                ok: true,
            })
            .await;
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_none(),
            "R2 converged"
        );
        assert!(
            !worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived,
            "last click wins"
        );
    }
    #[tokio::test]
    async fn concurrent_remote_matching_the_flying_value_keeps_the_queued_intent() {
        // Wire ambiguity, stated plainly: the library stamps every
        // archive mutation with now_millis at send time and echoes only
        // jid, timestamp and archived flag. No revision rides along, so
        // an R1 echo stamped after R2 looks exactly like a genuine phone
        // change to the R1 value stamped after R2. Policy keeps the queued
        // local intent and records only the order marker; the survivor
        // dispatch converges afterwards. This test drives the R1-echo
        // shape through the production event path.
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        let (sink, accepted) = std::sync::mpsc::channel();
        worker.sync_sink = Some(sink);
        worker
            .handle_command(Command::SetArchived(PEER.into(), true))
            .await;
        assert!(accepted.try_recv().is_ok());
        worker
            .handle_command(Command::SetArchived(PEER.into(), false))
            .await;
        let (_, updated2, _) = worker
            .archive
            .queued_chat_sync(PEER, "archived")
            .expect("queue")
            .expect("R2");
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, true, updated2 + 60_000)))
            .await;
        assert!(
            !worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived
        );
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_some()
        );
        assert_eq!(
            worker
                .archive
                .sync_order(PEER)
                .expect("order")
                .map(|(ms, _)| ms),
            Some(updated2 + 60_000)
        );
    }

    #[tokio::test]
    async fn stale_echo_below_accepted_order_is_ignored() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, true, 200)))
            .await;
        assert!(
            worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived
        );
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, false, 100)))
            .await;
        assert!(
            worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived
        );
        assert_eq!(
            worker.archive.sync_order(PEER).expect("order"),
            Some((200, true))
        );
    }

    #[tokio::test]
    async fn fresh_intent_reopens_a_spent_budget() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker.sync_attempts.insert((PEER.into(), 999), 9);
        worker
            .archive
            .queue_chat_sync(PEER, "archived", true, 1)
            .expect("intent");
        worker
            .handle_command(Command::SetArchived(PEER.into(), false))
            .await;
        assert!(
            !worker
                .sync_attempts
                .keys()
                .any(|(id, _)| id.as_str() == PEER),
            "explicit intent resets"
        );
        assert!(
            worker.sync_retry_at.contains_key(PEER),
            "offline intent scheduled"
        );
    }

    #[tokio::test]
    async fn learn_lid_failure_keeps_retry_open() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER_LID, "Old").expect("chat");
        worker
            .archive
            .queue_chat_sync(PEER_LID, "archived", true, 100)
            .expect("intent");
        worker
            .archive
            .drop_table_for_test("chat_sync_queue")
            .expect("sabotage");
        worker.learn_lid("167650256810092", "4917663430455");
        assert!(
            !worker.lid_to_pn.contains_key("167650256810092"),
            "mapping returns for retry"
        );
    }

    #[tokio::test]
    async fn save_failure_repaints_and_warns() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker
            .archive
            .drop_table_for_test("chat_sync_queue")
            .expect("sabotage");
        worker
            .handle_command(Command::SetArchived(PEER.into(), true))
            .await;
        assert!(
            !worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived,
            "nothing persisted"
        );
        let seen: Vec<Event> = ui_events(&events);
        assert!(
            seen.iter().any(|event| matches!(event, Event::Error(_))),
            "visible warning"
        );
        assert!(
            seen.iter()
                .any(|event| matches!(event, Event::ChatUpdated(_))),
            "repaint from truth"
        );
    }

    #[tokio::test]
    async fn exhausted_chats_do_not_starve_fresh_ones() {
        let (mut worker, _events, _inbox, _wa) = worker();
        for index in 0..4 {
            let chat = format!("old{index}@s.whatsapp.net");
            let rev = worker
                .archive
                .queue_chat_sync(&chat, "archived", true, index)
                .expect("intent");
            worker.sync_attempts.insert((chat, rev), 9);
        }
        for index in 0..2 {
            let chat = format!("new{index}@s.whatsapp.net");
            worker
                .archive
                .queue_chat_sync(&chat, "archived", true, 100 + index)
                .expect("intent");
        }
        let (sink, accepted) = std::sync::mpsc::channel();
        worker.sync_sink = Some(sink);
        for _ in 0..3 {
            worker.pump_chat_sync();
        }
        let mut seen = Vec::new();
        while let Ok(job) = accepted.try_recv() {
            seen.push(job.chat);
        }
        assert_eq!(seen.len(), 2, "only the fresh intents dispatch");
        assert!(seen.iter().all(|chat| chat.starts_with("new")));
        for chat in &seen {
            let rev = worker
                .archive
                .queued_chat_sync(chat, "archived")
                .expect("queue")
                .expect("rev")
                .2;
            worker
                .handle_command(Command::ChatSyncFlushed {
                    chat: chat.clone(),
                    rev,
                    ok: true,
                })
                .await;
        }
        let pending: Vec<String> = worker
            .archive
            .pending_chat_syncs()
            .expect("reads")
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(
            pending.iter().all(|id| id.starts_with("old")),
            "exhausted four stay queued and quiet"
        );
    }

    #[tokio::test]
    async fn global_flight_ceiling_blocks_the_ninth_chat() {
        let (mut worker, _events, _inbox, _wa) = worker();
        for index in 0..9 {
            let chat = format!("g{index}@s.whatsapp.net");
            worker
                .archive
                .queue_chat_sync(&chat, "archived", true, index)
                .expect("intent");
        }
        let (sink, accepted) = std::sync::mpsc::channel();
        worker.sync_sink = Some(sink);
        worker.pump_chat_sync();
        worker.pump_chat_sync();
        let mut seen = Vec::new();
        while let Ok(job) = accepted.try_recv() {
            seen.push(job.chat);
        }
        assert_eq!(seen.len(), 8, "eight fly at most");
        let first = seen[0].clone();
        let rev = worker
            .archive
            .queued_chat_sync(&first, "archived")
            .expect("queue")
            .expect("rev")
            .2;
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: first,
                rev,
                ok: true,
            })
            .await;
        worker.pump_chat_sync();
        assert!(accepted.try_recv().is_ok(), "room frees the waiter");
    }

    #[tokio::test]
    async fn echo_before_deadline_does_not_send() {
        use std::time::{Duration, Instant};
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker
            .archive
            .queue_chat_sync(PEER, "archived", true, 1)
            .expect("intent");
        let now = Instant::now();
        worker
            .sync_retry_at
            .insert(PEER.into(), now + Duration::from_secs(300));
        let (sink, accepted) = std::sync::mpsc::channel();
        worker.sync_sink = Some(sink);
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, false, 0)))
            .await;
        assert!(accepted.try_recv().is_err(), "backoff respected");
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_some()
        );
        worker
            .sync_retry_at
            .insert(PEER.into(), now - Duration::from_secs(1));
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, false, 0)))
            .await;
        assert!(accepted.try_recv().is_ok(), "due echo flushes");
    }

    #[tokio::test]
    async fn newer_remote_change_during_flight_wins() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker
            .handle_command(Command::SetArchived(PEER.into(), true))
            .await;
        worker.sync_in_flight.insert((PEER.into(), 1), ());
        let _ = ui_events(&events);
        let later = whatsapp_rust::wacore::time::now_millis() + 60_000;
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, false, later)))
            .await;
        assert!(
            !worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived,
            "phone change applied"
        );
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_none(),
            "intent superseded"
        );
        let applied = ui_events(&events)
            .into_iter()
            .filter(|e| matches!(e, Event::ChatUpdated(_)))
            .count();
        assert_eq!(applied, 1, "ui follows the phone");
        // The stale success must not flip the state back.
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev: 1,
                ok: true,
            })
            .await;
        assert!(
            !worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived
        );
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_none()
        );
    }

    #[tokio::test]
    async fn failed_stale_op_leaves_newer_remote_state() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker
            .handle_command(Command::SetArchived(PEER.into(), true))
            .await;
        worker.sync_in_flight.insert((PEER.into(), 1), ());
        let later = whatsapp_rust::wacore::time::now_millis() + 60_000;
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, false, later)))
            .await;
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev: 1,
                ok: false,
            })
            .await;
        assert!(
            !worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived
        );
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_none()
        );
        assert!(
            ui_events(&events)
                .iter()
                .all(|e| !matches!(e, Event::Error(_))),
            "no failure to report"
        );
    }

    #[tokio::test]
    async fn local_intent_after_remote_then_old_echo() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, false, 1_000)))
            .await;
        worker
            .handle_command(Command::SetArchived(PEER.into(), true))
            .await;
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, false, 500)))
            .await;
        assert!(
            worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived,
            "local intent kept"
        );
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_some(),
            "intent kept"
        );
    }

    #[tokio::test]
    async fn restart_with_queued_intent_dispatches_again() {
        // Attempts live in memory while the queue lives in storage: a fresh
        // worker over the same persisted row must dispatch again. Restart
        // reopens the budget by design; only explicit intents reset it sooner.
        let archive = {
            let (worker, _events, _inbox, _wa) = worker();
            worker
                .archive
                .queue_chat_sync(PEER, "archived", true, 1)
                .expect("persisted intent");
            worker.archive
        };
        let (mut restarted, _events, _inbox, _wa) = worker();
        restarted.archive = archive;
        assert!(restarted.sync_attempts.is_empty());
        let (sink, accepted) = std::sync::mpsc::channel();
        restarted.sync_sink = Some(sink);
        restarted.pump_chat_sync();
        assert!(accepted.try_recv().is_ok(), "restart reopens dispatch");
    }

    #[tokio::test]
    async fn completed_intent_rejects_older_echo() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker
            .handle_command(Command::SetArchived(PEER.into(), true))
            .await;
        worker.sync_in_flight.insert((PEER.into(), 1), ());
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev: 1,
                ok: true,
            })
            .await;
        let _ = ui_events(&events);
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, false, 1)))
            .await;
        assert!(
            worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived,
            "stale echo ignored"
        );
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_none()
        );
        assert!(ui_events(&events).is_empty(), "nothing repaints");
    }

    #[tokio::test]
    async fn agreeing_echo_during_flight_then_old_echo() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker
            .handle_command(Command::SetArchived(PEER.into(), true))
            .await;
        worker.sync_in_flight.insert((PEER.into(), 1), ());
        let now = whatsapp_rust::wacore::time::now_millis();
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, true, now)))
            .await;
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev: 1,
                ok: true,
            })
            .await;
        let _ = ui_events(&events);
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, false, 1)))
            .await;
        assert!(
            worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived,
            "old echo ignored"
        );
        assert!(ui_events(&events).is_empty());
    }

    #[tokio::test]
    async fn agreeing_echo_midflight_sets_order_that_rejects_older_echo() {
        let make_worker = worker;
        let (mut worker, events, _inbox, _wa) = make_worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        // Local archive intent at T100, already dispatched.
        let rev = worker
            .archive
            .set_archived_queued(PEER, true, 100)
            .expect("intent");
        worker.sync_in_flight.insert((PEER.into(), rev), ());
        // Agreeing echo at T300 while flying: order remembered, queue kept.
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, true, 300)))
            .await;
        assert_eq!(
            worker.archive.sync_order(PEER).expect("order"),
            Some((300, true)),
            "agreeing echo sets the accepted order"
        );
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_some(),
            "queue stays until the completion"
        );
        // Completion records T100 but newest-wins keeps T300.
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev,
                ok: true,
            })
            .await;
        assert_eq!(
            worker.archive.sync_order(PEER).expect("order"),
            Some((300, true)),
            "completion never moves the marker back"
        );
        let _ = ui_events(&events);
        // A delayed older echo at T200 loses against T300.
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, false, 200)))
            .await;
        assert!(
            worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived,
            "older echo rejected"
        );
        assert!(ui_events(&events).is_empty());
        // A genuinely new state at T400 still applies.
        worker
            .handle_wa_event(Arc::new(archive_update(PEER, false, 400)))
            .await;
        assert!(
            !worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived,
            "newer state applies"
        );
        // Same story after reopening the same store: the order survived.
        let archive = worker.archive;
        let (mut restarted, _events, _inbox, _wa) = make_worker();
        restarted.archive = archive;
        restarted
            .handle_wa_event(Arc::new(archive_update(PEER, true, 350)))
            .await;
        assert!(
            !restarted
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived,
            "order persists across restart"
        );
    }

    #[tokio::test]
    async fn migration_with_two_flights_counts_both_until_done() {
        let (mut worker, _events, _inbox, _wa) = worker();
        let (sink, accepted) = std::sync::mpsc::channel();
        worker.sync_sink = Some(sink);
        // One dispatch on each id before the equivalence is known. The
        // migrated id holds the newer intent, so it wins with a fresh
        // revision and both flights go stale against the survivor.
        worker.archive.ensure_chat(PEER_LID, "Old").expect("chat");
        worker.archive.ensure_chat(PEER, "New").expect("chat");
        worker
            .handle_command(Command::SetArchived(PEER.into(), false))
            .await;
        worker
            .handle_command(Command::SetArchived(PEER_LID.into(), true))
            .await;
        let r1 = worker
            .archive
            .queued_chat_sync(PEER_LID, "archived")
            .expect("queue")
            .expect("intent")
            .2;
        let r2 = worker
            .archive
            .queued_chat_sync(PEER, "archived")
            .expect("queue")
            .expect("intent")
            .2;
        assert_eq!(worker.sync_in_flight.len(), 2, "two real tasks");
        assert!(accepted.try_recv().is_ok(), "first dispatch went out");
        assert!(accepted.try_recv().is_ok(), "second dispatch went out");
        worker.learn_lid("167650256810092", "4917663430455");
        assert_eq!(
            worker.sync_in_flight.len(),
            2,
            "migration keeps both tasks, it never overwrites one"
        );
        assert!(
            accepted.try_recv().is_err(),
            "no third task while two still fly"
        );
        // The older task fails once the survivor exists: it yields without
        // spending, and still no new task starts while one flies.
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER_LID.into(),
                rev: r1,
                ok: false,
            })
            .await;
        assert_eq!(worker.sync_in_flight.len(), 1, "one task left");
        assert!(
            accepted.try_recv().is_err(),
            "survivor waits for the remaining flight"
        );
        // The second task succeeds but is stale against the survivor, so
        // the survivor finally goes out on its own revision.
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev: r2,
                ok: true,
            })
            .await;
        assert!(accepted.try_recv().is_ok(), "survivor dispatched last");
        let r3 = worker
            .sync_in_flight
            .keys()
            .find(|(id, _)| id.as_str() == PEER)
            .map(|(_, rev)| *rev)
            .expect("survivor flight");
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev: r3,
                ok: true,
            })
            .await;
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_none(),
            "winner converged"
        );
        assert!(
            worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived,
            "newest intent value kept"
        );
    }

    #[tokio::test]
    async fn failed_completion_persist_keeps_intent_and_recovers() {
        let (mut worker, events, _inbox, _wa) = worker();
        let (sink, accepted) = std::sync::mpsc::channel();
        worker.sync_sink = Some(sink);
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker
            .handle_command(Command::SetArchived(PEER.into(), true))
            .await;
        assert!(accepted.try_recv().is_ok(), "dispatch went out");
        let rev = worker
            .sync_in_flight
            .keys()
            .find(|(id, _)| id.as_str() == PEER)
            .map(|(_, rev)| *rev)
            .expect("flight");
        // The order write fails while the queue delete would succeed: the
        // transaction must roll everything back instead of half-persisting.
        worker
            .archive
            .test_batch("CREATE TRIGGER order_abort BEFORE INSERT ON chat_sync_order BEGIN SELECT RAISE(ABORT, 'boom'); END;")
            .expect("trigger");
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev,
                ok: true,
            })
            .await;
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_some(),
            "intent kept for recovery"
        );
        assert!(
            worker.archive.sync_order(PEER).expect("order").is_none(),
            "no half-persisted order"
        );
        assert!(
            ui_events(&events)
                .iter()
                .any(|e| matches!(e, Event::Error(_))),
            "failure is visible"
        );
        assert!(worker.sync_retry_at.contains_key(PEER), "retry scheduled");
        worker
            .archive
            .test_batch("DROP TRIGGER order_abort")
            .expect("cleanup");
        // Recovery resends the same idempotent value and converges.
        worker.push_archive_sync(PEER);
        assert!(accepted.try_recv().is_ok(), "resent after persist failure");
        let retry = worker
            .sync_in_flight
            .keys()
            .find(|(id, _)| id.as_str() == PEER)
            .map(|(_, rev)| *rev)
            .expect("retry flight");
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER.into(),
                rev: retry,
                ok: true,
            })
            .await;
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_none(),
            "recovered"
        );
        assert!(
            worker.archive.sync_order(PEER).expect("order").is_some(),
            "order recorded"
        );
    }

    #[tokio::test]
    async fn migration_without_queue_follows_newest_accepted_state() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER_LID, "Old").expect("chat");
        worker.archive.ensure_chat(PEER, "New").expect("chat");
        // Old id archived long ago, number unarchived more recently.
        worker
            .archive
            .apply_remote_archive(PEER_LID, true, 100)
            .expect("order");
        worker
            .archive
            .apply_remote_archive(PEER, false, 200)
            .expect("order");
        worker.learn_lid("167650256810092", "4917663430455");
        assert!(
            !worker
                .archive
                .chat(PEER)
                .expect("chat")
                .expect("row")
                .archived,
            "newest accepted state wins without intent"
        );
        assert_eq!(
            worker.archive.sync_order(PEER).expect("order"),
            Some((200, false)),
            "accepted order kept"
        );
        assert!(
            ui_events(&events)
                .iter()
                .any(|e| matches!(e, Event::Chats(_))),
            "reconciled chat repainted"
        );
    }
    #[tokio::test]
    async fn migration_keeps_one_flight_for_both_ids() {
        let (mut worker, _events, _inbox, _wa) = worker();
        let (sink, accepted) = std::sync::mpsc::channel();
        worker.sync_sink = Some(sink);
        worker
            .handle_command(Command::SetArchived(PEER_LID.into(), true))
            .await;
        assert_eq!(worker.sync_in_flight.len(), 1);
        worker.learn_lid("167650256810092", "4917663430455");
        assert_eq!(
            worker.sync_in_flight.len(),
            1,
            "same task transferred, not duplicated"
        );
        assert!(accepted.try_recv().is_ok(), "first dispatch went out");
        assert!(
            accepted.try_recv().is_err(),
            "no second dispatch while flying"
        );
        let survivor = worker
            .archive
            .queued_chat_sync(PEER, "archived")
            .expect("queue")
            .expect("survivor");
        assert!(survivor.0, "winner value moved");
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER_LID.into(),
                rev: 1,
                ok: true,
            })
            .await;
        assert!(
            accepted.try_recv().is_ok(),
            "survivor dispatched after completion"
        );
        assert!(
            worker.archive.chat(PEER).expect("chat").is_none(),
            "no chat row invented"
        );
    }

    #[tokio::test]
    async fn migration_failed_task_dispatches_survivor() {
        let (mut worker, _events, _inbox, _wa) = worker();
        let (sink, accepted) = std::sync::mpsc::channel();
        worker.sync_sink = Some(sink);
        worker
            .handle_command(Command::SetArchived(PEER_LID.into(), true))
            .await;
        worker.learn_lid("167650256810092", "4917663430455");
        assert!(accepted.try_recv().is_ok());
        worker
            .handle_command(Command::ChatSyncFlushed {
                chat: PEER_LID.into(),
                rev: 1,
                ok: false,
            })
            .await;
        assert!(
            accepted.try_recv().is_ok(),
            "survivor dispatched after failure"
        );
        assert!(
            worker.sync_attempts.is_empty(),
            "superseded failure spends nothing"
        );
    }

    #[tokio::test]
    async fn migration_during_backoff_sends_nothing_early() {
        use std::time::{Duration, Instant};
        let (mut worker, _events, _inbox, _wa) = worker();
        let (sink, accepted) = std::sync::mpsc::channel();
        worker.sync_sink = Some(sink);
        worker
            .archive
            .queue_chat_sync(PEER_LID, "archived", true, 100)
            .expect("intent");
        let now = Instant::now();
        worker
            .sync_retry_at
            .insert(PEER_LID.into(), now + Duration::from_secs(300));
        worker.learn_lid("167650256810092", "4917663430455");
        assert!(accepted.try_recv().is_err(), "inherited deadline respected");
        assert!(
            worker
                .archive
                .queued_chat_sync(PEER, "archived")
                .expect("queue")
                .is_some()
        );
    }

    fn delete_update(jid: &str, timestamp: i64) -> wa_events::Event {
        wa_events::Event::DeleteChatUpdate(
            wa_events::DeleteChatUpdate::builder()
                .jid(jid.parse().expect("jid"))
                .delete_media(false)
                .timestamp(whatsapp_rust::wacore::time::from_secs(timestamp).expect("time"))
                .action(Box::new(wa::sync_action_value::DeleteChatAction::default()))
                .from_full_sync(false)
                .build(),
        )
    }

    fn archive_update(chat: &str, archived: bool, timestamp_ms: i64) -> wa_events::Event {
        wa_events::Event::ArchiveUpdate(
            wa_events::ArchiveUpdate::builder()
                .jid(chat.parse().expect("jid"))
                .timestamp(whatsapp_rust::wacore::time::from_millis(timestamp_ms).expect("time"))
                .action(Box::new(wa::sync_action_value::ArchiveChatAction {
                    archived: Some(archived),
                    ..Default::default()
                }))
                .from_full_sync(false)
                .build(),
        )
    }

    fn delete_for_me_update(chat: &str, id: &str, timestamp_ms: i64) -> wa_events::Event {
        wa_events::Event::DeleteMessageForMeUpdate(
            wa_events::DeleteMessageForMeUpdate::builder()
                .chat_jid(chat.parse().expect("jid"))
                .message_id(id.to_owned())
                .from_me(false)
                .timestamp(whatsapp_rust::wacore::time::from_millis(timestamp_ms).expect("time"))
                .action(Box::new(
                    wa::sync_action_value::DeleteMessageForMeAction::default(),
                ))
                .from_full_sync(false)
                .build(),
        )
    }

    fn clear_update(jid: &str, timestamp: i64, delete_starred: bool) -> wa_events::Event {
        wa_events::Event::ClearChatUpdate(
            wa_events::ClearChatUpdate::builder()
                .jid(jid.parse().expect("jid"))
                .delete_starred(delete_starred)
                .delete_media(false)
                .timestamp(whatsapp_rust::wacore::time::from_secs(timestamp).expect("time"))
                .action(Box::new(wa::sync_action_value::ClearChatAction::default()))
                .from_full_sync(false)
                .build(),
        )
    }

    fn ui_events(events: &std::sync::mpsc::Receiver<Event>) -> Vec<Event> {
        events.try_iter().collect()
    }

    #[tokio::test]
    async fn delete_then_late_history_stays_deleted() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        for (id, timestamp) in [("m1", 100), ("m2", 200)] {
            worker.store_message(
                Message {
                    chat: PEER.into(),
                    ..own_message(id, timestamp)
                },
                None,
                None,
            );
        }
        worker
            .handle_wa_event(Arc::new(delete_update(PEER, 200)))
            .await;
        assert!(worker.archive.chat(PEER).expect("chat").is_none());
        assert!(ui_events(&events).iter().any(|event| matches!(
            event,
            Event::ChatRemoved { chat } if chat == PEER
        )));
        // Late history below the barrier never resurrects the chat.
        worker.store_message(
            Message {
                chat: PEER.into(),
                ..own_message("late", 150)
            },
            None,
            None,
        );
        assert!(worker.archive.chat(PEER).expect("chat").is_none());
        assert!(worker.archive.message(PEER, "late").expect("row").is_none());
        // A genuinely new message revives the conversation.
        worker.store_message(
            Message {
                chat: PEER.into(),
                ..own_message("fresh", 250)
            },
            None,
            None,
        );
        assert_eq!(
            worker
                .archive
                .message(PEER, "fresh")
                .expect("row")
                .unwrap()
                .id,
            "fresh"
        );
    }

    #[tokio::test]
    async fn offline_delete_then_history_never_shows() {
        // Deleted while offline: the mutation lands on an absent row,
        // the tombstone persists, and the message arriving later via
        // history never reaches the database nor the screen.
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker
            .handle_wa_event(Arc::new(delete_for_me_update(PEER, "gone", 150)))
            .await;
        assert!(worker.archive.message(PEER, "gone").expect("row").is_none());
        assert!(
            worker
                .archive
                .is_tombstoned(PEER, "gone")
                .expect("tombstone")
        );
        worker.store_message(
            Message {
                chat: PEER.into(),
                ..own_message("gone", 100)
            },
            None,
            None,
        );
        worker.store_message(
            Message {
                chat: PEER.into(),
                ..own_message("kept", 200)
            },
            None,
            None,
        );
        let stored: Vec<String> = worker
            .archive
            .messages(PEER, None, 50)
            .expect("reads")
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(stored, vec!["kept".to_owned()]);
        let shown: Vec<String> = ui_events(&events)
            .into_iter()
            .filter_map(|event| match event {
                Event::Messages { messages, .. } => Some(messages),
                _ => None,
            })
            .flatten()
            .map(|row| row.id)
            .collect();
        assert_eq!(shown, vec!["kept".to_owned()]);
    }

    #[tokio::test]
    async fn duplicate_deletes_stay_idempotent() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker.store_message(
            Message {
                chat: PEER.into(),
                ..own_message("m1", 100)
            },
            None,
            None,
        );
        for _ in 0..2 {
            worker
                .handle_wa_event(Arc::new(delete_for_me_update(PEER, "m1", 150)))
                .await;
        }
        assert!(worker.archive.message(PEER, "m1").expect("row").is_none());
        assert!(worker.archive.is_tombstoned(PEER, "m1").expect("tombstone"));
        assert!(
            ui_events(&events)
                .iter()
                .any(|event| matches!(event, Event::MessageDeleted { .. }))
        );
    }

    #[tokio::test]
    async fn offline_batch_files_without_loss_or_dup() {
        // The offline queue shape: several messages, equal timestamps,
        // then the same batch again. Database and emission must agree
        // exactly, with no duplicate row.
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        for (id, timestamp) in [("a", 100), ("b", 100), ("c", 101), ("a", 100)] {
            worker.store_message(
                Message {
                    chat: PEER.into(),
                    ..own_message(id, timestamp)
                },
                None,
                None,
            );
        }
        let stored: Vec<String> = worker
            .archive
            .messages(PEER, None, 50)
            .expect("reads")
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(stored, vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]);
        let shown: Vec<String> = ui_events(&events)
            .into_iter()
            .filter_map(|event| match event {
                Event::Messages { messages, .. } => Some(messages),
                _ => None,
            })
            .flatten()
            .map(|row| row.id)
            .collect();
        assert_eq!(
            shown,
            vec![
                "a".to_owned(),
                "b".to_owned(),
                "c".to_owned(),
                "a".to_owned()
            ]
        );
    }

    #[tokio::test]
    async fn clear_keeps_newer_messages_and_announces_once() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        for (id, timestamp) in [("m1", 100), ("m2", 300)] {
            worker.store_message(
                Message {
                    chat: PEER.into(),
                    ..own_message(id, timestamp)
                },
                None,
                None,
            );
        }
        worker
            .handle_wa_event(Arc::new(clear_update(PEER, 200, true)))
            .await;
        assert!(worker.archive.message(PEER, "m1").expect("row").is_none());
        assert!(worker.archive.message(PEER, "m2").expect("row").is_some());
        assert!(worker.archive.chat(PEER).expect("chat").is_some());
        let cleared = ui_events(&events)
            .into_iter()
            .filter(|event| matches!(event, Event::ChatCleared { .. }))
            .count();
        assert_eq!(cleared, 1);
        // A duplicated clear still invalidates the interface through the
        // stored boundary: the archive may already be right while the
        // screen still shows the cleared range.
        worker
            .handle_wa_event(Arc::new(clear_update(PEER, 200, true)))
            .await;
        assert_eq!(
            ui_events(&events)
                .iter()
                .filter(|event| matches!(event, Event::ChatCleared { .. }))
                .count(),
            1,
            "duplicate still invalidates"
        );
    }

    #[tokio::test]
    async fn clear_keeping_starred_deletes_nothing() {
        // The archive cannot tell starred messages apart yet, so a clear
        // that must preserve them must not delete anything: losing
        // user-curated messages with no way back is worse than divergence.
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        for (id, timestamp) in [("m1", 100), ("m2", 200)] {
            worker.store_message(
                Message {
                    chat: PEER.into(),
                    ..own_message(id, timestamp)
                },
                None,
                None,
            );
        }
        worker
            .handle_wa_event(Arc::new(clear_update(PEER, 200, false)))
            .await;
        // Storing the fixtures above already emitted; only the clear may
        // speak from here on.
        let _ = ui_events(&events);
        assert!(worker.archive.message(PEER, "m1").expect("row").is_some());
        assert!(worker.archive.message(PEER, "m2").expect("row").is_some());
        assert_eq!(
            worker.archive.removal_point(PEER).expect("point"),
            None,
            "no barrier either: nothing was removed"
        );
        assert!(
            ui_events(&events).is_empty(),
            "an unapplied clear stays completely quiet"
        );
    }

    #[test]
    fn removed_files_survive_failed_reference_lookups() {
        let (mut worker, _events, _inbox, _wa) = worker();
        let dir = std::env::temp_dir().join(format!("zapfast-refcheck-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let file = dir.join("kept.jpg");
        std::fs::write(&file, b"kept").expect("writes");
        // Break the reference lookup itself: without answers from the
        // database, no candidate may be treated as unreferenced.
        worker
            .archive
            .drop_table_for_test("messages")
            .expect("breaks");
        // Collection runs on the tick, not on the delete path.
        worker.queue_media_gc(vec![file.clone()]);
        worker.pump_media_gc();
        assert!(file.exists(), "a failed lookup keeps every file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removed_files_cover_favorites_and_catalog() {
        let (mut worker, _events, _inbox, _wa) = worker();
        let dir = std::env::temp_dir().join(format!("zapfast-refcover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let favorite = dir.join("favorite.webp");
        let cataloged = dir.join("cataloged.webp");
        let orphan = dir.join("orphan.webp");
        for file in [&favorite, &cataloged, &orphan] {
            std::fs::write(file, b"sticker").expect("writes");
        }
        // No message references any of them: one is favorited, one is
        // cataloged, one is truly orphaned.
        assert!(
            worker
                .archive
                .toggle_sticker_favorite(&favorite)
                .expect("stars")
        );
        worker
            .archive
            .upsert_phone_sticker("abc123", &[], 0, 0.0)
            .expect("catalogs");
        worker
            .archive
            .set_sticker_path("abc123", &cataloged)
            .expect("paths");
        // Collection runs on the tick, not on the delete path.
        worker.queue_media_gc(vec![favorite.clone(), cataloged.clone(), orphan.clone()]);
        worker.pump_media_gc();
        assert!(favorite.exists(), "a favorite without messages survives");
        assert!(
            cataloged.exists(),
            "a cataloged copy without messages survives"
        );
        assert!(!orphan.exists(), "a true orphan is still reclaimed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn lid_delete_applies_to_the_phone_chat() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.learn_lid("167650256810092", "4917663430455");
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        worker.store_message(
            Message {
                chat: PEER.into(),
                ..own_message("m1", 100)
            },
            None,
            None,
        );
        worker
            .handle_wa_event(Arc::new(delete_update(PEER_LID, 100)))
            .await;
        assert!(worker.archive.chat(PEER).expect("chat").is_none());
        assert!(ui_events(&events).iter().any(|event| matches!(
            event,
            Event::ChatRemoved { chat } if chat == PEER
        )));
    }

    #[test]
    fn removed_media_files_drop_only_when_unreferenced() {
        let (mut worker, _events, _inbox, _wa) = worker();
        // Its own folder: the archive removal test owns zapfast-removal in
        // this process and both remove their folder.
        let dir =
            std::env::temp_dir().join(format!("zapfast-removal-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let shared = dir.join("shared.jpg");
        let orphan = dir.join("orphan.jpg");
        std::fs::write(&shared, b"shared").expect("writes");
        std::fs::write(&orphan, b"orphan").expect("writes");
        // Two surviving rows reference the same file; the removed row owns
        // the orphan alone.
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        for (id, timestamp, path) in [
            ("old", 100, orphan.to_str().expect("path")),
            ("keeper", 300, shared.to_str().expect("path")),
            ("keeper2", 400, shared.to_str().expect("path")),
        ] {
            let mut message = own_message(id, timestamp);
            message.chat = PEER.into();
            message.content = Content::Image {
                caption: None,
                media: crate::model::Media {
                    mime: "image/jpeg".into(),
                    size: 6,
                    width: None,
                    height: None,
                    path: Some(path.into()),
                    state: crate::model::MediaState::Idle,
                },
            };
            worker
                .archive
                .insert_message(&message, None)
                .expect("insert");
        }
        let removed = worker
            .archive
            .remove_chat_through(PEER, 200, false)
            .expect("removes");
        // The orphan is collected; the shared file still has live rows.
        assert!(removed.media.contains(&orphan));
        // Collection runs on the tick, not on the delete path.
        worker.queue_media_gc(removed.media);
        worker.pump_media_gc();
        assert!(!orphan.exists(), "unreferenced file is deleted");
        assert!(shared.exists(), "referenced file is preserved");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn media_gc_collects_in_bounded_rounds() {
        let (mut worker, _events, _inbox, _wa) = worker();
        // Missing files finish through the NotFound path: the budget is
        // what the test measures, not the disk.
        let paths: Vec<std::path::PathBuf> = (0..100)
            .map(|n| std::path::PathBuf::from(format!("/tmp/gc{n}")))
            .collect();
        worker.queue_media_gc(paths);
        // A double delete never queues twice.
        worker.queue_media_gc(vec![std::path::PathBuf::from("/tmp/gc0")]);
        assert_eq!(worker.media_gc.len(), 100);
        worker.pump_media_gc();
        assert_eq!(
            worker.media_gc.len(),
            100 - Worker::MEDIA_GC_PER_TICK,
            "one round keeps its budget"
        );
        worker.pump_media_gc();
        assert!(worker.media_gc.is_empty(), "the rest drains next");
        assert!(worker.media_gc_retry.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn forget_pdf_never_blocks_the_loop() {
        use std::time::{Duration, Instant};
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        // Occupy the reader on its own thread, exactly like a slow
        // rasterization does. The test never holds the guard itself, so
        // no await runs under the mutex on either side.
        let pdf = worker.pdf.clone();
        let held = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal = held.clone();
        let holder = std::thread::Builder::new()
            .name("pdf-lock-holder".into())
            .spawn(move || {
                let _guard = pdf
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                signal.store(true, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(Duration::from_secs(3));
            })
            .expect("holder thread");
        while !held.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        // ForgetPdf returns while the mutex is still held: invalidation
        // is synchronous, the wait is not.
        tokio::time::timeout(
            Duration::from_secs(5),
            worker.handle_command(Command::ForgetPdf),
        )
        .await
        .expect("forget returns without the mutex");
        // A trivial command behind it is processed first, before release.
        worker
            .handle_command(Command::SetArchived(PEER.into(), true))
            .await;
        assert!(
            ui_events(&events)
                .iter()
                .any(|event| matches!(event, Event::ChatUpdated(_))),
            "the loop stays alive under the lock"
        );
        holder.join().expect("holder releases");
        // After release, the deferred clear really ran.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let empty = worker
                .pdf
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty();
            if empty {
                break;
            }
            assert!(Instant::now() < deadline, "the deferred clear ran");
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn forget_pdf_stale_clear_keeps_the_new_document() {
        use std::time::{Duration, Instant};
        let (mut worker, _events, _inbox, _wa) = worker();
        let dir = std::env::temp_dir().join(format!("zapfast-pdf-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        let tiny = b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 100]/Resources<</Font<</F1 5 0 R>>>>/Contents 4 0 R>>endobj\n4 0 obj<</Length 36>>stream\nBT /F1 24 Tf 20 40 Td (Hi) Tj ET\nendstream\nendobj\n5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n";
        let first = dir.join("first.pdf");
        let second = dir.join("second.pdf");
        std::fs::write(&first, tiny).expect("writes");
        std::fs::write(&second, tiny).expect("writes");
        worker
            .pdf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .render(&first, 0, 400)
            .expect("renders first");
        let pdf = worker.pdf.clone();
        let held = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal = held.clone();
        let holder = std::thread::Builder::new()
            .name("pdf-guard-holder".into())
            .spawn(move || {
                let _guard = pdf
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                signal.store(true, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(Duration::from_secs(3));
            })
            .expect("holder");
        while !held.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        tokio::time::timeout(
            Duration::from_secs(5),
            worker.handle_command(Command::ForgetPdf),
        )
        .await
        .expect("forget returns");
        worker
            .handle_command(Command::RenderPdfPage {
                path: second.clone(),
                page: 0,
                width: 400,
            })
            .await;
        holder.join().expect("holder releases");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let done = !worker
                .pdf
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty();
            if done {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "new document survives the stale clear"
            );
            tokio::task::yield_now().await;
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn search_applies_only_the_newest_answer() {
        let (mut worker, events, _inbox, _wa) = worker();
        // Two answers arrive out of order: only the newest generation
        // paints the panel.
        worker.search_generation = 2;
        worker
            .handle_command(Command::SearchReady {
                generation: 1,
                query: "old".into(),
                chat: None,
                hits: Ok(vec![]),
            })
            .await;
        worker
            .handle_command(Command::SearchReady {
                generation: 2,
                query: "new".into(),
                chat: None,
                hits: Ok(vec![]),
            })
            .await;
        let queries: Vec<String> = ui_events(&events)
            .into_iter()
            .filter_map(|event| match event {
                Event::SearchHits { query, .. } => Some(query),
                _ => None,
            })
            .collect();
        assert_eq!(queries, vec!["new".to_owned()]);
    }

    #[tokio::test]
    async fn search_late_stale_answer_does_not_repaint() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.search_generation = 2;
        worker
            .handle_command(Command::SearchReady {
                generation: 2,
                query: "new".into(),
                chat: None,
                hits: Ok(vec![]),
            })
            .await;
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tx.send(()).expect("barrier arms");
        rx.await.expect("barrier passes");
        worker
            .handle_command(Command::SearchReady {
                generation: 1,
                query: "old".into(),
                chat: None,
                hits: Ok(vec![]),
            })
            .await;
        let queries: Vec<String> = ui_events(&events)
            .into_iter()
            .filter_map(|event| match event {
                Event::SearchHits { query, .. } => Some(query),
                _ => None,
            })
            .collect();
        assert_eq!(queries, vec!["new".to_owned()]);
    }

    #[tokio::test]
    async fn search_coalesces_while_two_fly() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker.search_in_flight = 2;
        worker.search_generation = 2;
        worker.spawn_search(None, "first".to_owned(), 50);
        assert_eq!(worker.search_in_flight, 2);
        assert!(worker.search_pending.is_some());
        let pending_gen = worker.search_generation;
        worker.spawn_search(None, "second".to_owned(), 50);
        assert_eq!(worker.search_in_flight, 2);
        assert_eq!(worker.search_generation, pending_gen + 1);
        worker
            .handle_command(Command::SearchReady {
                generation: 0,
                query: "stale".into(),
                chat: None,
                hits: Ok(vec![]),
            })
            .await;
        assert!(worker.search_pending.is_none());
        assert_eq!(worker.search_in_flight, 2);
    }

    #[tokio::test]
    async fn search_filters_a_message_deleted_in_flight() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        // Production hits always come from the database: store both
        // first, then delete one after the query ran.
        for (id, timestamp) in [("gone", 100), ("kept", 200)] {
            worker.store_message(
                Message {
                    chat: PEER.into(),
                    ..own_message(id, timestamp)
                },
                None,
                None,
            );
        }
        let gone = Message {
            chat: PEER.into(),
            ..own_message("gone", 100)
        };
        let kept = Message {
            chat: PEER.into(),
            ..own_message("kept", 200)
        };
        worker
            .archive
            .tombstone_message(PEER, "gone", 300)
            .expect("tombstone");
        worker.search_generation = 7;
        worker
            .handle_command(Command::SearchReady {
                generation: 7,
                query: "hi".into(),
                chat: None,
                hits: Ok(vec![gone, kept]),
            })
            .await;
        let shown: Vec<String> = ui_events(&events)
            .into_iter()
            .filter_map(|event| match event {
                Event::SearchHits { messages, .. } => Some(messages),
                _ => None,
            })
            .flatten()
            .map(|message| message.id)
            .collect();
        assert_eq!(shown, vec!["kept".to_owned()]);
    }

    #[tokio::test]
    async fn search_drops_hits_cleared_while_flying() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        for (id, timestamp) in [("old", 100), ("new", 200)] {
            worker.store_message(
                Message {
                    chat: PEER.into(),
                    ..own_message(id, timestamp)
                },
                None,
                None,
            );
        }
        let stale = vec![
            Message {
                chat: PEER.into(),
                ..own_message("old", 100)
            },
            Message {
                chat: PEER.into(),
                ..own_message("new", 200)
            },
        ];
        worker
            .archive
            .remove_chat_through(PEER, 150, false)
            .expect("clears");
        worker.search_generation = 9;
        worker
            .handle_command(Command::SearchReady {
                generation: 9,
                query: "hi".into(),
                chat: None,
                hits: Ok(stale),
            })
            .await;
        let shown: Vec<String> = ui_events(&events)
            .into_iter()
            .filter_map(|event| match event {
                Event::SearchHits { messages, .. } => Some(messages),
                _ => None,
            })
            .flatten()
            .map(|message| message.id)
            .collect();
        assert_eq!(shown, vec!["new".to_owned()]);
    }

    #[tokio::test]
    async fn search_leaves_the_loop_while_it_runs() {
        let (mut worker, events, _inbox, _wa) = worker();
        worker.archive.ensure_chat(PEER, "Peer").expect("chat");
        // The command returns with no hits event: the query runs beside
        // the loop instead of inside it.
        worker
            .handle_command(Command::SearchMessages {
                query: "hello".into(),
            })
            .await;
        assert!(
            ui_events(&events)
                .iter()
                .all(|event| !matches!(event, Event::SearchHits { .. })),
            "no synchronous answer"
        );
        // A trivial command behind it is processed immediately.
        worker
            .handle_command(Command::SetArchived(PEER.into(), true))
            .await;
        assert!(
            ui_events(&events)
                .iter()
                .any(|event| matches!(event, Event::ChatUpdated(_))),
            "the loop stays responsive"
        );
        // A background answer still applies through the generation gate.
        worker
            .handle_command(Command::SearchReady {
                generation: worker.search_generation,
                query: "hello".into(),
                chat: None,
                hits: Ok(vec![]),
            })
            .await;
        assert!(
            ui_events(&events)
                .iter()
                .any(|event| matches!(event, Event::SearchHits { .. })),
            "the answer still lands"
        );
    }

    #[test]
    fn download_limits_cap_declared_plus_slack() {
        let ten_mebibytes = 10 * 1024 * 1024;
        assert_eq!(
            download_limits_for(Some(ten_mebibytes)).max_bytes,
            ten_mebibytes + 1024 * 1024
        );
        assert_eq!(
            download_limits_for(None).max_bytes,
            Worker::DOWNLOAD_MAX_BYTES
        );
        assert_eq!(
            download_limits_for(Some(u64::MAX)).max_bytes,
            Worker::DOWNLOAD_MAX_BYTES,
            "a lying length cannot raise the ceiling"
        );
        assert_eq!(
            download_limits_for(Some(100)).timeout,
            Worker::DOWNLOAD_TIMEOUT
        );
    }

    #[test]
    fn limited_file_enforces_cap_and_resets_on_truncate() {
        use whatsapp_rust::download::DownloadWriter;
        let dir = std::env::temp_dir().join(format!("zapfast-limited-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let path = dir.join("sink.bin");
        let mut sink = LimitedFile::create(&path, 16).expect("creates");
        use std::io::{Seek, SeekFrom, Write};
        sink.write_all(&[7u8; 10]).expect("fits");
        assert!(sink.write_all(&[7u8; 7]).is_err(), "over budget fails");
        sink.write_all(&[7u8; 6]).expect("exact fit lands");
        sink.truncate(0).expect("truncate");
        // The library always rewinds after clearing; the position is not
        // part of the truncate contract.
        sink.seek(SeekFrom::Start(0)).expect("rewind after clear");
        sink.write_all(&[9u8; 16])
            .expect("budget restored after clear");
        let mut back = Vec::new();
        use std::io::Read;
        std::fs::File::open(&path)
            .expect("opens")
            .read_to_end(&mut back)
            .expect("reads");
        assert_eq!(back, vec![9u8; 16]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunked_streaming_never_holds_the_whole_file() {
        // 32 MiB through 8 KiB writes: the file is exact while no single
        // allocation ever holds more than one chunk on our side.
        let dir = std::env::temp_dir().join(format!("zapfast-stream-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let path = dir.join("big.bin");
        let mut sink = LimitedFile::create(&path, 40 * 1024 * 1024).expect("creates");
        use std::io::Write;
        let chunk = vec![3u8; 8192];
        for _ in 0..4096 {
            sink.write_all(&chunk).expect("streams");
        }
        assert_eq!(
            std::fs::metadata(&path).expect("stat").len(),
            32 * 1024 * 1024
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn temp_guard_cleans_up() {
        let dir = std::env::temp_dir().join(format!("zapfast-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let path = dir.join("partial.part");
        std::fs::write(&path, b"partial").expect("writes");
        drop(TempGuard::new(path.clone()));
        assert!(!path.exists(), "the partial file is gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn publish_moves_aside_and_restores() {
        let dir = std::env::temp_dir().join(format!("zapfast-publish-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.expect("creates");
        let dest = dir.join("photo.jpg");
        let temp = dir.join("photo.jpg.part");
        // Fresh publish lands the file.
        tokio::fs::write(&temp, b"new").await.expect("writes");
        publish_download(&temp, &dest).await.expect("publishes");
        assert_eq!(tokio::fs::read(&dest).await.expect("reads"), b"new");
        assert!(!temp.exists(), "temporary renamed away");
        // Over an existing copy the new file wins and no backup lingers.
        tokio::fs::write(&temp, b"newer").await.expect("writes");
        publish_download(&temp, &dest).await.expect("publishes");
        assert_eq!(tokio::fs::read(&dest).await.expect("reads"), b"newer");
        assert!(!dir.join("photo.jpg.bak").exists());
        // A missing temporary fails without touching the last valid copy.
        let missing = dir.join("gone.part");
        assert!(publish_download(&missing, &dest).await.is_err());
        assert_eq!(
            tokio::fs::read(&dest).await.expect("reads"),
            b"newer",
            "the last valid copy survives a failed publish"
        );
        assert!(!dir.join("photo.jpg.bak").exists());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn expired_media_classification() {
        assert!(is_expired_media_error(&anyhow::anyhow!(
            "Download failed with status: 403"
        )));
        assert!(is_expired_media_error(&anyhow::anyhow!(
            "not found/expired with status: 410"
        )));
        assert!(!is_expired_media_error(&anyhow::anyhow!(
            "Download failed with status: 500"
        )));
        assert!(!is_expired_media_error(&anyhow::anyhow!(
            "socket closed without a status"
        )));
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let picture = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 200, 30, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(picture)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .expect("encodes");
        bytes
    }

    #[test]
    fn image_validation_decodes_small_and_sniffs_large() {
        let dir = std::env::temp_dir().join(format!("zapfast-imgval-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        // Small and valid: headers plus a full decode.
        let small = dir.join("small.png");
        std::fs::write(&small, png_bytes(4, 4)).expect("writes");
        assert!(validate_image_file(&small, "image/png", 1024 * 1024).is_ok());
        // Truncated mid-file: the full decode below the threshold catches it.
        let mut cut = png_bytes(16, 16);
        cut.truncate(cut.len() / 2);
        let truncated = dir.join("cut.png");
        std::fs::write(&truncated, &cut).expect("writes");
        assert!(validate_image_file(&truncated, "image/png", 1024 * 1024).is_err());
        // Valid headers with a garbage tail past a tiny threshold: accepted
        // on headers alone, without reading the megabytes that follow.
        let mut big = png_bytes(4, 4);
        big.extend_from_slice(&[0u8; 1024 * 1024]);
        let padded = dir.join("padded.png");
        std::fs::write(&padded, &big).expect("writes");
        assert!(validate_image_file(&padded, "image/png", 100).is_ok());
        // Pure garbage, tens of megabytes: the header sniff fails fast
        // without ever holding pixels.
        let garbage = dir.join("garbage.bin");
        std::fs::write(&garbage, vec![0u8; 40 * 1024 * 1024]).expect("writes");
        assert!(validate_image_file(&garbage, "image/png", 1024 * 1024).is_err());
        // Empty is empty, at any threshold.
        let empty = dir.join("empty.png");
        std::fs::write(&empty, []).expect("writes");
        assert!(validate_image_file(&empty, "image/png", 1024 * 1024).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn image_validation_rejects_excessive_dimensions() {
        let dir = std::env::temp_dir().join(format!("zapfast-imgdim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let wide = dir.join("wide.png");
        std::fs::write(&wide, png_bytes(66000, 8)).expect("writes");
        assert!(validate_image_file(&wide, "image/png", 1024 * 1024 * 1024).is_err());
        // Header-only path skips pixel decoding, so only the explicit
        // budget check guards it: sides fit but pixels do not.
        let huge = dir.join("huge.bmp");
        std::fs::write(&huge, bmp_header_only(60000, 6000)).expect("writes");
        assert!(validate_image_file(&huge, "image/bmp", 0).is_err());
        // A structurally valid BMP (headers, payload, coherent sizes)
        // passes the dimensions path on every platform now that the
        // decoder ships everywhere.
        let tiny = dir.join("tiny.bmp");
        std::fs::write(&tiny, bmp_valid_tiny()).expect("writes");
        assert!(validate_image_file(&tiny, "image/bmp", 0).is_ok());
        // A header declaring pixels it does not carry still fails once a
        // full decode is required: truncation rejection is preserved.
        let cut = dir.join("cut.bmp");
        std::fs::write(&cut, bmp_header_only(4, 4)).expect("writes");
        assert!(validate_image_file(&cut, "image/bmp", 1024 * 1024).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bmp_view_decodes_and_send_reencodes_to_jpeg() {
        let bytes = bmp_valid_tiny();
        let decoded = image::load_from_memory(&bytes).expect("bmp decodes for viewing");
        assert_eq!(decoded.width(), 4);
        let jpeg = encode_jpeg(&decoded, 80).expect("send path reencodes to jpeg");
        assert!(jpeg.len() > 8);
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]);
    }

    /// Bare BMP header declaring dimensions with no pixel data. The
    /// dimensions path reads headers only, so a 360-megapixel claim costs
    /// nothing to fixture; its only possible defect is the pixel budget.
    fn bmp_header_only(width: i32, height: i32) -> Vec<u8> {
        bmp_header(width, height, 54, 0, Vec::new())
    }

    /// Structurally valid 4x4 BMP: headers, pixel payload, row padding and
    /// coherent sizes, so every platform accepts it on the dimensions path.
    fn bmp_valid_tiny() -> Vec<u8> {
        // 24-bit rows of 4 pixels need no padding: 12 bytes each.
        bmp_header(4, 4, 102, 48, vec![0u8; 48])
    }

    /// Minimal 24-bit BMP with the given file size, image size and pixel
    /// payload. Sizes must stay coherent or the file is malformed.
    fn bmp_header(
        width: i32,
        height: i32,
        file_size: u32,
        image_size: u32,
        pixels: Vec<u8>,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"BM");
        bytes.extend_from_slice(&file_size.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 4]);
        bytes.extend_from_slice(&54u32.to_le_bytes());
        bytes.extend_from_slice(&40u32.to_le_bytes());
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&24u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&image_size.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 16]);
        bytes.extend_from_slice(&pixels);
        bytes
    }

    #[test]
    fn image_dimension_boundaries_hold() {
        assert!(image_dimensions_acceptable(4, 4));
        assert!(image_dimensions_acceptable(65535, 1525));
        assert!(!image_dimensions_acceptable(0, 4));
        assert!(!image_dimensions_acceptable(4, 0));
        assert!(!image_dimensions_acceptable(65536, 8));
        assert!(!image_dimensions_acceptable(8, 65536));
        // Sides fit, pixels do not: 360 megapixels needs no fixture.
        assert!(!image_dimensions_acceptable(60000, 6000));
        assert!(!image_dimensions_acceptable(65535, 65535));
    }

    #[test]
    fn image_validations_run_concurrently() {
        // Eight parallel validations of the same small file: no shared
        // state, every one answers.
        let dir = std::env::temp_dir().join(format!("zapfast-imgpar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let file = dir.join("small.png");
        std::fs::write(&file, png_bytes(8, 8)).expect("writes");
        let answers: Vec<bool> = std::thread::scope(|scope| {
            (0..8)
                .map(|_| {
                    let file = file.clone();
                    scope
                        .spawn(move || validate_image_file(&file, "image/png", 1024 * 1024).is_ok())
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| handle.join().expect("joins"))
                .collect()
        });
        assert!(answers.iter().all(|ok| *ok));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn interrupted_publishes_restore_or_clean_up() {
        let dir = std::env::temp_dir().join(format!("zapfast-pubrecover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        // Backup without destination: the publish died mid-step and the
        // backup is still the last valid copy.
        std::fs::write(dir.join("photo.jpg.bak"), b"old").expect("writes");
        // Backup beside a destination: the publish completed and only its
        // cleanup was missed.
        std::fs::write(dir.join("done.jpg"), b"new").expect("writes");
        std::fs::write(dir.join("done.jpg.bak"), b"old").expect("writes");
        let (restored, preserved) = recover_interrupted_publishes(&dir);
        assert_eq!(restored, 1);
        assert!(preserved.is_empty(), "nothing still pending");
        assert_eq!(std::fs::read(dir.join("photo.jpg")).expect("reads"), b"old");
        assert!(!dir.join("photo.jpg.bak").exists());
        assert_eq!(std::fs::read(dir.join("done.jpg")).expect("reads"), b"new");
        assert!(!dir.join("done.jpg.bak").exists());
        let (restored, _) = recover_interrupted_publishes(&dir);
        assert_eq!(restored, 0, "second run is quiet");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_restore_keeps_everything() {
        let dir = std::env::temp_dir().join(format!("zapfast-pubfail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        // The destination slot is occupied by a directory, so the backup
        // cannot move back: nothing may be deleted.
        std::fs::create_dir_all(dir.join("stuck.jpg")).expect("creates");
        std::fs::write(dir.join("stuck.jpg.bak"), b"old").expect("writes");
        let (restored, preserved) = recover_interrupted_publishes(&dir);
        assert_eq!(restored, 0);
        assert_eq!(preserved, vec![dir.join("stuck.jpg.bak")]);
        assert!(dir.join("stuck.jpg.bak").exists(), "the backup stays");
        assert!(dir.join("stuck.jpg").is_dir(), "the slot is untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn age_file(file: &std::path::Path) {
        std::fs::File::options()
            .write(true)
            .open(file)
            .expect("opens")
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
            .expect("ages");
    }

    #[test]
    fn startup_flow_keeps_pending_backups_and_favorites() {
        // The full startup composition, aged past the sweep settle time
        // without waiting: recover first, then sweep with the unified
        // keep set. A favorite-only file, a cataloged file, and a backup
        // that cannot move back all survive; a true orphan does not.
        let (worker, _events, _inbox, _wa) = worker();
        let dir = std::env::temp_dir().join(format!("zapfast-startup-flow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        let favorite = dir.join("favorite.webp");
        let cataloged = dir.join("cataloged.webp");
        let orphan = dir.join("orphan.webp");
        for file in [&favorite, &cataloged, &orphan] {
            std::fs::write(file, b"sticker").expect("writes");
            age_file(file);
        }
        assert!(
            worker
                .archive
                .toggle_sticker_favorite(&favorite)
                .expect("stars")
        );
        worker
            .archive
            .upsert_phone_sticker("flow123", &[], 0, 0.0)
            .expect("catalogs");
        worker
            .archive
            .set_sticker_path("flow123", &cataloged)
            .expect("paths");
        std::fs::create_dir_all(dir.join("stuck.jpg")).expect("creates");
        std::fs::write(dir.join("stuck.jpg.bak"), b"old").expect("writes");
        age_file(&dir.join("stuck.jpg.bak"));
        std::fs::write(dir.join("restored.jpg.bak"), b"old").expect("writes");
        // The exact startup composition: proven references, then recover,
        // then the preserved backups join the same keep set.
        let protected = worker.archive.protected_files().expect("provable");
        let mut keep = sweep_keep_set(protected, &[]);
        let (restored, preserved) = recover_interrupted_publishes(&dir);
        assert_eq!(restored, 1, "the orphaned backup moves back");
        keep.extend(sweep_keep_set(std::collections::HashSet::new(), &preserved));
        let freed = crate::cache::sweep(&dir, &|path| {
            keep.contains(&path.to_string_lossy().into_owned())
        });
        assert!(favorite.exists(), "favorite-only survives the sweep");
        assert!(cataloged.exists(), "cataloged survives the sweep");
        assert!(
            dir.join("stuck.jpg.bak").exists(),
            "pending backup survives"
        );
        assert!(dir.join("restored.jpg").exists(), "restored file stays");
        assert!(!orphan.exists(), "a true aged orphan is reclaimed");
        assert_eq!(freed.files, 1, "only the orphan went");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn damaged_favorites_abort_protection() {
        let (mut worker, _events, _inbox, _wa) = worker();
        worker
            .archive
            .set_meta("sticker_favorites", "not json")
            .expect("damages");
        assert!(
            worker
                .archive
                .sticker_favorites_strict()
                .expect("reads")
                .is_none(),
            "damaged list is unprovable, not empty"
        );
        assert!(
            worker.archive.protected_files().is_none(),
            "no proof, no cleanup anywhere"
        );
        let dir = std::env::temp_dir().join(format!("zapfast-damaged-fav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let file = dir.join("candidate.jpg");
        std::fs::write(&file, b"candidate").expect("writes");
        worker.queue_media_gc(vec![file.clone()]);
        worker.pump_media_gc();
        assert!(file.exists(), "unprovable keeps every file");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
