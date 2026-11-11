# Dataflow graph

Status: design. No implementation in this document.

详细设计：[2026-11-10-dataflow-graph-detailed-design.md](2026-11-10-dataflow-graph-detailed-design.md)。本文只保留已锁定的决定。

A Rust dataflow runtime for a cluster of agents, their tool execution, external
streams, and render. One session is one graph. Members are agent nodes.
Hooks, permission, MCP, and sandbox stay on the tool edge of each member.
An external frame joins only the decision stream wired to render. While that
agent is still working, the runtime sequencer advances the decision bound and
the join reuses the previous decision.

## Problem

Agent execution today is the orchestrator turn loop. `run_turn` holds the turn
lock, calls the model, runs tool calls, and writes results back into the
transcript. Control flow is a procedure. There are no ports, no timestamps, and
no place for an external stream to meet a decision without being folded into
the next prompt.

`crates/workflow` runs scripted multi-agent progress. `crates/fusion` runs a
panel of attempts. `media-runtime` owns attachment and audio routing. None of
them is a packet graph. This runtime is a new crate beside them. It does not
replace the turn loop in the first stage.

## What the runtime keeps from MediaPipe

MediaPipe's calculator graph is the reference for the data plane. The pieces
that carry over:

- An edge is a named port. One output manager mirrors a packet to each
  downstream input. Packets are immutable. The last downstream takes the queue;
  earlier ones receive a copy.
- A timestamp `t` is ready when at least one input has a packet at `t`, and
  every other input either has a packet at `t` or has a timestamp bound past `t`.
- A node has `Open`, `Process`, and `Close`. `max_in_flight` defaults to 1.
- Each input queue has a bound. A full queue throttles its upstream.
- Closing an input pushes its bound to done.

MediaPipe freezes topology at `Initialize`. Mirrors are wired once. An agent
that rewires edges inside `Process` races in-flight calls, permissions, and
side effects. Rebuilding the whole graph to apply one edit drops in-flight
packets and external subscriptions. This runtime does not do that.

A model call that sits on the same timestamp frontier as frames, and does not
publish a bound, stalls render. The first graph forbids that shape.

## Runtime

Two planes.

Each data-plane revision is immutable. The current revision admits new input;
retained revisions complete admitted work. Scheduling, mirrors, and backpressure
follow the rules above. A node never receives `&mut Graph`.

The control plane is a reserved patch event. A `GraphPatch` carries `patch_id`
and `base_revision`. The scheduler validates and prepares it, then atomically
publishes the next revision at an admission boundary without waiting for every
model call to finish. Spawn requires `AgentSpawn`, validation, and successful
`Open` before publication. Stale bases return a conflict; a duplicate patch id
returns its original outcome without rerunning hooks. Failed preparation
leaves the current graph intact and closes unpublished nodes.

### Revision cutover and retirement

Bind-only updates use the same serialized commit sequence and create a new
revision without a topology patch. Unchanged node instances are not reopened.
Model concurrency limits and session state are shared per logical `AgentId`
across revisions.

Invocations carry invocation, parent, agent, and revision ids; tool calls also
carry a tool-use id. Already-admitted inputs, continuations, tool results, and
Ask replies keep their original revision and routing. New ordinary external
input and root calls enter the current revision. Retired revisions may finish
their admitted work, hooks, and result collection, but cannot start new
autonomous model rounds. A retained agent's stable inbox passes completed
results to its next independent model call on the current revision; a removed
agent only records completion.

Each render clock cuts over at `c`, the successor of its last admitted frame.
The new revision admits only `t >= c`. Join and egress both check render epoch:
after publication, queued or late old-epoch render output is dropped, and old
decisions never update the new join. Tool results and mailboxes are not dropped
by this rule. The cached decision migrates only when source node, port identity,
and join policy are unchanged; switching source or policy clears it.

A removed node closes once its in-flight work, admitted queues, parent result
collection, Ask continuations, and resource references have drained. Then
`SubagentStop` fires. An outstanding Ask retains the old revision. Forced
cancellation and drain timeouts remain future work.

