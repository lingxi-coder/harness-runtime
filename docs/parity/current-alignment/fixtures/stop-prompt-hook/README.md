# Native Stop prompt-hook release probe

This fixture captures the public Stop prompt-hook request in Claude Code
2.1.293, 2.1.294, and 2.1.295. It installs one fixed `type: "prompt"` hook through an
explicit isolated settings file, returns one fixed main response and the hook
decision `{"ok":true}`, and captures the original HTTP request bodies,
including exact raw `system` and `messages` JSON value slices. Each version's
receipt and original requests are retained separately; no older baseline is
replaced by the 2.1.295 capture.

Run sequentially with explicitly selected binaries:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude-2.1.293 python3 docs/parity/current-alignment/fixtures/stop-prompt-hook/run-native.py
CLAUDE_NATIVE_BINARY=/path/to/claude-2.1.294 python3 docs/parity/current-alignment/fixtures/stop-prompt-hook/run-native.py
CLAUDE_NATIVE_BINARY=/path/to/claude-2.1.295 python3 docs/parity/current-alignment/fixtures/stop-prompt-hook/run-native.py
python3 docs/parity/current-alignment/fixtures/stop-prompt-hook/compare-295.py
```

The runner accepts only versions 2.1.293, 2.1.294, and 2.1.295, verifies the
SHA for each frozen version, and records the selected executable's version and SHA in each
receipt. It uses an isolated HOME/config, a fixed session/model/prompt,
loopback-only routing, a dummy key, and no persistence. The shared CWD and
settings path are identical across sequential version runs so those path
fields do not create an artificial prompt difference.

The checked-in `native-baseline/` keeps separate raw captures and receipts for
each version. `compare-versions.py` compares the original `system` and
`messages` JSON value bytes directly and writes
`native-baseline/comparison-2.1.293-vs-2.1.294.json`. `compare-295.py` records
the three-version raw-field hashes and release-scoped observations in
`native-baseline/comparison-2.1.293-vs-2.1.294-vs-2.1.295.json`. The captured
`system` field is 1,789 bytes for 2.1.293 and 2,262 bytes for each of 2.1.294
and 2.1.295. The allow/block and untrusted-event-JSON evaluator instructions
appear in 2.1.294 and are text-identical in the 2.1.295 capture. The captured
billing header is retained verbatim; its suffix differs across these fixtures
even for the same binary, so it is not attributed solely to the release.

The `messages` field is 15,206 bytes for 2.1.293/2.1.294 and 15,208 bytes for
2.1.295. The hook `ARGUMENTS.prompt_id` is a generated UUIDv4 and differs in
each capture. The field also contains a git-status snapshot and injected date,
which changed between runs. Raw bytes remain unequal; none of these values are
normalized or treated as a release-specific behavior change. See each version's
`request-02.system.raw.json` and `request-02.messages.raw.json` for the original
bytes.

All three runs produced one public Stop prompt-hook request, the fixed main
result, and a zero exit. The stub's fixed `{"ok":true}` response validates hook
reachability and the request/evaluation boundary; it cannot determine whether a
live model follows hook instructions more reliably. The 2.1.293 baseline remains
the frozen parity reference; 2.1.294/2.1.295 are separate Native captures, not
candidate acceptance. Compare original captured prompt-field bytes and keep
dynamic identifiers/time as separate constraints. A missing public hook request
remains Unverified rather than being inferred from settings parsing or source
code. `SubagentStop` is not covered by this fixture.
