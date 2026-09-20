# Changelog

All notable changes to the ZapExt fork are recorded here.

## [1.0.33] - 2026-09-20

### Fixed

- A full video frame buffer drops nothing anymore. Whatever does not fit
  stays queued in the channel or parks in a one-frame held slot, and the
  decoder waits on it, so pausing through a fast decode keeps every frame.
- Pipe reads that split a four-byte audio sample keep the leftover bytes
  for the next read instead of misaligning the soundtrack into noise. The
  ffmpeg helper is also stopped and reaped on every exit path.
- Long stickers and GIFs play to the end at any length: frames past the
  RAM budget page from a spool file, and only the textures around the
  playhead stay uploaded. A 200-frame sticker at full size keeps its tail,
  its timing and its last frame, vertical animations included.
- Visible animations are spared from memory eviction while anything else
  can go, so two big stickers on screen no longer evict each other in a
  decode loop. The shared budget counts bytes, spooled tails excluded.
- A broken picture really recovers: once its thirty-second cooldown passes,
  the cached error is dropped and the file is read again, and the viewer
  wakes up for the retry.
- Arrow keys adjust a focused progress or volume slider instead of stepping
  media; unfocused, they browse as before.

## [1.0.32] - 2026-09-20

### Fixed

- A fast decoder no longer loses future video frames. When the frame
  buffer fills, the player stops draining and lets the decoder wait on
  its channel instead of dropping what the picture has not reached, so
  pausing no longer risks a frozen picture over continuing sound.
- Switching voice speed mid-clip rebases the progress marker onto the
  switch instant. Walking 1x, 2x, 1.5x and back stays continuous instead
  of jumping ahead or rewinding what was already heard.
- Replaying a finished video starts playing with one click instead of
  parking paused at zero.
- The animation budget prices display pixels: a 512-wide sticker shows at
  320, so the 64 MB cap always holds more than 150 full-width frames. The
  shared resident budget counts bytes too, preparing uploads included, and
  each visible animation uploads fewer textures per tick.
- Arrow keys step between files, or adjust the progress and volume sliders
  while one of them is focused.
- A broken picture really retries: once its thirty-second cooldown passes,
  the cached error is dropped and the file is read again, and the viewer
  wakes up for the retry. A file fixed on disk recovers on its own.
- Soundtrack extraction through ffmpeg streams with the cap applied live:
  a long file no longer sits whole in RAM, and the helper stops past the
  cap instead of decoding the tail for nothing.

## [1.0.31] - 2026-09-20

### Fixed

- The voice speed button now changes the voice itself. A twelve-second
  clip takes about 12 s at 1x, 8 s at 1.5x and 6 s at 2x, the change is
  audible within a fraction of a second, and the progress marker follows
  the speed instead of lagging behind it.
- Pausing a video and resuming no longer counts the stretch before the
  pause twice, so the picture stops jumping ahead. Repeated pauses do not
  accumulate drift.
- A jump keeps its target when the soundtrack cannot land on it. The sound
  joins from a background extraction instead of silently restarting the
  clip from zero.
- The progress bar keeps full precision, so a click at seventy percent
  lands at seventy percent of the clip. Clicks and arrow keys jump at
  once; dragging still lands on release. A paused video stays paused
  across a jump.
- Files the player already refused say why at once instead of spinning
  forever on a player that will never arrive.
- Playback through the ffmpeg fallback runs at 30 frames per second, so
  motion stays smooth on files the in-process decoder cannot read.
- Long stickers and GIFs play to their end instead of looping a truncated
  head, within a 48 MB decoded budget per animation. Their first paint
  spreads over several interface ticks instead of stalling the scroll once.
- A broken picture shows its error and retries quietly after thirty
  seconds instead of burning a decode on every frame.
- The first video frame always paints, even when it is stamped at zero,
  and the picture holds its current frame instead of flashing a future
  one early.
- When a soundtrack ends before its picture (a short track or a capped
  background extraction), the wall clock drives on so the video plays to
  its end instead of freezing.
- Helper media processes (probe, decode, extraction) no longer flash a
  console window on Windows.

