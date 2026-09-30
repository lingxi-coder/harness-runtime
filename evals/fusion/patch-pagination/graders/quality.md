---
type: llm
name: quality
weight: 2.0
---
A strong proposal uses a cursor derived from a stable, unique, ordered key rather than an
offset, and says why: an offset shifts when rows are inserted or deleted mid-walk,
skipping or repeating rows. Look for: the tie-break that makes `created_at` unique, since
timestamps collide; what the cursor encodes and whether it is opaque to the client; the
page-size bound and its default; how the client knows it has reached the end; and the
index the query now needs. Penalise a proposal that uses limit and offset without noting
what it does to a concurrent walk.
