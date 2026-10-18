#!/bin/bash
set -Eeuo pipefail

readonly integration=/Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration
readonly package_runner="$integration/acceptance/product-final-r342-candidate/package-flare.sh"
readonly sdk_sha=a363c5b3a237d53eea12c3abfb3b98648ef15d3e
readonly mobile_sha=6b033d41a2b6b736fda1e8f996f55a4b20bd885d

if (($# != 3)); then
  echo "usage: $0 COMMITTED_PRODUCT_SHA COMMITTED_REPAIRED_HARNESS_SHA /private/tmp/flare-r342-unique.log" >&2
  exit 2
fi
readonly product_sha=$1
readonly harness_sha=$2
readonly package_log=$3
[[ "$product_sha" =~ ^[0-9a-f]{40}$ ]] || { echo 'Product SHA must be a full lowercase commit SHA' >&2; exit 2; }
[[ "$harness_sha" =~ ^[0-9a-f]{40}$ ]] || { echo 'Harness SHA must be a full lowercase commit SHA' >&2; exit 2; }
[[ "$product_sha" != f71087b428665c30d0bd752e16f76cb53ab7fef6 ]] || { echo 'retired Product baseline is not a final package input' >&2; exit 2; }
[[ "$harness_sha" != 4d6d0d6c428e6624a303f5e2a5c96eb4a392f1a1 ]] || { echo 'retired Harness baseline is not a final package input' >&2; exit 2; }
[[ -f "$package_runner" && ! -L "$package_runner" ]] || { echo 'r342 Flare package runner is missing or symlinked' >&2; exit 2; }

exec bash "$package_runner" "$product_sha" "$harness_sha" "$sdk_sha" "$mobile_sha" "$package_log"
