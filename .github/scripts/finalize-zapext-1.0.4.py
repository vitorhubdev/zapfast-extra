from pathlib import Path
import base64
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[2]
VERSION = "1.0.4"


def replace(path: str, old: str, new: str, count: int = 1) -> None:
    target = ROOT / path
    text = target.read_text(encoding="utf-8")
    actual = text.count(old)
    if actual < count:
        raise SystemExit(
            f"{path}: expected at least {count} occurrence(s), found {actual}: {old[:120]!r}"
        )
    target.write_text(text.replace(old, new, count), encoding="utf-8")


# One easy-to-find source of truth for the ZapExt fork version.
(ROOT / "VERSION").write_text(VERSION, encoding="utf-8")
replace(
    "src/updates.rs",
    'pub const ZAPEXT_VERSION: &str = "1.0.4";',
    'pub const ZAPEXT_VERSION: &str = include_str!("../VERSION");',
)

# Fork maintainer metadata. Internal crate/package name stays zapfast for compatibility.
cargo_path = ROOT / "Cargo.toml"
cargo = cargo_path.read_text(encoding="utf-8")
if "authors =" not in cargo:
    cargo = cargo.replace(
        'version = "0.14.0"\n',
        'version = "0.14.0"\nauthors = ["Vitor (ZapExt fork) <108148783+vitorhubdev@users.noreply.github.com>"]\n',
        1,
    )
cargo = cargo.replace(
    'description = "A native WhatsApp client built with Rust and egui"',
    'description = "ZapExt, a community mod of ZapFast with extra desktop features and fixes"',
)
cargo_path.write_text(cargo, encoding="utf-8")

# Build the exact user-supplied Z+ artwork into every platform icon surface.
source_b64 = ROOT / ".github/scripts/zapext-logo.webp.b64"
source = ROOT / ".zapext-logo-source.webp"
source.write_bytes(base64.b64decode(source_b64.read_text(encoding="utf-8").strip()))
image_tool = shutil.which("magick") or shutil.which("convert")
if not image_tool:
    raise SystemExit("ImageMagick is required to build ZapExt icon assets")


def image(*args: str) -> None:
    subprocess.run([image_tool, *args], check=True)


logo = ROOT / "assets/zapext.png"
logo.parent.mkdir(parents=True, exist_ok=True)
image(str(source), "-resize", "1024x1024", str(logo))
shutil.copyfile(logo, ROOT / "packaging/macos/icon-1024.png")

icon512 = ROOT / ".zapext-icon-512.png"
image(str(source), "-resize", "512x512", str(icon512))
encoded_png = base64.b64encode(icon512.read_bytes()).decode("ascii")
(ROOT / "packaging/icons/zapfast.svg").write_text(
    '<svg xmlns="http://www.w3.org/2000/svg" width="512" height="512" viewBox="0 0 512 512">'
    '<image width="512" height="512" href="data:image/png;base64,'
    + encoded_png
    + '"/></svg>\n',
    encoding="utf-8",
)
image(
    str(source),
    "-background",
    "none",
    "-define",
    "icon:auto-resize=256,128,64,48,32,16",
    str(ROOT / "packaging/windows/zapfast.ico"),
)
(ROOT / "packaging/macos/icon-1024.svg").unlink(missing_ok=True)
source.unlink(missing_ok=True)
icon512.unlink(missing_ok=True)

# Release tags must match VERSION. Tag pushes alone drive releases to avoid duplicate runs.
release_path = ROOT / ".github/workflows/release.yml"
release = release_path.read_text(encoding="utf-8")
release = release.replace(
    'on:\n  push:\n    tags: ["v*"]\n  workflow_dispatch:\n',
    'on:\n  push:\n    tags: ["v*"]\n',
    1,
)
if "Validate ZapExt release version" not in release:
    release = release.replace(
        '      - uses: actions/checkout@v7\n',
        '''      - uses: actions/checkout@v7
      - name: Validate ZapExt release version
        shell: bash
        run: |
          expected="v$(tr -d '\\r\\n' < VERSION)"
          if [ "$GITHUB_REF_NAME" != "$expected" ]; then
            echo "Tag $GITHUB_REF_NAME does not match VERSION ($expected)" >&2
            exit 1
          fi
''',
        1,
    )