### Ask and recovery

In-process resumption is required in stage one. It keeps the original future
and continuation. Replies are addressed by session, agent, revision, invocation,
Ask id, and generation, plus tool-use id where applicable. They are consumed
once; stale, duplicate, or mismatched replies do not execute anything. Model
switches use an operation id rather than a fabricated tool-use id.

Cross-process recovery is optional and must remain disabled unless the host
can restore all current and retained revisions and their ingress ownership,
versioned Bind descriptions, transcript position, invocation input
and execution phase, parent/branch/loop continuation state, completed results,
and pending Ask records. Live sockets and sandbox processes are not snapshots.
The detailed design's recovery contract also requires a durable call journal:
persist `dispatched` before sending, persist terminal results before idempotent
delivery, and commit Ask consumption with the phase transition. A checkpoint
includes a journal cursor; later records must be folded before resuming.

Only an undispatched paused step may resume after host capability and permission
validation. A dispatched call without a durable result becomes `outcome_unknown`
and is never automatically resent, including a crash between recording dispatch
and sending. This ends the step with a structured uncertain result; dependent
effects cannot proceed as though it succeeded. Completed children are not
rerun. MCP elicitation can resume only
while its original request and connection survive; loss of that connection
without a server recovery protocol closes the Ask and yields `outcome_unknown`.
No exactly-once external-effect guarantee is claimed.

### Cluster

A session is one cluster graph. A member is an agent node with an `AgentId`.
This matches the coordinator that already exists: `spawn_worker`, a mailbox
router keyed by id and by name, a bounded inbox, and team membership.

Mailbox traffic is an addressed Event, not a mirror. The sender names an id or
a teammate name. Delivery failure — closed, full, or unknown id — comes back
to the sender as an event. Broadcast is an explicit send, not fan-out along
every edge.

```text
cluster
  ingress frames -----------------------------+
  agent A -- decision (Stream) --------------+-- join -- render
  agent A <-- mailbox Event --> agent B      |
  agent B -- decision (Stream)               |
       only if that stream is wired to join -+
  each agent
    prompt / tool Event
      -> hooks -> permission gate -> executor -> hooks -> agent
    executor runs inside that agent's sandbox scope
    MCP tools are executor targets, not a second path
```

Only a stream that is explicitly wired into the render join affects frames.
Mailbox messages and other agents' decisions stay on Event ports. A quiet
teammate does not stall render. The first library slice may contain one
member, but every side-effect packet already carries `agent id`, revision, and
tool-use id, so a second member is another node in the same graph.

Spawn is a patch. Before the revision commits, `AgentSpawn` may deny it or
rewrite agent type, model, cwd, or the background flag. `SubagentStart` fires
after successful `Open` and revision publication. `SubagentStop` fires after
retirement on `Close`. `TeammateIdle`
fires when that agent's queues are idle.

Inheritance is the subset declared on the spawn: prompt, model, cwd, and plan
mode. The child does not share the parent's in-flight tool queue, permission
ask, or live sandbox process. A child tool use is a new id.

### Ports

| Kind | Alignment | Use |
| --- | --- | --- |
| Stream | Timestamp join | External frames, a decision wired to render, render input |
| Event | Queue only | Tool request and result, mailbox, patch, hook and permission asks |
| Bind | Constant for one revision | Model, permission policy, sandbox policy, tool pool, skill listing |

### One member

The earlier four-node drawing is the inside of one member, not the whole system.

```text
external --frames (Stream)--+
                            +-- join -- frame+decision -- render
agent ----- decision (Stream)
  | tool request (Event)
  v
hooks -> permission gate -> executor -> hooks
  |                              |
  |                              +-- sandbox scope for this agent
  |                              +-- local tool or MCP call
  v
tool result (Event) --> agent
agent -- GraphPatch (Event) --> controller
```

