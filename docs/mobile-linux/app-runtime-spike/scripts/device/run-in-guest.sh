#!/bin/sh
# shellcheck shell=dash
# run-in-guest.sh — runs ON the Android device via `adb shell` (mksh/toybox).
#
# Wraps the locally built PRoot binary around the staged Alpine 3.21.3 rootfs
# using the same invocation shape as the product runtime (see
# docs/superpowers/references/OpenMinis .../sandbox/PRootKernel.kt):
#
#   proot -0 --link2symlink -r <rootfs> -b /dev -b /proc -b /sys -w /root ...
#
# Usage (from the Mac):
#   adb shell /data/local/tmp/lingxi-appdev-spike/run-in-guest.sh 'cat /etc/alpine-release'
#   adb shell /data/local/tmp/lingxi-appdev-spike/run-in-guest.sh          # interactive guest sh
#
# All arguments are joined into one string and executed with `/bin/sh -c`
# inside the guest, so quote the whole guest command as a single argument.
# The device is assumed OFFLINE: nothing here fetches anything.

BASE="${SPIKE_DEVICE_DIR:-/data/local/tmp/lingxi-appdev-spike}"
PROOT_BIN="$BASE/bin/proot"
PROOT_LOADER_BIN="$BASE/bin/loader"
ROOTFS="$BASE/rootfs"

for required in "$PROOT_BIN" "$PROOT_LOADER_BIN"; do
    if [ ! -f "$required" ]; then
        echo "run-in-guest: missing $required (run scripts/mac/push-spike.sh first)" >&2
        exit 2
    fi
done
if [ ! -d "$ROOTFS/bin" ]; then
    echo "run-in-guest: rootfs not staged at $ROOTFS (run scripts/mac/push-spike.sh first)" >&2
    exit 2
fi
mkdir -p "$BASE/proot-tmp"

# PRoot host-side knobs. PROOT_LOADER points at the read-only pushed loader,
# mirroring how PRootKernel honours a side-loaded loader file.
PROOT_TMP_DIR="$BASE/proot-tmp"
PROOT_LOADER="$PROOT_LOADER_BIN"
export PROOT_TMP_DIR PROOT_LOADER

# Guest environment: mirror the PRootKernel defaults (PATH/HOME/ENV/CHARSET),
# plus offline/telemetry guards for the spike. `env` overrides the adb-shell
# Android PATH, which would otherwise leak into the guest.
GUEST_PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

if [ "$#" -eq 0 ]; then
    set -- /bin/sh
else
    set -- /bin/sh -c "$*"
fi

exec env \
    HOME=/root \
    TMPDIR=/tmp \
    PATH="$GUEST_PATH" \
    ENV=/etc/profile \
    CHARSET=UTF-8 \
    LANG=C.UTF-8 \
    NO_COLOR=1 \
    NEXT_TELEMETRY_DISABLED=1 \
    "$PROOT_BIN" -0 --link2symlink -r "$ROOTFS" \
    -b /dev -b /proc -b /sys \
    -w /root \
    "$@"