release = release.replace(
    '''      - uses: softprops/action-gh-release@v2
        with:
          files: dist/*
          generate_release_notes: true''',
    '''      - uses: softprops/action-gh-release@v2
        with:
          name: ZapExt ${{ github.ref_name }}
          files: dist/*
          generate_release_notes: true''',
    1,
)
release_path.write_text(release, encoding="utf-8")

# README becomes the fork/mod README, with maintainer and upstream credit.
readme_path = ROOT / "README.md"
readme = readme_path.read_text(encoding="utf-8")
if "## What it does" not in readme:
    raise SystemExit("README.md: What it does section not found")
_, body = readme.split("## What it does", 1)
body = body.replace("ZapFast", "ZapExt")
body = body.replace(
    "On macOS with Homebrew: `brew install --cask crmne/tap/zapfast`.\n\n",
    "For ZapExt, download the fork build from GitHub Releases. The upstream Homebrew and AUR recipes belong to the original ZapFast project and are not published by this fork.\n\n",
)
body = body.replace(
    "[releases page](https://github.com/crmne/zapfast/releases)",
    "[ZapExt releases page](https://github.com/vitorhubdev/zapfast-extra/releases)",
)
body = body.replace(
    "| Windows x64 and arm64 | `zapfast-vX.Y.Z-<target>-setup.exe` (no administrator rights needed), or the `.zip` |",
    "| Windows x64 and arm64 | `zapfast-vX.Y.Z-<target>-setup.exe`, `ZapExt-vX.Y.Z-windows-<arch>-portable.exe`, or the portable `.zip` |",
)
body = body.replace(
    '''The macOS release process signs the app with Developer ID, submits the DMG
to Apple's notarization service, and staples and validates its ticket before
publishing.''',
    '''The macOS release process always validates the universal app and its entitlements.
When Apple Developer credentials are configured it also signs with Developer ID,
notarizes the DMG, and validates the stapled ticket. Without those credentials,
the release uses an ad-hoc signature and skips only the Apple notarization checks.''',
)
body = body.replace(
    '''### Updating ZapExt

ZapExt checks GitHub once a day when **Check for updates** is enabled.''',
    '''### Updating ZapExt

ZapExt uses the fork's GitHub Releases API at
`https://api.github.com/repos/vitorhubdev/zapfast-extra/releases/latest`.
It checks once a day when **Check for updates** is enabled. The current fork
version has one canonical source in the repository root: [`VERSION`](VERSION).
Release tags are validated against that file before binaries are built.''',
)
body = body.replace(
    '''installed and portable builds register this identity in the current user's registry.
  On Linux,
  clicking a notification opens the chat,''',
    '''installed and portable builds register this identity in the current user's registry.
  Clicking a Windows notification opens its chat and anchors on the exact
  notified message. On Linux,
  clicking a notification opens the chat,''',
)
body = body.replace("https://zapfast.rocks", "https://github.com/crmne/zapfast")

intro = '''<p align="center">
  <img src="assets/zapext.png" width="220" alt="ZapExt logo">
</p>

# ZapExt

**A community mod of ZapFast, native and fast.**

ZapExt is an independent fork/mod maintained by
[Vitor (`@vitorhubdev`)](https://github.com/vitorhubdev). It is based on the
original [ZapFast](https://github.com/crmne/zapfast) project and keeps its MIT
license and original copyright notices. The fork adds its own desktop fixes,
packaging, updater path, branding, and extra behavior while preserving
compatibility-sensitive internal `zapfast` identifiers where changing them
would break existing installations.

ZapExt is written in Rust with [egui](https://github.com/emilk/egui) and uses
[whatsapp-rust](https://github.com/oxidezap/whatsapp-rust) for the WhatsApp Web
protocol. It runs on Linux, macOS, and Windows and links as a companion device
without embedding a browser engine.

**Downloads:** [latest ZapExt release](https://github.com/vitorhubdev/zapfast-extra/releases/latest)

> ZapExt is not the upstream ZapFast project and is not affiliated with
> WhatsApp or Meta.
'''
readme = intro + "\n## What it does" + body
credits = '''## Credits

ZapExt modifications and fork releases are maintained by
[Vitor (`@vitorhubdev`)](https://github.com/vitorhubdev).

The original project is [ZapFast](https://github.com/crmne/zapfast), created
and developed by its original authors and contributors. ZapExt is a derivative
MIT-licensed mod/fork. The original license and copyright notice remain in
[`LICENSE`](LICENSE).

'''
if "## Credits\n" not in readme:
    readme = readme.replace("## License\n", credits + "## License\n", 1)