- **Ingress.** Callers `push(port, packet)`. Each port has its own overflow
  policy. The render port drops frames when full. The tool-result port waits.
  A slow render does not block tools, and a slow tool does not change the
  render drop policy. A full mailbox is an error to the sender, not a dropped
  frame.
- **Agent.** `max_in_flight` is 1 for model `Process`, not for the runtime's
  progress sequencer. Inputs are tool results, mailbox events, and frames it
  chooses to read. Completed decisions are candidates; the sequencer stamps
  them for the next admitted frame. Tool requests, mailboxes, and patches are
  Events. Progress continues during model, tool, and Ask waits.
- **Executor.** The sole graph node for tool effects. It records dispatched
  before sending, durably when cross-process recovery is enabled. An unknown
  result is not retried. Calls retain their admitted revision's permission,
  sandbox, and tool bindings through host-provided revision adapters, not
  mutable global objects. A later revision does not rerun the same tool use.
- **Render.** An egress subscriber. The graph does not know about pixels. It
  delivers the joined packet to the caller.

The controller is the scheduler's patch application, not a data node.

### Hooks

Hooks stay the existing taxonomy in `HookEventType`. They are interceptors on
edges and on lifecycle points. They are not nodes an agent adds while running,
and they do not receive `&mut Graph`.

Sync hooks that can change the packet run before the downstream node is
scheduled, on the same revision:

| Point | Hook | Effect on the packet |
| --- | --- | --- |
| Prompt enters an agent | `UserPromptSubmit`, `UserPromptExpansion` | Rewrite or block. Expansion covers slash commands and MCP prompts |
| Tool request leaves an agent | `PreToolUse` | Block, mutate the input, or annotate |
| Tool result returns | `PostToolUse`, `PostToolUseFailure`, `PostToolBatch` | Augment or post-process before the agent sees it |
| Spawn patch | `AgentSpawn`, then `SubagentStart` / `SubagentStop` | Deny or rewrite before commit; start and stop observe the node |
| Permission gate | `PermissionRequest`, `PermissionDenied` | Observe or participate in the ask |
| Model change | `PreModelSwitch`, `PostModelSwitch` | Allow, ask, or block the switch |
| Member goes idle | `TeammateIdle` | Observe |

A hook that wants another agent emits a spawn request. That request still
passes `AgentSpawn`. A hook may call MCP through `HookMcpInvoker`. That call is
an executor-scoped effect with its own tool-use id. It does not skip the
permission gate.

Async hooks run on the Event plane. They do not hold the frame timestamp
frontier. A sync hook that has not returned leaves its edge waiting. The join
does not invent a decision to hide that wait.

Session and environment hooks (`SessionStart`, `SessionEnd`, `Setup`, `Stop`,
`PreCompact`, `PostCompact`, `ConfigChange`, `CwdChanged`, `FileChanged`,
`WorktreeCreate`, `WorktreeRemove`, `InstructionsLoaded`, `Notification`,
`TaskCreated`, `TaskCompleted`, and the rest of the taxonomy) stay observers
or gates at those existing points. The graph emits them. It does not replace
their payload contract.

### Permission

Permission is a gate on the tool edge. Bind stores the revision's policy
description. A host revision-bound `PermissionGate` adapter preserves the full
resolution path, including `PermissionCheckContext`, layers, classifier,
decision provenance, and prompt transport. Raw `PermissionPolicy::authorize`
is only the rule engine; it cannot replace `resolve_detailed_or_abort` and
the surrounding gate behavior. A shared mutable live gate is not a revision
snapshot and requires adaptation before this graph can use it.

- Allow: continue with the gate's effective input and authorization metadata.
- Deny: return the rejected tool result. Only a classifier-source denial fires
  `PermissionDenied`; rule, mode, and user denials do not. Denial notifications
  remain separate. Feature-gated hook `retry` remains a later meta reminder,
  never an executor retry.
- Ask: run `PermissionRequest` before entering the prompt transport. Apply a
  valid hook decision first; only a still-unresolved ask opens the external
  decision port for this invocation.

