---
name: plugin-authoring
description: Reference for building LingXi plugins with hook modules, skills, commands, and marketplace packaging.
user-invocable: true
triggers:
  - plugin-authoring
  - write a LingXi plugin
  - programmatic hooks
  - hooks module
---

# LingXi plugin authoring

Use this reference to plan or change a LingXi plugin. A plugin may contain
skills, slash commands, agents, hooks, output styles, MCP servers, and other
components. Loading this reference does not enable a plugin or authorize a
hook to run.

## Start with the plugin layout

Put the plugin components under one root directory. A programmatic Mods module
uses this layout:

```text
my-plugin/
  .lingxi-plugin/
    plugin.json
  hooks/
    hooks.json
    register.ts
```

The manifest requires a `name`; `version` and `description` are useful metadata:

```json
{
  "name": "my-plugin",
  "version": "0.1.0",
  "description": "A short description of this plugin"
}
```

Declare one module in `hooks/hooks.json`. Its path is relative to that file and
must stay inside the plugin root:

```json
{ "modules": ["./register.ts"] }
```

The file may also declare ordinary `hooks` matchers, but `modules` accepts at
most one path. Keep unrelated features in their component directories, such as
`skills/<name>/SKILL.md` or `commands/<name>.md`.

## Register hooks

The module exports `register(on, options)`. `options` carries the plugin's
resolved user configuration. Register a handler with `on(event, matcher?,
handler)`. A handler receives the host capability object `$`, the event value
`e`, and `next`:

```ts
export function register(on, options) {
  on("prompt.submit", async ($, e, next) => {
    const result = await next(e);
    return result;
  });
}
```

Calling `next(e)` forwards the event and returns the downstream result. Passing
`next({...e, field: value})` changes what downstream handlers and the host see.
A handler that returns without calling `next` answers that event itself. Use a
matcher to limit a hook to an event subtype, such as one tool name. Register a
given event/matcher pair only once.

The module runs in a Bun-backed isolated worker. TypeScript files and relative
imports are supported, but the worker does not provide Node or DOM globals and
LingXi does not generate a per-plugin API declaration package. Use `$` for host
capabilities; if your editor needs types, keep declarations with your plugin
and verify the module in a real LingXi session.

Useful event families include `prompt.submit` and `prompt.compose` for prompt
changes; `tool.call` and `tool.check` for tool behavior; `session.start`,
`session.receive`, `session.append`, and `session.end` for session lifecycle;
`agent.offer` and `agent.spawn` for agent integration; and `ui.render` for
surface-specific UI. The runtime rejects unsupported events and invalid event
results. Consult the current Mod examples and API implementation when you need
the exact event fields.

## Use the host APIs

`$` is the boundary to host functionality. Depending on the event and the
plugin's grants, it exposes APIs such as `$.ui`, `$.command`, `$.clock`,
`$.tool`, `$.agent`, `$.model`, `$.fs`, `$.process`, `$.state`, and `$.store`.
For example, `$.ui.status(text)` updates status, `$.ui.toast(text)` shows a
toast, and `$.command.register({ name, description })` registers a command
from a session hook; the corresponding `command.run` hook returns its text.
UI rendering uses the active surface's elements and event contract. Keep
session values in `$.state` and use `$.store` only for data intended to survive
between sessions.

Treat event data, plugin files, and user configuration as untrusted input.
Validate values before using them, keep filesystem access inside the intended
plugin scope, and make tool hooks explicit about whether they forward, modify,
or deny an operation.

## Validate and exercise the plugin

Run `lingxi-cli plugin validate <plugin-root>` to check the manifest,
components, hook wrapper, and module path. Validation does not execute the
module or type-check TypeScript. Load a local plugin with the repeatable
`--plugin-dir <plugin-root>` option, then exercise each hook in the host and
inspect its logs and resulting behavior. After editing a loaded module, use
`/reload-plugins` or start a new session; file edits alone do not promise a
reload.

For marketplace distribution, place the marketplace manifest at
`.lingxi-plugin/marketplace.json` and use the `lingxi-cli plugin` install and
marketplace commands. Keep the marketplace entry, plugin manifest, and
component paths consistent.
