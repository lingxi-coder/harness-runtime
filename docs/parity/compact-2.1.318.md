# Compaction alignment with Claude Code 2.1.318

Baseline checked on 2026-11-03: the official 2.1.318 executable has SHA-256
`677a2ea7bf576cedd20e191d973914ed8a41796f53a63b1923381ddc4d5b5e7d`.

## Alignment boundary

The core prompt, response selection, summary formatting, common Desktop
continuation text, and default split/retry behavior are checked against the
extracted native functions. The fixtures cover 6 prompt cases, 8 formatting
cases, 5 continuation cases, 5 response-selection cases, and 21 split/retry
traces. Eight isolated mock-provider scenarios were also exercised with the
unmodified official executable. These checks do not assert identical complete
provider requests, model outputs, or optional native REPL/Artifact environments.

The reproducible oracles and fixtures are:

- `scripts/tests/compact_318_oracle.mjs`
- `scripts/tests/compact_318_fallback_oracle.mjs`
- `scripts/tests/compact_live_oracle.py`
- `crates/compaction/tests/fixtures/claude_2_1_318_compact.json`
- `crates/compaction/tests/fixtures/claude_2_1_318_compact_fallback.json`

## Intentional project differences

1. Append `COMPACT_SUMMARY_PROVENANCE_GUIDANCE` to the summary prompt. This
   preserves human intent and evidence without turning historical skill material
   into a fresh user request or approval. The official core bytes remain intact.
2. Exclude only instruction rows carrying the host's `skill_activation` origin
   from the summarizer's view. Ordinary user messages, assistant messages, quoted
   evidence, and tool results remain eligible summary input. The source history
   is retained.
3. Host compaction entry points select Native's existing bounded API-attempt
   path: five physical dispatches per operation, while idle compaction retains
   its existing one-attempt budget. A nonempty untagged manual/reactive reply is re-asked
   using the native trailer. If validation still fails, the operation fails and
   committed history remains unchanged. The core library's unbudgeted legacy
   behavior remains available and oracle-tested; project entry points do not use
   its acceptance of an untagged fallback reply.

The third difference was explicitly selected after live Luna testing reproduced
an untagged short answer being accepted as a summary by the legacy path. Tests
cover exact history/message-ID preservation, shared attempt scope, and absence
of a successful compact commit after failure.

## Information retention

Prompt-byte alignment does not guarantee that different models preserve every
fact. A valid summary can omit an older dictionary entry or tool-result detail.
[Claude Code's documentation](https://code.claude.com/docs/en/how-claude-code-works#when-context-fills-up)
also describes this limitation. Use native compact focus instructions for facts
that must be retained; the continuation carries the full transcript path for
recovering details. Rejecting untagged summaries protects against that failure
class, not against every semantic omission in a well-formed summary.
