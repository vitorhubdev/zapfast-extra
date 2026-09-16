from pathlib import Path


def replace(path, old, new, count=1):
    p = Path(path)
    text = p.read_text()
    found = text.count(old)
    if found < count:
        raise SystemExit(f"{path}: expected {count}, found {found}: {old[:120]!r}")
    p.write_text(text.replace(old, new, count))


# Community classification must be explicit. A normal group can be read-only
# simply because only admins may post, which does not make it a Community.
replace(
    "src/model.rs",
    "    /// Whether this is an announcement group where we cannot post.\n    pub read_only: bool,\n    /// Disappearing-message duration in seconds, if enabled.",
    "    /// Whether this is an announcement group where we cannot post.\n    pub read_only: bool,\n    /// Whether this group is the parent container of a WhatsApp Community.\n    pub community: bool,\n    /// Disappearing-message duration in seconds, if enabled.",
)
replace(
    "src/model.rs",
    "            read_only: false,\n            ephemeral_expiration: None,",
    "            read_only: false,\n            community: false,\n            ephemeral_expiration: None,",
)
replace(
    "src/model.rs",
    '''    /// Whether this row belongs in the separate channels/communities view.
    pub fn is_channel_or_community(&self) -> bool {
        self.kind == ChatKind::Broadcast || (self.kind == ChatKind::Group && self.read_only)
    }''',
    '''    /// Whether this row belongs in the separate channels/communities view.
    pub fn is_channel_or_community(&self) -> bool {
        self.id.ends_with("@newsletter") || self.community
    }''',
)

# Persist Community identity independently from posting permission.
replace(
    "src/archive.rs",
    '''const CHAT_COLUMNS: &str =
    "c.id, c.name, c.kind, c.last_activity, c.unread, c.archived, c.pinned, c.muted_until,
                    m.from_me, m.sender_name, m.content, m.status, m.sender, c.participants, c.read_only,
                    c.pinned_at, c.ephemeral_expiration";''',
    '''const CHAT_COLUMNS: &str =
    "c.id, c.name, c.kind, c.last_activity, c.unread, c.archived, c.pinned, c.muted_until,
                    m.from_me, m.sender_name, m.content, m.status, m.sender, c.participants, c.read_only,
                    c.pinned_at, c.ephemeral_expiration, c.community";''',
)
replace(
    "src/archive.rs",
    '    ("chats", "read_only", "INTEGER NOT NULL DEFAULT 0"),',
    '    ("chats", "read_only", "INTEGER NOT NULL DEFAULT 0"),\n    ("chats", "community", "INTEGER NOT NULL DEFAULT 0"),',
)
replace(
    "src/archive.rs",
    '''        participants: serde_json::from_str(&participants).unwrap_or_default(),
        read_only: row.get(14)?,
        ephemeral_expiration: row
            .get::<_, Option<u32>>(16)?''',
    '''        participants: serde_json::from_str(&participants).unwrap_or_default(),
        read_only: row.get(14)?,
        community: row.get(17)?,
        ephemeral_expiration: row
            .get::<_, Option<u32>>(16)?''',
)
replace(
    "src/archive.rs",
    '''        Ok(())
    }

    pub fn rename_chat(&self, id: &str, name: &str) -> Result<()> {''',
    '''        Ok(())
    }

    /// Persists whether a group is the parent container of a Community.
    pub fn set_group_community(&self, id: &str, community: bool) -> Result<()> {
        self.connection.execute(
            "UPDATE chats SET community = ?2 WHERE id = ?1",
            params![id, community],
        )?;
        Ok(())
    }

    pub fn rename_chat(&self, id: &str, name: &str) -> Result<()> {''',
)
with Path("src/archive.rs").open("a") as f:
    f.write(
        '''

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
'''
    )

# Carry the explicit metadata from whatsapp-rust to the persistent Chat model.
replace(
    "src/backend.rs",
    '''        participants: Vec<String>,
        read_only: bool,
        ephemeral_expiration: Option<u32>,''',
    '''        participants: Vec<String>,
        read_only: bool,
        community: bool,
        ephemeral_expiration: Option<u32>,''',
)
replace(
    "src/backend/worker.rs",
    "read_only: metadata.is_parent_group || (metadata.is_announcement && !admin),",
    "read_only: metadata.is_announcement && !admin,\n                        community: metadata.is_parent_group,",
)
replace(
    "src/backend/worker.rs",
    '''                participants,
                read_only,
                ephemeral_expiration,''',
    '''                participants,
                read_only,
                community,
                ephemeral_expiration,''',
)
replace(
    "src/backend/worker.rs",
    '''                let _ =
                    self.archive
                        .set_group_info(&chat, name.as_deref(), &participants, read_only);
                if let Some(expiration) = ephemeral_expiration {''',
    '''                let _ =
                    self.archive
                        .set_group_info(&chat, name.as_deref(), &participants, read_only);
                let _ = self.archive.set_group_community(&chat, community);
                if let Some(expiration) = ephemeral_expiration {''',
)

# The demo tour clicks the first user-saved sticker. Keep its target aligned
# with the renamed UI section.
replace(
    "src/demo/tour.rs",
    'Target::Sticker => self.labels.get("Saved").map(|pos| *pos + vec2(25.0, 52.0)),',
    'Target::Sticker => self\n                .labels\n                .get("My stickers")\n                .map(|pos| *pos + vec2(25.0, 52.0)),',
)

# Make the release notes precise about Community classification and the
# favorite-sticker protocol limitation in the exact pinned library revision.
replace(
    "CHANGELOG.md",
    "- Community parent and read-only announcement containers are classified away from ordinary chats.",
    "- Community parent containers are classified explicitly, so ordinary admin-only groups remain in `Chats & groups`.",
)
replace(
    "CHANGELOG.md",
    "- The pinned `whatsapp-rust` app-state schema contains `FavoriteSticker`, but its high-level event API does not currently expose account favorite-sticker synchronization. ZapExt therefore prioritizes stickers explicitly saved in ZapExt and separates phone recents; complete WhatsApp-account favorite sync needs an upstream/library integration extension.",
    "- The pinned `whatsapp-rust` revision exposes the `FavoriteSticker` app-state schema but no public `FavoriteSticker` event. ZapExt therefore prioritizes stickers explicitly saved in ZapExt and separates phone recents; complete WhatsApp-account favorite sync requires extending the library event integration rather than guessing from general recents.",
)
