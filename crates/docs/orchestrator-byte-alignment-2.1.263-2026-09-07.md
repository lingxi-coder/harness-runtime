# Orchestrator — byte-level alignment vs Claude Code 2.1.263

**Date:** 2026-09-07
**Oracle:** `~/.local/share/claude/versions/2.1.263` (npm `latest` / `next`;
`claude --version` = `2.1.263 (Claude Code)`).
**SHA-256:** `ef5d2909c8af49f31ab6d5487e90316777bc2fac170adfe8160716caa8aaf4f9`
**Build:** `2026-09-06T01:08:56Z` / GIT `37ae3f38d765199d54a6913cd61c6c9ad8576cc6`
**Chunks:** `~/.claude/oracle-chunks/2.1.263/` (1651 files)
**Port pin:** `platform_api::CLAUDE_CODE_VERSION = "2.1.252"`
**Surface:** `lingxi-code/orchestrator/` (conversation, turn loop, streaming,
prompt, resume, hooks, reminders)

Last dedicated orchestrator tick was 2026-06-28 vs **2.1.195**. Compact was
later closed against 2.1.261. This pass re-judges the crate against 2.1.263
and only edits byte-lockable copy that the binary proves.

## Method

1. Preflight against the real binary, not `claude-code/src`.
2. Extract production literals (`1225` candidates; `276` production misses after
   dropping tests). Most misses are LingXi internals, rebrand, or interpolated
   fragments — not user-facing copy.
3. Distinctive-phrase hit/miss against the Mach-O, then `ctx.py` around each
   miss to recover the oracle's assembled form.
4. Changelog delta 2.1.252 → 2.1.263, restricted to orchestrator-owned
   surfaces (turn loop, streaming, resume, reminders, plan mode).

Carve-outs (not findings): multi-LLM routing, LingXi HEADER/`/help` rebrand,
`.lingxi` / `LINGXI_*` names, Artifact / workshop / Remote Control / CCR,
auto-mode classifier off, ContextCollapse.

## Headline

The prompt body, plan-mode (non-workshop), max-tokens nudge, and most
`<system-reminder>` envelopes still match 2.1.263 after interpolation. This
pass closed the streaming partial-finalize notices (still 2.1.207 copy) and
the remaining P2s: `Ldt` unknown-tool suffix, truncated-response recovery,
Opus-5 `oir` trailer, and `CLAUDE_CODE_STOP_HOOK_BLOCK_CAP` dual-read.

| | |
|---|---|
| Production literals ≥20 chars | 1225 |
| Segment-verified HIT | 291 (tests inflate MISS) |
| Production MISS after de-noise | mostly internals / rebrand / interpolated |
| Changelog-facing copy drift closed this pass | streaming incomplete-response notices; P2 `unk-suffix` / `trunc-recov` / `opus5-trailer` / `stop-cap-env` |

## Already aligned (re-verified against 2.1.263)

| Surface | Evidence |
|---|---|
| `Notes:` footer | Oracle `aue` interpolates `Do NOT ${Mn} report/summary/...` (`Mn` = Write). Port inlines `Write`. Concatenation is byte-identical. src_160988549.js @4675163 |
| `# System` / `# Doing tasks` distinctive bullets | HIT in binary (`Prefer editing existing files…`, `Default to writing no comments`, `All text you output outside of tool use…`) |
| Plan-mode banner (`This supercedes…`) | HIT. Subagent `dys`, sparse `uys`, full `cys` non-workshop bodies match `plan_reminder.rs` after `AskUserQuestion`/`ExitPlanMode`/`Edit`/`Write` interpolation |
| Max-tokens recovery nudge | Port `MAX_OUTPUT_TOKENS_RECOVERY_NUDGE` is the 2.1.263 pair of literals joined by U+2014 (`Output token limit hit. Resume directly — …`) @4258093 |
| Output-style reminder | Oracle `${QS(e.style)} output style is active. ${e.turnReminder??"Remember to follow…"}`. Port renders the fallback arm. @5337618 |
| Date-change reminder | HIT: `The date has changed. Today's date is now` |
| Hook additional-context | Oracle `${e.hookName} hook additional context: ${…}` — not a static `PostModelSwitch hook additional context` literal. Port assembly matches |
| Stop-hook block cap | HIT: `A hook blocked the turn from ending`. User-facing warning still names `LINGXI_STOP_HOOK_BLOCK_CAP`. Cap itself dual-reads `LINGXI_` then `CLAUDE_CODE_STOP_HOOK_BLOCK_CAP` (default 8) |
| Thinking-only nudge | Port `thinking_only_nudged` loop flag is present; 2.1.251 empty-text recovery is already wired |

