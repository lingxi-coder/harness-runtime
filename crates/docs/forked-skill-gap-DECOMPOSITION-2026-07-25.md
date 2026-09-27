# forked-skill gap — the real dependency chain, 2026-07-25

`delta-audit-217-218` files three items — `forkedSkillName` (L),
`forkedSkill` (L, security), `frozenCommandDenies` (XL, security) — which my
re-triage grouped as "one architectural cluster". They are a stack, and the
security guard is at the TOP, so it cannot be landed first.

Verified against `main` @ `d4bf6e352`, at the behaviour sites.

## Correction to this document's first revision

The first revision claimed the base of the stack was missing entirely: *"no
tool can dispatch a background agent."* **That was wrong.** I had grepped for
`spawn_local_agent` / `SpawnInput::LocalAgent` / `TaskKind::LocalAgent` under
`tools/` and read the empty result as absence — but the tool layer does not
name the tasks layer directly. It goes through a seam:

```
AgentTool::dispatch_async  →  SubagentSpawner::spawn_async
                              └─ BackgroundAgentSpawner (apps/engine-desktop/src/background_agent.rs)
                                   ├─ registry.spawn(LocalAgent{is_backgrounded:true})
                                   ├─ MailboxRouter.register(agent_id)  ← SendMessage routing
                                   └─ run_teammate_pump  ← the injectUserMessageToTeammate bridge
```

wired at the composition root (`apps/engine-desktop/src/lib.rs:6666`).
`run_in_background` has been live end-to-end.

Two things fed the wrong read, and both are worth naming because they will
mislead the next reader too:

1. **A negative grep for a name the architecture deliberately avoids.** The
   point of the `SubagentSpawner` seam is that `tools/` does not mention
   `tasks/`. Searching for the callee's vocabulary inside the caller's crate
   was guaranteed to return nothing whether or not the wiring existed.
2. **A stale comment that read as a live status.** `tasks/src/registry.rs:930`
   said, in prose, *"they flow only once the BACKGROUNDED local_agent path is
   wired (`AgentTool::call` dispatches synchronously today…)"*. That sentence
   was true when written and false when read, and it named the exact symbol I
   had just failed to find — so it confirmed the wrong conclusion instead of
   correcting it. **A deferral note that asserts a fact about other code,
   rather than pointing at it, becomes a lie the moment that code moves.**

## What was actually missing (fixed in this wave)

Only the last layer of the base: a terminating `local_agent` reported nothing
but a status. `take_pending_task_notifications` hardcoded `result: None,
usage: None, killed_by: None, worktree_path: None, worktree_branch: None`, and
`LocalAgentTaskState::error` was never written in production either — so every
completed background agent reached the model as a bare "finished" with no
answer, every failed one as claude's `Unknown error` fallback, and every stop
as the generic "was stopped".

## What forked-skill still needs, now that the base is whole

1. `SkillFrontmatter` (`skill-api/src/model.rs`) has `name`, `description`,
   `when_to_use`, `allowed_tools`, `disallowed_tools`, `model` — and **no
   `context` field**, so a `context: fork` declaration is parsed away and never
   seen. `background` is absent too. Add both + a `should_background_fork`
   predicate.
2. `tools/skill/src/skill.rs` hard-codes `"status": "inline"`. Route a forking
   skill through the same `SubagentSpawner::spawn_async` the Agent tool uses,
   and render claude's `status:"forked"` / `background:true` result shape.
3. `forkedSkillName` on the registry entry.
4. The two scoping sidecars + the four resume refusal reasons
   (`..._scoping_invalid` / `_missing` / `_missing_cold` / skill-name mismatch).
5. `frozenCommandDenies`: capture at fork, union ahead of live denies on resume.

Steps 4–5 are the security items and must land WITH 2–3, not after: a fork path
without the resume gate is precisely what the guard exists to stop.
