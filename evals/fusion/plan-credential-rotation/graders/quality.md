---
type: llm
name: quality
weight: 2.0
---
The hard part is the overlap window, and a strong plan is built around it. Look for: a
period in which both the old and new credential are valid, and how that is arranged on
the database side; how services that read at start are made to pick up the new value
without a coordinated restart; how the plan discovers a service nobody remembered; the
signal that says the old credential is unused and can be revoked; and what revocation
does if that signal was wrong. Penalise a plan that revokes before proving disuse.
