# Plan mode + goal — byte-level alignment to Claude Code 2.1.266

**Status: all three phases implemented and TESTED (one gap, named below).**
Each item below carries its oracle evidence (chunk + byte offset) and the LingXi
site it lands in. Phases are ordered by live impact, not by size. See
"Implementation status" and "Verification status" at the end before trusting any
of it.

## Oracle provenance

- Executable: `~/.local/share/claude/versions/2.1.266` (Mach-O arm64, 199 422 144 bytes).
- SHA-256 (verified 2026-09-08): `553d1b9e9e7068b275c0a783c7e139ff6503096f286e674c8c919379fb0eca62`.
- `npm view @anthropic-ai/claude-code version` → `2.1.266`, so this **is** latest.
- Extracted production chunks: `~/.claude/oracle-chunks/2.1.266/` (1659 chunks, 36 MB),
  produced by `~/.claude/oracle-chunks/extract.py 2.1.266`.
- Every claim below was read out of the extracted production source, not from the
  older TypeScript mirror in `claude-code/src` (which is stale) and not from
  `strings` on the binary.

### Chunk map for this subsystem

| Chunk | Contents |
|---|---|
| `src_160454523.js` | goal core: `CBe=4000`, clear tokens, `HTt` kickoff prompt, trust/hooks gates `YZe`, set/clear `rRe`/`oRe`, `goal_status` sentinel `bKt`, `queuedGoalOrigin` |
| `src_161430779.js` | `ProposeGoal` name/`LAt=500`/description/prompt |
| `src_177262364.js` | `ProposeGoalTool` definition |
| `src_176287568.js` | the two `/goal` command objects (`local-jsx` default + `goalNonInteractive`) |
| `src_185633862.js` | `/goal` non-interactive body (the one LingXi ports) |
| `src_188506762.js` | `/goal` interactive dialog body |
| `src_160477403.js` | plan-file module: plans dir, slug, `getPlan`, workshop doc, resume/fork copy |
| `src_160529511.js` | write/read permission resolver incl. the plan-file carve-out `Zl` |
| `src_162329786.js` | main app chunk: EnterPlanMode @3896500‑3905000, ExitPlanMode @3506000‑3520500, goal check-in @4166000‑4172000, goal evaluator @4202600‑4220500, plan reminders @5415000‑5462000, ultraplan @5058000‑5067000 |
| `src_159086541.js` | `modelProposedGoals` setting resolution (`rHe`, `vOn`) |
| `src_174482992.js` | `Gft(){return H("tengu_propose_goal",!1)}` |

## Method

`~/.claude/oracle-chunks/notes/plan-goal-2.1.266/diff_literals.py` extracts every
prose string literal (≥25 chars, template interpolations split out) from the
regions above, decodes JS escapes, and checks each against the whole LingXi tree
under a normalisation that survives Rust's `\"`, `\n`, `\u{…}` and line
continuations. Output: `~/.claude/oracle-chunks/notes/plan-goal-2.1.266/literals.json`
(350 literals, 114 present, 236 absent). The absent set was then triaged by hand —
most of it is oracle-internal logging or branches that cannot fire in LingXi.
Counts per region are reproducible by re-running the script.

## What is already aligned (do not re-port)

- `orchestrator/src/prompt/plan_reminder.rs` — the full/sparse/subagent plan-mode
  reminders. The 2.1.206 port is still **byte-identical** to 2.1.266's `mTs`/`gTs`
  bodies on the path LingXi can reach: `nSr` banner, Phase 1/2/3, `dTs` Phase 4,
  `xSr` exit tail, the NOTE tail, and `### Phase 5: Call ExitPlanMode` all match.
  2.1.266's additions to these strings are all inside `workshopOfferDocPath` /
  `workshopActiveDocPath` conditionals (see "Not applicable").
- `EXIT_PLAN_MODE_V2_TOOL_PROMPT` in `tools/plan/src/plan_mode.rs` — byte-equal to
  2.1.266 `k2n` @3509476 with `${As}` resolved to `AskUserQuestion`.
