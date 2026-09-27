#!/bin/sh
# shellcheck shell=dash
# measure-spike.sh — runs INSIDE the Alpine 3.21.3 guest under PRoot.
#
# Measures the local-apps phase-0 feasibility metrics for Node + Next.js:
#   1. cold `next dev` time-to-first-page   (gate: <= 120 s)
#   2. one HMR edit round-trip              (record-only)
#   3. warm `next dev` restart              (gate: <= 30 s)
#   4. `NEXT_OUTPUT=export next build`      (gate: <= 180 s)
#   5. peak RSS of the node process tree    (gate: <= 800 MB)
#   6. disk usage                           (record-only)
#   plus best-effort thermal/battery sysfs snapshots (record-only).
#
# The four gate thresholds are copied verbatim from the local-apps V1
# acceptance targets; this script does not define them. Do not edit the
# GATE_* values without updating the V1 target document.
#
# Compatibility contract: BusyBox 1.37 ash + busybox applets only
# (wget, sed, awk, du, cut, tr, grep, sleep with fractions), plus the staged
# `node` runtime. No bashisms, no network beyond 127.0.0.1, no npm/corepack
# (they are intentionally absent from the guest).
#
# Timing source: /proc/uptime (10 ms resolution, monotonic) — busybox `date`
# has no sub-second format on this rootfs.
#
# Invoke from the Mac:
#   adb shell /data/local/tmp/lingxi-appdev-spike/run-in-guest.sh \
#     'sh /opt/spike/measure-spike.sh'
#
# stdout: a single machine-readable JSON block between sentinel lines
#         (also written to $SPIKE_RESULTS_FILE). All progress goes to stderr.
# exit:   0 all gates pass, 3 at least one gate failed, 2 preflight failure.

set -u

APP_DIR="${SPIKE_APP_DIR:-/root/hello-next}"
PORT="${SPIKE_PORT:-3000}"
BASE_URL="http://127.0.0.1:${PORT}/"
WORK="${SPIKE_WORK_DIR:-/root/spike-work}"
RESULTS_FILE="${SPIKE_RESULTS_FILE:-/root/spike-results.json}"
SAMPLE_INTERVAL="${SPIKE_RSS_SAMPLE_INTERVAL:-1}"

# ── Gates: V1 acceptance targets (verbatim — do not edit here) ──────────────
GATE_FIRST_START_S=120
GATE_WARM_START_S=30
GATE_PEAK_RSS_MB=800
GATE_EXPORT_BUILD_S=180

# Data-collection caps, intentionally above the gates so a near-miss still
# yields a number instead of a timeout null.
FIRST_START_TIMEOUT_S=600
WARM_START_TIMEOUT_S=300
HMR_TIMEOUT_S=180
BUILD_TIMEOUT_S=900

DEV_PID=""
BUILD_PID=""
SAMPLER_PID=""

log() { printf '[measure-spike] %s\n' "$*" >&2; }
now() { cut -d' ' -f1 /proc/uptime; }
elapsed() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", b - a }'; }
# float_le A B — true when A <= B.
float_le() { awk -v a="$1" -v b="$2" 'BEGIN { exit !(a <= b) }'; }

# ── process-tree helpers ────────────────────────────────────────────────────

# write_proc_table FILE — one "pid ppid rss_kb" line per readable process.
# Pure shell builtins per pid (no forks) so the RSS sampler does not flood
# PRoot's ptrace path with short-lived children.
write_proc_table() {
    : > "$1"
    for proc_dir in /proc/[0-9]*; do
        [ -r "$proc_dir/stat" ] || continue
        stat_line=""
        read -r stat_line 2>/dev/null < "$proc_dir/stat" || continue
        table_pid="${stat_line%% *}"
        rest="${stat_line##*) }"
        # shellcheck disable=SC2086  # intentional word split of stat fields
        set -- $rest
        [ "$#" -ge 2 ] || continue
        table_ppid="$2"
        table_rss_kb=0
        while read -r key value _unit; do
            case "$key" in
            VmRSS:) table_rss_kb="$value"; break ;;
            esac
        done 2>/dev/null < "$proc_dir/status"
        printf '%s %s %s\n' "$table_pid" "$table_ppid" "$table_rss_kb" >> "$1"
    done
}