## Fixed this pass

### Fable identity (`nss`, src_160988549.js @4644692)

Port still had a 2.1.220-era rewrite (`Project Glasswing`,
`platform.claude.com/docs/en/models/fable-5-1/overview`, "share the same
capabilities"). 2.1.263 `nss` is:

> This iteration of Claude is Claude Fable 5.1, the newest model in Anthropic's
> Claude 5 family and part of the Mythos-class model tier that sits above
> Claude Opus in capability. … share the same underlying model. … our most
> intelligent generally available model, and includes additional safety
> measures … https://www.anthropic.com/claude/fable

Updated `prompt/body_sections.rs` `FABLE_IDENTITY_SECTION`.

### Streaming has-output incomplete notices (2.1.263 @4784072)

Oracle (`Bl` = `API Error`, `f4` = has real output):

| cause (`tee`) | has-output notice |
|---|---|
| `watchdog` (`Yg`) | `API Error: The response stopped arriving. The response above may be incomplete.` |
| `server_error` (`Hu`) | `API Error: Server error mid-response. The response above may be incomplete.` |
| `stream_suspended` (`u4`, `rp.code==="StreamSuspended"`) | `API Error: Your computer went to sleep mid-response. The response above may be incomplete.` |
| `network_down` / `stale_connection` | `API Error: Connection lost mid-response. The response above may be incomplete.` |

Port still shipped the 2.1.207 tails (`Response stalled mid-stream`, `Connection closed mid-response`) and had no `stream_suspended` cause.

Updated:

- `orchestrator/src/streaming_loop.rs` — notices + `PartialFinalizeCause::StreamSuspended`
- tests in `streaming_loop.rs` and `tests/streaming_partial_finalize_test.rs`
- the same has-output strings in `agent/src/runner.rs` and `tools/web/src/web_search.rs` (those crates copy the query-loop finalize text)

No-output `Try again` tails remain on the retry/exhaustion path, as before.

### Thinking-signature 400 strip + latch (`sig-strip`, 2.1.259)

Oracle `_ot` / `Zz` / `wkt` / `CCt` / `QZ`: a 400 whose body matches Anthropic
thinking-signature copy strips assistant `thinking` / `redacted_thinking`
(and empty text), retries once (`tengu_thinking_signature_strip_retry`), and
latches `{type:"thinking_stripped",scope:"all"}` so later turns do not resend
the rejected blocks.

LingXi is multi-provider and other models also support thinking. The port:

- classifies by **400 copy** (`signature in thinking block`,
  `thinking.signature` + `field required`, modified/invalid signature,
  `thinking_signature` token), not by protocol family. OpenAI-compat /
  Gemini thinking models heal the same 400. DeepSeek `reasoning_content`
  400s stay terminal.
- later-turn latch strips outbound thinking on every thinking-capable
  route except DeepSeek / Kimi, which must round-trip `reasoning_content`.
- does not mutate persisted history; the latch is outbound-only.

### Resume hook additional context (`resume-hook-ctx`, 2.1.261)

Cold resume now rebuilds `hook_additional_context` (and `hook_blocking_error`)
into meta user messages in JSONL walk order, so hook output interleaved with
parallel `tool_result`s re-enters the resumed request. Empty `content` is
skipped. `thinking_stripped` restores the session latch and is not model-facing.

### Unknown-tool `Ldt` suffix (`unk-suffix`)

`synthetic_unknown_tool` now appends the 2.1.263 `Ldt` suffix after
`No such tool available: ${name}`. Mapped arms with substrate:

- Glob/Grep via Bash (or Shell): `… is not available in this session — find
  files with \`find\` / search file contents with \`grep\` via the {shell}
  tool instead.` No shell in the registry → `… is disabled for this session.`
- MCP `mcp__{server}__*` disconnected (`a5o` `disconnected`): main
  `has disconnected… reconnects`; subagent `is not available in this context`.
- Genuinely-unknown stays empty.

Also mapped this pass:

- Subagent-restricted `d1e`/`ct("external")` names (TaskOutput, ExitPlanMode,
  EnterPlanMode, AskUserQuestion, Poll, ConnectGitHub, propose_skills,
  WaitForMcpServers, RefreshMcpTools, Workflow, ScheduleWakeup,
  ReadNotifications, ProposeGoal, EndConversation):
  `… is not available inside subagents. Complete the task with the tools
  provided and return findings to the orchestrator.`
- Pending MCP `l5o` (non-subagent, `WaitForMcpServers` enabled):
  `The MCP server '${name}' is still connecting. Call WaitForMcpServers…`
- Coordinator `Y7e`: session `CoordinatorModeHandle` is on (`Ci()`), not
  `LINGXI_SIMPLE` / `CLAUDE_CODE_SIMPLE`, `Agent` is in the current tools,
  and the name is in the unfiltered catalog but not `qbt` (matched on the
  registered canonical name, so `ListPeers` → `ListAgents`) → `… is not
  available to you as the coordinator — run it from a worker via the Agent
  tool instead.` Desktop/CLI `engine_desktop::build` wires the same
  `CoordinatorMode` Arc used by `BuiltinToolContext`. The main-loop
  advertise/dispatch set follows the actual `Idr` assembler: `qbt` base
  (`Agent`, `TaskStop`, `SendMessage`, `StructuredOutput`, `Skill`,
  `ReadNotifications`, `ListAgents`, `Workflow`), PR subscription suffixes,
  MCP `comms`, brief tools, and configured extra names/V1/family names.
  Simple mode suppresses the redirect suffix, not pool filtering. User and
  plan controls require explicit extras; neither real outer assembler
  appends them afterward. The earlier prompt-derived allowlist was incorrect;
  see the 2026-09-08 re-audit. Worker redirects also require an enabled
  external-worker tool and no tool-wide deny, with Agent in the current pool.
  Workers retain the full registry. Reminder gates use dispatch lookup, and
  coordinator tests inject mode options without changing process environment.
- Full-catalog disabled: unfiltered `find_registered` hits a hidden builtin
  → `… is disabled for this session, in subagents as well as here.`
  (this arm precedes Glob/Grep-via-shell, matching `Ldt`).

WebFetch→Artifact remains a carve-out. The WebFetch→`web-fetch` agent
redirect needs the live roster (`pq`); that agent is gated off by default
(`xgi()`), so a catalog-hidden WebFetch takes the disabled / coordinator
arm. `ltr` spread into `ct` is not fully named.

### Truncated-response recovery (`trunc-recov`, 2.1.263 `tZo`)

Has-output incomplete-response notices persist `truncatedAfterOutput: true`
(omitted when false; JSONL order is `errorDetails?`, `truncatedAfterOutput?`,
`isApiErrorMessage` per `Ggr`). After tools drain, subagent (`query_source`
starts with `agent` or is `subagent`) and non-interactive main inject the
byte-exact meta nudge and continue, sharing `maxOutputTokensRecoveryCount`
(limit 3). Interactive main still ends. GB `tengu_truncated_response_recovery`
defaults true; the port matches the default-on arm.

### Opus-5 trailer (`opus5-trailer`, `oir`)

`OPUS_5_TERMINAL_RESTRICTIONS` is now one line:
`Do not use the Agent tool, workflows, or deep-research unless the user, a
LINGXI.md file, or a skill asks for it` (`CLAUDE.md` → `LINGXI.md` rebrand).

### Stop-hook cap dual-read (`stop-cap-env`)

Cap parse is `LINGXI_STOP_HOOK_BLOCK_CAP` then
`CLAUDE_CODE_STOP_HOOK_BLOCK_CAP` (unset / non-numeric → 8). User-facing
override warning still says `Set LINGXI_STOP_HOOK_BLOCK_CAP`.

## Confirmed remaining (not edited)

| id | sev | verdict | gap |
|---|---|---|---|
| `think-resume` | P3 | substrate | `nZo` / `resumeIncompleteThinking` / `preserveTrailingThinking` exist, but `tengu_thinking_block_resumption` defaults **false**. Inert unless GB enables it. Do not port a dead flag |
| `workshop` | — | carve-out | Plan-mode `kCt`/`rhr` workshop-document / Artifact-tool clauses. Artifact is an explicit exclusion |
| `ver-pin` | P3 | bookkeeping | `CLAUDE_CODE_VERSION` is still `"2.1.252"`. Not an orchestrator behaviour gap |

### Changelog items checked and not opened as orchestrator bugs

- **2.1.261 teammate re-sending first-turn skill/tool announcements** — teammate runner preloads once and keeps history across turn-sets (`agent/src/runner.rs`). Closed.
- **2.1.259 empty attachment on resume** — `resume.rs` skips a missing payload. Closed.
- **2.1.259 thinking rejected on every later turn** — two mechanisms: GB-gated `resumeIncompleteThinking` (default **off**, not a gap) and the always-on thinking-signature strip retry (`sig-strip`, closed this pass for all thinking-capable models except DeepSeek/Kimi round-trip).
- **2.1.261 `claude -p --resume <file>` malformed session ID** — CLI/session loader, not this crate.
- **HEADER** `You are LingXi…` vs `You are Claude Code, Anthropic's official CLI` / `You are an interactive CLI tool` — rebrand carve-out.

## Not done

- Bumping `CLAUDE_CODE_VERSION`.
- Workshop/Artifact plan-mode.
- Dead `tengu_thinking_block_resumption` (GB default false).
- Remaining `Ldt` arms: WebFetch→Artifact (carve-out) and WebFetch→`web-fetch`
  agent (`pq` roster; agent gated off by default). `CLAUDE_CODE_VERSION` stays
  `2.1.252` until the rest of the port (permission 2.1.263 delta, etc.)
  matches — the bump is last.

Fusion is LingXi-specific and is not aligned to Claude Code.

## Verification

Targeted tests (`CARGO_TARGET_DIR=/tmp/lx-orch-tgt`, `--offline`):

```sh
cargo test -p orchestrator --lib incomplete_notice_matches_2_1_263
cargo test -p orchestrator --lib partial_finalize_cause_classifies
cargo test -p orchestrator --lib fable_identity_matches_2_1_263
cargo test -p orchestrator --test streaming_partial_finalize_test
cargo test -p orchestrator --lib unknown_tool_suffix
cargo test -p orchestrator --lib truncated_response_recovery_nudge
cargo test -p orchestrator --lib delivering_work_and_corrections
cargo test -p orchestrator --lib synthetic_api_error_envelope
cargo test -p orchestrator --test stop_hooks_test stop_hook_block_cap_accepts
cargo test -p session --test jsonl_schema_test assistant_api_error
```

Passed:

- `incomplete_notice_matches_2_1_263_has_output_copy`
- `partial_finalize_cause_classifies_suspend_before_idle`
- `fable_identity_matches_2_1_263_nss`
- `streaming_partial_finalize_test` (7/7, including idle + suspend)
- `production_prompt_bodies_match_normalized_2_1_238_manifests` after
  re-blessing Fable/Mythos lengths `10_445` / `10_447`
- `unknown_tool_suffix_*` + coordinator dispatch/advertise tests (`add_tool_coordinator_hidden_worker_tool_gets_y7e`, `build_wire_tools_coordinator_matches_idr_assembly`)
- `truncated_response_recovery_nudge_matches_2_1_263`
- `delivering_work_and_corrections_are_opus_5_only`
- `synthetic_api_error_envelope_stamps_top_level_fields`
- `stop_hook_block_cap_accepts_claude_code_env_alias`
- `assistant_api_error_outer_key_order_matches_claude`
- `assistant_api_error_truncated_after_output_sits_before_is_api_error_message`

`tools/workflow/src/lib.rs` needed `additional_working_dirs.paths()` so
orchestrator could compile (`WorkingDirectory` vs `PathBuf` — pre-existing
type mismatch, not an alignment change).