- `ExitPlanMode` input schema (`allowedPrompts` deprecated + passthrough) — equal to `w2n` @3511400.
- Both tools' `searchHint`s — equal to 2.1.266.
- `orchestrator/src/prompt/goal_checkin.rs` — the two turn-end check-in bodies are
  byte-equal to 2.1.266 `Xer` (the 2.1.238 text did not change), as is
  `TASK_LINE_CHAR_CAP = 120` (`Ans`), `DEFAULT_CHECKIN_MINUTES = 30` (`vns`) and the
  `tengu_saffron_wren`-default-true reading.
- goal core constants: `MAX_CONDITION_CHARS = 4000` (`CBe`), the six clear tokens
  (`H`), both gate messages (`A`/`O` @ `YZe`), and the `HTt` kickoff directive.
- `core/src/settings/schema.rs` accepts `modelProposedGoals: auto|alwaysAsk|disabled`,
  matching `rHe`/`vOn` in `src_159086541.js`.
- The plans directory resolver (`ConversationOrchestrator::plans_dir`) matches `iT`.

---

## Phase 1 — plan mode is half-wired (P0, live, user-visible)

These three compose into one broken workflow: LingXi tells the model to write a
plan file, then denies the write, then never reads the file back.

### P0-1 · No plan-file write carve-out ⇒ every plan-file write raises a prompt

Oracle `src_160529511.js` `LZe` @351534 returns an ALLOW before the plan-mode
branch is reached:

```js
if(Zl(d,{includeWorkshopDoc:o?.permissionMode==="plan"}))
  return Oe(n,"Plan files for current session are allowed for writing");
…
if(r.mode==="plan")return{behavior:"ask",message:`Cannot write to ${d} while in plan mode.`,…}
```

with `Zl` @323641 matching `dirname === plansDir()` and basename `${slug}.md`,
`${slug}.workshop.md` (plan mode only), or `${slug}-agent-*.md`. The read
resolver has the mirror allow, `"Plan files for current session are allowed for reading"`.

LingXi: `git grep 'Plan files for current session'` → **0 hits**. `Write`/`Edit`
are absent from `PLAN_SAFE_TOOLS` (`permission/src/mode_policy.rs:37`), so
`permission/src/policy.rs:1532` takes the `ask_plan_mutation` branch for the very
file the reminder just told the model to write. `platform_api::teammate_plan::own_plan_file_root`
(`tool-api/src/tool_invoker_impl.rs:298`) is the only carve-out and it requires an
`agent_id` — teammates only, never the main session.

**Land in:** `permission/src/policy.rs` (path-keyed allow ahead of the Plan
backstop) + a plan-file predicate in `permission/` fed by the session's plan path.
Keep deny/ask rules and the safety walks ahead of it, exactly as `LZe` does.

### P0-2 · `ExitPlanMode` never reads the plan file

Oracle `t6.call` @3513700: `let D = I ?? await Kke(n.agentId, n.storageV5)` — the
plan comes from `getPlan()` off disk when the model did not pass one; the approved
branch @3516828 then emits:

```
User has approved your plan. You can now start coding. Start with updating your todo list if applicable

## Approved Plan:
<plan>

Your plan has been saved to: <filePath>
You can refer back to it if needed during implementation.
```

plus `SSt(hasTaskTool)` @3509271 when the Agent tool is available and the output
style is default:

```
\n\nIf this plan can be broken down into multiple independent tasks, consider spawning named teammates with the Agent tool (pass a `name`) to parallelize the work.
```

LingXi `tools/plan/src/plan_mode.rs:525` reads `input["plan"]`, and nothing in the
tree injects it — `git grep normalize_tool_input` finds only comments. Since the
already-ported prompt tells the model *not* to pass a plan, the live behaviour is
always the empty-plan branch: `"User has approved exiting plan mode. You can now proceed."`
The `## Approved Plan:` path and `EXIT_PLAN_APPROVED_PREFIX` are dead code today.

