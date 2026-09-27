# Claude Code 2.1.220 parity — Session B wave report (2026-07-26)

Branch: `gap220/integration` (lanes `gap220/ui-misc`, `gap220/mcp-stream`, `gap220/auth-eng` merged).
Base: `a13591ed6` (already contained H5 config+trust, H6, H7, H8, M3, M4, mcp-list health checks — re-verified per item before fixing).
Head at gate time: see the gate section at the bottom.
Audit source: `docs/claude-code-2.1.220-global-parity-reaudit-2026-07-26.md`; per-item evidence: session scratchpad `gap220-evidence.json` (ephemeral — key summaries copied below).

Statuses marked "gate-verified" are items whose implementation agent crashed after committing: the orchestrator recorded them NOT-FIXED, but the commits are in the lanes and the gate re-verified each at the behaviour site (oracle strings + tests present).

## 1. Report items (10)

| Item | Title (audit §) | Status | Commits |
|---|---|---|---|
| H5 | `sandbox.network.strictAllowlist` — runtime enforcement (§H5) | FIXED (config+trust half was already at base) | `45b6f00eb` |
| M5 | MCP list/get observable state + diagnostics (§M5) | FIXED — deferred sliver: M5-issue-formatting | `4f263bd58` |
| M6 | managed MCP allow/deny env-var sources (§M6) — expand predicates against the startup env snapshot | FIXED — deferred sliver: M6-flagSettings-env-tier | `934237b4b` |
| M7 | agent frontmatter `mcpServers` → live tool registry (§M7) | FIXED — deferred sliver: M7-plugin-warn-variant | `b1ad5b477` + `f38e5babb` (bridge-server `strict_mcp_config` repair) |
| M8 | `auth login` browser-failure manual fallback (§M8) | FIXED | `2d55f32c5` |
| M9 | agent names still allow `:` (§M9) — NFKC `:` reject + leading `-` on the markdown path | FIXED (gate-verified) | `cc4c92e36` |
| M11 | Windows `CLAUDE_CODE_GIT_BASH_PATH` validation + auto-detect (§M11) | FIXED (gate-verified) | `fc09d6f8b` |
| M12 | agent-view left-arrow state machine (§M12) — settings gate, kGt hints, vim NORMAL | FIXED — deferred slivers: M12-attach-detach, M12-midturn-backgrounding, M12-managed-row | `7777dfae4` |
| M13 | OAuth source / enterprise subscriber seam (§M13) | FIXED (both halves) | `8b23d69dc`, `41c66c276` |
| P1 | OTEL record-site coverage (§P1) — `code_edit_tool.decision`, pr/commit counts, CC-named log records, dispatch-gate decision | FIXED — deferred sliver: P1-user_prompt-log | `95264c04c`, `6900c2094` |

## 2. Attempted hunted gaps (this wave)

| Item | Title | Status | Commits |
|---|---|---|---|
| N-changelog-1 | skill/plugin frontmatter booleans: yes/no/on/off/1/0 coercion (2.1.218) | FIXED (gate-verified) | `b4734fa27` |
| N-changelog-2 | prompt-history persistence (`history.jsonl`, locked + deduped writes; substrate for the 2.1.218 race fix) | FIXED | `85675f6fd` |
| N-env-4 | mid-session `date_change` attachment | FIXED — landed post-crash by the orchestrator (see §2.1) | `c203d32c3` |
| N-env-5 | `CLAUDE_CODE_DISPATCH_V2S` → `anthropic-dispatch-id: v2s` header + 5xx degradation latch (2.1.219) | FIXED (gate-verified) | `6e52ad676` |
| N-protocol-1 | interrupt receipt contract (`capabilities` in system/init, `still_queued`, `cancel_queued`/`cancelled`) | FIXED | `51949c597` |
| N-protocol-2 | `fast_mode_disabled_reason` in system/init, result frames, initialize control_response | FIXED | `51949c597` (same commit) |
| N-protocol-3 | Skill tool prompt was pre-2.1.217 text; missing 2.1.218 background-skill sentence | FIXED (gate-verified) | `cbb1ee581` |
| N-protocol-6 | result-frame `modelUsage.provider` field (2.1.218 sibling of `canonicalModel`) | FIXED — landed post-crash by the orchestrator (see §2.1) | `4c65b21b5` |

