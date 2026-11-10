# MCP and tool-hook contracts — Claude Code 2.1.322

## Reference

The public npm `latest` metadata resolved to `@anthropic-ai/claude-code@2.1.322`
on 2026-11-09. The installed macOS ARM64 executable reports that same version:

```text
SHA-256 a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450
```

The matching official `claude-code-darwin-arm64` archive was downloaded and
verified against npm's SHA-512 integrity value. Its executable matches the
installed file byte for byte; `outputs/official-baseline.json` retains the
archive/member hashes and package URL.

The checked-in oracles reject any other executable. They extract exact byte
ranges from that executable and run the selected functions in an isolated
JavaScript context. Fixture inputs and expected JSON spellings remain source
projections, including lone UTF-16 surrogates. This is a finite contract check,
not a claim that the entire MCP implementation or hook lifecycle is identical.

## Changes

- `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequest`, and
  `PermissionDenied` carry optional `mcp_server: {name, source}` last in their
  payloads. Common fields precede `hook_event_name`. Provenance comes from the
  configured server record, independently of normalized tool names and scoped
  registry keys. Builtins omit it. Explicit host source wins; trusted plugin
  loading stamps `plugin`; SDK transports use `sdk`; other servers keep their
  scope. A plugin-authored agent's inline server remains `agent`.
  Native selected-route effort is resolved through the host's existing capped
  effort snapshot. Failure hooks carry an explicit interruption boolean,
  including `false` for ordinary failures.
- MCP tool discovery retains the original description. Inline prompts,
  keyword search, auto-threshold counting, and server instructions use 4,096
  UTF-16 units; a tool actually loaded through ToolSearch uses 16,384. Both
  append `… [truncated]` outside the budget and remove a high surrogate at the
  cut. The existing branded environment override applies to both budgets.
  Loaded descriptions enter Mod `tool.describe` before customization, with
  separate cache entries for the two modes. Discovery cache schema 4 rejects
  older entries whose descriptions were already shortened.
- `mcp_tool` hook templates follow the native ASCII placeholder grammar,
  null/missing handling, array property lookup, JSON rendering, and JavaScript
  prototype behavior. Ordinary paths retain the iterative source mapper;
  prototype-sensitive cases use the existing capability-free QuickJS sandbox.
- Hook output validation follows the native event-specific union. Falsy
  `updatedMCPToolOutput` values are suppressed. Within one hook,
  `updatedToolOutput` takes precedence; across hooks, eligible rewrites apply
  in completion order. A later invalid rewrite retains the prior valid result.
  Legacy MCP rewrites do not affect builtin tools. Hook matchers use the
  current public aliases, including `Task` → `Agent`.

## Reproduction

```sh
node scripts/tests/mcp_322_oracle.mjs /absolute/path/to/claude
node scripts/tests/hooks_322_oracle.mjs /absolute/path/to/claude
python3 scripts/tests/mcp_hooks_322_live.py /absolute/path/to/claude --output /absolute/new/output
cargo test --locked -p mcp -p tool-mcp --lib -- --test-threads=1
cargo test --locked -p hooks -p tool-api --lib
cargo test --locked -p orchestrator --lib native_322 -- --test-threads=1
cargo test --locked -p mcp --test mock_mcp
cargo check --locked -p harness-runtime --features desktop --lib
```

Accepted gates:

| Gate | Result |
| --- | --- |
| MCP library / mock-server integration | 736 / 28 passed |
| MCP tools | 170 passed; final description regression repeated successfully |
| Hooks | 860 passed, 1 ignored; 5 final fixtures passed, including the added interruption case |
| Tool API | 312 passed |
| Orchestrator hooks, descriptions, and Mods | 44 distinct tests passed |
| Desktop runtime | `cargo check` with `desktop` passed |
| Native CLI | 6 isolated loopback cases passed |

The final fixture reruns overlap the broader suites. The initial sandbox run
could not bind OAuth callback ports; rerunning with localhost access resolved
those failures. MCP's shared telemetry capture also required serial test
execution. Neither required a product change.

The native live runner uses isolated settings, fake credentials, a loopback
model API, and a local stdio MCP fixture. Six cases cover success, logical
error, JSON-RPC error, connection closure, hook-approved permission requests,
and ToolSearch discovery. It records actual MCP requests/responses, hook stdin,
and model requests. All six passed on the reference executable. The observed
description sizes are 4,109 and 16,397 units including the suffix. Builtin
ToolSearch hooks omit provenance; MCP hooks retain the raw `wire audit` config
key while the advertised tool is named `mcp__wire_audit__echo`.

The local task evidence is in `artifacts/mcp-hooks-322-20261109/` in the parent
workspace: `work/npm-latest.json`, exact source-range receipts under
`work/{mcp,hooks}/`, the native `work/live/run4/report.json`, Rust gate logs,
and `outputs/rea-evidence.json`. REA's retained static records cover selected
extracted helpers, not execution of the complete executable.

## Limits

Live SDK, HTTP/SSE, and auto-classifier `PermissionDenied` paths were not
exercised. The five payload serializers and scope/source precedence have
fixture coverage. Hook provenance itself preserves arbitrary UTF-16 strings;
the wider MCP configuration catalog still represents authored server names
as Rust strings. Prototype-sensitive interpolation inherits the existing
1-second/8-MiB sandbox limits and reports evaluation failures before invoking
an external MCP tool. Wider lifecycle contracts such as hook-implemented
WorktreeCreate, internal permission cards/cloud policy, and feature-gated
scratchpad fields are outside this change. Reconnect, OAuth, and uncertain-call
recovery are not covered by this byte-contract acceptance.
