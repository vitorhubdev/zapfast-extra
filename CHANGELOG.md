# Changelog

All notable changes to the ZapExt fork are recorded here.

## [1.0.91] - 2026-09-25

### Fixed

- A phone sticker whose descriptor changes can be fetched again after earlier failures. Bytes that do not parse as sticker metadata do not use up that retry. A fetched file is what the picker lists. A favorite and an imported pack stay.

### Tests

- Voice notes at two minutes use overlap-add. One sample past that still uses the streaming hop blend, which is the path that hissed. A later player could overlap-add windows of about twenty seconds and keep only that window. Listening is still required, including every clip longer than two minutes.
- The formant samples at 1x, 1.5x and 2x are written again under the local voice-samples folder. The high-band gap is a hint, not proof that the sound is clean.

## [1.0.90] - 2026-09-25

### Tests

- The 1.5x and 2x voice comparison uses a noise burst and a three-formant pulse, not a click on a sine. On that signal the overlap-add has less high-band energy than the old hop blend. A steeper sample step is the pulse, not hiss. The clips are written under the local voice-samples folder. Listening on a device is still required. This does not describe video soundtracks.
- Sticker recovery is classified: a broken phone-cache copy is removed and its path cleared, a cache path with no file is cleared, a saved file outside that cache stays, and raw bytes that are not sticker metadata are not a download reference.

## [1.0.89] - 2026-09-25

### Fixed

- Arabic-Indic digits in a left-to-right line stay in the order they were typed. `٤٥` no longer paints as `٥٤`.
- The soundtrack test for an interleaved video checks that about six seconds of audio decode, not merely that some frames exist. The Rodio pin that stops the track after the first video packet was already in place.

## [1.0.88] - 2026-09-25

### Fixed

- CI installs ffmpeg before the video scrub tests, which were failing because the runner had no encoder.
- Archive restart tests keep their key in a process-local store, so they no longer need a desktop secret service. Installed copies still use the OS keyring.
- The Flatpak command is `zapext`, matching the binary the manifest installs and the desktop entry launches.

## [1.0.87] - 2026-09-25

### Fixed

