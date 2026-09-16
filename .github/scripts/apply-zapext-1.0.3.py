from pathlib import Path


def replace(path: str, old: str, new: str, count: int = 1) -> None:
    file = Path(path)
    text = file.read_text()
    found = text.count(old)
    if found < count:
        raise SystemExit(f"{path}: expected at least {count} occurrence(s), found {found}: {old!r}")
    file.write_text(text.replace(old, new, count))


# ZapExt product version. Cargo/internal zapfast version remains compatibility-sensitive.
replace("src/updates.rs", 'pub const ZAPEXT_VERSION: &str = "1.0.2";', 'pub const ZAPEXT_VERSION: &str = "1.0.3";')
replace("src/main.rs", 'assert_eq!(APP_VERSION, "1.0.2");', 'assert_eq!(APP_VERSION, "1.0.3");')
replace("src/main.rs", 'assert_eq!(app_title(false), "ZapExt - 1.0.2");', 'assert_eq!(app_title(false), "ZapExt - 1.0.3");')

# Windows: visible installer identity and fork links. Keep AppId, executable,
# AppUserModelID, marker names, and release asset names unchanged for upgrades.
replace("packaging/windows/zapfast.iss", '#define AppName "ZapFast"', '#define AppName "ZapExt"')
replace("packaging/windows/zapfast.iss", 'AppPublisher=Carmine Paolino', 'AppPublisher=vitorhubdev')
replace("packaging/windows/zapfast.iss", 'AppPublisherURL=https://zapfast.rocks', 'AppPublisherURL=https://github.com/vitorhubdev/zapfast-extra')
replace("packaging/windows/zapfast.iss", 'AppSupportURL=https://github.com/crmne/zapfast/issues', 'AppSupportURL=https://github.com/vitorhubdev/zapfast-extra/issues')
replace("packaging/windows/zapfast.iss", 'AppUpdatesURL=https://github.com/crmne/zapfast/releases', 'AppUpdatesURL=https://github.com/vitorhubdev/zapfast-extra/releases')

# macOS: visible bundle/DMG identity. Keep the bundle id, executable, and icon
# resource names stable for compatibility.
replace("packaging/macos/Info.plist", '<key>CFBundleName</key><string>ZapFast</string>', '<key>CFBundleName</key><string>ZapExt</string>')
replace("packaging/macos/Info.plist", '<key>CFBundleDisplayName</key><string>ZapFast</string>', '<key>CFBundleDisplayName</key><string>ZapExt</string>')
replace("packaging/macos/Info.plist", 'ZapFast uses the microphone', 'ZapExt uses the microphone')
replace("packaging/macos/bundle.sh", '# Build ZapFast.app from a GUI binary on macOS.', '# Build ZapExt.app from a GUI binary on macOS.')
replace("packaging/macos/verify.sh", 'app="$mount/ZapFast.app"', 'app="$mount/ZapExt.app"')
replace("packaging/macos/dmg.rb", '"-volname", "ZapFast",', '"-volname", "ZapExt",')
replace(".github/workflows/release.yml", 'dist/macos-input/ZapFast.app', 'dist/macos-input/ZapExt.app')

