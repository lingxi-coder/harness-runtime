# LingXi Code vs Claude Code 2.1.252 — parity audit residual ledger
<!-- markdownlint-disable MD013 MD060 -->

> Audit date: 2026-09-01
>
> Oracle: `/Users/luolingfeng/.local/share/claude/versions/2.1.252`
>
> Oracle SHA-256: `b661c6a094fcc32656bf7c0071c5b45bf900b34d4f0a1ab3d78fd59aeba2c2c7`
>
> Secondary oracle for comparison: `/Users/luolingfeng/.local/share/claude/versions/2.1.251`
> Secondary SHA-256: `625869b01e0050f260b2980fac248fd9cef9e462612bded4ec9d3d49ff8969a5`

This document is read-only evidence, not implementation commentary. It records
what is already wired, what is explicitly deferred, and what still blocks full
workspace verification.

## 1. Scope and classification rules

This audit covers the current uncommitted tree against the local Claude Code
2.1.252 oracle and only classifies three buckets:

- implemented and verified in-tree;
- explicitly deferred because the prerequisite substrate is absent or the
  feature is outside this branch’s agreed scope;
- still pending verification because a build/test blocker remains.

It does not treat project-specific exclusions or explicit deferrals as open
engineering gaps.

## 2. Changelog-facing fixes already present in this branch

These are the four changelog items that belong in the 2.1.252 parity ledger.
Three are already wired; one is an explicit deferred boundary.

| Surface                                                 | Status              | Evidence |
| ------------------------------------------------------- | ------------------- | -------- |
| Bash task-output swap false failures                    | Implemented         | [`tasks/src/output_manager.rs`](../tasks/src/output_manager.rs:23), [`tasks/src/output_manager.rs`](../tasks/src/output_manager.rs:150) |
| Always-allow persistence when `.claude/settings.local.json` is absent | Implemented | [`apps/engine-desktop/src/lib.rs`](../apps/engine-desktop/src/lib.rs:730), [`apps/cli/src/init.rs`](../apps/cli/src/init.rs:1293), [`permission/src/persist.rs`](../permission/src/persist.rs:26) |
| Remote Control weak-network behavior                    | Deferred, explicit  | [`sandbox/src/wrap.rs`](../sandbox/src/wrap.rs:211), [`sandbox-runtime/src/macos.rs`](../sandbox-runtime/src/macos.rs:499) |
| Very large background failure notifications             | Implemented         | [`orchestrator/src/prompt/task_notification.rs`](../orchestrator/src/prompt/task_notification.rs:1) |

Notes:

- The weak-network Remote Control item is a deliberate deferred surface, not a
  missing local implementation.
- The earlier `XaaError::Prm(_)` compile blocker is no longer the reason
  workspace verification is pending.

## 2.1 Additional closed gaps

These are real parity surfaces already closed in this branch, but they are not
part of the four changelog items above.

| Surface                                  | Status      | Evidence |
| ---------------------------------------- | ----------- | -------- |
| `agents --restricted` parser/help parity | Implemented | [`apps/cli/src/commands/agents.rs`](../apps/cli/src/commands/agents.rs:27), [`apps/cli/tests/cli_agents.rs`](../apps/cli/tests/cli_agents.rs), [`test-harness/src/parity/fixtures/cc_2_1_252_agents_help.txt`](../test-harness/src/parity/fixtures/cc_2_1_252_agents_help.txt) |
| MCP `add` parser variadics and callback-port string parsing | Implemented | [`apps/cli/src/commands/mcp.rs`](../apps/cli/src/commands/mcp.rs:1) |
| PERM-02 permission persistence row copy | Implemented | [`tui/src/bottom_pane/permission_view.rs`](../tui/src/bottom_pane/permission_view.rs:112) |
| `prompt_suggestion` frame pipeline   | Implemented | [`apps/cli/src/argv.rs`](../apps/cli/src/argv.rs:688), [`apps/cli/src/run.rs`](../apps/cli/src/run.rs:83), [`apps/cli/src/stream_json.rs`](../apps/cli/src/stream_json.rs:699), [`orchestrator/src/conversation/model.rs`](../orchestrator/src/conversation/model.rs:685) |

`agents --restricted` is exact at parse/help time, but the flag is intentionally
inert for existing connect/resume flows because the upstream behavior here is
the new-session FleetView dispatch path, and that dispatch remains absent in
this branch.

## 3. Explicitly deferred boundaries

These are intentionally not counted as remaining engineering gaps in this
branch:

