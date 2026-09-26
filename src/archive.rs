//! SQLite archive of chats, messages, contacts, and stickers.
//!
//! Each message keeps its raw protobuf because attachment download keys may be
//! needed long after history sync.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::model::{Chat, ChatKind, Contact, Content, Delivery, LastMessage, Message};

mod encryption;
mod polls;
mod receipts;
pub use polls::PollVote;

/// Recent phone sticker metadata, last-used time, and optional local file.
#[derive(Clone, Debug)]
pub struct PhoneSticker {
    pub hash: String,
    pub raw: Vec<u8>,
    pub last_used: i64,
    pub path: Option<std::path::PathBuf>,
}

/// Downloaded chat sticker with its last-seen time and source message.
#[derive(Clone, Debug)]
pub struct ArchivedSticker {
    pub last_used: i64,
    pub path: std::path::PathBuf,
    pub raw: Option<Vec<u8>>,
}

/// One favorite sticker sync state, keyed by content hash so a sticker
/// filed from any origin stays one favorite. Adapted from upstream ZapFast
/// (crmne/zapfast, MIT): whether it is a favorite, when that last changed
/// on either side, the encoded references the phone needs to fetch it, and
/// whether the phone has been told.
#[derive(Clone, Debug, PartialEq)]
pub struct FavoriteStickerSync {
    pub favorite: bool,
    pub updated_at: i64,
    pub action: Option<Vec<u8>>,
    pub pushed: bool,
}

/// What a chat removal took out. existed is false for a replayed sync
/// action with nothing left to remove. media lists the attachment paths
/// the removed messages referenced; a file is deleted only when no
/// surviving message still references it.
#[derive(Clone, Debug, Default)]
pub struct Removed {
    pub existed: bool,
    /// Whether any row actually disappeared: replays that delete nothing
    /// stay quiet instead of refreshing the interface twice.
    pub changed: bool,
    pub media: Vec<std::path::PathBuf>,
}

pub struct Archive {
    connection: Connection,
}

