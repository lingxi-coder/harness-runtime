#!/bin/bash
set -Eeuo pipefail

readonly integration=/Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration
readonly product="$integration/lingxi-app"
readonly frozen_env=/Users/luolingfeng/lingxi/.buildcache/claude-alignment-integration-r51/frozen-build-env.sh
readonly cargo_bin=/Users/luolingfeng/.rustup/toolchains/1.94.0-aarch64-apple-darwin/bin
readonly cargo_home_bin=/Users/luolingfeng/.cargo/bin
readonly bun_bin=/Users/luolingfeng/lingxi/.buildcache/claude-alignment-integration-r51/bun-1.4.3
readonly python=/Users/luolingfeng/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3.12
readonly guard="$integration/acceptance/product-final-r342-candidate/source_guard.py"
readonly package_parent=/Users/luolingfeng/lingxi/.buildcache/claude-alignment-integration-r51/packages
readonly sdk_sha=a363c5b3a237d53eea12c3abfb3b98648ef15d3e
readonly mobile_sha=6b033d41a2b6b736fda1e8f996f55a4b20bd885d

if (($# != 5)); then
  echo "usage: $0 FINAL_PRODUCT_SHA FINAL_HARNESS_SHA FINAL_SDK_SHA FINAL_MOBILE_SHA /private/tmp/flare-r342-unique.log" >&2
  exit 2
fi
readonly product_sha=$1
readonly harness_sha=$2
readonly passed_sdk_sha=$3
readonly passed_mobile_sha=$4
readonly log_path=$5
[[ "$passed_sdk_sha" == "$sdk_sha" && "$passed_mobile_sha" == "$mobile_sha" ]] || {
  echo 'SDK/Mobile SHA arguments differ from the accepted current pins' >&2
  exit 2
}
case "$log_path" in
  /private/tmp/flare-r342-*.log) ;;
  *) echo 'package log must use a fresh /private/tmp/flare-r342-*.log path' >&2; exit 2 ;;
esac
[[ ! -e "$log_path" && ! -L "$log_path" ]] || { echo 'package log already exists' >&2; exit 2; }
[[ -d "$package_parent" && ! -L "$package_parent" ]] || { echo 'isolated package parent is missing or symlinked' >&2; exit 2; }

set -a
source "$frozen_env"
set +a
export PATH="$bun_bin:$cargo_bin:$cargo_home_bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
export CARGO_BUILD_JOBS=6
export CARGO_NET_OFFLINE=true
export CARGO_TARGET_DIR=/Users/luolingfeng/lingxi/.buildcache/claude-alignment-integration-r51/target
export RUST_MIN_STACK=16777216

readonly output_root="$package_parent/p-${product_sha:0:10}-r342-$(date -u '+%Y%m%dT%H%M%SZ')"
[[ ! -e "$output_root" && ! -L "$output_root" ]] || { echo 'package output path already exists' >&2; exit 2; }

guard() {
  "$python" "$guard" --product-sha "$product_sha" --harness-sha "$harness_sha" \
    --sdk-sha "$sdk_sha" --mobile-sha "$mobile_sha" --space
}

guard
mkdir -m 700 "$output_root"
guard
echo "Flare package: Product $product_sha / Harness $harness_sha" >"$log_path"
if npm run package:mac:flare -- --channel development --output-root "$output_root" \
  >>"$log_path" 2>&1; then
  package_exit=0
else
  package_exit=$?
fi
guard
if ((package_exit != 0)); then
  cat "$log_path" >&2
  exit "$package_exit"
fi
echo "Flare package verified by normal wrapper: $output_root" | tee -a "$log_path"
