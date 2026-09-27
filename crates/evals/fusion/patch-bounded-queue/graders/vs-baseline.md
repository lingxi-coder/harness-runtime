---
type: baseline
name: vs-baseline
weight: 1.0
---
Score the candidate higher only when the proposed change is more likely to work as
described. Reward: a call site or failure mode the baseline overlooked, a migration step
the baseline skipped, or a correction to a mechanism the baseline proposed that would not
have held. Penalise: a larger design that does not address the stated requirement, and
any proposal whose described behaviour contradicts the code it is changing. Equal quality
scores 0.5.
