---
type: llm
name: quality
weight: 2.0
---
The defect that matters is that this is not crash-safe: `open(path, "w")` truncates first,
so a crash between truncate and complete write leaves an empty or partial file where a
valid one was. A strong review proposes writing to a temporary file in the same directory,
flushing and fsyncing it, then renaming over the target, and notes that the directory
itself must be fsynced for the rename to survive. Other genuine points: no error handling,
and concurrent writers race. Penalise a review that suggests a lock as the fix, since a
lock does not survive a crash.
