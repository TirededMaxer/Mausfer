#!/usr/bin/env bash
# Rebuild the macOS DMG without the .VolumeIcon.icns that tauri's bundle_dmg
# adds (macOS renders dmg volume icons as a grey template, which some users
# find confusing). The resulting DMG shows the plain Finder disk icon and
# only contains the app + Applications link, exactly like a clean release.
#
# Usage: ./rebuild_dmg.sh <source.dmg> <output.dmg>
set -euo pipefail

SRC="${1:?usage: rebuild_dmg.sh <source.dmg> <output.dmg>}"
OUT="${2:?missing output dmg}"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# 1. Mount the bundled DMG.
hdiutil attach "$SRC" -nobrowse -mountpoint "$TMP/mnt" >/dev/null

# 2. Stage: copy the app, keep the Applications link.
mkdir -p "$TMP/stage"
cp -R "$TMP/mnt/Mausfer.app" "$TMP/stage/"
ln -sf /Applications "$TMP/stage/Applications"

# Tauri's unsigned build leaves the Mach-O with a linker-level ad-hoc
# signature that does not seal the app bundle resources. Re-sign the complete
# bundle so `codesign --verify --deep --strict` succeeds after repackaging.
codesign --force --deep --sign - "$TMP/stage/Mausfer.app"

# 3. Unmount.
hdiutil detach "$TMP/mnt" >/dev/null

# 4. Create a new DMG (UDRW then convert for compression, standard layout).
UDRW="$TMP/tmp-udrw.dmg"
hdiutil create -size 200m -fs HFS+ -volname "Mausfer" -srcfolder "$TMP/stage" "$UDRW" >/dev/null
hdiutil convert "$UDRW" -format UDZO -imagekey zlib-level=9 -o "$OUT" >/dev/null

echo "rebuilt: $OUT"
