# Compact alignment with Claude Code 2.1.261

## Reference and scope

The official npm `latest` tag resolved to **2.1.261** on 2026-09-04 and was
rechecked on 2026-09-05. The
installed CLI was 2.1.260; this change uses the downloaded 2.1.261 release,
not the older `claude-code/src` checkout.

- [Official release package](https://registry.npmjs.org/@anthropic-ai/claude-code/2.1.261)
- [Native macOS ARM package](https://registry.npmjs.org/@anthropic-ai/claude-code-darwin-arm64/2.1.261)
- Native binary SHA-256: `5efecaff231b798be3c66def9be54183623b328b80eaef17f93c43987024e82a`
- The native package's SHA-512 npm integrity was checked before extraction.

The target is the supported manual, proactive automatic, and reactive compact
paths, their model-facing text, and CLI/desktop command and transcript contracts.
Runtime UUIDs, timestamps, measured durations, and model-generated summaries are
not expected to be identical across independent runs.

## Changes

| Surface | Corrected behavior |
| --- | --- |
| Prompt text | Entire base/custom prompt and continuation goldens; ECMAScript trim, replacement-string expansion, and empty transcript-path handling |
| Summary fork | Parent tools and ordering, provider profile, output budget, thinking and effort; retain typed API errors and first-text-block selection |
| Manual/reactive retry | Preserve the oldest summary prefix; move increasing API rounds into the preserved tail after overflow; keep the original history when summarization fails |
| Automatic summary | Separate full-history automatic compaction from manual/reactive tail-preserving compaction |
| Summary validation | Reject empty/whitespace-only responses; PostCompact receives the trimmed original summary rather than the continuation wrapper |
| Token estimates | UTF-16 lengths, per-block rounding, nested tool-result media, and exact UTF-16 sidecars |
| Microcompact | Exclude persisted-output previews; report the distinct IDs actually cleared; share estimates with context hints |
| PTL markers | Only recognize internal meta markers, preserving identical user-authored text |
| Window controls | Oracle environment parsing/clamping and model-window blocking threshold |
| Restored context | Read-result rendering and structured deduplication; stable skill insertion order; latest skill preamble and reminder wrappers |
| SDK/desktop | Compact lifecycle/boundary metadata, preserved history replay, synthetic compact output, and Desktop `/compact <instructions>` dispatch |
| Transcript GC | Handle continued-in, cost-state, and artifact-comment-monitor according to their current retention policies |

The change reuses existing parsing, token estimation, request and Read rendering
helpers. It removes the destructive main-request head-truncation loop and a
duplicate context-hint estimator. No dependencies were added.

Main implementation entry points:

- [`compaction/src/autocompact.rs`](../compaction/src/autocompact.rs),
  [`prompt.rs`](../compaction/src/prompt.rs),
  [`thresholds.rs`](../compaction/src/thresholds.rs),
  [`microcompact.rs`](../compaction/src/microcompact.rs), and
  [`post_compact.rs`](../compaction/src/post_compact.rs).
- [`orchestrator/src/conversation/compaction.rs`](../orchestrator/src/conversation/compaction.rs),
  [`model.rs`](../orchestrator/src/conversation/model.rs), and
  [`turn_loop.rs`](../orchestrator/src/turn_loop.rs).
- [`sidequery/src/forked_agent.rs`](../sidequery/src/forked_agent.rs),
  [`provider_side_query.rs`](../sidequery/src/provider_side_query.rs), and the
  [`Anthropic codec`](../llm-runtime/src/providers/anthropic.rs).
- [`apps/cli/src/run.rs`](../apps/cli/src/run.rs),
  [`stream_json.rs`](../apps/cli/src/stream_json.rs),
  [`stream_json_input.rs`](../apps/cli/src/stream_json_input.rs), and
  [`session/src/jsonl/transcript_compact.rs`](../session/src/jsonl/transcript_compact.rs).
- [`Desktop command dispatch`](../../clients/electron/src/renderer/bridge/desktopCommands.ts)
  and its bridge callbacks. Shared DTO callers received the corresponding
  optional fields; regression tests cover each changed boundary.

## Reproducing the reference

From `lingxi-code`, with the pinned binary supplied locally:

```sh
node scripts/compact_prompt_oracle.mjs /path/to/2.1.261/claude --check
python3 scripts/compact_live_oracle.py /path/to/2.1.261/claude --output /tmp/compact-oracle
```

The first command evaluates only the pinned release's pure prompt functions
and compares the checked-in fixture. The second runs the real CLI with a fresh
configuration, fake key, and loopback API. It does not use the user's account
or incur model charges. Its request/response evidence is written to the output
directory rather than checking entire upstream system prompts into the repo.

Live reference scenarios exercised: successful `/compact focus` followed by
continuation, empty summary, BOM/whitespace summary, too few messages, no
messages, and a prompt-too-long summary retry. The successful request preserved
the parent's tool definitions, adaptive thinking, high effort and 32,000-token
budget. The retry preserved the oldest prefix and moved more rounds into the
tail. Replay-enabled and replay-disabled command output were both inspected.

## Verification

Final validation on 2026-09-05:

- `cargo test -p compaction -p orchestrator -p command-core --all-features --no-fail-fast`:
  **2,074 passed, 0 failed, 2 ignored** across 91 test/doc-test executables.
- `cargo test -p protocol -p platform-api -p session -p sidequery -p llm-runtime --all-features --no-fail-fast`:
  **2,006 passed, 0 failed** across 65 test/doc-test executables.
- `cargo test -p cli --lib compact`: **15 passed, 0 failed**, including SDK
  boundary/status ordering, replay and failed-command result text.
- Desktop slash dispatch: **16 passed**; both Node and web TypeScript checks passed.
- Changed Rust files: `rustfmt --check` passed for all 45 files; `git diff --check` passed.
- Production Clippy passed for the seven affected crates with `--all-features
  --lib --bins`; existing repository warnings remain.
- Repository gates: **9 of 10 passed**. The sole failure is the pre-existing
  brand-check finding set described below.
- Regenerated prompt oracle: exact fixture match, including Unicode trim and
  replacement-token edge cases.
- Live release probes: success, empty, too-few, no-messages, whitespace and
  prompt-too-long all passed against the loopback mock.

Evidence is saved in `.omx/logs/compact-2.1.261/` at the repository root.
The earlier broad CLI test run exposed its signed-broker startup dependency
and an existing model-capability assertion; it was not reported as a clean
full-workspace run. Likewise, the repository-wide brand check reports 21
findings, all of whose flagged source lines were verified to exist in `HEAD`
before this task's edits (`brand-baseline-check.json`). Repository-wide format
checking also found unchanged mobile/task files, and all-target Clippy found
an existing `never_loop` test in `apps/cli/src/commands/agents.rs`.

## Limits of the evidence

- Live oracle probes cover bare manual compaction on Sonnet. Automatic and
  media-error paths are verified through extracted release logic and local
  regression tests, rather than paid model calls.
- Private client-data/experiment controls, precomputed/background compaction,
  and optional summarize-all fallback are not established as equivalent by
  these probes. Metadata support alone is not an implementation of those
  optional services.
- The local message representation does not carry upstream virtual/resumed
  assistant-round flags. Grouping parity applies to representable messages.
- Restored files use the existing bounded UTF-8 reader. The full Read tool's
  image/PDF/notebook dispatch and server token-counting behavior are outside
  that reader's implementation.
- Skill restoration preserves exact UTF-16 output, but the persisted skill
  deduplication index still uses display text; split-surrogate collisions are
  not established as equivalent.
- The compact command reuses the general CLI accounting serializer. It does
  not add upstream `subagent_stats`, `thinkingTokens`, or `costBasis` fields
  for which that serializer has no backing accounting state.
- A raw macOS LingXi CLI cannot complete startup without its signed
  Credential Broker. Real Claude Code was probed against the loopback API;
  LingXi's corresponding request, event and persistence paths were exercised
  through local tests, without bypassing the broker or changing credentials.

This is evidence for the contracts above, not a claim that every feature-flag
combination or all nondeterministic output bytes are globally identical.
