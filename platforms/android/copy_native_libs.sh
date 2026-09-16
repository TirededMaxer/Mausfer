#!/usr/bin/env bash
# Copies prebuilt Rust native libraries (built manually for each target triple)
# into the Android project's jniLibs directories, replacing the broken
# `cargo tauri android android-studio-script` WebSocket flow.
#
# Usage: copy_native_libs.sh <arch: aarch64|armv7|x86|x86_64> <profile: debug|release>
set -euo pipefail

ARCH="$1"
PROFILE="${2:-release}"

case "$ARCH" in
  arm64|aarch64) TRIPLE="aarch64-linux-android"; ABI="arm64-v8a" ;;
  arm|armv7)      TRIPLE="armv7-linux-androideabi";  ABI="armeabi-v7a" ;;
  x86|i686)       TRIPLE="i686-linux-android";        ABI="x86" ;;
  x86_64)         TRIPLE="x86_64-linux-android";      ABI="x86_64" ;;
  *) echo "unknown arch: $ARCH" >&2; exit 1 ;;
esac

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"          # platforms/android
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SRC="$REPO_ROOT/target/$TRIPLE/$PROFILE/libmausfer_android.so"
DEST="$SCRIPT_DIR/gen/android/app/src/main/jniLibs/$ABI/libmausfer_android.so"

if [ ! -f "$SRC" ]; then
  echo "[copy-native-libs] missing: $SRC (build it first)" >&2
  exit 1
fi
# The library MUST export the JNI entry points the Kotlin side calls
# (Java_com_mausfer_app_Rust_*). A missing symbol means the app crashes with
# UnsatisfiedLinkError on startup. NOTE: `grep -q` would SIGPIPE `strings`
# and fail under `set -o pipefail`, so grep without -q instead.
if ! strings "$SRC" | grep "Java_com_mausfer_app_Rust_create" >/dev/null; then
  echo "[copy-native-libs] ERROR: $SRC has no Java_com_mausfer_app_Rust_create JNI symbol" >&2
  echo "  (rebuild with build_android.sh so TAURI_ANDROID_* env vars are set)" >&2
  exit 1
fi
mkdir -p "$(dirname "$DEST")"
# Copy through a sibling temporary file, then atomically replace the target.
# This also works when an earlier build left DEST as a symlink back to SRC;
# a direct `cp SRC DEST` treats that as copying a file onto itself on macOS.
TMP_DEST="${DEST}.tmp-$$"
trap 'rm -f "$TMP_DEST"' EXIT
cp "$SRC" "$TMP_DEST"
mv -f "$TMP_DEST" "$DEST"
trap - EXIT
echo "[copy-native-libs] $TRIPLE -> $DEST"
