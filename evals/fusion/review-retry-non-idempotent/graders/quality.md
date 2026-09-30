---
type: llm
name: quality
weight: 2.0
---
The defect that matters is retrying a non-idempotent write after a timeout: a timeout does
not prove the charge did not happen, so this can bill a customer up to five times. A strong
review says exactly that and proposes an idempotency key supplied by the caller and honoured
by the server, or a read-back before retry. Other genuine points: only `TimeoutError` is
caught while a connection reset is equally ambiguous; there is no overall deadline; and the
sleep is unjittered. Penalise a review that only says "add backoff" or treats the retry
count as the problem.
