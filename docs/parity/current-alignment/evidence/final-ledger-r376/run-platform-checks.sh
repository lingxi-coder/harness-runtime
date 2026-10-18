#!/bin/bash
set -uo pipefail

readonly integration=/Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration
readonly repo="$integration/lingxi-app"
readonly frozen_env=/Users/luolingfeng/lingxi/.buildcache/claude-alignment-integration-r51/frozen-build-env.sh
readonly cargo_bin=/Users/luolingfeng/.rustup/toolchains/1.94.0-aarch64-apple-darwin/bin
readonly cargo_home_bin=/Users/luolingfeng/.cargo/bin
readonly python=/Users/luolingfeng/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3.12
readonly source_guard="$integration/acceptance/product-final-r342-candidate/source_guard.py"
readonly source_guard_sha=d1a855731b2c4ec0fcb880ef09513592d4aac95067210182383127185dc49f8d
readonly sdk_sha=a363c5b3a237d53eea12c3abfb3b98648ef15d3e
readonly mobile_sha=6b033d41a2b6b736fda1e8f996f55a4b20bd885d
readonly ndk=/Users/luolingfeng/Library/Android/sdk/ndk/27.2.12479018
readonly minimum_free_kib=3145728

if (($# != 2)); then
  echo "usage: $0 FINAL_PRODUCT_SHA FINAL_HARNESS_SHA" >&2
  exit 2
fi
readonly product_sha=$1
readonly harness_sha=$2
[[ "$product_sha" =~ ^[0-9a-f]{40}$ ]] || { echo 'Product SHA must be a full lowercase commit SHA' >&2; exit 2; }
[[ "$harness_sha" =~ ^[0-9a-f]{40}$ ]] || { echo 'Harness SHA must be a full lowercase commit SHA' >&2; exit 2; }
[[ "$product_sha" != f71087b428665c30d0bd752e16f76cb53ab7fef6 ]] || { echo 'retired Product baseline is not a final acceptance input' >&2; exit 2; }
[[ "$harness_sha" != 4d6d0d6c428e6624a303f5e2a5c96eb4a392f1a1 ]] || { echo 'retired Harness baseline is not a final acceptance input' >&2; exit 2; }
: "${LINGXI_PLATFORM_LOG_DIR:?Set a fresh absolute log directory before running}"
case "$LINGXI_PLATFORM_LOG_DIR" in
  /*) ;;
  *) echo 'platform log directory must be absolute' >&2; exit 2 ;;
esac
[[ ! -e "$LINGXI_PLATFORM_LOG_DIR" && ! -L "$LINGXI_PLATFORM_LOG_DIR" ]] || {
  echo "platform log directory already exists: $LINGXI_PLATFORM_LOG_DIR" >&2
  exit 2
}
[[ -f "$source_guard" && ! -L "$source_guard" ]] || { echo 'r342 source guard is missing or symlinked' >&2; exit 2; }
actual_guard_sha=$(shasum -a 256 "$source_guard" | awk '{print $1}')
[[ "$actual_guard_sha" == "$source_guard_sha" ]] || { echo "r342 source guard SHA changed: $actual_guard_sha" >&2; exit 2; }
[[ -f "$frozen_env" && ! -L "$frozen_env" ]] || { echo 'frozen build environment is missing or symlinked' >&2; exit 2; }

cd "$repo"
set -a
source "$frozen_env"
set +a
export PATH="/Users/luolingfeng/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin:/Users/luolingfeng/lingxi/.buildcache/claude-alignment-integration-r51/bun-1.4.3:$cargo_bin:$cargo_home_bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
export CARGO_HOME=/Users/luolingfeng/lingxi/.buildcache/claude-alignment-integration-r51/cargo-home
export CARGO_TARGET_DIR=/Users/luolingfeng/lingxi/.buildcache/claude-alignment-integration-r51/target
export CARGO_NET_OFFLINE=true
export RUSTUP_TOOLCHAIN=1.94.0
export CARGO_BUILD_JOBS=6

assert_source_stable() {
  "$python" "$source_guard" \
    --product-sha "$product_sha" \
    --harness-sha "$harness_sha" \
    --sdk-sha "$sdk_sha" \
    --mobile-sha "$mobile_sha" \
    --space >/dev/null || return $?
}

assert_source_stable || exit $?
[[ -d "$ndk" ]] || { echo "NDK not found: $ndk" >&2; exit 2; }
[[ -x "$cargo_bin/cargo" ]] || { echo 'Pinned Cargo 1.94 binary not found' >&2; exit 2; }
[[ -x "$cargo_home_bin/cargo-ndk" ]] || { echo 'cargo-ndk executable not found' >&2; exit 2; }
mkdir -m 700 "$LINGXI_PLATFORM_LOG_DIR"
printf 'product_sha=%s\nharness_sha=%s\nsdk_sha=%s\nmobile_sha=%s\n' \
  "$product_sha" "$harness_sha" "$sdk_sha" "$mobile_sha" \
  >"$LINGXI_PLATFORM_LOG_DIR/identities.txt"

ensure_space() {
  local available_kib
  available_kib=$(df -Pk "$CARGO_TARGET_DIR" | awk 'END { print $4 }')
  if (( available_kib < minimum_free_kib )); then
    printf 'disk guard: %s KiB free, minimum is %s KiB\n' "$available_kib" "$minimum_free_kib" >&2
    return 2
  fi
  printf 'disk free: %s KiB\n' "$available_kib"
}

run_check() {
  local name=$1
  shift
  assert_source_stable || return $?
  ensure_space || return $?
  printf '\n=== %s ===\n' "$name" | tee "$LINGXI_PLATFORM_LOG_DIR/$name.log"
  "$@" 2>&1 | tee -a "$LINGXI_PLATFORM_LOG_DIR/$name.log"
  local check_exit=${PIPESTATUS[0]}
  assert_source_stable || return $?
  printf 'exit_code=%s\n' "$check_exit" | tee -a "$LINGXI_PLATFORM_LOG_DIR/$name.log"
  return "$check_exit"
}

run_check android-arm64-default env \
  ANDROID_NDK_HOME="$ndk" ANDROID_NDK_ROOT="$ndk" NDK_API=28 \
  cargo ndk --target arm64-v8a --platform 28 --manifest-path apps/android/ffi/Cargo.toml \
  check -p android-aar --locked --offline || exit $?
run_check android-x86_64-default env \
  ANDROID_NDK_HOME="$ndk" ANDROID_NDK_ROOT="$ndk" NDK_API=28 \
  cargo ndk --target x86_64 --platform 28 --manifest-path apps/android/ffi/Cargo.toml \
  check -p android-aar --locked --offline || exit $?
run_check android-arm64-computer-use env \
  ANDROID_NDK_HOME="$ndk" ANDROID_NDK_ROOT="$ndk" NDK_API=28 \
  cargo ndk --target arm64-v8a --platform 28 --manifest-path apps/android/ffi/Cargo.toml \
  check -p android-aar --features android-computer-use --locked --offline || exit $?
run_check android-x86_64-computer-use env \
  ANDROID_NDK_HOME="$ndk" ANDROID_NDK_ROOT="$ndk" NDK_API=28 \
  cargo ndk --target x86_64 --platform 28 --manifest-path apps/android/ffi/Cargo.toml \
  check -p android-aar --features android-computer-use --locked --offline || exit $?
run_check ios-simulator-default env IPHONEOS_DEPLOYMENT_TARGET=18.0 \
  cargo check --locked --offline -p ios-framework --target aarch64-apple-ios-sim || exit $?

assert_source_stable || exit $?
