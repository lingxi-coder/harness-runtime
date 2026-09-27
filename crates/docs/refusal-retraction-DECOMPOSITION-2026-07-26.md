# `tengu_refusal_fallback_notice_collapsed` — decomposition, 2026-07-26

**This document's first revision named the wrong prerequisite.** It said the
block was that this port decides refusal at response terminal rather than
in-stream, so a banner could never be provisional. The response-terminal
observation is true but it is not what makes a banner provisional. Corrected
below from the binary.

Verified against `main` @ `9be0f1711`.

## What actually makes a banner provisional

A refusal-fallback **cascade**. 2.1.220 @232960600 carries a multi-hop chain:

```js
firstReachableStage(remaining, s => resolveStage(s, model, triedModels))
logSkippedStages({stages, chainLength, firstStageIndex, model, apiRefusalCategory, requestId})
cursor = {remaining: o.remainingChain, nextHopIndex: n.nextHopIndex + 1,
          triedModels: [...], chainLength, triggerRequestId, triggerCategory}
routeMatched === "chain"
```

and an episode state beside it (@232961500):

```js
{inEpisode, cursor, pendingNotice: {originModel, servingModel,
  retractedMessageUuids, refusedUserMessageUuid, apiRefusalCategory, requestId,
  provisionalFlushUuid}}
```

`Jad` MERGES a new notice onto the pending one, folding the previous hop's
`provisionalFlushUuid` into the accumulated `retractedMessageUuids`. `Xad`
hands out the pending notice while stamping it provisional; `Oks` settles it
only if it was never flushed provisionally.

So the sequence is: model A refuses → notice emitted PROVISIONALLY (hop 1 may
not be the end) → model B also refuses → the new notice names hop 1's uuid in
`retractedMessageUuids` → `j0m` sees the incoming banner retract the held one,
drops it and increments `suppressedCount` → the settled banner reports
`tengu_refusal_fallback_notice_collapsed {suppressed_count, emitted_via}`.

**`suppressedCount` counts intermediate hops of a cascade.** With one hop there
is nothing to collapse, by construction.

## Why this port cannot produce one

`OrchestratorConfig::refusal_fallback_model` is a single `Option<String>`, and
`maybe_swap_to_refusal_fallback` (`conversation.rs:3944`) latches once per
session: the first refusal swaps to that one model, every later refusal takes
the terminal arm. One hop, no chain, so no second notice ever names the first.

## Two DIFFERENT subsystems share the vocabulary — do not conflate them

The `retracted_message_uuids` field appears in two places, and only one is
about the local turn loop:

- **`j0m`** (@246308542) — inside the SDK/headless query loop (`V0m.submitMessage`).
  This is the collapse queue, and it is what the audit item names.
- **`_ui` / `fzf` / `FOn`** (@243016900-243019900) — inside `useRemoteSession`,
  claude-code's REMOTE-session client (SSE reconnect, `session_stale_relogin`,
  `truncation_harvest` paging). It applies retraction signals to drop messages
  and evict in-progress tool uses, and it authenticates them: `fzf` applies a
  signal ONLY when `event.source === "worker"`, counting anything else as
  `tengu_refusal_retraction_unauthenticated_signal`. That is a trust boundary —
  an unauthenticated retraction could erase history.

The remote-session client is an **accepted divergence** for this project (no
Anthropic private relay / auth contract; see the 2.1.216 audit's "HONEST
BOUNDARY" for `remote-control`). So the six `tengu_refusal_retraction_*` events
are NOT prerequisites for the audit item — they belong to a subsystem the
project has deliberately not built. The first revision of this document implied
otherwise.

## STATUS: COMPLETE 2026-07-26

All four steps landed (`adbc367c8` for step 1, this wave for 2-4). The
`tengu_refusal_fallback_notice_collapsed` telemetry now fires for real: a
three-hop cascade emits ONE notice naming where the session ended up, with
`suppressed_count: 2` for the hops folded into it. `orchestrator::refusal_cascade`
is the routing; `orchestrator::refusal_notice` is the episode accumulator plus
the `j0m` collapse queue.

One decision worth carrying: `origin_model` and `refused_user_message_uuid` are
FIRST-writer-wins across a merge, everything else latest-wins. The user is told
where the episode began and where it ended — not which intermediate hop the
merge happened to see last.

The section below is the plan as written before the work.

## The actual decomposition

1. **Refusal-fallback cascade.** Replace the single `refusal_fallback_model` +
   once-per-session latch with an ordered chain: stage resolution
   (`firstReachableStage` / `resolveStage` against `triedModels`), skipped-stage
   logging, `nextHopIndex` / `chainLength`. This is a parity gap in its own
   right, independent of the notice collapse.
2. **Episode + pending-notice state.** `inEpisode`, and a `pendingNotice` that
   MERGES across hops (`Jad`), carrying `provisionalFlushUuid` and the
   accumulated `retractedMessageUuids`.
3. **A typed banner with an identity.** The frame shape at @246323682 — uuid,
   trigger, direction, original/fallback model, request id, refusal
   category/explanation, `retracted_message_uuids`, `refused_user_message_uuid`
   — instead of today's formatted `emit_text` string.
4. **`j0m`.** ~40 lines, read verbatim, and the easy part.

1–3 are one feature and must land together. Step 4 alone, or step 4 with only
step 3, is inert: `provisional` never true ⇒ `suppressedCount` permanently 0 ⇒
the telemetry never fires, while the code's presence reads as coverage.

## The trap worth restating

The port's once-per-session latch means a SECOND refusal surfaces the
byte-locked `API Error: …` terminal message rather than a second banner. That
is faithful to the oracle's latch and is NOT what `suppressedCount` counts.
Wiring the latch to it would produce a plausible-looking `suppressed_count`
that means something else entirely.