## [1.0.30] - 2026-09-18

### Fixed

- A jump in a video starts on a key frame. It used to begin on a delta
  frame whenever the search window held no key frame, which fed the decoder
  pictures it could not reconstruct and lined up the timestamps after them
  wrong, so a jump played as a stutter.
- The player keeps one texture per video and updates it in place, instead of
  allocating a texture for every decoded frame.
- A sink opened for a video no longer prints rodio's drop notice on every
  seek.

## [1.0.29] - 2026-09-18

### Fixed

- Videos with trailing metadata or odd streams play with sound. Files
  the streaming decoder cannot read fall back to an in-process background
  extraction (symphonia) instead of going quiet, so sound works without
  ffmpeg installed; ffmpeg stays only as a last resort for exotic codecs.
  Fragmented files and other codecs play through ffmpeg when it is
  installed instead of refusing.
- Seeks jump instead of scanning: the player searches a small window of
  samples around the estimated position for the nearest key frame, so
  dragging the bar lands at once instead of fast-forwarding.
- Unplayable files say why: HEVC (often from iPhone) and other codecs are
  named, and fragmented files explain they need ffmpeg or the default app.

## [1.0.28] - 2026-09-18

### Fixed

- Saved chats open at once with their messages. The newest chats preload
  when the app starts, a chat whose first page never answers is asked again
  instead of staying blank, and the loading state says so.
- Chats split across a privacy id and a phone number read as one. Rows
  written before the mapping was known move under the number when it is
  learned and at every startup, and reads check both ids until the move.

## [1.0.27] - 2026-09-18

### New

- Jumps land instantly. A jump opens on its keyframe still right away and
  resumes at the asked second once live frames arrive, instead of scanning
  through everything between. A Seeking chip marks the catch-up.
- The player has sound controls: a mute button, a level slider, and the M
  key, with the level remembered across restarts. Clicking the picture
  itself plays or pauses.
- A video shows the sender's poster until its first frame decodes, and a
  video that arrived without one gets its first frame as a poster after
  downloading.
- A PDF reopens where its reader left it, turns pages with a quarter-turn
  button or the R key, and grows a strip of page previews on the left,
  with the open page marked.

## [1.0.26] - 2026-09-18

### New

- Videos play inside the app. Clicking one opens the viewer playing it with
  its soundtrack, with play and pause, a bar to jump through it, and Space
  as a shortcut. The true length comes from the file itself. A file the
  in-process decoder cannot read says why and still offers the default app.

### Fixed

- A program attached to a chat can no longer run from the app at all.
  Clicking its card reveals the file selected in its folder instead of
  executing it, and Show in folder now selects the file instead of opening
  the bare folder.
- Stickers from older versions are filed under their content hash on sight,
  so they get thumbnails, one shared identity, and a single entry instead
  of showing twice with full-size tiles.
- The picker keys every downloaded sticker by the bytes on disk first, so
  the phone's list and the chat history agree and the same picture is
  listed once however it arrived.
- Previews whose sticker file is gone, and phone copies nothing names
  anymore, are reclaimed by the cache sweep. Saved stickers and packs are
  never touched.

## [1.0.25] - 2026-09-18

### New

- The picker has a Favourites tab. Every sticker you marked is listed there,
  wherever it came from, and a sticker seen in a chat can be marked from its
  own right-click menu, which now offers Add to favourites and Remove from
  favourites the way the official client does.

### Changed

- Importing a pack from a signal.art link is gone. The picker no longer shows
  a link field or the Find packs button, and the code that fetched and
  decrypted those packs went with them. Open pack file, for a .wastickers or
  zip archive, stays.
- A favourite follows its file. When a sticker copy is filed under the hash of
  its bytes, a favourite that named the old file is renamed with it, so a
  favourite made before 1.0.22 no longer disappears from the picker.
- A tile's menu follows the folder it is in. Only a sticker in the saved folder
  offers Remove from saved (a pack sticker or one of the phone's copies used to
  show a menu item that did nothing), and only a copy in the app's own cache is
  ever thrown away to be fetched again.