A decision may update session allowances in the host. That update is visible
in the next revision's Bind. It does not rewrite the revision that already
dispatched a call. `dangerously_disable_sandbox` is a permission input to the
sandbox choice below, not a graph edit.

### MCP

MCP connections stay host services: socket, auth, dropped tools, and server
role. The graph does not own the connection. A connected server publishes tool
descriptors into the agent's Bind. Calling one is the same edge as a local
tool: hooks, permission gate, then executor. The executor performs the MCP call.

`Elicitation` and `ElicitationResult` are a second ask port on that call. The
ask occurs after dispatch and pauses that original request and connection.
It does not pause other agents or render. Recovery follows the Ask contract
above; replaying the MCP tool call is not a recovery mechanism.

A disconnect removes that server's descriptors on the next revision. An
in-flight call finishes or fails on the revision that dispatched it. It is not
replayed onto another server.

### Sandbox

Sandbox is the executor environment for a command, scoped to the agent. The
existing `SandboxDecision` still chooses `NoSandbox` or `Sandbox { policy }`
from sandbox availability, the permission decision, excluded commands, and the
agent's workspace. Filesystem and network restrictions, credential handling,
and the violation store stay in the sandbox crate.

A violation is an Event the agent and hooks can observe. It is not a dropped
frame. Each agent revision has its own sandbox scope: cwd, roots, and network.
Spawn may copy the policy definition. It does not attach the child to the
parent's live sandbox process.

### Tools

A member has one executor node. Tools are names in that member's pool, not
nodes. `ToolRegistry::available_tools` still builds the pool: enabled builtins
first, then MCP, LSP, and plugin tools, with builtin names winning conflicts
and the session allow-list applied. The pool for a revision is a Bind. A call
whose name is absent from that Bind fails before hooks and before the
permission gate.

Several tool uses in one turn are several invocations of the same executor,
each with its own tool-use id. The agent stays at `max_in_flight` 1. The
executor may run a batch concurrently. `PostToolBatch` fires once after the
per-tool `PostToolUse` hooks, before the next model request. A later revision
does not turn one of those ids into a new node.

Pool changes — `is_enabled`, descriptors arriving or leaving, and session tool
allow-lists — become the next Bind without topology patches. Skill
`allowed-tools` and `disallowed-tools` are permission rules: command-source
Allow and Deny rules, retaining content such as `Bash(git commit:*)`. They are
not a replacement tool-name allow-list. Advertising a tool and authorizing a
particular invocation remain separate, and every call still passes the full
gate.

Tool-result media stays on the tool-result Event. It is not mirrored onto the
render stream. Render shows a tool image only when the wired decision carries
it, or when a patch wires some other stream into the join.

Deferred tool search stays a tool call against `ToolRegistry`. It changes what
the model is shown on a later Bind. It does not add an executor.

### Skills

A skill is a `Skill` in `SkillRegistry`: markdown frontmatter and body, or a
skill built from an MCP tool by `skill_from_mcp_tool`. The registry is host
state, like an MCP connection. Names and descriptions enter the agent's prompt
as the skill listing. That listing is a Bind. Loading a skill does not add a
node.

The `Skill` tool is one entry in the tool pool. Model calls and user slash
commands retain their origin and the host's corresponding entry validation:

- `disable-model-invocation: true` rejects model invocation without forbidding
  a host-permitted manual call.
- `user-invocable: false` hides the user slash palette entry, not model
  invocation. Exact manual input keeps existing dispatcher checks.
- Eligible model calls continue through hooks, permission, and executor;
  manual input keeps the host's permission and shell-expansion checks.

What the executor does with a skill depends on `context`:

- `inline`, the default: inject the body. The host retains existing permission
  scopes: embedded shell expansion builds a fresh effective policy with that
  expansion's `allowed-tools`; inline `disallowed-tools` union with the current
  input's denies and are replaced by the next user input. Publish changes for
  later calls through Bind, preserving rule content and scope. Per-skill hooks
  retain their host invocation scope and the session taxonomy.
