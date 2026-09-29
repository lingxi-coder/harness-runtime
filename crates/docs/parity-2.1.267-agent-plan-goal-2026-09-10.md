# agent / plan-goal vs 2.1.267

The last two subsystems in the 2.1.267 sweep. Both were aligned at **2.1.266**,
one release back, so this is a narrow delta rather than a re-audit.

Oracles: `~/.local/share/claude/versions/2.1.266` and `…/2.1.267` (sha256
`a681f300…`). Method as in the sibling docs — naive diff, then raw-byte
re-verification of each candidate against 2.1.266, then a port check with Rust
escape spellings, with the port haystack self-checked first
(`when_to_use_lean` 48 hits, `tengu_goal_cleared` 10).

650 new prose strings across the whole binary; 24 mention agent/subagent/skill
and 2 mention plan or goal. Of those 26, **6 were false positives** and 18
survived.

---

## plan / goal — nothing

Both candidates were false positives: the rejected-plan reminder and the
`ProposeGoal` description are present verbatim in 2.1.266. **Zero genuine delta.**
The accepted divergences stand untouched — ProposeGoal is still doubly dormant
upstream and still not ported, and ultraplan is still out of scope.

## agent — one new feature, and it is dormant upstream

Fourteen of the eighteen belong to a single mechanism new in 2.1.267: a
**subagent hand-back contract**. Instead of a subagent's trailing text becoming
its result, the child delivers a report through a dedicated tool:

```js
var p_ = "SubagentHandback";
{ alwaysLoad:!0, userFacingName(){return p_},
  get inputSchema(){return Ses()}, get outputSchema(){return kes()},
  isReadOnly(){return!1}, classifierOnly(){return{onBlock:"flag"}},
  async description(){return r5n}, async prompt(){return o5n},
  toAutoClassifierInput(e){return `${s5n}: ${e.message}`}, … }
```

with copy for every edge: *"Deliver your final report to the agent that spawned
you, once, as your last tool call. The only way your report reaches it."*,
*"The subagent ended without delivering a report through …"*, *"Nothing was
sent: the agent that spawned you is no longer running."*, *"This agent has not
reported yet: it is waiting on its own background work…"*, plus two SECURITY
WARNING forms for a report the auto-mode classifier blocked or could not review.

`SubagentHandback` is **0 hits** in this port.

**⛔ That is not a gap today.** The tool is gated:

```js
function aue(){ let e = Rn.CLAUDE_CODE_SENDMESSAGE_HANDBACK;
                if (e !== void 0) return e;
                return H("tengu_lively_waffle", !1) }        // ← default FALSE
function mln(e){ return e.mode === "auto" && aue() }          // ← and auto mode only
```

The statsig default is **false**, and even when on it applies only in auto mode.
On a host with no statsig backend — which is this port's situation, the same
reasoning already recorded for the Artifact tool's `tengu_cobalt_plinth` — the
feature is off, so a default install of 2.1.267 behaves exactly as this port
does. Building the hand-back subsystem now would be porting a mechanism upstream
does not itself run.

**Revisit when** the gate flips, or `CLAUDE_CODE_SENDMESSAGE_HANDBACK` shows up
in a real environment, or LingXi wants the feature on its own merits. At that
point note that it is not just a tool: it carries an enforcement path (a
subagent that ends without calling it), a safety review of the report, and a
`'send' | 'flagged' | 'withheld'` disposition recorded for telemetry.

### The other four

| string | disposition |
|---|---|
| `[plugin-skill-list] degraded to empty: list-plugins` / `[plugin-skill-search] … no user:plugins scope in this session` | the claude.ai plugin surface (`user:plugins` scope) — accepted divergence |
| `a changed provider (pinned: who provides the agent is a fact)` | plugin function-hook API — classified, not to build |
| the GitHub App installation token line | cloud-runner git surface — accepted divergence, alongside `--use-anthropic-git-proxy` |

---

## `tengu_goal_evaluated` — LANDED (`df42fedc5`)

`src_163219561.js` @4343091, in the evaluator's `finally`:

