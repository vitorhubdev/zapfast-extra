#!/usr/bin/env bash
# Proves a built file contains the commit it claims. A sidecar stamp is not
# enough: the compiled bytes must contain the SHA.
# Usage: release-origin.sh <sha> <file>
set -euo pipefail
sha=${1:?sha}
file=${2:?file}
[[ "$sha" =~ ^[0-9a-fA-F]{40}$ ]] || {
  echo "Not a commit sha: $sha" >&2
  exit 1
}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
case "$file" in
  *.tar.gz|*.tgz)
    tar -xzf "$file" -C "$work"
    ;;
  *.zip)
    unzip -q "$file" -d "$work"
    ;;
  *)
    cp "$file" "$work/payload"
    ;;
esac
if ! grep -a -F -q "$sha" "$work" -r; then
  echo "$file does not contain commit $sha" >&2
  exit 1
fi
echo "Origin $sha confirmed in $(basename "$file")"
