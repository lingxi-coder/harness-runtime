---
type: llm
name: quality
weight: 2.0
---
The requirement the prompt actually states is a bound, and the interesting part is the
policy at the bound. A strong proposal picks one of block, reject, or drop, says which
callers can tolerate it, and changes `push`'s signature so the caller learns what happened
rather than silently losing an event. Look for: whether the bound is items or bytes and
why; how a blocking policy avoids deadlock when the producer and consumer share a thread;
and what the metric is that tells an operator the bound is being hit. Penalise a proposal
that adds a bound while keeping `push` infallible.