```js
if (Fe) {                                             // an active goal existed
  let Qe = p.abortController.signal.aborted,
      Je = We === "met" || We === "not_met" || We === "impossible";
  i("tengu_goal_evaluated", {
    outcome: u(We ?? (_e ? "error" : Qe ? "cancelled" : "absent")),
    durationMs: Date.now() - D,                       // D = the QUERY start
    iterations: Fe.iterations + (Je ? 1 : 0),
    parentAborted: Qe,
    origin: we(Fe.origin),
    ...Xe })
}
```

Four details that are easy to get wrong:

* **`iterations` is `+1` only for a real verdict.** met / not_met / impossible
  increment; error, cancelled, absent and deferred do not. This port's
  `record_goal_evaluation` increments the STORED count on every path, so the
  telemetry value is computed from the count read BEFORE the evaluation.
* **`...Xe` is not general.** It is set in exactly one branch — the deferred one
  — as `{activeAgents, activeShells}`. Everywhere else it contributes nothing.
  Both counts run over the same `Ofr`-filtered list under two disjoint
  predicates (`A9t` = the four agent-ish types, `v9t` = `local_bash`), so the
  port counts both by predicate rather than as a total and a remainder.
* **The seventh outcome is `deferred`**, and this port already had that branch.
* 🚨 **`D` is stamped at the TOP of the query generator, not where the goal
  evaluation starts.** An earlier version of this section said the opposite.
  `durationMs` is therefore the elapsed time of the whole query up to the stop
  dispatch — the same base `tengu_stop_hook_error`'s `duration` uses. The port
  had no clock at that seam; `CompactionRuntime::query_started_at` now sits
  beside `turn_start_output_baseline`, whose three store sites already are the
  port's spelling of that point.

### 🚨 The retracted blocker

This section previously recorded `parentAborted` as **blocked**, on the grounds
that threading it would reach "both turn loops". That was asserted from the
shape of the problem without reading the call graph, and it is wrong:
`turn_loop::execute_one_turn` never calls `handle_stop_at_end`. Only
`drivers/mod.rs` does, at ten sites, and it already holds `user_cancel`.

Threading it found three different answers, which is the part worth keeping:
the streaming driver has `user_cancel` as a local, `try_run_turn_cancelable`
owns its token, and the non-cancelable `try_run_turn` has none at all — so it
passes a commented `false`, not a stub. `StreamingTurnState` carries the token
for the two firings that reach `&ConversationOrchestrator`, which owns no
cancellation of its own.

## `origin` — the "always user" note was one setter short (`047d5968e`)

All three `tengu_goal_*` events carry `origin`, and the port hardcoded `"user"`
with a note saying upstream reads it from `queuedGoalOrigin`, which only holds
`ProposeGoal`'s two spellings — a tool LingXi does not ship.

Resolving `y(e,t)` (`src_161508826.js`) confirms half of that: `"user"` is a
literal fallback, so the field is never absent, and the two `proposal_*` values
really are unreachable. But `mon` (`src_182607998.js`) stamps
`origin:"restored"` on every goal a resume recovers, and this port has a resume
fold — so a resumed session had been reporting its goal as freshly set.

`origin` is not part of the persisted goal shape upstream, so it is `serde(skip)`
here and the resume fold re-derives it. Compact metadata is the exception:
upstream dumps the whole in-memory goal across a boundary, so
`CompactActiveGoalState` mirrors the field.

⚠️ **Not a defect:** the port restores a goal's `iterations` and `setAt` where
`mon` resets them to 0/now. That follows from `goal_state`, already recorded on
`GoalStatusAttachment` as a deliberate LingXi addition — upstream resets because
it can only recover the condition string from the transcript.

## Where the 2.1.267 sweep now stands

| subsystem | verdict |
|---|---|
| task | re-audited; 3 of 4 handoff findings struck, 1 real defect recorded with its blocker |
| skills | first-ever audit; 3 defects fixed, name-set lock added |
| mcp / plugin | re-audited from 2.1.251; MCP-01 and MCP-02 fixed, MCP-03 open pending a scope decision |
| cron / compact / teammate / orchestrator | swept; dominated by out-of-scope surfaces, nothing structural |
| **agent** | **swept; one new feature, dormant upstream** |
| **plan / goal** | **swept; zero delta** |
| permission | not swept here — `scripts/checks/perm_verify_literals.py` is the better instrument for that crate |
