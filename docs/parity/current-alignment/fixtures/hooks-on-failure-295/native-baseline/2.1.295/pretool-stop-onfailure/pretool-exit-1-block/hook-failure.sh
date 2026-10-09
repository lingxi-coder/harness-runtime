#!/bin/sh
cat > /Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration/harness-runtime/docs/parity/current-alignment/fixtures/hooks-on-failure-295/run-output/pretool-exit-1-block/pretool-hook-input.raw.json
printf '%s\n' 'ran' > /Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration/harness-runtime/docs/parity/current-alignment/fixtures/hooks-on-failure-295/run-output/pretool-exit-1-block/pretool-hook-invocation.marker
printf '%s\n' 'FIXED_PRETOOL_HOOK_STDOUT'
printf '%s\n' 'FIXED_PRETOOL_HOOK_STDERR' >&2
exit 1
