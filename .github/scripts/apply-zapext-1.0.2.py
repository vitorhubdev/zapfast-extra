from pathlib import Path


def replace(path, old, new, count=1):
    p = Path(path)
    text = p.read_text()
    found = text.count(old)
    if found < count:
        raise SystemExit(f"{path}: expected {count}, found {found}: {old[:100]!r}")
    p.write_text(text.replace(old, new, count))


# Fork identity, version and updater.
replace(
    "src/updates.rs",
    'const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/crmne/zapfast/releases/latest";',
    'pub const ZAPEXT_VERSION: &str = "1.0.2";\nconst LATEST_RELEASE_URL: &str = "https://api.github.com/repos/vitorhubdev/zapfast-extra/releases/latest";',
)
replace(
    "src/updates.rs",
    '.header("User-Agent", concat!("ZapFast/", env!("CARGO_PKG_VERSION")))',
    '.header("User-Agent", format!("ZapExt/{ZAPEXT_VERSION}"))',
)
replace(
    "src/updates.rs",
    'is_newer(&version, env!("CARGO_PKG_VERSION"))',
    'is_newer(&version, ZAPEXT_VERSION)',
)
replace(
    "src/main.rs",
    'const APP_VERSION: &str = "1.0.1";',
    'const APP_VERSION: &str = zapfast::updates::ZAPEXT_VERSION;',
)
replace(
    "src/main.rs",
    'assert_eq!(app_title(false), "ZapExt - 1.0.1");',
    'assert_eq!(APP_VERSION, "1.0.2");\n        assert_eq!(app_title(false), "ZapExt - 1.0.2");',
)
replace(
    "src/updates/transfer.rs",
    'format!("https://api.github.com/repos/crmne/zapfast/releases/tags/v{version}")',
    'format!("https://api.github.com/repos/vitorhubdev/zapfast-extra/releases/tags/v{version}")',
)
replace(
    "src/updates/transfer.rs",
    '.user_agent(concat!("ZapFast/", env!("CARGO_PKG_VERSION")))',
    '.user_agent(format!("ZapExt/{}", super::ZAPEXT_VERSION))',
)
replace(
    "src/updates/transfer.rs",
    '"/crmne/zapfast/releases/download/v{}/{}",',
    '"/vitorhubdev/zapfast-extra/releases/download/v{}/{}",',
)
replace(
    "src/updates/install.rs",
    'version.trim() == format!("zapfast {expected}")',
    'version.trim() == format!("zapext {expected}")',
)
replace("build.rs", '.set("ProductName", "ZapFast")', '.set("ProductName", "ZapExt")')
replace("build.rs", '.set("FileDescription", "ZapFast")', '.set("FileDescription", "ZapExt")')
replace(
    "Cargo.toml",
    'homepage = "https://zapfast.rocks"',
    'homepage = "https://github.com/vitorhubdev/zapfast-extra"',
)
replace(
    "Cargo.toml",
    'repository = "https://github.com/crmne/zapfast"',
    'repository = "https://github.com/vitorhubdev/zapfast-extra"',
)

# Notifications: never create notifications for archived/muted chats, and clear
# any already-visible notification as soon as either state reaches the UI.
replace(
    "src/app.rs",
    'if chat.unread == 0 || chat.muted(now) || now - message.timestamp > 60 {',
    'if chat.unread == 0 || chat.archived || chat.muted(now) || now - message.timestamp > 60 {',
)
replace(
    "src/app.rs",
    '''                    for chat in &chats {
                        if chat.unread == 0 {
                            self.notifications.clear(&chat.id);
                        }
                    }''',
    '''                    let now = crate::util::now();
                    for chat in &chats {
                        if chat.unread == 0 || chat.archived || chat.muted(now) {
                            self.notifications.clear(&chat.id);
                        }
                    }''',
)
replace(
    "src/app.rs",
    '''        if chat.unread == 0 {
            self.notifications.clear(&chat.id);
        }
        if is_open && chat.unread > 0''',
    '''        if chat.unread == 0 || chat.archived || chat.muted(crate::util::now()) {
            self.notifications.clear(&chat.id);
        }
        if is_open && chat.unread > 0''',
)

