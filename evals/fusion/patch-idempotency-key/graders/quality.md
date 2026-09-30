---
type: llm
name: quality
weight: 2.0
---
A strong proposal has the client supply a key and the server store it atomically with the
order, so a replay returns the original result instead of creating a second one. Look for:
where the key comes from and its uniqueness scope; a unique constraint or equivalent doing
the enforcement rather than a check-then-insert; what a replay returns; what happens when
the same key arrives with a different body; and how long keys are retained. Penalise a
proposal that checks for an existing key and then inserts, since that races.
