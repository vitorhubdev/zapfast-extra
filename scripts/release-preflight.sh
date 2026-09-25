#!/usr/bin/env bash
# Checks that a ZapExt tag, VERSION, and the crate metadata are one version.
# Refuses a tag that already points at another commit. Does not create tags
# or publish anything.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

version=$(tr -d '\r\n' < VERSION)
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "VERSION must be X.Y.Z, found: $version" >&2
  exit 1
fi

crate=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
if [[ "$crate" != "$version" ]]; then
  echo "Cargo.toml version ($crate) does not match VERSION ($version)" >&2
  exit 1
fi

lock=$(awk '
  $0 == "name = \"zapfast\"" { found = 1; next }
  found && $1 == "version" { gsub(/"/, "", $3); print $3; exit }
' Cargo.lock)
if [[ "$lock" != "$version" ]]; then
  echo "Cargo.lock zapfast version ($lock) does not match VERSION ($version)" >&2
  exit 1
fi

tag=${1:-}
if [[ -n "$tag" ]]; then
  expected="v$version"
  if [[ "$tag" != "$expected" && ! "$tag" =~ ^"$expected"-rc\.[0-9]+$ ]]; then
    echo "Tag $tag does not match VERSION ($expected or ${expected}-rc.N)" >&2
    exit 1
  fi
  if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
    tagged=$(git rev-parse "refs/tags/$tag^{}")
    head=$(git rev-parse HEAD)
    if [[ "$tagged" != "$head" ]]; then
      echo "Tag $tag already points at $tagged, not HEAD $head. Refusing to move it." >&2
      exit 1
    fi
  fi
fi

echo "Release preflight passed for $version${tag:+ ($tag)}"
