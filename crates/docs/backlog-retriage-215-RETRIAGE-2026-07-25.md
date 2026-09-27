# backlog-retriage-215 — re-triage, 2026-07-25

Re-verification of the 21 items `backlog-retriage-215-2026-07-20.json` lists as
`open`, against `main` @ `10dbf65fe`.

That document's `verify` fields are **binary-side** — they confirm the ORACLE
has each feature. They do not say whether the port does. Every judgement below
is port-side, checked at the behaviour site the audit itself cited.

Net: **18 of 21 are done. 3 remain, and 2 of those are explicitly deferred
by design.**

## Closed (18)

Each confirmed at the site the audit named as missing:

| id | where it landed |
|---|---|
| `SRC-01` | `permission/src/policy.rs:50-51` — `SOURCES_BY_PRIORITY` now walks `ToolsNarrowing` + `McpServerPolicy` |
| `SRC-05` | `permission/src/shadow.rs:105-106` — `"CLI tool narrowing"` / `"MCP server policy"` arms, with a lock test at `:583` |
| `PATH-04` | `permission/src/path_constraints.rs:659` — the `..`-after-real-directory under-ask guard |
| `PS-CD-03` | `permission/src/policy.rs:229` — compound-cd flag, "1:1 with claude-code" |
| `GATE-WIRE-01` | `apps/cli/src/control_plane.rs:496` — `decision_reason_type` / `matched_ask_rule` on the wire |
| `HOOK-ASKFLOOR-03` | `permission/src/policy_gate_test.rs:1391` |
| `HOOKALLOW-01` | `permission/src/policy_gate.rs` ask-arm + tests at `policy_gate_test.rs:443,531` |
| `GATE-SYSMSG-01` | `platform-api/src/permission_gate.rs:233,241` (+16 more sites) |
| `AUTO-04` | 16 hits for `mcp_permission_mode_override` across `permission/` and `apps/` |
| `SED-XWU` | `permission/src/sed_validation.rs` |
| `AUTO-03` | `permission/src/policy.rs:2945` |
| `AUTO-07-followup` | `permission/src/denial_tracking.rs` |
| `metadata parent_session_id` | `llm-runtime/src/service.rs:778-819` — now taken and emitted |
| `GATE-UPDATES-01` | `permission/src/policy_gate_test.rs:1972` |
| `ps-acceptedits-cgs` | 42 hits for the `has_sub_expressions` / `has_script_blocks` / … predicates |
| `H-BIN-10` | `llm-runtime/tests/client_auth_test.rs:928` (Azure AI Foundry Claude) |
| `P2-01` | `platforms/posix/src/mcp.rs:379`, `tools/mcp/src/mcp_tool.rs:728` |
| `WIZARD-06` | completed 2026-07-24/25, waves 47–59 (producers, `--propose`, `/auto-mode-setup` slash surface + its runners) |

No `TODO`/`unimplemented!` is tied to any of these IDs (checked).

## STATUS UPDATE 2026-07-26

Two of the three are CLOSED; the section below is the original evidence.

- `PS-CALLER-06-2` (bare-repo indicators) — **CLOSED**,
  `permission/src/git_bare_repo.rs`.
- `P1-12` (background-attach stall detection) — **CLOSED**,
  `apps/cli/src/bg_attach_stall.rs`, with the `tengu_bg_attach_*` telemetry the
  audit found missing.

Still open (1): `PS-CALLER-06-5`, PS5.1 cwd-first shadowing (Windows-only,
security). Still an explicit in-code deferral at
`permission/src/powershell_containment.rs:3087`.

## Open (3) — ORIGINAL, superseded by the block above

### Deliberately deferred, still marked as such (2)

Both are explicit in-code placeholders, unchanged since the audit:

- `PS-CALLER-06-2` — `permission/src/powershell_containment.rs:3022`
  `// (bare-repo indicators — if(E) — OUT OF SCOPE, deferred.)`
- `PS-CALLER-06-5` — `permission/src/powershell_containment.rs:3078`
  `// (PS5.1 cwd-first shadowing — Windows-only — OUT OF SCOPE, deferred.)`

Both are security-sensitive per the audit, and both are Windows/PowerShell
edge paths. They are deferrals with a written reason, not oversights — but the
reason should be re-examined rather than inherited, since "out of scope" was a
scoping decision made for a different wave.

### Genuinely unimplemented (1)

`P1-12` (LOW, effort L) — background-attach stall detection.
`apps/cli/src/daemon_roster.rs:303` declares `attach_stall_respawns:
Option<i64>`, but it is written `None` at all five construction sites
(`background_launch.rs:974`, `daemon_roster.rs:1090`, `commands/attach.rs:286`,
`commands/daemon.rs:870,1107`). The field is dead: no first-frame heartbeat, no
stall kick/respawn/give-up logic, and zero `tengu_bg_attach_*` telemetry in
`telemetry/`. Exactly as the audit described.

## Method note

The same two traps recorded in `delta-audit-217-218-RETRIAGE-2026-07-25.md`
apply here and were avoided deliberately:

- an ID appearing in the source does **not** mean done (it could be a `TODO`),
  so each reference was read, not counted;
- an ID **not** appearing does not mean absent — `SRC-01`, `SRC-05`, `AUTO-04`,
  `metadata parent_session_id` and `ps-acceptedits-cgs` all carry zero
  references to their audit ID and are all implemented. They were found by
  checking the behaviour site instead.

Five of the eight items with no ID reference turned out to be **done**. Judging
this backlog by ID grep alone would have reported 8 open instead of 3.