# Separate channels/communities from ordinary chats/groups when browsing.
replace(
    "src/app.rs",
    "    pub show_archived: bool,\n    pub toasts: Vec<Toast>,",
    "    pub show_archived: bool,\n    /// Show channels and community/announcement containers instead of normal chats.\n    pub show_channels: bool,\n    pub toasts: Vec<Toast>,",
)
replace(
    "src/app.rs",
    "            show_archived: false,\n            toasts: Vec::new(),",
    "            show_archived: false,\n            show_channels: false,\n            toasts: Vec::new(),",
)
replace(
    "src/app.rs",
    ".filter(|chat| chat.archived == self.show_archived || !needle.is_empty())\n            .filter(|chat| {",
    ".filter(|chat| chat.archived == self.show_archived || !needle.is_empty())\n            .filter(|chat| !needle.is_empty() || chat.is_channel_or_community() == self.show_channels)\n            .filter(|chat| {",
)
replace(
    "src/model.rs",
    '''    pub fn muted(&self, now: i64) -> bool {
        matches!(self.muted_until, Some(0)) || self.muted_until.is_some_and(|until| until > now)
    }

    /// Direct-chat phone number as digits.''',
    '''    pub fn muted(&self, now: i64) -> bool {
        matches!(self.muted_until, Some(0)) || self.muted_until.is_some_and(|until| until > now)
    }

    /// Whether this row belongs in the separate channels/communities view.
    pub fn is_channel_or_community(&self) -> bool {
        self.kind == ChatKind::Broadcast || (self.kind == ChatKind::Group && self.read_only)
    }

    /// Direct-chat phone number as digits.''',
)
replace(
    "src/backend/worker.rs",
    "read_only: metadata.is_announcement && !admin,",
    "read_only: metadata.is_parent_group || (metadata.is_announcement && !admin),",
)
replace(
    "src/ui/chats.rs",
    '''    if !app.search.trim().is_empty() {
        results(app, ui);
        return;
    }
    let chats: Vec<Chat> = app.visible_chats().into_iter().cloned().collect();''',
    '''    if !app.search.trim().is_empty() {
        results(app, ui);
        return;
    }
    if !app.show_archived {
        ui.horizontal(|ui| {
            if ui.selectable_label(!app.show_channels, "Chats & groups").clicked() {
                app.show_channels = false;
            }
            if ui
                .selectable_label(app.show_channels, "Channels & communities")
                .clicked()
            {
                app.show_channels = true;
            }
        });
        ui.add_space(6.0);
    }
    let chats: Vec<Chat> = app.visible_chats().into_iter().cloned().collect();''',
)

# Name + phone/DDD search.
replace(
    "src/app.rs",
    '''|| crate::util::search_key(&chat.name).contains(&needle)
                    || chat.phone().is_some_and(|phone| phone.contains(&needle))''',
    '''|| crate::util::search_key(&self.chat_title(chat)).contains(&needle)
                    || chat
                        .phone()
                        .is_some_and(|phone| crate::util::phone_matches(phone, self.search.trim()))''',
)
app = Path("src/app.rs")
text = app.read_text()
old = "is_some_and(|phone| phone.contains(&needle))"
if old not in text:
    raise SystemExit("src/app.rs: contact phone match not found")
app.write_text(
    text.replace(
        old,
        "is_some_and(|phone| crate::util::phone_matches(phone, self.search.trim()))",
        1,
    )
)

util = Path("src/util.rs")
text = util.read_text()
marker = "#[cfg(test)]\nmod tests {"
if marker not in text:
    raise SystemExit("src/util.rs: tests marker not found")
helpers = '''/// Digits from a user-entered phone number.
pub fn phone_digits(text: &str) -> String {
    text.chars().filter(|ch| ch.is_ascii_digit()).collect()
}

/// Match a stored international WhatsApp number against a formatted query.
/// Suffix matching allows DDD + local number without requiring country code.
pub fn phone_matches(phone: &str, query: &str) -> bool {
    let phone = phone_digits(phone);
    let query = phone_digits(query);
    if query.len() < 4 {
        return false;
    }
    phone == query || phone.ends_with(&query) || query.ends_with(&phone)
}

'''
util.write_text(text.replace(marker, helpers + marker, 1))
with util.open("a") as f:
    f.write(
        '''
#[cfg(test)]
mod zapext_phone_search_tests {
    use super::*;

    #[test]
    fn phone_search_accepts_country_code_ddd_and_formatting() {
        let stored = "5575991234567";
        assert!(phone_matches(stored, "+55 (75) 99123-4567"));
        assert!(phone_matches(stored, "(75) 99123-4567"));
        assert!(phone_matches(stored, "75991234567"));
        assert!(!phone_matches(stored, "71991234567"));
        assert!(!phone_matches(stored, "75"));
    }
}
'''
    )

