from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def replace(path: str, old: str, new: str, count: int = 1) -> None:
    target = ROOT / path
    text = target.read_text(encoding="utf-8")
    actual = text.count(old)
    if actual < count:
        raise SystemExit(f"{path}: expected at least {count} occurrence(s), found {actual}: {old[:100]!r}")
    text = text.replace(old, new, count)
    target.write_text(text, encoding="utf-8")


# Fork version and title tests.
replace("src/updates.rs", 'pub const ZAPEXT_VERSION: &str = "1.0.3";', 'pub const ZAPEXT_VERSION: &str = "1.0.4";')
replace(
    "src/main.rs",
    '        assert_eq!(APP_VERSION, "1.0.3");\n        assert_eq!(app_title(false), "ZapExt - 1.0.3");',
    '        assert_eq!(APP_VERSION, "1.0.4");\n        assert_eq!(app_title(false), "ZapExt - 1.0.4");',
)

# Notification click payload now carries the exact message id.
replace(
    "src/app.rs",
    '    /// Chat ids from clicked notifications.\n    notification_opens: std::sync::Arc<std::sync::Mutex<Vec<ChatId>>>,',
    '    /// Chat and message ids from clicked notifications.\n    notification_opens: std::sync::Arc<std::sync::Mutex<Vec<(ChatId, String)>>>,',
)
replace(
    "src/app.rs",
    '''    fn handle_notification_opens(&mut self) {\n        let opened: Vec<ChatId> = std::mem::take(\n            &mut *self\n                .notification_opens\n                .lock()\n                .unwrap_or_else(|p| p.into_inner()),\n        );\n        for chat in opened {\n            self.actions.push(Action::OpenChat(chat));\n            self.actions.push(Action::ShowWindow);\n        }\n    }''',
    '''    fn handle_notification_opens(&mut self) {\n        let opened: Vec<(ChatId, String)> = std::mem::take(\n            &mut *self\n                .notification_opens\n                .lock()\n                .unwrap_or_else(|p| p.into_inner()),\n        );\n        for (chat, message) in opened {\n            self.actions.push(Action::OpenMessage { chat, message });\n            self.actions.push(Action::ShowWindow);\n        }\n    }''',
)
replace(
    "src/app.rs",
    '''        self.notifications.show(\n            title,\n            body,\n            picture,\n            chat_id.to_owned(),\n            std::sync::Arc::clone(&self.notification_opens),\n            move || waker.wake(),\n        );''',
    '''        self.notifications.show(\n            title,\n            body,\n            picture,\n            chat_id.to_owned(),\n            message.id.clone(),\n            std::sync::Arc::clone(&self.notification_opens),\n            move || waker.wake(),\n        );''',
)

