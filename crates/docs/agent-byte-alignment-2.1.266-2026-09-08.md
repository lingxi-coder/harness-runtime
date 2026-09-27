# Agent subsystem — byte alignment vs Claude Code 2.1.266 (2026-09-08)

## Scope

The whole agent feature, model-facing first: the `Agent` tool (description
generator, input schema, dispatch guards, result rendering), the built-in agent
roster, the agent-definition catalog (markdown/JSON frontmatter, discovery,
precedence), the `agent_listing_delta` `<system-reminder>` catalog, and the
spawn-time gates that decide isolation and background execution.

## Method

1. 2.1.266 (`~/.local/share/claude/versions/2.1.266`, `VERSION:"2.1.266"`,
   `GIT_SHA:eb01d60909645dfca0bf35a844b946aabaa3a75e`) was split into its 1659
   `// @bun @bytecode` chunks under `~/.claude/oracle-chunks/2.1.266/`.
   The agent surface lives in two of them:
   * `src_160528463.js` — the constants chunk (`mt`/`Zar`/`elr`/`tlr`/`SW`/
     `nlr`/`dy`/`IKt`/`PKt`/`pRe`/`OKt`).
   * `src_162329786.js` — the main bundle: built-in definitions and the catalog
     loader (@1489000–1536500), the prompt generator `H2n`/`U2n`/`H4o`
     (@3554000–3572000), the tool object and `call` (@3572000–3607000), the
     result finalizer `bft` (@3530400–3533400), and the
     `agent_listing_delta` producer/renderer (`B3t` @5174317, renderer
     @5459577).
2. 2.1.245 was pulled from npm (`@anthropic-ai/claude-code-darwin-arm64@2.1.245`)
   and split the same way, because the port's agent code is annotated against
   2.1.238/2.1.245. Every 2.1.266 literal in the regions above was tested for
   RAW presence in the 2.1.245 chunk corpus, which isolates the true
   2.1.245 → 2.1.266 delta from build-to-build minifier churn. (A naive
   whole-binary string diff is useless here — the scanner mis-parses nested
   template literals and reports ~78% of all strings as "new".)
3. Each 2.1.266 literal was then checked against the port's 1886 tracked `.rs`
   files, matching both the plain and the Rust-escaped spelling, and every hit /
   miss was read in context before being called a finding.

Derived oracle facts are in `~/.claude/oracle-chunks/notes/agent-audit/`.

## Result

Sixteen divergences confirmed. Fifteen are fixed; the rest are recorded with their
blockers.

A correction to an earlier draft of this report: it said source precedence was
"already aligned", on the strength of the port's tier map matching `Z$`'s
`[built-in, plugin, userSettings, projectSettings, flagSettings,
policySettings]`. The tier ORDER is right, but three of the SOURCES that feed
those tiers are not populated at all (AG-17..AG-19). A literal sweep cannot see
that, and neither can reading the merge function — only reading the loader
that produces its input can. Everything else in the agent surface — the LONG/LEAN prompt
arms, the fork sections, the examples, agent-type normalization and the
ambiguity/deny/not-found errors, the depth/budget/concurrency caps, required-MCP
gating, worktree isolation, the `<usage>` trailer, one-shot built-ins, frontmatter
parsing, source precedence, the `agent_listing_delta` renderer — was checked
literal by literal and is already aligned.

---

## Fixed

### AG-01 (P1) — the `model` parameter description was two releases stale

`G4o()` (@3573156) rewrote it in 2.1.266 to name the configured default subagent
model. The port carried the 2.1.238 sentence, which describes a precedence the
resolver no longer has:

```
port    …Takes precedence over the agent definition's model frontmatter. If omitted,
        uses the agent definition's model, or inherits from the parent.…
2.1.266 …Takes precedence over the agent definition's model frontmatter and the
        configured default subagent model. If omitted, uses the agent definition's
        model, else the default (inherits from the parent unless a default subagent
        model is configured).…
```

Fixed: `AGENT_MODEL_PARAM_DESCRIPTION` in `tools/agent/src/agent.rs`.

### AG-02 (P2) — the coordinator arms of that description, and the argument drop

