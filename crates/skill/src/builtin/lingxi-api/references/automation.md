# Schedules, workflows and background work

Select by the requested lifecycle:

| Need | Existing service/entry when present |
| --- | --- |
| Repeat work on a schedule | `CronCreate`, `CronList`, `CronDelete`; active scheduler required |
| Continue this main interaction later | `ScheduleWakeup`; requires its owning host queue |
| Reusable/resumable procedure | Workflow tools and `workflow-authoring` skill |
| Independent runtime task | Background Agent/task service and its actual task IDs |
| Persistent personal assistant's routine | Bot's own routine UI/controller; read `bots` |

Inspect live schemas for schedule format, timezone, duration and recurrence.
Use the user's timezone and clarify only ambiguities that change the schedule.
A registered scheduling tool can still lack an active executor; in particular
a mobile host does not gain perpetual background scheduling from its tool name.
`ScheduleWakeup` without a host queue does not establish a future continuation.

Persisted schedule, next trigger, started run, waiting approval and completed
run are different states. Verify the state the service actually reports. A
process or app exiting can affect future execution; do not promise unattended
continuation across shutdown unless the specific host guarantees it.

A Workflow definition describes a procedure; saving it is not a successful
execution. Use current Workflow interfaces for launch, status and recovery.
Avoid recreating task state in arbitrary files. For background Agent work,
retain task/mailbox identifiers and inspect completion through that service;
the list of active subagents is not a replacement for durable task state.

Source anchors: Harness `crates/tools/cron`, `crates/workflow`,
`crates/runtime/src/desktop/background_agent.rs`; Bot-capable product scheduler
and `apps/electron/src/main/bots/service.ts`.