pub type Result<T> = std::result::Result<T, rusqlite::Error>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS chats (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    last_activity INTEGER NOT NULL DEFAULT 0,
    unread INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0,
    pinned INTEGER NOT NULL DEFAULT 0,
    muted_until INTEGER
);
CREATE TABLE IF NOT EXISTS messages (
    chat TEXT NOT NULL,
    id TEXT NOT NULL,
    sender TEXT NOT NULL,
    sender_name TEXT,
    from_me INTEGER NOT NULL,
    timestamp INTEGER NOT NULL,
    content TEXT NOT NULL,
    status INTEGER NOT NULL DEFAULT 0,
    quoted TEXT,
    reactions TEXT NOT NULL DEFAULT '[]',
    edited INTEGER NOT NULL DEFAULT 0,
    raw BLOB,
    PRIMARY KEY (chat, id)
);
CREATE INDEX IF NOT EXISTS messages_by_time ON messages (chat, timestamp);
CREATE TABLE IF NOT EXISTS contacts (
    id TEXT PRIMARY KEY,
    full_name TEXT,
    push_name TEXT
);
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS lids (
    lid TEXT PRIMARY KEY,
    pn TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS stickers (
    hash TEXT PRIMARY KEY,
    raw BLOB NOT NULL,
    last_used INTEGER NOT NULL DEFAULT 0,
    weight REAL NOT NULL DEFAULT 0,
    path TEXT
);
CREATE TABLE IF NOT EXISTS favorite_stickers (
    hash TEXT PRIMARY KEY,
    favorite INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    action BLOB,
    pushed INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS group_receipts (
    chat TEXT NOT NULL,
    id TEXT NOT NULL,
    recipient TEXT NOT NULL,
    expected INTEGER NOT NULL DEFAULT 0,
    status INTEGER NOT NULL DEFAULT 0,
    delivered_at INTEGER,
    read_at INTEGER,
    played_at INTEGER,
    PRIMARY KEY (chat, id, recipient)
);
CREATE TABLE IF NOT EXISTS chat_removals (
    chat TEXT PRIMARY KEY,
    through INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS chat_sync_queue (
    chat TEXT NOT NULL,
    setting TEXT NOT NULL,
    value INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    rev INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (chat, setting)
);
CREATE TABLE IF NOT EXISTS message_tombstones (
    chat TEXT NOT NULL,
    id TEXT NOT NULL,
    deleted_ms INTEGER NOT NULL,
    PRIMARY KEY (chat, id)
);
CREATE TABLE IF NOT EXISTS chat_sync_order (
    chat TEXT PRIMARY KEY,
    order_ms INTEGER NOT NULL,
    archived INTEGER NOT NULL
);
CREATE TRIGGER IF NOT EXISTS delete_group_receipts AFTER DELETE ON messages BEGIN
    DELETE FROM group_receipts WHERE chat = OLD.chat AND id = OLD.id;
END;
";

const CHAT_COLUMNS: &str =
    "c.id, c.name, c.kind, c.last_activity, c.unread, c.archived, c.pinned, c.muted_until,
                    m.from_me, m.sender_name, m.content, m.status, m.sender, c.participants, c.read_only,
                    c.pinned_at, c.ephemeral_expiration, c.community";

/// Adds columns introduced after the initial schema when missing.
const MIGRATIONS: &[(&str, &str, &str)] = &[
    ("messages", "thumbnail", "BLOB"),
    ("messages", "mentions", "TEXT NOT NULL DEFAULT '[]'"),
    ("chats", "participants", "TEXT NOT NULL DEFAULT '[]'"),
    ("chats", "read_only", "INTEGER NOT NULL DEFAULT 0"),
    ("chats", "community", "INTEGER NOT NULL DEFAULT 0"),
    ("messages", "forwarded", "INTEGER NOT NULL DEFAULT 0"),
    ("messages", "delivered_at", "INTEGER"),
    ("messages", "read_at", "INTEGER"),
    ("chats", "read_through", "INTEGER"),
    ("chats", "pending_read", "INTEGER"),
    ("chats", "ephemeral_expiration", "INTEGER"),
    ("chats", "ephemeral_setting_timestamp", "INTEGER"),
    ("chats", "pinned_at", "INTEGER NOT NULL DEFAULT 0"),
    ("chats", "pin_updated_at", "INTEGER"),
    ("chats", "mute_updated_at", "INTEGER"),
    ("chat_sync_queue", "rev", "INTEGER NOT NULL DEFAULT 0"),
];
const CHAT_JOIN: &str = "FROM chats c
             LEFT JOIN messages m ON m.chat = c.id AND m.rowid = (
                 SELECT rowid FROM messages WHERE chat = c.id ORDER BY timestamp DESC, rowid DESC LIMIT 1
             )";

fn chat_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Chat> {
    let content: Option<String> = row.get(10)?;
    let last = match content {
        Some(content) => {
            let content: Content = serde_json::from_str(&content).unwrap_or(Content::Unsupported {
                what: "unreadable".into(),
            });
            Some(LastMessage {
                from_me: row.get(8)?,
                sender: row.get::<_, Option<String>>(12)?.unwrap_or_default(),
                sender_name: row.get(9)?,
                summary: content.summary(),
                full: content.full_summary(),
                status: status_from_rank(row.get(11)?),
            })
        }
        None => None,
    };
    let kind: String = row.get(2)?;
    let participants: String = row.get(13)?;
    Ok(Chat {
        id: row.get(0)?,
        name: row.get(1)?,
        kind: kind_from_name(&kind),
        last_activity: row.get(3)?,
        unread: row.get(4)?,
        archived: row.get(5)?,
        pinned: row.get(6)?,
        pinned_at: row.get(15)?,
        muted_until: row.get(7)?,
        last,
        participants: serde_json::from_str(&participants).unwrap_or_default(),
        read_only: row.get(14)?,
        community: row.get(17)?,
        ephemeral_expiration: row
            .get::<_, Option<u32>>(16)?
            .filter(|expiration| *expiration != 0),
    })
}

fn status_rank(status: Delivery) -> i64 {
    match status {
        Delivery::None => 0,
        Delivery::Pending => 1,
        Delivery::Sent => 2,
        Delivery::Delivered => 3,
        Delivery::Read => 4,
        Delivery::Played => 5,
        Delivery::Failed => 6,
    }
}

/// Timestamp column for a remembered delivery stage.
fn stamp_column(status: Delivery) -> Option<&'static str> {
    match status {
        Delivery::Delivered => Some("delivered_at"),
        Delivery::Read | Delivery::Played => Some("read_at"),
        _ => None,
    }
}

fn status_from_rank(rank: i64) -> Delivery {
    match rank {
        1 => Delivery::Pending,
        2 => Delivery::Sent,
        3 => Delivery::Delivered,
        4 => Delivery::Read,
        5 => Delivery::Played,
        6 => Delivery::Failed,
        _ => Delivery::None,
    }
}

fn kind_name(kind: ChatKind) -> &'static str {
    match kind {
        ChatKind::Direct => "direct",
        ChatKind::Group => "group",
        ChatKind::Broadcast => "broadcast",
    }
}

fn kind_from_name(name: &str) -> ChatKind {
    match name {
        "group" => ChatKind::Group,
        "broadcast" => ChatKind::Broadcast,
        _ => ChatKind::Direct,
    }
}

impl Archive {
    /// Unlocks the on-disk archive with its OS keyring key, migrating plaintext
    /// archives before their first encrypted use. Never falls back to plaintext.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let key = encryption::key_for(path)?;
        Self::open_with_key(path, &key)
    }

    fn open_with_key(path: &Path, key: &[u8; 32]) -> anyhow::Result<Self> {
        Ok(Self::prepare(encryption::open(path, key)?)?)
    }

    pub fn in_memory() -> Result<Self> {
        Self::prepare(Connection::open_in_memory()?)
    }

    /// Runs raw SQL in tests: fault-injection triggers and legacy rows.
    #[cfg(test)]
    pub fn test_batch(&self, sql: &str) -> Result<()> {
        self.connection.execute_batch(sql)?;
        Ok(())
    }

    fn prepare(connection: Connection) -> Result<Self> {
        // A second writer waits instead of failing at once. Single-writer
        // today, so this only removes a latent SQLITE_BUSY trap.
        connection.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA busy_timeout = 5000;",
        )?;
        connection.execute_batch(SCHEMA)?;
        connection.execute_batch(polls::SCHEMA)?;
        for (table, column, definition) in MIGRATIONS {
            let exists = connection
                .prepare(&format!("PRAGMA table_info({table})"))?
                .query_map([], |row| row.get::<_, String>(1))?
                .any(|name| name.as_deref() == Ok(*column));
            if !exists {
                connection.execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN {column} {definition}"
                ))?;
            }
        }
        Ok(Self { connection })
    }

    /// Creates a chat or replaces a phone-number title with a better name.
    pub fn upsert_chat(&self, chat: &Chat) -> Result<()> {
        self.connection.execute(
            "INSERT INTO chats (id, name, kind, last_activity, unread, archived, pinned, muted_until, pinned_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                last_activity = MAX(last_activity, excluded.last_activity),
                archived = excluded.archived,
                pinned = CASE WHEN pin_updated_at IS NULL THEN excluded.pinned ELSE pinned END,
                pinned_at = CASE WHEN pin_updated_at IS NULL THEN excluded.pinned_at ELSE pinned_at END,
                muted_until = CASE WHEN mute_updated_at IS NULL THEN excluded.muted_until ELSE muted_until END",
            params![
                chat.id,
                chat.name,
                kind_name(chat.kind),
                chat.last_activity,
                chat.unread,
                chat.archived,
                chat.pinned,
                chat.muted_until,
                chat.pinned_at,
            ],
        )?;
        Ok(())
    }

    /// Inserts a chat row only when missing.
    pub fn ensure_chat(&self, id: &str, name: &str) -> Result<()> {
        self.connection.execute(
            "INSERT OR IGNORE INTO chats (id, name, kind) VALUES (?1, ?2, ?3)",
            params![id, name, kind_name(ChatKind::from_id(id))],
        )?;
        Ok(())
    }

    /// Updates group subject, members, and posting permission.
    pub fn set_group_info(
        &self,
        id: &str,
        name: Option<&str>,
        participants: &[String],
        read_only: bool,
    ) -> Result<()> {
        self.connection.execute(
            "UPDATE chats SET name = COALESCE(?2, name), participants = ?3, read_only = ?4 WHERE id = ?1",
            params![
                id,
                name,
                serde_json::to_string(participants).unwrap_or_else(|_| "[]".into()),
                read_only
            ],
        )?;
        Ok(())
    }

    /// Persists whether a group is the parent container of a Community.
    pub fn set_group_community(&self, id: &str, community: bool) -> Result<()> {
        self.connection.execute(
            "UPDATE chats SET community = ?2 WHERE id = ?1",
            params![id, community],
        )?;
        Ok(())
    }

    pub fn rename_chat(&self, id: &str, name: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE chats SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    pub fn set_archived(&self, id: &str, archived: bool) -> Result<()> {
        self.connection.execute(
            "UPDATE chats SET archived = ?2 WHERE id = ?1",
            params![id, archived],
        )?;
        Ok(())
    }

    /// Allocates the next intent revision from a persistent counter. Unlike
    /// MAX(rev) over the live queue, this never reuses a revision after rows
    /// are cleared, so a stale completion cannot match a fresh intent.
    fn alloc_sync_rev(&self) -> Result<i64> {
        // Upgrades may already hold queued rows with old revisions: the
        // counter starts above both the stored counter and the live rows.
        let stored: i64 = self
            .meta("sync_rev")?
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let live: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(rev), 0) FROM chat_sync_queue",
            [],
            |row| row.get(0),
        )?;
        let next = stored.max(live) + 1;
        self.set_meta("sync_rev", &next.to_string())?;
        Ok(next)
    }

    /// Remembers a chat setting the phone has not confirmed yet and returns
    /// its monotonic revision. Newer intents overwrite older ones; revisions
    /// tell stale completions apart when responses arrive out of order.
    pub fn queue_chat_sync(
        &self,
        chat: &str,
        setting: &str,
        value: bool,
        now_ms: i64,
    ) -> Result<i64> {
        let rev = self.alloc_sync_rev()?;
        self.connection.execute(
            "INSERT INTO chat_sync_queue (chat, setting, value, updated_ms, rev) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(chat, setting) DO UPDATE SET value = excluded.value,
             updated_ms = excluded.updated_ms, rev = excluded.rev",
            params![chat, setting, value, now_ms, rev],
        )?;
        Ok(rev)
    }

    /// Newest unconfirmed intent for one chat setting, if any, with its revision.
    pub fn queued_chat_sync(&self, chat: &str, setting: &str) -> Result<Option<(bool, i64, i64)>> {
        self.connection
            .query_row(
                "SELECT value, updated_ms, rev FROM chat_sync_queue WHERE chat = ?1 AND setting = ?2",
                params![chat, setting],
                |row| Ok((row.get::<_, bool>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
            )
            .optional()
    }

    /// Every chat setting with an unconfirmed intent, oldest first.
    pub fn pending_chat_syncs(&self) -> Result<Vec<(String, String)>> {
        let mut statement = self
            .connection
            .prepare("SELECT chat, setting FROM chat_sync_queue ORDER BY updated_ms")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Drops one confirmed intent.
    pub fn clear_chat_sync(&self, chat: &str, setting: &str) -> Result<()> {
        self.connection.execute(
            "DELETE FROM chat_sync_queue WHERE chat = ?1 AND setting = ?2",
            params![chat, setting],
        )?;
        Ok(())
    }

    /// Drops every intent for a chat that no longer exists.
    pub fn clear_chat_syncs_for(&self, chat: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM chat_sync_queue WHERE chat = ?1", params![chat])?;
        Ok(())
    }

    /// Last accepted archive order per chat: local completions record their
    /// intent time, remote applications their event time. Survives restarts
    /// so an older echo can never flip the state back afterwards.
    pub fn sync_order(&self, chat: &str) -> Result<Option<(i64, bool)>> {
        self.connection
            .query_row(
                "SELECT order_ms, archived FROM chat_sync_order WHERE chat = ?1",
                params![chat],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)),
            )
            .optional()
    }

    /// Records an accepted archive state, keeping the newest order only.
    pub fn record_sync_order(&self, chat: &str, order_ms: i64, archived: bool) -> Result<()> {
        self.connection.execute(
            "INSERT INTO chat_sync_order (chat, order_ms, archived) VALUES (?1, ?2, ?3)
             ON CONFLICT(chat) DO UPDATE SET order_ms = excluded.order_ms, archived = excluded.archived
             WHERE excluded.order_ms >= chat_sync_order.order_ms",
            params![chat, order_ms, archived],
        )?;
        Ok(())
    }

    /// Applies one confirmed remote archive state together with its order
    /// marker and the intent cleanup in a single transaction: either the
    /// whole acceptance persists or nothing does.
    pub fn apply_remote_archive(&self, chat: &str, archived: bool, remote_ms: i64) -> Result<()> {
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "UPDATE chats SET archived = ?2 WHERE id = ?1",
            params![chat, archived],
        )?;
        transaction.execute(
            "INSERT INTO chat_sync_order (chat, order_ms, archived) VALUES (?1, ?2, ?3)
             ON CONFLICT(chat) DO UPDATE SET order_ms = excluded.order_ms, archived = excluded.archived
             WHERE excluded.order_ms >= chat_sync_order.order_ms",
            params![chat, remote_ms, archived],
        )?;
        transaction.execute(
            "DELETE FROM chat_sync_queue WHERE chat = ?1 AND setting = ?2",
            params![chat, "archived"],
        )?;
        transaction.commit()?;
        Ok(())
    }
    /// Completes one local intent transactionally: removes exactly its queue
    /// row and records its order marker together, returning what was sent.
    /// A replaced row yields nothing, so a newer intent is never settled.
    pub fn complete_chat_sync(&self, chat: &str, rev: i64) -> Result<Option<(bool, i64)>> {
        let transaction = self.connection.unchecked_transaction()?;
        let completed: Option<(bool, i64)> = transaction
            .query_row(
                "SELECT value, updated_ms FROM chat_sync_queue WHERE chat = ?1 AND setting = ?2 AND rev = ?3",
                params![chat, "archived", rev],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((value, updated)) = completed else {
            return Ok(None);
        };
        transaction.execute(
            "DELETE FROM chat_sync_queue WHERE chat = ?1 AND setting = ?2 AND rev = ?3",
            params![chat, "archived", rev],
        )?;
        transaction.execute(
            "INSERT INTO chat_sync_order (chat, order_ms, archived) VALUES (?1, ?2, ?3)
             ON CONFLICT(chat) DO UPDATE SET order_ms = excluded.order_ms, archived = excluded.archived
             WHERE excluded.order_ms >= chat_sync_order.order_ms",
            params![chat, updated, value],
        )?;
        transaction.commit()?;
        Ok(Some((value, updated)))
    }
    /// Drops an intent only for the exact revision a completion attempted,
    /// so a stale completion cannot erase a fresh intent that reused nothing.
    pub fn clear_chat_sync_if_rev(&self, chat: &str, setting: &str, rev: i64) -> Result<()> {
        self.connection.execute(
            "DELETE FROM chat_sync_queue WHERE chat = ?1 AND setting = ?2 AND rev = ?3",
            params![chat, setting, rev],
        )?;
        Ok(())
    }

    /// Applies one local archive change together with its sync intent in a
    /// single transaction: the interface never shows a state the queue lost.
    pub fn set_archived_queued(&self, chat: &str, archived: bool, now_ms: i64) -> Result<i64> {
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "UPDATE chats SET archived = ?2 WHERE id = ?1",
            params![chat, archived],
        )?;
        let rev = self.alloc_sync_rev()?;
        transaction.execute(
            "INSERT INTO chat_sync_queue (chat, setting, value, updated_ms, rev) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(chat, setting) DO UPDATE SET value = excluded.value, updated_ms = excluded.updated_ms, rev = excluded.rev",
            params![chat, "archived", archived, now_ms, rev],
        )?;
        transaction.commit()?;
        Ok(rev)
    }

    /// Marks one message id as deleted for this device, before removing
    /// its row, so a late history replay cannot resurrect it.
    pub fn tombstone_message(&self, chat: &str, id: &str, now_ms: i64) -> Result<()> {
        self.connection.execute(
            "INSERT INTO message_tombstones (chat, id, deleted_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(chat, id) DO NOTHING",
            params![chat, id, now_ms],
        )?;
        Ok(())
    }

    /// Whether this message id was deleted for this device.
    pub fn is_tombstoned(&self, chat: &str, id: &str) -> Result<bool> {
        self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM message_tombstones WHERE chat = ?1 AND id = ?2)",
            params![chat, id],
            |row| row.get(0),
        )
    }

    pub fn set_pinned(&self, id: &str, pinned: bool) -> Result<()> {
        self.set_pinned_at(id, pinned, jiff::Timestamp::now().as_millisecond())
    }

    /// Apply app-state in timestamp order. A later history chunk has no state
    /// version and must not overwrite a pin/unpin already received from sync.
    pub fn set_pinned_at(&self, id: &str, pinned: bool, timestamp: i64) -> Result<()> {
        self.connection.execute(
            "UPDATE chats SET pinned = ?2, pinned_at = CASE WHEN ?2 THEN ?3 ELSE 0 END,
                pin_updated_at = ?3 WHERE id = ?1
                AND (pin_updated_at IS NULL OR pin_updated_at <= ?3)",
            params![id, pinned, timestamp],
        )?;
        Ok(())
    }

    pub fn set_muted(&self, id: &str, until: Option<i64>) -> Result<()> {
        self.set_muted_at(id, until, jiff::Timestamp::now().as_millisecond())
    }

    /// Keep mute/unmute actions across history replay, including actions that
    /// precede the initial chat snapshot and older app-state replay.
    pub fn set_muted_at(&self, id: &str, until: Option<i64>, timestamp: i64) -> Result<()> {
        self.connection.execute(
            "UPDATE chats SET muted_until = ?2, mute_updated_at = ?3 WHERE id = ?1
                AND (mute_updated_at IS NULL OR mute_updated_at <= ?3)",
            params![id, until, timestamp],
        )?;
        Ok(())
    }

    /// Applies disappearing-message metadata unless a newer setting is stored.
    pub fn set_ephemeral(&self, id: &str, expiration: u32, setting_timestamp: i64) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE chats SET ephemeral_expiration = ?2, ephemeral_setting_timestamp = ?3
             WHERE id = ?1 AND (ephemeral_setting_timestamp IS NULL OR ephemeral_setting_timestamp <= ?3)",
            params![id, expiration, setting_timestamp],
        )? > 0)
    }

    /// Returns the chat timer, including zero for an explicitly disabled timer.
    pub fn ephemeral_expiration(&self, id: &str) -> Result<Option<u32>> {
        self.connection
            .query_row(
                "SELECT ephemeral_expiration FROM chats WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map(Option::flatten)
    }

    pub fn mark_read(&self, id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE chats SET unread = 0,
             read_through = MAX(COALESCE(read_through, 0), last_activity) WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// A read on another device covers messages up to its position, not newer
    /// arrivals. Keep the position across restarts and history replays.
    pub fn mark_read_through(&self, id: &str, timestamp: i64) -> Result<()> {
        self.connection.execute(
            "UPDATE chats SET read_through = MAX(COALESCE(read_through, 0), ?2),
             unread = MIN(unread, (SELECT COUNT(*) FROM messages
                 WHERE chat = ?1 AND from_me = 0
                 AND timestamp > MAX(COALESCE(read_through, 0), ?2))) WHERE id = ?1",
            params![id, timestamp],
        )?;
        Ok(())
    }

    /// A message id disambiguates rapid messages with the same second-level
    /// timestamp. A receipt for the first must leave the later messages unread.
    pub fn mark_read_to(&self, chat: &str, message: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE chats SET
             read_through = MAX(COALESCE(read_through, 0),
                 (SELECT timestamp FROM messages WHERE chat = ?1 AND id = ?2)),
             unread = MIN(unread, (SELECT COUNT(*) FROM messages m
                 JOIN messages boundary ON boundary.chat = m.chat AND boundary.id = ?2
                 WHERE m.chat = ?1 AND m.from_me = 0
                 AND (m.timestamp > boundary.timestamp
                      OR (m.timestamp = boundary.timestamp AND m.rowid > boundary.rowid))))
             WHERE id = ?1 AND EXISTS(SELECT 1 FROM messages WHERE chat = ?1 AND id = ?2)",
            params![chat, message],
        )?;
        Ok(())
    }

    pub fn read_through(&self, id: &str) -> Result<Option<i64>> {
        self.connection
            .query_row(
                "SELECT read_through FROM chats WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map(Option::flatten)
    }

    pub fn queue_read_sync(&self, id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE chats SET pending_read = read_through WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn pending_reads(&self) -> Result<Vec<(String, i64)>> {
        self.connection
            .prepare("SELECT id, pending_read FROM chats WHERE pending_read IS NOT NULL")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect()
    }

    pub fn finish_read_sync(&self, id: &str, through: i64) -> Result<()> {
        self.connection.execute(
            "UPDATE chats SET pending_read = NULL WHERE id = ?1 AND pending_read <= ?2",
            params![id, through],
        )?;
        Ok(())
    }

    /// Limit a phone snapshot to messages after any more recent read here.
    pub fn history_unread(&self, id: &str, unread: u32) -> Result<u32> {
        let Some(through) = self.read_through(id)? else {
            return Ok(unread);
        };
        let remaining: u32 = self.connection.query_row(
            "SELECT COUNT(*) FROM messages WHERE chat = ?1 AND from_me = 0 AND timestamp > ?2",
            params![id, through],
            |row| row.get(0),
        )?;
        Ok(unread.min(remaining))
    }

    pub fn set_unread(&self, id: &str, unread: u32) -> Result<()> {
        self.connection.execute(
            "UPDATE chats SET unread = ?2 WHERE id = ?1",
            params![id, unread],
        )?;
        Ok(())
    }

    /// Returns all chats with their latest message, newest first.
    pub fn chats(&self) -> Result<Vec<Chat>> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {CHAT_COLUMNS} {CHAT_JOIN} ORDER BY c.last_activity DESC"
        ))?;
        let rows = statement.query_map([], chat_from_row)?;
        rows.collect()
    }

    pub fn chat(&self, id: &str) -> Result<Option<Chat>> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {CHAT_COLUMNS} {CHAT_JOIN} WHERE c.id = ?1"
        ))?;
        statement.query_row(params![id], chat_from_row).optional()
    }

    pub fn bump_unread(&self, id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE chats SET unread = unread + 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Returns recent incoming message ids and senders for read receipts.
    pub fn unread_incoming(&self, chat: &str, limit: u32) -> Result<Vec<(String, String)>> {
        let mut statement = self.connection.prepare(
            "SELECT id, sender FROM messages WHERE chat = ?1 AND from_me = 0
             AND timestamp >= COALESCE((SELECT read_through FROM chats WHERE id = ?1), -1)
             ORDER BY timestamp DESC, rowid DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![chat, i64::from(limit)], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        rows.collect()
    }

    /// Records an attachment's local path.
    pub fn set_media_path(&self, chat: &str, id: &str, path: &Path) -> Result<Option<Message>> {
        self.put_media_path(chat, id, Some(path))
    }

    /// Clears an attachment path so it can be downloaded again.
    pub fn clear_media_path(&self, chat: &str, id: &str) -> Result<Option<Message>> {
        self.put_media_path(chat, id, None)
    }

    fn put_media_path(&self, chat: &str, id: &str, path: Option<&Path>) -> Result<Option<Message>> {
        let Some(mut message) = self.message(chat, id)? else {
            return Ok(None);
        };
        let Some(media) = message.content.media_mut() else {
            return Ok(None);
        };
        media.path = path.map(Path::to_path_buf);
        self.set_content(chat, id, &message.content, message.edited)?;
        Ok(Some(message))
    }

    /// Returns all recorded attachment paths.
    pub fn media_paths(&self) -> Result<Vec<(String, String, std::path::PathBuf)>> {
        let mut statement = self.connection.prepare(
            "SELECT chat, id, json_extract(content, '$.media.path') AS path
             FROM messages WHERE path IS NOT NULL",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                std::path::PathBuf::from(row.get::<_, String>(2)?),
            ))
        })?;
        rows.collect()
    }

    /// Stores a privacy id mapping and carries early mute/pin sync to the
    /// canonical chat. Returns whether that chat's preferences were touched.
    pub fn put_lid(&self, lid: &str, pn: &str) -> Result<bool> {
        self.connection.execute(
            "INSERT INTO lids (lid, pn) VALUES (?1, ?2) ON CONFLICT(lid) DO UPDATE SET pn = excluded.pn",
            params![lid, pn],
        )?;
        self.merge_group_recipient(&format!("{lid}@lid"), &format!("{pn}@s.whatsapp.net"))?;
        // A removal recorded under the privacy id protects the number too.
        self.connection.execute(
            "INSERT INTO chat_removals (chat, through) SELECT ?2, through FROM chat_removals WHERE chat = ?1
             ON CONFLICT(chat) DO UPDATE SET through = MAX(through, excluded.through)",
            params![format!("{lid}@lid"), format!("{pn}@s.whatsapp.net")],
        )?;
        let changed = self.connection.execute(
            "INSERT INTO chats (id, name, kind, pinned, pinned_at, pin_updated_at,
                muted_until, mute_updated_at)
             SELECT ?2, ?3, 'direct', pinned, pinned_at, pin_updated_at,
                muted_until, mute_updated_at FROM chats WHERE id = ?1
                AND (pin_updated_at IS NOT NULL OR mute_updated_at IS NOT NULL)
             ON CONFLICT(id) DO UPDATE SET
                pinned = CASE WHEN excluded.pin_updated_at >= COALESCE(pin_updated_at, -1)
                    THEN excluded.pinned ELSE pinned END,
                pinned_at = CASE WHEN excluded.pin_updated_at >= COALESCE(pin_updated_at, -1)
                    THEN excluded.pinned_at ELSE pinned_at END,
                pin_updated_at = NULLIF(MAX(COALESCE(pin_updated_at, -1), COALESCE(excluded.pin_updated_at, -1)), -1),
                muted_until = CASE WHEN excluded.mute_updated_at >= COALESCE(mute_updated_at, -1)
                    THEN excluded.muted_until ELSE muted_until END,
                mute_updated_at = NULLIF(MAX(COALESCE(mute_updated_at, -1), COALESCE(excluded.mute_updated_at, -1)), -1)",
            params![format!("{lid}@lid"), format!("{pn}@s.whatsapp.net"), pn],
        )?;
        Ok(changed > 0)
    }

    pub fn lids(&self) -> Result<Vec<(String, String)>> {
        let mut statement = self.connection.prepare("SELECT lid, pn FROM lids")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect()
    }

    /// Drops a table, for tests that need a failing lookup. Test-only:
    /// production never drops schema objects.
    #[cfg(test)]
    pub(crate) fn drop_table_for_test(&self, table: &str) -> Result<()> {
        self.connection
            .execute(&format!("DROP TABLE {table}"), [])?;
        Ok(())
    }

    /// Every file the archive still vouches for: message attachments plus
    /// sticker favorites and cataloged copies, which live outside messages
    /// but may name the same file. None means unprovable (a failed lookup
    /// or a damaged favorites list), never an empty disk: without proof
    /// every candidate stays.
    pub fn protected_files(&self) -> Option<std::collections::HashSet<std::path::PathBuf>> {
        let mut live = std::collections::HashSet::new();
        let paths = self.media_paths().ok()?;
        live.extend(paths.into_iter().map(|(_, _, path)| path));
        live.extend(self.sticker_favorites_strict().ok()??);
        live.extend(self.sticker_file_refs().ok()?);
        Some(live)
    }

    /// Sticker favorites with strict decoding: a missing list is empty, but
    /// a damaged one is unprovable. The picker keeps its lenient fallback;
    /// destructive cleanups must go through protected_files and abort on
    /// None instead.
    pub fn sticker_favorites_strict(&self) -> Result<Option<Vec<std::path::PathBuf>>> {
        let Some(raw) = self.meta("sticker_favorites")? else {
            return Ok(Some(Vec::new()));
        };
        Ok(serde_json::from_str(&raw).ok())
    }

    /// Upserts a message, preserves the furthest delivery state, and updates
    /// chat activity. `raw` contains attachment metadata.
    pub fn insert_message(&self, message: &Message, raw: Option<&[u8]>) -> Result<()> {
        // A delete-for-me tombstone wins over any late replay of the same id.
        if self.is_tombstoned(&message.chat, &message.id)? {
            return Ok(());
        }
        // A clear/delete barrier wins over any late replay below it, no
        // matter which ingestion path filed the row: history sync writes
        // straight through here, bypassing the worker live-message guard.
        if self
            .removal_point(&message.chat)?
            .is_some_and(|through| message.timestamp <= through)
        {
            return Ok(());
        }
        let existing: Option<i64> = self
            .connection
            .query_row(
                "SELECT status FROM messages WHERE chat = ?1 AND id = ?2",
                params![message.chat, message.id],
                |row| row.get(0),
            )
            .optional()?;
        let status = match existing {
            Some(rank)
                if message.status != Delivery::Failed && rank > status_rank(message.status) =>
            {
                rank
            }
            _ => status_rank(message.status),
        };
        self.connection.execute(
            "INSERT INTO messages (chat, id, sender, sender_name, from_me, timestamp, content, status, quoted, reactions, edited, raw, thumbnail, mentions, forwarded, delivered_at, read_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
             ON CONFLICT(chat, id) DO UPDATE SET
                sender_name = COALESCE(excluded.sender_name, sender_name),
                -- A revocation sticks: late replays must not resurrect it.
                content = CASE WHEN json_extract(content, '$.kind') = 'revoked'
                    THEN content ELSE excluded.content END,
                status = excluded.status,
                quoted = COALESCE(excluded.quoted, quoted),
                reactions = excluded.reactions,
                edited = excluded.edited,
                raw = COALESCE(excluded.raw, raw),
                thumbnail = COALESCE(excluded.thumbnail, thumbnail),
                mentions = excluded.mentions,
                forwarded = excluded.forwarded,
                delivered_at = COALESCE(delivered_at, excluded.delivered_at),
                read_at = COALESCE(read_at, excluded.read_at)",
            params![
                message.chat,
                message.id,
                message.sender,
                message.sender_name,
                message.from_me,
                message.timestamp,
                serde_json::to_string(&message.content).unwrap_or_default(),
                status,
                message
                    .quoted
                    .as_ref()
                    .map(|quoted| serde_json::to_string(quoted).unwrap_or_default()),
                serde_json::to_string(&message.reactions).unwrap_or_default(),
                message.edited,
                raw,
                message.thumbnail.as_deref(),
                serde_json::to_string(&message.mentions).unwrap_or_default(),
                message.forwarded,
                message.delivered_at,
                message.read_at,
            ],
        )?;
        self.connection.execute(
            "UPDATE chats SET last_activity = MAX(last_activity, ?2) WHERE id = ?1",
            params![message.chat, message.timestamp],
        )?;
        Ok(())
    }

    /// Resolves the cursor rowid once, up front: a cursor deleted between
    /// the UI read and a paging query has no rowid left, and the paging
    /// queries below handle that absence with the whole-second fallback.
    fn cursor_rowid(&self, chat: &str, id: &str) -> Result<Option<i64>> {
        self.connection
            .query_row(
                "SELECT rowid FROM messages WHERE chat = ?1 AND id = ?2",
                params![chat, id],
                |row| row.get(0),
            )
            .optional()
    }
    /// Returns up to `limit` messages before an optional timestamp/id boundary,
    /// in ascending order.
    pub fn messages(
        &self,
        chat: &str,
        before: Option<(i64, &str)>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let mut statement = self.connection.prepare(
            "SELECT id, sender, sender_name, from_me, timestamp, content, status, quoted, reactions, edited, thumbnail, mentions, forwarded, delivered_at, read_at
             FROM messages
             WHERE chat = ?1 AND (timestamp < ?2 OR (timestamp = ?2 AND rowid < ?3))
             ORDER BY timestamp DESC, rowid DESC
             LIMIT ?4",
        )?;
        let (before_time, before_id) = before.unwrap_or((i64::MAX, ""));
        // A deleted cursor has no rowid left: fall back to the whole
        // second instead of an empty comparison, so same-second siblings
        // still page. Callers dedupe by id; nothing is skipped twice.
        let before_rowid = self.cursor_rowid(chat, before_id)?.unwrap_or(i64::MAX);
        let rows = statement.query_map(
            params![chat, before_time, before_rowid, limit as i64],
            |row| {
                let content: String = row.get(5)?;
                let quoted: Option<String> = row.get(7)?;
                let reactions: String = row.get(8)?;
                let mentions: String = row.get(11)?;
                Ok(Message {
                    id: row.get(0)?,
                    chat: chat.to_owned(),
                    sender: row.get(1)?,
                    sender_name: row.get(2)?,
                    from_me: row.get(3)?,
                    timestamp: row.get(4)?,
                    content: serde_json::from_str(&content).unwrap_or(Content::Unsupported {
                        what: "unreadable".into(),
                    }),
                    status: status_from_rank(row.get(6)?),
                    delivered_at: row.get(13)?,
                    read_at: row.get(14)?,
                    quoted: quoted.and_then(|quoted| serde_json::from_str(&quoted).ok()),
                    reactions: serde_json::from_str(&reactions).unwrap_or_default(),
                    edited: row.get(9)?,
                    mentions: serde_json::from_str(&mentions).unwrap_or_default(),
                    forwarded: row.get(12)?,
                    thumbnail: row.get(10)?,
                })
            },
        )?;
        let mut messages: Vec<Message> = rows.collect::<Result<_>>()?;
        messages.reverse();
        Ok(messages)
    }

    /// Searches visible message text, filenames, polls, contacts, and places.
    /// ASCII matching is case-insensitive; other text follows SQLite behavior.
    pub fn search_messages(&self, needle: &str, limit: usize) -> Result<Vec<Message>> {
        self.search_messages_in(None, needle, limit)
    }

    /// Searches one chat's visible text, or every chat when chat is absent.
    pub fn search_messages_in(
        &self,
        chat: Option<&str>,
        needle: &str,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let pattern = format!(
            "%{}%",
            needle
                .to_lowercase()
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let mut statement = self.connection.prepare(
            "SELECT chat, id, sender, sender_name, from_me, timestamp, content, status, quoted, reactions, edited, thumbnail, mentions, forwarded, delivered_at, read_at
             FROM messages
             WHERE json_valid(content) AND (?3 IS NULL OR chat = ?3) AND lower(
                     coalesce(json_extract(content, '$.text'), '') || char(10) ||
                     coalesce(json_extract(content, '$.caption'), '') || char(10) ||
                     coalesce(json_extract(content, '$.file_name'), '') || char(10) ||
                     coalesce(json_extract(content, '$.question'), '') || char(10) ||
                     coalesce(json_extract(content, '$.display_name'), '') || char(10) ||
                     coalesce(json_extract(content, '$.name'), '')
                 ) LIKE ?1 ESCAPE '\\'
             ORDER BY timestamp DESC, rowid DESC
             LIMIT ?2",
        )?;
        let rows = statement.query_map(params![pattern, limit as i64, chat], |row| {
            let chat: String = row.get(0)?;
            let content: String = row.get(6)?;
            let quoted: Option<String> = row.get(8)?;
            let reactions: String = row.get(9)?;
            let mentions: String = row.get(12)?;
            Ok(Message {
                id: row.get(1)?,
                chat,
                sender: row.get(2)?,
                sender_name: row.get(3)?,
                from_me: row.get(4)?,
                timestamp: row.get(5)?,
                content: serde_json::from_str(&content).unwrap_or(Content::Unsupported {
                    what: "unreadable".into(),
                }),
                status: status_from_rank(row.get(7)?),
                delivered_at: row.get(14)?,
                read_at: row.get(15)?,
                quoted: quoted.and_then(|quoted| serde_json::from_str(&quoted).ok()),
                reactions: serde_json::from_str(&reactions).unwrap_or_default(),
                edited: row.get(10)?,
                mentions: serde_json::from_str(&mentions).unwrap_or_default(),
                forwarded: row.get(13)?,
                thumbnail: row.get(11)?,
            })
        })?;
        let messages: Vec<Message> = rows.collect::<Result<_>>()?;
        Ok(messages)
    }

    /// Returns messages from `from` through `before`, ascending and limited.
    pub fn messages_range(
        &self,
        chat: &str,
        from: i64,
        before: (i64, &str),
        limit: usize,
    ) -> Result<Vec<Message>> {
        let mut statement = self.connection.prepare(
            "SELECT id, sender, sender_name, from_me, timestamp, content, status, quoted, reactions, edited, thumbnail, mentions, forwarded, delivered_at, read_at
             FROM messages
             WHERE chat = ?1 AND timestamp >= ?2 AND (timestamp < ?3 OR (timestamp = ?3 AND rowid < ?4))
             ORDER BY timestamp ASC, rowid ASC
             LIMIT ?5",
        )?;
        // Same deleted-cursor fallback as messages(): the whole second
        // still pages instead of vanishing behind a NULL comparison.
        let before_rowid = self.cursor_rowid(chat, before.1)?.unwrap_or(i64::MAX);
        let rows = statement.query_map(
            params![chat, from, before.0, before_rowid, limit as i64],
            |row| {
                let content: String = row.get(5)?;
                let quoted: Option<String> = row.get(7)?;
                let reactions: String = row.get(8)?;
                let mentions: String = row.get(11)?;
                Ok(Message {
                    id: row.get(0)?,
                    chat: chat.to_owned(),
                    sender: row.get(1)?,
                    sender_name: row.get(2)?,
                    from_me: row.get(3)?,
                    timestamp: row.get(4)?,
                    content: serde_json::from_str(&content).unwrap_or(Content::Unsupported {
                        what: "unreadable".into(),
                    }),
                    status: status_from_rank(row.get(6)?),
                    delivered_at: row.get(13)?,
                    read_at: row.get(14)?,
                    quoted: quoted.and_then(|quoted| serde_json::from_str(&quoted).ok()),
                    reactions: serde_json::from_str(&reactions).unwrap_or_default(),
                    edited: row.get(9)?,
                    mentions: serde_json::from_str(&mentions).unwrap_or_default(),
                    forwarded: row.get(12)?,
                    thumbnail: row.get(10)?,
                })
            },
        )?;
        rows.collect()
    }

    /// Returns downloaded stickers the user sent, newest first.
    ///
    /// The picker offers what the user chose: saved stickers, imported packs
    /// and the phone's recent list. Stickers that merely passed through a
    /// chat are never listed, even after their file is cached.
    pub fn recent_stickers(&self, limit: usize) -> Result<Vec<ArchivedSticker>> {
        let mut statement = self.connection.prepare(
            "SELECT json_extract(content, '$.media.path') AS path, MAX(timestamp), raw
             FROM messages
             WHERE json_extract(content, '$.kind') = 'sticker'
               AND from_me = 1
               AND path IS NOT NULL
             GROUP BY path
             ORDER BY 2 DESC
             LIMIT ?1",
        )?;
        let rows = statement.query_map(params![limit as i64], |row| {
            Ok(ArchivedSticker {
                last_used: row.get(1)?,
                path: std::path::PathBuf::from(row.get::<_, String>(0)?),
                raw: row.get(2)?,
            })
        })?;
        Ok(rows
            .flatten()
            .filter(|sticker| sticker.path.exists())
            .collect())
    }

    /// Returns undownloaded sticker messages the user sent, newest first.
    pub fn stickers_without_file(&self, limit: usize) -> Result<Vec<(String, String)>> {
        let mut statement = self.connection.prepare(
            "SELECT chat, id FROM messages
             WHERE json_extract(content, '$.kind') = 'sticker'
               AND from_me = 1
               AND json_extract(content, '$.media.path') IS NULL
               AND raw IS NOT NULL
             ORDER BY timestamp DESC
             LIMIT ?1",
        )?;
        let rows = statement.query_map(params![limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect()
    }

    /// Upserts a recent phone sticker, preserving the latest use time.
    pub fn upsert_phone_sticker(
        &self,
        hash: &str,
        raw: &[u8],
        last_used: i64,
        weight: f32,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO stickers (hash, raw, last_used, weight) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(hash) DO UPDATE SET
                 raw = excluded.raw,
                 last_used = MAX(stickers.last_used, excluded.last_used),
                 weight = excluded.weight",
            params![hash, raw, last_used, weight as f64],
        )?;
        Ok(())
    }

    /// Returns recent phone stickers by latest use.
    pub fn phone_stickers(&self) -> Result<Vec<PhoneSticker>> {
        let mut statement = self.connection.prepare(
            "SELECT hash, raw, last_used, path FROM stickers
             ORDER BY last_used DESC, weight DESC
             LIMIT 120",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(PhoneSticker {
                hash: row.get(0)?,
                raw: row.get(1)?,
                last_used: row.get(2)?,
                path: row
                    .get::<_, Option<String>>(3)?
                    .map(std::path::PathBuf::from),
            })
        })?;
        rows.collect()
    }

    pub fn set_sticker_path(&self, hash: &str, path: &Path) -> Result<()> {
        self.connection.execute(
            "UPDATE stickers SET path = ?2 WHERE hash = ?1",
            params![hash, path.to_string_lossy()],
        )?;
        Ok(())
    }

    /// Forgets a phone sticker's file so it downloads again.
    pub fn clear_sticker_path(&self, hash: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE stickers SET path = NULL WHERE hash = ?1",
            params![hash],
        )?;
        Ok(())
    }
    /// A favorite sync state, by content hash. Adapted from upstream ZapFast.
    pub fn favorite_sticker(&self, hash: &str) -> Result<Option<FavoriteStickerSync>> {
        use rusqlite::OptionalExtension;
        self.connection
            .query_row(
                "SELECT favorite, updated_at, action, pushed FROM favorite_stickers WHERE hash = ?1",
                params![hash],
                |row| {
                    Ok(FavoriteStickerSync {
                        favorite: row.get(0)?,
                        updated_at: row.get(1)?,
                        action: row.get(2)?,
                        pushed: row.get(3)?,
                    })
                },
            )
            .optional()
    }
    /// Records a favorite change. A missing action keeps the references
    /// already known, since removing a favorite does not carry them.
    pub fn set_favorite_sticker(
        &self,
        hash: &str,
        favorite: bool,
        updated_at: i64,
        action: Option<&[u8]>,
        pushed: bool,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO favorite_stickers (hash, favorite, updated_at, action, pushed) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(hash) DO UPDATE SET favorite = excluded.favorite, updated_at = excluded.updated_at, action = COALESCE(excluded.action, favorite_stickers.action), pushed = excluded.pushed",
            params![hash, favorite, updated_at, action, pushed],
        )?;
        Ok(())
    }
    /// Favorite changes the phone has not been told about yet.
    pub fn unpushed_favorite_stickers(&self) -> Result<Vec<(String, FavoriteStickerSync)>> {
        let mut statement = self.connection.prepare(
            "SELECT hash, favorite, updated_at, action, pushed FROM favorite_stickers WHERE pushed = 0 ORDER BY updated_at",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get(0)?,
                FavoriteStickerSync {
                    favorite: row.get(1)?,
                    updated_at: row.get(2)?,
                    action: row.get(3)?,
                    pushed: row.get(4)?,
                },
            ))
        })?;
        rows.collect()
    }
    /// Favorite hashes, newest first.
    pub fn favorite_hashes(&self) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT hash FROM favorite_stickers WHERE favorite = 1 ORDER BY updated_at DESC",
        )?;
        let rows = statement.query_map([], |row| row.get(0))?;
        rows.collect()
    }
    /// Marks a change as delivered to the phone, unless a newer one replaced
    /// it meanwhile, and keeps the references it was sent with.
    pub fn favorite_sticker_pushed(
        &self,
        hash: &str,
        updated_at: i64,
        action: Option<&[u8]>,
    ) -> Result<()> {
        self.connection.execute(
            "UPDATE favorite_stickers SET pushed = 1, action = COALESCE(?3, action) WHERE hash = ?1 AND updated_at = ?2",
            params![hash, updated_at, action],
        )?;
        Ok(())
    }
    /// Raw sticker messages, newest first, to find a sticker CDN references.
    pub fn sticker_message_raws(&self, limit: usize) -> Result<Vec<Vec<u8>>> {
        let mut statement = self.connection.prepare(
            "SELECT raw FROM messages WHERE json_extract(content, ?1) = ?2 AND raw IS NOT NULL ORDER BY timestamp DESC LIMIT ?3",
        )?;
        let rows =
            statement.query_map(params!["$.kind", "sticker", limit as i64], |row| row.get(0))?;
        rows.collect()
    }
    /// Moves every row filed under one chat id to another id.
    ///
    /// A chat behind a privacy id is filed under its phone number once the
    /// mapping is known, but rows written before that moment keep the old
    /// id. Without this move the sidebar shows the chat while opening it
    /// reads an empty id, so saved messages never appear. Returns whether
    /// anything moved.
    pub fn rekey_chat(&self, from: &str, to: &str) -> Result<bool> {
        if from == to {
            return Ok(false);
        }
        // One transaction: a failure anywhere rolls every associated row back.
        let transaction = self.connection.unchecked_transaction()?;
        // A message id already living under both ids keeps the target copy.
        let dupes = transaction.execute(
            "DELETE FROM messages WHERE chat = ?1 AND id IN (SELECT id FROM messages WHERE chat = ?2)",
            params![from, to],
        )?;
        let moved = transaction.execute(
            "UPDATE messages SET chat = ?2 WHERE chat = ?1",
            params![from, to],
        )?;
        transaction.execute(
            "DELETE FROM group_receipts WHERE chat = ?1 AND (id, recipient) IN (SELECT id, recipient FROM group_receipts WHERE chat = ?2)",
            params![from, to],
        )?;
        transaction.execute(
            "UPDATE group_receipts SET chat = ?2 WHERE chat = ?1",
            params![from, to],
        )?;
        transaction.execute(
            "INSERT INTO chat_removals (chat, through) SELECT ?2, through FROM chat_removals WHERE chat = ?1
             ON CONFLICT(chat) DO UPDATE SET through = MAX(through, excluded.through)",
            params![from, to],
        )?;
        transaction.execute("DELETE FROM chat_removals WHERE chat = ?1", params![from])?;
        // Delete-for-me tombstones follow the chat: both copies mean the same
        // deletion, so the union is kept.
        transaction.execute(
            "INSERT OR IGNORE INTO message_tombstones (chat, id, deleted_ms) SELECT ?2, id, deleted_ms FROM message_tombstones WHERE chat = ?1",
            params![from, to],
        )?;
        transaction.execute(
            "DELETE FROM message_tombstones WHERE chat = ?1",
            params![from],
        )?;
        // Accepted-state order follows with newest-wins, like the intents.
        transaction.execute(
            "INSERT INTO chat_sync_order (chat, order_ms, archived) SELECT ?2, order_ms, archived FROM chat_sync_order WHERE chat = ?1
             ON CONFLICT(chat) DO UPDATE SET order_ms = excluded.order_ms, archived = excluded.archived
             WHERE excluded.order_ms > chat_sync_order.order_ms",
            params![from, to],
        )?;
        let states = self.rekey_chat_syncs(from, to)?;
        let chats = self.merge_chat_rows(from, to)?;
        // A row the tombstone condemns must not survive under either id.
        let condemned = transaction.execute(
            "DELETE FROM messages WHERE chat = ?1 AND id IN (SELECT id FROM message_tombstones WHERE chat = ?1)",
            params![to],
        )?;
        if condemned > 0 {
            transaction.execute(
                "UPDATE chats SET unread = MIN(unread, (SELECT COUNT(*) FROM messages WHERE chat = ?1 AND from_me = 0)), pending_read = NULL, last_activity = MIN(last_activity, COALESCE((SELECT MAX(timestamp) FROM messages WHERE chat = ?1), last_activity)) WHERE id = ?1",
                params![to],
            )?;
        }
        // The surviving intent outranks the OR-merged flag; a lookup failure
        // fails the migration instead of silently keeping a merged flag.
        // The flag follows the newest of the surviving intent and the
        // accepted order, never the OR merge above nor any queue alone:
        // with no intent the accepted state wins, and a stale intent
        // cannot flip a newer accepted state. Ties keep the intent, the
        // only unconfirmed voice. A lookup failure fails the migration
        // instead of silently keeping a merged flag.
        let queued: Option<(bool, i64)> = transaction
            .query_row(
                "SELECT value, updated_ms FROM chat_sync_queue WHERE chat = ?1 AND setting = ?2",
                params![to, "archived"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let accepted: Option<(i64, bool)> = transaction
            .query_row(
                "SELECT order_ms, archived FROM chat_sync_order WHERE chat = ?1",
                params![to],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let winner: Option<bool> = match (queued, accepted) {
            (Some((value, updated)), Some((order_ms, archived))) => {
                if order_ms > updated {
                    Some(archived)
                } else {
                    Some(value)
                }
            }
            (Some((value, _)), None) => Some(value),
            (None, Some((_, archived))) => Some(archived),
            (None, None) => None,
        };
        if let Some(archived) = winner {
            transaction.execute(
                "UPDATE chats SET archived = ?2 WHERE id = ?1",
                params![to, archived],
            )?;
        }
        transaction.commit()?;
        Ok(dupes > 0 || moved > 0 || chats || states || condemned > 0)
    }

    /// Moves sync intents across a privacy-id migration. When both ids hold
    /// an intent for the same setting, the newer one wins and takes a fresh
    /// revision so in-flight completions from before the move stay stale.
    fn rekey_chat_syncs(&self, from: &str, to: &str) -> Result<bool> {
        let intents: Vec<(String, bool, i64, i64)> = self
            .connection
            .prepare("SELECT setting, value, updated_ms, rev FROM chat_sync_queue WHERE chat = ?1")?
            .query_map(params![from], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<Result<Vec<_>>>()?;
        if intents.is_empty() {
            return Ok(false);
        }
        for (setting, value, updated, rev) in intents {
            let current: Option<(bool, i64, i64)> = self
                .connection
                .query_row(
                    "SELECT value, updated_ms, rev FROM chat_sync_queue WHERE chat = ?1 AND setting = ?2",
                    params![to, setting],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            // Newest (updated_ms, rev) wins; legacy rows share rev 0 and keep
            // the canonical side by rule. The winner takes a fresh revision
            // so older in-flight completions stay stale, never as its order.
            let newer = current.is_none_or(|(_, at, r)| (updated, rev) > (at, r));
            if newer {
                let fresh = self.alloc_sync_rev()?;
                // One UPSERT on the primary key: any other failure rolls the
                // whole migration back instead of masking as a no-op update.
                self.connection.execute(
                    "INSERT INTO chat_sync_queue (chat, setting, value, updated_ms, rev) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(chat, setting) DO UPDATE SET value = excluded.value, updated_ms = excluded.updated_ms, rev = excluded.rev",
                    params![to, setting, value, updated, fresh],
                )?;
            }
        }
        self.connection
            .execute("DELETE FROM chat_sync_queue WHERE chat = ?1", params![from])?;
        Ok(true)
    }
    /// Folds the chat row `from` into `to`, keeping the liveliest values.
    fn merge_chat_rows(&self, from: &str, to: &str) -> Result<bool> {
        let have_from: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM chats WHERE id = ?1)",
            params![from],
            |row| row.get(0),
        )?;
        if !have_from {
            return Ok(false);
        }
        let have_to: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM chats WHERE id = ?1)",
            params![to],
            |row| row.get(0),
        )?;
        if !have_to {
            self.connection
                .execute("UPDATE chats SET id = ?2 WHERE id = ?1", params![from, to])?;
            return Ok(true);
        }
        self.connection.execute(
            "UPDATE chats SET
                last_activity = MAX(last_activity, (SELECT last_activity FROM chats WHERE id = ?1)),
                unread = unread + (SELECT unread FROM chats WHERE id = ?1),
                archived = archived | (SELECT archived FROM chats WHERE id = ?1),
                pinned = pinned | (SELECT pinned FROM chats WHERE id = ?1),
                pinned_at = MAX(COALESCE(pinned_at, 0), COALESCE((SELECT pinned_at FROM chats WHERE id = ?1), 0)),
                muted_until = MAX(COALESCE(muted_until, 0), COALESCE((SELECT muted_until FROM chats WHERE id = ?1), 0)),
                name = CASE WHEN name IS NULL OR name = '' THEN (SELECT name FROM chats WHERE id = ?1) ELSE name END
             WHERE id = ?2",
            params![from, to],
        )?;
        self.connection
            .execute("DELETE FROM chats WHERE id = ?1", params![from])?;
        Ok(true)
    }
    /// Stores a generated video poster, keeping the phone's own thumbnail
    /// when one arrived with the message.
    pub fn set_thumbnail(&self, chat: &str, id: &str, thumbnail: &[u8]) -> Result<()> {
        self.connection.execute(
            "UPDATE messages SET thumbnail = COALESCE(thumbnail, ?3) WHERE chat = ?1 AND id = ?2",
            params![chat, id, thumbnail],
        )?;
        Ok(())
    }

    /// Stores what analyzing a downloaded video learned: its real length
    /// and a poster built from its own frames.
    ///
    /// The length fills in only while the message still carries none (or
    /// a zero the phone sent); the poster always upgrades, because a
    /// generated frame carries a bubble a 96 px phone thumbnail cannot.
    pub fn set_video_meta(
        &self,
        chat: &str,
        id: &str,
        seconds: Option<u32>,
        thumbnail: Option<&[u8]>,
    ) -> Result<Option<Message>> {
        let Some(mut message) = self.message(chat, id)? else {
            return Ok(None);
        };
        let Content::Video {
            seconds: stored, ..
        } = &mut message.content
        else {
            return Ok(None);
        };
        if let Some(seconds) = seconds
            && stored.is_none_or(|known| known == 0)
        {
            *stored = Some(seconds);
        }
        self.set_content(chat, id, &message.content, message.edited)?;
        if let Some(thumbnail) = thumbnail {
            self.connection.execute(
                "UPDATE messages SET thumbnail = ?3 WHERE chat = ?1 AND id = ?2",
                params![chat, id, thumbnail],
            )?;
            message.thumbnail = Some(thumbnail.to_vec());
        }
        Ok(Some(message))
    }

    /// Videos already on disk whose length is still unknown (or zero) or
    /// whose poster never arrived: the backfill analyzes them in order.
    pub fn videos_needing_meta(&self) -> Result<Vec<(String, String, std::path::PathBuf)>> {
        let mut statement = self.connection.prepare(
            "SELECT chat, id, json_extract(content, '$.media.path') AS path
             FROM messages
             WHERE json_extract(content, '$.media.path') IS NOT NULL
             AND (
                 json_extract(content, '$.seconds') IS NULL
                 OR json_extract(content, '$.seconds') = 0
                 OR thumbnail IS NULL
             )",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                std::path::PathBuf::from(row.get::<_, String>(2)?),
            ))
        })?;
        // Only videos: other attachments share the media path column.
        let mut videos = Vec::new();
        for (chat, id, path) in rows.flatten() {
            let is_video = self
                .message(&chat, &id)?
                .is_some_and(|message| matches!(message.content, Content::Video { .. }));
            if is_video {
                videos.push((chat, id, path));
            }
        }
        Ok(videos)
    }

    /// Every phone-sticker file the picker may still list.
    ///
    /// The cache sweep keeps these and may reclaim anything else in the
    /// sticker folders.
    pub fn sticker_file_refs(&self) -> Result<Vec<std::path::PathBuf>> {
        let mut statement = self
            .connection
            .prepare("SELECT path FROM stickers WHERE path IS NOT NULL")?;
        let rows = statement.query_map([], |row| {
            Ok(row
                .get::<_, Option<String>>(0)?
                .map(std::path::PathBuf::from))
        })?;
        Ok(rows.flatten().flatten().collect())
    }

    /// Returns raw messages for re-deriving fields in newer versions.
    pub fn rows_with_raw(&self) -> Result<Vec<(String, String, Vec<u8>)>> {
        let mut statement = self
            .connection
            .prepare("SELECT chat, id, raw FROM messages WHERE raw IS NOT NULL")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect()
    }

    /// Replaces protobuf-derived fields while preserving local file state.
    pub fn set_derived(
        &self,
        chat: &str,
        id: &str,
        content: &Content,
        mentions: &[crate::model::MentionRef],
        thumbnail: Option<&[u8]>,
        forwarded: bool,
    ) -> Result<()> {
        self.connection.execute(
            "UPDATE messages SET content = ?3, mentions = ?4, thumbnail = COALESCE(?5, thumbnail), forwarded = ?6
             WHERE chat = ?1 AND id = ?2",
            params![
                chat,
                id,
                serde_json::to_string(content).unwrap_or_default(),
                serde_json::to_string(mentions).unwrap_or_default(),
                thumbnail,
                forwarded
            ],
        )?;
        Ok(())
    }

    pub fn delete_message(&self, chat: &str, id: &str) -> Result<bool> {
        let deleted = self.connection.execute(
            "DELETE FROM messages WHERE chat = ?1 AND id = ?2",
            params![chat, id],
        )?;
        Ok(deleted > 0)
    }

    /// Deletes one message for this device only: tombstones its id against
    /// replay, removes the row, and recomputes the chat counters from what
    /// survived. Returns whether a row existed plus its attachment path.
    pub fn delete_message_for_me(
        &self,
        chat: &str,
        id: &str,
        now_ms: i64,
    ) -> Result<(bool, Vec<std::path::PathBuf>)> {
        // One transaction: a crash between tombstone and row removal must not
        // leave a message deleted in one place and alive in the other.
        let transaction = self.connection.unchecked_transaction()?;
        self.tombstone_message(chat, id, now_ms)?;
        let row: Option<(i64, Option<String>)> = self
            .connection
            .query_row(
                "SELECT timestamp, json_extract(content, '$.media.path') FROM messages WHERE chat = ?1 AND id = ?2",
                params![chat, id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((timestamp, path)) = row else {
            transaction.commit()?;
            return Ok((false, Vec::new()));
        };
        let media = path.map(std::path::PathBuf::from).into_iter().collect();
        self.connection.execute(
            "DELETE FROM messages WHERE chat = ?1 AND id = ?2",
            params![chat, id],
        )?;
        self.connection.execute(
            "UPDATE chats SET unread = MIN(unread, (SELECT COUNT(*) FROM messages
                WHERE chat = ?1 AND from_me = 0 AND timestamp > COALESCE(read_through, -1))),
                pending_read = CASE WHEN pending_read <= ?2 THEN NULL ELSE pending_read END,
                last_activity = MIN(last_activity, COALESCE((SELECT MAX(timestamp) FROM messages
                WHERE chat = ?1), last_activity))
             WHERE id = ?1",
            params![chat, timestamp],
        )?;
        transaction.commit()?;
        Ok((true, media))
    }

    /// Where a deleted or cleared chat ends: messages at or below this
    /// timestamp belong to the removed range and must never come back via
    /// late history. Survives restarts with the archive itself.
    pub fn removal_point(&self, chat: &str) -> Result<Option<i64>> {
        self.connection
            .query_row(
                "SELECT through FROM chat_removals WHERE chat = ?1",
                params![chat],
                |row| row.get(0),
            )
            .optional()
    }

    /// Atomically removes only the range the linked device knew about,
    /// keeping newer messages and a durable barrier against replay. With
    /// delete, a chat left without newer messages is removed entirely;
    /// otherwise its messages are cleared but the chat stays listed.
    /// Monotonic: a later call with an older boundary changes nothing.
    pub fn remove_chat_through(&self, chat: &str, through: i64, delete: bool) -> Result<Removed> {
        let transaction = self.connection.unchecked_transaction()?;
        let through = self
            .removal_point(chat)?
            .map_or(through, |old| old.max(through));
        self.connection.execute(
            "INSERT INTO chat_removals (chat, through) VALUES (?1, ?2)
             ON CONFLICT(chat) DO UPDATE SET through = MAX(through, excluded.through)",
            params![chat, through],
        )?;
        let newer: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE chat = ?1 AND timestamp > ?2)",
            params![chat, through],
            |row| row.get(0),
        )?;
        let removed = if newer {
            let media = {
                let mut statement = self.connection.prepare(
                    "SELECT json_extract(content, '$.media.path') AS path FROM messages
                     WHERE chat = ?1 AND timestamp <= ?2 AND path IS NOT NULL",
                )?;
                statement
                    .query_map(params![chat, through], |row| {
                        row.get::<_, String>(0).map(std::path::PathBuf::from)
                    })?
                    .collect::<Result<Vec<_>>>()?
            };
            let deleted = self.connection.execute(
                "DELETE FROM messages WHERE chat = ?1 AND timestamp <= ?2",
                params![chat, through],
            )?;
            self.connection.execute(
                "UPDATE chats SET unread = MIN(unread, (SELECT COUNT(*) FROM messages
                    WHERE chat = ?1 AND from_me = 0 AND timestamp > COALESCE(read_through, -1))),
                    pending_read = CASE WHEN pending_read <= ?2 THEN NULL ELSE pending_read END,
                    last_activity = MIN(last_activity, COALESCE((SELECT MAX(timestamp) FROM messages
                    WHERE chat = ?1), last_activity))
                 WHERE id = ?1",
                params![chat, through],
            )?;
            Removed {
                existed: true,
                changed: deleted > 0,
                media,
            }
        } else if delete {
            self.delete_chat(chat)?
        } else {
            self.clear_chat(chat)?
        };
        transaction.commit()?;
        Ok(removed)
    }

    /// Removes a chat with everything stored for it. existed reports whether
    /// a chat row was actually there, so a replayed sync action does not
    /// announce a removal twice.
    pub fn delete_chat(&self, chat: &str) -> Result<Removed> {
        let media = self.chat_media(chat)?;
        // A removed chat has no setting left to sync.
        self.clear_chat_syncs_for(chat)?;
        let existed = self
            .connection
            .execute("DELETE FROM chats WHERE id = ?1", params![chat])?
            > 0;
        let purged = self.purge_chat_rows(chat)?;
        Ok(Removed {
            existed,
            changed: existed || purged > 0,
            media,
        })
    }

    /// Removes a chat's messages while keeping the chat itself listed, with
    /// its counters clamped to what survived.
    pub fn clear_chat(&self, chat: &str) -> Result<Removed> {
        let media = self.chat_media(chat)?;
        let purged = self.purge_chat_rows(chat)?;
        self.connection.execute(
            "UPDATE chats SET unread = 0, pending_read = NULL,
                last_activity = COALESCE((SELECT MAX(timestamp) FROM messages WHERE chat = ?1), 0)
             WHERE id = ?1",
            params![chat],
        )?;
        let existed: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM chats WHERE id = ?1)",
            params![chat],
            |row| row.get(0),
        )?;
        Ok(Removed {
            existed,
            changed: purged > 0,
            media,
        })
    }

    /// Attachment paths filed under one chat, for safe cleanup after the
    /// rows are gone. Callers delete a file only when no surviving message
    /// still references it.
    fn chat_media(&self, chat: &str) -> Result<Vec<std::path::PathBuf>> {
        let mut statement = self.connection.prepare(
            "SELECT json_extract(content, '$.media.path') AS path FROM messages
             WHERE chat = ?1 AND path IS NOT NULL",
        )?;
        statement
            .query_map(params![chat], |row| {
                row.get::<_, String>(0).map(std::path::PathBuf::from)
            })?
            .collect()
    }

    /// Deletes every stored row of a chat, returning how many messages went.
    fn purge_chat_rows(&self, chat: &str) -> Result<usize> {
        let purged = self
            .connection
            .execute("DELETE FROM messages WHERE chat = ?1", params![chat])?;
        self.connection
            .execute("DELETE FROM polls WHERE chat = ?1", params![chat])?;
        self.connection
            .execute("DELETE FROM poll_history WHERE chat = ?1", params![chat])?;
        self.connection
            .execute("DELETE FROM poll_votes WHERE chat = ?1", params![chat])?;
        Ok(purged)
    }

    pub fn message(&self, chat: &str, id: &str) -> Result<Option<Message>> {
        let mut statement = self.connection.prepare(
            "SELECT sender, sender_name, from_me, timestamp, content, status, quoted, reactions, edited, thumbnail, mentions, forwarded, delivered_at, read_at
             FROM messages WHERE chat = ?1 AND id = ?2",
        )?;
        statement
            .query_row(params![chat, id], |row| {
                let content: String = row.get(4)?;
                let quoted: Option<String> = row.get(6)?;
                let reactions: String = row.get(7)?;
                let mentions: String = row.get(10)?;
                Ok(Message {
                    id: id.to_owned(),
                    chat: chat.to_owned(),
                    sender: row.get(0)?,
                    sender_name: row.get(1)?,
                    from_me: row.get(2)?,
                    timestamp: row.get(3)?,
                    content: serde_json::from_str(&content).unwrap_or(Content::Unsupported {
                        what: "unreadable".into(),
                    }),
                    status: status_from_rank(row.get(5)?),
                    delivered_at: row.get(12)?,
                    read_at: row.get(13)?,
                    quoted: quoted.and_then(|quoted| serde_json::from_str(&quoted).ok()),
                    reactions: serde_json::from_str(&reactions).unwrap_or_default(),
                    edited: row.get(8)?,
                    mentions: serde_json::from_str(&mentions).unwrap_or_default(),
                    forwarded: row.get(11)?,
                    thumbnail: row.get(9)?,
                })
            })
            .optional()
    }

    /// Returns the earliest message for phone-history requests.
    pub fn oldest(&self, chat: &str) -> Result<Option<Message>> {
        let id: Option<String> = self
            .connection
            .query_row(
                "SELECT id FROM messages WHERE chat = ?1 ORDER BY timestamp ASC, rowid ASC LIMIT 1",
                params![chat],
                |row| row.get(0),
            )
            .optional()?;
        match id {
            Some(id) => self.message(chat, &id),
            None => Ok(None),
        }
    }

    /// Returns a message's raw protobuf for attachment downloads.
    pub fn raw(&self, chat: &str, id: &str) -> Result<Option<Vec<u8>>> {
        self.connection
            .query_row(
                "SELECT raw FROM messages WHERE chat = ?1 AND id = ?2",
                params![chat, id],
                |row| row.get(0),
            )
            .optional()
            .map(Option::flatten)
    }

    /// Advances delivery state, except that `Failed` may replace it. Stores the
    /// first timestamp for each delivery stage.
    pub fn set_status(&self, chat: &str, id: &str, status: Delivery, at: i64) -> Result<bool> {
        let rank = status_rank(status);
        let changed = if status == Delivery::Failed {
            self.connection.execute(
                "UPDATE messages SET status = ?3 WHERE chat = ?1 AND id = ?2",
                params![chat, id, rank],
            )?
        } else if let Some(column) = stamp_column(status) {
            self.connection.execute(
                &format!(
                    "UPDATE messages SET status = ?3, {column} = COALESCE({column}, ?4)
                     WHERE chat = ?1 AND id = ?2 AND status < ?3"
                ),
                params![chat, id, rank, at],
            )?
        } else {
            self.connection.execute(
                "UPDATE messages SET status = ?3 WHERE chat = ?1 AND id = ?2 AND status < ?3",
                params![chat, id, rank],
            )?
        };
        Ok(changed > 0)
    }

    /// Advances outgoing messages through `timestamp` to `status` and returns changed ids.
    pub fn advance_statuses(
        &self,
        chat: &str,
        up_to: i64,
        status: Delivery,
        at: i64,
    ) -> Result<Vec<String>> {
        let rank = status_rank(status);
        let mut statement = self.connection.prepare(
            "SELECT id FROM messages WHERE chat = ?1 AND from_me = 1 AND timestamp <= ?2 AND status > 0 AND status < ?3",
        )?;
        let ids: Vec<String> = statement
            .query_map(params![chat, up_to, rank], |row| row.get(0))?
            .collect::<Result<_>>()?;
        if let Some(column) = stamp_column(status) {
            self.connection.execute(
                &format!(
                    "UPDATE messages SET status = ?3, {column} = COALESCE({column}, ?4)
                     WHERE chat = ?1 AND from_me = 1 AND timestamp <= ?2 AND status > 0 AND status < ?3"
                ),
                params![chat, up_to, rank, at],
            )?;
        } else {
            self.connection.execute(
                "UPDATE messages SET status = ?3 WHERE chat = ?1 AND from_me = 1 AND timestamp <= ?2 AND status > 0 AND status < ?3",
                params![chat, up_to, rank],
            )?;
        }
        Ok(ids)
    }

    pub fn set_content(
        &self,
        chat: &str,
        id: &str,
        content: &Content,
        edited: bool,
    ) -> Result<bool> {
        let changed = self.connection.execute(
            "UPDATE messages SET content = ?3, edited = ?4 WHERE chat = ?1 AND id = ?2",
            params![
                chat,
                id,
                serde_json::to_string(content).unwrap_or_default(),
                edited
            ],
        )?;
        Ok(changed > 0)
    }

    /// Replaces an edited text body and its mention metadata.
    pub fn set_edited_text(
        &self,
        chat: &str,
        id: &str,
        content: &Content,
        mentions: &[crate::model::MentionRef],
    ) -> Result<bool> {
        let changed = self.connection.execute(
            "UPDATE messages SET content = ?3, mentions = ?4, edited = 1 WHERE chat = ?1 AND id = ?2",
            params![
                chat,
                id,
                serde_json::to_string(content).unwrap_or_default(),
                serde_json::to_string(mentions).unwrap_or_default(),
            ],
        )?;
        Ok(changed > 0)
    }

    /// Upserts a reaction, or removes it when the emoji is empty.
    pub fn set_reaction(
        &self,
        chat: &str,
        id: &str,
        sender: &str,
        from_me: bool,
        emoji: &str,
    ) -> Result<Option<Message>> {
        let Some(mut message) = self.message(chat, id)? else {
            return Ok(None);
        };
        message
            .reactions
            .retain(|reaction| reaction.sender != sender);
        if !emoji.is_empty() {
            message.reactions.push(crate::model::Reaction {
                sender: sender.to_owned(),
                from_me,
                emoji: emoji.to_owned(),
            });
        }
        self.connection.execute(
            "UPDATE messages SET reactions = ?3 WHERE chat = ?1 AND id = ?2",
            params![
                chat,
                id,
                serde_json::to_string(&message.reactions).unwrap_or_default()
            ],
        )?;
        Ok(Some(message))
    }

    pub fn upsert_contact(&self, contact: &Contact) -> Result<()> {
        self.connection.execute(
            "INSERT INTO contacts (id, full_name, push_name) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                full_name = COALESCE(excluded.full_name, full_name),
                push_name = COALESCE(excluded.push_name, push_name)",
            params![contact.id, contact.full_name, contact.push_name],
        )?;
        Ok(())
    }

    /// Returns a contact by id.
    pub fn contact(&self, id: &str) -> Result<Option<Contact>> {
        self.connection
            .query_row(
                "SELECT id, full_name, push_name FROM contacts WHERE id = ?1",
                params![id],
                |row| {
                    Ok(Contact {
                        id: row.get(0)?,
                        full_name: row.get(1)?,
                        push_name: row.get(2)?,
                    })
                },
            )
            .optional()
    }

    pub fn contacts(&self) -> Result<Vec<Contact>> {
        let mut statement = self
            .connection
            .prepare("SELECT id, full_name, push_name FROM contacts")?;
        let rows = statement.query_map([], |row| {
            Ok(Contact {
                id: row.get(0)?,
                full_name: row.get(1)?,
                push_name: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    /// Sticker files the reader marked as favourites, newest first.
    pub fn sticker_favorites(&self) -> Result<Vec<std::path::PathBuf>> {
        let Some(raw) = self.meta("sticker_favorites")? else {
            return Ok(Vec::new());
        };
        // A damaged list counts as empty instead of failing the picker.
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }

    /// Adds or removes a favourite and reports its new state.
    pub fn toggle_sticker_favorite(&self, path: &Path) -> Result<bool> {
        let mut favorites = self.sticker_favorites()?;
        let added = match favorites.iter().position(|known| known == path) {
            Some(index) => {
                favorites.remove(index);
                false
            }
            None => {
                favorites.insert(0, path.to_path_buf());
                true
            }
        };
        if let Ok(raw) = serde_json::to_string(&favorites) {
            self.set_meta("sticker_favorites", &raw)?;
        }
        Ok(added)
    }

    /// Points a favourite at its file's new name.
    ///
    /// A sticker copy in the app's cache is filed under the hash of its bytes,
    /// which renames it once. A favourite that named the old file follows it
    /// instead of quietly disappearing from the picker.
    pub fn rename_sticker_favorite(&self, from: &Path, to: &Path) -> Result<()> {
        let mut favorites = self.sticker_favorites()?;
        let mut changed = false;
        for favorite in &mut favorites {
            if favorite == from {
                *favorite = to.to_path_buf();
                changed = true;
            }
        }
        if changed && let Ok(raw) = serde_json::to_string(&favorites) {
            self.set_meta("sticker_favorites", &raw)?;
        }
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.connection
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
    }

    /// Older archives discarded pin times and could lose mute sync. Request
    /// one library-managed snapshot for an existing archive. Fresh links
    /// already receive snapshots; reconnecting must not add another request.
    pub fn take_preferences_refresh(&self) -> Result<bool> {
        const KEY: &str = "chat_preferences_refresh_v1";
        if self.meta(KEY)?.is_some() {
            return Ok(false);
        }
        let existing: bool =
            self.connection
                .query_row("SELECT EXISTS(SELECT 1 FROM chats)", [], |row| row.get(0))?;
        self.set_meta(KEY, "requested")?;
        Ok(existing)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Clears all archived data during unlinking.
    pub fn clear(&self) -> Result<()> {
        self.connection.execute_batch(
            "DELETE FROM poll_history; DELETE FROM poll_votes; DELETE FROM polls; DELETE FROM group_receipts; DELETE FROM messages; DELETE FROM chats; DELETE FROM contacts; DELETE FROM meta; DELETE FROM lids;",
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::Content;
    use crate::model::Media;

    /// A downloaded video message: real file on disk, phone metadata.
    fn video_message(
        chat: &str,
        id: &str,
        seconds: Option<u32>,
        path: &std::path::Path,
    ) -> Message {
        let mut message = message(chat, id, 10, false);
        message.content = Content::Video {
            caption: None,
            media: Media {
                mime: "video/mp4".into(),
                size: 100,
                width: Some(64),
                height: Some(64),
                path: Some(path.to_path_buf()),
                state: Default::default(),
            },
            seconds,
            gif: false,
        };
        message
    }

    #[test]
    fn analyzed_video_meta_fills_length_and_upgrades_the_poster() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("clip.mp4");
        std::fs::write(&file, b"bytes").unwrap();
        let archive = Archive::in_memory().expect("opens");
        archive.ensure_chat("chat", "Chat").expect("chat");
        archive
            .insert_message(&video_message("chat", "m1", None, &file), None)
            .expect("insert");
        let stored = archive
            .set_video_meta("chat", "m1", Some(42), Some(b"poster"))
            .expect("stores")
            .expect("the message");
        assert!(
            matches!(
                &stored.content,
                Content::Video {
                    seconds: Some(42),
                    ..
                }
            ),
            "an unknown length fills in"
        );
        assert_eq!(stored.thumbnail.as_deref(), Some(b"poster".as_slice()));
        // A known length is never overwritten by a later analysis.
        let stored = archive
            .set_video_meta("chat", "m1", Some(7), Some(b"new"))
            .expect("stores")
            .expect("the message");
        assert!(
            matches!(
                &stored.content,
                Content::Video {
                    seconds: Some(42),
                    ..
                }
            ),
            "a real length stays"
        );
        assert_eq!(stored.thumbnail.as_deref(), Some(b"new".as_slice()));
        // Zero counts as unknown and fills in too.
        archive
            .insert_message(&video_message("chat", "m2", Some(0), &file), None)
            .expect("insert");
        let stored = archive
            .set_video_meta("chat", "m2", Some(9), None)
            .expect("stores")
            .expect("the message");
        assert!(
            matches!(
                &stored.content,
                Content::Video {
                    seconds: Some(9),
                    ..
                }
            ),
            "a zero length fills in"
        );
        assert!(stored.thumbnail.is_none(), "no poster means no poster");
    }

    #[test]
    fn videos_needing_meta_lists_only_the_unknown_or_posterless() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("clip.mp4");
        std::fs::write(&file, b"bytes").unwrap();
        let archive = Archive::in_memory().expect("opens");
        archive.ensure_chat("chat", "Chat").expect("chat");
        // Unknown length, posterless, zero length: all need analysis.
        for (id, seconds) in [("m1", None), ("m2", Some(0)), ("m3", Some(42))] {
            archive
                .insert_message(&video_message("chat", id, seconds, &file), None)
                .expect("insert");
        }
        let mut needed: Vec<String> = archive
            .videos_needing_meta()
            .expect("lists")
            .into_iter()
            .map(|(_, id, _)| id)
            .collect();
        needed.sort();
        assert_eq!(
            needed,
            vec!["m1".to_owned(), "m2".to_owned(), "m3".to_owned()]
        );
        // m1 is complete now: length known and poster stored.
        archive
            .set_video_meta("chat", "m1", Some(42), Some(b"poster"))
            .expect("stores");
        let needed: Vec<String> = archive
            .videos_needing_meta()
            .expect("lists")
            .into_iter()
            .map(|(_, id, _)| id)
            .collect();
        assert!(
            !needed.contains(&"m1".to_owned()),
            "a complete video drops out"
        );
    }
    #[test]
    fn rekeying_moves_a_chat_from_its_privacy_id_to_its_number() {
        let archive = Archive::in_memory().expect("opens");
        let lid = "123@lid";
        let pn = "15550001111@s.whatsapp.net";
        archive.ensure_chat(lid, "").expect("old row");
        archive.ensure_chat(pn, "Mom").expect("new row");
        // The same message id under both ids keeps the canonical copy.
        for (chat, id, timestamp) in [
            (lid, "m1", 10),
            (lid, "m2", 20),
            (pn, "m2", 20),
            (pn, "m3", 30),
        ] {
            archive
                .insert_message(&message(chat, id, timestamp, false), None)
                .expect("insert");
        }
        assert!(archive.rekey_chat(lid, pn).expect("moves"));
        assert!(
            !archive.rekey_chat(lid, pn).expect("settled"),
            "a second pass moves nothing"
        );
        let rows = archive.messages(pn, None, 10).expect("lists");
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["m1", "m2", "m3"],
            "every message lives under one id"
        );
        assert!(archive.messages(lid, None, 10).expect("old").is_empty());
        let chats = archive.chats().expect("lists");
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].id, pn);
        assert_eq!(chats[0].name, "Mom", "the kept row keeps its name");
        assert!(chats[0].last.is_some(), "the preview follows the messages");
        assert!(
            !archive.rekey_chat(pn, pn).expect("same"),
            "an id maps to itself"
        );
    }

    fn image_message(chat: &str, id: &str, timestamp: i64, path: &str) -> Message {
        let mut message = message(chat, id, timestamp, false);
        message.content = Content::Image {
            caption: None,
            media: crate::model::Media {
                mime: "image/jpeg".into(),
                size: 3,
                width: Some(2),
                height: Some(2),
                path: Some(path.into()),
                state: crate::model::MediaState::Idle,
            },
        };
        message
    }

    #[test]
    fn removal_keeps_newer_messages_and_is_monotonic() {
        let archive = Archive::in_memory().expect("opens");
        let chat = "1@s.whatsapp.net";
        archive.ensure_chat(chat, "Ada").expect("chat");
        for (id, timestamp) in [("m1", 100), ("m2", 200), ("m3", 300)] {
            archive
                .insert_message(&message(chat, id, timestamp, false), None)
                .expect("insert");
        }
        archive.set_unread(chat, 2).expect("unread");
        let removed = archive
            .remove_chat_through(chat, 200, false)
            .expect("removes");
        assert!(removed.existed);
        assert_eq!(archive.removal_point(chat).expect("point"), Some(200));
        let ids: Vec<String> = archive
            .messages(chat, None, 10)
            .expect("lists")
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(ids, vec!["m3".to_owned()], "only newer survives");
        // An older boundary changes nothing; the barrier only moves forward.
        let replayed = archive
            .remove_chat_through(chat, 100, false)
            .expect("replay");
        assert!(replayed.existed);
        assert_eq!(archive.removal_point(chat).expect("point"), Some(200));
        assert_eq!(archive.messages(chat, None, 10).expect("lists").len(), 1);
    }

    #[test]
    fn delete_removes_the_chat_row_without_newer_messages() {
        let archive = Archive::in_memory().expect("opens");
        let chat = "1@s.whatsapp.net";
        archive.ensure_chat(chat, "Ada").expect("chat");
        for (id, timestamp) in [("m1", 100), ("m2", 200)] {
            archive
                .insert_message(&message(chat, id, timestamp, false), None)
                .expect("insert");
        }
        let removed = archive
            .remove_chat_through(chat, 200, true)
            .expect("deletes");
        assert!(removed.existed);
        assert!(archive.chat(chat).expect("chat").is_none());
        assert!(archive.messages(chat, None, 10).expect("lists").is_empty());
        // A replayed delete finds nothing and reports it.
        let replayed = archive
            .remove_chat_through(chat, 200, true)
            .expect("replay");
        assert!(!replayed.existed, "a replay announces nothing twice");
    }

    #[test]
    fn clear_keeps_the_chat_row_without_newer_messages() {
        let archive = Archive::in_memory().expect("opens");
        let chat = "1@s.whatsapp.net";
        archive.ensure_chat(chat, "Ada").expect("chat");
        archive
            .insert_message(&message(chat, "m1", 100, false), None)
            .expect("insert");
        let removed = archive
            .remove_chat_through(chat, 100, false)
            .expect("clears");
        assert!(removed.existed);
        assert!(
            archive.chat(chat).expect("chat").is_some(),
            "chat stays listed"
        );
        assert!(archive.messages(chat, None, 10).expect("lists").is_empty());
        assert_eq!(archive.chat(chat).expect("chat").unwrap().unread, 0);
    }

    #[test]
    fn removal_collects_media_and_survives_reopen() {
        let dir = std::env::temp_dir().join(format!("zapfast-removal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        let path = dir.join("archive.db");
        let chat = "1@s.whatsapp.net";
        let removed = {
            let archive = Archive::open_with_key(&path, &[7; 32]).expect("opens");
            archive.ensure_chat(chat, "Ada").expect("chat");
            archive
                .insert_message(&image_message(chat, "m1", 100, "old.jpg"), None)
                .expect("insert");
            archive
                .insert_message(&image_message(chat, "m2", 300, "new.jpg"), None)
                .expect("insert");
            archive
                .remove_chat_through(chat, 200, false)
                .expect("removes")
        };
        assert_eq!(
            removed.media,
            vec![std::path::PathBuf::from("old.jpg")],
            "only the removed range is collected"
        );
        let archive = Archive::open_with_key(&path, &[7; 32]).expect("reopens");
        assert_eq!(archive.removal_point(chat).expect("point"), Some(200));
        let ids: Vec<String> = archive
            .messages(chat, None, 10)
            .expect("lists")
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(ids, vec!["m2".to_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removal_barrier_follows_the_lid_mapping() {
        let archive = Archive::in_memory().expect("opens");
        let lid = "123@lid";
        let pn = "15550001111@s.whatsapp.net";
        archive.ensure_chat(lid, "").expect("old row");
        archive
            .insert_message(&message(lid, "m1", 100, false), None)
            .expect("insert");
        archive
            .insert_message(&message(lid, "m2", 300, false), None)
            .expect("insert");
        archive
            .remove_chat_through(lid, 100, true)
            .expect("removes");
        archive.put_lid("123", "15550001111").expect("maps");
        assert_eq!(
            archive.removal_point(pn).expect("point"),
            Some(100),
            "the number inherits the privacy-id barrier"
        );
        assert!(archive.rekey_chat(lid, pn).expect("moves"));
        assert_eq!(
            archive.removal_point(pn).expect("point"),
            Some(100),
            "the barrier survives the rekey"
        );
        assert_eq!(archive.removal_point(lid).expect("point"), None);
        let ids: Vec<String> = archive
            .messages(pn, None, 10)
            .expect("lists")
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(ids, vec!["m2".to_owned()]);
    }

    pub(crate) fn message(chat: &str, id: &str, timestamp: i64, from_me: bool) -> Message {
        Message {
            id: id.into(),
            chat: chat.into(),
            sender: if from_me { "me@s.whatsapp.net" } else { chat }.into(),
            sender_name: None,
            from_me,
            timestamp,
            content: Content::text(format!("message {id}")),
            status: if from_me {
                Delivery::Pending
            } else {
                Delivery::None
            },
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
    fn search_finds_text_captions_and_file_names() {
        let archive = Archive::in_memory().expect("opens");
        archive
            .ensure_chat("1@s.whatsapp.net", "Ada")
            .expect("chat");
        let media = || crate::model::Media {
            mime: "application/pdf".into(),
            size: 1,
            width: None,
            height: None,
            path: None,
            state: crate::model::MediaState::Idle,
        };
        let mut plain = message("1@s.whatsapp.net", "m1", 10, false);
        plain.content = Content::text("The Difference Engine assembles");
        let mut caption = message("1@s.whatsapp.net", "m2", 20, true);
        caption.content = Content::Document {
            media: media(),
            file_name: "Notes on the Engine.pdf".into(),
            caption: Some("progress at 100% now".into()),
            pages: None,
        };
        let mut other = message("1@s.whatsapp.net", "m3", 30, false);
        other.content = Content::text("Nothing of note");
        for row in [&plain, &caption, &other] {
            archive.insert_message(row, None).expect("insert");
        }
        // A message in another chat must not answer for this one.
        let mut elsewhere = message("2@s.whatsapp.net", "m4", 5, false);
        elsewhere.content = Content::text("Late reply");
        archive.insert_message(&elsewhere, None).expect("insert");
        // Match body and filename case-insensitively, newest first.
        let hits = archive.search_messages("ENGINE", 10).expect("search");
        let ids: Vec<&str> = hits.iter().map(|hit| hit.id.as_str()).collect();
        assert_eq!(ids, vec!["m2", "m1"]);
        // Escape LIKE wildcards from the search query.
        let hits = archive.search_messages("100%", 10).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "m2");
        assert!(
            archive
                .search_messages("100&", 10)
                .expect("search")
                .is_empty(),
            "the percent sign was matched literally"
        );
        assert!(
            archive
                .search_messages("zebra", 10)
                .expect("search")
                .is_empty()
        );
        // Apply the result limit.
        let hits = archive.search_messages("e", 1).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "m3", "newest first");
        // One chat's hits can be asked for on their own.
        let here: Vec<String> = archive
            .search_messages_in(Some("1@s.whatsapp.net"), "e", 10)
            .expect("search")
            .into_iter()
            .map(|hit| hit.id)
            .collect();
        assert!(here.contains(&"m3".to_owned()));
        assert!(
            !here.contains(&"m4".to_owned()),
            "another chat's message is not a hit: {here:?}"
        );
        let there: Vec<String> = archive
            .search_messages_in(Some("2@s.whatsapp.net"), "e", 10)
            .expect("search")
            .into_iter()
            .map(|hit| hit.id)
            .collect();
        assert_eq!(there, vec!["m4".to_owned()]);
    }

    /// M3: global-search latency over 100k synthetic rows. Asserts
    /// correctness (the needle is found) and reports the elapsed time
    /// without an absolute gate: CI runners vary too much for one.
    #[test]
    fn global_search_latency_on_100k_synthetic_messages() {
        let archive = Archive::in_memory().expect("opens");
        archive
            .ensure_chat("1@s.whatsapp.net", "Ada")
            .expect("chat");
        for index in 0..100_000u32 {
            let mut row = message(
                "1@s.whatsapp.net",
                &format!("m{index}"),
                i64::from(index),
                false,
            );
            row.content = Content::text(format!("filler message number {index}"));
            archive.insert_message(&row, None).expect("insert");
        }
        let mut needle = message("1@s.whatsapp.net", "needle", 100_001, false);
        needle.content = Content::text("the needle has zebra stripes".to_owned());
        archive.insert_message(&needle, None).expect("insert");
        let start = std::time::Instant::now();
        let hits = archive.search_messages("zebra", 10).expect("search");
        let elapsed = start.elapsed();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "needle");
        eprintln!("M3: 100k-row global search took {elapsed:?}");
    }

    #[test]
    fn migrations_add_columns_to_an_older_archive() {
        let connection = Connection::open_in_memory().expect("opens");
        connection
            .execute_batch(
                "CREATE TABLE chats (id TEXT PRIMARY KEY, name TEXT NOT NULL, kind TEXT NOT NULL,
                    last_activity INTEGER NOT NULL DEFAULT 0, unread INTEGER NOT NULL DEFAULT 0,
                    archived INTEGER NOT NULL DEFAULT 0, pinned INTEGER NOT NULL DEFAULT 0, muted_until INTEGER);
                 INSERT INTO chats (id, name, kind) VALUES ('1@s.whatsapp.net', 'A', 'direct');",
            )
            .expect("old schema");
        let archive = Archive::prepare(connection).expect("migrates");
        let chats = archive.chats().expect("chats");
        assert_eq!(chats.len(), 1);
        assert!(chats[0].participants.is_empty());
        assert!(!chats[0].read_only);
        let mut with_thumbnail = message("1@s.whatsapp.net", "m1", 1, false);
        with_thumbnail.thumbnail = Some(vec![1, 2, 3]);
        archive
            .insert_message(&with_thumbnail, None)
            .expect("insert");
        assert_eq!(
            archive
                .message("1@s.whatsapp.net", "m1")
                .expect("read")
                .expect("exists")
                .thumbnail,
            Some(vec![1, 2, 3])
        );
        assert_eq!(
            archive
                .oldest("1@s.whatsapp.net")
                .expect("oldest")
                .map(|m| m.id),
            Some("m1".into())
        );
    }

    #[test]
    fn ephemeral_setting_keeps_the_newest_timestamp() {
        let archive = Archive::in_memory().expect("opens");
        let chat = "1@s.whatsapp.net";
        archive.ensure_chat(chat, "Ada").expect("chat");

        assert!(archive.set_ephemeral(chat, 604_800, 20).expect("setting"));
        assert!(!archive.set_ephemeral(chat, 86_400, 10).expect("stale"));

        assert_eq!(
            archive.ephemeral_expiration(chat).expect("expiration"),
            Some(604_800)
        );
    }

    #[test]
    fn ephemeral_setting_preserves_explicitly_disabled_timer() {
        let archive = Archive::in_memory().expect("opens");
        let chat = "1@s.whatsapp.net";
        archive.ensure_chat(chat, "Ada").expect("chat");

        archive.set_ephemeral(chat, 0, 20).expect("setting");

        assert_eq!(
            archive.ephemeral_expiration(chat).expect("expiration"),
            Some(0)
        );
    }

    #[test]
    fn group_info_is_kept() {
        let archive = Archive::in_memory().expect("opens");
        let chat = "1-2@g.us";
        archive.ensure_chat(chat, "Group").expect("chat");
        archive
            .set_group_info(
                chat,
                Some("Rust Berlin"),
                &["a@s.whatsapp.net".into()],
                true,
            )
            .expect("info");
        let row = archive.chat(chat).expect("chat").expect("exists");
        assert_eq!(row.name, "Rust Berlin");
        assert_eq!(row.participants, vec!["a@s.whatsapp.net"]);
        assert!(row.read_only);
        archive
            .set_group_info(chat, None, &[], false)
            .expect("info");
        assert_eq!(
            archive.chat(chat).expect("chat").expect("exists").name,
            "Rust Berlin"
        );
    }

    #[test]
    fn existing_archives_request_preference_recovery_once_across_restarts() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fixture.db");
        let key = [31; 32];
        {
            let archive = Archive::open_with_key(&path, &key).unwrap();
            archive.ensure_chat("1@s.whatsapp.net", "Fixture").unwrap();
            assert!(archive.take_preferences_refresh().unwrap());
            assert!(!archive.take_preferences_refresh().unwrap());
        }
        let archive = Archive::open_with_key(&path, &key).unwrap();
        assert!(!archive.take_preferences_refresh().unwrap());
        let fresh = Archive::in_memory().unwrap();
        assert!(!fresh.take_preferences_refresh().unwrap());
        fresh
            .ensure_chat("1@s.whatsapp.net", "Initial history")
            .unwrap();
        assert!(!fresh.take_preferences_refresh().unwrap());
    }

    #[test]
    fn mute_and_pin_versions_survive_restart_and_ignore_older_updates() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fixture.db");
        let key = [29; 32];
        let id = "1@s.whatsapp.net";
        {
            let archive = Archive::open_with_key(&path, &key).unwrap();
            archive.ensure_chat(id, "Fixture").unwrap();
            archive.set_muted_at(id, Some(0), 200).unwrap();
            archive.set_pinned_at(id, true, 200).unwrap();
        }
        let archive = Archive::open_with_key(&path, &key).unwrap();
        archive.set_muted_at(id, None, 100).unwrap();
        archive.set_pinned_at(id, false, 100).unwrap();
        archive
            .upsert_chat(&Chat::new(id.into(), "History name".into()))
            .unwrap();
        let chat = archive.chat(id).unwrap().unwrap();
        assert_eq!(chat.name, "History name");
        assert_eq!(chat.muted_until, Some(0));
        assert!(chat.pinned);
        assert_eq!(chat.pinned_at, 200);
        archive.set_muted_at(id, None, 300).unwrap();
        archive.set_pinned_at(id, false, 300).unwrap();
        let chat = archive.chat(id).unwrap().unwrap();
        assert_eq!(chat.muted_until, None);
        assert!(!chat.pinned);
        assert_eq!(chat.pinned_at, 0);
    }

    #[test]
    fn chats_order_by_activity_and_carry_their_last_message() {
        let archive = Archive::in_memory().expect("opens");
        let a = "1@s.whatsapp.net";
        let b = "2@s.whatsapp.net";
        archive.ensure_chat(a, "A").expect("chat");
        archive.ensure_chat(b, "B").expect("chat");
        archive
            .insert_message(&message(a, "m1", 100, false), None)
            .expect("insert");
        archive
            .insert_message(&message(b, "m2", 200, true), None)
            .expect("insert");
        archive
            .insert_message(&message(a, "m3", 150, false), None)
            .expect("insert");
        let chats = archive.chats().expect("chats");
        assert_eq!(chats[0].id, b);
        assert_eq!(
            chats[0].last.as_ref().map(|last| last.summary.as_str()),
            Some("message m2")
        );
        assert_eq!(
            chats[0].last.as_ref().map(|last| last.status),
            Some(Delivery::Pending)
        );
        assert_eq!(chats[1].id, a);
        assert_eq!(chats[1].last_activity, 150);
        assert_eq!(
            chats[1].last.as_ref().map(|last| last.summary.as_str()),
            Some("message m3")
        );
    }

    #[test]
    fn a_saved_name_keeps_the_push_name_beside_it() {
        let archive = Archive::in_memory().expect("opens");
        let id = "491700000001@s.whatsapp.net";
        archive
            .upsert_contact(&Contact {
                id: id.into(),
                full_name: None,
                push_name: Some("~slavic".into()),
            })
            .expect("stores");
        archive
            .upsert_contact(&Contact {
                id: id.into(),
                full_name: Some("Slavic".into()),
                push_name: None,
            })
            .expect("renames");
        let stored = archive.contact(id).expect("reads").expect("exists");
        assert_eq!(stored.full_name.as_deref(), Some("Slavic"));
        assert_eq!(stored.push_name.as_deref(), Some("~slavic"));
        assert!(
            archive
                .contact("nobody@s.whatsapp.net")
                .expect("reads")
                .is_none()
        );
    }

    #[test]
    fn statuses_only_move_forward() {
        let archive = Archive::in_memory().expect("opens");
        let chat = "1@s.whatsapp.net";
        archive.ensure_chat(chat, "A").expect("chat");
        archive
            .insert_message(&message(chat, "m1", 100, true), None)
            .expect("insert");
        assert!(
            archive
                .set_status(chat, "m1", Delivery::Read, 500)
                .expect("status")
        );
        assert!(
            !archive
                .set_status(chat, "m1", Delivery::Delivered, 600)
                .expect("status")
        );
        let stored = archive.message(chat, "m1").expect("read").expect("exists");
        assert_eq!(stored.status, Delivery::Read);
        assert_eq!(stored.read_at, Some(500));
        assert_eq!(stored.delivered_at, None);
        // History replay must not lower an existing Read state.
        archive
            .insert_message(&message(chat, "m1", 100, true), None)
            .expect("insert");
        assert_eq!(
            archive
                .message(chat, "m1")
                .expect("read")
                .expect("exists")
                .status,
            Delivery::Read
        );
        assert!(
            archive
                .set_status(chat, "m1", Delivery::Failed, 700)
                .expect("status")
        );
    }

    #[test]
    fn a_read_receipt_covers_everything_before_it() {
        let archive = Archive::in_memory().expect("opens");
        let chat = "1@s.whatsapp.net";
        archive.ensure_chat(chat, "A").expect("chat");
        for (id, timestamp) in [("m1", 100), ("m2", 200), ("m3", 300)] {
            archive
                .insert_message(&message(chat, id, timestamp, true), None)
                .expect("insert");
        }
        archive
            .insert_message(&message(chat, "theirs", 250, false), None)
            .expect("insert");
        let changed = archive
            .advance_statuses(chat, 200, Delivery::Read, 400)
            .expect("advance");
        assert_eq!(changed, vec!["m1", "m2"]);
        let messages = archive.messages(chat, None, 10).expect("messages");
        let statuses: Vec<Delivery> = messages.iter().map(|message| message.status).collect();
        assert_eq!(
            statuses,
            vec![
                Delivery::Read,
                Delivery::Read,
                Delivery::None,
                Delivery::Pending
            ]
        );
        assert_eq!(messages[0].read_at, Some(400));
    }

    #[test]
    fn paging_walks_backwards_in_time() {
        let archive = Archive::in_memory().expect("opens");
        let chat = "1@s.whatsapp.net";
        archive.ensure_chat(chat, "A").expect("chat");
        for index in 0..10 {
            archive
                .insert_message(
                    &message(chat, &format!("m{index}"), 100 + index, false),
                    None,
                )
                .expect("insert");
        }
        let newest = archive.messages(chat, None, 3).expect("messages");
        assert_eq!(
            newest.iter().map(|m| m.timestamp).collect::<Vec<_>>(),
            vec![107, 108, 109]
        );
        let older = archive
            .messages(chat, Some((107, "m7")), 3)
            .expect("messages");
        assert_eq!(
            older.iter().map(|m| m.timestamp).collect::<Vec<_>>(),
            vec![104, 105, 106]
        );
    }

    #[test]
    fn paging_keeps_every_message_of_a_second() {
        // Cover messages sharing one timestamp across page boundaries.
        let archive = Archive::in_memory().expect("opens");
        let chat = "1@s.whatsapp.net";
        archive.ensure_chat(chat, "A").expect("chat");
        archive
            .insert_message(&message(chat, "before", 99, false), None)
            .expect("insert");
        for index in 0..5 {
            archive
                .insert_message(&message(chat, &format!("a{index}"), 100, false), None)
                .expect("insert");
        }
        let first = archive.messages(chat, None, 3).expect("messages");
        assert_eq!(
            first.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["a2", "a3", "a4"]
        );
        let oldest = &first[0];
        let second = archive
            .messages(chat, Some((oldest.timestamp, &oldest.id)), 3)
            .expect("messages");
        assert_eq!(
            second.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["before", "a0", "a1"],
            "the rest of the second comes next, not the message before it alone"
        );
        let range = archive
            .messages_range(chat, 100, (100, "a2"), 10)
            .expect("range");
        assert_eq!(
            range.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["a0", "a1"]
        );
    }

    #[test]
    fn paging_survives_a_deleted_cursor() {
        // Five messages share timestamp 100; the middle one pages, then
        // is deleted before the next page runs with the stale cursor.
        let archive = Archive::in_memory().expect("opens");
        let chat = "1@s.whatsapp.net";
        archive.ensure_chat(chat, "A").expect("chat");
        archive
            .insert_message(&message(chat, "before", 99, false), None)
            .expect("insert");
        for index in 0..5 {
            archive
                .insert_message(&message(chat, &format!("a{index}"), 100, false), None)
                .expect("insert");
        }
        let first = archive.messages(chat, None, 3).expect("messages");
        assert_eq!(
            first.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["a2", "a3", "a4"]
        );
        // The cursor a2 vanishes before the next page.
        archive
            .delete_message_for_me(chat, "a2", 101)
            .expect("delete");
        let second = archive
            .messages(chat, Some((100, "a2")), 10)
            .expect("messages");
        // a2 lived on the first page; every other survivor must arrive
        // now, exactly once, with no jump over the deleted cursor.
        let mut second_ids: Vec<&str> = second.iter().map(|m| m.id.as_str()).collect();
        second_ids.sort_unstable();
        assert_eq!(second_ids, ["a0", "a1", "a3", "a4", "before"]);
        let mut union: Vec<&str> = first
            .iter()
            .chain(second.iter())
            .map(|m| m.id.as_str())
            .collect();
        union.sort_unstable();
        union.dedup();
        assert_eq!(union, ["a0", "a1", "a2", "a3", "a4", "before"]);
        // The range query shares the fallback: deleted cursor, same union.
        let range = archive
            .messages_range(chat, 90, (100, "a2"), 10)
            .expect("range");
        let mut ids: Vec<&str> = range.iter().map(|m| m.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["a0", "a1", "a3", "a4", "before"]);
    }

    #[test]
    fn replay_never_resurrects_a_revoked_message() {
        // Late history replay of a revoked id must not resurrect it.
        let archive = Archive::in_memory().expect("opens");
        let chat = "1@s.whatsapp.net";
        archive.ensure_chat(chat, "A").expect("chat");
        archive
            .insert_message(&message(chat, "m1", 100, false), None)
            .expect("insert");
        archive
            .set_content(chat, "m1", &Content::Revoked, false)
            .expect("revoke");
        archive
            .insert_message(&message(chat, "m1", 100, false), None)
            .expect("replay");
        let row = archive.message(chat, "m1").expect("row").expect("present");
        assert!(
            matches!(row.content, Content::Revoked),
            "revocation sticks across replays"
        );
    }

    #[test]
    fn ranges_and_deletion() {
        let archive = Archive::in_memory().expect("opens");
        let chat = "1@s.whatsapp.net";
        archive.ensure_chat(chat, "A").expect("chat");
        for index in 0..6 {
            archive
                .insert_message(
                    &message(chat, &format!("m{index}"), 100 + index, false),
                    None,
                )
                .expect("insert");
        }
        let range = archive
            .messages_range(chat, 102, (105, "m5"), 10)
            .expect("range");
        assert_eq!(
            range.iter().map(|m| m.timestamp).collect::<Vec<_>>(),
            vec![102, 103, 104]
        );
        assert!(archive.delete_message(chat, "m3").expect("delete"));
        assert!(!archive.delete_message(chat, "m3").expect("delete"));
        assert!(archive.message(chat, "m3").expect("read").is_none());
    }

    #[test]
    fn rekey_older_number_yields_to_newer_lid_intent() {
        let archive = Archive::in_memory().expect("opens");
        archive
            .queue_chat_sync("1@lid", "archived", true, 300)
            .expect("intent");
        archive
            .queue_chat_sync("55@s.whatsapp.net", "archived", false, 100)
            .expect("intent");
        assert!(
            archive
                .rekey_chat("1@lid", "55@s.whatsapp.net")
                .expect("rekey")
        );
        let (value, updated, _) = archive
            .queued_chat_sync("55@s.whatsapp.net", "archived")
            .expect("queue")
            .expect("winner");
        assert!(value);
        assert_eq!(updated, 300);
        assert!(
            archive
                .queued_chat_sync("1@lid", "archived")
                .expect("queue")
                .is_none()
        );
    }

    #[test]
    fn rekey_aborted_insert_keeps_the_origin_intent() {
        let archive = Archive::in_memory().expect("opens");
        archive
            .queue_chat_sync("1@lid", "archived", true, 100)
            .expect("intent");
        archive
            .connection
            .execute_batch("CREATE TRIGGER sync_abort BEFORE INSERT ON chat_sync_queue BEGIN SELECT RAISE(ABORT, 'boom'); END;")
            .expect("trigger");
        assert!(archive.rekey_chat("1@lid", "55@s.whatsapp.net").is_err());
        archive
            .connection
            .execute_batch("DROP TRIGGER sync_abort")
            .expect("cleanup");
        let (value, updated, _) = archive
            .queued_chat_sync("1@lid", "archived")
            .expect("queue")
            .expect("origin kept");
        assert!(value);
        assert_eq!(updated, 100);
        assert!(
            archive
                .queued_chat_sync("55@s.whatsapp.net", "archived")
                .expect("queue")
                .is_none()
        );
    }

    #[test]
    fn complete_chat_sync_rolls_back_when_order_write_fails() {
        let archive = Archive::in_memory().expect("opens");
        archive.ensure_chat("c", "C").expect("chat");
        let rev = archive
            .queue_chat_sync("c", "archived", true, 100)
            .expect("intent");
        archive
            .connection
            .execute_batch("CREATE TRIGGER order_abort BEFORE INSERT ON chat_sync_order BEGIN SELECT RAISE(ABORT, 'boom'); END;")
            .expect("trigger");
        assert!(archive.complete_chat_sync("c", rev).is_err());
        archive
            .connection
            .execute_batch("DROP TRIGGER order_abort")
            .expect("cleanup");
        assert!(
            archive
                .queued_chat_sync("c", "archived")
                .expect("queue")
                .is_some(),
            "intent kept"
        );
        assert!(
            archive.sync_order("c").expect("order").is_none(),
            "no half-persisted order"
        );
        // Recovery on the same store converges.
        assert!(archive.complete_chat_sync("c", rev).is_ok());
        assert_eq!(archive.sync_order("c").expect("order"), Some((100, true)));
    }

    #[test]
    fn sync_order_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("zapfast-sync-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        let path = dir.join("archive.db");
        let rev = {
            let archive = Archive::open(&path).expect("opens");
            archive.ensure_chat("c", "C").expect("chat");
            let rev = archive
                .queue_chat_sync("c", "archived", true, 100)
                .expect("intent");
            archive.complete_chat_sync("c", rev).expect("completes");
            assert_eq!(archive.sync_order("c").expect("order"), Some((100, true)));
            rev
        };
        let _ = rev;
        let reopened = Archive::open(&path).expect("reopens");
        assert_eq!(reopened.sync_order("c").expect("order"), Some((100, true)));
        assert!(
            reopened
                .queued_chat_sync("c", "archived")
                .expect("queue")
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn a_favorite_intent_survives_a_restart_unpushed() {
        let dir = std::env::temp_dir().join(format!("zapfast-fav-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        let path = dir.join("archive.db");
        {
            let archive = Archive::open(&path).expect("opens");
            archive
                .set_favorite_sticker("aa", true, 30, Some(b"refs"), false)
                .expect("stores");
        }
        let reopened = Archive::open(&path).expect("reopens");
        let stored = reopened
            .favorite_sticker("aa")
            .expect("reads")
            .expect("row");
        assert!(stored.favorite && !stored.pushed);
        assert_eq!(stored.action.as_deref(), Some(&b"refs"[..]));
        assert_eq!(
            reopened.unpushed_favorite_stickers().expect("lists").len(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tombstone_under_old_id_dies_with_the_row_on_rekey() {
        // Mutation applied before the mapping was known: tombstone under
        // one id, row under the other. Learning the mapping must condemn
        // the row, and a restart must keep it dead.
        let dir = std::env::temp_dir().join(format!("zapfast-rekey-tomb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creates");
        let path = dir.join("archive.db");
        let archive = Archive::open(&path).expect("opens");
        archive.ensure_chat("1@lid", "Old").expect("chat");
        archive
            .ensure_chat("55@s.whatsapp.net", "New")
            .expect("chat");
        archive
            .insert_message(&message("1@lid", "m1", 100, false), None)
            .expect("insert");
        archive
            .tombstone_message("55@s.whatsapp.net", "m1", 150)
            .expect("tombstone");
        assert!(
            archive
                .rekey_chat("1@lid", "55@s.whatsapp.net")
                .expect("rekey")
        );
        assert!(
            archive
                .message("55@s.whatsapp.net", "m1")
                .expect("row")
                .is_none()
        );
        assert!(archive.message("1@lid", "m1").expect("row").is_none());
        drop(archive);
        let reopened = Archive::open(&path).expect("reopens");
        assert!(
            reopened
                .message("55@s.whatsapp.net", "m1")
                .expect("row")
                .is_none()
        );
        assert!(
            reopened
                .is_tombstoned("55@s.whatsapp.net", "m1")
                .expect("tombstone")
        );
        reopened
            .insert_message(&message("55@s.whatsapp.net", "m1", 100, false), None)
            .expect("replay");
        assert!(
            reopened
                .message("55@s.whatsapp.net", "m1")
                .expect("row")
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_second_pages_cover_every_row_once() {
        let archive = Archive::in_memory().expect("opens");
        let chat = "1@s.whatsapp.net";
        archive.ensure_chat(chat, "A").expect("chat");
        for index in 0..5 {
            archive
                .insert_message(&message(chat, &format!("m{index}"), 100, false), None)
                .expect("insert");
        }
        let first = archive.messages(chat, None, 2).expect("first");
        let cursor = (first[0].timestamp, first[0].id.clone());
        let second = archive
            .messages(chat, Some((cursor.0, &cursor.1)), 10)
            .expect("second");
        let mut union: Vec<String> = first.into_iter().chain(second).map(|row| row.id).collect();
        union.sort_unstable();
        union.dedup();
        assert_eq!(
            union,
            vec![
                "m0".to_owned(),
                "m1".to_owned(),
                "m2".to_owned(),
                "m3".to_owned(),
                "m4".to_owned()
            ]
        );
    }

    #[test]
    fn rekey_without_queue_reconciles_flag_to_newest_order() {
        // Each row: old flag and time, new flag and time, expected winner.
        for (old_archived, old_ms, new_archived, new_ms, expected) in [
            (true, 100, false, 200, false),
            (true, 200, false, 100, true),
        ] {
            let archive = Archive::in_memory().expect("opens");
            archive.ensure_chat("1@lid", "Old").expect("chat");
            archive
                .ensure_chat("55@s.whatsapp.net", "New")
                .expect("chat");
            archive
                .apply_remote_archive("1@lid", old_archived, old_ms)
                .expect("order");
            archive
                .apply_remote_archive("55@s.whatsapp.net", new_archived, new_ms)
                .expect("order");
            assert!(
                archive
                    .rekey_chat("1@lid", "55@s.whatsapp.net")
                    .expect("rekey")
            );
            assert_eq!(
                archive
                    .chat("55@s.whatsapp.net")
                    .expect("read")
                    .expect("row")
                    .archived,
                expected,
                "newest accepted state wins without intent"
            );
            assert!(
                archive
                    .queued_chat_sync("55@s.whatsapp.net", "archived")
                    .expect("queue")
                    .is_none(),
                "no intent invented"
            );
        }
    }

    #[test]
    fn rekey_newer_order_beats_stale_intent() {
        let archive = Archive::in_memory().expect("opens");
        archive.ensure_chat("1@lid", "Old").expect("chat");
        archive
            .ensure_chat("55@s.whatsapp.net", "New")
            .expect("chat");
        // Accepted unarchive at T300 outranks a stale archive intent at T100.
        archive
            .apply_remote_archive("55@s.whatsapp.net", false, 300)
            .expect("order");
        archive
            .queue_chat_sync("1@lid", "archived", true, 100)
            .expect("intent");
        assert!(
            archive
                .rekey_chat("1@lid", "55@s.whatsapp.net")
                .expect("rekey")
        );
        assert!(
            !archive
                .chat("55@s.whatsapp.net")
                .expect("read")
                .expect("row")
                .archived,
            "stale intent cannot flip the newer accepted state"
        );
        assert_eq!(
            archive.sync_order("55@s.whatsapp.net").expect("order"),
            Some((300, false))
        );
    }
    #[test]
    fn sync_rev_starts_above_legacy_queued_rows() {
        let archive = Archive::in_memory().expect("opens");
        archive
            .connection
            .execute("INSERT INTO chat_sync_queue (chat, setting, value, updated_ms, rev) VALUES ('c', 'archived', 1, 50, 7)", [])
            .expect("legacy row");
        assert_eq!(archive.alloc_sync_rev().expect("counter"), 8);
    }

    #[test]
    fn insert_below_a_removal_barrier_stays_gone() {
        let archive = Archive::in_memory().expect("opens");
        archive.ensure_chat("c", "C").expect("chat");
        // History sync writes straight through insert_message, so the
        // barrier must live here and not only in the live-message guard.
        archive
            .remove_chat_through("c", 200, false)
            .expect("barrier");
        let mut old = message("c", "old", 100, false);
        archive.insert_message(&old, None).expect("insert");
        assert!(archive.message("c", "old").expect("row").is_none());
        old.id = "new".to_string();
        // Timestamps are seconds in the archive: 300 stays above 200.
        old.timestamp = 300;
        archive.insert_message(&old, None).expect("insert");
        assert!(archive.message("c", "new").expect("row").is_some());
    }
    #[test]
    fn sync_rev_never_repeats_after_clear() {
        let archive = Archive::in_memory().expect("opens");
        let first = archive
            .queue_chat_sync("c", "archived", true, 1)
            .expect("intent");
        archive.clear_chat_sync("c", "archived").expect("clears");
        let second = archive
            .queue_chat_sync("c", "archived", false, 2)
            .expect("intent");
        assert!(
            second > first,
            "cleared rows must not free revisions: {first} then {second}"
        );
        archive
            .clear_chat_sync_if_rev("c", "archived", first)
            .expect("stale clear");
        assert!(
            archive
                .queued_chat_sync("c", "archived")
                .expect("queue")
                .is_some(),
            "exact equality only"
        );
    }

    #[test]
    fn rekey_reconciles_the_flag_and_condemns_survivors() {
        let archive = Archive::in_memory().expect("opens");
        archive.ensure_chat("1@lid", "Old").expect("chat");
        archive
            .insert_message(&message("1@lid", "m1", 100, false), None)
            .expect("insert");
        // Stale archived flag under the old id must not leak through the merge.
        archive
            .queue_chat_sync("1@lid", "archived", true, 100)
            .expect("intent");
        archive
            .ensure_chat("55@s.whatsapp.net", "New")
            .expect("chat");
        archive
            .insert_message(&message("55@s.whatsapp.net", "m2", 200, false), None)
            .expect("insert");
        archive
            .queue_chat_sync("55@s.whatsapp.net", "archived", false, 200)
            .expect("intent");
        // A row condemned by a tombstone from the other id must not survive.
        archive
            .tombstone_message("1@lid", "m2", 50)
            .expect("tombstone");
        assert!(
            archive
                .rekey_chat("1@lid", "55@s.whatsapp.net")
                .expect("rekey")
        );
        assert!(
            !archive
                .chat("55@s.whatsapp.net")
                .expect("read")
                .expect("row")
                .archived,
            "winning intent outranks the OR merge"
        );
        assert!(
            archive
                .message("55@s.whatsapp.net", "m1")
                .expect("read")
                .is_some()
        );
        assert!(
            archive
                .message("55@s.whatsapp.net", "m2")
                .expect("read")
                .is_none()
        );
        assert_eq!(
            archive
                .chat("55@s.whatsapp.net")
                .expect("read")
                .expect("row")
                .unread,
            0
        );
    }

    #[test]
    fn rekey_rolls_back_when_protection_copy_fails() {
        let archive = Archive::in_memory().expect("opens");
        archive.ensure_chat("1@lid", "Old").expect("chat");
        archive
            .insert_message(&message("1@lid", "m1", 100, false), None)
            .expect("insert");
        archive
            .queue_chat_sync("1@lid", "archived", true, 100)
            .expect("intent");
        archive
            .tombstone_message("1@lid", "m2", 50)
            .expect("tombstone");
        // Fail after the message, receipt, and removal steps already ran.
        archive
            .drop_table_for_test("chat_sync_queue")
            .expect("sabotage");
        assert!(archive.rekey_chat("1@lid", "55@s.whatsapp.net").is_err());
        // Nothing moved: rows, tombstones, and intents are all still home.
        assert!(archive.message("1@lid", "m1").expect("read").is_some());
        assert!(archive.is_tombstoned("1@lid", "m2").expect("tombstone"));
        assert!(
            archive.chat("1@lid").expect("read").is_some(),
            "chat row unmoved"
        );
    }

    #[test]
    fn rekey_moves_tombstones_and_sync_intents() {
        let archive = Archive::in_memory().expect("opens");
        archive.ensure_chat("1@lid", "Old").expect("chat");
        archive
            .insert_message(&message("1@lid", "m1", 100, false), None)
            .expect("insert");
        let (deleted, _) = archive
            .delete_message_for_me("1@lid", "m1", 50)
            .expect("deletes");
        assert!(deleted);
        archive
            .queue_chat_sync("1@lid", "archived", true, 100)
            .expect("intent");
        archive
            .ensure_chat("55@s.whatsapp.net", "New")
            .expect("chat");
        // A newer intent already filed under the number wins the merge.
        archive
            .queue_chat_sync("55@s.whatsapp.net", "archived", false, 200)
            .expect("intent");
        // A lone intent moves with a fresh revision.
        archive
            .queue_chat_sync("2@lid", "archived", true, 150)
            .expect("intent");
        assert!(
            archive
                .rekey_chat("1@lid", "55@s.whatsapp.net")
                .expect("rekey")
        );
        assert!(!archive.is_tombstoned("1@lid", "m1").expect("moved"));
        assert!(
            archive
                .is_tombstoned("55@s.whatsapp.net", "m1")
                .expect("moved")
        );
        archive
            .insert_message(&message("55@s.whatsapp.net", "m1", 100, false), None)
            .expect("replay");
        assert!(
            archive
                .message("55@s.whatsapp.net", "m1")
                .expect("read")
                .is_none()
        );
        let (value, updated, _) = archive
            .queued_chat_sync("55@s.whatsapp.net", "archived")
            .expect("queue")
            .expect("intent");
        assert!(!value);
        assert_eq!(updated, 200);
        assert!(
            archive
                .rekey_chat("2@lid", "66@s.whatsapp.net")
                .expect("rekey")
        );
        let (value, _, rev) = archive
            .queued_chat_sync("66@s.whatsapp.net", "archived")
            .expect("queue")
            .expect("intent");
        assert!(value);
        assert!(rev >= 4, "moved intent takes a fresh revision, got {rev}");
        assert!(
            archive
                .queued_chat_sync("2@lid", "archived")
                .expect("queue")
                .is_none()
        );
    }

    #[test]
    fn queue_failure_leaves_no_partial_archive_state() {
        let archive = Archive::in_memory().expect("opens");
        archive.ensure_chat("c", "C").expect("chat");
        archive
            .connection
            .execute_batch("PRAGMA query_only = ON")
            .expect("pragma");
        assert!(archive.set_archived_queued("c", true, 1).is_err());
        archive
            .connection
            .execute_batch("PRAGMA query_only = OFF")
            .expect("pragma");
        // Neither the flag nor the intent survived the failed transaction.
        assert!(!archive.chat("c").expect("read").expect("row").archived);
        assert!(
            archive
                .queued_chat_sync("c", "archived")
                .expect("read")
                .is_none()
        );
    }

    #[test]
    fn chat_sync_intents_keep_only_the_newest() {
        let archive = Archive::in_memory().expect("opens");
        assert!(archive.pending_chat_syncs().expect("reads").is_empty());
        archive
            .queue_chat_sync("c", "archived", true, 100)
            .expect("queues");
        archive
            .queue_chat_sync("c", "archived", false, 200)
            .expect("replaces");
        assert_eq!(
            archive.queued_chat_sync("c", "archived").expect("reads"),
            Some((false, 200, 2))
        );
        assert_eq!(archive.pending_chat_syncs().expect("reads").len(), 1);
        archive.clear_chat_sync("c", "archived").expect("clears");
        assert!(
            archive
                .queued_chat_sync("c", "archived")
                .expect("reads")
                .is_none()
        );
    }

    #[test]
    fn delete_for_me_tombstones_against_replay() {
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join("shared.mp4");
        let lone = root.path().join("lone.mp4");
        std::fs::write(&shared, b"bytes").unwrap();
        std::fs::write(&lone, b"bytes").unwrap();
        let archive = Archive::in_memory().expect("opens");
        archive.ensure_chat("chat", "Chat").expect("chat");
        archive
            .insert_message(&video_message("chat", "m1", None, &shared), None)
            .expect("insert");
        archive
            .insert_message(&video_message("chat", "m2", None, &shared), None)
            .expect("insert");
        archive
            .insert_message(&video_message("chat", "m3", None, &lone), None)
            .expect("insert");
        let (deleted, media) = archive
            .delete_message_for_me("chat", "m1", 50)
            .expect("deletes");
        assert!(deleted);
        assert_eq!(media, vec![shared.clone()]);
        assert!(archive.message("chat", "m1").expect("read").is_none());
        // A late replay of the same id stays gone; the survivor still files.
        archive
            .insert_message(&video_message("chat", "m1", None, &shared), None)
            .expect("replay");
        assert!(archive.message("chat", "m1").expect("read").is_none());
        assert!(archive.message("chat", "m2").expect("read").is_some());
        // An unknown id tombstones quietly so a delete that arrived first still wins.
        let (deleted, _) = archive
            .delete_message_for_me("chat", "mx", 60)
            .expect("tombstones");
        assert!(!deleted);
        archive
            .insert_message(&video_message("chat", "mx", None, &shared), None)
            .expect("replay");
        assert!(archive.message("chat", "mx").expect("read").is_none());
        let (deleted, media) = archive
            .delete_message_for_me("chat", "m3", 70)
            .expect("deletes");
        assert!(deleted);
        assert_eq!(media, vec![lone]);
    }

    #[test]
    fn reactions_replace_per_sender() {
        let archive = Archive::in_memory().expect("opens");
        let chat = "1@s.whatsapp.net";
        archive.ensure_chat(chat, "A").expect("chat");
        archive
            .insert_message(&message(chat, "m1", 100, false), None)
            .expect("insert");
        archive
            .set_reaction(chat, "m1", chat, false, "👍")
            .expect("react");
        let updated = archive
            .set_reaction(chat, "m1", chat, false, "❤️")
            .expect("react")
            .expect("exists");
        assert_eq!(updated.reactions.len(), 1);
        assert_eq!(updated.reactions[0].emoji, "❤️");
        let removed = archive
            .set_reaction(chat, "m1", chat, false, "")
            .expect("react")
            .expect("exists");
        assert!(removed.reactions.is_empty());
    }

    #[test]
    fn unread_counts_and_incoming_ids_track_the_other_side() {
        let archive = Archive::in_memory().expect("opens");
        let chat = "1@s.whatsapp.net";
        archive.ensure_chat(chat, "A").expect("chat");
        archive
            .insert_message(&message(chat, "m1", 100, false), None)
            .expect("insert");
        archive.bump_unread(chat).expect("bump");
        archive
            .insert_message(&message(chat, "mine", 150, true), None)
            .expect("insert");
        archive
            .insert_message(&message(chat, "m2", 200, false), None)
            .expect("insert");
        archive.bump_unread(chat).expect("bump");
        assert_eq!(archive.chat(chat).expect("chat").expect("exists").unread, 2);
        let ids: Vec<String> = archive
            .unread_incoming(chat, 2)
            .expect("ids")
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec!["m2", "m1"]);
        archive.mark_read(chat).expect("read");
        assert_eq!(archive.chat(chat).expect("chat").expect("exists").unread, 0);
    }

    #[test]
    fn read_positions_and_pending_sync_survive_reopening_the_archive() {
        let dir = std::env::temp_dir().join(format!("zapfast-read-test-{}", std::process::id()));
        let path = dir.join("archive.db");
        let _ = std::fs::remove_dir_all(&dir);
        let chat = "1@s.whatsapp.net";
        {
            let archive = Archive::open_with_key(&path, &[7; 32]).unwrap();
            archive.ensure_chat(chat, "A").unwrap();
            archive
                .insert_message(&message(chat, "a", 100, false), None)
                .unwrap();
            archive.bump_unread(chat).unwrap();
            archive.mark_read(chat).unwrap();
            archive.queue_read_sync(chat).unwrap();
        }
        {
            let archive = Archive::open_with_key(&path, &[7; 32]).unwrap();
            assert_eq!(archive.read_through(chat).unwrap(), Some(100));
            assert_eq!(archive.pending_reads().unwrap(), vec![(chat.into(), 100)]);
            archive
                .insert_message(&message(chat, "b", 200, false), None)
                .unwrap();
            archive.bump_unread(chat).unwrap();
            archive.mark_read(chat).unwrap();
            archive.queue_read_sync(chat).unwrap();
            archive.finish_read_sync(chat, 100).unwrap();
            assert_eq!(
                archive.pending_reads().unwrap(),
                vec![(chat.into(), 200)],
                "an old completion must not lose the next read"
            );
            archive.finish_read_sync(chat, 200).unwrap();
            assert!(archive.pending_reads().unwrap().is_empty());
            archive.clear().unwrap();
            assert!(archive.read_through(chat).unwrap().is_none());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn media_paths_are_written_into_the_content() {
        let archive = Archive::in_memory().expect("opens");
        let chat = "1@s.whatsapp.net";
        archive.ensure_chat(chat, "A").expect("chat");
        let mut picture = message(chat, "p1", 100, false);
        picture.content = Content::Image {
            caption: None,
            media: crate::model::Media {
                mime: "image/jpeg".into(),
                size: 10,
                width: None,
                height: None,
                path: None,
                state: Default::default(),
            },
        };
        archive.insert_message(&picture, None).expect("insert");
        let updated = archive
            .set_media_path(chat, "p1", Path::new("/tmp/p1.jpg"))
            .expect("set")
            .expect("exists");
        assert_eq!(
            updated.content.media().and_then(|media| media.path.clone()),
            Some(std::path::PathBuf::from("/tmp/p1.jpg"))
        );
        let reread = archive.message(chat, "p1").expect("read").expect("exists");
        assert_eq!(reread.content, updated.content);
    }

    #[test]
    fn raw_bytes_survive_a_replay_without_them() {
        let archive = Archive::in_memory().expect("opens");
        let chat = "1@s.whatsapp.net";
        archive.ensure_chat(chat, "A").expect("chat");
        archive
            .insert_message(&message(chat, "m1", 100, false), Some(&[1, 2, 3]))
            .expect("insert");
        archive
            .insert_message(&message(chat, "m1", 100, false), None)
            .expect("insert");
        assert_eq!(archive.raw(chat, "m1").expect("raw"), Some(vec![1, 2, 3]));
    }
}

#[cfg(test)]
mod sticker_tests {
    use super::*;
    use crate::model::{Content, Delivery, Media, MediaState};
    #[test]
    fn sticker_favourites_toggle_and_survive_a_damaged_list() {
        let archive = Archive::in_memory().expect("opens");
        let path = std::path::PathBuf::from("/stickers/frog.webp");
        assert!(archive.sticker_favorites().expect("fresh").is_empty());
        assert!(archive.toggle_sticker_favorite(&path).expect("adds"));
        assert_eq!(
            archive.sticker_favorites().expect("list"),
            vec![path.clone()]
        );
        assert!(!archive.toggle_sticker_favorite(&path).expect("removes"));
        assert!(archive.sticker_favorites().expect("list").is_empty());
        // A damaged list counts as empty instead of failing the picker.
        archive
            .set_meta("sticker_favorites", "{ not json")
            .expect("damage");
        assert!(archive.sticker_favorites().expect("damaged").is_empty());
    }

    #[test]
    fn a_favourite_follows_its_file_to_its_new_name() {
        let archive = Archive::in_memory().expect("opens");
        let before = std::path::PathBuf::from("/cache/media/5541-3EB0.webp");
        let after = std::path::PathBuf::from("/cache/media/9f2c.webp");
        let other = std::path::PathBuf::from("/state/stickers/frog.webp");
        archive.toggle_sticker_favorite(&before).expect("adds");
        archive.toggle_sticker_favorite(&other).expect("adds");
        archive
            .rename_sticker_favorite(&before, &after)
            .expect("renames");
        assert_eq!(
            archive.sticker_favorites().expect("list"),
            vec![other, after],
            "the favourite follows its file, and the rest is untouched"
        );
        // A favourite that named neither name changes nothing at all.
        let missing = std::path::PathBuf::from("/gone.webp");
        archive
            .rename_sticker_favorite(&missing, &before)
            .expect("renames nothing");
        assert_eq!(archive.sticker_favorites().expect("list").len(), 2);
    }
    fn sticker(chat: &str, id: &str, timestamp: i64, path: Option<&str>, from_me: bool) -> Message {
        Message {
            id: id.into(),
            chat: chat.into(),
            sender: chat.into(),
            sender_name: None,
            from_me,
            timestamp,
            content: Content::Sticker {
                media: Media {
                    mime: "image/webp".into(),
                    size: 10,
                    width: Some(512),
                    height: Some(512),
                    path: path.map(std::path::PathBuf::from),
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
        }
    }

    #[test]
    fn phone_stickers_keep_the_latest_use_and_their_file() {
        let archive = Archive::in_memory().expect("opens");
        archive
            .upsert_phone_sticker("aa", b"one", 100, 0.5)
            .expect("stored");
        archive
            .upsert_phone_sticker("bb", b"two", 300, 0.1)
            .expect("stored");
        // Older repeated use does not lower the last-used time.
        archive
            .upsert_phone_sticker("aa", b"one", 50, 0.9)
            .expect("stored");
        let list = archive.phone_stickers().expect("lists");
        assert_eq!(
            list.iter().map(|s| s.hash.as_str()).collect::<Vec<_>>(),
            ["bb", "aa"]
        );
        assert_eq!(list[1].last_used, 100);
        assert!(list.iter().all(|s| s.path.is_none()));
        archive
            .set_sticker_path("aa", Path::new("/tmp/aa.webp"))
            .expect("filed");
        let list = archive.phone_stickers().expect("lists");
        assert_eq!(list[1].path.as_deref(), Some(Path::new("/tmp/aa.webp")));
    }
    #[test]
    fn a_favorite_intent_survives_a_late_push_ack() {
        let archive = Archive::in_memory().expect("opens");
        assert!(archive.favorite_sticker("aa").expect("reads").is_none());
        archive
            .set_favorite_sticker("aa", true, 10, None, false)
            .expect("stores");
        archive
            .favorite_sticker_pushed("aa", 10, Some(b"refs"))
            .expect("marks");
        let stored = archive.favorite_sticker("aa").expect("reads").expect("row");
        assert!(stored.favorite && stored.pushed);
        assert_eq!(stored.action.as_deref(), Some(&b"refs"[..]));
        archive
            .set_favorite_sticker("aa", false, 20, None, false)
            .expect("stores");
        archive
            .favorite_sticker_pushed("aa", 10, None)
            .expect("marks");
        let stored = archive.favorite_sticker("aa").expect("reads").expect("row");
        assert!(!stored.favorite && !stored.pushed);
        let waiting = archive.unpushed_favorite_stickers().expect("lists");
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].0, "aa");
        assert_eq!(stored.action.as_deref(), Some(&b"refs"[..]));
    }

    #[test]
    fn the_picker_lists_the_stickers_the_user_sent() {
        let archive = Archive::in_memory().expect("opens");
        archive.ensure_chat("a@s.whatsapp.net", "A").expect("chat");
        archive
            .insert_message(
                &sticker("a@s.whatsapp.net", "s1", 10, None, true),
                Some(b"raw"),
            )
            .expect("inserted");
        archive
            .insert_message(
                &sticker("a@s.whatsapp.net", "s2", 20, None, false),
                Some(b"raw"),
            )
            .expect("inserted");
        let missing = archive.stickers_without_file(10).expect("lists");
        assert_eq!(
            missing,
            vec![("a@s.whatsapp.net".to_owned(), "s1".to_owned())]
        );

        // Files on disk decide what the picker can show; a received sticker
        // never enters it, and a missing file keeps a sent one out too.
        let dir = std::env::temp_dir().join(format!("zapfast-stickers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let sent = dir.join("sent.webp");
        let received = dir.join("received.webp");
        std::fs::write(&sent, b"sticker").expect("writes");
        std::fs::write(&received, b"sticker").expect("writes");
        archive
            .insert_message(
                &sticker(
                    "a@s.whatsapp.net",
                    "s3",
                    30,
                    Some(&sent.to_string_lossy()),
                    true,
                ),
                Some(b"raw"),
            )
            .expect("inserted");
        archive
            .insert_message(
                &sticker(
                    "a@s.whatsapp.net",
                    "s4",
                    40,
                    Some(&received.to_string_lossy()),
                    false,
                ),
                Some(b"raw"),
            )
            .expect("inserted");
        let recent = archive.recent_stickers(10).expect("lists");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].path, sent);
        assert_eq!(recent[0].last_used, 30);
        std::fs::remove_dir_all(dir).expect("cleans up");
    }
}

#[cfg(test)]
mod media_path_tests {
    use super::*;
    use crate::model::{Content, Delivery, Media, MediaState};

    fn picture(id: &str) -> Message {
        Message {
            id: id.into(),
            chat: "a@s.whatsapp.net".into(),
            sender: "a@s.whatsapp.net".into(),
            sender_name: None,
            from_me: false,
            timestamp: 1,
            content: Content::Image {
                media: Media {
                    mime: "image/jpeg".into(),
                    size: 10,
                    width: None,
                    height: None,
                    path: None,
                    state: MediaState::Idle,
                },
                caption: None,
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
        }
    }

    #[test]
    fn attachment_paths_can_be_listed_moved_and_forgotten() {
        let archive = Archive::in_memory().expect("opens");
        archive.ensure_chat("a@s.whatsapp.net", "A").expect("chat");
        archive
            .insert_message(&picture("p1"), None)
            .expect("inserted");
        assert!(archive.media_paths().expect("lists").is_empty());
        archive
            .set_media_path("a@s.whatsapp.net", "p1", Path::new("/old/media/p1.jpg"))
            .expect("filed");
        let listed = archive.media_paths().expect("lists");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].2, Path::new("/old/media/p1.jpg"));
        archive
            .clear_media_path("a@s.whatsapp.net", "p1")
            .expect("cleared");
        assert!(archive.media_paths().expect("lists").is_empty());
    }
}

#[cfg(test)]
mod zapext_community_tests {
    use super::*;

    #[test]
    fn community_state_is_independent_from_admin_only_posting() {
        let archive = Archive::in_memory().expect("opens");
        let chat = "1-2@g.us";
        archive.ensure_chat(chat, "Group").expect("chat");
        archive
            .set_group_info(chat, Some("Admin only"), &[], true)
            .expect("group info");
        archive
            .set_group_community(chat, false)
            .expect("community state");
        let row = archive.chat(chat).expect("chat").expect("exists");
        assert!(row.read_only);
        assert!(!row.community);

        archive
            .set_group_community(chat, true)
            .expect("community state");
        let row = archive.chat(chat).expect("chat").expect("exists");
        assert!(row.read_only);
        assert!(row.community);
    }
}
