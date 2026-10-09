#!/bin/sh
cat > /Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration/harness-runtime/docs/parity/current-alignment/fixtures/hooks-on-failure-295/run-output/command-exit-1-block/command-hook-input.raw.json
printf '%s\n' 'FIXED_COMMAND_HOOK_STDOUT'
printf '%s\n' 'FIXED_COMMAND_HOOK_STDERR' >&2
printf '%s\n' 'ran' > /Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration/harness-runtime/docs/parity/current-alignment/fixtures/hooks-on-failure-295/run-output/command-exit-1-block/command-hook-ran.marker
exit 1
