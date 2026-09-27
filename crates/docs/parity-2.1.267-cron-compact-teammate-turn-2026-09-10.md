# cron / compact / teammate / orchestrator vs 2.1.267

Delta sweep for the four subsystems whose baselines were 2.1.261–263. Same
oracles and method as
[`parity-2.1.267-mcp-plugin-2026-09-10.md`](./parity-2.1.267-mcp-plugin-2026-09-10.md):
2.1.251 (sha256 `625869b0…`, matching `REMAINING.md`) against 2.1.267 (sha256
`a681f300…`). Using 251 as the old side over-reports for these four — they were
aligned later than that — but every candidate is port-checked anyway, so the
over-report is filtered rather than believed.

155 candidates → 31 dropped by raw-byte re-verification against 251 (**20%
false-positive rate**) → **119** genuinely new and absent from the port. The
port haystack was self-checked first (`tengu_goal_cleared` 8 hits,
`checkin_idle` 35) so the miss counts mean something.

**Headline: these four are in good shape. The delta is dominated by surfaces
already classified out of scope, and produced one real finding, which turns out
to be a deliberate divergence rather than a gap.**

| bucket | count | what it actually is |
|---|---|---|
| turn / orchestrator | 64 | overwhelmingly the plugin **function-hook API** (`next(e)`, `next.to`, `turn.complete`, hook veto/skip copy) — classified, not to build |
| compact | 33 | same: `$.session.compact`, hook-ordering copy, plus SDK prefix-cache telemetry docs (`expected_rebuilds`, `misses`, `recache_tokens_if_cold`) |
| cron / schedule | 4 | nothing structural |
| teammate | 1 | nothing structural |

That `cron` yields 4 and `teammate` 1 across sixteen releases is the useful
result here: the 2026-09-06 cron audit and the 2.1.263 teammate work are holding.

---

## TURN-01 — two entries of the interrupt-marker table are absent, and should stay absent

The port carries this upstream constant table (`src_160367958.js` @4358)
three-fifths:

| upstream constant | port |
|---|---|
| `[Request interrupted by user]` | ✅ 10 hits |
| `[Request interrupted by user for tool use]` | ✅ 5 hits |
| `The user doesn't want to take this action right now…` | ✅ 3 hits |
| `[Tool call did not complete: the turn was ended to deliver the message that follows. Nothing refused it; re-run it if still needed.]` | ❌ 0 |
| `[Tool call skipped: the turn ended to deliver the message that follows before this call ran. …]` | ❌ 0 |

Three of five ported is the shape that normally means "someone missed two rows".
It is not that here.

Both missing strings describe a mechanism upstream has and this port
deliberately does not: **ending a turn, mid-tool-call, to deliver an inbound
message.** The port queues instead — a message lands in
`LocalAgentTaskState::pending_messages`, the agent parks
(`registry.rs:2908`, `is_parked = true`), and the queue drains as a digest when
it comes to rest. `tasks/src/observer.rs`'s own module doc names the choice:
*"Independent observer task creation and **non-interrupting** activity
delivery."*

⛔ **Do not port these two strings.** With no interrupting-delivery path they are
dead bytes, and a `[Tool call skipped: …]` marker that nothing can ever emit is
exactly the "named, computed, never wired" shape these audits exist to find.

**Reopen when** an interrupting delivery path is actually built. At that point
both strings are needed together, and the distinction between them matters: `ow`
is for a call that had already STARTED, `gle` for one that had not run yet.

## What was checked and found clean

- `cron` — nothing structural in sixteen releases. The accepted divergences from
  the 2026-09-06 audit (per-task `expiresAt` over a global 7-day expiry;
  `expiresAt` / `sessionId` not being oracle fields) are untouched by 267.
- `teammate` — one candidate, nothing structural.
- `compact` — every genuinely-new string belongs to the function-hook API or SDK
  telemetry documentation, not to compaction behaviour.

## Not re-derived here

The 267 deltas for `agent` and `plan/goal` (baseline 2.1.266, one release back)
were out of this sweep's scope, and `permission` (2.1.263) was not swept —
`permission`'s own literal-regression gate
(`scripts/perm_verify_literals.py`) is the better instrument for it.