# tree_closure FILE ROOT_PID MODE — MODE=rss prints summed VmRSS kB of the
# subtree rooted at ROOT_PID; MODE=pids prints the subtree pids.
tree_closure() {
    awk -v root="$2" -v mode="$3" '
        { ppid_of[$1] = $2; rss_of[$1] = $3 }
        END {
            mark[root] = 1
            changed = 1
            while (changed) {
                changed = 0
                for (p in ppid_of) {
                    if (!(p in mark) && (ppid_of[p] in mark)) {
                        mark[p] = 1
                        changed = 1
                    }
                }
            }
            if (mode == "pids") {
                for (p in mark) if (p in ppid_of) print p
            } else {
                total = 0
                for (p in mark) total += rss_of[p]
                print total
            }
        }
    ' "$1"
}

scan_tree_rss_kb() {
    write_proc_table "$WORK/rss-scan.list"
    tree_closure "$WORK/rss-scan.list" "$1" rss
}

kill_tree() {
    write_proc_table "$WORK/kill-tree.list"
    tree_pids="$(tree_closure "$WORK/kill-tree.list" "$1" pids)"
    [ -n "$tree_pids" ] || return 0
    # shellcheck disable=SC2086  # pid list is intentionally word-split
    kill $tree_pids 2>/dev/null
    sleep 2
    for tree_pid in $tree_pids; do
        kill -0 "$tree_pid" 2>/dev/null && kill -9 "$tree_pid" 2>/dev/null
    done
    return 0
}

# ── RSS sampler (runs as a background job per dev/build session) ────────────

rss_sampler() {
    sampler_root="$1"
    sampler_out="$2"
    sampler_peak=0
    sampler_n=0
    while kill -0 "$sampler_root" 2>/dev/null && [ ! -f "$sampler_out.stop" ]; do
        sampler_cur="$(scan_tree_rss_kb "$sampler_root")"
        [ -n "$sampler_cur" ] || sampler_cur=0
        sampler_n=$((sampler_n + 1))
        [ "$sampler_cur" -gt "$sampler_peak" ] && sampler_peak="$sampler_cur"
        printf '%s %s\n' "$sampler_peak" "$sampler_n" > "$sampler_out"
        sleep "$SAMPLE_INTERVAL"
    done
}

start_sampler() {
    rm -f "$2" "$2.stop"
    rss_sampler "$1" "$2" &
    SAMPLER_PID=$!
}

# stop_sampler PEAK_FILE — stops the sampler and echoes "peak_kb samples".
stop_sampler() {
    : > "$1.stop"
    if [ -n "$SAMPLER_PID" ]; then
        wait "$SAMPLER_PID" 2>/dev/null
        SAMPLER_PID=""
    fi
    if [ -f "$1" ]; then
        cat "$1"
    else
        echo "0 0"
    fi
}

# ── HTTP polling ────────────────────────────────────────────────────────────

fetch_page() { wget -q -T 5 -O "$1" "$2" 2>/dev/null; }

# wait_for_marker URL MARKER TIMEOUT_S T0 — prints seconds-from-T0 on
# success; returns 1 on timeout.
wait_for_marker() {
    wfm_deadline="$(awk -v a="$4" -v t="$3" 'BEGIN { print a + t }')"
    while :; do
        if fetch_page "$WORK/page.html" "$1" && grep -F -q "$2" "$WORK/page.html"; then
            elapsed "$4" "$(now)"
            return 0
        fi
        if float_le "$wfm_deadline" "$(now)"; then
            return 1
        fi
        sleep 0.5
    done
}

wait_port_closed() {
    wpc_deadline="$(awk -v a="$(now)" 'BEGIN { print a + 30 }')"
    while fetch_page "$WORK/probe.html" "$BASE_URL"; do
        if float_le "$wpc_deadline" "$(now)"; then
            log "WARNING: port $PORT still answering 30s after kill"
            return 1
        fi
        sleep 0.5
    done
    return 0
}

# ── process launchers ───────────────────────────────────────────────────────

start_dev() {
    ( cd "$APP_DIR" && exec env NEXT_TELEMETRY_DISABLED=1 \
        node node_modules/next/dist/bin/next dev -p "$PORT" ) > "$1" 2>&1 &
    DEV_PID=$!
}

start_build() {
    ( cd "$APP_DIR" && exec env NEXT_TELEMETRY_DISABLED=1 NEXT_OUTPUT=export \
        node node_modules/next/dist/bin/next build ) > "$1" 2>&1 &
    BUILD_PID=$!
}

