# Subagents, Fusion and collaboration

A subagent is a delegated runtime execution with its own tool/model policy and
result. It is not a persistent Bot, an App Agent, a remote host or a durable
schedule. Use the available Agent/collaboration schema and current catalog;
custom agent types and tools can differ by host and session.

Delegate a specific goal and required context. The child receives its permitted
tools and workspace; do not assume it has all parent capabilities, credentials,
OS permission or files. A skill describing a tool does not add that tool to a
child. A live Media conversation belongs to its interactive main-session
handoff, not an arbitrary child or scheduled turn.

Fusion, where offered, is the `fusion` Agent type with multiple configured
executors plus its recorder/analysis machinery. Check the actual catalog and
configured model profiles; do not infer readiness from the type's presence.
Its extra model work consumes additional usage. Treat panels' claims as results
to evaluate, not independent proof that an external action occurred.

Preserve agent, session, task and Bot identities and use the relevant service
for continuation/cancellation. A turn being interrupted does not prove all
spawned processes stopped. Report terminal state from the owning task service.
`ListAgents` and background task lists need not describe the same population.

Source anchors: Harness `crates/agent/src/{builtins,tool_resolver}.rs`,
`crates/tools/agent/src/agent.rs`, `crates/fusion`,
`crates/runtime/src/desktop/background_agent.rs`.
