---
title: Release packaging
description: Shared packaging automation and application-specific release definitions.
nav_order: 20
---

Vespera keeps release asset definitions, nFPM configuration and native AUR
templates in `native-packages.yaml` and `packaging/`. Common automation comes from the pinned
[native-packages](https://github.com/crmne/native-packages) gem, installed with `gem install native-packages --version 0.5.1`.

Stable releases build the existing Linux, macOS and Windows artifacts first.
The shared packaging workflow then verifies published checksums and attaches
Linux DEB/RPM packages and a recipe archive. Automatic AUR publication requires
its configured repository variable and secrets. PRs only validate recipes.

Native bundle contents, signing and installer settings remain application-specific.
See the repository's [maintainer packaging guide](https://github.com/crmne/zapfast/blob/main/PACKAGING.md)
for commands and the shared tool's [platform coverage](https://github.com/crmne/native-packages/blob/main/docs/platforms.md)
for the boundaries.

macOS release builds use `native-packages.macos.yaml`. Complete Apple CI secrets
enable shared signing, notarization and ticket validation automatically before
checksums are written. Earlier downloads retain their original signing status.
