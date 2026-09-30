---
type: llm
name: quality
weight: 2.0
---
A strong answer is a sequence with a rollback point after each step, not a single
operation. Look for: whether the engine can do this in place and how the answer decides
that; the shadow-column and backfill pattern including how the backfill avoids a long
transaction; keeping the two columns consistent during the window, and whether that is
done by a trigger, dual writes, or the application; how reads switch over; and when the
old column can actually be dropped. Penalise an answer that omits the read-path cutover
or that assumes the backfill can run as one statement.