`G4o()` appends one of two suffixes on a coordinator session (@3573581 /
@3573661), and `call` clears the argument outright under
`CLAUDE_CODE_COORDINATOR_FORCE_WORKER_INHERIT_MODEL` (@3577560). Neither existed
in the port, so a coordinator advertised a `model` parameter it would then use.

Fixed: `project_agent_input_schema` + the early `parsed.model = None` in `call`.
Env: `LINGXI_COORDINATOR_FORCE_WORKER_INHERIT_MODEL`, with the `CLAUDE_CODE_*`
spelling as fallback (the convention `orchestrator/src/conversation/wiring.rs:558`
already uses for coordinator env).

### AG-03 (P2) — `CLAUDE_CODE_SUBAGENT_MODEL_FORCE` had no effect

`gSn()` drops `model` from the advertised schema under this env, and the LONG-arm
bullet drops its `"; the \`model\` parameter here overrides the definition for this
one call"` clause (@3569298). The port hardcoded both, so a deployment that pins
the subagent model still advertised the override and told the model it worked.

Fixed: `subagent_model_forced()` gates both sites.

### AG-04 (P1) — `workflow-subagent` was advertised to the model

The oracle declares `workflow-subagent` (`bn`) in the **workflow chunk**
(`src_173804794.js` @34591) and hands it straight to the workflow runtime;
`cre()` — the built-in roster every catalog is built from — has never contained
it. The port registers it in `builtin_agent_definitions()`, which is the input to
BOTH model-facing catalogs (`PoolSubagentSpawner::listing_entries` for the inline
Agent tool prompt, and `agent_listing_reminder_message` for the
`agent_listing_delta` reminder). Every LingXi session therefore emitted a line no
oracle session emits —

```
- workflow-subagent: Internal subagent for workflow script orchestration. (Tools: All tools except SendUserMessage, Agent, Workflow)
```

— and made an internal type selectable via `subagent_type`.

Fixed: `crate::agent_listing_entries` drops it, next to the existing `fusion`
filter. The definition stays in `builtin_agent_definitions` because that map is
also the workflow path's resolution registry; the listing is the only place the
oracle's split matters.

### AG-05 (P1) — a turn-limited agent reported that it had produced nothing

Two bugs on one path.

* `agent/src/runner.rs`'s max-turns fall-through published
  `{"reason":"max_turns_exhausted","max_turns":N}` as the WHOLE result, throwing
  away the assistant text the agent had already produced. The oracle finalizes a
  max-turns exit through the same `bft` as any other completion (@3532630) — the
  content is the last assistant message's text blocks either way.
* The harness NOTE `bft` prepends was entirely unported. `pRe`
  (`"NOTE: this agent stopped at its "`) has zero hits in the tree.

Net effect: a synchronous `Agent` call that exhausted its turns returned
`(Subagent completed but returned no output.)` — the model was told the run
produced nothing, with no hint that it had merely run out of turns and could be
continued. A green test (`completed_no_output_uses_marker`) was pinning exactly
that.

Fixed: the runner now builds the normal result and stamps the reason onto it, and
`AgentTool`'s completion arm prepends

```
NOTE: this agent stopped at its {N}-turn limit before finishing. {body}{continuation}\n
```

with `body` = `The text below is PARTIAL output; treat it as incomplete.` when
the agent produced text and `It was still calling tools and had produced no
report.` when it did not, and `continuation` =
` Send the agent a message (SendMessage) to let it continue from where it stopped.`
for everything except the one-shot built-ins (`PKt = {Explore, Plan}`). The note
is a harness block, so it does NOT go through the subagent output guard — matching
`bft`, where `Le` is built outside `x2n`.

The async path already says this in the `<task-notification>` summary
(`stopped at its N-turn limit (partial result; …)`, landed as AGT-08), so it is
left alone; see AG-10 for the residual.

### AG-06 (P2) — Explore/Plan and statusline-setup were registered unconditionally

`cre()` (@1520500) gates two of its pushes:

```js
if(!Ir())n.push(xnn);        // Ir() = CLAUDE_CODE_SAFE_MODE || --safe-mode
if(d8())n.push(b0,$Ee);      // d8() = !CLAUDE_CODE_DISABLE_EXPLORE_PLAN_AGENTS
```

