---
type: llm
name: quality
weight: 2.0
---
The defect that matters is that `start` drops the `JoinHandle`, so `stop` has no way to
end the task: setting a flag nobody reads leaves the loop running for the life of the
process, holding its clone of `state` alive. A strong review says the handle must be
retained and the task cancelled, by abort or by a cancellation token the loop selects on,
and notes that `running` is never read. A further genuine point: calling `start` twice
spawns a second task. Penalise a review that proposes checking `running` inside the loop
without addressing that the flag lives on `self` and the task holds no reference to it.
