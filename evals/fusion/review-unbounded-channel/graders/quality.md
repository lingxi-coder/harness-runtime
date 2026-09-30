---
type: llm
name: quality
weight: 2.0
---
The defect that matters is that an unbounded channel converts a slow consumer into
unbounded memory growth, and `store.write` is the slow side. A strong review says the
producer has no backpressure path and proposes a bounded channel with a stated policy for
what happens when it is full. Other genuine points: `let _ = tx.send(..)` discards the
error that would signal a dead consumer, so the producer keeps reading the source forever;
neither task is joined or cancelled, so shutdown drops in-flight events. Penalise a review
that recommends a bounded channel without saying what happens at the bound.