# shellcheck disable=SC2329  # invoked via the trap below
cleanup() {
    [ -n "$SAMPLER_PID" ] && kill "$SAMPLER_PID" 2>/dev/null
    [ -n "$DEV_PID" ] && kill_tree "$DEV_PID"
    [ -n "$BUILD_PID" ] && kill_tree "$BUILD_PID"
}
trap cleanup EXIT INT TERM

# ── record-only snapshots (best effort — nulls are fine) ────────────────────

battery_capacity_pct() { cat /sys/class/power_supply/battery/capacity 2>/dev/null | tr -cd '0-9'; }
battery_temp_deci_c() { cat /sys/class/power_supply/battery/temp 2>/dev/null | tr -cd '0-9-'; }

thermal_summary() {
    ts_out=""
    ts_count=0
    for zone in /sys/class/thermal/thermal_zone*; do
        [ -r "$zone/temp" ] || continue
        [ "$ts_count" -ge 16 ] && break
        zone_type="$(cat "$zone/type" 2>/dev/null | tr -cd 'A-Za-z0-9_.-')"
        zone_temp="$(cat "$zone/temp" 2>/dev/null | tr -cd '0-9-')"
        [ -n "$zone_type" ] || zone_type="${zone#/sys/class/thermal/}"
        [ -n "$zone_temp" ] || continue
        ts_out="${ts_out}${zone_type}=${zone_temp};"
        ts_count=$((ts_count + 1))
    done
    printf '%s' "$ts_out"
}

# ── JSON helpers ────────────────────────────────────────────────────────────

json_num() { if [ -n "${1:-}" ]; then printf '%s' "$1"; else printf 'null'; fi; }
json_str() { if [ -n "${1:-}" ]; then printf '"%s"' "$1"; else printf 'null'; fi; }
gate_pass() {
    if [ -n "${1:-}" ] && float_le "$1" "$2"; then printf 'true'; else printf 'false'; fi
}

# ── preflight ───────────────────────────────────────────────────────────────

mkdir -p "$WORK"
rm -f "$RESULTS_FILE"

preflight_fail() { log "PREFLIGHT FAILED: $*"; exit 2; }

command -v node > /dev/null 2>&1 || preflight_fail "node not on PATH (offline apk install missing?)"
command -v wget > /dev/null 2>&1 || preflight_fail "busybox wget missing"
[ -r /proc/uptime ] || preflight_fail "/proc/uptime unreadable (is /proc bound?)"
[ -d "$APP_DIR" ] || preflight_fail "app dir $APP_DIR missing"
[ -f "$APP_DIR/node_modules/next/dist/bin/next" ] || preflight_fail "next not staged under $APP_DIR/node_modules"
grep -q 'SPIKE_HMR_TOKEN_' "$APP_DIR/app/page.jsx" || preflight_fail "HMR marker missing from app/page.jsx"
swc_binding=""
for swc_candidate in "$APP_DIR"/node_modules/@next/swc-linux-arm64-musl/*.node; do
    [ -e "$swc_candidate" ] && swc_binding="$swc_candidate" && break
done
[ -n "$swc_binding" ] || preflight_fail "@next/swc-linux-arm64-musl native binding missing"
if [ -e "$APP_DIR/node_modules/.bin/npm" ] || command -v npm > /dev/null 2>&1; then
    preflight_fail "npm found in guest — the spike contract forbids npm/corepack on the device"
fi
command -v corepack > /dev/null 2>&1 && preflight_fail "corepack found in guest — forbidden"

NODE_VERSION="$(node --version 2>/dev/null | tr -cd 'v0-9.')"
NEXT_VERSION="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    "$APP_DIR/node_modules/next/package.json" | head -n 1 | tr -cd '0-9A-Za-z.-')"
ALPINE_RELEASE="$(cat /etc/alpine-release 2>/dev/null | tr -cd '0-9.')"
KERNEL_RELEASE="$(uname -r | tr -cd 'A-Za-z0-9_.-')"
MACHINE="$(uname -m | tr -cd 'A-Za-z0-9_')"
CPU_COUNT="$(nproc 2>/dev/null || grep -c '^processor' /proc/cpuinfo)"
MEM_TOTAL_KB="$(awk '/^MemTotal:/ { print $2 }' /proc/meminfo)"
PAGE_SIZE_KB="$(awk '/^KernelPageSize:/ { print $2; exit }' /proc/self/smaps 2>/dev/null)"

log "node=$NODE_VERSION next=$NEXT_VERSION alpine=$ALPINE_RELEASE kernel=$KERNEL_RELEASE"
log "cpus=$CPU_COUNT mem_total_kb=$MEM_TOTAL_KB kernel_page_size_kb=${PAGE_SIZE_KB:-?}"

BEFORE_BATTERY_PCT="$(battery_capacity_pct)"
BEFORE_BATTERY_TEMP="$(battery_temp_deci_c)"
BEFORE_THERMAL="$(thermal_summary)"

# ── phase 1: cold dev start ─────────────────────────────────────────────────

log "phase 1/5: cold 'next dev' start (gate ${GATE_FIRST_START_S}s)"
rm -rf "$APP_DIR/.next" "$APP_DIR/out"
COLD_T0="$(now)"
start_dev "$WORK/dev-cold.log"
start_sampler "$DEV_PID" "$WORK/rss-dev1"
if FIRST_START_S="$(wait_for_marker "$BASE_URL" SPIKE_PAGE_OK "$FIRST_START_TIMEOUT_S" "$COLD_T0")"; then
    log "cold start ready in ${FIRST_START_S}s"
else
    FIRST_START_S=""
    log "cold start TIMED OUT after ${FIRST_START_TIMEOUT_S}s — dev log tail:"
    tail -n 40 "$WORK/dev-cold.log" >&2 || true
fi

# ── phase 2: HMR edit round-trip (record-only) ──────────────────────────────

HMR_S=""
if [ -n "$FIRST_START_S" ]; then
    log "phase 2/5: HMR edit round-trip (record-only)"
    HMR_TOKEN="SPIKE_HMR_TOKEN_$(now | tr -d '.')"
    HMR_T0="$(now)"
    sed -i "s/SPIKE_HMR_TOKEN_[A-Za-z0-9]*/${HMR_TOKEN}/" "$APP_DIR/app/page.jsx"
    if HMR_S="$(wait_for_marker "$BASE_URL" "$HMR_TOKEN" "$HMR_TIMEOUT_S" "$HMR_T0")"; then
        log "HMR round-trip ${HMR_S}s"
    else
        HMR_S=""
        log "HMR TIMED OUT after ${HMR_TIMEOUT_S}s"
    fi
