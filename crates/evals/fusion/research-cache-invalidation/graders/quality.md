---
type: llm
name: quality
weight: 2.0
---
A strong answer names several distinct mechanisms rather than one, and is explicit about
what each one costs and what it cannot do. Look for: whether staleness is bounded by
construction or only in expectation; what happens during a cache or bus outage; whether
the mechanism needs cooperation from the writers the prompt says cannot be changed;
and the interaction between the chosen mechanism and a cold cache or a cache restart.
A specific recommendation must follow from the stated constraints, not precede them.
Penalise any answer that assumes the writers can be modified, since the prompt forbids it.
