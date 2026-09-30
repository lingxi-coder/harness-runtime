---
type: llm
name: quality
weight: 2.0
---
A strong plan recognises that a trace is only useful when a whole path is instrumented,
and orders the work accordingly. Look for: choosing an edge-in or a core-out order and
saying why; context propagation across services that have not been instrumented yet;
sampling decided at the entry point and what that costs; the storage and retention
consequence stated in advance; and how a team learns whether their service is done.
Penalise a plan that treats instrumentation as thirty independent tasks.