- On macOS, hiding to the tray turns input methods off and waits one frame before the window is destroyed. Closing the window no longer shares the frame in which AppKit delivers the close. The menu repaint handle is cleared when that window is gone. This targets the input-method crash while the view is being torn down (winit 0.30.13, the same family as winit #4333 and #4626). It is not validated on macOS 26 here.

## [1.0.86] - 2026-09-25

### Fixed

- Voice at 1.5x and 2x for clips up to two minutes uses the same overlap-add as ZapFast 0.16.2. The previous hop blend made larger sample jumps on speech-like audio with a click and noise. Listening on a device is still required. Longer clips keep the streaming stretcher.
- A sticker tile that cannot be drawn says so, instead of the toolkit's error mark. Healing a sticker no longer deletes a file that is neither a cache copy nor a chat attachment.
- The drag data object is covered by a test that calls format listing, format query, and GetData.

## [1.0.85] - 2026-09-24

### Fixed

- Dragging a file out no longer stays in "continue" when the mouse button is released, and the data object lists `CF_HDROP` so a folder can see the file. The path in the drop is absolute. If the folder refuses it, ZapExt says so and points at Save a copy. Accepting the drop is not reported as a finished copy.

## [1.0.84] - 2026-09-24

### Fixed

- An accepted drag no longer deletes the staged file on a timer. `DoDragDrop` only reports that `Drop` returned or that the drag was cancelled. A cancel removes the staged name. A copy stays in `drag-export` until a later sweep, which removes idle files older than a day and leaves open files alone. The original is not deleted. A consumer that waits more than a day to open the export, and is not holding it open, finds the name gone. The native drop into Explorer is not validated.

## [1.0.83] - 2026-09-24

### Fixed

- A drag that returns keeps the staged link until the reader lets go, or for two minutes, and never deletes the original. A second drag of the same name does not remove the first link. If the link cannot be created, ZapExt says so and points at Save a copy. The file does not open on the same gesture.

### Note

- The native drop into Explorer was not exercised here. See the manual steps recorded with this version.
- The staged name is removed at two minutes even while a reader has it open. That drops the name only: an already-open read still returns the bytes, and a reader that has not opened it yet finds nothing. Preparing an 8 MB hard link on this machine took 1 ms. That measurement is not a smoothness result.

## [1.0.82] - 2026-09-24

### Added

- On Windows, drag a downloaded photo, video, or document onto a folder to copy it. The chat message and the original file stay. The drag uses the system shell, does not read the file on the interface thread, and holds the file out of cache cleanup until the drop finishes. A missing or empty file does not start a drag. Save a copy remains available. Names keep accents and drop characters Windows rejects.

### Fixed

- Release checksums may start with a `# commit` line. The updater still finds the file hash.
- A release binary is compiled with the tag's commit and, for a candidate tag, with the `X.Y.Z-rc.N` version. Archives are accepted only when the binary inside contains that commit. Rerunning a workflow may finish a draft and refuses a release that is already published.

## [1.0.81] - 2026-09-24

### Changed

- A version tag now stops in a draft GitHub release. The draft is created only after every platform artifact exists, and its checksums are tied to that commit. An already published release for the same tag is left untouched. Native packages are built and install-checked without being published.

## [1.0.80] - 2026-09-24

### Added

- Resting the pointer on a chat-list preview that was cut short, or that hides further lines, shows the whole last message in a tooltip (group sender first), without opening the chat or marking it read.

### Tests

- Full-summary lines stay behind the one-line label, and a headless hover shows the cut-short message, skips a preview that already fits, and skips a row that is typing.

## [1.0.79] - 2026-09-24

### Fixed

- Chat videos with picture and sound tracks play the whole soundtrack instead of a tenth of a second of audio: rodio is pinned to the revision carrying upstream RustAudio/rodio#833 (same pin as upstream ZapFast 0.16.2), which skips packets of every track but the selected one.

### Tests

- New whole-sound test proving a three-second clip played 209 ms before the pin and its full sound after.

## [1.0.78] - 2026-09-24

### Added

- Link watchdog: after sleep or two silent minutes on a connected link, the worker reconnects through the library instead of showing connected forever.
- Chat photos remember their drawn size, so a texture released while away reloads at its old height instead of the fallback that moved the chat.

### Fixed

- Voice-recording bars grow toward Send from the right edge.
- The crate version now matches VERSION, so the built binary no longer reports itself as 1.0.61.

### Tests

- Ported link-watch coverage (health, silence limit with fresh allowance, sleep detection, slow-worker tolerance) plus a drawn-size round trip for pending layout.

## [1.0.77] - 2026-09-24

### Added

- Update channel in Settings: Stable (finished releases, the default) or Testing (also release candidates, never older builds). Release candidates install only on Testing, with the same checksum and backup guarantees.

### Fixed

- Update checks now run with a 20-second timeout, one flight at a time, and an ETag cache, so a stalled listing fails fast, concurrent checks never stack, and repeat checks revalidate instead of re-downloading. Rate limits, offline hosts and unexpected listings report distinct messages.

### Tests

- Channel compare matrix (including release-candidate ordering and no-downgrade rules), listing selection over drafts and mislabeled entries, ETag replay, rate-limit mapping, busy-skip, stalled-listing timeout, candidate install behind Testing with Stable refusal, and channel setting round trips.

## [1.0.76] - 2026-09-24

### Fixed

- Dragging the video bar fast over slow clips no longer drops the early keyframe picture: a superseded decode still hands over its approximate while only the stale exact is skipped. The release jump stays exact, single and through the full player path.
- Keyframe search measured on a 60-second file (600 samples, distant keyframes): index stays at 0 to 1 ms cold, so no index change was needed; the worst-case wide-window path remains documented for very large files.

### Tests

- New stacked-targets test proving the superseded approximate paints (fails on the old skip path, passes with the fix), plus the 60-second ladder fixture reporting open, index, decode and resize stages.

## [1.0.75] - 2026-09-24

### Added

- Chat photos, thumbnails, video posters, sticker tiles and avatars now share a budgeted image cache (96 resident): pictures that scroll away release their bytes, pixels and GPU textures, and re-register when they come back.
- Screen-reader and keyboard-navigation support through the accesskit integration in the UI framework.

### Fixed

- Protocol errors in the log file no longer repeat private details (contacts, message contents, keys, QR payloads): recognized failures keep their category, everything else becomes a generic note.

### Tests

- Ported cache eviction coverage (current frame never released, scrolled-away images freed with their textures, released images re-registered on return) plus a chat-thumbnail round trip through sweep and sanitized protocol-error cases.

## [1.0.74] - 2026-09-24

### Added

- Dragging the video bar now shows a fast keyframe picture first, then refines to the exact spot. The thumbnail names both the chosen time and the shown frame time, so an early picture never reads as exact.

### Fixed

- Video previews decode much faster on 480p, 720p and 1080p by using smaller thumbnails, a faster downscale, and skipping work for frames that never paint. A newer drag target now aborts a stale decode instead of queueing behind it.
- Releasing the bar still jumps exactly once to the last previewed spot, with pause and volume preserved. The jump uses the full player path, never a thumbnail as a decoder shortcut.

### Tests

- Staged preview (approximate before exact), abort yielding to newer targets, and a resolution ladder reporting open, index, decode and resize stages with cold, warm and cache-hit splits across baseline, distant keyframes, B-frames and VFR.

## [1.0.73] - 2026-09-24

### Added

 - Search the sticker tab by emoji, word, or pack name, using the emoji
   tags carried inside sticker files.
 - Sticker packs shared in chats open for preview from their card and can
   be kept as a new pack, with the listed emoji tags preserved.
 - Favourite stickers sync toward the phone: the intent is persisted with
   its time, retried across restarts and connections, and a late phone
   receipt never claims a newer change still waiting.

### Fixed

 - The same picture favourited from a pack, the recents, or the saved
   stickers is one favourite instead of one entry per path, resolved from
   whichever copy still exists; old path lists migrate without losing files.

### Tests

 - Hash bridge across base64 flavors, EXIF read/write round trip, search
   by emoji/word/pack, intent collapse and late receipts, migration and
   copy-following resolution, restart persistence, phone-change ordering,
   pack unpacking order with hostile entry names, pack classification,
   keeping a viewed pack, and one favourite sharing a pack with a remainder.

## [1.0.72] - 2026-09-23

### Added

- Dragging the video progress bar now previews the destination: playback
  holds, the knob and clock follow the pointer at once, and a thumbnail
  with the target time appears above the bar while the decoded frame loads.
  Releasing jumps exactly once to the last previewed spot, even when the
  pointer leaves the bar; Escape cancels with no jump and restores playback.
  Previews decode off the interface thread with a bounded worker and cache,
  never spawn ffmpeg per movement, and retire when the video is switched
  or closed.

### Fixed

- Releasing a scrub outside the bar no longer jumps to the pointer spot:
  the release confirms the last previewed destination. Stepping to another
  file or opening a new viewer retires an active drag with no jump.

### Tests

- Real slider drag across frames (press, back-and-forth moves, release
  outside the bar), click without a hold, cancel/step/close through the
  production action path, plus preview coverage for B-frames, VFR, wide
  keyframe gaps, silent clips and refusals, stale generations, latest-wins,
  cache budgets and p50/p95 preview latency on representative fixtures.

## [1.0.71] - 2026-09-23

### Fixed

- Voice messages at 1.5x and 2x now compress time while keeping the
  pitch instead of resampling the voice higher. The sink always runs at
  1x; a streaming time-stretch shortens memory and spool sources with
  constant state, keeping position, pause, seek, volume and replay.

### Tests

- Tone, synthetic-speech pitch, durations, silence, short and long
  clips, restart offsets and spool sources through the production PCM
  path, plus listening samples of the same speech at every speed.

## [1.0.70] - 2026-09-23

### Fixed

- Late history can no longer resurrect a message deleted for everyone:
  the revocation sticks across replays on every ingestion path.
- Each fresh connection now asks the phone chat collections for changes
  committed while offline, so deletions done on the phone still apply.

### Tests

- Offline delete then history, duplicate deletes, offline batch filing,
  tombstone migration across privacy ids with restart, same-second paging
  and deletion across open, closed and archived views.

## [1.0.69] - 2026-09-23

### Tests

- Print-screen freeze diagnosis: unfocused frames never touch the
  clipboard and attach nothing; a release outside the window cannot arm
  a later duplicate; a 4K paste costs milliseconds on the interface
  thread; a stalled reader stalls the frame by the same amount. No
  application behavior changed.

## [1.0.68] - 2026-09-23

### Changed

- The built executable is now named zapext instead of zapfast on every
  platform, including the installer, portable build, app bundle and Linux
  packages. The internal crate, storage paths, app ids and sync wire
  identity stay unchanged for compatibility with existing installations.

## [1.0.67] - 2026-09-23

### Fixed

- Videos that decode to zero pictures now fail loudly and take the single
  controlled ffmpeg fallback instead of refusing a playable file.
- Background search results cleared or deleted while flying no longer
  repaint the panel: clear barriers and missing rows filter with tombstones.
- Refusals after a decode failure name ffmpeg with PATH guidance when it
  is missing; the README documents the external fallback requirement.
- Switching or closing during a fallback retires the old engine by
  generation without leaking its pictures into the new file.

### Tests

- Synthetic VFR clip (10fps joined to 5fps) plays and seeks in process:
  forward, back, paused and rapid jumps.
- The same join through a filter re-encode proves fallback after a silent
  decode miss, with engine change and seek landing.
- Release seek benchmark reports frame-available and landed p50 and p95.

## [1.0.66] - 2026-09-23

### Fixed

- Local deletes drop the resident id as well as the row, so a removed
  message can no longer linger in the id set while gone from the screen.
- The PDF forget is generation-guarded: a cleanup started before a new
  document opens can no longer clear the new reader.
- Background search runs at most two at once with the newest waiting query
  coalesced, and only the newest answer paints; hits deleted in flight are
  filtered by tombstone.
- Stale archive echoes below the accepted order are ignored, and the
  accepted order survives a restart.
- Same-second pages, overlapping pages and clear-during-load keep every
  surviving message with a consistent id set.
- Switching files never shows the old clip, and BMP viewing decodes while
  the send path reencodes to JPEG.
- README now states that archive sync between devices is covered only by
  synthetic tests pending live two-device validation.

## [1.0.65] - 2026-09-24

### Fixed

- Archive and unarchive racing each other now converge on the last tap:
  a late phone echo of an older revision can no longer flip the chat back
  or discard the newer queued intent.
- Scrolling through deleted messages no longer skips same-second history:
  pages after a removed cursor still bring every surviving message.
- Closing the PDF viewer never stalls the backend waiting for a slow page.
- Open chats keep full history while inactive ones shrink to recent pages
  and leave memory past ten, reloading from the archive on reopen.
- History pages merge in linear time with a persistent id set instead of
  resorting everything per page.
- Search runs on its own database connection off the worker loop, and only
  the newest query paints.
- The image-dimension test uses a structurally valid BMP on every system
  now that the decoder ships on all platforms.

## [1.0.64] - 2026-09-24

### Fixed

- Videos that reorder frames now play through ffmpeg with correct timing
  instead of silently mistiming the in-process decoder; files the decoder
  stops on fall back automatically once, then refuse with the real reason.
- Seeking holds picture and sound together: audio waits paused until the
  landing frame arrives, then both resume from the target.
- Deleting a playing video or voice really stops it, and the viewer keeps
  the open item by identity instead of sliding to a neighbour.
- Removed attachment files are collected at most 64 per tick with
  deduplication and a second chance before the startup sweep owns orphans.
- Clearing a huge selection stays linear through set membership.

## [1.0.63] - 2026-09-23

### Fixed

- Deleted messages now vanish from every screen at once: conversation,
  global and in-chat search, reply/edit/selection, media viewer, and
  pending notifications go through one invalidation layer.
- A repeated delete or clear still cleans the screen even when the archive
  is already right, so a stale view can never outlive its deletion.
- Clearing a chat keeps newer messages in memory instead of reloading the
  whole conversation, and history arriving late can no longer resurrect
  anything below a deletion barrier on any ingestion path.
- Removed attachment files are reclaimed in batches on the worker tick
  instead of during the delete event.

## [1.0.62] - 2026-09-23

### Fixed

- The Windows installer license page now credits both the upstream ZapFast
  author and the ZapExt fork instead of the upstream author alone.
- Release tags build the macOS universal app again and attach its DMG to
  the release; the temporary macOS skip is gone.
- The image-dimension test no longer depends on a BMP decoder the app only
  carries on Windows: oversized-dimension fixtures are header-only PNGs,
  so Ubuntu and macOS run the same assertions.

## [1.0.61] - 2026-09-23

### Changed

- Prepare an opt-in test release of the accumulated desktop fixes. Release
  candidate tags publish as prereleases without replacing the stable release.
- Windows release candidates keep a numeric executable version while displaying
  the candidate suffix in the installer. Align the crate version with VERSION.
- Isolate worker test directories so parallel avatar and cache fixtures cannot
  reuse each other's files.
- Phone synchronization, installation rollback and cross-platform runtime
  validation remain incomplete; this release is for testing, not stable approval.

## [1.0.60] - 2026-09-23

### Fixed

- Agreeing phone echoes during a flight now move the accepted order forward,
  so a delayed older echo can never flip the chat back afterwards.
- Two archive tasks keep their own identity across a privacy-id migration;
  each completion settles exactly its revision and no third task jumps ahead.
- Local completion records its order marker and clears its intent in one
  transaction, with a visible error and retry instead of a half-persisted
  conclusion.
- Migration reconciles the archived flag with the newest of the surviving
  intent and the accepted order, instead of the merged flag or any queue
  alone.

## [1.0.59] - 2026-09-23

### Fixed

- Archive dispatch reports started tasks only, with a global flight ceiling;
  spent budgets never starve fresh chats and explicit intents reopen them.
- Newer phone changes applied mid-flight survive stale completions; old echoes
  are rejected against the persisted accepted order, restarts included.
- Identity migration transfers the live task instead of racing it, ties break
  by queue order, and aborted writes keep the origin intent with full rollback.

## [1.0.58] - 2026-09-23

### Fixed

- Archive sync dispatches only started tasks against the round cap, so spent
  intents never starve fresh chats; one central decision covers every caller.
- Simultaneous archive flights are bounded globally; explicit intents bypass
  backoff deliberately while the tick respects it.
- Newer phone changes applied mid-flight survive the stale completion; stale
  echoes after cleanup are ignored via recorded remote timestamps.
- Migration budgets ride with winning intent revisions; ties break by queue
  order and aborts keep the origin intent with full rollback.
- Revisions start above legacy queued rows on upgrade.

## [1.0.57] - 2026-09-22

### Fixed

- Archive intents carry persistent revisions that survive clears, with exact
  confirmation and in-flight echo handling; one revision flies per chat.
- Sync retries run on a bounded tick with backoff, per-round dispatch cap, and
  a spent budget that only explicit intents reopen.
- Privacy-id migration reconciles the archived flag, deletes condemned rows,
  moves tombstones and intents atomically, and retries failed migrations.
- Save failures repaint from storage truth and warn instead of diverging.

## [1.0.56] - 2026-09-22

### Fixed

- Archive sync now concludes by revision: a late success cannot erase a newer
  intent, an old failure never spends the new intent budget, and one revision
  flies per chat.
- Unconfirmed archive intents retry on a bounded backoff tick while connected,
  resume after restart, and surface one visible error instead of stalling quiet.
- Deleted-for-me replays stay out of bubbles, unread counts, and notifications,
  not just out of storage.
- Tombstones and sync intents migrate across privacy-id rekeys, with newer-wins
  merges and fresh revisions.
- Archive flag and sync intent persist in one transaction, with failures
  reported instead of half-applied.

## [1.0.55] - 2026-09-22

### Fixed

- Archive and unarchive now survive offline clicks: the intent is queued,
  flushed on reconnect, and a visible error appears after repeated failures
  instead of diverging silently. Newer local intents outrank older remote
  echoes, and agreeing echoes converge.

- Messages deleted for me on the phone now vanish in ZapExt too: single-row
  removal with tombstones against replay, shared files kept while referenced,
  and the open conversation notified.

## [1.0.54] - 2026-09-22

### Fixed

- Demo presentation stays on the fork: the tour opens on the fork
  repository, the sample chat uses a neutral reserved example link
  preview, and the simulated update advertises the fork releases page.
  A detached backend drops update commands, so the demo can never
  fetch or install a real update.

## [1.0.53] - 2026-09-22

### Fixed

- Paste no longer depends on a press the integration never delivers: a
  plain V press marks typing until its release, and a bare release
  without typing behind it is its own gesture. Holding V while pressing
  Ctrl attaches nothing. The arm advances inside the event fold, so a
  Paste and its release in one frame count once.
- A clear that must preserve starred messages deletes nothing, and the
  reference policy is now single: message attachments, sticker favorites
  and cataloged copies guard both direct removal and the startup sweep,
  while interrupted publishes restore before sweeping and pending backups
  stay protected. Damaged favorites abort cleanup instead of emptying it.
- Images validate from disk with headers always, explicit decoder limits
  and pixel budget, and full decode only when small. Test fixtures use
  isolated folders.

## [1.0.52] - 2026-09-22

### Fixed

- Paste gestures no longer depend on a Key press the integration never
  delivers: a plain V press marks typing, Ctrl starts a new shortcut
  epoch, and a bare release without typing behind it is its own gesture.
  The arm advances inside the event fold, so a Paste and its release in
  one frame count once. A clear that must preserve starred messages now
  deletes nothing instead of destroying them. Reference-lookup failures
  keep every file, and sticker favorites and cataloged copies join the
  protected set. Images validate from disk with headers always and full
  decode only when small, under explicit dimension caps. Interrupted
  publishes recover their backup at startup before the cache sweep.

## [1.0.51] - 2026-09-22

### Fixed

- Seeking to the very end of a video no longer reports the file as
  undecodable: the target backs off one millisecond, the decoder always
  emits the final sample, and an exhausted jump past the last picture
  settles as the finished state. Replay from the end restarts playing.
  New fixture tests cover pause/resume, silent clips, soundtrack
  replacement on seek and switch, invalid files, and volume routing.
  Voice speed keeps rodio resampling (pitch rises); the analysis for a
  pitch-preserving WSOLA port lives in .local-roadmap.

## [1.0.50] - 2026-09-22

### Fixed

- Thumbnail rebuilds are proven off the worker thread: a barrier-style
  test holds two rebuilds inside the build step, keeps a third queued,
  and still applies other commands meanwhile. A late avatar download
  after give-up still shows, and the avatar fetch policy (absolute URLs
  only, readable images only) is locked in tests. Newsletter metadata
  picture fields stay unsupported: they are CDN direct paths needing
  media-host auth the app does not negotiate.

## [1.0.49] - 2026-09-22

### Fixed

- Attachment downloads stream into a temporary file instead of RAM: a
  byte budget (declared length plus slack, 2 GiB ceiling) refuses
  exorbitant streams mid-write, a total deadline covers download,
  re-upload, and retry, and the file is published only after validation,
  preserving the last valid copy on failure. Simultaneous requests for
  the same file share one fetch, and expired references still trigger a
  single phone re-upload, now detected by typed HTTP status first.

## [1.0.48] - 2026-09-22

### Added

- Delete and Clear from a linked device now apply locally: a persisted
  removal barrier drops only the range the phone knew, keeps newer
  messages, and stops late history from resurrecting what was removed.
  Replayed actions stay quiet, attachment files are deleted only when no
  surviving message references them, and the interface closes a deleted
  chat or reloads a cleared one. Starred filtering is not preserved:
  clearing removes the range, documented as a limitation.

## [1.0.47] - 2026-09-22

### Fixed

- Paste gestures are folded in delivery order with a Ctrl latch: a Ctrl+V
  press opens image-only gestures so releasing Ctrl first no longer loses
  the paste, a release sharing its frame with the next Paste still sees
  the armed flag instead of duplicating, and the Paste origin comes from
  the Ctrl state at Paste time rather than the end of the frame. Plain V
  typing never attaches, and focus loss dissolves a pending gesture.

## [1.0.46] - 2026-09-22

### Fixed

- Paste gestures are told apart by origin instead of image contents: a
  keyboard Paste arms exactly its own release, a menu paste arms none,
  and a release with nothing armed is its own gesture. Two quick pastes
  of the same picture attach twice, Paste plus its release attaches once,
  menu then shortcut attaches twice, and switching chats carries no
  suppression over. Pure text, empty clipboard, search focus, and channel
  refusal behave as before.

## [1.0.45] - 2026-09-22

### Fixed

- Giving up on a profile picture no longer wipes the photo on screen:
  the worker reports the cached picture when one is still stored and
  reports absence only when there is truly nothing to show.
- Thumbnail rebuilds moved off the worker thread: each rebuild runs as a
  limited background task and reports back, so the worker keeps answering
  other commands while slow stickers render.
- Paste suppression now belongs to its gesture: the release of the staged
  image ends it quietly, while a later paste with another image still
  attaches. A menu paste no longer swallows the next Ctrl+V image.

## [1.0.44] - 2026-09-21

### Fixed

- Image+text paste now handles the event the platform really delivers:
  the integration consumes Ctrl+V presses that carry text and emits Paste
  instead, so the takeover consumes the delivered Paste before the
  composer sees it. A release-only fallback still stages once per gesture.
- Avatar retries keep their failure count, deadline, and in-flight state
  across ticks, backing off between tries and reporting absence after
  three failures instead of retrying forever.
- Thumbnail rebuilds report an explicit per-request result that the picker
  applies: success repaints, failure parks the file without requeueing,
  and the interface no longer polls the disk to infer completion.
- Sends to unknown newsletters are refused by address, new recipients are
  allowed only for direct and group chats, and a store failure denies
  instead of permitting.

## [1.0.43] - 2026-09-21

### Fixed

- Channel pictures no longer pass a relative reference to the HTTP client:
  avatars download only from absolute HTTP(S) URLs, validate as readable
  images before the cache is marked ready, and store atomically. Failures
  retry later and keep the last good photo.
- History sync without an archived flag keeps the stored state, and an
  explicit value archives or unarchives. Group chunks without a name keep
  the known subject instead of falling back to a generic name.
- Channels no longer offer sending: the composer shows a channel notice
  and every send path (text, files, voice, stickers, forwards, pastes,
  recordings, drops) refuses without proven capability.
- Pasting an image plus text stages only the attachment: the key press and
  the textual paste of the same gesture are consumed when the image is
  accepted. Plain text, search fields, and refused chats are untouched.
  (Press-based only; audit found the integration consumes the press, so
  1.0.44 handles the delivered Paste event instead.)

## [1.0.42] - 2026-09-20

### Fixed

- Channel pictures now come from the channel's own metadata instead of
  the contacts lookup: list tiles use the preview image and dialogs use
  the full image, with initials kept when a channel has no picture.

## [1.0.41] - 2026-09-20

### Fixed

- Broken sticker thumbnails now fall back to the original picture at once
  and rebuild locally: the original file is never deleted, nothing is
  downloaded again, and the fixed tile appears without restarting.

## [1.0.40] - 2026-09-20

### Fixed

- Short conversations now sit near the composer: empty space stays above
  the messages instead of between the last bubble and the reply box.
- Rolling another panel no longer pulls the open conversation off its end:
  only wheel and scrollbar gestures over the message list release the follow.

## [1.0.39] - 2026-09-20

### Fixed

- The Archived row now follows the open tab: Channels no longer offers
  archived chats when only normal chats are archived, and opening
  Archived lists exactly the counted chats.

## [1.0.38] - 2026-09-20

### Fixed

- Marked numbers in group messages are clickable again: tapping one
  opens that person's chat, like on the phone.

## [1.0.37] - 2026-09-20

### Fixed

- A jump reuses the open video instead of reopening the file: the still
  stays on screen, the clock holds the target until live frames arrive,
  and picture and sound resume together. Jumps on a paused video stay
  paused and paint without needing the mouse, and only the newest of
  rapid jumps survives.
- The soundtrack opens beside the jump instead of on the interface
  thread, and older decodes and extractions stand down as soon as a newer
  jump retires them.
- A jump opens behind its target even across long gaps between key
  frames, never on a key frame ahead where the frames to show could never
  decode.
- A downloaded video reports its real length and a poster built from its
  own frames, skipping black openings. Zero stays unknown, so the bubble
  omits the length instead of showing a misleading zero, and videos
  downloaded before this version are analyzed without a new download.

## [1.0.36] - 2026-09-20

### Fixed

- Discarding an animation cancels its queued reads: each spool request
  carries the window's epoch, retiring the file refuses every request from
  the old one, and the registry forgets the path, so a session that cycles
  through stickers does not accumulate entries.
- A failed sticker schedules its own retry: the window wakes when the
  cooldown ends instead of waiting for the reader to move something.
- A truncated temporary file is recognised as unusable even though it still
  exists, so the animation fails over with a controlled retry instead of
  painting a frozen picture with pointless fast repaints.
- The pager, not the interface, decides whether one index is unreadable or
  a tail is gone: no disk query runs on the interface thread, and a still
  shown while a frame is missing repaints at the next frame boundary
  rather than every ten milliseconds.

## [1.0.35] - 2026-09-20

### Fixed

- Spooled animation queues stay bounded and cancel cleanly: at most 64
  queued reads and 64 delivered frames, requests that never block a paint,
  and a per-spool epoch that drops work for rebased windows on sight.
- An unreadable tail leaves recorded holes instead of asking forever, and
  a spool file that vanishes mid-play fails over with a controlled retry
  instead of freezing on its last picture.
- A window left behind jumps straight to the playhead and holds its
  request until the frame lands, so coming back to a sticker paints the
  current moment instead of paging stale frames or stalling empty.

## [1.0.34] - 2026-09-20

### Fixed

- Spooled animation frames keep their transparency: pixels roundtrip
  premultiplied instead of darkening soft edges twice.
- A failed spool write aborts the decode instead of skewing later frames,
  a failed final flush ships nothing, and decoding stops at the
  4096-frame cap instead of processing frames that will never be stored.
  The cap covers minutes of sticker; past it the head plays with a warning.
- Spool files delete themselves when their animation is evicted, replaced,
  pruned, abandoned mid-decode or left behind at shutdown.
- Spooled tails page in on a background thread, so a busy disk never blocks
  the interface; corrupt tails leave a hole and page on instead of wedging.

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
