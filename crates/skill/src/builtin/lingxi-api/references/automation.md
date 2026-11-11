# Schedules, workflows and background work

Select by the requested lifecycle:

| Need | Existing service/entry when present |
| --- | --- |
| Repeat work on a schedule | `CronCreate`, `CronList`, `CronDelete`; active scheduler required |
| Continue this main interaction later | `ScheduleWakeup`; requires its owning host queue |
| Reusable/resumable procedure | Workflow tools and `workflow-authoring` skill |
| Independent runtime task | Background Agent/task service and its actual task IDs |
| Persistent personal assistant's routine | Bot's own routine UI/controller; read `bots` |

Inspect live schemas for timing/recurrence and use the user's timezone. A
registered tool can lack an executor. `ScheduleWakeup` needs its host queue;
Mobile has finite lifecycle/background constraints. Persisted, next-trigger,
running, waiting-approval and completed are distinct states. A saved Workflow
is not a successful run. Recover work through its original task/mailbox ID,
not a new submission or the list of currently live agents. Do not promise
continuation across host shutdown without an explicit host guarantee.

Source anchors: Harness `crates/tools/cron`, `crates/workflow`,
`crates/runtime/src/desktop/background_agent.rs`; Bot-capable product scheduler
and `apps/electron/src/main/bots/service.ts`.

Native task-center schedules and session Cron are independent contracts. The
Desktop task center uses `<controller-cwd>/{{config_dir}}/scheduled_tasks.json`
(schema 2 automation). Desktop tracks controller scopes in
`userData/scheduled-scopes.json`; this is not the job definition file.
Mobile resolves its own native/guest scope through the host's scheduler/store;
do not apply a Desktop filesystem path to it. CronCreate durable jobs use
`<session-cwd>/{{config_dir}}/session-cron/scheduled_tasks.json`, with
separate leader/mutation locks. Session-only jobs remain in memory. Opening an
empty project does not create scheduler files. loop.md lives under {{config_dir}} or
in the current directory. Persistence alone does not guarantee an active host.
Old paths and task formats are not read, migrated or imported. Only the current
store contracts are accepted. Do not run old tasks as part of an upgrade.

For a task without a selected Project or session, the host owns its workspace
under `~/{{config_dir}}-workspaces`. Generated runs use
`cron/<sha256(taskId)>/runs/<date>/<title>-<runId>/work/` and `outputs/`;
task memory uses `cron/<sha256(taskId)>/memory.md`. Existing selected-session
tasks retain their authorized cwd. Read the host's resolved paths and task
record instead of using the private config home for working files. See product
`docs/WORKSPACE_STORAGE.md` and `apps/electron/src/main/scheduled.ts`.
