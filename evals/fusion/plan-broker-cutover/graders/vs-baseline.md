---
type: baseline
name: vs-baseline
weight: 1.0
---
Score the candidate higher only when its plan is more executable, not when it is longer.
Reward: a step the baseline left implicit made concrete, a rollback point the baseline
lacked, an ordering constraint the baseline got wrong, or a failure mode the baseline did
not consider. Penalise: extra phases that do not change what anyone does, restating the
goal as a step, and removing a commitment the baseline had made. Equal quality scores 0.5.
