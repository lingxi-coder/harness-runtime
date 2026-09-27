# Cross-session agent resume — decomposition, 2026-07-26

Resuming a background agent in a LATER process. Verified against `main` @
`b0ba5b765`, at the behaviour sites.

Today's resume is `send_message` → `MailboxRouter` → pump →
`registry.send_message(task_id)` → `LocalAgentHandler::send_message` →
`StreamingSubagentSpawner::resume(agent_id)`, which routes to a LIVE
in-process runner. Nothing in that chain survives the process.

## Layer 0 — persist the agent's conversation ✅ (this wave)

`AgentTranscriptWriter` existed with **no production caller**. The runner
computed `agent_transcript_path` purely to put it in the `SubagentStop` hook
payload, so the hook named a file nothing created, and a background agent's
conversation lived only in `run_subagent_loop`'s in-memory `history`.

Now wired: `SubagentContext::transcript_fs`, flushed by WATERMARK at each
turn-set boundary. Watermark rather than per-`history.push` because there are
five push sites and a sixth would silently skip persistence. Turn-set
boundaries are the flush points because they are the RESUME boundaries — a
persistent agent parks between turn-sets, so a transcript complete at every
park is complete at every point anything could resume from.

Also replaced the writer's read-then-rewrite hack with a real `append_file`:
it round-tripped the whole file through `read_file`, whose returned view is not
guaranteed byte-identical, which corrupted the JSONL — caught by the first test
that actually read the file back — and rewrote every prior line per message.

## Layer 1 — persist task state ✅ DONE (`d76739609`)

`TaskRegistry` holds `tasks: RwLock<HashMap<String, TaskState>>` and nothing
else. A `LocalAgentTaskState` carries the fields a restore needs (`agent_id`,
`subagent_type`, `prompt`, `is_backgrounded`, `forked_skill_name`, `outcome`)
and already derives `Serialize`/`Deserialize` with `#[serde(default)]` on the
additive fields, so the row shape is ready; the writer and the load-at-boot are
not.

Open design questions that need answering BEFORE writing it:
- Write-through on every mutation, or snapshot at rest/terminal? Rest is the
  only point a resume can target, which argues for snapshot-at-rest and matches
  Layer 0's flush points.
- Where: `<subagents_dir>/agent-<id>.task.json` keeps a row beside its
  transcript and its scoping sidecars, so one directory holds everything about
  one agent and a stale row is trivially detectable (no transcript ⇒ discard).
- Eviction: `evict_terminal_tasks` currently drops rows from memory. Terminal
  rows must not be restored as live.

## Layer 2 — rebuild a runner ✅ DONE (`d76739609`)

`StreamingSubagentSpawner::spawn_persistent(request, inherit)` starts a runner
from a `SubagentSpawnRequest`. A restore needs a variant that also seeds
`history` from a transcript — the runner's `history` is local to
`run_subagent_loop` and seeded from `fork_context_messages` + `prompt_messages`
+ preload. The natural shape is `SubagentContext::resumed_history:
Option<Vec<ConversationMessage>>`, which REPLACES that seeding when present
(re-running preload/hooks on a resume would re-inject context the agent has
already seen).

The `SubagentSpawnRequest` persisted on the task row supplies model / cwd /
isolation / depth, so the rebuilt runner is configured as the original was.

## Layer 3 — the forked-skill gate ✅ (already built)

`session::forked_skill` + `ForkResumeGate` already refuse a resume whose
scoping cannot be corroborated, with six byte-exact refusals. The COLD branch
(`check_scoping_provenance` with `task_forked_skill_name: None`, corroborating
against the provenance marker) exists specifically for this case and is
currently unreachable — Layer 1 is what makes a resume able to arrive without a
live task record.

`union_frozen_command_denies` likewise: verified to have no in-process caller
because `PolicyPermissionGate` holds a boot-snapshot policy that cannot drift
within a process. A cross-session resume reads a freshly-loaded policy that
genuinely can differ, and that is the transform it needs.

## STATUS: COMPLETE

All three layers landed. `session::agent_rows` holds the parked row
(`agent-<id>.task.json`, absence-means-terminal),
`platform_api::parked_agent_store::ParkedAgentStore` is the seam,
`engine_desktop::agent_restore::restore_parked_agents` rebuilds through the same
`spawn_async` a fresh launch uses, and `SubagentContext::resumed_history`
REPLACES the seed (prompt + fork context + preload) rather than prefixing it.

The sections below are the plan as written before the work.

## Ordering

0 → 1 → 2. Layer 0 is independently valuable (the hook payload stopped lying).
Layers 1 and 2 are only valuable together: a persisted row with no way to
rebuild a runner is a row nothing reads, and a rebuild path with no rows is
unreachable code. They should land in one wave, not two.