# Cross-platform notification plumbing. Linux also gains exact-message opening.
replace(
    "src/notify.rs",
    '''        chat: String,\n        opened: Arc<Mutex<Vec<String>>>,\n        wake: impl Fn() + Send + 'static,''',
    '''        chat: String,\n        message: String,\n        opened: Arc<Mutex<Vec<(String, String)>>>,\n        wake: impl Fn() + Send + 'static,''',
)
replace(
    "src/notify.rs",
    '''                    picture.as_deref(),\n                    chat,\n                    opened,''',
    '''                    picture.as_deref(),\n                    chat,\n                    message,\n                    opened,''',
)
replace(
    "src/notify.rs",
    '''    chat: String,\n    opened: Arc<Mutex<Vec<String>>>,\n    wake: impl Fn() + Send + 'static,\n    mut cancelled: tokio::sync::oneshot::Receiver<()>,\n) {''',
    '''    chat: String,\n    message: String,\n    opened: Arc<Mutex<Vec<(String, String)>>>,\n    wake: impl Fn() + Send + 'static,\n    mut cancelled: tokio::sync::oneshot::Receiver<()>,\n) {''',
)
replace(
    "src/notify.rs",
    '''                            opened.lock().unwrap_or_else(|p| p.into_inner()).push(chat);\n                            wake();''',
    '''                            opened\n                                .lock()\n                                .unwrap_or_else(|p| p.into_inner())\n                                .push((chat, message));\n                            wake();''',
)
replace(
    "src/notify.rs",
    '''#[cfg(target_os = "windows")]\nfn deliver(\n    title: &str,\n    body: &str,\n    picture: Option<&std::path::Path>,\n    _chat: String,\n    _opened: Arc<Mutex<Vec<String>>>,\n    _wake: impl Fn() + Send + 'static,\n    mut cancelled: tokio::sync::oneshot::Receiver<()>,\n) {\n    if matches!(\n        cancelled.try_recv(),\n        Err(tokio::sync::oneshot::error::TryRecvError::Empty)\n    ) && let Err(error) = windows::show(title, body, picture)\n    {\n        log::debug!("no Windows notification: {error}");\n    }\n}''',
    '''#[cfg(target_os = "windows")]\nfn deliver(\n    title: &str,\n    body: &str,\n    picture: Option<&std::path::Path>,\n    chat: String,\n    message: String,\n    opened: Arc<Mutex<Vec<(String, String)>>>,\n    wake: impl Fn() + Send + 'static,\n    mut cancelled: tokio::sync::oneshot::Receiver<()>,\n) {\n    if !matches!(\n        cancelled.try_recv(),\n        Err(tokio::sync::oneshot::error::TryRecvError::Empty)\n    ) {\n        return;\n    }\n    let activated = move || {\n        opened\n            .lock()\n            .unwrap_or_else(|p| p.into_inner())\n            .push((chat.clone(), message.clone()));\n        wake();\n    };\n    if let Err(error) = windows::show(title, body, picture, activated) {\n        log::debug!("no Windows notification: {error}");\n    }\n}''',
)
replace(
    "src/notify.rs",
    '''    _chat: String,\n    _opened: Arc<Mutex<Vec<String>>>,\n    _wake: impl Fn() + Send + 'static,''',
    '''    _chat: String,\n    _message: String,\n    _opened: Arc<Mutex<Vec<(String, String)>>>,\n    _wake: impl Fn() + Send + 'static,''',
)
replace(
    "src/notify.rs",
    '''            "test".into(),\n            Default::default(),''',
    '''            "test".into(),\n            "test-message".into(),\n            Default::default(),''',
)

# Windows WinRT toast activation callback and visible identity.
replace("src/notify/windows.rs", '    let value = wide("ZapFast");', '    let value = wide("ZapExt");')
replace(
    "src/notify/windows.rs",
    '''pub(super) fn show(title: &str, body: &str, picture: Option<&Path>) -> anyhow::Result<()> {\n    static REGISTERED: OnceLock<Result<(), String>> = OnceLock::new();\n    if let Err(error) = REGISTERED.get_or_init(|| register_identity().map_err(|e| e.to_string())) {\n        anyhow::bail!("notification identity unavailable: {error}");\n    }\n    notification(title, body, picture).show()?;\n    Ok(())\n}''',
    '''pub(super) fn show(\n    title: &str,\n    body: &str,\n    picture: Option<&Path>,\n    mut activated: impl FnMut() + Send + 'static,\n) -> anyhow::Result<()> {\n    static REGISTERED: OnceLock<Result<(), String>> = OnceLock::new();\n    if let Err(error) = REGISTERED.get_or_init(|| register_identity().map_err(|e| e.to_string())) {\n        anyhow::bail!("notification identity unavailable: {error}");\n    }\n    notification(title, body, picture)\n        .on_activated(move |_| {\n            activated();\n            Ok(())\n        })\n        .show()?;\n    Ok(())\n}''',
)

