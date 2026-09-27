---
name: llm-agent
description: Guide a Local App's own JS source through the host-owned Persistent Agent session, turn, and profile-proposal bridge — budgets, gates, and the approval flow.
---

# Run a Local App's Persistent Agent session

Own guiding correct use of the App Persistent Agent bridge from *inside an
app's own page code* — session lifecycle, turns, and profile proposals. The
host owns every session and every approved instruction set; the page can
request operations on them but is never the authority for what they
contain. This skill does not change what a Persistent Agent IS or does —
turning it into anything resembling a Plugin Agent is out of scope here.

## Sessions

- `createAgentSession(request?)` starts a new host-persisted session for
  this app, at the app's currently-approved profile. An optional `budget`
  may ask for something SMALLER than the host's own ceilings on tokens,
  wall-clock time, turn count, bridge calls, and MCP calls — never larger;
  a larger request is silently clamped back down, not honoured.
- `listAgentSessions()` lists this app's sessions and their state.
- `resumeAgentSession(request)` / `closeAgentSession(request)` change one
  session's status. A **closed session can never be resumed** — guide app
  code to create a new session instead of retrying a resume against one it
  already closed.

## Turns

- `sendAgentTurn(request)` runs one non-streaming turn in an existing,
  active session; `streamAgentTurn(request)` runs one streaming turn —
  subscribe with `onAgentStreamFrame` before calling it, the same ordering
  requirement `$llm-sidequery` describes for its own stream. Both need a
  `sessionId` and a non-empty `prompt`.
- `cancelAgentTurn(request)` cancels the turn currently running for a
  session (or a specific `turnId`, if given).

A session's budget is **cumulative across its whole lifetime, not
per-turn** — once its turn count, output tokens, bridge calls, or MCP calls
reach what `createAgentSession` settled on, every further turn is refused
until the app creates a new session; there is no per-turn reset. Only one
turn may run per session at a time — a second concurrent send or stream
against the same session is refused rather than queued. A turn can also end
early, mid-stream, if it would exceed the session's remaining token budget,
or if it runs past its wall-clock budget — guide app code to expect a
turn's stream ending with `cancelled: true` as a normal outcome, not
something to retry blindly. A turn that completes normally always advances
the session's turn count by one, whether it ran through `sendAgentTurn` or
`streamAgentTurn`; a cancelled turn does not, and instead moves a still-open
session to a paused state.

## Profile proposals

`proposeAgentProfileUpdate(request)` sends `instructions` and a `reason`
for changing the app's approved instruction set. This call **never applies
the change** — it only records a pending proposal and surfaces it to the
user; the resolved promise tells the page the proposal was received and
that approval is still required, not that the instructions are live. Guide
app code to treat the response's own "approval required" field as a
statement of fact, never something it can act around. If the user
approves, every still-open session for the app picks up the new profile on
its NEXT turn — the app does not need to recreate sessions to see it.

## Boundaries

- Never let app code apply, assume, or fabricate approval for its own
  profile proposal — that decision is the user's, made outside this bridge
  call, and this skill has no way to see the outcome from the propose
  response alone.
- Never treat this as a channel for unattended or background turns — a
  background flow's own Agent-session capability family (see
  `$local-app-background`) is the host-authorized path for that; this
  bridge is for a page-driven, foreground session.
- Never assume a larger requested `budget` raises the host's real ceiling —
  it is clamped down, never enlarged.
- Never retry a `busy` or `not active` refusal by immediately repeating the
  same call — read the session's actual state with `listAgentSessions`
  first.
- Never describe a Persistent Agent session as, or turn it into, a Plugin
  Agent — nothing here changes the session record, the approved profile
  record, or the app-scoped transport those live behind; this skill only
  guides using the bridge that already exists over them.
