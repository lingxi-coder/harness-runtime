# What remains after the 2.1.252 parity pass
<!-- markdownlint-disable MD013 MD060 -->

Audit date: 2026-09-01

This file replaces the stale “engineering items remaining: zero” note. That
statement no longer matches the current tree or the current oracle evidence.

## Current oracle pins

- Claude Code 2.1.252 oracle: `/Users/luolingfeng/.local/share/claude/versions/2.1.252`
- SHA-256: `b661c6a094fcc32656bf7c0071c5b45bf900b34d4f0a1ab3d78fd59aeba2c2c7`
- Claude Code 2.1.251 oracle: `/Users/luolingfeng/.local/share/claude/versions/2.1.251`
- SHA-256: `625869b01e0050f260b2980fac248fd9cef9e462612bded4ec9d3d49ff8969a5`

## Actual 2.1.252 changelog items

These are the four changelog surfaces that belong in the 2.1.252 parity ledger.
Three are already implemented; one is an explicit deferred boundary.

| Surface                                                 | Status             | Evidence |
| ------------------------------------------------------- | ------------------ | -------- |
| Bash task-output swap false failures                    | Implemented        | `tasks/src/output_manager.rs` (rooted file identity pin + swap refusal around spool I/O) |
| Always-allow persistence when `.claude/settings.local.json` is absent | Implemented | `apps/engine-desktop/src/lib.rs`, `apps/cli/src/init.rs`, `permission/src/persist.rs` |
| Remote Control weak-network behavior                    | Deferred, explicit  | `sandbox/src/wrap.rs`, `sandbox-runtime/src/macos.rs` |
| Very large background failure notifications             | Implemented        | `orchestrator/src/prompt/task_notification.rs` |

## Additional closed gaps

These are real parity surfaces already closed in this branch, but they are not
part of the four changelog items above.

| Surface                                  | Status      | Evidence |
| ---------------------------------------- | ----------- | -------- |
| `agents --restricted` parser/help parity | Implemented | `apps/cli/src/commands/agents.rs`, `apps/cli/tests/cli_agents.rs`, `test-harness/src/parity/fixtures/cc_2_1_252_agents_help.txt` |
| MCP `add` parser variadics / callback-port strings | Implemented | `apps/cli/src/commands/mcp.rs` |
| PERM-02 permission persistence row copy  | Implemented | `tui/src/bottom_pane/permission_view.rs` |
| `prompt_suggestion` frame pipeline       | Implemented | `apps/cli/src/argv.rs`, `apps/cli/src/run.rs`, `apps/cli/src/stream_json.rs`, `orchestrator/src/conversation/model.rs` |

`agents --restricted` is exact at parse/help time, but the flag is intentionally
inert for existing connect/resume flows because the upstream behavior here is
the new-session FleetView dispatch path, and that dispatch is still absent in
this branch.

## Explicitly deferred boundaries

These are not being counted as open engineering gaps in this branch because the
prerequisite substrate is absent or the feature is explicitly out of scope:

- Remote Control / CCR / private cloud
- missing internal `session_context` substrate
- absent slow-mode controller
- absent unified-grace controller
- desktop/CLI central reconcile substrate
- unavailable prompt-suppression state buckets
- trusted first-party raw-name allow gate
- upstream query-observer abort marker
- new-session FleetView restricted dispatch
- separate slow-mode / unified-grace / UI notice
- the 11 substrate-private MCP analytics events listed in the audit doc

The deferred MCP events are upstream-real names, but they depend on substrate
that is not present here:

- `instructions_pool_change`
- `dropped_tools_pool_change`
- `skills_funnel`
- `arg_trailing_invoke_suffix`
- `description_contains_toolcall_xml`
- `dialog_choice`
- `multidialog_choice`
- `proxy_needs_approval_retry`
- `registry_fetch`
- `sdk_generation`
- `tripwire`

## Verification evidence

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

## Where to read the full evidence ledger

See [claude-code-2.1.252-parity-audit-2026-09-01.md](./claude-code-2.1.252-parity-audit-2026-09-01.md).