### Docs

- STICKERS.md records what this fork compared itself against, what was taken
  from each reference, and which decisions are local.

## [1.0.24] - 2026-09-18

### Fixed

- The arrow keys of the PDF viewer do what the hint and the buttons say: all
  four turn the page. Left and right used to jump to the next file in the chat
  while the bar's Previous and Next turned pages.

## [1.0.23] - 2026-09-18

### New

- Clicking the quoted block of a message takes the chat to the message it
  answers and lights it up: the view scrolls it to the middle of the screen and
  a soft glow fades over that bubble for a moment, so it is clear which message
  the reply belongs to. A search result or a notification that opens a message
  gets the same glow.
- A page number in the PDF viewer's top bar is a field: type a page and press
  Enter to jump straight to it. PageUp and PageDown walk ten pages at a time,
  and Home and End go to the first and last page.

### Changed

- PDF pages turn faster and use less memory. The document stays parsed in
  memory while it is on screen, so turning a page or zooming no longer reads
  and interprets the file again; the page after the one on screen is rendered
  ahead, and a page already drawn comes back from memory instead of being
  rasterised twice. The open document is dropped when the viewer closes.
- A very tall PDF page is scaled back, so one raster cannot eat the memory of
  the viewer, and a page wider than the last render is the one that replaces
  it, so a closer look stays sharp without rasterising the same page twice.

### Fixed

- A PDF that is password protected or damaged now says so, and offers Open in
  the default app, instead of leaving the viewer waiting for a page.
- A crash inside the PDF parser is reported as a damaged file rather than
  leaving the spinner on screen forever.

## [1.0.22] - 2026-09-18

### Fixed

- A sticker you send is on screen as soon as you confirm it. The bubble used
  to wait for the whole upload before it appeared, so a slow one left a
  spinner in the chat for minutes with nothing to show. The copy is filed
  locally and drawn first, the upload fills in the tick behind it, and a send
  that fails marks the bubble instead of leaving it spinning.
- Sending a sticker no longer leaves the reply bar armed, and a sticker sent
  while a reply was open is sent as that reply.
- Stickers keep their shape. egui paints a picture into whatever rectangle it
  is handed, so a sticker that is not square was stretched to fill its tile in
  the picker and its box in the send and peek dialogs. Every picture drawn by
  the picker or those dialogs is now measured and fitted.

### Changed

- Every sticker copy in the app's own cache is filed under the hash of its
  bytes. The phone's recents get the same small previews as saved stickers and
  packs instead of being decoded at full size, and the same picture is one
  file, one preview, and one entry in the picker.
- The attachment cache is swept once per run. Files no message points at any
  more, left over from an interrupted download, a failed write, or a message
  that is gone, are reclaimed and the freed space is written to the log.
  Saved stickers and imported packs are your own files and are never touched.

## [1.0.21] - 2026-09-18

### Fixed

- Stickers load properly. The picker used to hand egui the full 512 px file
  for every tile, and the WebP loader decodes every frame of an animated one
  and keeps it, so a grid of them buried the interface in decoded pixels and
  most tiles never appeared. Every sticker now gets a 128 px static preview
  built in the background, filed beside the stickers, and the grid draws only
  that; the full file is decoded on hover, in the peek dialog and when sending.
  A tile that is still being read shows its own surface instead of a hole.
- The same picture can no longer be listed twice. Imported packs are filed
  under the hash of their content with a manifest that keeps the title and the
  order, older packs are brought up to date the first time they are read, and
  the picker drops anything already listed higher up: favourites, then saved
  stickers, then packs, then the phone's recents.
- Clicking a Windows notification brings the app to the front. Windows only
  lets the foreground process take focus, so the window could stay behind
  another one, or stay minimized, with the chat already open. Showing the
  window now restores it, borrows the foreground thread's input queue while
  asking for the front, and repeats the request for a moment while the window
  comes up.

### Changed

- A picture in the viewer can be copied to the clipboard, from a Copy button
  in the bar or from the right-click menu over the picture, which also offers
  Save a copy.

## [1.0.20] - 2026-09-18

