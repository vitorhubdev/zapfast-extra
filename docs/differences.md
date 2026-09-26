# Vespera compared with ZapFast 0.16.2

Compared with the ZapFast tag `v0.16.2`, reviewed on 2026-09-24.
This page lists only differences checked in this tree or in that review.
It is not a full changelog. Credits and the MIT license stay in the
[README](../README.md).

[Português](differences.pt-BR.md) · [Español](differences.es.md)

## Validated in this tree

- The window title and `--version` report `Vespera` and the fork version from
  `VERSION`, not the upstream ZapFast version.
- Settings has an update channel: Stable, or Testing which can also install
  release candidates. Draft GitHub releases are never offered.
- There is no Ctrl-click or Shift-click selection of several messages.
  Selection here is text, including a drag that continues outside the message
  list. ZapFast 0.16.2 added message multi-select; that commit does not apply
  to this selection.
- Chat labels, channels, and the archive leave-column from that Vespera
  release are not in this tree.
- Reordering of numbers inside right-to-left lines from that release is not
  in this tree. Paragraphs in Hebrew and Arabic are reordered by font runs
  only.

## Implemented, native drop not yet validated

- On Windows, a downloaded photo, video, or document can be dragged toward a
  folder. The shell is asked for a copy only, including while Shift is held.
  The original stays. `DoDragDrop` returns when the target’s `Drop` returns
  or the drag is cancelled. That does not say the target has finished reading
  the path or will not open it again. A cancelled drag deletes the staged
  name. An accepted copy stays in `drag-export` until a later sweep. The
  sweep removes only idle files older than a day, and never one that is open.
  A consumer that waits longer than that day before opening, and is not
  holding the file open, finds the export gone.
  A failed link shows an error and points at Save a copy. On this machine,
  preparing an 8 MB file (the export directory and the hard link, without
  reading the bytes) took 1 ms. That is only the preparation time. It does
  not show whether the drag feels smooth.
  Dropping onto Explorer was not run in this pass. Use the short checklist
  below before treating it as confirmed.

## Checklist for the Windows drag

1. Use synthetic files only: one photo, one video, one document, including a
   name with an accent.
2. Drag each onto an empty folder. Compare the hash of the copy with the
   original. The original must still be the same file.
3. Hold Shift during a drag. The result must be a copy, not a move.
4. Press Escape, and drop somewhere that refuses the file. The original must
   remain.
5. Do not delete `drag-export` yourself. After the copy is in the destination
   folder, check that the copy still opens. The app removes idle export files
   older than a day.
6. Drag a document whose name already exists in the hold folder, and a file
   on a volume that cannot be hard-linked. Vespera should explain the failure
   and leave Save a copy in the menu.
7. While dragging, the message must not open, text selection in the message
   body must still work, and the video progress bar must still seek.