- Remote Control / CCR / private cloud.
- Missing internal `session_context` substrate.
- Absent slow-mode controller.
- Absent unified-grace controller.
- Desktop/CLI central reconcile substrate.
- Unavailable prompt-suppression state buckets.
- Trusted first-party raw-name allow gate.
- Upstream query-observer abort marker.
- New-session FleetView restricted dispatch.
- Separate slow-mode / unified-grace / UI notice.
- Eleven substrate-private MCP analytics events.

The MCP analytics names are upstream-real, but they are deferred because their
prerequisite substrate is absent here. The audit fixture and telemetry module
both intentionally record them as deferred, not as unexplained missing wiring:

- `tengu_mcp_instructions_pool_change`
- `tengu_mcp_dropped_tools_pool_change`
- `tengu_mcp_skills_funnel`
- `tengu_mcp_arg_trailing_invoke_suffix`
- `tengu_mcp_description_contains_toolcall_xml`
- `tengu_mcp_dialog_choice`
- `tengu_mcp_multidialog_choice`
- `tengu_mcp_proxy_needs_approval_retry`
- `tengu_mcp_registry_fetch`
- `tengu_mcp_sdk_generation`
- `tengu_mcp_tripwire`

For the current branch, these names are best understood as upstream parity
targets that require missing substrate. They should not be reopened as ordinary
open engineering work while that substrate is out of scope.

## 4. Evidence that the deferred surfaces are deliberate

The current tree and fixtures already say this explicitly:

- [`telemetry/src/tengu/mcp.rs`](../telemetry/src/tengu/mcp.rs:39) states that
  `telemetry::tengu::mcp::NAMES` intentionally covers 44 of the oracle’s 55
  provider-neutral MCP analytics names, and that the remaining 11 are an open
  parity gap only in the oracle-surface sense, not a claim that the branch is
  expected to implement them right now.
- [`test-harness/src/parity/fixtures/tengu_events.json`](../test-harness/src/parity/fixtures/tengu_events.json:3)
  mirrors that accounting and explicitly labels those 11 as absent.
- [`orchestrator/src/conversation/reminders.rs`](../orchestrator/src/conversation/reminders.rs:309)
  and [`orchestrator/src/prompt/task_notification.rs`](../orchestrator/src/prompt/task_notification.rs:1)
  show the task-notification path is already separate from the deferred
  streaming-output-delta surface.

## 5. Verification evidence

Root verification completed successfully:

- `cargo test --workspace --all-features --no-fail-fast` — exit 0, including
  integration and doc tests
- `.githooks/pre-commit` — exit 0, with 7/7 discovered gates passing and the
  trigger check passing
- `cargo build -p cli --bin lingxi-cli` — exit 0
- `parity_surface.py` self-check — 58 paths / 129 long flags, oracle 89 flags,
  0 missing in the port
- `parity_behaviour.py` — 12 identical / 0 differing, and write probes 5
  identical / 0 differing

Supporting evidence, not a substitute for the root runs:

- `cargo test -p telemetry --test mcp_schema_test -- --nocapture`

## 6. Project-specific exclusions

The following are still excluded from the parity ledger and should remain in a
separate project/range decision bucket:

- provider-specific integrations that are not oracle parity targets here;
- product naming / branding decisions;
- any local app / fleet-view semantics that belong to the separate local-app
  lane;
- any private-cloud or remote-control substrate that is outside this branch.

## 7. Read against the working tree

The current uncommitted diff should be read alongside:

- [`apps/cli/src/commands/agents.rs`](../apps/cli/src/commands/agents.rs)
- [`apps/cli/src/commands/mcp.rs`](../apps/cli/src/commands/mcp.rs)
- [`apps/cli/src/argv.rs`](../apps/cli/src/argv.rs)
- [`apps/cli/src/run.rs`](../apps/cli/src/run.rs)
- [`apps/cli/src/stream_json.rs`](../apps/cli/src/stream_json.rs)
- [`orchestrator/src/conversation/model.rs`](../orchestrator/src/conversation/model.rs)
- [`orchestrator/src/conversation/reminders.rs`](../orchestrator/src/conversation/reminders.rs)
- [`orchestrator/src/turn_loop.rs`](../orchestrator/src/turn_loop.rs)
- [`telemetry/src/tengu/mcp.rs`](../telemetry/src/tengu/mcp.rs)
- [`test-harness/src/parity/fixtures/tengu_events.json`](../test-harness/src/parity/fixtures/tengu_events.json)

Those files are the current evidence trail for this parity pass.
