---
name: local-app-background
description: Schedule, list, check, cancel, and retry a local app's bounded background flows, and the capability, durable-approval, and step-allowlist limits that keep them safe unattended.
---

# Manage a local app's background jobs

Own an app's background-flow lifecycle: register a bounded declarative flow
with the system scheduler, list its tasks, read one task's status, cancel
it, or retry it — and know the platform limits that make an unattended flow
safe to run. Never invent a capability or schedule the app hasn't already
had granted.

## The tools

- `LocalAppBackgroundSchedule` — `app_id`, `interval_ms` (900,000 to
  2,592,000,000 — 15 minutes to 30 days), `flow` (a `FlowDefinition`).
  Returns `{task, scheduled:true, scheduler:"host-journal"}`.
- `LocalAppBackgroundList` — `app_id`, optional `task_id`/`status`/`limit`
  (1-100, default 50). Read-only, allow-by-default.
- `LocalAppBackgroundStatus` — `app_id`, `task_id`. Read-only,
  allow-by-default; fails with "background task was not found" for an
  unknown id rather than an empty list.
- `LocalAppBackgroundCancel` — `app_id`, `task_id`. Returns
  `{cancelled: bool}` — `false` means the task was already in a terminal or
  non-cancellable state, not an error.
- `LocalAppBackgroundRetry` — `app_id`, `task_id`. Requeues one failed or
  cancelled task immediately; returns `{retried: bool}` with the same
  "false is not an error" meaning.

A task's lifecycle is one of `scheduled`, `running`, `waiting_for_system`,
`succeeded`, `failed`, `cancelled`.

## Before a schedule call succeeds

Three gates, all separate, all enforced by the host — not by this skill:

1. The manifest must already declare `background_schedule` in its
   capabilities, or the call fails immediately with "background scheduling
   is not declared in the app manifest," before any prompt. Declaring it is
   a manifest-update operation, not this skill's job.
2. The first schedule call against an app raises the host's own
   `BackgroundSchedule` capability prompt — "应用请求在系统后台按计划运行一个
   流程" — Deny / Allow once / Allow for session / Always allow.
3. **Background scheduling requires the durable grant specifically.** After
   that prompt, the host reloads the *persisted* permission record and
   refuses with "background scheduling requires durable approval" unless
   the user chose Always Allow — Allow Once or Allow For Session are
   accepted by the prompt but are **not enough** to actually schedule.
   Report a refusal here as "the user needs to grant this permanently," not
   as a bug.

## What a flow step can actually be

A `FlowDefinition` is `{flowId, version, steps[]}`, 1-128 steps, each
`{stepId, capability, dependsOn[], inputJson}`. Step ids are unique;
`dependsOn` may only name an *earlier* step id (the flow must be acyclic);
`inputJson` must be valid JSON within the bounded payload size.

Two separate filters then apply to every step's `capability`:

- **No interactive capability, ever.** `calendar`, `contacts`, `share`,
  `deep_link`, `media`, `camera`, `photo_library`, `microphone`,
  `speech_to_text`, and `location` are all foreground-bound and rejected
  outright for a system-scheduler origin — they need a human in front of
  the screen, which a background wake never has.
- **Even a non-interactive capability must be one the scheduler actually
  authorizes.** Only `data.query`, `data.mutate`, `runtime.status`,
  `device.notifications`, `llm.complete`, the Agent-session family
  (`agent.sessions.create/list/resume/close`, `agent.send`), `agent.emit`,
  and `network.request` have an authorization path here. Everything else —
  `files.read`/`files.write`, `clipboard.access`, `device.haptics`,
  `llm.stream`, `agent.stream`, `agent.cancel`,
  `agent.profiles.propose-update`, `flow.execute`, and
  `background.schedule` itself — fails with "background capability … is not
  supported," even though several of those aren't flagged interactive. A
  background flow can never schedule another background flow.
- Each authorized step also needs its OWN capability declared in the
  manifest and its own durable grant — the same "durable only" rule as
  `background_schedule` itself applies per step (a `network.request` step
  additionally needs its target domain durably approved). One
  under-approved step fails the whole schedule call, not just that step.

## Boundaries

- Never declare or assume a capability a step needs — an undeclared or
  under-granted one fails the schedule call; hand that back rather than
  retrying with a workaround or a smaller ask.
- Never treat `cancelled:false` or `retried:false` as failure — report the
  task's actual state instead.
- Writing the flow's own step logic beyond this catalog, or the app source
  that calls `window.lingxi.v2.background.*`, is not this skill's job —
  see `$device` for the bridge side and `$create-local-app` for app source.
