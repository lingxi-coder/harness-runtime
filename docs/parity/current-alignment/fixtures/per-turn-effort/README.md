# Per-turn effort and cold resume

`run-native.py` uses a pinned 2.1.293 or 2.1.295 public CLI, a dummy API key and an
isolated loopback server. The first process uses `--effort low`; a second
process resumes the same session with `--effort high`. Both processes exited
successfully and each sent exactly one Messages request in both separately
captured versions. The response body is
the existing fixed HTTP-layout fixture (including its fixed response ID).

```sh
CLAUDE_NATIVE_BINARY=/Users/luolingfeng/.local/share/claude/versions/2.1.295 \
  python3 docs/parity/current-alignment/fixtures/per-turn-effort/run-native.py
```

In both versions, the second request retains the earlier `low` system marker before
the earlier assistant and adds a `high` marker after the new user message.
The markers merge into existing model-facing system context. Their content
is not necessarily an empty array. Native JSONL assistant rows retain
`perTurnEffort` as `low` and `high` respectively.

The committed `*.raw.json` and `*.raw.jsonl` files are original source slices,
not reconstructed JSON. Full requests, process output and the original
session remain in the receipt's isolated run directory. Both processes
emitted an informational gateway billing notice on stderr; no credentials
or real provider requests were used. Reruns create a fresh temporary run and
its `curated/<version>/` directory; checked-in captures are never overwritten.
The original 2.1.293 receipt retains its original capture-runner hash; the
2.1.295 runner also supports fresh, version-checked replay of that older input.

This is **Native evidence only**, not a passing Harness comparison. It proves
the current request-only tail marker is insufficient for cold history and
for merging with an existing API system row. Environment path/date and row
IDs/timestamps are observed dynamic fields, not fixed-byte acceptance.

The committed reference implementation at Harness `3f27b673cab41924fc9f0c165b6c5c1a33f3d792`
provides useful producer locations; it is not a substitute for this runtime
capture:

- Core `types/messages.rs`: admitted `Assistant.per_turn_effort`, structured
  `api_system`, and `project_per_turn_effort`.
- llm-runtime `service.rs` and `history.rs`: capture the admitted request
  effort on successful responses, including streaming and fallback paths;
  project historical effort before SDK encoding.
- Orchestrator `streaming_loop.rs`, `turn_loop/visible_response.rs` and
  `conversation/transcript.rs`: preserve the host-owned value in accepted
  assistant rows and Native JSONL.
- Orchestrator `resume.rs`: restore that value and structured API system
  rows. Provider output must not manufacture this trusted request metadata.

Integration must rebase these contracts onto current Source carriers rather
than copy the older `serde_json::Value` implementation or unrelated changes.
