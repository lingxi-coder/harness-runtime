---
type: llm
name: quality
weight: 2.0
---
A strong plan is ordered, and each step names its own verification and its own rollback.
Look for: dual-write or dual-read and which one is chosen and why; how the drain of A is
detected rather than assumed; what happens to in-flight messages at the switch; whether
consumer offsets or acknowledgements carry across; and an explicit statement of the last
point at which rollback is still cheap. Penalise a plan whose steps cannot be checked, or
that treats "verify everything works" as a step.
