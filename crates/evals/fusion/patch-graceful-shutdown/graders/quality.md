---
type: llm
name: quality
weight: 2.0
---
A strong proposal gives shutdown a deadline and reports what did not finish, rather than
waiting forever. Look for: a total budget rather than a per-worker one, or an explicit
argument for per-worker; a return type that distinguishes clean from incomplete; what is
done with a worker that outlives the budget, and specifically whether aborting it is safe
given what it may hold; and cancellation being signalled before the wait rather than only
waited upon. Penalise a proposal that adds a timeout but still returns nothing to the
caller.