- `fork`: the result is a spawn patch carrying the skill's `agent`, `model`,
  and `background` flag. `AgentSpawn` still runs before commit. The child
  pool retains the existing agent-definition and parent-advertisement limits.
  Skill permission rules, frozen parent command denies, and
  `additional_disallowed_tools` follow the existing fork path; intersecting
  skill `allowed-tools` strings as names is not a substitute. The child does
  not share the parent's in-flight tool batch.

`paths` skills stay out of the listing until a touched path matches. Activation
is one-way for the session and updates the next Bind. A later non-match does
not withdraw a skill the model has already been shown. `FileChanged` can feed
that check. It does not itself create a node.

`reload_skills` on a hook result refreshes `SkillRegistry` into the next
listing Bind. `SkillDiscoveryPrefetch` may surface a system reminder on the
Event plane. It is off unless `EXPERIMENTAL_SKILL_SEARCH` is on, and it does
not hold the render join.

An MCP disconnect drops skills whose `loaded_from` is that connection, together
with that server's tool descriptors, on the next Bind.

### Multiple receivers

Putting a packet on an output and running a receiver are different steps.
Delivery fills input queues. `Process` is scheduled after that, independently
per receiver.

Delivery walks the receiver list of that revision from front to back. Every
receiver except the last gets a copy. The last receiver takes the queue. The
list order is the order subscribers were added; a patch appends. Delivery does
not call `Process`, and it does not wait for a receiver to finish work it
already had queued.

After delivery, receivers do not wait for each other. The scheduler may run
them in any order, including overlapping when they sit on different executor
queues. Nothing in the fan-out assigns `Process` order. Stream joins wait on
timestamps; parallel/map parents wait for their own invocation's branch
results. Neither is a receiver-order barrier for mirror delivery.

Backpressure is per receiver.

| Receiver policy | Queue full | Effect on the sender | Effect on other receivers |
| --- | --- | --- | --- |
| Wait | Sender is not scheduled again until this queue has room | This receiver blocks the sender | Other receivers keep running what they already have. They get no new packet until the sender is unblocked |
| Drop | This receiver drops according to its own rule (render drops frames) | Sender is not blocked by this receiver | Other receivers are unaffected |
| Error | Mailbox full, closed, or unknown id | Sender gets an error event | There is one addressed receiver, so there is no sibling to affect |

A wait-policy receiver can stall the sender, and through the sender it can
stall further packets to everyone else. It cannot stall a sibling that already
holds a packet. A drop-policy receiver never participates in that stall. Render
stays drop. Tool results and permission asks stay wait. Mailbox stays error.

Hooks on one edge are not a fan-out. They are a chain. Matching hooks run at
priority descending; equal priority keeps the registry's existing order, which
`sort_by` preserves. Every blocking hook is awaited. The first `Block` sticks
as the verdict, and later blocking hooks still run for their side effects. The
packet continues to the single downstream only after that chain returns. A
hook with `blocking == false` is backgrounded, left out of the aggregate, and
cannot block the edge or the other hooks.

Two subscribers on one ingress are two receivers under the rules above. They
are not two hooks. Wiring the same decision stream to render and to a second
agent is fan-out: render drops, the agent waits, and the agent's full queue is
what stalls the producer.

### Context, mode, and model

Context injection, permission mode, conversation profile, and model are four
different axes. None of them is a fan-out, and none of them adds a node.

Context has one receiver: the agent whose next model call will see it. The
graph does not reassemble the prompt. The host's existing assembler still
builds that request. The graph only decides which channel carries each piece,
and it does not mirror that channel to render, to a mailbox, or to another
agent. A second agent gets its own copy only when a sender addresses one to it.

| Channel | What travels on it | When it takes effect |
| --- | --- | --- |
| Bind | System prompt and instruction blocks, `ConversationMode`, tool pool, skill listing, permission mode, model | Next revision. The call already in flight keeps the Bind it started with |
| Meta Event | Hook `additionalContext`, an inline skill body, a skill-discovery reminder, a memory reminder, a `PostModelSwitch` reminder, environment and token reminders | Queued to that agent as a meta message. Wait policy. Not a frame |
| Transcript rewrite | Compaction | Between revisions, after `PreCompact` and before `PostCompact`. One agent's history. Not a packet |

