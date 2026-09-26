---
title: Download
description: Get Vespera for Linux, macOS, or Windows, with install instructions for each.
nav_order: 1
---

{% assign v = site.vespera_version %}
{% assign name = site.release_asset_prefix %}
{% assign app = site.release_app_name %}
{% assign base = "https://github.com/vitorhubdev/Vespera/releases/download/v" | append: v %}

Vespera was previously called Vespera. Version 0.13.0 introduces the new
package and executable names. Your existing session and local data move
automatically when you first start Vespera; quit Vespera before upgrading.

The current version is **v{{ v }}**. SHA-256 checksums are in
[checksums.txt]({{ base }}/checksums.txt). Older versions are on the
[releases page](https://github.com/vitorhubdev/Vespera/releases).

## Linux

On Arch Linux and derivatives, install from the
[AUR](https://aur.archlinux.org/packages/{{ name }}-bin):

```sh
paru -S {{ name }}-bin   # prebuilt
paru -S {{ name }}       # builds from the release source
paru -S {{ name }}-git   # builds from the latest commit
```

For other distributions, download a tarball with the binary, desktop file,
and icon:

- [{{ name }}-v{{ v }}-x86_64-unknown-linux-gnu.tar.gz]({{ base }}/{{ name }}-v{{ v }}-x86_64-unknown-linux-gnu.tar.gz)
- [{{ name }}-v{{ v }}-aarch64-unknown-linux-gnu.tar.gz]({{ base }}/{{ name }}-v{{ v }}-aarch64-unknown-linux-gnu.tar.gz)

{{ app }} needs the standard egui libraries and ALSA:
`libglvnd`, `libxkbcommon`, `wayland`, `libx11`, and `alsa-lib` (on
Debian or Ubuntu: `libasound2`, `libgl1`, `libxkbcommon0`, `libwayland-client0`).
For color emoji, install `noto-fonts-emoji` (`fonts-noto-color-emoji` on
Debian). The file picker uses `xdg-desktop-portal`.

## macOS

There is no disk image in this release. The build runner could not create it,
so the macOS job is off until that is sorted out, and no file is offered here
that does not exist. Images from earlier releases are on the
[releases page](https://github.com/vitorhubdev/Vespera/releases).

## Windows

The installer adds {{ app }} to the Start menu and needs no administrator
rights. Choose x86_64 for most PCs or aarch64 for Windows on ARM:

- [{{ name }}-v{{ v }}-x86_64-pc-windows-msvc-setup.exe]({{ base }}/{{ name }}-v{{ v }}-x86_64-pc-windows-msvc-setup.exe)
- [{{ name }}-v{{ v }}-aarch64-pc-windows-msvc-setup.exe]({{ base }}/{{ name }}-v{{ v }}-aarch64-pc-windows-msvc-setup.exe)

To run {{ app }} without installing it, download a zip, extract it, and run
`{{ name }}.exe`.

- [{{ name }}-v{{ v }}-x86_64-pc-windows-msvc.zip]({{ base }}/{{ name }}-v{{ v }}-x86_64-pc-windows-msvc.zip)
- [{{ name }}-v{{ v }}-aarch64-pc-windows-msvc.zip]({{ base }}/{{ name }}-v{{ v }}-aarch64-pc-windows-msvc.zip)

SmartScreen may warn about an unknown publisher on first run. Choose **More
info**, then **Run anyway**.