**Land in:** `tools/plan/src/plan_mode.rs` (read the session plan file when
`input.plan` is absent; write it back when present, as the oracle's `Ykn` does),
the two missing tail lines, and `SSt`. Needs the plan path on `ToolUseContext` or
a `platform_api` seam — `ConversationOrchestrator::plan_file_path` already computes it.

### P0-3 · Tool `description` / `prompt` bytes

| Site | LingXi today | 2.1.266 |
|---|---|---|
| `EnterPlanMode.description` | `"Enter plan mode"` | `"Requests permission to enter plan mode for complex tasks requiring exploration and design"` @3902685 |
| `EnterPlanMode.prompt` | one-line stub mentioning `[PLAN MODE]` | `N8o()` @3898812 — 2 348 chars: "Use this tool proactively when you're about to start a non-trivial implementation task…" + `D8o()` "## What Happens in Plan Mode" + the GOOD/BAD examples + "## Important Notes" |
| `ExitPlanMode.description` | `"Exit plan mode"` | `"Prompts the user to exit plan mode and start coding"` @3512544 |

These are request bytes on every turn that advertises the tools, so this is the
single largest wire-byte divergence in the subsystem. `D8o()`'s tool-name slots
resolve to `Read`/`Grep`/`Glob` (or the shell forms when `oS()&&ei()`), `${As}` →
`AskUserQuestion`, `${iy}` → `ExitPlanMode`; `O8o()` appends `" (use the Agent tool instead)"`
only when the output style is default.

Also in this phase: `ExitPlanMode`'s output-schema field descriptions (`plan`,
`isAgent`, `filePath`, `hasTaskTool`, `planWasEdited`, `awaitingLeaderApproval`,
`requestId`) @3511807‑3512353, all absent from `EXIT_INPUT_SCHEMA`'s sibling.

**Verification for phase 1:** unit tests pinning each literal against a fixture
extracted by script (never by eye); a permission test that a `Write` to the
session plan path under `PermissionMode::Plan` returns Allow while a write to a
sibling path still asks; an ExitPlanMode test that seeds a plan file, calls the
tool with `{}`, and asserts the `## Approved Plan:` body and both tail lines.

---

## Phase 2 — goal surface drift (P1, live)

### P1-1 · `/goal` status rendering

Oracle `src_185633862.js`:

```js
if(e===""){let t=o.options.activeGoal;
  if(!t)return{type:"text",value:"No goal set. Usage: `/goal <condition>`"};
  let r=t.iterations===0?"not yet evaluated":`${t.iterations} ${x(t.iterations,"turn")}`,
      n=t.lastReason?`\nLast check: ${Hr(t.lastReason.trim())}`:"";
  return{type:"text",value:`Goal active: ${t.condition} (${r})${n}`}}
```

`Hr(t) = pt(t,"\n")` (`src_157781101.js`) — **first line only**.

LingXi `commands/core/src/goal.rs:212` renders elapsed wall time in the
parenthetical, adds a non-existent `"Evaluations: …"` line, prints the whole
`last_reason`, and returns bare `"No goal set"` for the empty-arg case. The module
doc flags the `lastReason` suffix as "NOT byte-verified" — it is now verified, and
it is wrong. The bare `"No goal set"` stays correct for the *clear* branch only.

### P1-2 · Idle goal check-ins (new in 2.1.266)

`src_162329786.js` @4166900‑4171500 adds a whole timer path LingXi has nothing for:

- `qmt = 60000` re-arm floor, `Cns = 2` backoff exponent cap, `xns = 3` idle cap.
- `Rns = " · idle check-ins paused until your next message"` and
  `Pns = " Claude Code won't wake this session for another check-in until the user sends a message, so say clearly where things stand."`,
  appended by `Mns` to the summary/body once `RUe(goal)` (`idleCheckinCount >= 3`).
- `HZ`/`Vmt`/`Ons` — arm an unref'd timer at `min($S, max(60 000, backoff − elapsed))`,
  re-arm without firing while the cap is reached, and skip a tick when a
  passive task-notification is already queued (`Ins`).
- `idleCheckinCount` bookkeeping on `activeGoal`, cleared by `vSt`.
- Armed in the evaluator's `finally` only when `!options.isNonInteractiveSession`
  (@4217700) — so this is live for the TUI/desktop hosts, not for `-p` runs.

`git grep -i idlecheckin` → 0 hits. The existing `GoalDeferralState` covers only
the turn-end half.

### P1-3 · `goal_status` attachment shape

