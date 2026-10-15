# Child tool result termination in Claude Code 2.1.286

The child runner now honors trusted successful tool result termination. The
registry invoker computes the control before lowering a result to JSON/text,
and the core `ToolInvocationResult` carries its `Tool` or `McpMeta` source.
Ordinary model input and result data cannot supply this control. Error results
never end the query, and an error or absent marker does not erase an earlier
accepted marker. The final accepted marker in the issued batch supplies the
source, matching native `Sr`.

The runner completes every issued tool call and appends tool results, Read
instructions and diagnostics before ending the query. It suppresses refusal
fallback, truncation/schema nudges and another notification-driven model query.
The outer StructuredOutput contract still fails when no required output was
captured. Existing foreground ownership/parking and persistent-agent wakeups
remain active; each new batch resets its termination control.

## Native evidence

The binary SHA is
`75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433`.
`scripts/tests/tool_turn_end_286_oracle.mjs` executes the complete native
`wEt`/`vEt`/`dEo` source without dependency substitutions or source rewrites.
Its byte slice is `[185265466,185265767)`, extracted source line 1528 in
`src_183727495.js`, with source SHA
`50f625eb5bc7534891b6f205d09e20b7a937a4f6bf0c0d6b5284752a3eca4296`.

```sh
node scripts/tests/tool_turn_end_286_oracle.mjs /path/to/2.1.286 --check
```

The saved fixture has 20 MCP-marker checks and 58 raw-frame checks. Only 28 of
the raw frames map to the dispatcher's single-result boolean domain; the other
30 retain their native observations without coercion or claims of typed runtime
coverage. Together, the Rust helper driver consumes 48 native observations.
MCP metadata requires the nested `claude/endTurn` value to be strictly true.
Tool control takes precedence over MCP metadata on the same successful result.

Native query `Sr` assigns each accepted source at `src_191662485.js:48`, character
108579. Its accepted terminal branch at characters 165866–166305 follows
abort/deferred/prevent-continuation handling and skips natural enforcement
nudges. The outer workflow schema check is independently visible at
`src_196199764.js:75`, character 71297.

## Verification and remaining boundaries

The Agent library's 566 tests pass, including seven new runtime regressions
capturing real runner API requests, detailed dispatch, immediate JSONL reads
before completion, and terminal events. They cover both sources, trailing
Read/Write calls, failed/untrusted signals, last accepted source, refusal and
schema behavior, a later real message, and owned-background notification wakeup.
The registry invoker's actual dispatch tests also pass.

Successful transcript writes are ordered before completion. The existing
writer remains best-effort on I/O failure; this does not establish strong
durability when an append fails. Child telemetry currently retains the source
through tracing, without the main loop's AnalyticsBus/query-chain attribution.
Native PostToolBatch, forced Stop/SubagentStop and turn-end reactions ordering
still needs a complete child-hook port. No synthetic native JSONL row was added.

This fixes the general child termination boundary. It does not implement
SubagentHandback injection, classifier-only review, report admission, enforcement,
recipient transfer, or report persistence. Those remain in
`subagent-handback-2.1.286.md`.