# Recognize the official single-file Windows portable download as update-enabled.
replace(
    "src/updates/install.rs",
    '''#[cfg(not(target_os = "macos"))]\nconst MARKER: &str = "zapfast-portable-v1";''',
    '''#[cfg(not(target_os = "macos"))]\nconst MARKER: &str = "zapfast-portable-v1";\n\n#[cfg(not(target_os = "macos"))]\nfn official_portable_filename(path: &Path) -> bool {\n    path.file_name()\n        .and_then(|name| name.to_str())\n        .is_some_and(|name| name.to_ascii_lowercase().ends_with("-portable.exe"))\n}''',
)
replace(
    "src/updates/install.rs",
    '''        ensure!(\n            fs::read_to_string(directory.join("zapfast-portable.txt"))\n                .is_ok_and(|value| value.trim() == MARKER),\n            "This installation does not identify itself as a portable download. Use the download page to install an update-enabled build."\n        );''',
    '''        ensure!(\n            fs::read_to_string(directory.join("zapfast-portable.txt"))\n                .is_ok_and(|value| value.trim() == MARKER)\n                || official_portable_filename(executable),\n            "This installation does not identify itself as an official portable download. Use the download page to install an update-enabled build."\n        );''',
)
replace(
    "src/updates/install.rs",
    '''    #[test]\n    fn unknown_and_package_managed_paths_are_not_portable() {''',
    '''    #[cfg(not(target_os = "macos"))]\n    #[test]\n    fn official_portable_executable_name_is_recognized() {\n        assert!(official_portable_filename(Path::new(\n            "ZapExt-v1.0.4-windows-x64-portable.exe"\n        )));\n        assert!(official_portable_filename(Path::new(\n            "zapext-v1.0.4-windows-arm64-portable.EXE"\n        )));\n        assert!(!official_portable_filename(Path::new("zapfast.exe")));\n    }\n\n    #[test]\n    fn unknown_and_package_managed_paths_are_not_portable() {''',
)

# Release artifacts: setup, original-compatible ZIP, plus direct portable EXE.
replace(
    ".github/workflows/release.yml",
    '''          if [ "${{ runner.os }}" = "Windows" ]; then\n            cp "target/${{ matrix.target }}/release/zapfast.exe" "dist/$name/"\n          else''',
    '''          if [ "${{ runner.os }}" = "Windows" ]; then\n            cp "target/${{ matrix.target }}/release/zapfast.exe" "dist/$name/"\n            case "${{ matrix.target }}" in\n              x86_64-*) portable_arch="x64" ;;\n              aarch64-*) portable_arch="arm64" ;;\n              *) portable_arch="${{ matrix.target }}" ;;\n            esac\n            cp "target/${{ matrix.target }}/release/zapfast.exe" \\\n              "dist/ZapExt-${GITHUB_REF_NAME}-windows-${portable_arch}-portable.exe"\n          else''',
)
replace(
    ".github/workflows/release.yml",
    '''            dist/*.zip\n            dist/*-setup.exe''',
    '''            dist/*.zip\n            dist/*-setup.exe\n            dist/*-portable.exe''',
)
replace(
    ".github/workflows/release.yml",
    '''      - name: Verify the notarized release app\n        run: bash packaging/macos/verify.sh "dist/zapfast-${GITHUB_REF_NAME}-macos-universal.dmg"''',
    '''      - name: Verify the macOS release app\n        run: |\n          mode=adhoc\n          if [[ -n "${APPLE_CERTIFICATE_P12:-}" \\\n             && -n "${APPLE_CERTIFICATE_PASSWORD:-}" \\\n             && -n "${APPLE_SIGNING_IDENTITY:-}" \\\n             && -n "${APPLE_ID:-}" \\\n             && -n "${APPLE_TEAM_ID:-}" \\\n             && -n "${APPLE_APP_PASSWORD:-}" ]]; then\n            mode=notarized\n          fi\n          bash packaging/macos/verify.sh \\\n            "dist/zapfast-${GITHUB_REF_NAME}-macos-universal.dmg" "$mode"''',
)

