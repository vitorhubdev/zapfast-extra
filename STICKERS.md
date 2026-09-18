# Stickers, media, and documents: what we looked at

ZapExt is a small native client with no browser engine, so the sticker
picker, the attachment cache, and the document viewer were built against the
experiments of clients that came before. This file records those references,
what was taken from each, and which decisions are local to this fork, so a
later change can tell a borrowed idea from one that only looks like a
standard.

## Signal Desktop

TypeScript and Electron, and the closest thing to a specification for a
sticker store outside WhatsApp itself.

- `sticker_packs` holds `id`, `key`, `title`, `coverStickerId`, `lastUsed`,
  `position`, `status`, `downloadAttempts`, and `stickerCount`.
- `stickers` is keyed by `(id, packId)` and holds `path`, `width`, `height`,
  `size`, `lastUsed`, and `isCoverOnly`, with
  `CREATE INDEX stickers_recents ON stickers (lastUsed) WHERE lastUsed IS NOT NULL`
  for the recents row.
- Issue 7064 (the scrollable picker) recorded the problem this fork had as
  well: the grid loaded the full-quality files and was slow, and the
  suggested fix was small compressed thumbnails plus row virtualisation.

Taken: one small static preview per sticker, built once and drawn by the
grid, and the ordering of favourites and recents by last use. Not taken:
their SQL shape. Packs here are directories of `<hash>.webp` files with a
`pack.json` manifest (`adopt_pack` in `src/stickers.rs`), so an imported pack
stays readable, movable, and deletable as plain files.

Evidence: `signalapp/Signal-Desktop` `DATABASE_SCHEMA.md` (tables
`sticker_packs` and `stickers`), and issue 7064 for the picker work.

## WhatsApp Web and WhatsApp Desktop

The official clients are the only source for what the server expects.

- A sticker travels as a `StickerMessage` with `url`, `directPath`,
  `mediaKey`, `fileSha256`, `fileEncSha256`, `fileLength`, `mimetype`,
  `width`, `height`, and `isAnimated`. ZapExt builds exactly that body in
  `prepare_sticker` (`src/backend/worker.rs`), through whatsapp-rust, and
  sends it with the reply's context when a reply was open.
- The picker is a row of pack entries with the recents and the starred
  stickers first, and a sticker is sent once, with its own bubble.

Taken: the message shape, the pack row idea, and the separate place for
favourites (the tab this fork added in 1.0.25). Not taken: their storage
(IndexedDB blobs). This fork keeps SQLite plus ordinary files.

## Baileys (WhiskeySockets)

A JavaScript library used by bots and mods, and a good record of the
protocol's rough edges.

- `mediaCache` exists so uploaded media is not uploaded again.
- Retry counts live in caches with published lifetimes, for example
  `MSG_RETRY` at one hour and `USER_DEVICES` at five minutes.
- Issue 1085 documents that a sticker's `url` field points at
  `web.whatsapp.net` while the bytes come from `mmg.whatsapp.net` plus the
  `directPath`.

Taken: the principle that a file already in hand is never fetched or
uploaded again if it can be avoided, which is what the content-hash file
names and the attachment sweep implement here, and the bounded-concurrency
idea behind the download semaphore and the sticker fetch rounds. Not taken:
their pluggable cache store; this app has one store and no second backend.

## whatsmeow and whatsmeow-node

Go, and the reference for a minimal correct send.

- A sticker is an ordinary upload plus a `stickerMessage`, and `width` and
  `height` are required, otherwise the receiver shows a generic file
  instead of a sticker.

Taken: the confirmation that the dimensions belong in the message, and that
an animated sticker must say so. ZapExt reads both from the WebP header
(`sticker_shape` in `src/backend/worker.rs`) rather than trusting the
picker.

## ZapZap

A native Linux client over WhatsApp Web, useful as a sanity check on what a
desktop client exposes (tray, notifications, in-chat viewer).

Taken: nothing in code. It informed the decision to stay a native client
with a local store instead of embedding a browser.

## SumatraPDF

The reference for the document viewer, and the model for what comes next.

- Keyboard-first paging, zoom to fit, a page number you can type, and errors
  that say what is wrong with the file.

Taken: the arrows, PageUp and PageDown, Home and End, the typed page number,
rendering the page after the one on screen ahead of time, and keeping the
parsed document in memory while the viewer is open
(`src/pdf.rs`). Not taken yet: continuous scrolling between pages and text
search, which need text extraction from the parser.

## Local to this fork

Decisions with no borrowed equivalent, kept here so they are not mistaken for
a standard:

- A sticker is identified by the SHA-256 of its bytes, and that hash is the
  file name, the thumbnail name, and the de-duplication key across
  favourites, saved stickers, packs, and the phone's recents.
- The pack manifest `pack.json` (title plus the author's order).
- The optimistic send: the local copy and its row are filed before the
  upload starts, so the bubble paints the sticker at once and only its tick
  waits.
- The attachment sweep in `src/cache.rs`, which reclaims files no archived
  message points at, and never touches saved stickers or packs.
- The favourites tab, and the glow that marks the message a quote jump
  landed on.
- In-app video playback in `src/video.rs`: the H.264 track decodes beside the
  soundtrack instead of handing the file to the system player, with the audio
  clock deciding which frame is on screen. Seeking restarts at the nearest
  earlier key frame, so it pays only for the frames between the two.

## Rules that came out of the references

- The picker grid draws previews; the full file is decoded on hover, in the
  preview dialog, and when sending.
- One picture, one file: a copy inside the app's own cache is filed under the
  hash of its bytes.
- The archive is the list of what matters. A sweep may only remove what it no
  longer points at.
- A favourite follows its file when the file is renamed.