# Avatar recovery: retry negative cache quickly and try the contact privacy LID.
replace(
    "src/backend/worker.rs",
    "const AVATAR_FRESH: Duration = Duration::from_secs(24 * 60 * 60);",
    "const AVATAR_FRESH: Duration = Duration::from_secs(24 * 60 * 60);\nconst AVATAR_MISS_FRESH: Duration = Duration::from_secs(5 * 60);",
)
replace(
    "src/backend/worker.rs",
    '''.is_some_and(|age| age < AVATAR_FRESH)
        {
            let path = (metadata.len() > 0).then_some(path);''',
    '''.is_some_and(|age| {
                    age < if metadata.len() > 0 { AVATAR_FRESH } else { AVATAR_MISS_FRESH }
                })
        {
            let path = (metadata.len() > 0).then_some(path);''',
)
replace(
    "src/backend/worker.rs",
    '''        } else {
            Self::jid_of(&id).into_iter().collect()
        };
        if candidates.is_empty() {''',
    '''        } else {
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
        if candidates.is_empty() {''',
)

# Saved-by-user stickers stay before synchronized phone recents.
replace("src/ui/picker.rs", 'theme::text(ui, "Saved",', 'theme::text(ui, "My stickers",')
replace("src/ui/picker.rs", 'theme::text(ui, "Recent",', 'theme::text(ui, "Recent from phone",')
replace(
    "src/ui/picker.rs",
    '"Recent stickers appear here. Right-click one to save it. To import a pack, paste a signal.art link or open a .wastickers file."',
    '"Your saved stickers appear first. Phone recents stay in a separate section. To import a pack, paste a signal.art link or open a .wastickers file."',
)

# Fork rules and changelog.
replace(
    "AGENTS.md",
    "- The fork version starts at `1.0.1`. `APP_VERSION` in `src/main.rs` is the source\n  of truth for the ZapExt version shown to users.",
    "- The fork version starts at `1.0.1`. `ZAPEXT_VERSION` in `src/updates.rs` is the\n  source of truth for the ZapExt product version; `src/main.rs` must use that value.",
)
changelog = Path("CHANGELOG.md")
text = changelog.read_text()
anchor = "All notable changes to the ZapExt fork are recorded here.\n\n"
if anchor not in text:
    raise SystemExit("CHANGELOG.md: header anchor not found")
entry = '''## [1.0.2] - 2026-09-16

### Fixed

- Archived conversations no longer generate desktop notifications, and any visible notification is cleared as soon as a chat becomes archived.
- Muted chats, groups and channels remain excluded from new notifications; visible notifications are also cleared immediately when mute state arrives or changes.
- Search resolves the best contact display name and normalizes formatted phone numbers, country codes, and DDD input.
- Failed profile-picture lookups retry after a short negative-cache interval; direct contacts also try their known privacy LID identity.

### Changed

- Added separate `Chats & groups` and `Channels & communities` sidebar views.
- Community parent and read-only announcement containers are classified away from ordinary chats.
- Sticker picker prioritizes `My stickers` and keeps synchronized phone history separately under `Recent from phone`.
- Update checks and release-asset validation now point at `vitorhubdev/zapfast-extra`, and downloaded builds are verified through the ZapExt CLI identity.
- Windows executable metadata and Cargo repository links now identify ZapExt/the fork while compatibility-sensitive internal `zapfast` identifiers remain unchanged.

### Known limitation

- The pinned `whatsapp-rust` app-state schema contains `FavoriteSticker`, but its high-level event API does not currently expose account favorite-sticker synchronization. ZapExt therefore prioritizes stickers explicitly saved in ZapExt and separates phone recents; complete WhatsApp-account favorite sync needs an upstream/library integration extension.

'''
changelog.write_text(text.replace(anchor, anchor + entry, 1))