Safe mode exists to keep a session from writing executable configuration, and
`statusline-setup`'s entire job is to write a `statusLine` command into settings.
The port registered all three unconditionally, so both kill-switches were inert
and the catalog the model sees could not be trimmed.

Fixed: `explore_plan_agents_enabled()` / `safe_mode_enabled()` in
`agent/src/builtins.rs`, with `builtin_agent_definitions_with_gates` keeping the
composition testable without touching process-global env.

### AG-17 (P1) — nested project agent directories were never discovered

`wQr` builds the projectSettings tier from `O5(kind, cwd)` (@1338241), which
walks UP:

```js
let r=i9e(yQr()).normalize("NFC"),o=kQr(n),d=i9e(n),p=[];
while(!0){
  if(Pf(d)===Pf(r))break;            // the home dir is the ceiling, NOT collected
  p.push(SN(d,".claude",e));
  if(o&&Pf(d)===Pf(o))break;         // the project root IS collected, then stop
  let C=_Qr(d); if(C===d)break;      // filesystem root
  d=C}
```

Every level from the cwd to the enclosing project root contributes a
`<dir>/.claude/agents`, and `Z$` then sorts that tier by `ITe` — the separator
count of `baseDir`, ascending — so under later-wins the DEEPEST directory wins.

The port read exactly one project directory, `<cwd>/<DOT_DIR>/agents`. In a
monorepo that means an agent defined at `packages/foo/<DOT_DIR>/agents/` was
invisible whenever the session ran from the repo root, and one defined at the
repo root was invisible from inside a package. Both are ordinary layouts, and
neither produced a diagnostic — the agent simply was not in the catalog.

Fixed: `catalog::project_agent_dirs` ports `O5` (pure, given cwd / home /
project root) and `catalog::agent_dir_precedence` hands the walk to
`load_agents_from_dirs` reversed — shallowest first — so later-wins reproduces
`ITe` without a second sort. The composition root passes `$HOME` as the ceiling
and `permission::set_cwd::project_root_of(cwd)` as the `kQr(cwd)` boundary.
Existence is not pre-checked: `load_agents_from_dirs` already treats an
unreadable directory as an empty contribution, which is `O5`'s ENOENT arm.

One behaviour change falls out and is correct: with the cwd AT `$HOME`, the walk
now collects nothing, so `~/<DOT_DIR>/agents` is loaded once as `userSettings`
instead of twice (the second time as `projectSettings`, which used to win).

### AG-21 (P1) — agent directories were scanned only at the top level

`vG(dir)` — the loader behind every agent tier — scans with

```
rg --files --hidden --follow --no-ignore --glob "*.md"
```

which RECURSES, and its non-ripgrep fallback `TQr` is an explicit recursive
walk that follows symlinks and keeps a `dev:ino` visited set so a symlink cycle
cannot hang it. It also applies a per-file cap (`wie = 1048576`) and logs
`loadMarkdownFilesFromDir: skipping <path>: not a regular file or exceeds
<limit> byte limit`.

`load_agents_from_dirs` used a single `read_dir`, so anything below the top
level — `<DOT_DIR>/agents/reviewers/api.md`, an ordinary way to group
definitions — was invisible, with no diagnostic, and there was no size cap at
all.

Fixed: `collect_markdown_files` walks recursively, resolves entries through
`metadata` (that is `--follow`), keys its visited set on the canonical path
(`TQr` prefers `dev:ino` and falls back to `realpath`; the two are
interchangeable for loop detection), and applies the 1 MiB cap. Results are
sorted, which claude leaves to ripgrep's traversal order: the port's caller
inserts later-wins into a map, so an unsorted walk would let filesystem order
decide between two files declaring the same `name`. Sorting pins that without
changing which definitions exist.

The symlink-cycle test does not merely assert a count — without the visited set
it does not terminate.

### AG-07 (P2) — the built-in web-fetch agent auto-backgrounded

2.1.266 moved the async decision into `vBo` and, doing so, factored the
AUTO-background arm behind `!Pw(n)`:

