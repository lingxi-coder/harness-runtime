#!/bin/sh
set -eu
umask 077
event_name="$1"
capture_path="$2"
cat > "$capture_path"
printf '%s\n' "$event_name" > "$capture_path.event"