# Linux desktop/Flatpak: visible name and fork metadata. Keep desktop filename,
# Flatpak application id, executable, icon names, and package names unchanged.
replace("packaging/applications/zapfast.desktop", 'Name=ZapFast', 'Name=ZapExt')
replace("packaging/flatpak/rocks.zapfast.ZapFast.metainfo.xml", '<name>ZapFast</name>', '<name>ZapExt</name>')
replace("packaging/flatpak/rocks.zapfast.ZapFast.metainfo.xml", '<developer id="me.paolino">\n    <name>Carmine Paolino</name>\n  </developer>', '<developer id="io.github.vitorhubdev">\n    <name>vitorhubdev</name>\n  </developer>')
replace("packaging/flatpak/rocks.zapfast.ZapFast.metainfo.xml", 'running in the system tray. ZapFast is an independent project', 'running in the system tray. ZapExt is an independent project')
replace("packaging/flatpak/rocks.zapfast.ZapFast.metainfo.xml", '<url type="homepage">https://zapfast.rocks/</url>', '<url type="homepage">https://github.com/vitorhubdev/zapfast-extra</url>')
replace("packaging/flatpak/rocks.zapfast.ZapFast.metainfo.xml", '<url type="bugtracker">https://github.com/crmne/zapfast/issues</url>', '<url type="bugtracker">https://github.com/vitorhubdev/zapfast-extra/issues</url>')
replace("packaging/flatpak/rocks.zapfast.ZapFast.metainfo.xml", '<url type="help">https://zapfast.rocks/getting-started/</url>', '<url type="help">https://github.com/vitorhubdev/zapfast-extra#readme</url>')
replace("packaging/flatpak/rocks.zapfast.ZapFast.metainfo.xml", '<url type="vcs-browser">https://github.com/crmne/zapfast</url>', '<url type="vcs-browser">https://github.com/vitorhubdev/zapfast-extra</url>')
replace("packaging/flatpak/rocks.zapfast.ZapFast.metainfo.xml", 'https://raw.githubusercontent.com/crmne/zapfast/main/docs/screenshot.png', 'https://raw.githubusercontent.com/vitorhubdev/zapfast-extra/main/docs/screenshot.png')
replace("packaging/flatpak/rocks.zapfast.ZapFast.metainfo.xml", '<release version="0.13.1" date="2026-09-14">\n      <url>https://github.com/crmne/zapfast/releases/tag/v0.13.1</url>\n    </release>', '<release version="1.0.3" date="2026-09-16">\n      <url>https://github.com/vitorhubdev/zapfast-extra/releases/tag/v1.0.3</url>\n    </release>')

# Native packages: keep package/file names zapfast for compatibility, but fetch
# source/release data from this fork and identify the fork maintainer.
replace("native-packages.yaml", 'maintainer: Carmine Paolino <carmine@paolino.me>', 'maintainer: vitorhubdev <108148783+vitorhubdev@users.noreply.github.com>')
replace("native-packages.yaml", 'homepage: https://zapfast.rocks', 'homepage: https://github.com/vitorhubdev/zapfast-extra')
replace("native-packages.yaml", 'description: Fast native WhatsApp client', 'description: ZapExt, a fast native WhatsApp client')
replace("native-packages.yaml", 'release:\n  repository: crmne/zapfast', 'release:\n  repository: vitorhubdev/zapfast-extra')
replace("native-packages.yaml", 'url: https://github.com/crmne/zapfast/archive/refs/tags/v@VERSION@.tar.gz', 'url: https://github.com/vitorhubdev/zapfast-extra/archive/refs/tags/v@VERSION@.tar.gz')

# Changelog entry for this completed packaging/identity batch.
changelog = Path("CHANGELOG.md")
text = changelog.read_text()
anchor = "All notable changes to the ZapExt fork are recorded here.\n\n"
if anchor not in text:
    raise SystemExit("CHANGELOG.md header anchor not found")
entry = '''## [1.0.3] - 2026-09-16\n\n### Fixed\n\n- Windows installer now displays `ZapExt` and points publisher, support, and update links at the fork.\n- macOS bundle, DMG volume, microphone permission text, and release verification now use the visible `ZapExt` name.\n- Linux desktop and Flatpak metadata now display `ZapExt` and link to `vitorhubdev/zapfast-extra`.\n- Native package release/source metadata now reads from the fork instead of `crmne/zapfast`.\n\n### Compatibility\n\n- Internal executable/package names, Windows `AppId`, macOS bundle identifier, Flatpak application id, storage identifiers, and `zapfast-v*` release asset names remain unchanged so existing installations and the updater continue to work.\n\n'''
changelog.write_text(text.replace(anchor, anchor + entry, 1))