```js
let d=e.isCoordinator&&!o||e.forceAsync||!o&&r!==!1;
let y=r===!0||n.background===!0||!Pw(n)&&d;
```

`Pw(e)` (@1514599) is `e.source==="built-in"&&e.agentType==="web-fetch"`. That
agent answers a question the caller is usually waiting on — its own `whenToUse`
tells the model to run it in the foreground — so omitting `run_in_background`
must not background it. Only an explicit `run_in_background: true` or a
definition's `background: true` still can; both of those arms sit outside the
factor in the binary and outside it here.

Fixed in `AgentTool::call`. (Impact is bounded today: the web-fetch agent is
behind `LINGXI_WEB_FETCH_AGENT`, off by default.)

### AG-08 (P2) — a non-lean session saw the lean Explore description

2.1.266's listing formatter takes the lean flag:

```js
function U2n(e,n){let r=H4o(e),o=n&&e.whenToUseLean||e.whenToUse;return `- ${e.agentType}: ${o} (Tools: ${r})`}
```

and the producer computes it from the main-loop model
(`D=VU(YK(e.options.mainLoopModel))`, @5174317). `Explore` is the only
definition in 2.1.266 that declares `whenToUseLean` (`vto` full / `Cto` lean,
@1495xxx). The port stored only the LEAN text in its single `when_to_use` field,
so a NON-lean session rendered the lean description where the oracle renders the
full one — and no non-listing surface could reach the full text at all.

Live on LingXi rather than theoretical: `dh_simple_system_prompt` returns `false`
(⇒ non-lean) for every `PromptProfile::FullHarness` model — i.e. every
non-Anthropic provider — and for sonnet/haiku/claude-3/opus-4-0..4-7.

Fixed: `AgentDefinition` keeps the FULL text; `SubagentListingEntry` gained
`when_to_use_lean`, populated from `builtins::when_to_use_lean` (BUILT-IN
definitions only — a catalog agent that overrides `Explore` by name brings one
`description` and must render it on both arms); and `format_agent_line(entry,
lean)` picks between them per RENDER, keeping the JS `||` fall-through so an
empty lean variant does not render a blank description. Both call sites already
knew their model, so the `SubagentSpawner` trait is unchanged.

### AG-22 (P3) — the tool declared no `outputSchema`, so hook rewrites went unchecked

`V4o` is a union of the result shapes `call` can return. It is NOT advertised to
the model — its one consumer is the PostToolUse hook path, where a hook's
`updatedToolOutput` is validated against it and DISCARDED on mismatch, keeping
the original result (`turn_loop.rs`, `e.outputSchema?.safeParse(...)`). The
port's Agent tool implemented no `output_schema`, so ANY shape a hook returned
was substituted into an agent result unchecked.

I had recorded this as "actively risky — declaring it could fail live calls".
That was wrong, and reading the consumer rather than grepping for it is what
showed why: the schema never fails a tool call, it only gates a hook's
substitution.

Fixed with the two arms `call` actually emits (`completed`, `async_launched`).
The `remote_launched` arm is omitted on purpose: the port has no remote path and
cannot produce that status, so declaring it would only widen what a hook may
substitute. `harnessNoteCount` / `harnessTailCount` / `harnessSectionHash` are
declared because `D2n` declares them (all optional) even though AG-14 leaves
them unpopulated.

### AG-09 (P3) — the Explore inherit-cap kill-switch

2.1.266's `yX` short-circuits the cap ahead of the tier test:

```js
if(a.CLAUDE_CODE_DISABLE_EXPLORE_INHERIT_CAP)return"inherit";
```

`resolve_builtin_explore_model` implemented the cap but not the escape hatch.
Fixed, reading the env through `is_env_truthy` like every other bare `a.X` gate
here (it differs only for a value like `"0"`, truthy in JS, which nobody sets on
a kill-switch).

### AG-18 (P2) — `--add-dir` agent directories were never loaded

For `kind === "agents"` only, `wQr` adds a second projectSettings source: each
`Rp()` directory (the `--add-dir` set) contributes `<dir>/.claude/agents`,
tagged `fromAdditionalDirectory: true`, minus any the upward walk already
covered. `Z$` ranks those BELOW the ordinary project directories within the
same tier.