Adjudicated during the wave (not counted as attempted):

- **N-protocol-4** (`set_cwd` control request incl. `needs_trust`/`trust_root`): **already closed at baseline** — commit `7b3e6be1b` (contained in base `a13591ed6`) implemented it in `permission/src/set_cwd.rs` + the `run.rs` dispatch arm. Re-verified this session against the 2.1.220 binary: success `{status:"ok",cwd,changed,transcript_relocated}` and `{status:"needs_trust",directory,trust_root?}` match key-for-key (`trust_root` omitted when redundant, exactly like the oracle's conditional); the 4 `set_cwd` regression tests pass in this worktree. No change needed.

### 2.1 N-env-4 / N-protocol-6 — landed post-crash by the orchestrator

The agent that owned N-env-4 + N-protocol-6 stalled mid-stream (API error) AFTER finishing the edits but before verifying/committing. The orchestrator session took over its in-progress task: confirmed every `ModelUsageRow` literal site in the workspace was covered, ran `cargo check -q --workspace --tests` (clean; the two remaining warnings — missing-docs on `StdinChannels` and the `#[test]`-less `init_frame_matches_2_1_201_p_mode_shape` — pre-exist on base `a13591ed6` from `cea12a147`), ran the targeted suites (orchestrator `date_change` 2, cli `canonical_model` 2, cost 84, tui `screen_view` 15, command-core 376 — all green), then split-committed the WIP per item (`conversation.rs` divided by hunk via `git apply --cached`): `4c65b21b5` (N-protocol-6), `c203d32c3` (N-env-4). Merged into integration at `db5a4bf6b`.

## 3. Hunted-gap backlog — NOT attempted this wave

Evidence summaries copied verbatim from `gap220-evidence.json` (the scratchpad is ephemeral).

### N-env-2 (Medium, est L) — Ultracode ultra-effort enter/exit reminder chain + `CLAUDE_CODE_JUNIPER_SUNDIAL` cadence override missing

> Oracle 2.1.220: j2y (@237710871) is a transcript state machine: when EK(model,effort,workflowsOn)===true (`EK(e,t,r){return r===!0&&LA()&&Goe(e,t)==="xhigh"}`, LA()=workflows available+enabled — a real external feature) it emits `ultra_effort_enter` reminderType:"full" ('Ultracode is on: … Use the Workflow tool on every substantive task…'), re-emits sparse ('Ultracode is still on…') every bop() non-meta user turns, and `ultra_effort_exit` ('Ultracode is off…') on leave. bop() (@237702588): env CLAUDE_CODE_JUNIPER_SUNDIAL > statsig `tengu_juniper_sundial` > gate > ULTRA_EFFORT_CONFIG.TURNS_BETWEEN_MAINTENANCE=10. Chain exists in 2.1.217; env var added 2.1.218. Related `workflow_keyword_request` attachment + `workflowKeywordTriggerEnabled` setting. Port: rg for ultra_effort|Ultracode|JUNIPER_SUNDIAL|TURNS_BETWEEN_MAINTENANCE|workflow_keyword|workflowKeywordTriggerEnabled → zero code hits; effort.rs:106 hardcodes `dynamic_workflows_enabled() -> false` although tools/workflow is live (per report M3/M4).

Port files: `commands/core/src/effort.rs`, `orchestrator/src/conversation.rs`, `tools/workflow/src/lib.rs`.

### N-env-3 (Medium, est XL) — Memory-sync push mass-delete hold + `CLAUDE_CODE_DISABLE_MEMORY_MASS_DELETE_HOLD` escape hatch absent (whole push/pull sync engine unported)

> Oracle 2.1.220 (@235604090): `function Ity(e){if(Z.CLAUDE_CODE_DISABLE_MEMORY_MASS_DELETE_HOLD)return Number.POSITIVE_INFINITY;return Math.max(kty,Math.floor(e*xty))}` with kty=50, xty=0.1 — a memory push that would delete >= max(50, 10% of store) is HELD (data-loss safety, default ON); env var (new 2.1.218) disables the hold. Sits in the user+team multistore sync engine: push_written/push_deleted/conflicts log, delete modes corroborate|immediate|never via CLAUDE_CODE_MEMORY_PUSH_DELETE_MODE/`tengu_mem_push_delete_mode` (already in 2.1.217). Port: memory/src/team_memory.rs is only a local mtime poll watcher over ~/.lingxi/team-mem; rg push_deleted|push_written|mass.?delete|corroborate|MASS_DELETE_HOLD|MEMORY_PUSH_DELETE over all Rust → zero hits; no remote push/pull/delete path exists, so neither the hold nor its escape hatch has a consumer. Subsystem-scope product call needed first (team-memory adjacency to the frozen team/swarm features); if not ported, record an explicit Divergence(reason).

Port files: `memory/src/team_memory.rs`, `memory/src/lib.rs`.

### N-changelog-3 (Medium, est L) — `/deep-research` built-in workflow absent (with 2.1.218 manual-invocation-only semantics) — port ships no built-in workflow library

> Oracle: 2.1.218 changelog "Changed /deep-research to start only when invoked manually". 2.1.220 binary bundles the full workflow: `"deep-research"`, phases Scope/Search/Fetch/Verify/Synthesize, generated script (`VOTES_PER_CLAIM=3`, `MAX_FETCH=15`), invoked via `Workflow({name:'deep-research', args:'<question>'})`, gated only by kill-switch `tengu_sorrel_avocet` (enabled by default); 2.1.220 adds prompt guard "Do not use workflows or deep-research unless the user requested it" (0 hits in 217/218, 2 in 220). Port: only hit is a test fixture string (apps/cli/src/stream_json.rs:1648); tools/workflow/src/lib.rs:174-175 states "LingXi ships no built-in workflow library, so a `name` that isn't a saved file is an error" — `Workflow({name:'deep-research'})` errors.

Port files: `tools/workflow/src/lib.rs`, `commands/core/src/bundled/mod.rs`. Coordinate with H1 owners for the guard sentence.

### N-changelog-4 (Low, est M) — Copy-on-select (OSC 52) absent, so the 2.1.219 GNU-screen DCS-passthrough fix has no behaviour site

> Oracle: 2.1.219 changelog "Fixed copy-on-select inside GNU screen printing base64 into the terminal instead of copying the selection". 2.1.219 binary contains the OSC 52 clipboard write `]52;c;`, `tmux;` DCS-passthrough wrapping, and "copy on select" strings; feature predates the window (13 hits already in 2.1.217) — 219's delta is wrapping the OSC 52 emission in a DCS passthrough when $TERM/screen is detected. Port: zero hits for `]52;`/osc52/copy_on_select across tui and tui-core in main and w75; the only clipboard surfaces are /copy and clipboard-image paste — mouse-selection copy never emits OSC 52, so neither the feature nor the screen passthrough exists.

Port files: `tui/src/app.rs`, `tui-core/src/terminal_setup.rs`.

### N-changelog-5 (Low, est M) — Screen-reader input announcements stuck at 2.1.201: no deleted-text announcements (2.1.218) and full-line re-announce instead of typed-char echo (2.1.219)

> Oracle: 2.1.218 "Added screen-reader announcements of deleted text for word and line deletions in --ax-screen-reader mode" + "Fixed VoiceOver reading 'new line' instead of echoing the typed space"; 2.1.219 "Fixed screen-reader mode rewriting the entire input line on every keystroke instead of echoing only the typed character". Binary corroboration: 217→218 adds one `announce` and two `aria-live` sites. Port: both modules self-document as 2.1.201 ports (apps/cli/src/ax_screen_reader.rs:3, tui/src/screen_reader.rs:9); announcement granularity is whole-line diffs (`diff_lines`, screen_reader.rs:402) — exactly the pre-219 behavior — and the file has zero deletion-announcement logic. Verify exact wording against a live 2.1.220 run before locking strings.

Port files: `tui/src/screen_reader.rs`, `tui/src/composer.rs`, `apps/cli/src/ax_screen_reader.rs`.

### N-protocol-5 (Medium, est XL) — Agent observer pairing absent — frontmatter `observer`/`observerMessage` and 2.1.218 `observeSubagents` fan-out

> Oracle: agent definition zod schema has observer (string, 217+), observerMessage (217+), and observeSubagents (boolean, 0 hits in 2.1.217 → 12 in 218+). Runtime: '[agentObserver] Agent X declares observer Y…', pairing carries fanoutToSubagents:e.observeSubagents!==!1 and fanoutDepth with a depth cap, and spawned subagents inherit the observer unless observeSubagents:false ('not fanning out to observer agent (no chaining)'). Port: rg observer/observerMessage/observeSubagents across all .rs = 0 hits; agent/src/catalog.rs frontmatter parser has zero 'observ' matches, so the fields are silently dropped and no observer agent is ever armed. Schedule after H7 (depth 3) since fan-out interacts with spawn depth.

Port files: `agent/src/catalog.rs`, `platform-api/src/subagent_spawn.rs`.

### N-protocol-7 (Low, est S) — Lean-model (Opus 5) Bash/Agent tool description content not ported

> Oracle black-box: 2.1.220 with claude-opus-5 sends a Bash description WITHOUT the 'IMPORTANT: Avoid using this tool to run `cat`…' bullet and WITH new bullet '- Command output is displayed to you, not reliably to the user.'; the Agent tool drops its 'Reach for this when the task matches…' lead-in. Rerun of the SAME binary with claude-opus-4-8 shows neither change → model-conditional lean-tool-prompt content, unreachable pre-219. Port: tools/shell/src/prompt.rs:656-685 simple_prompt_concise still contains the avoid-list bullet and lacks the new line ('displayed to you' = 0 hits port-wide). **Depends on the sibling session's H1/M1/H2 capability-registry work** — refresh the SHORT prompt CONTENT when that lands.

Port files: `tools/shell/src/prompt.rs`, `tools/task/src`, `tool-api/src/model_prompt_gate.rs` (sibling-owned).

### N-protocol-8 (Low, est M) — Memory-backend push mass-delete hold (2.1.218, default-on) absent — rides an unported memory sync/push pipeline

> Oracle: 2.1.218 adds CLAUDE_CODE_DISABLE_MEMORY_MASS_DELETE_HOLD (0 hits in 2.1.217, 3 in 218+); binary: pushes deleting more than max(50, 10% of entries) are held by default, alongside the pre-existing push-delete modes (corroborate|immediate|never). Port: rg mass_delete/push_delete/corroborate/memory_stream across .rs = 0 hits; memory/src has no backend push module at all, so the entire push-delete pipeline the hold guards is absent. First make a product call whether backend memory sync is in scope (first-party account endpoints); if no, record it as an explicit boundary. Overlaps N-env-3 — triage the two together.

Port files: `memory/src/lib.rs`.

## 4. Adjudicated refutations / already-closed

| Item | Adjudication |
|---|---|
| H4 (`/code-review` 仍为 inline prompt) | **REFUTED** — in 2.1.220 `/code-review` IS the built-in skill invocation ("runs the built-in skill at its lightest effort level"); `CODE_REVIEW_WORKFLOW_NAME` has no consumer in the binary. Audit item was stale. |
| P2 (micro-compact 无真实 idle-gap 判定) | **REFUTED** at adjudication — evidence did not hold up oracle-side. Do not re-open without fresh binary evidence. |
| H3 (`context: fork` 没有真实执行链) | **Already closed** before this wave (context:fork + resume gate landed in the previous parity wave). |
| P3 (`MEMORY.md` near-cap advisory 未接入 write hook) | **Already closed** before this wave. |
| H6 (`DirectoryAdded` hook + `register_repo_root`) | **Partial** — the event, payload, and firing site were already closed at base `a13591ed6`; adjudicated as partially complete, remainder stays on the backlog. |

## 5. Out of scope for Session B (sibling session)

- **M10** (对外 parity 版本仍标记为 2.1.217 — `CLAUDE_CODE_VERSION` bump) and **H1** (lean main system prompt consumer) remain with the sibling session's prompt/model wave (H1/H2/M1/M2/M10). This gate deliberately did NOT bump `CLAUDE_CODE_VERSION`.
- Sibling-owned files were never touched by this wave: `orchestrator/src/prompt/*`, `tool-api/src/model_prompt_gate.rs`, `platform-api/src/model_capabilities.rs`.
- ⚠️ **Duplicate implementations to reconcile at the integration→main merge**: the sibling session's `d0ddcfb32` on main re-implemented **H5 runtime enforcement + M11** (Session B items per the split) from the same oracle sites (`wSu`/`srt` @229871903, `MQ` @226607421). Same four files on both sides: `sandbox-runtime/src/{config,matcher}.rs`, `sandbox-runtime-runner/src/convert.rs`, `tools/shell/src/bash.rs`. Whoever merges second keeps ONE implementation and the UNION of tests (this wave: `45b6f00eb` incl. the http_proxy CONNECT-403 enforcement-layer test + runner structural-key tests, `fc09d6f8b`; main: the cross-separator Windows-basename fix).

## 6. Deferred slivers carried out of this wave

M12-attach-detach, M12-midturn-backgrounding, M12-managed-row, P1-user_prompt-log, M5-issue-formatting, M6-flagSettings-env-tier, M7-plugin-warn-variant — documented in the owning lane summaries; nothing in the merge blocks them.

## 7. Integration + gate

- Merges: `gap220/ui-misc` → `6e3170341`, `gap220/mcp-stream` → `c609e5edd`, `gap220/auth-eng` → `5b9d6f4ea`; one textual conflict (`mcp/src/json_config.rs`, M5×M7 — resolved preserving both) and one semantic cross-lane breakage (M7's `strict_mcp_config` field missing from bridge-server initializers) repaired in `f38e5babb`. Second `gap220/ui-misc` merge (N-protocol-6 + N-env-4 remainder) → `db5a4bf6b` (clean; `stream_json.rs` auto-merged against the mcp-stream lane).
- Gate run 1 (head `db5a4bf6b`): `cargo build --workspace --tests` FAILED — E0063, M7's engine-desktop test helper missed mcp-stream's new `McpServerConfig.config_error` field (the second M5×M7 cross-lane seam; only `--tests` builds see it). Repaired in `51853980a`; the accompanying `cargo check -q --workspace --tests` sweep found no further sites (remaining warnings all pre-exist on base in files this wave never touched).
- Gate run 2 (head `51853980a`): `cargo build --workspace --tests` exit 0; `cargo test --workspace` (doctests included) — **502 suites, 12694 passed, 0 failed, 10 ignored**. No test-count drop vs the ~12.6k baseline (count is UP ~80 from this wave's new tests), so no missing-test-binary red flag.
- `CLAUDE_CODE_VERSION` was NOT bumped by this wave (stays `2.1.217`; the bump is the sibling session's M10, after both waves merge).

## 8. Ultra-review remediation (2026-07-27)

An adversarial ultra-review of the whole wave (review base `4c55d6aa4`) produced 48
candidate findings across 8 lanes. 9 were refuted during review; the remaining **39
were confirmed**, each re-verified against the 2.1.220 binary by its fix lane before
any code changed, and handed to three fix lanes — `fix220/mcp`, `fix220/auth`,
`fix220/soc` — all now merged into `gap220/integration`.

### 8.1 Confirmed findings (39)

| Id | Disposition | Commit | Lane |
|---|---|---|---|
| MCPCORE-1 | FIXED | `5ef7a725f` | mcp |
| MCPCORE-2 | FIXED | `c2d0e8a76` | mcp |
| MCPCLI-1 | FIXED | `5908afc55` | mcp |
| MCPCLI-2 | FIXED | `fa61c9718` | mcp |
| MCPCLI-3 | FIXED | `5908afc55` | mcp |
| MCPCLI-4 | FIXED | `2a8c6fc3c` | auth |
| MCPCLI-6 | FIXED | `5908afc55` | mcp |
| MCPCLI-7 | FIXED | `529fb6c66` | mcp |
| AUTH-1 | FIXED | `2a8c6fc3c` | auth |
| AUTH-2 | FIXED | `9c55bd4fb` | auth |
| AUTH-3 | FIXED | `c10a97e7c` | auth |
| AUTH-6 | FIXED | `166ba3375` | auth |
| AUTH-7 | FIXED | `6af0a2cda` | auth |
| SANDBOX-1 | FIXED | `2c5bf5324` | auth |
| TELSH-1 | FIXED | `c10a97e7c` | auth |
| TELSH-2 | FIXED | `c10a97e7c` | auth |
| TELSH-3 | FIXED | `6a8d96377` | soc |
| TELSH-4 | FIXED | `caea97614` | auth |
| TELSH-6 | FIXED | `0cf1830a9` | auth |
| TELSH-7 | FIXED | `c10a97e7c` | auth |
| TELSH-8 | FIXED | `c10a97e7c` | auth |
| ORCH-1 | FIXED | `6a8d96377` | soc |
| ORCH-2 | FIXED | `d71bb1585` | soc |
| ORCH-3 | FIXED | `d71bb1585` | soc |
| ORCH-4 | FIXED | `6a8d96377` | soc |
| ORCH-5 | FIXED | `6a8d96377` | soc |
| ORCH-8 | FIXED | `6a8d96377` | soc |
| ORCH-9 | FIXED | `6a8d96377` | soc |
| UISESS-1 | FIXED | `678da6d79` + `f7bf7af1e` | soc |
| UISESS-2 | FIXED | `678da6d79` | soc |
| UISESS-3 | FIXED | `70dc9409f` | soc |
| UISESS-4 | FIXED | `70dc9409f` | soc |
| UISESS-5 | FIXED | `678da6d79` | soc |
| STREAM-1 | FIXED | `4a74df014` + `1c177cbe7` | soc |
| STREAM-2 | FIXED | `1c177cbe7` | soc |
| STREAM-3 | FIXED | `1c177cbe7` | soc |
| STREAM-4 | FIXED | `1c177cbe7` | soc |
| STREAM-5 | FIXED | `1c177cbe7` | soc |
| STREAM-6 | FIXED | `1c177cbe7` + `63bfc5d6c` | soc |

No confirmed finding was DEFERRED or REFUTED-ON-RECHECK: all 39 were still real at
`4c55d6aa4` when their lane re-verified them, and all 39 are present in the merged
tree (spot-checked at the behaviour site, not just by commit message).

Six adjacent follow-ups the lanes documented rather than fixed (each needs a file
outside the fixing lane's ownership, or is pre-existing; none is a regression from
this remediation). These are BACKLOG, not silent drops:

- **`afe` loader-side enterprise/`mcp`-locked exclusivity** — `mcp list` still prints
  user/project/local rows under a managed `managed-mcp.json` that the oracle's `afe`
  (@231822040) replaces with the managed set alone. MCPCLI-6 removed the *warning*
  half; the server-set half needs `mcp/src/json_config.rs`.
- **`Nxe`'s third pre-dial gate** (@232117552) — a syntactically invalid but non-blank
  `url` is `INVALID_CONFIG` without dialing in the oracle; the port still dials and
  reports the transport's error. Clean standalone follow-up in `mcp/src/registry.rs`.
- **`afe`'s shadow rule for rejected project servers** (adjacent to MCPCLI-3) — the
  oracle's project loop skips a name already present in user/local scope *before* the
  rejected arm, so a rejected project server that shadows a USER server leaves the user
  entry standing. The port's loader collapses by name into one `Vec` entry, so filtering
  the rejected project server would drop the legitimate user row too. Needs
  `mcp/src/json_config.rs`. No regression: the pre-fix row loop already skipped it.
- **`disabledMcpServers` vs an agent server replacing a gated discovered one** (adjacent
  to MCPCLI-2) — the oracle evaluates the disable check by name *downstream* of the map
  merge; the port precomputes `McpServerConfig::disabled` in `apply_project_server_gate`,
  which runs BEFORE the merge. After MCPCLI-2 an agent's replacement config carries
  `disabled = false` — right for the rejected-`.mcp.json` case the finding cites, but it
  lets an agent server named in `disabledMcpServers` connect. Distinguishing the two
  needs `mcp/src/server_gate.rs` to record *why* a server was disabled.
- **`mcp get` never health-checks a connectable server** (pre-existing, not from this
  wave) — `hJy` @238844777 always calls `yEp(t,i)` and prints a `Status:` line for every
  server; the port's `run_get` prints `Status:` only on the pending / rejected /
  config-only branches, so a healthy server gets none. MCPCLI-1 only retargeted the
  config-only branch.
- **ORCH `decisionClassification` + `tool_parameters`** (documented at the site in
  `6a8d96377`) — parsing the host's explicit `decisionClassification` needs a new field
  on `PermissionOutcome` plus the stdio control-plane parse; `tool_parameters` (`HWr`
  under `OTEL_LOG_TOOL_DETAILS`) is inert unless that env flag is set. Behaviour is
  unchanged today.

Two of the nine review-refuted ids (§8.2) were refuted for **scope**, not for being
wrong about the oracle, and belong on the same backlog: **MCPCLI-5** (empty
`mcpServers: {}` drops the whole JSON agent instead of warning per entry) and
**ORCH-7** (the per-turn `# currentDate` push vs the oracle's memoized `LGe`).

### 8.2 Refuted during review (9, for the record)

| Id | Why it was dropped |
|---|---|
| MCPCORE-3 | Claimed `setenv`-reallocation race is impossible between Rust accessors — std's unix backend takes a process-wide `ENV_LOCK` in `env()`, `getenv` and `setenv`; the probe var is unique to the one test, and `OnceLock::get_or_init` populates the snapshot before the mutation. |
| MCPCLI-5 | Real oracle divergence (zod `z.record` accepts `{}`; `obs` `continue`s per entry) but **pre-existing base behavior** — the `!map.is_empty()` guard dates to `412c597b6` (2026-06-19); the wave only rewrote the arm's body. Out of scope, re-file separately. |
| AUTH-4 | Premise inverted: `forceLoginMethod`'s resolver `Ber()` has 5 call sites and **none** participate in auth-source resolution (`PA()`/`e1()` have no such branch), so `managed_oauth_only: false` at `mcp serve`/`auto-mode-setup` is oracle-CORRECT. Its FD half would be actively harmful (the port never reads the descriptor). The real entrypoint-consistency defect is MCPCLI-4, fixed by `2a8c6fc3c`. |
| AUTH-5 | Decisive mechanism false: the CLI installs the OTEL runtime in its own `run()` (`apps/cli/src/lib.rs:477`) *before* subcommand dispatch (`:642`), and `AnalyticsBus::log_event` calls `mirror_analytics_event` unconditionally, so the event does egress. The sink-less half is workspace-wide pre-existing architecture (`attach_sink` has zero production call sites). |
| SANDBOX-2 | No defect at HEAD — all three transports already call `filter_network_request_with_ask`, and the whole strict gate lives in that one shared function (which gained 3 dedicated tests). `socks_proxy.rs` has a zero-byte diff vs base; ask-coverage was 0/3 before the wave and is now 1/3. |
| ORCH-6 | Cited cause chain is dead code: the sole production caller passes `pricing_provider_id_for_profile(...)`, whose range never includes `VertexClaude`/`FoundryClaude`, so the quoted arms are unreachable; the actual emitted value is the profile name. Claude-on-Vertex/Foundry only reaches LingXi through the multi-provider profile system (an accepted divergence). |
| ORCH-7 | Out of scope + the quoted doc line is not false. The per-turn `current_date_string()` push is byte-identical at base `a13591ed6` and untouched by the diff; the wave strictly *reduced* divergence (two deltas → one). Worth filing on its own. |
| ORCH-10 | Describes a hypothetical future regression, not a defect: at all five sites the deferred prepend runs first and `date_change` second, giving `[date_change, deferred_tools_delta, …]` — exactly what `Ky("date_change")` @237703570 inside a `Promise.all` requires. The untested-ordering property is pre-existing base convention. |
| TELSH-5 | The `set_var` is inside `if cfg!(windows)` and its only caller gates on `cfg!(windows)` too, so it never executes on the platforms whose runtime the finding names; on Windows std documents `set_var` as always safe (bare `SetEnvironmentVariableW`). The oracle premise is also wrong — `P6n` is not memoized and has a lazy per-invocation call site @241508347. |

### 8.3 Integration + gate (ultra-review remediation)

- Merges into `gap220/integration`, in order: `fix220/mcp` → `29d675478`,
  `fix220/auth` → `eb550b984`, `fix220/soc` → `6c73779e6`. **No conflicts.** The
  anticipated `apps/engine-desktop/src/lib.rs` collision did not materialise
  textually: the mcp lane's agent-frontmatter MCP merge region (`dynamic_names`,
  the `FWt`/`afe` ordering fix) and the auth lane's credential region
  (`subscription_seed`, `host_managed_oauth_only`, the `KWr()` read inside
  `resolve_llm_stack`) are disjoint, and both intents are present in the merged
  file — verified by reading the merged result, not by trusting the auto-merge.
- `cargo check -q --workspace --tests` after the `fix220/mcp` merge and again after
  the `fix220/auth` merge: **exit 0 both times**, so no compile-level repair was
  needed (contrast the two `--tests`-only cross-lane breakages earlier in this
  wave). The `--tests` flag still matters: `fix220/soc` turns
  `platform_api::PermissionResolution::Allow` from a unit into a struct variant
  (`rule_source`), which only test-side constructors would have exposed.
- **One real repair — `dac087a04`.** Gate run 1 at `6c73779e6` built clean but
  `-p llm-runtime --lib` failed ~50% of the time on
  `global_fallback_model_works_without_chain_entry`
  (`left: Some("claude-opus-4-6")`, i.e. the primary model on the attempt that
  should have been the fallback). Root cause: `fix220/auth`'s `c10a97e7c` added
  the first tests that flip `tengu_cedar_lattice` (`CedarLatticeOn`), and
  `telemetry::test_set_flag` writes the PROCESS-GLOBAL override map (the port of
  `ROt()`/`Uvi`). Serializing the flippers against each other left every OTHER
  concurrently-running test in the binary reading the flipped value; a leaked
  opt-in makes a first-party attempt carry `anthropic-dispatch-id`, and
  `note_dispatch_header_failure` then inserts the oracle's budget-free
  `"retry:dispatch-header-strip"` attempt — correct production behaviour that
  silently shifts every attempt-count and fallback-position assertion.
  The exact victim set (6 drive-loop tests + the 2 default-asserting flag tests)
  was established by forcing `dispatch_v2s_opt_in()` to `true` for one run, and
  `DISPATCH_FLAG_LOCK` became an `RwLock`: flippers take the write side, the
  tests that need the opt-in OFF take the read side. **No production code changed
  and no assertion was weakened.** 12/12 green after; removing just the one read
  guard puts it back to 0/5.
- Gate at head `dac087a04`: `cargo build --workspace --tests` **exit 0**;
  `cargo test --workspace --no-fail-fast` (doctests included) —
  **502 suites, 12738 passed, 0 failed, 10 ignored** (81 of the 502 are
  doctest suites), exit 0.
- Baseline for the count check was 502 suites / 12694 passed / 0 failed at
  `51853980a`. The suite count is IDENTICAL (502 → 502) and the passing
  count is UP 44 (12694 → 12738) from the fix lanes' new tests, so there is no
  missing-test-binary red flag.
- `CLAUDE_CODE_VERSION` was again **NOT** bumped (still the sibling session's M10).
- `permission/src/policy_test.rs` (sibling-owned) was not touched by any lane in
  this remediation; its last commit is still `80dc7a6a6`.
- `main` was deliberately **not** merged or pulled: the duplicate-implementation
  reconciliation flagged in §5 (H5/M11, `sandbox-runtime/src/{config,matcher}.rs`,
  `sandbox-runtime-runner/src/convert.rs`, `tools/shell/src/bash.rs`) is scheduled
  separately. Note that `fix220/auth` touched two of those four files
  (`sandbox-runtime/src/matcher.rs` via SANDBOX-1, `tools/shell/src/bash.rs` via
  TELSH-4), so the reconciliation must now keep the union of THREE
  implementations' tests, not two.