readme_path.write_text(readme, encoding="utf-8")

# Package metadata and visible author/maintainer information.
replace(
    "native-packages.yaml",
    "  description: ZapExt, a fast native WhatsApp client",
    "  description: ZapExt, a community mod of ZapFast with extra desktop features and fixes",
)
replace(
    "packaging/flatpak/rocks.zapfast.ZapFast.metainfo.xml",
    "  <summary>A native WhatsApp client</summary>",
    "  <summary>A community mod of ZapFast with extra desktop features</summary>",
)
replace(
    "packaging/flatpak/rocks.zapfast.ZapFast.metainfo.xml",
    "    <p>A native WhatsApp companion for Linux, macOS and Windows, written in Rust",
    "    <p>ZapExt is a community mod of ZapFast for Linux, macOS and Windows, written in Rust",
)
replace(
    "packaging/flatpak/rocks.zapfast.ZapFast.metainfo.xml",
    '''    <release version="1.0.3" date="2026-09-16">
      <url>https://github.com/vitorhubdev/zapfast-extra/releases/tag/v1.0.3</url>
    </release>''',
    '''    <release version="1.0.4" date="2026-09-16">
      <url>https://github.com/vitorhubdev/zapfast-extra/releases/tag/v1.0.4</url>
    </release>''',
)
replace(
    "packaging/macos/Info.plist",
    "<key>NSHumanReadableCopyright</key><string>MIT License. Not affiliated with WhatsApp or Meta.</string>",
    "<key>NSHumanReadableCopyright</key><string>ZapExt fork maintained by vitorhubdev. Based on ZapFast, MIT License. Not affiliated with WhatsApp or Meta.</string>",
)
replace(
    "build.rs",
    '''            .set("ProductName", "ZapExt")
            .set("FileDescription", "ZapExt");''',
    '''            .set("ProductName", "ZapExt")
            .set("FileDescription", "ZapExt")
            .set("CompanyName", "vitorhubdev")
            .set("LegalCopyright", "ZapExt fork by vitorhubdev, based on ZapFast under MIT");''',
)

# Extend the 1.0.4 changelog created by the functional patch.
changelog_path = ROOT / "CHANGELOG.md"
changelog = changelog_path.read_text(encoding="utf-8")
if "new green ZapExt `Z+` artwork" not in changelog:
    changelog = changelog.replace(
        "### Changed\n\n",
        "### Changed\n\n"
        "- Replaced the application icon with the new green ZapExt `Z+` artwork across the runtime window, Windows executable and setup, Linux/Flatpak icon, macOS app/Dock icon, and README.\n"
        "- Added a root `VERSION` file as the single fork-version source; release tags are checked against it before building.\n"
        "- README and package metadata now identify ZapExt as a community mod/fork, credit the original ZapFast project and contributors, and identify `vitorhubdev` as the fork maintainer.\n"
        "- The updater continues to use the fork's `vitorhubdev/zapfast-extra` GitHub Releases API and daily update checks.\n",
        1,
    )
changelog_path.write_text(changelog, encoding="utf-8")

# No staging machinery in the release tag.
source_b64.unlink(missing_ok=True)
(ROOT / ".github/scripts/finalize-zapext-1.0.4.py").unlink(missing_ok=True)
