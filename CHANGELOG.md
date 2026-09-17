# Changelog

All notable changes to the ZapExt fork are recorded here.

## [1.0.5] - 2026-09-16

### Fixed

- The application icon is the real ZapExt artwork on every surface: the window, tray, taskbar, Dock, and the in-app logo draw `assets/zapext.png`, and `packaging/icons/zapfast.svg` is now a faithful vector trace of it (gradient background, ribbon Z, speed lines, plus) instead of a simplified flat mark. `scripts/make-icons.py` regenerates all of them from the master logo.
- The chat list's two views are labelled `Chats` and `Channels`.
- Brazilian numbers use the national shape: `+55 75 9 9539 9345` for a mobile (country code, DDD, the mobile 9, then two groups of four) and `+55 75 8351 1141` for a landline, instead of arbitrary groups of three.
- Chats without an address-book name show the profile name their owner chose (`~Name`) instead of the raw number, and a contact update without a name no longer erases a name that is already known.
- The sticker picker offers saved stickers, imported packs, the phone's recent list, and the stickers the user sent. Stickers that merely passed through a chat are no longer listed or downloaded, even when their file is cached.
- A media download that fails for a transient reason (dropped connection, busy server) repeats quietly with backoff (1s, 3s, 8s, 20s) while the bubble keeps its loading state, so the reader sees the picture instead of an error. Expired media (403/404/410, already re-requested once) still reports immediately.
- A picture or sticker whose local file cannot be decoded yet keeps its loading state and retries quietly, instead of showing `Could not display this picture`, and only reports after the attempts are used up.
- Notifications use a grouped `NotificationTarget` (chat, message, opener) so `cargo clippy -D warnings` passes on all platforms; clicking still opens the exact message.
- The `icon_has_clear_corners_and_visible_interior` test passes on Linux, macOS, and Windows, and a mark that cannot be decoded at all still falls back to a plain disc instead of a malformed icon.
- Fork version handling trims `VERSION` everywhere (window title, CLI `--version`, User-Agent, update checks) and tests assert the trimmed value instead of a hardcoded number.
- Self-update verification compares the fork version (`zapext_version()`) instead of `CARGO_PKG_VERSION`, so `1.0.x` releases no longer fail the `wrong version` receipt check.
- User-visible branding now says `ZapExt` (About dialog, Settings, update window, tray, macOS menus, login, empty state, notifications, keyring errors, linked-device name) while internal `zapfast` executable, storage, AppUserModelID, bundle identifier, and Flatpak ID remain unchanged for compatibility.
- macOS updater accepts `ZapExt.app` first and still accepts `ZapFast.app`/`FastsApp.app` for upgrades and rollbacks; staging preserves the downloaded bundle name.
- Linked-device pairing now reports `ZapExt` with the fork version; existing pairings keep their old name until relinking.

### Tests

- Added `brazilian_numbers_follow_the_national_shape`, `the_installed_vector_logo_stays_scalable`, `only_transient_download_failures_are_repeated`, and `the_picker_lists_the_stickers_the_user_sent`; `app_icon_scales_and_falls_back_without_panicking` now asserts the raster artwork is what the window and tray start from and that the vector still rasterizes.
- Added `notification_target_keeps_chat_and_message_together`, expanded `lines` edge cases, `zapext_version_is_clean_and_comparable`, `version_parsing_rejects_bad_input`, `app_icon_scales_and_falls_back_without_panicking`, `search_keys_ignore_case_and_accents`, `phone_digits_and_grouping_cover_edge_cases`, expanded phone-search and portable-filename cases, and expanded macOS bundle-rename coverage.

### Docs

- `README.md` and `docs/_guide/using-zapfast.md` describe the sticker picker's contents (saved stickers, packs, the phone's recent list and what the user sent) and the Brazilian number grouping.
- `scripts/make-icons.py` documents how the icons are built from the master logo.
- `AGENTS.md` now names `VERSION` as the single source of truth (not `ZAPEXT_VERSION` in code).
- `PACKAGING.md` documents the fork release flow (`VERSION` + `vX.Y.Z` tags, ad-hoc vs notarized macOS) and notes upstream AUR/Homebrew are not published by this fork.
- `docs/_config.yml` and `docs/_data/versions.yml` point at the fork (`ZapExt`, `vitorhubdev/zapfast-extra`) instead of upstream.

## [1.0.4] - 2026-09-16

### Fixed

- Clicking a Windows desktop notification now shows ZapExt, opens the originating chat, and anchors the conversation on the exact notified message.
- macOS release verification now distinguishes notarized builds from ad-hoc builds, so repositories without Apple Developer credentials can still publish a verified universal DMG without falsely requiring a stapled notarization ticket.
- The Windows notification identity now displays `ZapExt` while retaining the existing compatibility-sensitive AppUserModelID.

### Packaging

- Windows x64 and ARM64 releases now publish a directly downloadable `*-portable.exe` in addition to the original-compatible portable ZIP and Inno Setup `*-setup.exe`.
- Official `*-portable.exe` downloads are recognized as portable installations for ZapExt self-update detection.

### Compatibility

- Existing `zapfast-v*` ZIP/setup asset naming, executable name, installer AppId, AppUserModelID, and storage identifiers remain intact; the direct portable EXE is additive.

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

- Replaced the application icon with the new green ZapExt `Z+` artwork across the runtime window, Windows executable and setup, Linux/Flatpak icon, macOS app/Dock icon, and README.
- Added a root `VERSION` file as the single fork-version source; release tags are checked against it before building.
- README and package metadata now identify ZapExt as a community mod/fork, credit the original ZapFast project and contributors, and identify `vitorhubdev` as the fork maintainer.
- The updater continues to use the fork's `vitorhubdev/zapfast-extra` GitHub Releases API and daily update checks.
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
