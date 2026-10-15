# Claude Code Mods 2.1.288 alignment handoff — 2026-10-03

## Scope and checkout

This branch aligns **Mods functionality**, not the generic MCP implementation, with Claude Code 2.1.288. The latest release used for this pass is [2.1.288](https://github.com/anthropics/claude-code/releases/tag/v2.1.288); the public [Mods reference](https://code.claude.com/docs/en/plugins/mods/reference) still labels its tables through 2.1.287. Detailed feature status and evidence live in [the contract matrix](claude-mods-contract-matrix-2.1.288.md) and [the implementation ledger](claude-mods-2.1.288.md).

| Repository | Isolated worktree | Branch | Starting HEAD |
| --- | --- | --- | --- |
| Harness | `/Users/luolingfeng/lingxi/.worktrees/claude-mods-parity/harness-runtime` | `codex/claude-mods-parity` | `4dfd9abef1dc293a9617d35387165cb2256b5d98` |
| Product | `/Users/luolingfeng/lingxi/.worktrees/claude-mods-parity/lingxi-app` | `codex/claude-mods-parity` | `4c4f77860280401aaf92d1be9b7826db9e24f6a5` |

Both worktrees are intentionally dirty and contain substantial requested Mods work. Preserve every existing change; there is no requested commit, merge or PR. Product `Cargo.lock` has remained byte-identical to its checkout. Harness `Cargo.lock` is intentionally modified on this branch. Both trees passed `git diff --check` at the handoff preparation check.

## Verified in this pass

| Area | Current implementation and focused evidence |
| --- | --- |
| CLI/TUI build repair | The `pane` local-shadowing E0618 at `apps/cli/tui/src/bottom_pane/mod.rs` was fixed earlier in this continuation. Product TUI serial suite: 1190 passed, one pre-existing ignore. |
| `session.compact` | Native-shaped row codec, real summarizer inside `next(e)`, skip/direct replacement and UUID refresh. Unit 4/4; integration 8/8. Internal assistant usage and tool-result metadata are still absent. |
| `session.append` | Main retained transcript rows are screened before history/JSONL storage with native identity/overlay rules: 3/3; compaction 8/8. Child retained rows and original attachment sidecars dispatch with `agentId`, origin and door, including hook-only runs and retry dedup: three focused agent filters, each 1/1. Runtime model switches may leave assistant origin using the child's initial model. |
| `session.attach` / `session.detach` | Explicit bridge commands and Electron surface lifecycle update the client roster before Mods events. Harness lifecycle 1/1; Product bridge-server 1/1; targeted Electron/bridge-client Node checks passed. Clear/resume retain the roster; end detaches once. |
| `session.measure` | Turn measure follows `turn.complete`; observed rate-limit changes can independently issue Limits measure before completion. Single-flight coalescing, threshold/baseline rules: unit 4/4, idle dispatch 1/1, turn-order regression 1/1. External background limit refresh has no Harness signal. |
| `$.session.usage()` | Native `{breakdown,columns}` validation and live `{startedAt,context,rateLimits,cost}` operation: 2/2. `startedAt` is inferred from `Instant`; native contextData breakdown, gateway spend-limit and unbound-session fallback are not available. |
| `$.ui.selection()` | Live user/assistant requestId maps to persisted JSONL UUID; row identity tests 2/2. Its `ui.selection` operation is interceptable, preserves undefined when empty and rejects modified pinned input: Hooks 1/1. Full terminal interaction and Desktop remain open. |
| `ui.resolve` / `ui.render` / `ui.press` | Terminal `AbovePrompt` Box/Text/Button render, invalidation, keyboard action token and stale unload/reload protection. Harness focused UI/press tests passed; Product TUI 1190 passed, one ignored. Mouse, Desktop, other sites/elements and whole worker-process restart remain open. |
| `agent.offer` | Model-facing AgentTool list and `agent_listing_delta` reminder screened, explicit dispatch retains raw catalog. Agent 6/6, reminder 1/1, tool-agent 1/1. Dynamic remote/agentId/hookCaller context is not available at existing call seams. |
| `plugin.register` scanner | Reachable registered handlers and same-file helpers feed `e.uses`; direct literal env/state classified. Dynamic event names, computed API methods and namespace aliases reject before load. Hooks scanner 6/6; materialize refusal 1/1. Cross-file helper graph and full literal grammar remain open. |
| `$.tool.call()` | One `tool.call` Mod operation with argument rewrite enters the session-visible permissioned core. Hooks middleware 1/1; orchestrator allow/policy-deny 1/1. Consent is an ephemeral user message, not a grant. The API-specific path still drops post-tool-batch calls, injected messages, context modifiers and the SDK result frame; cancellation and Projects consent rules remain. |
| Other Mods slices | `command.describe` cancellation isolation 3/3; immediate command subset 3/3; managed `sec-default` alias 1/1; large nested `agents-md` roots 1/1; child-worktree `tool.call` event rewrite 1/1. Earlier focused checks are recorded in the ledger. |

## Current round at pause

`$.tool.call()` is the last completed implementation lane before the requested pause. Its Hooks test passed 1/1 after final fixture cleanup; its orchestrator allow/policy-deny test passed 1/1. The operation runs the Mods middleware once, then `next(e)` enters the permissioned tool core without recursion. `tool`, `tool_use_id`, `agentId` and consent stay pinned; consent is only an ephemeral human message in the permission context. A result stage isolates transcript side tables and distinguishes `{deny}` from ordinary `{isError:true}`. This verifies a focused local slice, not the full native API: the returned post-tool-batch calls, injected messages, context modifiers and SDK result frame are not applied, and cancellation/Projects consent behavior remains. The event `tool.call` and the separate `$.tool.call()` API must not be conflated.

## Acceptance boundary

The local 2.1.288 executable is `/private/tmp/harness-claude-code-2.1.288-darwin-arm64`; extracted source is under `/private/tmp/claude-code-oracle-2.1.288/src`. Its Mods runtime rollout is **off** in the local profile. `claude plugin validate --json` accepted `docs/parity/fixtures/claude-mods-2.1.288-api` with no errors and recognizes the added API/event names, but that command only scans source. None of the local Rust/Node tests or the static validator proves native runtime byte parity. Do not label any row `B288` until the same Mod fixture has run against an enabled 2.1.288 oracle and raw event/result/error bytes have been compared.

Remaining high-value gaps include full `$.tool.call()` effects/cancellation; `session.send` and several other `$` methods; `engine.create`; the full plugin scanner/repository seat graph; UI sites/elements and interactive controls; bundled `diff`, `telemetry` and `you-should-know` behavior; exact native bytes for the existing partial slices. The matrix lists each missing contract and its next gate. Avoid unrelated generic MCP work; MCP appears in this ledger only where a Mod-facing API or provider value directly affects Mods behavior.

## How to resume safely

1. Recheck both worktrees, branch names, dirty files and Product lockfile before editing. Keep existing changes. Read this handoff, then the matrix and ledger; inspect the focused `$.tool.call()` test result before adding another feature.
2. Finish `$.tool.call()` parity beyond its verified slice: apply or explicitly account for post-tool-batch calls, injected messages, context modifiers and SDK result frame; propagate cancellation and Projects consent rules; test visible/unknown tools, ordinary tool error and missing result through the real permission gate.
3. Continue the P0/P1 rows in the matrix with focused native-source evidence and local tests. Capture a live 2.1.288 Mods oracle when rollout permits; compare byte-level event, result and error JSON. Until then, report static-source alignment and local behavior separately from byte parity.
4. Run one Cargo build at a time. Use `CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 RUST_MIN_STACK=4194304` and `--offline` for focused Harness tests. The Product bridge uses `/private/tmp/lingxi-mods-local-harness-patch.toml`; before any Product Cargo invocation, back up Product `Cargo.lock`, restore it in an EXIT trap, and compare it byte-for-byte afterward. Do not commit a temporary lockfile change.
5. Watch free disk space before linking. This pass safely ran `cargo clean -p hooks --offline` **inside the isolated Harness worktree only**, removing 9.8 GiB of rebuildable target artifacts when free space fell to 1.2 GiB. Avoid broad cleanup or source deletion.

At handoff preparation, the exact worktree paths and local source oracle above were still present. Revalidate the release version and oracle availability when work resumes; both may change.
