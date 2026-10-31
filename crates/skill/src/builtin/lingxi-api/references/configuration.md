# Settings, MCP and extensions

Use the current `update-config` skill for settings/hooks when it is available;
it includes the loader's actual Settings JSON Schema. Settings are layered:
user `~/.lingxi/settings.json`, project `.lingxi/settings.json`, and private
project `.lingxi/settings.local.json`. Managed policy can restrict them.
Resolve the host's configured home rather than assuming the default path.

MCP has separate storage:

| Scope | Default store |
| --- | --- |
| User | `~/.lingxi.json`, top-level `mcpServers` |
| Local project | `~/.lingxi.json`, `projects[<cwd>].mcpServers` |
| Shared project | `<project>/.mcp.json`, `mcpServers` |

Use an available LingXi CLI's `mcp` subcommands or the native MCP settings UI.
Inspect that CLI version's help before constructing flags. A model without that
CLI or a registered management tool can explain the native steps. Host-private
Bridge/FFI commands are not implicitly callable through `Skill`.

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

Project/user skills and installed plugin skills extend the current catalog.
Use `plugin-authoring` or `workflow-authoring` only if present. A skill body
does not install its required MCP server or add a tool by mentioning its name.

Source anchors: Harness `crates/configuration-admin/src/mcp_bridge.rs`,
`crates/command-api/src/builtins/bundled/update_config_skill.rs`; product
`apps/cli/host/src/commands/mcp.rs` and native settings surfaces.
