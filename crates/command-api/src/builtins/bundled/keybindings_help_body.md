# Keybindings Skill

Create or modify `~/.lingxi/keybindings.json` to customize keyboard shortcuts.

## CRITICAL: Read Before Write

**Always read `~/.lingxi/keybindings.json` first** (it may not exist yet). Merge changes with existing bindings — never replace the entire file.

- Use **Edit** tool for modifications to existing files
- Use **Write** tool only if the file does not exist yet

## File Format

```json
{
  "$schema": "https://www.schemastore.org/claude-code-keybindings.json",
  "$docs": "https://code.claude.com/docs/en/keybindings",
  "bindings": [
    {
      "context": "Chat",
      "bindings": {
        "ctrl+e": "chat:externalEditor"
      }
    }
  ]
}
```

Always include the `$schema` and `$docs` fields.

## Keystroke Syntax

**Modifiers** (combine with `+`):
- `ctrl` (alias: `control`)
- `alt` (aliases: `opt`, `option`) — note: `alt` and `meta` are identical in terminals
- `shift`
- `meta` (aliases: `cmd`, `command`)

**Special keys**: `escape`/`esc`, `enter`/`return`, `tab`, `space`, `backspace`, `delete`, `up`, `down`, `left`, `right`

**Chords**: Space-separated keystrokes, e.g. `ctrl+k ctrl+s` (1-second timeout between keystrokes)

**Examples**: `ctrl+shift+p`, `alt+enter`, `ctrl+k ctrl+n`

## Unbinding Default Shortcuts

Set a key to `null` to remove its default binding:

```json
{
  "context": "Chat",
  "bindings": {
    "ctrl+s": null
  }
}
```

## How User Bindings Interact with Defaults

- User bindings are **additive** — they are appended after the default bindings
- To **move** a binding to a different key: unbind the old key (`null`) AND add the new binding
- A context only needs to appear in the user's file if they want to change something in that context

## Common Patterns

### Rebind a key
To change the external editor shortcut from `ctrl+g` to `ctrl+e`:
```json
{
  "context": "Chat",
  "bindings": {
    "ctrl+g": null,
    "ctrl+e": "chat:externalEditor"
  }
}
```

### Add a chord binding
```json
{
  "context": "Global",
  "bindings": {
    "ctrl+k ctrl+t": "app:toggleTodos"
  }
}
```

## Behavioral Rules

1. Only include contexts the user wants to change (minimal overrides)
2. Validate that actions and contexts are from the known lists below
3. Warn the user proactively if they choose a key that conflicts with reserved shortcuts or common tools like tmux (`ctrl+b`) and screen (`ctrl+a`)
4. When adding a new binding for an existing action, the new binding is additive (existing default still works unless explicitly unbound)
5. To fully replace a default binding, unbind the old key AND add the new one

## Validation

LingXi validates `~/.lingxi/keybindings.json` when it loads; warnings go to the debug log. After editing the file, re-check it against the rules below and fix anything that matches.

### Common Issues and Fixes

{{ot(["Issue","Cause","Fix"],[['`keybindings.json must have a "bindings" array`',"Missing wrapper object",'Wrap bindings in `{ "bindings": [...] }`'],['`"bindings" must be an array`',"`bindings` is not an array",'Set `"bindings"` to an array: `[{ context: ..., bindings: ... }]`'],['`Unknown context "X"`',"Typo or invalid context name","Use exact context names from the Available Contexts table"],['`Duplicate key "X" in Y bindings`',"Same key defined twice in one context","Remove the duplicate; JSON uses only the last value"],['`"X" may not work: ...`',"Key conflicts with terminal/OS reserved shortcut","Choose a different key (see Reserved Shortcuts section)"],['`Invalid action for "X"`',"Action value is not a string or null",'Actions must be strings like `"app:help"` or `null` to unbind']])}}

### Example validation warnings (debug log)

```
[keybindings] Found 2 validation issue(s)
[keybindings] [error] Unknown context "chat" — Valid contexts: Global, Chat, Autocomplete, ...
[keybindings] [warning] "ctrl+c" may not work: Terminal interrupt (SIGINT)
```

**Errors** prevent bindings from working and must be fixed. **Warnings** indicate potential conflicts but the binding may still work.