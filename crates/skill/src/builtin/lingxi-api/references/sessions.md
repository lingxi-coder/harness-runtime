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

On the CLI, no argument opens an interactive session. A positional prompt runs
one turn; `-p` / `--print` prints that turn and exits. `--continue` reopens the
latest session in this directory. `--resume` picks one, and `--resume ID`
reopens that id. `--resume ID --fork-session` keeps that history under a new
session id. Trust a folder only when the user does; after confirmation the
session can read, edit, and run files there.

Instruction files are the project-root `{{memory_file}}`,
`{{config_dir}}/{{memory_file}}`, and `{{memory_local_file}}`, plus the user
file `~/{{config_dir}}/{{memory_file}}`. `/init` creates the project
`{{memory_file}}`. With the default instruction mode, a project that already
has one of those branded files does not also load `AGENTS.md`. When it has
none, LingXi reads `AGENTS.md` or `{{config_dir}}/AGENTS.md`. An import that
points outside the project asks before it is read. `/memory` lists the files
loaded for this session.

The config home is `{{config_dir_env}}` when that variable is set, including an
empty value. The CLI resolves a relative or empty value against the working
directory. When the variable is unset, the home is `~/{{config_dir}}`.
`/doctor` prints the home it is using. Settings for the session are
`settings.json` in that directory.

Source anchors: Harness `crates/session`, `crates/compaction`,
`crates/memory/src/lingxi_md`, `crates/runtime/src/mobile/skill_loader.rs`,
`crates/runtime/src/app_agent_input.rs`; product `apps/cli/host/src/argv.rs`
and native conversation and remote session controls.

Mobile currently exposes ForkSession with target_mode and rejects an in-flight
mode fork. Desktop does not expose that same fork command. Creating a new
Desktop session and forking a Mobile session are different operations; a future
session-space design is not proof of current product behavior.