`agent::catalog::load_agents_from_additional_directory` existed with **zero
callers** — named, implemented, never wired.

Fixed: `agent_dir_precedence` takes the add-dir roots and places them between
the user tier and the project tier, skipping any the walk already covers. The
composition root passes `cfg.add_dir`. The single-directory loader is deleted
rather than left beside the new path, so there is only one way to load them.

(An earlier draft of this report said `EngineConfig` carried no add-dir list.
That was wrong — `cfg.add_dir` has been there all along, which is why this
turned out to be wiring rather than plumbing.)

claude compares REALPATHS when excluding an already-covered add-dir; this
compares the joined paths. They differ only when one directory is reachable
under two spellings, and then the port merely reads it twice — the ordinary
project entry still comes later and still wins, which is the answer claude
reaches by dropping the duplicate.

### AG-19 (P2) — the managed policy agent directory was never loaded

`wQr`'s TOP tier is `SN(Jb(),".claude",e)` — `<managed dir>/.claude/agents`,
tagged `policySettings`. `AgentSource::PolicySettings` existed in the port and
was threaded through the MCP and hook-trust paths, but nothing ever produced an
agent carrying it, so an org-provisioned definition simply did not exist.

Precedence matters here and is easy to get backwards: `Z$` applies
`[built-in, plugin, userSettings, projectSettings, flagSettings, policySettings]`
later-wins, so policy outranks EVERYTHING, `--agents` included. It is therefore
merged AFTER `merge_cli_flag_agents`, not alongside the directory tiers.

`wQr` gives this tier no `Fr(...)` / `ku("agents")` gate of its own — unlike the
user and project tiers — because org policy is not user customization. Only the
safe-mode / `--bare` arm suppresses it, and that already drops the whole disk
catalog upstream.

Fixed: `catalog::policy_agent_dir` + `catalog::merge_agents_later_wins`, wired
from the composition root through the existing
`settings_watch::managed_settings_dir()`.

(Another blocker of my own that did not survive contact: I had recorded this as
needing a managed-settings DIRECTORY helper the port lacked. It has had one all
along.)

### AG-20 (P3) — one file reachable from two directories loaded twice

`wQr` de-duplicates every loaded markdown file by INODE across all tiers at
once, keeping the first occurrence:

```js
let ve=await Promise.all(Se.map((De)=>SQr(De.filePath))), xe=new Map, Ie=[];
… if(je!==void 0){t(`Skipping duplicate file '${Le.filePath}' from ${Le.source} (same inode already loaded from ${je})`);continue}
let Oe=Se.length-Ie.length;
if(Oe>0)t(`Deduplicated ${Oe} files in ${e} (same inode via symlinks or hard links)`);
```

Without it, one definition symlinked or hard-linked into two of the directories
was parsed twice and landed under two different `source` labels, with only
map-insertion order deciding which the rest of the session saw.

Fixed: `load_agents_from_dirs` keys a cross-root set on `dev:ino` (the path off
Unix, the same substitution `TQr` makes for its directory visited set) and skips
repeats, logging each one. The messages go through `tracing`, so they carry the
port's wording rather than claude's — these are diagnostics, not model-facing
bytes.

The within-directory half of this finding was already closed by AG-21's sort:
two DISTINCT files declaring one name are not inode duplicates, so both load and
ordinary later-wins decides, deterministically.

---

## Not fixed — recorded with the blocker

### AG-13 (P2) — the 2.1.266 stop-pending spawn guard

New in 2.1.266 (@3578059):

```js
if(n.agentId!==void 0&&jH(n.agentId))
  throw new pE("This agent has been stopped and its stop is still completing; it cannot launch new agents.")
```

Blocker, and this one held up when I went back to test it the way the other
"blockers" failed to. In the oracle the set is filled by the KILL-ESCALATION
path `Xne`: a kill marks every agent id of the task stop-pending, arms a 10s
escalation and a 30s overdue timer, and a settle callback clears them; `N3`
clears an id again when a run starts. Three sites read it — the `Agent` tool,
the `Skill` tool, and shell exec, each with its own refusal copy.

