#!/usr/bin/env bash
# Builds the Rust native library for all Android ABIs and copies each into the
# Gradle project's jniLibs.
#
# The JNI env vars are REQUIRED: tauri-build reads the identifier from
# config (com.mausfer.app) and the `mobile_entry_point` macro expands the
# `Java_com_mausfer_app_Rust_*` exported symbols from them. Without these the
# .so has no JNI entry points and the app crashes with UnsatisfiedLinkError
# on startup. `--features mobile` gates the `tauri::mobile_entry_point` run().
#
# Usage: build_android.sh [profile: debug|release]
set -euo pipefail

PROFILE="${1:-release}"
case "$PROFILE" in
  debug|release) ;;
  *) echo "unknown profile: $PROFILE" >&2; exit 2 ;;
esac

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
NDK_ROOT="${ANDROID_NDK_HOME:?Set ANDROID_NDK_HOME to your Android NDK directory}"
if [ -x "$NDK_ROOT/bin/llvm-ar" ]; then
  # Accept the earlier script's toolchain-directory override as well.
  NDK="$NDK_ROOT"
else
  case "$(uname -s)" in
    Darwin) NDK_HOST=darwin-x86_64 ;;
    Linux) NDK_HOST=linux-x86_64 ;;
    *) echo "unsupported NDK build host" >&2; exit 2 ;;
  esac
  NDK="$NDK_ROOT/toolchains/llvm/prebuilt/$NDK_HOST"
fi
if [ ! -x "$NDK/bin/llvm-ar" ]; then
  echo "Android NDK toolchain missing: $NDK" >&2
  exit 2
fi

# Required by tauri-build / mobile_entry_point macro (identifier com.mausfer.app).
export TAURI_ANDROID_PACKAGE_NAME_PREFIX=com_mausfer_app
export TAURI_ANDROID_PACKAGE_NAME_APP_NAME=app
export WRY_ANDROID_PACKAGE=com.mausfer.app
export WRY_ANDROID_LIBRARY=mausfer_android
export WRY_ANDROID_KOTLIN_FILES_OUT_DIR="$SCRIPT_DIR/gen/android/app/src/main/java/com/mausfer/app/generated"
export TAURI_ANDROID_PROJECT_PATH="$SCRIPT_DIR/gen/android"
mkdir -p "$WRY_ANDROID_KOTLIN_FILES_OUT_DIR"

build_abi() {
  local triple="$1" arch="$2"
  echo "[build-android] building ${triple}"
  (
    cd "$REPO_ROOT"
    export CC_aarch64_linux_android="$NDK/bin/aarch64-linux-android24-clang"
    export AR_aarch64_linux_android="$NDK/bin/llvm-ar"
    export CC_armv7_linux_androideabi="$NDK/bin/armv7a-linux-androideabi24-clang"
    export AR_armv7_linux_androideabi="$NDK/bin/llvm-ar"
    export CC_x86_64_linux_android="$NDK/bin/x86_64-linux-android24-clang"
    export AR_x86_64_linux_android="$NDK/bin/llvm-ar"
    export CC_i686_linux_android="$NDK/bin/i686-linux-android24-clang"
    export AR_i686_linux_android="$NDK/bin/llvm-ar"
    case "$triple" in
      aarch64-linux-android) export CC="$NDK/bin/aarch64-linux-android24-clang" ;;
      armv7-linux-androideabi) export CC="$NDK/bin/armv7a-linux-androideabi24-clang" ;;
      x86_64-linux-android) export CC="$NDK/bin/x86_64-linux-android24-clang" ;;
      i686-linux-android) export CC="$NDK/bin/i686-linux-android24-clang" ;;
    esac
    export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK/bin/aarch64-linux-android24-clang"
    export CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER="$NDK/bin/armv7a-linux-androideabi24-clang"
    export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$NDK/bin/x86_64-linux-android24-clang"
    export CARGO_TARGET_I686_LINUX_ANDROID_LINKER="$NDK/bin/i686-linux-android24-clang"
    export AR="$NDK/bin/llvm-ar"
    # i686: ring's cc-rs looks up the bare `i686-linux-android-clang` in PATH.
    export PATH="$NDK/bin:$PATH"
    # Keep the array nonempty: Bash 3 treats empty arrays as unset under -u.
    build_command=(cargo build --locked --target "$triple" -p mausfer-android --features mobile,custom-protocol)
    if [ "$PROFILE" = release ]; then build_command+=(--release); fi
    "${build_command[@]}"
  ) 2>&1 | tail -6
  bash "$SCRIPT_DIR/copy_native_libs.sh" "$arch" "$PROFILE"
}

case "${2:-all}" in
  all)
    build_abi aarch64-linux-android aarch64
    build_abi armv7-linux-androideabi armv7
    build_abi x86_64-linux-android x86_64
    build_abi i686-linux-android i686 ;;
  aarch64) build_abi aarch64-linux-android aarch64 ;;
  *) echo "unsupported architecture selection: $2" >&2; exit 2 ;;
esac

echo "[build-android] done"