# macOS: only Gatekeeper/stapler assertions require real Apple notarization.
verify = ROOT / "packaging/macos/verify.sh"
verify.write_text('''#!/bin/bash\n# Verify the app users receive inside a release DMG. Notarized builds get\n# Gatekeeper/stapler checks; ad-hoc builds still get signature, architecture,\n# microphone metadata, and entitlement validation.\nset -euo pipefail\n\ndmg="$1"\nmode="${2:-notarized}"\ncase "$mode" in\n    notarized|adhoc) ;;\n    *) echo "unknown verification mode: $mode" >&2; exit 2 ;;\nesac\n\ntemporary="$(mktemp -d)"\nmount="$temporary/mount"\nmkdir "$mount"\ncleanup() {\n    hdiutil detach "$mount" >/dev/null 2>&1 || true\n    rm -f "$temporary/entitlements.plist"\n    rmdir "$mount" "$temporary" 2>/dev/null || true\n}\ntrap cleanup EXIT\n\nif [ "$mode" = notarized ]; then\n    xcrun stapler validate "$dmg"\nfi\nhdiutil attach "$dmg" -readonly -nobrowse -mountpoint "$mount" >/dev/null\napp="$mount/ZapExt.app"\ncodesign --verify --strict --deep "$app"\nif [ "$mode" = notarized ]; then\n    spctl --assess --type execute --verbose=2 "$app"\nfi\nlipo "$app/Contents/MacOS/zapfast" -verify_arch x86_64 arm64\ncodesign --display --entitlements - --xml "$app" > "$temporary/entitlements.plist"\n\npython3 - "$app/Contents/Info.plist" "$temporary/entitlements.plist" <<'PY'\nimport plistlib\nimport sys\n\nwith open(sys.argv[1], "rb") as source:\n    info = plistlib.load(source)\nwith open(sys.argv[2], "rb") as source:\n    entitlements = plistlib.load(source)\nif not info.get("NSMicrophoneUsageDescription", "").strip():\n    sys.exit("The release app is missing its microphone permission description")\nif entitlements.get("com.apple.security.device.audio-input") is not True:\n    sys.exit("The signed release app is missing its audio-input entitlement")\nprint("Verified microphone permission metadata in the signed universal app")\nPY\n''', encoding="utf-8")

# Changelog.
changelog = ROOT / "CHANGELOG.md"
text = changelog.read_text(encoding="utf-8")
needle = "All notable changes to the ZapExt fork are recorded here.\n\n"
entry = '''## [1.0.4] - 2026-09-16\n\n### Fixed\n\n- Clicking a Windows desktop notification now shows ZapExt, opens the originating chat, and anchors the conversation on the exact notified message.\n- macOS release verification now distinguishes notarized builds from ad-hoc builds, so repositories without Apple Developer credentials can still publish a verified universal DMG without falsely requiring a stapled notarization ticket.\n- The Windows notification identity now displays `ZapExt` while retaining the existing compatibility-sensitive AppUserModelID.\n\n### Packaging\n\n- Windows x64 and ARM64 releases now publish a directly downloadable `*-portable.exe` in addition to the original-compatible portable ZIP and Inno Setup `*-setup.exe`.\n- Official `*-portable.exe` downloads are recognized as portable installations for ZapExt self-update detection.\n\n### Compatibility\n\n- Existing `zapfast-v*` ZIP/setup asset naming, executable name, installer AppId, AppUserModelID, and storage identifiers remain intact; the direct portable EXE is additive.\n\n'''
if entry not in text:
    if needle not in text:
        raise SystemExit("CHANGELOG.md header not found")
    text = text.replace(needle, needle + entry, 1)
    changelog.write_text(text, encoding="utf-8")

# Remove one-shot patch machinery from the final commit.
for transient in [
    ROOT / ".github/scripts/apply-zapext-1.0.4.py",
    ROOT / ".github/workflows/zapext-1.0.4-apply.yml",
]:
    transient.unlink(missing_ok=True)