Oracle emits four shapes: the sentinel `{type,met,sentinel:true,condition}` (`bKt`),
met `{met:true,condition,reason,iterations,durationMs,tokens}`, impossible
`{met:false,failed:true,…}` (@4211500), and not-met `{met:false,condition,reason}`
(@4212300). `fRn` (`src_160454523.js`) reads the last non-sentinel `met` record
back to render "Goal achieved".

LingXi `orchestrator/src/conversation/transcript.rs:1155` writes
`{kind,status:Set|Cleared,condition,iterations,duration_ms,tokens,last_reason,goal_state}` —
different field names, no `met`/`failed`/`sentinel`, and no not-met record. This
is persisted JSONL, so it is a transcript-format divergence, and `resume.rs`
already keys on these attachment kinds.

### P1-4 · Plan-mode boundary attachments

Three `isMeta` system reminders exist in the oracle and nowhere in LingXi's Rust,
even though `apps/cli/src/resume_truncation.rs:100‑103` already knows their names:

- `plan_mode_exit` @5432394: `## Exited Plan Mode\n\nYou have exited plan mode. You can now make edits, run tools, and take actions.` + `" The plan file is located at <path> if you need to reference it."` when it exists.
- `plan_mode_reentry` @5447210: `## Re-entering Plan Mode\n\n…` + the numbered "Before proceeding" block.
- `plan_file_reference` @5430355: `A plan file exists from plan mode at: <path>\n\nPlan contents:\n\n<content>\n\nIf this plan is relevant to the current work and not already complete, continue working on it.`

**Verification for phase 2:** byte-exact unit tests per literal; an idle-timer test
that asserts the cap re-arms without delivering and that the paused suffixes appear
exactly at the 3rd idle check-in; a JSONL round-trip test for each `goal_status`
shape; assert the attachments are `isMeta` and NOT `<system-reminder>`-wrapped where
the oracle leaves them bare.

---

## Phase 3 — P2 completeness

- **Plan-file identity.** Oracle names plan files by transcript **slug**
  (`^[a-z0-9][a-z0-9-]{0,119}$`, `WF(J())`), with `-agent-<id>.md` for subagents,
  `.workshop.md` siblings, collision reservation, and resume/fork copying
  (`src_160477403.js`). LingXi uses `<session-uuid>.md` with none of that. Porting
  the slug changes on-disk file names — safe here, since unreleased means no
  migration is owed, but it must land together with the `Zl` predicate in P0-1.
- **Read carve-out** — the `"…allowed for reading"` mirror of P0-1.
- **`tengu_goal_*` telemetry** — `tengu_goal_achieved` / `_failed` / `_evaluated` /
  `_cleared` / `_proposed` / `_proposal_available` / `_proposal_decided` /
  `_checkin_injected`, all carrying `origin` (`user` | `proposal_direct` |
  `proposal_approved`) via `queuedGoalOrigin` (`Mar`/`C` in `src_160454523.js`).
  LingXi emits only `goal_set` / `tengu_stop_hook_removed`.
- **Plan reminder subagent gating** — oracle `o = d8() && zx()==="default"` chooses
  the Explore/Plan-subagent Phase 1/2 text; LingXi keys only off
  `CLAUDE_CODE_DISABLE_EXPLORE_PLAN_AGENTS`. Same strings, different predicate.
- **`ExitPlanMode.checkPermissions`** — oracle asks `"Exit plan mode?"` (allow only
  when remote); LingXi's `check_permissions` returns Allow and defers to a
  dedicated `check_exit_plan_mode` gate seam. Confirm the gate actually renders an
  ask before calling this aligned.

---

## Not applicable / recommended divergences (do not port)

- **`ProposeGoal` — dormant twice over.** `isEnabled()` (`src_177262364.js`) bails
  on `Re()||Mn()` (non-interactive or remote) *and* on `!Gft()`, where
  `Gft(){return H("tengu_propose_goal",!1)}` is a **default-false** GrowthBook flag
  (`src_174482992.js`). LingXi's engine is the non-interactive shape, so the tool
  would never be advertised even if the flag flipped. Zero wire-byte impact —
  same finding as TR-06 in the 2.1.238 audit. Porting it is optional completeness,
  and it needs an interactive approval-dialog surface (`requestDialog`) plus
  `messageQueue.enqueue("/goal <cond>")` that LingXi's headless path lacks.
  `modelProposedGoals` staying accepted-but-unread is consistent with this.
