# Native Stop prompt-hook release probe

This fixture compares the public Stop prompt-hook request in Claude Code
2.1.293 and 2.1.294. It installs one fixed `type: "prompt"` hook through an
explicit isolated settings file, returns one fixed main response and the hook
decision `{"ok":true}`, and captures the original HTTP request bodies,
including exact raw `system` and `messages` JSON value slices. The two version
receipts and original requests are retained separately; no 2.1.293 baseline is
replaced by the 2.1.294 capture.

Run sequentially with explicitly selected binaries:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude-2.1.293 python3 docs/parity/current-alignment/fixtures/stop-prompt-hook/run-native.py
CLAUDE_NATIVE_BINARY=/path/to/claude-2.1.294 python3 docs/parity/current-alignment/fixtures/stop-prompt-hook/run-native.py
```

The runner accepts only versions 2.1.293 and 2.1.294, verifies the frozen
2.1.293 SHA, and records the selected executable's version and SHA in each
receipt. It uses an isolated HOME/config, a fixed session/model/prompt,
loopback-only routing, a dummy key, and no persistence. The shared CWD and
settings path are identical across sequential version runs so those path
fields do not create an artificial prompt difference.

The checked-in `native-baseline/` keeps separate raw captures and receipts for
both versions. `compare-versions.py` compares the original `system` and
`messages` JSON value bytes directly and writes
`native-baseline/comparison-2.1.293-vs-2.1.294.json`. On the captured hook
request, `system` is 1,789 bytes for 2.1.293 and 2,262 bytes for 2.1.294. The
2.1.294 evaluator adds instructions for interpreting allow/block conditions
and treating event JSON as evidence rather than authority; it also has the
expected `cc_version` billing-header change. The `messages` field is the same
size (15,206 bytes) but raw-unequal: the nested `ARGUMENTS.prompt_id` differs
between Native invocations. Both captured values have UUIDv4 shape. The fixture
does not normalize that value or claim whole-message equality. See the two
versioned raw `request-02.system.raw.json` and
`request-02.messages.raw.json` files for the complete original bytes.

The stub's fixed `{"ok":true}` response validates hook reachability and the
request/evaluation boundary; it cannot determine whether a live model follows
hook instructions more reliably. The 2.1.293 baseline remains the frozen
parity reference; the 2.1.294 capture is a separate release delta that still
needs candidate comparison. Compare original captured prompt-field bytes and
keep dynamic identifiers/time as separate constraints. A missing public hook
request remains Unverified rather than being inferred from settings parsing
or source code. `SubagentStop` is not covered by this fixture.