The port has no window to hook. `kill_backing_task`'s own comment records why:
"a handler's status sink flips the row to `Killed` INSIDE `handler.kill(..)`",
so by the time `kill()` returns the task is already terminal. A
`stop_pending_agent_ids` set would be filled and drained inside one call — a
gate that can never fire, which is worse than its absence. Porting the guard
means porting `Xne`'s escalation machinery first, and that lives in the task
registry another session is actively committing into.

### AG-14 (P2) — the harness-note layer around agent results

`bft` returns `{harnessNoteCount, harnessTailCount, harnessSectionHash, content}`
and `Rae(content, harnessNoteCount, …)` splits notes from body downstream: the
kill path filters the `pRe` note out of `finalMessage` (@2100176), and
`TaskOutput` surfaces the remaining notes as `harnessHead` (@3754042). None of
that exists in the port (`harness_note` / `harness_tail` / `harness_head`: zero
hits).

AG-05 fixes the note the model actually loses today (the synchronous path). The
rest of the layer — the second `⚠ {notice}` note, the split, and the async
`harnessHead` — is left for a task-subsystem change, because on the async path
the turn-limit fact already reaches the model through the notification summary,
and adding the note to `outcome.result` would put it INSIDE `<result>`, which is
not where the oracle puts it.

### AG-15 (P3) — the `agent.spawn` plugin hook

New in 2.1.266: `_Bo` (@2955987) runs the spawn through a plugin **function
hook** that can deny it, rewrite the agent type / model / cwd / background flag,
and is re-checked against permission rules afterwards. Its six error strings
(@3585420–3587034) are all absent from the port.

Blocker: this is not an agent-subsystem gap, and it is not a small one. The
oracle runs plugin function hooks in a bundled JS worker
(`HOOKS_WORKER_URL: "/$bunfs/root/src/plugins/functionHooks/hooks-worker/hooks-worker.js"`).
The port has no such runtime — `functionHooks` / `hooks-worker` have zero hits
outside this report — and its `HookEvent` enum carries only the command-hook
events (`PreToolUse`, `PostToolUse`, `SubagentStart`, `SubagentStop`, …).
`agent.spawn` lands only after a function-hook runtime exists, which is a
subsystem port in its own right rather than a gap in this one.

### AG-16 (P3) — the remaining two residuals

**`cacheTtl` — deliberately NOT ported.** `sKt(e)` reads
`frontmatter.experimental.cacheTtl` (key case-normalized to `cachettl`, value
checked against `k_e`) onto the definition. The port has no prompt-cache TTL
concept ANYWHERE: no request-side `cache_control` breakpoint selection, no
`1h`/`5m` ephemeral setting — the `cache_control` occurrences in
`llm-runtime/src/stream_accumulator.rs` are response parsing. Adding the field
costs an edit to all 74 `AgentDefinition` literals and produces a value nothing
can ever read, which is the "named, computed, never wired" shape this audit
exists to find, not to create. It becomes worth porting the moment a cache-TTL
seam exists on the request path.

**`forceAsync`** — see AG-07; the reason is recorded on
`should_run_in_background`.

## Preserved LingXi divergences

Not aligned, by standing decision: the absent `claude-code-guide` and `claude`
catch-all built-ins (multi-provider), the `fusion` agent surface and its listing
entry, `.lingxi/agents/*.md` in place of `.claude/agents/*.md`, `LINGXI_*` env
branding, and the deferred remote/CCR isolation path.

## Verification

`cargo check` clean across `platform-api`, `agent`, `tool-agent`, `tasks`,
`tool-skill` (`--all-targets`) and the `orchestrator` lib.
`cargo test -p platform-api -p agent -p tool-agent --no-fail-fast`:

| suite | before | after |
|---|---|---|
| `agent --lib` | 405 passed, 8 failed | 425 passed, 2 failed |
| `tool-agent --lib` re-run | — | 165 passed |
| `tool-agent --lib` | 161 passed, 0 failed | 166 passed, 0 failed |
| `platform-api --lib` | 300 passed, 0 failed | 302 passed, 0 failed |

`cargo check -p engine-desktop` is clean too — the composition root is the only
caller of the new discovery seam.

