---
title: Settings & Files
description: Settings and paths for the archive, configuration, caches, and logs.
nav_order: 0
---

## File locations

Vespera follows each platform's conventions. On Linux:

| What | Where | Safe to delete? |
| --- | --- | --- |
| Settings | `~/.config/vespera/settings.json` | Yes, you lose preferences |
| Message archive | `~/.local/state/vespera/archive.db` | Yes; only history available from WhatsApp can be restored |
| Session keys | `~/.local/state/vespera/session.db` | Yes; you must link again |
| Attachments | `~/.cache/vespera/media/` | Yes; available files download again when viewed |
| Profile pictures | `~/.cache/vespera/avatars/` | Always |
| Stickers | `~/.cache/vespera/stickers/` | Always |
| Last run's log | `~/.local/state/vespera/vespera.log` | Always |
| Crash log | `~/.local/state/vespera/panic.log` | Always |

Back up the archive if you need its history. WhatsApp sends only recent
history to a new device, although Vespera can request some older messages from
the phone. Clearing the media cache makes Vespera download attachments again.
Expired attachments may still be available through the phone.

On macOS, settings, state, and the logs are in
`~/Library/Application Support/me.paolino.vespera` and the caches in
`~/Library/Caches/me.paolino.vespera`. On Windows, settings are in
`%APPDATA%\paolino\vespera\config`, state and the logs in
`%LOCALAPPDATA%\paolino\vespera\data`, and the caches in
`%LOCALAPPDATA%\paolino\vespera\cache`.

On first start, Vespera moves the corresponding `vespera` directories (or
`vespera` from earlier versions), including the session, archive, saved
stickers, and window state. Existing Vespera directories are never overwritten.
Quit Vespera first; launching Vespera while it is running brings the existing
window forward.

## Settings

Changes on the Settings page are saved to `settings.json` immediately:

- **Theme**: light, dark, or follow the system.
- **Enter sends**: swap Enter and Shift+Enter.
- **Download attachments automatically**: download files up to 64 MB when
  they enter view, or only when clicked.
- **Show sender pictures**: avatars next to group messages.
- **Names from your address book**: use contact names everywhere. When off,
  prefer public profile names.
- **Send read receipts**: the blue ticks others see.
- **Keep running in the background**: keep Vespera in the tray when the
  window closes.
- **Notifications**: use desktop notifications with the chat picture.
- **Check for updates**: ask GitHub once a day whether a newer release exists.

## The log

Each run replaces `vespera.log` and records warnings and errors. Include the
end of this file when reporting an issue.
