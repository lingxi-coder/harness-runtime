# Current Claude alignment acceptance

This directory contains the finite current-candidate acceptance matrix,
selected Native 2.1.293 inputs and raw evidence, and replay instructions for
Harness owners. It covers the integrated runtime, SDK and client domains; it
does not claim that the listed fixtures exhaust every behavior.

The pinned Native executable is identified by SHA-256
4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e. Set
CLAUDE_NATIVE_BINARY explicitly before running any Native fixture. Runners
check the hash, use a dummy API key and loopback-only model stub, isolate HOME
and Claude config, and write fresh output under each fixture's run-output/.
They do not download a binary or read credentials from the invoking shell.

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