### Changed

- A sticker no longer opens the media viewer. Clicking one shows it bigger in
  a small dialog with a Save button, and stickers stay out of the viewer's next
  and previous order.
- The sticker picker never lists the same picture twice: a sticker that is
  saved or marked as a favourite is not repeated under the phone's recents.
- Attachments say what they are. An image sent as a file shows as an image and
  opens in the viewer, a document shows its kind (PDF, Program, Archive) beside
  the size, and a page count of zero is no longer printed as "0 pages".
- A program carries a warning and its menu offers Save a copy instead of
  opening it, so running something from a chat takes a deliberate step.
- Every attachment menu has Show info, listing the type, MIME type, size,
  pixels, length, pages, date, chat, message id, the file on disk and its
  SHA-256.
- Downloads show a moving bar while the bytes are on their way, on file cards
  and on video posters.
- The message menu shows one line for its timeline (sent, delivered, read)
  instead of a row per step.
- The Chats and Channels tabs are inset from the sidebar edge.
- The audio speed chip belongs to its own bubble: 1x, 1.5x or 2x for that clip
  alone, applied the moment it is clicked and from where the clip already is.
  Settings explains the chip instead of carrying a global speed.
- View-once photos and videos are labelled as such, with no download that can
  only fail and no quiet retries behind them.
- A file WhatsApp does not offer to linked devices says so in plain words
  instead of repeating the library's error.
- The About dialog says ZapExt is a community fork of ZapFast, and the
  repository description no longer points at the upstream site.

### Tests

- Added per-message speed tests in audio.rs and app.rs, and demo pages for the
  sticker peek and the file info dialog so the headless layout test renders
  them.

## [1.0.19] - 2026-09-18

### Fixed

- A sticker marked as a favourite is shown even when the saved, pack and
  recent lists are all empty. The picker's empty check counted only those
  three lists, so the Favourites section never appeared on its own.

## [1.0.18] - 2026-09-17

### Fixed

- The PDF viewer only replaced the page on screen when a different page
  arrived, so a page rendered again at a higher resolution after a zoom was
  thrown away and the worker was asked for it on every frame. A sharper render
  of the page now takes the place of the one in memory.
- A sticker marked as a favourite only showed up in the Favourites section
  when it was also one of your saved stickers. The section now holds every
  marked sticker that is still on disk, wherever it came from, and leaves out
  the ones whose copy is gone.
- A clip that finished while the next-audio setting was off no longer starts
  playing when the setting is turned on later.
- Typing in the chat search keeps working after a result is clicked: the field
  takes focus back.

### Tests

- Added a_rendered_pdf_page_reaches_the_viewer_and_stale_ones_are_dropped,
  covering the page hand-off, a stale answer, the page limits and the close
  path, and reworked favourites_come_first_and_unknown_ones_are_dropped around
  real files on disk.

## [1.0.17] - 2026-09-17

### Tests

