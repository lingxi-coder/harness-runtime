# Native 2.1.293 registered-tool persistence oracle

This fixture uses the public Native plugin CLI, `$.tool.register`, and the
documented `tool.call` hook filter. A fixed loopback Anthropic Messages stub
asks Native to call the registered tool exactly once. The hook returns a
50,007 UTF-16-unit result: `begin`, one lone U+D800, then 50,001 `z` units.
The next model response is a fixed completion. No real account or upstream
service is used.

Run from any directory with the pinned Native binary explicitly selected:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude python3 /path/to/harness-runtime/docs/parity/current-alignment/fixtures/tool-result-persistence/run-native.py
```

The runner checks Native 2.1.293's version and SHA-256, starts an isolated
HOME/config/cwd and a loopback-only stub, and uses a dummy API key. A fresh
unique run directory is created under ignored `run-output/`; it is not
removed or overwritten. Full model request bodies (which include Native's
system prompt) remain only in that ignored run directory. The runner also
extracts the exact `tool_result.content` JSON value from the second request,
the raw session JSONL row containing the persisted-output envelope, and the
persisted result file. These focused captures retain their original bytes.

The checked-in `native-baseline/` is the 2.1.293 capture. Its receipt gives
hashes, byte sizes, and source/runtime provenance; the exact selected JSONL
row, second-request `tool_result.content` JSON value, plugin call metadata,
stdout/stderr, and persisted file are retained as separate raw artifacts.
The session preview keeps the lone unit as `\\ud800` in JSONL and decodes to
U+D800 in memory. The second Anthropic request serializes that preview as
U+FFFD, while the actual `tool-results` file contains the UTF-8 replacement
bytes `EF BF BD`. Those are three separate observed surfaces; this fixture
does not treat them as a single byte-equality claim. The model-visible
envelope also embeds a machine-local persisted-output path, so full
tool-result-envelope byte comparison requires a fixed run path and session
identity.

The `unicode-boundary` case uses the same public CLI and registered tool:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude NATIVE_TOOL_RESULT_CASE=unicode-boundary \
  python3 /path/to/harness-runtime/docs/parity/current-alignment/fixtures/tool-result-persistence/run-native.py
```

It returns 50,007 UTF-16 units with one U+1F600 pair beginning at unit 1,999,
so the 2,000-unit preview boundary falls between its high and low surrogates;
a second U+1F600 pair appears in the tail. `native-baseline/unicode-boundary/`
preserves the raw selected JSONL row, exact model-facing `tool_result.content`
JSON value, full persisted file bytes, hook metadata, stdout/stderr and receipt.
The Native session preview ends in U+D83D; request serialization exposes
U+FFFD at that position. The persisted file retains both complete four-byte
UTF-8 emoji sequences, with no replacement bytes. Neither preview includes an
explicit omitted-character count; the raw envelope still includes its ordinary
ellipsis. The JSONL row embeds an isolated-run file path, so its full bytes are
evidence, not a cross-run golden unless the run path is also fixed.

This Native oracle is not a Harness pass. Candidate comparison must use the
same registered-tool input and compare (a) source preview UTF-16 units, (b)
Claude request-body serialization, and (c) persisted UTF-8 file bytes as
separate assertions. The candidate integration gate
`crates/orchestrator/tests/current_alignment_tool_result_persist.rs` now runs
both registered-tool source cases through production `persist`/`wrap_utf16`
and sends the boundary envelope through `ApiService`'s Anthropic SDK request
path with a fixed local transport. It asserts preview units, final request
projection and persisted bytes separately. E owns its Cargo run; this is not a
full Agent/tool/CLI replay, and the candidate test is not marked passed before
that run. The 50,000-unit
`tool_result_text::normalized_tool_result_projection` JSON-string
truncation/omitted-count contract is a separate path and is measured by the
Native cold-resume oracle in
`../tool-result-json-resume/native-baseline/cold-resume-object/`; it is not
covered by this 2,000-unit persistence-preview fixture. Candidate Cargo
verification remains pending.
