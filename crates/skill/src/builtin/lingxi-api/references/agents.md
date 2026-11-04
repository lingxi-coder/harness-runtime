# Subagents, Fusion and collaboration

A subagent is a delegated runtime execution with its own tool/model policy and
result. It is not a persistent Bot, an App Agent, a remote host or a durable
schedule. Use the available Agent/collaboration schema and current catalog;
custom agent types and tools can differ by host and session.

Definitions are Markdown files in project `{{config_dir}}/agents/` or user
`~/{{config_dir}}/agents/`, including nested directories. A deeper project file
wins over a shallower one, and a project file wins over the user file.
`/agents` tells the user to create or edit those files. It does not open a
manager. `lingxi agents` opens the background-session view in a terminal.
`lingxi agents --json` prints that listing. `--agent NAME` selects the
agent for one launch. `--agents` takes JSON that overrides the files for that
launch; safe mode ignores that JSON.

Agent view is on unless settings set `disableAgentView` to true or
`LINGXI_DISABLE_AGENT_VIEW` is `1`, `true`, `yes`, or `on`. Either one is
enough. While it is on, `/fork` copies this conversation into a background
session. `/subtask` needs a task, runs only after the first assistant turn,
and is unavailable in a coordinator session. The command returns when the
spawn is accepted; the subagent result comes back to this session later.
While agent view is off, `/fork` is the in-session spawn and `/subtask` is not registered.

Delegate a specific goal and required context. The child receives its permitted
tools and workspace; do not assume it has all parent capabilities, credentials,
OS permission or files. A skill describing a tool does not add that tool to a
child. A live Media conversation belongs to its interactive main-session
handoff, not an arbitrary child or scheduled turn.

Fusion, where offered, is the `fusion` Agent type. Check the catalog before
calling it ready. Its extra model work costs more usage, and a panel result is
not proof that an external action happened.

Preserve agent, session, task and Bot identities and use the relevant service
for continuation/cancellation. A turn being interrupted does not prove all
spawned processes stopped. Report terminal state from the owning task service.
`ListAgents` and background task lists need not describe the same population.

Source anchors: Harness `crates/agent/src/{builtins,tool_resolver}.rs`,
`crates/tools/agent/src/agent.rs`, `crates/fusion`,
`crates/runtime/src/desktop/background_agent.rs`.

Execution completion is distinct from report delivery. When handback is enabled,
only a host-issued receipt establishes delivered/flagged delivery; withheld or
pending reports are not successful parent results. Plain text and a Done label
are insufficient. Host-supplied control tools use the enforcing invoker and
cannot be replaced by a Mod result. Cancel/resume preserves run-generation and
receipt identity. Pool and host configuration determine actual concurrency.
lingxi-code-guide has only Read/Grep/Glob and the same bundled references as this
skill: repeating that corpus is not independent verification of source code.
