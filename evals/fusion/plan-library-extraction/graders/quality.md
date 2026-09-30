---
type: llm
name: quality
weight: 2.0
---
Look for the two halves being kept separate: moving the code with history, and switching
consumers over. A strong plan covers: how history is preserved and what is lost either
way; the interval during which both copies exist and which one is authoritative; how
consumers depend on the new location before the old copy is deleted; versioning and
release of the extracted library; and how CI is kept green rather than merely fixed
afterwards. Penalise a plan that deletes the in-repo copy before consumers have moved.
