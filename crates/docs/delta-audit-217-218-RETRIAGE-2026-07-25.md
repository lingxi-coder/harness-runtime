# delta-audit 217→218 — re-triage, 2026-07-25

Re-verification of the 11 items `delta-audit-217-218-2026-07-23.json` lists as
`open`, against `main` @ `72b281387`. Each was checked at its **behaviour
site**, not by symbol name — see the methodology warning at the bottom, which
cost three false results on the first pass.

Net: **4 already done, 1 needs no change, 6 genuinely open** — and of those 6,
three are one architectural cluster and one is blocked on a prerequisite.

## Closed since the audit (4)

| id | evidence |
|---|---|
| `HOOKS-ORIGIN-TRUST` (HIGH, security) | `agent/src/hooks_trust.rs` exists AND is wired at the subagent call site — `agent/src/runner.rs:150` `agent_hooks_origin_trusted(...)` with `report_untrusted_hooks(..., HooksTrustSurface::Subagent)`. |
| `HOOK-TRUST-AGENT-FRONTMATTER-ORIGIN` (HIGH, security) | Same gate wired on the main thread — `apps/engine-desktop/src/lib.rs:8144`, `HooksTrustSurface::MainThread`. Both surfaces the audit named are covered. |
| `tengu_repair_double_escaped_unicode` (MEDIUM) | `llm-runtime/src/unicode_repair.rs` — its own header names the port as claude-code `jYd` / `L6s` (2.1.218), "successor to 2.1.217's `sOo`", i.e. exactly this item. It rewrites argument values, not telemetry. |
| `canonicalModel` (LOW) | `apps/cli/src/stream_json.rs:874-908` emits `"canonicalModel"` with a `(cc 2.1.218)` provenance comment, plus a lock test at `:1504`. |

## No change required (1)

`SWEEP-CRITICAL-PERMISSION-SANDBOX-LITERALS` — the audit itself concluded the
port sites are already aligned.

## STATUS UPDATE 2026-07-26

Four of the six are now CLOSED. This section's original text is kept below for
the evidence chain; read this block first.

- The forked-skill cluster (`frozenCommandDenies`, `forkedSkill`,
  `forkedSkillName`) — **CLOSED** across `40e91c38f` … `d76739609`. The premise
  below ("no Rust substrate") was true when written and wrong when acted on:
  `AgentTool::dispatch_async` → `SubagentSpawner::spawn_async` →
  `BackgroundAgentSpawner` had been wired at the composition root all along.
  `context: fork` now forks (background AND synchronous), applies the skill's
  scoping, persists it, and refuses to resume without it — six byte-exact
  refusals. `frozenCommandDenies` is captured and persisted; its union has no
  in-process caller BY CONSTRUCTION (verified: `PolicyPermissionGate` holds a
  boot snapshot, so frozen/live/boot cannot drift within a process) and is what
  a cross-session resume reads.
- `trust_root` — **CLOSED** (this wave). Its prerequisite was the point: the
  `set_cwd` control request now exists (`permission::set_cwd` +
  `apps/cli/src/run.rs`), with the full needs_trust → echo handshake, and
  `trust_root` is the enclosing project root offered alongside it.

- `tengu_left_arrow_editing_guard` — **CLOSED** (`91903a009`). Its prerequisite
  was the point: the ←-on-empty gesture did not exist, so the guard would have
  been a debounce for nothing. Both landed together —
  `tui_core::left_arrow_gesture` (claude `W_p`/`G_p`, read verbatim) plus the
  composer wiring.

Still open (1): `tengu_refusal_fallback_notice_collapsed`. Its prerequisite is
a refusal-fallback CASCADE — `suppressedCount` counts the intermediate hops of
a multi-hop chain, and this port has a single hop behind a once-per-session
latch, so there is never a second notice to retract the first. (An earlier note
here named "move refusal detection into the stream" as the prerequisite; that
was wrong — provisional-ness comes from a hop that a LATER hop may supersede,
not from an open stream.) The `tengu_refusal_retraction_*` family is NOT a
prerequisite: it belongs to the remote-session client, an accepted divergence.
Full decomposition in `docs/refusal-retraction-DECOMPOSITION-2026-07-26.md`.

