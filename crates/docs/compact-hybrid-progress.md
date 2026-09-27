# Phase-bounded compact progress

The CLI/TUI, Desktop, Android, and iOS show **estimated progress** bounded by
engine-observed compaction phases. This replaces the Claude Code elapsed-only
formula. It does not measure bytes or tokens processed and does not predict a
completion deadline. Provider selection does not change the calculation.

| Phase | Entry | Maximum while running | Time constant |
| --- | ---: | ---: | ---: |
| Preparing | 0% | 9% | 5 seconds |
| Summarizing | 10% | 84% | 90 seconds |
| Restoring | 85% | 99% | 10 seconds |
| Successful completion | 100% | 100% | — |

For a known running phase, `elapsed` is seconds since that phase began:

```text
min(cap, base + round(span * (1 - exp(-max(0, elapsed) / tau))))
```

The `(base, span, tau, cap)` tuples are `(0, 10, 5, 9)`, `(10, 75, 90, 84)`,
and `(85, 14, 10, 99)`. Reserving the next phase's entry value prevents a stalled
request from claiming the next stage has begun. These are fixed initial tuning
parameters, not a provider-specific latency model.

Only forward phase transitions reset the phase clock. Repeated and regressive
phase events do not restart or reduce progress. Component remounts reuse stored
timestamps. Total elapsed time is display-only. Waiting and unknown phases have
no percentage; error, cancellation, and skipped attempts never display 100%.

The existing `compaction_status` wire event is reused without numeric progress
fields. Its additive `skipped` phase marks an attempt which made no compaction
transition. `OutputStream::emit_compaction_skipped` defaults to the existing
successful finish callback, retaining legacy SDK output; UI adapters override
it to display the skipped outcome. The reactive path which previously had no
legacy terminal event continues to use the phase-only callback.

Successful completion means the existing engine flow returned successfully,
including its existing best-effort persistence semantics. This change does not
modify model selection, provider routing, prompts, or token thresholds.

## Verification contract

The shared `client-protocol/snapshots/compaction_hybrid_progress.json` contains
505 cases: phase entries, negative times, long stalls, rounding boundaries, and
terminal/unknown phases. All four implementations must consume these cases.
Lifecycle tests cover forward/repeated/regressive phases, remounts, failure,
cancellation, skipped results, and completion metric enrichment. Backend tests
cover manual, proactive, reactive, hook, idle WebSocket, and SDK compatibility
paths. Local verification logs are under `.omx/logs/desktop-compact-progress/`.

Verified in this change: Desktop reducer/render 87 tests, real Electron
interaction and Node/web typechecks; Android 76 unit tests; iOS simulator 10
tests; TUI 21 compact tests and TUI-core 3 bridge/boundary tests;
orchestrator 20 lifecycle tests; adapter 2 compact
tests; side-query 8 tests, including four parent-provider route configurations.
The idle WebSocket lifecycle test and 2 CLI compaction tests (including skipped
legacy SDK event compatibility) passed. All-feature Clippy for the affected
production libraries completed successfully with warnings; diff whitespace
checks passed.
The provider tests use request-capturing substitutes rather than live API calls.
Desktop visual verification used a real Electron fixture, not a signed package;
mobile validation did not exercise physical-device release builds.
