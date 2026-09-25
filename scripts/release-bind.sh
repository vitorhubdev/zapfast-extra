#!/usr/bin/env bash
# Binds downloaded release artifacts to one commit and writes checksums.txt.
# Usage: release-bind.sh <dist-dir> <expected-sha> <tag>
set -euo pipefail

dist=${1:?dist directory}
sha=${2:?commit sha}
tag=${3:?tag}
root=$(cd "$(dirname "$0")" && pwd)

cd "$dist"
mapfile -t stamps < <(find . -name 'release-commit-*.txt' -print | sort)
if [[ ${#stamps[@]} -lt 6 ]]; then
  echo "Expected a commit stamp from each platform job, found ${#stamps[@]}" >&2
  printf '  %s\n' "${stamps[@]:-}" >&2
  exit 1
fi
for stamp in "${stamps[@]}"; do
  got=$(tr -d '\r\n' < "$stamp")
  if [[ "$got" != "$sha" ]]; then
    echo "$stamp records $got, not $sha" >&2
    exit 1
  fi
done
find . -name 'release-commit-*.txt' -delete

required=(
  "zapfast-${tag}-x86_64-unknown-linux-gnu.tar.gz"
  "zapfast-${tag}-aarch64-unknown-linux-gnu.tar.gz"
  "zapfast-${tag}-x86_64-pc-windows-msvc.zip"
  "zapfast-${tag}-aarch64-pc-windows-msvc.zip"
  "zapfast-${tag}-x86_64-pc-windows-msvc-setup.exe"
  "zapfast-${tag}-aarch64-pc-windows-msvc-setup.exe"
  "ZapExt-${tag}-windows-x64-portable.exe"
  "ZapExt-${tag}-windows-arm64-portable.exe"
  "zapfast-${tag}-macos-universal.dmg"
  "zapfast-${tag}-x86_64.flatpak"
)
for name in "${required[@]}"; do
  if [[ ! -f "$name" ]]; then
    echo "Missing release file: $name" >&2
    exit 1
  fi
done
for name in "${required[@]}"; do
  case "$name" in
    *.tar.gz|*.zip|*-portable.exe) bash "$root/release-origin.sh" "$sha" "$name" ;;
  esac
done

{
  echo "# commit $sha"
  sha256sum "${required[@]}"
} > checksums.txt
echo "Checksums bound to $sha"
