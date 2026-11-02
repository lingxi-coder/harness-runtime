# Update Config Skill

Modify LingXi configuration by updating settings.json files.

## When Hooks Are Required (Not Memory)

If the user wants something to happen automatically in response to an EVENT, they need a **hook** configured in settings.json. Memory/preferences cannot trigger automated actions.

**These require hooks:**
- "Before compacting, ask me what to preserve" → PreCompact hook
- "After writing files, run prettier" → PostToolUse hook with Write|Edit matcher
- "When I run bash commands, log them" → PreToolUse hook with Bash matcher
- "Always run tests after code changes" → PostToolUse hook

**Hook events:** PreToolUse, PostToolUse, PreCompact, PostCompact, Stop, Notification, SessionStart

## CRITICAL: Read Before Write

**Always read the existing settings file before making changes.** Merge new settings with existing ones - never replace the entire file.

## CRITICAL: Use AskUserQuestion for Ambiguity

When the user's request is ambiguous, use AskUserQuestion to clarify:
- Which settings file to modify (user/project/local)
- Whether to add to existing arrays or replace them
- Specific values when multiple options exist

## Decision: /config command vs Direct Edit

`/config` opens the current host's resolved Harness user settings file in
its editor. It is not the Desktop appearance settings UI. Use native Settings
for theme, editor/input preferences and provider account login/model selection.
Do not write these client preferences into Harness settings.json or assume a
saved default changes an already running session.

Edit the resolved user/project/local settings.json for schema-supported
Harness permissions, hooks, environment and plugin configuration. Honor the
host-pinned configuration home; an empty {{config_dir_env}} means unset.

**MCP server definitions use separate stores**, not settings.json:
user scope is `~/{{global_config_file}}` → `mcpServers`, local scope is
`~/{{global_config_file}}` → `projects[<cwd>].mcpServers`, and shared project scope is
`<project>/.mcp.json` → `mcpServers`. Resolve a relocated configuration home
through the host. Prefer the available LingXi CLI's `mcp` management commands
or the native MCP settings UI; check that version's help for flags. Preserve
unrelated entries and distinguish saving, authentication, connection and tool
discovery. The `lingxi-api` skill's `configuration` topic explains these boundaries.

## Workflow

1. **Clarify intent** - Ask if the request is ambiguous
2. **Read existing file** - Use Read tool on the target settings file
3. **Merge carefully** - Preserve existing settings, especially arrays
4. **Edit file** - Use Edit tool (create a missing file only at the resolved, authorized settings scope)
5. **Confirm** - Tell user what was changed

## Merging Arrays (Important!)

When adding to permission arrays or hook arrays, **merge with existing**, don't replace:

**WRONG** (replaces existing permissions):
```json
{ "permissions": { "allow": ["Bash(npm *)"] } }
```

**RIGHT** (preserves existing + adds new):
```json
{
  "permissions": {
    "allow": [
      "Bash(git *)",
      "Edit({{config_dir}})",
      "Bash(npm *)"
    ]
  }
}
```

## Settings scope

User: resolved config-home/settings.json (default ~/{{config_dir}}/settings.json).
Project: <project>/{{config_dir}}/settings.json. Private project overrides:
<project>/{{config_dir}}/settings.local.json. Managed policy can restrict every layer.
These are separate from MCP stores, client preferences and provider accounts.

For hook event shapes and construction examples, invoke `update-config` with
`[hooks-only] <request>` when that entry is available.

Use the generated schema below as the authority for accepted settings keys.

## Example Workflows

### Adding a Hook

User: "Format my code after LingXi writes it"

1. **Clarify**: Which formatter? (prettier, gofmt, etc.)
2. **Read**: `{{config_dir}}/settings.json` (or create if missing)
3. **Merge**: Add to existing hooks, don't replace
4. **Result**:
```json
{
  "hooks": {
    "PostToolUse": [{
      "matcher": "Write|Edit",
      "hooks": [{
        "type": "command",
        "command": "jq -r '.tool_response.filePath // .tool_input.file_path' | { read -r f; prettier --write \"$f\"; } 2>/dev/null || true"
      }]
    }]
  }
}
```

### Adding Permissions

User: "Allow npm commands without prompting"

1. **Read**: Existing permissions
2. **Merge**: Add `Bash(npm *)` to allow array
3. **Result**: Combined with existing allows

### Environment Variables

User: "Set DEBUG=true"

1. **Decide**: User settings (global) or project settings?
2. **Read**: Target file
3. **Merge**: Add to env object
```json
{ "env": { "DEBUG": "true" } }
```

## Common Mistakes to Avoid

1. **Replacing instead of merging** - Always preserve existing settings
2. **Wrong file** - Ask user if scope is unclear
3. **Invalid JSON** - Validate syntax after changes
4. **Forgetting to read first** - Always read before write

## Troubleshooting Hooks

If a hook isn't running:
1. **Check the settings file** - Read ~/{{config_dir}}/settings.json or {{config_dir}}/settings.json
2. **Verify JSON syntax** - Invalid JSON produces a settings parse diagnostic; preserve the original and fix the reported syntax error
3. **Check the matcher** - Does it match the tool name? (e.g., "Bash", "Write", "Edit")
4. **Check hook type** - Is it "command", "prompt", or "agent"?
5. **Test the command** - Run the hook command manually to see if it works
6. **Use --debug** - Run `lingxi-cli --debug` to see hook execution logs