else
    log "phase 2/5: skipped (cold start failed)"
fi

read -r PEAK_DEV1_KB DEV1_SAMPLES <<EOF
$(stop_sampler "$WORK/rss-dev1")
EOF
log "dev session 1 peak rss ${PEAK_DEV1_KB} kB over ${DEV1_SAMPLES} samples"
kill_tree "$DEV_PID"
DEV_PID=""
wait_port_closed || true

# ── phase 3: warm dev restart ───────────────────────────────────────────────

log "phase 3/5: warm 'next dev' restart (gate ${GATE_WARM_START_S}s)"
WARM_T0="$(now)"
start_dev "$WORK/dev-warm.log"
start_sampler "$DEV_PID" "$WORK/rss-dev2"
if WARM_START_S="$(wait_for_marker "$BASE_URL" SPIKE_PAGE_OK "$WARM_START_TIMEOUT_S" "$WARM_T0")"; then
    log "warm start ready in ${WARM_START_S}s"
else
    WARM_START_S=""
    log "warm start TIMED OUT after ${WARM_START_TIMEOUT_S}s — dev log tail:"
    tail -n 40 "$WORK/dev-warm.log" >&2 || true
fi
read -r PEAK_DEV2_KB DEV2_SAMPLES <<EOF
$(stop_sampler "$WORK/rss-dev2")
EOF
log "dev session 2 peak rss ${PEAK_DEV2_KB} kB over ${DEV2_SAMPLES} samples"
kill_tree "$DEV_PID"
DEV_PID=""
wait_port_closed || true

# ── phase 4: export build ───────────────────────────────────────────────────

log "phase 4/5: 'NEXT_OUTPUT=export next build' (gate ${GATE_EXPORT_BUILD_S}s)"
rm -rf "$APP_DIR/out"
BUILD_T0="$(now)"
start_build "$WORK/build.log"
start_sampler "$BUILD_PID" "$WORK/rss-build"
BUILD_DEADLINE="$(awk -v a="$BUILD_T0" -v t="$BUILD_TIMEOUT_S" 'BEGIN { print a + t }')"
BUILD_TIMED_OUT=0
while kill -0 "$BUILD_PID" 2>/dev/null; do
    if float_le "$BUILD_DEADLINE" "$(now)"; then
        BUILD_TIMED_OUT=1
        break
    fi
    sleep 1
done
if [ "$BUILD_TIMED_OUT" -eq 1 ]; then
    log "build TIMED OUT after ${BUILD_TIMEOUT_S}s — killing tree"
    kill_tree "$BUILD_PID"
    BUILD_RC=124
