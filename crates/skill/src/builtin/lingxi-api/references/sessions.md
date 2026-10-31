# Sessions, context and memory

A session owns conversational history and runtime state; the current host owns
its persisted storage and UI controls. Use native history/resume/fork commands
or an available tool. Do not edit transcript JSONL to simulate a resumed turn,
changed permission, branch, or tool result.

Resume continues a selected conversation. Fork creates a branch from the
selected history; it does not transfer every live resource, OS permission or
in-flight operation. Keep the source session and identifiers distinct. A remote
viewer attaches to the host's session rather than importing it into a new local
history file.

Mobile Chat and Code are capability profiles. Chat may filter out shell-based,
forked or undeclared skills. Only the native control/SDK at the current version
determines whether a change is in place or creates a fork with `target_mode`.
Do not invent a `SetSessionMode` tool or claim a mode change from text alone.

Context-window compaction is distinct from durable memory and full history.
Explain what was retained only from the actual context/status result. Ordinary
session memory, Bot memory, App business data and retrieved visual assets have
different owners. A source image ID or cached metadata does not put pixels into
the current model context. Local App Agent invocations can intentionally omit
ordinary automatic/session memory.

When a tool, plugin, model or workspace changes, check the current caller's
catalog/schema again. A description loaded earlier is not a permanent grant.
Report pending permission or question state rather than treating it as an idle
or completed Agent.

Source anchors: Harness `crates/session`, `crates/compaction`,
`crates/runtime/src/mobile/skill_loader.rs`, `crates/runtime/src/app_agent_input.rs`;
product native conversation and remote session controls.
