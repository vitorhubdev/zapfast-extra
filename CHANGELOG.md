# Changelog

All notable changes to the ZapExt fork are recorded here.

## [1.0.3] - 2026-09-16

### Fixed

- Windows installer now displays `ZapExt` and points publisher, support, and update links at the fork.
- macOS bundle, DMG volume, microphone permission text, and release verification now use the visible `ZapExt` name.
- Linux desktop and Flatpak metadata now display `ZapExt` and link to `vitorhubdev/zapfast-extra`.
- Native package release/source metadata now reads from the fork instead of `crmne/zapfast`.

### Compatibility

- Internal executable/package names, Windows `AppId`, macOS bundle identifier, Flatpak application id, storage identifiers, and `zapfast-v*` release asset names remain unchanged so existing installations and the updater continue to work.

## [1.0.2] - 2026-09-16

### Fixed

- Archived conversations no longer generate desktop notifications, and any visible notification is cleared as soon as a chat becomes archived.
- Muted chats, groups and channels remain excluded from new notifications; visible notifications are also cleared immediately when mute state arrives or changes.
- Search resolves the best contact display name and normalizes formatted phone numbers, country codes, and DDD input.
- Failed profile-picture lookups retry after a short negative-cache interval; direct contacts also try their known privacy LID identity.

### Changed

- Added separate `Chats & groups` and `Channels & communities` sidebar views.
- Community parent containers are classified explicitly, so ordinary admin-only groups remain in `Chats & groups`.
- Sticker picker prioritizes `My stickers` and keeps synchronized phone history separately under `Recent from phone`.
- Update checks and release-asset validation now point at `vitorhubdev/zapfast-extra`, and downloaded builds are verified through the ZapExt CLI identity.
- Windows executable metadata and Cargo repository links now identify ZapExt/the fork while compatibility-sensitive internal `zapfast` identifiers remain unchanged.

### Known limitation

- The pinned `whatsapp-rust` revision exposes the `FavoriteSticker` app-state schema but no public `FavoriteSticker` event. ZapExt therefore prioritizes stickers explicitly saved in ZapExt and separates phone recents; complete WhatsApp-account favorite sync requires extending the library event integration rather than guessing from general recents.

## [1.0.1] - 2026-09-16

### Changed

- Renamed the visible fork identity from ZapFast to ZapExt where the application identifies itself to the user.
- The application window title now shows the fork version as `ZapExt - 1.0.1`.
- The command-line application identity now uses `zapext` and reports the ZapExt fork version.
- Added mandatory fork agent rules: work directly on `main`, bump the ZapExt version for every completed modification batch, and update this changelog for every version.
- Kept internal `zapfast` crate names, storage paths, app ids, and compatibility identifiers unchanged for now to avoid breaking existing sessions and user data.

### Verification

- Added a test that asserts the visible application title includes `ZapExt - 1.0.1`.