- Added a_videos_soundtrack_decodes_when_its_metadata_leads. It pins what the
  audio path can do with a video today: rodio and symphonia decode the AAC
  track of an mp4 when the moov atom leads the file (ffmpeg's +faststart) and
  refuse the same clip when it sits at the end. Playing videos in the app is
  the next piece of work and this is the constraint it has to handle.

## [1.0.16] - 2026-09-17

### Changed

- Right-clicking a sticker in the picker can mark it as a favourite or clear
  the mark. The sticker tab leads with a Favourites section, then the rest of
  your saved stickers, then imported packs and the phone's recent list. The
  list lives in the encrypted archive, so it survives a restart and stays on
  this machine.

### Tests

- Added sticker_favourites_toggle_and_survive_a_damaged_list, which covers
  marking, clearing and a corrupted stored list, and
  favourites_come_first_and_unknown_ones_are_dropped.

## [1.0.15] - 2026-09-17

### Changed

- PDFs open in the app instead of the desktop. The viewer renders the page on
  this device with a pure-Rust rasteriser (hayro, so no native library has to
  be bundled), turns the pages with the up and down arrows or the bar buttons,
  shows where you are in the document, and keeps Save a copy and Open in the
  default app. A page viewed closer is rasterised again at the higher
  resolution, and a page that cannot be rendered says so instead of failing
  quietly. Other documents and videos still open in their desktop apps.

### Tests

- Added a_page_renders_to_pixels_and_a_missing_one_reports, which renders a
  hand-written PDF and checks the pixels, the page count, the asked width and
  the missing-page error, plus the_asked_width_follows_the_zoom_within_limits;
  the demo has a pdf page for screenshots.

## [1.0.14] - 2026-09-17

### Changed

- Every chat header has a magnifier that searches inside that chat. Results
  appear under the field with the sender, the time and the message text, and
  clicking one jumps to the message in the conversation. The search runs a
  quarter of a second after you stop typing, only the open chat answers, and
  Esc or the X closes it.

### Tests

- Added the_in_chat_search_opens_fills_and_closes, which covers opening,
  scheduling the run, the answer, a stale answer being dropped, a chat switch
  closing the search and the close path; search_finds_text_captions_and_file_names
  now checks that one chat's hits never include another chat's.

## [1.0.13] - 2026-09-17

### Changed

- Voice messages and audio play faster when you want them to. Every audio
  bubble carries a speed chip that walks through 1x, 1.5x and 2x, and Settings
  has the same control under Audio. The choice is saved. Speed is playback
  only, so the file, the waveform and the played receipt are unchanged.
- When a voice message or audio clip reaches its end, the next one in the same
  chat starts on its own, passing over text and clips without a file, the way
  the phone behaves. Settings has a switch for it.

### Tests

- Added autoplay_takes_the_next_audio_that_has_a_file,
  the_speed_button_walks_the_supported_speeds and
  audio_speed_walks_a_fixed_cycle.

## [1.0.12] - 2026-09-17

### Changed

- Clicking a picture or sticker opens it in a full-window viewer instead of
  handing the file to the desktop. Scroll to zoom, drag to move, the arrow keys
  walk the chat's pictures, 0 or F fits it again, and Esc closes. The bar offers
  Save a copy, Open in the default app, and a counter of where you are.
  Animated stickers and GIFs keep playing in it.

### Tests

- Added the_viewer_walks_the_pictures_that_are_on_disk,
  a_picture_fits_the_window_without_blowing_up_thin_air and
  only_moving_formats_are_decoded_frame_by_frame; the demo has a viewer page
  for screenshots.

## [1.0.11] - 2026-09-17

### Changed

- The picker offers emoji and stickers only. The GIF tab and its GIPHY search
  are gone, along with the GIPHY key setting and the `ZAPFAST_GIPHY_KEY` build
  variable; GIFs received in chats still play in the message list. A settings
  file written while the GIF tab existed still opens, and only the stored picker
  tab falls back to emoji.

### Fixed

- The sticker picker fills in a few tiles at a time instead of asking for a
  whole library at once: one round fetches ten stickers, the next starts as
  soon as one of them lands, and every download in the app shares four slots.
  A sticker that keeps failing is left alone until the picker is opened again.
- A phone sticker whose cached file disappeared (a cleared cache directory, for
  example) is downloaded again instead of staying invisible in the picker
  forever, because the recorded path is now checked against the disk.

### Tests

- Added `a_cached_sticker_that_is_gone_is_fetched_again_and_stuck_ones_wait`
  and `a_retired_gif_picker_tab_keeps_the_other_settings`; the demo tour now
  covers emoji and stickers without the GIF segment, and is 35 seconds long.

## [1.0.10] - 2026-09-17

### Fixed

- The About dialog has a Check for updates button next to the version. It reports all three outcomes (available, already latest, failed) instead of checking silently once a day, and never gets stuck spinning.

### Tests

- Added manual_update_check_reports_all_outcomes, covering the button flow from tap through update-found, up-to-date and failure answers.

## [1.0.9] - 2026-09-17

### Fixed

- Right-click a message and choose Select to tick several messages, then forward them together or delete them together. The selection bar above the composer shows the count with Forward, Delete and a clear button; Escape also leaves selection mode.
- Forwarding follows WhatsApp's own caps, researched against the WhatsApp Help Center, the WhatsApp Blog and the protocol library: up to five destination chats at once (the historical 20-chat limit was cut to five), or a single chat for frequently forwarded messages (forwarding score of five or more). Anything beyond the cap is cut with an explanation instead of fanning out.
- Bulk forwards travel the same single-forward path one at a time with a short pause between sends, so a big forward never looks like a burst to the server.
- The delete confirmation splits the batch: messages still inside the two-day revoke window go to everyone, the rest only here, with exact counts before anything happens.

### Tests

- Added only_plain_content_can_be_forwarded, selection_toggles_and_clears, deleting_many_splits_revocable_from_local_only, five_hops_make_a_frequently_forwarded_message and forward_destinations_keep_five_chats_or_one_for_viral; the demo tour still passes unmodified.

## [1.0.8] - 2026-09-17

### Fixed

- Clicking a sticker now previews it first: a confirmation dialog shows the sticker with Send and Cancel, and sending closes both the dialog and the picker. The Send button stays disabled when the sticker file is gone.
- The picker reopens on the last used tab (emoji, GIF or stickers) instead of always starting on emoji, and remembers the choice across restarts.
- Chat and search rows show Brazilian numbers in the national shape again: names stored before that grouping landed are normalized on display, and contact rows no longer show raw digits.
- Downloads that could never display are rejected before filing: empty files and image bytes no decoder accepts now fail the download (with the usual quiet retries) instead of sitting in the cache as permanent error tiles.
- A picture, sticker or picker tile whose filed file never decodes heals itself once: the broken copy is deleted and downloaded again (phone-cache copies through the sticker fetcher, chat copies as attachments), keeping the loading state meanwhile. Only bytes that come back broken too end up as an error. Saved stickers and imported packs are never touched.

### Tests

- Added `stored_number_names_display_in_the_national_shape`, `opening_the_picker_remembers_its_tab`, `sending_a_sticker_closes_its_confirm_dialog`, `broken_pictures_heal_exactly_once`, `empty_and_unreadable_downloads_are_rejected_before_filing` and `healing_a_broken_copy_clears_it_for_redownload`; the demo tour now confirms the sticker dialog through its Send button like a user would.

## [1.0.7] - 2026-09-17

### Fixed

- The Windows executable reports the fork version in its file properties (`1.0.7`) instead of the upstream crate version, both as the file version and as the product version, while the crate keeps the upstream package version for compatibility.
- Long audio attachments no longer materialize fully in memory: formats other than OGG/Opus decode to a temporary mono 48 kHz spool file and stream from disk during playback, seeking reuses the shared samples (memory) or reopens the spool (disk) instead of copying the remaining tail, and the decoded data is released when playback ends. Tapping play again decodes the original file, so replaying shows a brief loading state instead of restarting instantly.
- Voice recordings spill raw microphone samples to disk as they arrive and convert once at the end, so a long recording peaks at one copy of the clip instead of two.
- The in-app logo texture is uploaded once per window and reused across frames instead of re-uploaded on every frame.
- A forced group metadata request replaces the group's older queued entry instead of duplicating it, so each group is queried at most once per tick as the rate limit intends.
- Text without a strongly RTL paragraph reuses egui's cached galley untouched instead of cloning it on every frame; Hebrew and Arabic rendering is unchanged.
- History sync that would clear a locally archived flag is now logged (without identifiers) so the phone's behavior can be confirmed before changing the merge rule; the flag itself is still applied as before.
- Removed the stale one-off `release-zapext-v1.0.4.yml` workflow: it pinned the v1.0.4 tag with a version check that fails on current main, while the canonical `release.yml` already handles every `v*` tag.

### Tests

- Added `shared_samples_play_only_the_tail`, `file_samples_read_back_with_skip`, `spooled_conversion_matches_in_memory` (bit-exact against `voice::mono_at_rate` at 44.1 kHz stereo, 8 kHz mono and 48 kHz stereo), `spooled_waveform_matches`, `ltr_text_reuses_the_cached_galley` and `the_logo_texture_is_uploaded_once_per_context`; `group_questions_wait_in_line` now asserts a forced request replaces the older entry.

## [1.0.6] - 2026-09-17

### Fixed

- The macOS updater no longer gives up when a disk image carries a name that is not a real app bundle (a symlink, for example). It skips that name and keeps looking, so an upgrade or a rollback still finds the usable bundle; a disk image with nothing usable still reports an invalid app bundle. The `renamed_images_prefer_the_new_bundle_and_still_accept_old_images` test that caught this now passes on macOS.

## [1.0.5] - 2026-09-17

### Fixed

- The application icon is the real ZapExt artwork on every surface: the window, tray, taskbar, Dock, and the in-app logo draw `assets/zapext.png`, and `packaging/icons/zapfast.svg` is now a faithful vector trace of it (gradient background, ribbon Z, speed lines, plus) instead of a simplified flat mark. `scripts/make-icons.py` regenerates all of them from the master logo.
- The chat list's two views are labelled `Chats` and `Channels`.
- Brazilian numbers use the national shape: `+55 75 9 9539 9345` for a mobile (country code, DDD, the mobile 9, then two groups of four) and `+55 75 8351 1141` for a landline, instead of arbitrary groups of three.
- Chats without an address-book name show the profile name their owner chose (`~Name`) instead of the raw number, and a contact update without a name no longer erases a name that is already known.
- The sticker picker offers saved stickers, imported packs, the phone's recent list, and the stickers the user sent. Stickers that merely passed through a chat are no longer listed or downloaded, even when their file is cached.
- A media download that fails for a transient reason (dropped connection, busy server) repeats quietly with backoff (1s, 3s, 8s, 20s) while the bubble keeps its loading state, so the reader sees the picture instead of an error. Expired media (403/404/410, already re-requested once) still reports immediately.
- A picture or sticker whose local file cannot be decoded yet keeps its loading state and retries quietly, instead of showing `Could not display this picture`, and only reports after the attempts are used up.
- A sticker or GIF whose decoder failed once (a file still being written, a decode that ran out of memory) is decoded again after 30 seconds instead of staying blank for the rest of the session, and a decode that never finishes is forgotten the same way.
- The quiet retry of a picture holds a bounded budget (five attempts, 600 ms apart) and releases its per-message state as soon as the picture shows.
- Notifications use a grouped `NotificationTarget` (chat, message, opener) so `cargo clippy -D warnings` passes on all platforms; clicking still opens the exact message.
- The `icon_has_clear_corners_and_visible_interior` test passes on Linux, macOS, and Windows, and a mark that cannot be decoded at all still falls back to a plain disc instead of a malformed icon.
- Fork version handling trims `VERSION` everywhere (window title, CLI `--version`, User-Agent, update checks) and tests assert the trimmed value instead of a hardcoded number.
- Self-update verification compares the fork version (`zapext_version()`) instead of `CARGO_PKG_VERSION`, so `1.0.x` releases no longer fail the `wrong version` receipt check.
- User-visible branding now says `ZapExt` (About dialog, Settings, update window, tray, macOS menus, login, empty state, notifications, keyring errors, linked-device name) while internal `zapfast` executable, storage, AppUserModelID, bundle identifier, and Flatpak ID remain unchanged for compatibility.
- macOS updater accepts `ZapExt.app` first and still accepts `ZapFast.app`/`FastsApp.app` for upgrades and rollbacks; staging preserves the downloaded bundle name.
- Linked-device pairing now reports `ZapExt` with the fork version; existing pairings keep their old name until relinking.

### Tests

- Added `brazilian_numbers_follow_the_national_shape`, `the_installed_vector_logo_stays_scalable`, `only_transient_download_failures_are_repeated`, `the_picker_lists_the_stickers_the_user_sent`, `a_failure_that_aged_out_is_decoded_again`, and `picture_retries_are_spaced_and_bounded`; `app_icon_scales_and_falls_back_without_panicking` now asserts the raster artwork is what the window and tray start from and that the vector still rasterizes.
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