- **Workshop / prototype offer blocks** in the plan reminder (@5421242, @5423218)
  are emitted only when `workshopOfferDocPath` / `prototypeOffer` are set, i.e.
  when the workshop and prototype skills are present. LingXi ships neither
  (`git grep workshop -- commands/ skill-api/` → 0), so the oracle itself would
  emit nothing here. Not a divergence; revisit if those skills are ported.
- **Ultraplan** (@5058000‑5067000, `src_182021089.js`) is remote plan mode running
  in Claude Code on the web — cloud session URLs, `## Approved Plan (edited by user):`
  round-trips, PR delivery. Recommend logging as Divergence(cloud-only) next to the
  existing accepted divergences rather than porting.
- **`[PLAN MODE]` / `[EXIT PLAN MODE]` markers** (`tools/plan/src/plan_mode.rs:53`)
  are a LingXi invention with no oracle counterpart. They are confined to the
  tool's `data` payload and a parity fixture (`test-harness/tests/parity_workflow_tools.rs`),
  not to model-facing text, so they cost no wire bytes — but they should be deleted
  with P0-2 if the fixture is re-cut, since nothing upstream produces them.

## Sequencing

Phase 1 → Phase 2 → Phase 3, each landing as its own commit with its own tests.
P0-1 and P0-2 share the plan-path seam and should land together; P0-3 is
independent and can land first as a pure-literal commit. Every literal goes in via
a script-generated fixture with chunk + offset recorded, never retyped by eye.


---

## Corrections to this document (found while implementing)

Two claims above were wrong when written; both are corrected in place in the
sections below, and both are recorded here because the *reasoning* that produced
them is the reusable part.

1. **"LingXi has no idle goal check-in" (P1-2) — WRONG.** The evidence given was
   `git grep -i idlecheckin` → 0 hits. That grep was literally true and the
   conclusion false: LingXi has had a complete idle check-in task since the
   2.1.238 port, spelled `sync_goal_checkin_idle_task` /
   `run_goal_checkin_idle_loop` (`orchestrator/src/conversation/model.rs:1750`,
   `hooks.rs:622`) — `checkin_idle`, not `idle_checkin`. A zero-hit grep for a
   FOREIGN identifier proves nothing about the ported behaviour; the search has to
   be for the behaviour. What 2.1.266 actually adds on top is narrower: the
   3-delivery cap (`xns`/`RUe`), the 60 s re-arm floor (`qmt`), the
   `min($S, max(...))` delay shape, the two pause suffixes (`Rns`/`Pns`), and
   `idleCheckinCount` with its reset (`vSt`).

2. **"Plan-file naming: oracle names plan files by transcript slug" (Phase 3) —
   incomplete.** The slug is not derived from the transcript in the normal case.
   `getPlanSlug(sessionId, seed)` (`src_160477403.js`) generates a RANDOM
   human-readable name: `B7t()` = `${adj}-${adj}-${noun}` from three wordlists in
   `src_159374504.js`, or `${ynt(seed)}-${adj}-${noun}` when a seed is supplied
   (`ynt` slugifies the first 4 words, lowercased, `[^a-z0-9]+`→`-`, 40 chars).
   It retries on collision against a primed listing of the plans directory and
   caches per session. So the file is `~/.claude/plans/brave-quiet-otter.md`, not
   `<session-uuid>.md`. This is NOT implemented.

## Implementation status