## Genuinely open (6) — ORIGINAL, superseded by the block above

### The forked-skill cluster — one gap, not three

`frozenCommandDenies` (MEDIUM, security, XL), `forkedSkill` (MEDIUM, security,
L), `forkedSkillName` (MEDIUM, L).

All three depend on a subsystem that does not exist. `tools/skill/src/skill.rs`
says so in its own header: the forked-agent execution path
(`executeForkedSkill` → `runAgent`, `prepareForkedCommandContext`, progress
streaming, `createAgentId`) has **no Rust substrate**; the tool does inline
resolution and metadata surfacing only.

So this is a documented architectural gap, not three oversights. It should be
scheduled as one piece of work — and `frozenCommandDenies` in particular is a
permission-scoping mechanism, so it must land WITH the fork path, never after
it: a fork that executes before its deny-freeze exists would run with the
parent's live rules instead of the frozen set.

### Blocked on a prerequisite

`trust_root` (LOW, S) — an additive field on `set_cwd`'s `needs_trust`
response. But `set_cwd` does not exist as a control request at all (repo-wide,
the only `set_cwd` hit is an unrelated `set_current_dir` in a TUI test). The
audit flagged this as "subordinate to a pre-217 gap" and that holds: the field
cannot be added to a response the port never sends. Do the `set_cwd` trust flow
first.

### Blocked on a missing prerequisite (2) — CORRECTED 2026-07-25

I first listed these as "independently actionable". That was wrong, and the
error is worth naming: I judged reachability in the ORACLE (the flag defaults
true, that code path runs) instead of asking whether the PORT has the thing
being guarded. Re-verified at the behaviour site — both guard something that
does not exist here, so implementing either yields code that can never execute.

- `tengu_left_arrow_editing_guard` (LOW, M) — a debounce for the **←-on-empty
  gesture** (open the agents panel / detach). **The port has no such gesture.**
  Every `KeyCode::Left` handler in `tui/` moves the cursor:
  `bottom_pane/mod.rs:1311` (`composer.move_left()`), `vim.rs:125` and `:243`,
  `rewind_picker_view.rs:246`. There is no left-arrow → agents-panel or →
  detach binding anywhere. The audit's own sketch says so: *"Only worth doing
  together with the missing ←-on-empty gesture."*

- `tengu_refusal_fallback_notice_collapsed` (LOW, L) — a queue that collapses
  **provisional** `model_refusal_fallback` banners when a later banner retracts
  their uuid. **The port emits no provisional banners and has no retraction
  subsystem.** `refusal_fallback_latched` (`conversation.rs:978`) fires the
  banner at most ONCE per session, and repo-wide there are zero hits for
  `retractedMessageUuids` / `retracted_message_uuids` / `suppressedCount` (the
  two `provisional` matches are unrelated plugin-marketplace clone-dir
  naming). With one un-retractable banner there is nothing to collapse. The
  sketch agrees: *"the port emits no provisional banners today, so there is
  nothing to collapse."*

Schedule each WITH its prerequisite, never before it. A guard shipped ahead of
the thing it guards is not partial coverage — it is dead code that reads as
coverage.

## Methodology warning for the next pass

Two traps hit during this re-triage, both of which produced **false "absent"**
results:

1. **`grep -E` with `\|`.** In extended regex, alternation is `|`; `\|` matches
   a LITERAL pipe. A batch probe using `-E 'a\|b'` reported 0 hits for three
   items that were all present. Use `|` with `-E`, or drop `-E`.
2. **Grepping the oracle's symbol name.** `tengu_repair_double_escaped_unicode`
   is ported as the module `unicode_repair`, and `canonicalModel` sat behind a
   `build_model_usage_block` helper. A name that does not appear proves
   nothing; check the behaviour site the audit cites.

Both are the same underlying error: **a negative search result is not
evidence.** Every "absent" claim in this document was confirmed by reading the
site, not by a failed grep.
