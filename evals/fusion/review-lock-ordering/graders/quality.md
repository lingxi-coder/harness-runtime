---
type: llm
name: quality
weight: 2.0
---
The defect that matters is the lock-ordering inversion between the two functions, which
deadlocks when they run concurrently on the same pair. A strong review names it, explains
the interleaving that produces it, and proposes an ordering discipline such as locking by
a stable key. Secondary observations that are genuinely true: `transfer` can drive a
balance negative with no check, and calling `transfer(a, a, ..)` deadlocks against itself.
Penalise a review that stops at style, or that proposes a fix which still permits two
orders.