A full meta queue backpressures its producer while allowing the agent to drain
it. The next model request absorbs queued context. This does not occupy the
progress sequencer; render keeps using the previous decision during context
processing or switch-hook waits.

`ConversationMode` (`chat` or `code`) is the session profile fixed when the
session is created. It selects instruction blocks. It does not change the tool
gate or the model. A later profile change, if the host allows one, replaces
those instruction blocks on the next Bind.

Permission mode is the mode inside `PermissionPolicy`: `Default`, `Plan`,
`AcceptEdits`, `BypassPermissions`, `DontAsk`, `Auto`, and the internal
`Bubble`. Plan tools use the existing `PermissionGate` mode-change interface;
the host publishes the result in the next Bind rather than mutating a shared
policy. Plan behavior, explicit rules, mutation asks, and available bypass
grants remain gate-owned; Plan is not an unconditional read-only guarantee.
Mode does not remove the executor or change the model. Calls already admitted
to an old revision, including queued and waiting calls, retain that binding;
new admissions use the new one. Forced revocation is cancellation work.
`Bubble` asks the parent with the original Ask identity.

Model switch is per agent. `command`, `picker`, and `sdk` sources run the
blocking `PreModelSwitch` chain before the model Bind changes. The hook chain
rules apply: priority order, first `Block` sticks, an ask uses that agent's
decision port and pauses only this switch. `auto` and `resume` do not run
`PreModelSwitch`. After the Bind changes, `PostModelSwitch` runs and cannot
roll the model back. Its `additionalContext` becomes one meta message for the
next model request. A parent switch does not change a child. A child takes a
model from its spawn patch or its skill frontmatter, and keeps that model
until it switches itself.

### Join

Each join declares one frame clock domain with strictly increasing logical
timestamps and a checked successor operation. A bound `b` promises that no
future packet has timestamp below `b`; late packets and timestamp wraparound
are rejected. Device clocks require an ingress adapter.

Decisions carry a provenance `source_timestamp` and a separate render
`effective_at`. A lightweight output sequencer serializes frame admission,
decision completion, and revision cutover independently of model execution.
At each frame `t`, it takes the latest current-revision completed candidate,
if any, and enqueues one decision at `effective_at=t`. It then advances the
bound to `successor(t)` and exposes the frame. Packet and bound updates are
atomic to the join; multiple candidates before one frame coalesce by sequencer
order, without coalescing tool-result or mailbox Events.

The join updates its cache from that decision or reuses the previous one.
Before the first decision, frames are consumed and dropped rather than queued
for the model. A model reading frame 10 and finishing just before frame 20
therefore publishes `source_timestamp=10, effective_at=20`, never a late
packet at 10. The sequencer progresses even when the agent ignores later
frames or waits for tools or Ask.

Ordinary Stream joins still wait for a packet at `t` or a bound beyond `t`.
This sequencer adapts render decision state, not tool completion. Stage one
defaults to one wired decision stream; multiple streams require per-source
caches/bounds and an explicit join combination policy.

### Patches in the first stage

Edits that travel as patches:

- Spawn or remove an agent. Spawn waits for `AgentSpawn` before commit. A
  `context: fork` skill is this kind of patch.
- Add a mailbox edge between two existing agents.
- Remove one edge.
- Change the join's sync policy, or change which decision stream is wired
  into the join.
- Add another subscriber on an ingress.

Topology edits travel as patches. Frame payloads, permission decisions, tool
pools, and skill listings do not. MCP connection state, session allowances,
`available_tools`, skill activation, and `reload_skills` cross a revision only
by becoming the next Bind.

## Placement

New crate next to `workflow`. The first stage runs one member inside a
cluster-shaped graph: backpressure, timestamp bounds, one patch, one tool call
through the hook and permission edge, and the executor selecting a sandbox
decision. A second member is the same graph with a spawn patch and a mailbox
edge. The orchestrator turn loop stays where it is until those rules hold in
the library.