else
    wait "$BUILD_PID"
    BUILD_RC=$?
fi
BUILD_T1="$(now)"
read -r PEAK_BUILD_KB BUILD_SAMPLES <<EOF
$(stop_sampler "$WORK/rss-build")
EOF
BUILD_PID=""
EXPORT_BUILD_S=""
EXPORT_OK=false
if [ "$BUILD_RC" -eq 0 ] && [ -f "$APP_DIR/out/index.html" ]; then
    EXPORT_BUILD_S="$(elapsed "$BUILD_T0" "$BUILD_T1")"
    EXPORT_OK=true
    log "export build ok in ${EXPORT_BUILD_S}s (peak rss ${PEAK_BUILD_KB} kB over ${BUILD_SAMPLES} samples)"
else
    log "export build FAILED rc=$BUILD_RC out/index.html present=$([ -f "$APP_DIR/out/index.html" ] && echo yes || echo no) — build log tail:"
    tail -n 40 "$WORK/build.log" >&2 || true
fi

# ── phase 5: disk usage ─────────────────────────────────────────────────────

log "phase 5/5: disk usage"
du_kb() { du -sk "$1" 2>/dev/null | cut -f1; }
DISK_APP_KB="$(du_kb "$APP_DIR")"
DISK_NODE_MODULES_KB="$(du_kb "$APP_DIR/node_modules")"
DISK_NEXT_CACHE_KB="$(du_kb "$APP_DIR/.next")"
DISK_OUT_KB="$(du_kb "$APP_DIR/out")"
DISK_ROOTFS_KB="$(du -sk /bin /etc /home /lib /media /mnt /opt /root /run /sbin /srv /tmp /usr /var 2>/dev/null | awk '{ t += $1 } END { print t }')"

AFTER_BATTERY_PCT="$(battery_capacity_pct)"
AFTER_BATTERY_TEMP="$(battery_temp_deci_c)"
AFTER_THERMAL="$(thermal_summary)"

# ── gates + results ─────────────────────────────────────────────────────────

PEAK_OVERALL_KB="$PEAK_DEV1_KB"
[ "$PEAK_DEV2_KB" -gt "$PEAK_OVERALL_KB" ] && PEAK_OVERALL_KB="$PEAK_DEV2_KB"
[ "$PEAK_BUILD_KB" -gt "$PEAK_OVERALL_KB" ] && PEAK_OVERALL_KB="$PEAK_BUILD_KB"
PEAK_RSS_MB="$(awk -v kb="$PEAK_OVERALL_KB" 'BEGIN { printf "%.1f", kb / 1024 }')"
TOTAL_SAMPLES=$((DEV1_SAMPLES + DEV2_SAMPLES + BUILD_SAMPLES))
if [ "$TOTAL_SAMPLES" -eq 0 ]; then
    PEAK_RSS_MB=""
fi

PASS_FIRST="$(gate_pass "$FIRST_START_S" "$GATE_FIRST_START_S")"
PASS_WARM="$(gate_pass "$WARM_START_S" "$GATE_WARM_START_S")"
PASS_RSS="$(gate_pass "$PEAK_RSS_MB" "$GATE_PEAK_RSS_MB")"
PASS_BUILD="$(gate_pass "$EXPORT_BUILD_S" "$GATE_EXPORT_BUILD_S")"
OVERALL=false
if [ "$PASS_FIRST" = true ] && [ "$PASS_WARM" = true ] \
    && [ "$PASS_RSS" = true ] && [ "$PASS_BUILD" = true ]; then
    OVERALL=true
fi

