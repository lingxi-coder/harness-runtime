# Settings, MCP and extensions

Use the current `update-config` skill for settings/hooks when it is available;
it includes the loader's actual Settings JSON Schema. Settings are layered:
user `~/{{config_dir}}/settings.json`, project `{{config_dir}}/settings.json`, and private
project `{{config_dir}}/settings.local.json`. Managed policy can restrict them.
Resolve the host's configured home rather than assuming the default path.

MCP has separate storage:

| Scope | Default store |
| --- | --- |
| User | `~/{{global_config_file}}`, top-level `mcpServers` |
| Local project | `~/{{global_config_file}}`, `projects[<cwd>].mcpServers` |
| Shared project | `<project>/.mcp.json`, `mcpServers` |

`lingxi mcp add` is the ordinary way to add a server. On Mac and on WSL,
`lingxi mcp add-from-claude-desktop` only imports servers that desktop app
already saved. Inspect that CLI version's help before constructing other flags.
Host-private Bridge/FFI commands are not implicitly callable through `Skill`.

On Desktop, open Settings → MCP servers, use that page's own scope selector,
then add/edit the server. The general settings-layer selector is not the MCP
scope selector. On mobile, use the native MCP settings page and inspect the
scope/location it shows; labels can differ by build.

For a requested file change, read and merge the correct store; preserve
unrelated entries and reject malformed/concurrently changed data. Do not put
MCP definitions into `settings.json`, credentials in shared files, or rewrite
plugin/managed definitions. Report saved configuration, login, connection and
discovered tools separately. Bot/router configuration uses its native owner.
Hooks react to events; a memory entry does not execute them.

A skill is a directory that contains `SKILL.md`. Project skills are
`{{config_dir}}/skills` directories from the working directory up. The walk ends at the git root when that root is below the home directory, and at the home directory when there is no such root. User skills are
`~/{{config_dir}}/skills`. `lingxi plugin init NAME`
scaffolds that user directory. `--disable-slash-commands` empties the
slash-command and skill registry for that launch. `--bare` and `--safe-mode` both disable settings hooks, plugins, skills, and custom agents. `--bare` skips instruction auto-discovery unless `--add-dir` is set, still discovers
MCP servers, and skips managed policy hooks. `--safe-mode` always skips
instruction files and discovered `.mcp.json` servers, and it still runs managed policy hooks. Servers passed with `--mcp-config` remain. A skill body
does not install an MCP server or add a tool by naming it. Use
`plugin-authoring` or `workflow-authoring` only if that skill is registered.

Bundled skills such as `lingxi-api` are compiled in, with no filesystem skill
root. Use the catalog and `Skill` instead of scanning private configuration.
File reads cover permitted workspaces and registered skill roots; a known user
skills path does not grant access to the whole tree.

For installation, preserve the original frontmatter, instructions and linked
resources in the chosen scope. Verify loader refresh separately from file
creation. Instructions do not install CLI/browser dependencies: report saved,
loaded and executed separately. Desktop built-ins work without the standalone
`lingxi` executable on PATH.

Source anchors: Harness `crates/configuration-admin/src/mcp_bridge.rs`,
`crates/command-api/src/builtins/bundled/update_config_skill.rs`; product
`apps/cli/host/src/commands/mcp.rs` and native settings surfaces.

Desktop preferences are `userData/settings.v1.json`; provider metadata is
host-global `provider-accounts/<store_scope>.json`, with credentials in the
native broker. Bot/App grants have separate owners. `/config` opens the
resolved Harness user file, not Desktop appearance settings. Provider auth
variables apply only to their selected route. Config-home rules are in sessions.

Permission mode decides whether an unmatched tool call asks. It does not start
an operating-system sandbox. The starting mode is manual (ask first) and prints
no mode line. Shift+Tab cycles modes in the terminal. `--permission-mode`
selects one launch. Help shows `manual`; `default` is a hidden alias of that
same mode. `acceptEdits` allows file edits without asking. `plan` stays
read-only. `dontAsk` denies anything without an allow rule.
`bypassPermissions` allows actions unless its kill switch is on;
`--dangerously-skip-permissions` selects it. `auto` lets the classifier decide
and can be selected for one launch.

In `settings.json` the field is `permissions.defaultMode`, not a top-level
`permissionMode`. With no `--setting-sources`, the session policy reads the
user file, the project `settings.json`, then `settings.local.json`, then `--settings`, then managed policy. A later allowed value replaces the earlier
one. The project file and `settings.local.json` can set `acceptEdits`, `plan`,
`dontAsk`, and `manual`. They cannot grant `auto` or `bypassPermissions`.
User settings, `--settings`, and managed policy can. `--restricted` skips the
user, project, and local files. `--settings` still applies under
`--restricted`. Hooks can still read the local file. User settings may store
`auto`; the session stays manual when auto mode is unavailable. Internal
modes are not user settings.
