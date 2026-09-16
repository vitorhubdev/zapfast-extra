#!/bin/bash
# Verify the app users receive inside a release DMG. Notarized builds get
# Gatekeeper/stapler checks; ad-hoc builds still get signature, architecture,
# microphone metadata, and entitlement validation.
set -euo pipefail

dmg="$1"
mode="${2:-notarized}"
case "$mode" in
    notarized|adhoc) ;;
    *) echo "unknown verification mode: $mode" >&2; exit 2 ;;
esac

temporary="$(mktemp -d)"
mount="$temporary/mount"
mkdir "$mount"
cleanup() {
    hdiutil detach "$mount" >/dev/null 2>&1 || true
    rm -f "$temporary/entitlements.plist"
    rmdir "$mount" "$temporary" 2>/dev/null || true
}
trap cleanup EXIT

if [ "$mode" = notarized ]; then
    xcrun stapler validate "$dmg"
fi
hdiutil attach "$dmg" -readonly -nobrowse -mountpoint "$mount" >/dev/null
app="$mount/ZapExt.app"
codesign --verify --strict --deep "$app"
if [ "$mode" = notarized ]; then
    spctl --assess --type execute --verbose=2 "$app"
fi
lipo "$app/Contents/MacOS/zapfast" -verify_arch x86_64 arm64
codesign --display --entitlements - --xml "$app" > "$temporary/entitlements.plist"

python3 - "$app/Contents/Info.plist" "$temporary/entitlements.plist" <<'PY'
import plistlib
import sys

with open(sys.argv[1], "rb") as source:
    info = plistlib.load(source)
with open(sys.argv[2], "rb") as source:
    entitlements = plistlib.load(source)
if not info.get("NSMicrophoneUsageDescription", "").strip():
    sys.exit("The release app is missing its microphone permission description")
if entitlements.get("com.apple.security.device.audio-input") is not True:
    sys.exit("The signed release app is missing its audio-input entitlement")
print("Verified microphone permission metadata in the signed universal app")
PY
