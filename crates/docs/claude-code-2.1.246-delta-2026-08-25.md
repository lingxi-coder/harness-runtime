# Claude Code 2.1.245 → 2.1.246 delta audit — 2026-08-25

## Scope

This note only covers the observable delta between the locally installed Claude Code binaries:

- `/Users/luolingfeng/.local/share/claude/versions/2.1.245`
- `/Users/luolingfeng/.local/share/claude/versions/2.1.246`

The follow-up audit used the now-published 2.1.246 release and changelog as the
behavior oracle in addition to the local native binary:

- <https://github.com/anthropics/claude-code/releases/tag/v2.1.246>
- <https://code.claude.com/docs/en/changelog>

## Evidence

| Probe | 2.1.245 | 2.1.246 | Result |
| --- | --- | --- | --- |
| `--version` | `2.1.245 (Claude Code)` | `2.1.246 (Claude Code)` | version bump on the CLI surface |
| normalized `--help` | SHA-256 `71ad650f59e08ae40ede14c534db4f49d8590ee5a4f92f6da2882d3a5560fea6` | same | byte-identical help surface |
| help byte length | `16890` | `16890` | identical |
| help diff | empty | empty | no new top-level CLI flags/commands proven |
| binary type | Mach-O arm64 | Mach-O arm64 | same platform class |

Additional binary metadata:

- `sha256sum` 2.1.245: `9f7c2260251765a18d0b35198669dacc1912f6e8129a3b01f6b58d93365ff1f1`
- `sha256sum` 2.1.246: `7b09f01cb76a38e0e3a7c47c5d698d382162a5ff26538fc778683770caf9218b`
- file size 2.1.245: `376109392`
- file size 2.1.246: `230824016`

## Findings

### Confirmed

- The public `--help` surface is byte-for-byte identical after ANSI stripping;
  2.1.246 changed runtime behavior rather than top-level CLI syntax.
- The native binary contains the exact interrupted-MCP text documented below
  and a reverse transcript walk that restores only an open plan-mode segment.
- The published changelog identifies concrete fixes that were not visible from
  `--help` probing alone.

### Orchestrator parity matrix

| 2.1.246 behavior | LingXi owner | Audit result |
| --- | --- | --- |
| Interrupted MCP calls return an explicit interrupted error instead of an empty success | `orchestrator::streaming_executor` and streaming driver | Ported with the binary-extracted model-facing text and MCP-specific denial classification |
| MCP tools whose input schema is literal `{}` receive decoded JSON values instead of JSON strings | `tool-mcp::MCPTool` | Ported at the per-tool dispatch boundary; non-empty schemas and malformed JSON remain byte-stable |
| Resuming a transcript whose latest trustworthy plan segment is open re-enters plan mode when no explicit launch mode overrides it | `orchestrator::resume`, CLI seed, bridge, mobile host | Ported using the binary's reverse-walk precedence (permission mode, `/plan`, successful Enter/ExitPlanMode, and plan attachments) |
| Invalid persisted tool blocks do not cause a provider 400 on resume | `llm-runtime` request normalization | Already covered by `ensure_tool_result_pairing`; no orchestrator change required |
| User JSONL rows carry the live `permissionMode` in canonical head-field order | transcript writer and session JSONL schema | Ported so future resume can recover the latest explicit mode without parsing prompt text |

The changelog's third-party `tool_use` rendering-without-id fix belongs to the
client rendering boundary, and `/cd` catalog reload belongs to command/settings
lifecycle. They were reviewed as scope boundaries but are not evidence for a
turn-driver or transcript behavior change.

### Unconfirmed / noisy string deltas

The raw binary string inventory differs substantially, but the differences are dominated by bundled runtime and generated code strings. That signal is not strong enough to claim a user-facing behavior change on its own.

### Working conclusion

The original packaging-only conclusion was superseded when Anthropic published
the 2.1.246 changelog. The CLI help surface remained stable, but the release did
contain runtime deltas relevant to MCP interruption, MCP argument typing, and
plan-mode resume. Those deltas are now represented by executable behavior tests
rather than source-string tripwires.

## Related port update

The live parity constant is `platform_api::CLAUDE_CODE_VERSION = "2.1.246"`; the
version-facing test is `test-harness/tests/parity_claude_2_1_246.rs`. Runtime
regressions live beside their owning orchestrator, session, CLI, and MCP code.

## Verification commands

```bash
/Users/luolingfeng/.local/share/claude/versions/2.1.245 --version
/Users/luolingfeng/.local/share/claude/versions/2.1.246 --version
/Users/luolingfeng/.local/share/claude/versions/2.1.245 --help | perl -0pe 's/\e\[[0-9;]*[A-Za-z]//g' > /tmp/claude-2.1.245.help.txt
/Users/luolingfeng/.local/share/claude/versions/2.1.246 --help | perl -0pe 's/\e\[[0-9;]*[A-Za-z]//g' > /tmp/claude-2.1.246.help.txt
diff -u /tmp/claude-2.1.245.help.txt /tmp/claude-2.1.246.help.txt
sha256sum /Users/luolingfeng/.local/share/claude/versions/2.1.245 /Users/luolingfeng/.local/share/claude/versions/2.1.246
```