| Item | State |
|---|---|
| P0-1 plan-file write **and read** carve-out | implemented — `permission/src/plan_files.rs` (`PlanFileMatcher`, byte-locked allow reasons), `PermissionPolicy::plan_files` + `with_plan_files`, the allow placed after the deny/ask walks and the EDIT-READDENY guard and before every guard, the safety walk and the plan-mode ask |
| P0-2 `ExitPlanMode` reads/persists the plan file | implemented — reads the session plan file when no inline `plan`, persists an inline one first (`Ykn`), and the approved text now carries the saved-to pair, the teammate suffix (`SSt`) and the `## Approved Plan:` section LAST, as 2.1.266 orders them |
| P0-3 tool `description` / `prompt` bytes | implemented — `ENTER_PLAN_MODE_TOOL_PROMPT` (3 997 chars, script-generated from the executable), both `description()`s |
| P0-3b output schemas | implemented — `ENTER_OUTPUT_SCHEMA` / `EXIT_OUTPUT_SCHEMA` with all seven field descriptions |
| P1-1 `/goal` status line | implemented — iteration count in the parenthetical, no `Evaluations:` line, `Hr` first-line truncation, `No goal set. Usage: …` for the status branch only |
| P1-2 idle check-in cap / floor / pause suffixes | implemented — constants, `idle_cap_reached`, `next_idle_delay_ms`, `with_idle_pause_suffix`, `GoalDeferralState::idle_checkin_count` + `clear_idle_checkins`, wired into the existing idle loop and reset on every user prompt |
| P1-4 `plan_mode_exit` / `plan_mode_reentry` | implemented — renderers plus `plan_mode_exited` / `plan_mode_exit_pending` session flags (`NM`/`Vz`), emitted from both turn loops |
| P1-4 `plan_file_reference` | renderer implemented; **not wired** — upstream emits it from the post-compaction assembly (`KOe` → `_ts`), which is a separate seam |
| P1-3 `goal_status` attachment shape | **not started** — still `{status: Set/Cleared/Achieved/Failed}` rather than `{met, failed, sentinel, reason}`, and the not-met record is still absent |
| Phase 3 plan-file slug | **not started** (see correction 2) |
| Phase 3 `tengu_goal_*` telemetry | **not started** |
| Phase 3 reminder subagent gating | **not started** |
| Markers `[PLAN MODE]` / `[EXIT PLAN MODE]` | deleted, with their fixture entries |
| ProposeGoal + ultraplan | recorded as accepted divergences (user decision 2026-09-08) |

Literal coverage moved from 114/350 present to 138/350 over the same extraction
(`diff_literals.py`). The remaining misses are dominated by the two accepted
divergences (39 literals), oracle-internal log strings, and the interactive
`/goal` TUI dialog's input guides.

## Verification status — NOTHING HERE HAS BEEN COMPILED

`cargo check -p permission -p tool-plan -p core -p orchestrator -p command-core
--all-targets` was queued and waited ~25 minutes without ever acquiring the
build lock. `lsof target/debug/.cargo-lock` shows the holder is another Claude
Code session's `cargo test -p tasks` / `cargo test -p agent` run (different
shell-snapshot files), so the lock is not this session's to take and killing it
would clobber concurrent work. Disk is also at 100 % (2.3 GiB free;
`lingxi-code/target` is 187 GB and `/tmp/lingxi-teammate-target` is 74 GB), which
is itself a hazard — a full disk surfaces as spurious `could not compile` errors.

Consequently: every file parses (`rustfmt` round-trips each edited file, and the
repo was rustfmt-clean at HEAD so the only formatting changes are inside these
hunks), but **no type check, no test run, and no behavioural verification has
happened.** Treat the whole change set as unverified until a build lands. The
first things to check when it does:

1. `permission`: the new module compiles and `PlanFileMatcher`'s unit tests pass.
2. `tool-plan`: the two new statics, the plan-file read/write path, and the
   updated result assembly — plus a NEW test that the approved text is byte-equal
   to the 2.1.266 template (the literal spans a runtime concatenation, so the
   literal diff cannot see it).
3. `orchestrator`: the `plan_mode_turn_messages` signature change reached all
   seven test call sites and both turn loops.
4. A permission test that a `Write` to the session plan path under
   `PermissionMode::Plan` returns Allow with the byte-locked reason, while a
   sibling path still asks.
5. That the reminder's path, the matcher's path and `ExitPlanMode`'s path are the
   same string — they are three derivations today (orchestrator recomputes from
   the session id; the host builds the identity from the same inputs), which is
   exactly the shape that silently diverges. The slug work in Phase 3 must
   collapse them to one source.


---

## Verification (final, 2026-09-08)

