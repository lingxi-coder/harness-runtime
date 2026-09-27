#!/usr/bin/env bash
# record-host-metrics.sh — capture the RECORD-ONLY spike metrics (battery,
# thermal) from the Android side via adb, before and after a measurement run.
#
# These metrics have no pass/fail threshold in the local-apps V1 acceptance
# targets — they are recorded for the feasibility report only.
#
# Usage:
#   record-host-metrics.sh [--serial SERIAL] [--label before|after|NAME] [--out FILE]
#
# Emits one JSON object (stdout by default). Raw `dumpsys` dumps are written
# next to --out as sidecar .txt files when --out is given.
set -euo pipefail

SERIAL=""
LABEL="snapshot"
OUT=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --serial) SERIAL="${2:-}"; shift 2 ;;
    --label) LABEL="${2:-}"; shift 2 ;;
    --out) OUT="${2:-}"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

ADB=(adb)
[[ -n "${SERIAL}" ]] && ADB=(adb -s "${SERIAL}")
run_adb() { "${ADB[@]}" "$@"; }

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

run_adb shell dumpsys battery > "${WORK}/battery.txt" 2>/dev/null || true
run_adb shell dumpsys thermalservice > "${WORK}/thermalservice.txt" 2>/dev/null || true
# One "type milli_c" line per readable thermal zone (best effort; some ROMs
# hide sysfs thermal zones from the shell user).
# shellcheck disable=SC2016  # $z must expand on the DEVICE shell, not here
run_adb shell 'for z in /sys/class/thermal/thermal_zone*; do
  [ -r "$z/temp" ] || continue
  printf "%s %s\n" "$(cat "$z/type" 2>/dev/null)" "$(cat "$z/temp" 2>/dev/null)"
done 2>/dev/null' > "${WORK}/thermal_zones.txt" || true

JSON="$(python3 - "${WORK}" "${LABEL}" <<'PY'
import json
import pathlib
import re
import sys
import time

work, label = pathlib.Path(sys.argv[1]), sys.argv[2]

def read(name):
    path = work / name
    return path.read_text(errors="replace") if path.is_file() else ""

battery_raw = read("battery.txt")
def battery_field(key):
    match = re.search(rf"^\s*{key}:\s*(-?\d+)\s*$", battery_raw, re.MULTILINE)
    return int(match.group(1)) if match else None

def battery_bool(key):
    match = re.search(rf"^\s*{key}:\s*(true|false)\s*$", battery_raw, re.MULTILINE)
    return None if match is None else match.group(1) == "true"

zones = []
for line in read("thermal_zones.txt").splitlines():
    parts = line.replace("\r", "").rsplit(" ", 1)
    if len(parts) == 2 and re.fullmatch(r"-?\d+", parts[1]):
        zones.append({"type": parts[0].strip(), "milli_c": int(parts[1])})

print(json.dumps({
    "schema_version": 1,
    "kit": "app-runtime-spike",
    "kind": "host-record-only-metrics",
    "label": label,
    "captured_at_epoch_s": int(time.time()),
    "battery": {
        "level_pct": battery_field("level"),
        "temperature_deci_c": battery_field("temperature"),
        "voltage_mv": battery_field("voltage"),
        "ac_powered": battery_bool("AC powered"),
        "usb_powered": battery_bool("USB powered"),
    },
    "thermal_zones": zones[:32],
}, indent=2))
PY
)"

if [[ -n "${OUT}" ]]; then
  printf '%s\n' "${JSON}" > "${OUT}"
  cp "${WORK}/battery.txt" "${OUT}.battery.txt" 2>/dev/null || true
  cp "${WORK}/thermalservice.txt" "${OUT}.thermalservice.txt" 2>/dev/null || true
  echo "wrote ${OUT} (+ raw dumpsys sidecars)"
else
  printf '%s\n' "${JSON}"
fi
