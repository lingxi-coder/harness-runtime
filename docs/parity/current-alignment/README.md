# Current Claude alignment acceptance

This directory contains the finite current-candidate acceptance matrix,
selected Native inputs and raw evidence, and replay instructions for Harness
owners. The final comparison target is Claude Code 2.1.295. Existing 2.1.293
baselines and 2.1.294 release-delta captures remain immutable, version-scoped
evidence; neither changes the final target or substitutes for 2.1.295 gates.
This covers the integrated runtime, SDK and client domains; it does not claim
that the listed fixtures exhaust every behavior.

The frozen final-target Native executable is Claude Code 2.1.295 at
`/Users/luolingfeng/.local/share/claude/versions/2.1.295`, SHA-256
`0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`.
Official release: https://github.com/anthropics/claude-code/releases/tag/v2.1.295.
Set `CLAUDE_NATIVE_BINARY` explicitly for any new 2.1.295 probe. Historical
2.1.293/2.1.294 fixture runners retain their own pinned version and hash
checks; use those exact binaries when replaying those baselines. Runners use a
dummy API key and loopback-only model stub, isolate HOME and Claude config, and
write fresh output only to run-specific temporary directories or ignored
`run-output/` locations documented by each fixture. Checked-in Native captures
are not overwritten. Runners do not download a binary or read credentials
from the invoking shell. No versions after 2.1.295 are in scope for this
acceptance pass.

## Presentation scope

LingXi keeps its own UI. Claude TUI layout, ANSI decoration, and key mapping do
not need byte-for-byte replication. This is a visual-presentation exception
only: it does not relax checks for machine-readable CLI JSON/JSONL and event
bytes, model prompts or requests, Skill availability/selection/parsing, or
ModUI subscriptions, event payloads and ordering, state updates, disposal,
and restart behavior. Interactive Skill-menu width remains unmeasured as a
visual detail; its functional selection and prompt effects remain in scope.

From this directory, the current public Native fixture runners are:

    CLAUDE_NATIVE_BINARY=/path/to/claude python3 fixtures/hook-regexp/run-native.py
    CLAUDE_NATIVE_BINARY=/path/to/claude python3 fixtures/mod-middleware/run-native.py
    CLAUDE_NATIVE_BINARY=/path/to/claude python3 fixtures/mod-tool-register/run-native.py
    CLAUDE_NATIVE_BINARY=/path/to/claude python3 fixtures/commands/run-native.py
    CLAUDE_NATIVE_BINARY=/path/to/claude python3 fixtures/provider-error/run-native.py
    CLAUDE_NATIVE_BINARY=/path/to/claude python3 fixtures/statusless-stream-error/run-native.py
    CLAUDE_NATIVE_BINARY=/path/to/claude python3 fixtures/stop-details/run-native.py
    CLAUDE_STOP_DETAILS_CASE=refusal-generic CLAUDE_NATIVE_BINARY=/path/to/claude python3 fixtures/stop-details/run-native.py

The checked-in native-baseline files are immutable captured evidence. Fresh
runs write to ignored run-output directories and do not overwrite baselines.
Do not copy generated config directories, user account data, plugin compiler
caches, or runtime caches into this acceptance tree.

Harness replay must use the same fixture input and the actual production
runtime path. See HARNESS-REPLAY.md for the artifact contract and entrypoints.
Only evidence from the shared final candidate closes a matrix row. A Native
receipt, historical test, mocked unit test, or unexecuted case remains scoped
evidence or Pending/Blocked, not a combined pass.
