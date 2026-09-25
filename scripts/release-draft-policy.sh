#!/usr/bin/env bash
# absent and draft may continue (a rerun can finish the draft).
# published never changes.
set -euo pipefail
case "${1:-}" in
  absent|draft) echo "Release may continue ($1)" ;;
  published)
    echo "Refusing to rewrite a published release" >&2
    exit 1
    ;;
  *)
    echo "Unknown release state: ${1:-}" >&2
    exit 1
    ;;
esac
