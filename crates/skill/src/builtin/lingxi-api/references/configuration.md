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

If the user requests file configuration and permitted file tools are available,
read and merge the correct JSON store directly, retaining other keys and
servers. Do not overwrite malformed content or a concurrently changed file.
Validate JSON syntax and report that connection/authentication remains unchecked
until a real reload or connection result. Host-managed Bot configuration and
router secrets must be changed through their owner, not this fallback.

Before changing a server, resolve the requested scope, inspect existing content,
and preserve unrelated entries. Do not put MCP server definitions into
`settings.json`, copy credentials into a shared project, or show secret values.
Plugin-provided and managed server definitions are not writable user scopes.
Afterward report separately what was persisted, whether login is needed,
whether a connection succeeded, and which tools were actually discovered.

Hooks react to runtime events; a memory entry does not execute them. Cron and
Bot routines schedule work and have different owners. Permissions remain
runtime/host decisions even when the skill describes the desired operation.

A skill is a directory that contains `SKILL.md`. Project skills are
`{{config_dir}}/skills` directories from the working directory up. The walk ends at the git root when that root is below the home directory, and at the home directory when there is no such root. User skills are
`~/{{config_dir}}/skills`. `lingxi plugin init NAME`
scaffolds that user directory. `--disable-slash-commands` empties the
slash-command and skill registry for that launch. `--bare` and `--safe-mode` both disable settings hooks, plugins, skills, and custom agents. `--bare` skips instruction auto-discovery unless `--add-dir` is set, still discovers
MCP servers, and skips managed policy hooks. `--safe-mode` always skips
instruction files and discovered `.mcp.json` servers, and it still runs managed policy hooks. Servers passed with `--mcp-config` remain. A skill body
does not install an MCP server or add a tool by naming it. Use
`plugin-authoring` or `workflow-authoring` only if that skill is registered.

Source anchors: Harness `crates/configuration-admin/src/mcp_bridge.rs`,
`crates/command-api/src/builtins/bundled/update_config_skill.rs`; product
`apps/cli/host/src/commands/mcp.rs` and native settings surfaces.

Configuration has distinct owners: Harness settings are the layers above;
Desktop preferences are userData/settings.v1.json; provider account metadata is
host-global under provider-accounts/<store_scope>.json, with credentials in the
native broker/secure store. Bot and Local App grants have their own native
owners. These are not interchangeable JSON files. /config opens the resolved
Harness user file, not Desktop appearance settings. Config cannot set native
appearance preferences. Do not infer .claude or CLAUDE.md paths from code ancestry. Provider-specific
authentication variables apply only to the selected provider; actual Anthropic
protocol identifiers and credentials remain valid on an Anthropic route.
Config-home and global-file rules are in sessions. Do not collapse them.
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
