#!/bin/sh
count=0
if [ -f /Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration/harness-runtime/docs/parity/current-alignment/fixtures/hooks-on-failure-295/run-output/stop-exit-1-block/stop-hook-attempt-count ]; then count=$(cat /Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration/harness-runtime/docs/parity/current-alignment/fixtures/hooks-on-failure-295/run-output/stop-exit-1-block/stop-hook-attempt-count); fi
count=$((count + 1))
printf '%s\n' "$count" > /Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration/harness-runtime/docs/parity/current-alignment/fixtures/hooks-on-failure-295/run-output/stop-exit-1-block/stop-hook-attempt-count
cat > /Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration/harness-runtime/docs/parity/current-alignment/fixtures/hooks-on-failure-295/run-output/stop-exit-1-block/stop-hook-input-$count.raw.json
printf '%s\n' "attempt-$count" > /Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration/harness-runtime/docs/parity/current-alignment/fixtures/hooks-on-failure-295/run-output/stop-exit-1-block/stop-hook-attempt-$count.marker
printf '%s\n' "FIXED_STOP_HOOK_STDOUT_ATTEMPT_$count"
printf '%s\n' "FIXED_STOP_HOOK_STDERR_ATTEMPT_$count" >&2
if [ "$count" -eq 1 ]; then exit 1; fi
exit 0