All builds and tests ran against a private `CARGO_TARGET_DIR`
(`/private/tmp/lingxi-plangoal-target`), because the repo's shared
`target/debug/.cargo-lock` was held for hours by other Claude Code sessions'
`cargo test` runs — `lsof target/debug/.cargo-lock` names the holder, and
killing another session's run is not this session's call.

**Type check — `EXIT=0`** for `cargo check --all-targets` over
`platform-api`, `permission`, `orchestrator`, `engine-desktop`, `engine-mobile`,
`tool-plan`, `command-core`, `test-harness`.

**Tests — green.** The consolidated `--all-targets --no-fail-fast` run over
`platform-api`, `permission`, `tool-plan`, `command-core`, `orchestrator`
finished at **3765 passed / 2 failed**; both failures were mine, both are fixed,
and the affected binaries were re-run green afterwards (`orchestrator --lib`:
1128 passed / 0 failed; `orchestrator --test stop_hooks_test`: 14 passed / 0
failed; `platform-api --lib plan_slug`: 8 passed / 0 failed).

The two failures are worth recording, because both were tests pinning behaviour
that 2.1.266 changes:

1. `plan_file_is_excluded_from_post_compact_restore` asserted
   `restored.is_empty()`. The plan file is indeed excluded from the FILE-restore
   arm — but `KOe` composes `[...files, ...skills, ...planFileReference, …]`, so
   the plan now legitimately comes back through its own provider. The test now
   asserts what the exclusion actually means: no file-restore telemetry, and the
   one restored message is the `plan_file_reference` carrying the plan's REAL
   contents, not the stale read-file snapshot.
2. `idle_loop_exit_allows_a_new_deferral_stretch_to_rearm` timed out. The old
   loop slept `max(1)` ms; 2.1.266 `HZ` floors the re-arm at `qmt` = 60 s, so no
   real-time wait can observe it any more. The test now runs on the paused
   clock.

A third failure was caught by a test I wrote and was MY error, not the code's:
`slugify_seed` on `"[Pasted text #1] migrate the call sites"` yields
`migrate-the-call-sites`, not `migrate-the-call` — the stripped marker becomes a
space and is then dropped by `filter(Boolean)`, so it does not consume one of
`ynt`'s four words. The expectation was fixed in the generator, not the code.

### The final run (after reclaiming disk)

`/tmp/lingxi-teammate-target` (74 GB) was deleted with the user's approval,
taking the volume from 281 MiB free to 31 GiB, and the consolidated run then
completed: **172 test binaries, 3989 passed, 5 failed**. All five failures are
in `test-harness` and none of them come from this work:

| failing test | why it is not this change |
|---|---|
| `every_tool_file_declares_a_permission_result` | reads `tools/team/src/team.rs`, **deleted in commit `bd0abff79`** — absent at HEAD, so red independently |
| `every_tool_with_output_calls_truncate_or_opts_out` | same missing file, same commit |
| `denied_fqn_tool_use_yields_permission_denied_result_and_skips_server` | fails with `tool not found: mcp__mock__a` — a REGISTRY miss raised before any permission decision, and the plan-file carve-out is gated on `file_tool_kind`, which classifies an MCP FQN as `NonFile` and skips the block entirely |
| `production_prompt_bodies_match_normalized_2_1_238_manifests` | a 9-byte drift in the assembled system-prompt body; nothing in this change is reachable from `assemble_system_prompt` — `body_sections.rs` is untouched, and the one prompt-module file this change edits (`conversation/prompt.rs`) gains a new function and three visibility widenings, neither of which can move a byte |
| `production_output_style_bodies_match_normalized_2_1_238_manifests` | the same 9-byte drift in the shared body |

### `main` does not currently compile

A HEAD baseline could not be taken, and that is itself the finding: at
`b1a02ce08` (and still at `8567e5c6e`) `agent/src/handle.rs` references
`crate::permission_mode::SpawnBypassGates`, but `agent/src/permission_mode.rs`
at HEAD does not define it — the type exists only in another session's
UNCOMMITTED working copy (18 occurrences there, 0 at HEAD). A concurrent session
committed the caller without the definition, so `cargo build -p agent` fails on a
clean checkout. Worth flagging to whoever owns that change; it is unrelated to
plan/goal, but it means "is this red at HEAD?" is unanswerable for anyone right
now.