`orchestrator`'s **lib test target** could not be built: another session's
uncommitted `orchestrator/src/prompt/goal_checkin.rs` is missing a field in a
`GoalDeferralState` initializer. The lib itself compiles, so the one
orchestrator change here (the `agent_listing_delta` renderer taking the
main-loop model's lean flag) is checked; its tests are not, and none of them
assert on the Explore description.

Six of the eight `agent` failures were the AG-04 fix landing: the listing tests
took `builtin_agent_definitions().len()` as the expected LISTING length, so they
had encoded the extra `workflow-subagent` line. They now count only the LISTED
built-ins, and a new test
(`agent_listing_entries_never_advertises_the_workflow_subagent`) asserts the
split on the NAME rather than on a count, so a later roster change cannot
quietly re-advertise it.

The remaining two — `permission_mode::tests::confined_arm_shadows_the_bypass_arm`
and `…::confined_run_refuses_a_definition_declared_escalation` — are NOT from
this change. `agent/src/permission_mode.rs` is another session's in-flight work
(+302/−6 uncommitted, mtime during this audit, with its own
`cargo test -p agent -- permission_mode` running); the file imports only
`AgentPermissionMode` and `PermissionMode` and touches nothing in this change.

Tests were added for behaviour that had none:

* `turn_limited_agent_reports_the_limit_and_keeps_partial_output` — the note
  fronts the report, the partial text survives, and the no-output marker is gone.
* `turn_limited_one_shot_builtin_omits_the_continuation_tail` — the `PKt` arm and
  the text-less `Dt` arm, asserted as an exact string.
* `roster_gates_withdraw_statusline_setup_and_the_explore_plan_pair` — both
  `cre()` gates, including that Explore and Plan move together.
* `agent_schema_projection_covers_coordinator_and_model_force` and an assertion
  on the `model` description in `agent_schema_requires_description_and_prompt`.
* `format_agent_line_renders_the_lean_variant_only_on_the_lean_arm` — both arms
  plus the empty-lean fall-through; `a_catalog_override_of_explore_carries_no_lean_variant`
  — an override renders its own text on the lean arm too.
* `background_decision_ports_vbo` — every arm of `vBo`, including that `!Pw(n)`
  gates only the implicit default.
* `explore_inherit_cap_kill_switch_restores_plain_inherit` — with a premise
  assertion that the session would otherwise be capped, so a pass cannot come
  from the model being under the cap anyway.
* `agent_files_are_found_in_subdirectories` — a nested definition loads, a
  non-markdown neighbour and an over-cap file do not;
  `a_symlink_cycle_does_not_hang_the_scan`, which fails by hanging rather than
  by asserting if the visited set is dropped.
* `agent_dir_precedence_places_add_dirs_below_the_project_tier` — the tier
  position and the dedup against the walk.
* `output_schema_covers_every_status_the_tool_emits` — both emitted shapes
  satisfy their arm's `required` set and every emitted key is declared, plus the
  negative half that the omitted remote status is not accepted.
* `one_file_reachable_from_two_dirs_is_loaded_once` and its negative half
  `two_distinct_files_with_one_name_still_resolve_by_precedence`;
  `merge_agents_later_wins_replaces_in_place` and `policy_agent_dir_sits_under_the_managed_root`.
* `project_agent_dirs_walks_up_to_the_project_root` — all three stop conditions
  (project root, home ceiling, filesystem root) and the returned order;
  `agent_dir_precedence_puts_the_deepest_project_dir_last`; and
  `a_nested_project_agent_overrides_a_shallower_one`, which asserts the OUTCOME
  through the real loader on a real temp tree rather than the ordering alone.

`completed_no_output_uses_marker` still passes: its fixture is
`{"reason":"max_turns_exhausted"}` with no `max_turns`, which is not a shape the
runner emits, so it exercises the genuine no-output path rather than the
turn-limit one.

Builds ran with a private `CARGO_TARGET_DIR` — the workspace `target/debug/.cargo-lock`
was held by a pile-up of eight idle cargo/rustc processes from other sessions
(all at 0% CPU, the oldest 35 minutes), and the volume was at 100% with 2.2 GiB
free when this started.