The crate calls the existing hook, permission, MCP, sandbox, tool, and skill
contracts. It does not reimplement `PermissionPolicy`, `HookEventType`, MCP
connection state, `SandboxDecision`, `ToolRegistry`, or `SkillRegistry`.
The detailed design's acceptance matrix is required for implementation;
the recovery cases apply when that optional capability is enabled. This
document does not claim those tests have already run.

External packet payloads are opaque to the graph. A later adapter may push
frames from `media-api`. The first stage does not link `media-runtime`.

## Out of scope

- Mutating the adjacency list inside `Process`.
- Timely, differential dataflow, or Hydroflow.
- Replacing `run_turn`.
- Failure and cancellation beyond the cluster cases above. Node `Process`
  errors, closing an external stream, cancelling an agent, and a render
  subscriber disconnecting are still the next design section. Spawn denial,
  mailbox full, permission deny, and MCP disconnect are specified here.
  The general rule for an in-flight tool call meeting a failed node is not.

## Decisions

| Topic | Choice |
| --- | --- |
| First graph | One cluster member: agent, executor, ingress, render, timestamp join. Identity and edges are already cluster-shaped |
| Cluster | One graph, many agents. Mailbox is an addressed Event. Spawn is a patch gated by `AgentSpawn` |
| Join while the agent is working | Runtime sequencer advances bounds at frame admission. Completed decisions take effect on the next frame; source time is separate. Reuse the previous decision or consume/drop before the first |
| Hooks | Existing `HookEventType` interceptors on edges and lifecycle. Sync hooks run before the downstream node. No `&mut Graph` |
| Permission | Revision-bound adapter around the full gate, preserving context, classifier and source. Only classifier denial fires PermissionDenied; PermissionRequest precedes prompt transport |
| MCP | Host-owned connection. Tools use the ordinary tool edge. Elicitation pauses the original request; connection loss without recovery support yields outcome_unknown, never resend |
| Sandbox | Executor environment per agent scope. `SandboxDecision` stays in the sandbox crate. Violations are Events |
| Tools | One executor per member. The tool pool is a Bind from `ToolRegistry::available_tools`. A batch is many invocations of that node, not many nodes |
| Skills | Registry stays host-owned; fork is a spawn patch. allowed-tools/disallowed-tools retain scoped permission-rule semantics. disable-model-invocation gates the model; user-invocable controls the slash palette |
| Multiple receivers | Delivery is in subscriber order and does not run `Process`. Receivers then run independently. Only a wait-policy receiver stalls the sender. Hooks are a priority chain, not a fan-out |
| Control nodes | Basic execution is the leaf. Serial runs the next child only after the previous returns. Parallel starts all branches and emits after every branch result, in declared order. Condition runs one arm and writes a bound on the others. Map is dynamic parallel. Loop is bounded and does not replay a dispatched tool use |
| Context | One receiver, the agent. Bind holds instructions, mode, and model. Meta reminders are wait-policy events. Compaction rewrites that agent's transcript between revisions. None of these are mirrored to render |
| Mode | Session profile is separate from permission mode. Existing gate mode transitions publish the next Bind; admitted calls retain their binding. Plan behavior remains gate-owned |
| Model switch | Per agent. PreModelSwitch can block or ask except auto/resume. PostModelSwitch cannot roll back. Runtime progress continues while the switch waits |
| Graph edits | Idempotent patch ids and base-revision checks; atomic admission cutover; old work drains before Close; join and egress fence old render epochs |
| Side effects | Tool executor records dispatched before send; durable when recovery is enabled; no automatic resend; original invocation identity and revision remain |
| Recovery | In-process Ask required. Cross-process recovery requires durable continuations, call/result journal, Ask deduplication, and no replay of completed or uncertain effects |
| Next step | Failure and cancellation for node errors, stream close, agent cancel, and render disconnect |
