# Sessions, context and memory

The host owns persisted session storage and navigation; the runtime owns
conversation history and execution state. Use native history/resume/fork
entries. Do not edit JSONL to simulate a turn, permission or tool result.

Chat and Code are separate conversation spaces. Each conversation has a fixed
mode and logical Project owner. Switching tabs selects that space's navigation;
it does not change a conversation's mode. A no-Project conversation still has
a real working directory. Tools and skills follow its fixed mode and host.

Native handoff prepares a summary and a new draft in the other space. The user
sends its first message. A backlink preserves the source identity; live tools,
approvals and grants do not transfer. Mobile `ForkSession` is a separate
same-mode branch: a different `target_mode` is rejected. Do not invent a
`SetSessionMode` tool. Remote viewing attaches to the host's existing session;
it does not import a new local history.

Compaction is distinct from full history and durable memory. Explain retention
from actual status. Session/Bot memory, App data and retrieved assets have
separate owners. An image ID or metadata does not put pixels in model context.
Local App Agent runs may omit ordinary automatic/session memory. Pending
permission or questions mean the Agent is waiting, not completed.

On the CLI, no argument opens an interactive session. A positional prompt runs
one turn; `-p` / `--print` prints it and exits. `--continue` reopens the latest
session here; `--resume` picks one; `--resume ID` selects its ID.
`--resume ID --fork-session` branches its history under a new session ID.
Folder trust and current tool grants still apply after resume or fork.

Instruction files are project-root `{{memory_file}}`,
`{{config_dir}}/{{memory_file}}` and `{{memory_local_file}}`, plus
`~/{{config_dir}}/{{memory_file}}`. `/init` creates the project file. Under the
default instruction mode, branded project files take precedence; when none
exist, LingXi reads `AGENTS.md` or `{{config_dir}}/AGENTS.md`. An import outside
the project asks before reading. `/memory` lists loaded instruction files.

The config home is `{{config_dir_env}}` when set, including an empty value.
The CLI resolves relative/empty values against cwd. Otherwise it uses
`~/{{config_dir}}`. `/doctor` prints the resolved home; its `settings.json`
contains user settings. The sibling workspace root appends `-workspaces` to
the config directory name: default `~/{{config_dir}}-workspaces`. No-Project
Chat/Code use `chat/` and `code/`; Local App sources use `apps/`. Explicitly
selected Projects keep their cwd. Resolve host paths rather than using private
configuration/transcript directories as working directories.

A transcript can remain in its original bucket after cwd changes. Discovery
uses exact relocation links and fixed ownership, not titles. Renaming does
not create a session. For a missing renamed chat, inspect UUID, fixed mode,
selected source and catalog scope before concluding that history was deleted.
Preserve its file and use host recovery without broadening directory grants.

Source anchors: Harness `crates/session`, `crates/compaction`,
`crates/memory/src/lingxi_md`, `crates/runtime/src/mobile/{host,skill_loader}.rs`,
`crates/runtime/src/app_agent_input.rs`; product `apps/cli/host/src/argv.rs`,
`apps/electron/src/{shared/conversationSpaces,main/conversationSpaceStore,main/host}.ts`,
`apps/ios/native/Sources/App/SessionHost.swift`, `docs/WORKSPACE_STORAGE.md`.
