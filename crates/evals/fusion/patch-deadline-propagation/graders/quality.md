---
type: llm
name: quality
weight: 2.0
---
A strong proposal threads a deadline, not a duration, through every layer and has each
call derive its remaining budget from it. Look for: why an absolute deadline beats
re-passing 30 seconds at each hop; the database call receiving a statement timeout derived
from the remaining time; cancellation actually reaching in-flight work rather than only
abandoning the wait; what the caller sees when the deadline expires mid-call; and where
the deadline comes from when the caller supplied none. Penalise a proposal that adds a
separate fixed timeout at each layer.
