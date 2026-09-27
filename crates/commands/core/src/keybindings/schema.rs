//! Keybinding context + action vocabularies — a 1:1 port of the runtime-relevant
//! constants from `claude-code/src/keybindings/schema.ts`.
//!
//! The TS file uses Zod for JSON-schema generation; we only need the value
//! tables (`KEYBINDING_CONTEXTS`, `KEYBINDING_CONTEXT_DESCRIPTIONS`,
//! `KEYBINDING_ACTIONS`) for validation. The zod schema/JSON-schema generation
//! itself is not part of the loader path (it powers the `$schema` editor URL,
//! which the template already references as a static string).

/// Valid context names where keybindings can be applied.
/// 1:1 with `KEYBINDING_CONTEXTS` (schema.ts:12-32), same order.
pub const KEYBINDING_CONTEXTS: &[&str] = &[
    "Global",
    "Chat",
    "Autocomplete",
    "Confirmation",
    "Help",
    "Transcript",
    "HistorySearch",
    "Task",
    "ThemePicker",
    "Settings",
    "Tabs",
    // New contexts for keybindings migration
    "Attachments",
    "Footer",
    "MessageSelector",
    "DiffDialog",
    "ModelPicker",
    "Select",
    "Plugin",
];

/// Human-readable descriptions for each keybinding context.
/// 1:1 with `KEYBINDING_CONTEXT_DESCRIPTIONS` (schema.ts:37-59).
pub const KEYBINDING_CONTEXT_DESCRIPTIONS: &[(&str, &str)] = &[
    ("Global", "Active everywhere, regardless of focus"),
    ("Chat", "When the chat input is focused"),
    ("Autocomplete", "When autocomplete menu is visible"),
    (
        "Confirmation",
        "When a confirmation/permission dialog is shown",
    ),
    ("Help", "When the help overlay is open"),
    ("Transcript", "When viewing the transcript"),
    ("HistorySearch", "When searching command history (ctrl+r)"),
    ("Task", "When a task/agent is running in the foreground"),
    ("ThemePicker", "When the theme picker is open"),
    ("Settings", "When the settings menu is open"),
    ("Tabs", "When tab navigation is active"),
    (
        "Attachments",
        "When navigating image attachments in a select dialog",
    ),
    ("Footer", "When footer indicators are focused"),
    (
        "MessageSelector",
        "When the message selector (rewind) is open",
    ),
    ("DiffDialog", "When the diff dialog is open"),
    ("ModelPicker", "When the model picker is open"),
    ("Select", "When a select/list component is focused"),
    ("Plugin", "When the plugin dialog is open"),
];

/// All valid keybinding action identifiers.
/// 1:1 with `KEYBINDING_ACTIONS` (schema.ts:64-172), same order.
pub const KEYBINDING_ACTIONS: &[&str] = &[
    // App-level actions (Global context)
    "app:interrupt",
    "app:exit",
    "app:toggleTodos",
    "app:toggleTranscript",
    "app:toggleBrief",
    "app:toggleTeammatePreview",
    "app:toggleTerminal",
    "app:redraw",
    "app:globalSearch",
    "app:quickOpen",
    // History navigation
    "history:search",
    "history:previous",
    "history:next",
    // Chat input actions
    "chat:cancel",
    "chat:killAgents",
    "chat:cycleMode",
    "chat:modelPicker",
    "chat:fastMode",
    "chat:thinkingToggle",
    "chat:submit",
    "chat:newline",
    "chat:undo",
    "chat:externalEditor",
    "chat:stash",
    "chat:imagePaste",
    "chat:messageActions",
    // Autocomplete menu actions
    "autocomplete:accept",
    "autocomplete:dismiss",
    "autocomplete:previous",
    "autocomplete:next",
    // Confirmation dialog actions
    "confirm:yes",
    "confirm:no",
    "confirm:previous",
    "confirm:next",
    "confirm:nextField",
    "confirm:previousField",
    "confirm:cycleMode",
    "confirm:toggle",
    "confirm:toggleExplanation",
    // Tabs navigation actions
    "tabs:next",
    "tabs:previous",
    // Transcript viewer actions
    "transcript:toggleShowAll",
    "transcript:exit",
    // History search actions
    "historySearch:next",
    "historySearch:accept",
    "historySearch:cancel",
    "historySearch:execute",
    // Task/agent actions
    "task:background",
    // Theme picker actions
    "theme:toggleSyntaxHighlighting",
    // Help menu actions
    "help:dismiss",
    // Attachment navigation (select dialog image attachments)
    "attachments:next",
    "attachments:previous",
    "attachments:remove",
    "attachments:exit",
    // Footer indicator actions
    "footer:up",
    "footer:down",
    "footer:next",
    "footer:previous",
    "footer:openSelected",
    "footer:clearSelection",
    "footer:close",
    // Message selector (rewind) actions
    "messageSelector:up",
    "messageSelector:down",
    "messageSelector:top",
    "messageSelector:bottom",
    "messageSelector:select",
    // Diff dialog actions
    "diff:dismiss",
    "diff:previousSource",
    "diff:nextSource",
    "diff:back",
    "diff:viewDetails",
    "diff:previousFile",
    "diff:nextFile",
    // Model picker actions (ant-only)
    "modelPicker:decreaseEffort",
    "modelPicker:increaseEffort",
    // Select component actions (distinct from confirm: to avoid collisions)
    "select:next",
    "select:previous",
    "select:accept",
    "select:cancel",
    // Plugin dialog actions
    "plugin:toggle",
    "plugin:install",
    // Permission dialog actions
    "permission:toggleDebug",
    // Settings config panel actions
    "settings:search",
    "settings:retry",
    "settings:close",
    // Voice actions
    "voice:pushToTalk",
];

/// Whether `value` is a recognized context name.
#[must_use]
pub fn is_valid_context(value: &str) -> bool {
    KEYBINDING_CONTEXTS.contains(&value)
}

/// Whether `value` is a recognized action id (one of [`KEYBINDING_ACTIONS`]).
/// `command:*` bindings are NOT in this list — they are validated separately by
/// the `^command:[a-zA-Z0-9:\-_]+$` regex in `validate`.
#[must_use]
pub fn is_valid_action(value: &str) -> bool {
    KEYBINDING_ACTIONS.contains(&value)
}