{
    printf '{\n'
    printf '  "schema_version": 1,\n'
    printf '  "kit": "app-runtime-spike",\n'
    printf '  "phase": 0,\n'
    printf '  "captured_at_epoch_s": %s,\n' "$(date +%s)"
    printf '  "environment": {\n'
    printf '    "alpine_release": %s,\n' "$(json_str "$ALPINE_RELEASE")"
    printf '    "kernel_release": %s,\n' "$(json_str "$KERNEL_RELEASE")"
    printf '    "machine": %s,\n' "$(json_str "$MACHINE")"
    printf '    "node_version": %s,\n' "$(json_str "$NODE_VERSION")"
    printf '    "next_version": %s,\n' "$(json_str "$NEXT_VERSION")"
    printf '    "bundler": "next-16-default",\n'
    printf '    "cpu_count": %s,\n' "$(json_num "$CPU_COUNT")"
    printf '    "mem_total_kb": %s,\n' "$(json_num "$MEM_TOTAL_KB")"
    printf '    "kernel_page_size_kb": %s,\n' "$(json_num "$PAGE_SIZE_KB")"
    printf '    "rss_sample_interval_s": %s\n' "$(json_num "$SAMPLE_INTERVAL")"
    printf '  },\n'
    printf '  "metrics": {\n'
    printf '    "first_start_s": %s,\n' "$(json_num "$FIRST_START_S")"
    printf '    "warm_start_s": %s,\n' "$(json_num "$WARM_START_S")"
    printf '    "hmr_round_trip_s": %s,\n' "$(json_num "$HMR_S")"
    printf '    "export_build_s": %s,\n' "$(json_num "$EXPORT_BUILD_S")"
    printf '    "export_build_rc": %s,\n' "$(json_num "$BUILD_RC")"
    printf '    "export_output_present": %s,\n' "$EXPORT_OK"
    printf '    "peak_rss_mb": %s,\n' "$(json_num "$PEAK_RSS_MB")"
    printf '    "peak_rss_kb_dev_session1": %s,\n' "$(json_num "$PEAK_DEV1_KB")"
    printf '    "peak_rss_kb_dev_session2": %s,\n' "$(json_num "$PEAK_DEV2_KB")"
    printf '    "peak_rss_kb_build": %s,\n' "$(json_num "$PEAK_BUILD_KB")"
    printf '    "rss_samples_total": %s,\n' "$(json_num "$TOTAL_SAMPLES")"
    printf '    "disk_app_kb": %s,\n' "$(json_num "$DISK_APP_KB")"
    printf '    "disk_node_modules_kb": %s,\n' "$(json_num "$DISK_NODE_MODULES_KB")"
    printf '    "disk_next_cache_kb": %s,\n' "$(json_num "$DISK_NEXT_CACHE_KB")"
    printf '    "disk_export_out_kb": %s,\n' "$(json_num "$DISK_OUT_KB")"
    printf '    "disk_rootfs_total_kb": %s\n' "$(json_num "$DISK_ROOTFS_KB")"
    printf '  },\n'
    printf '  "record_only": {\n'
    printf '    "battery_capacity_pct_before": %s,\n' "$(json_num "$BEFORE_BATTERY_PCT")"
    printf '    "battery_capacity_pct_after": %s,\n' "$(json_num "$AFTER_BATTERY_PCT")"
    printf '    "battery_temp_deci_c_before": %s,\n' "$(json_num "$BEFORE_BATTERY_TEMP")"
    printf '    "battery_temp_deci_c_after": %s,\n' "$(json_num "$AFTER_BATTERY_TEMP")"
    printf '    "thermal_zones_milli_c_before": %s,\n' "$(json_str "$BEFORE_THERMAL")"
    printf '    "thermal_zones_milli_c_after": %s\n' "$(json_str "$AFTER_THERMAL")"
    printf '  },\n'
    printf '  "gates": {\n'
    printf '    "source": "local-apps V1 acceptance targets",\n'
    printf '    "first_start_s": { "value": %s, "max": %s, "pass": %s },\n' \
        "$(json_num "$FIRST_START_S")" "$GATE_FIRST_START_S" "$PASS_FIRST"
    printf '    "warm_start_s": { "value": %s, "max": %s, "pass": %s },\n' \
        "$(json_num "$WARM_START_S")" "$GATE_WARM_START_S" "$PASS_WARM"
    printf '    "peak_rss_mb": { "value": %s, "max": %s, "pass": %s },\n' \
        "$(json_num "$PEAK_RSS_MB")" "$GATE_PEAK_RSS_MB" "$PASS_RSS"
    printf '    "export_build_s": { "value": %s, "max": %s, "pass": %s }\n' \
        "$(json_num "$EXPORT_BUILD_S")" "$GATE_EXPORT_BUILD_S" "$PASS_BUILD"
    printf '  },\n'
    printf '  "overall_pass": %s\n' "$OVERALL"
    printf '}\n'
} > "$RESULTS_FILE"

echo '-----BEGIN SPIKE RESULTS JSON-----'
cat "$RESULTS_FILE"
echo '-----END SPIKE RESULTS JSON-----'

if [ "$OVERALL" = true ]; then
    log "overall: PASS"
    exit 0
fi
log "overall: FAIL (see gates in $RESULTS_FILE)"
exit 3
