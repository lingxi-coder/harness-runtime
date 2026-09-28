//! Default keybindings — a 1:1 port of
//! `claude-code/src/keybindings/defaultBindings.ts` for the canonical build.
//!
//! ## Canonical build
//!
//! The TS `DEFAULT_BINDINGS` is parameterized by feature flags
//! (`KAIROS`/`KAIROS_BRIEF`, `QUICK_SEARCH`, `TERMINAL_PANEL`, `MESSAGE_ACTIONS`,
//! `VOICE_MODE`) and platform branches. This port pins the canonical build that
//! every other keybindings artifact in this crate already targets (see
//! `mod.rs` / `keybindings_template.json`):
//!
//! - all five feature flags **off** (so the `KAIROS`, `QUICK_SEARCH`,
//!   `TERMINAL_PANEL`, `MESSAGE_ACTIONS`, `VOICE_MODE` rows and the
//!   `MessageActions` block are omitted);
//! - non-Windows / VT-mode-capable platform → `IMAGE_PASTE_KEY = "ctrl+v"` and
//!   `MODE_CYCLE_KEY = "shift+tab"`.
//!
//! Unlike the `/keybindings` template (which strips the reserved/non-rebindable
//! rows via `filterReservedShortcuts`), these defaults INCLUDE every row exactly
//! as `DEFAULT_BINDINGS` does — `ctrl+c`/`ctrl+d`/`enter`/`escape`/`up`/`down`
//! etc. — because the resolver must be able to find them. This keeps the live
//! default chords byte-identical to the hardcoded TUI `match` so a user with no
//! `keybindings.json` sees zero behavior change.

use super::types::KeybindingBlock;
use indexmap::IndexMap;

/// Image-paste key for the canonical (non-Windows) build.
/// 1:1 with `IMAGE_PASTE_KEY` (defaultBindings.ts:15) on the non-Windows branch.
const IMAGE_PASTE_KEY: &str = "ctrl+v";

/// Mode-cycle key for the canonical (VT-capable) build.
/// 1:1 with `MODE_CYCLE_KEY` (defaultBindings.ts:30) on the VT-mode branch.
const MODE_CYCLE_KEY: &str = "shift+tab";

/// Build one block from an ordered `(key, action)` slice.
fn block(context: &str, pairs: &[(&str, &str)]) -> KeybindingBlock {
    let mut bindings = IndexMap::with_capacity(pairs.len());
    for (k, v) in pairs {
        bindings.insert((*k).to_string(), Some((*v).to_string()));
    }
    KeybindingBlock {
        context: context.to_string(),
        bindings,
    }
}

/// The canonical default keybindings.
/// 1:1 with `DEFAULT_BINDINGS` (defaultBindings.ts:32-340), flags off, non-Windows.
// A flat data table mirroring the TS literal — long by nature, not branchy.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn default_bindings() -> Vec<KeybindingBlock> {
    vec![
        block(
            "Global",
            &[
                // ctrl+c / ctrl+d are defined so the resolver finds them, but
                // are NON-rebindable (reservedShortcuts) — special double-press.
                ("ctrl+c", "app:interrupt"),
                ("ctrl+d", "app:exit"),
                ("ctrl+l", "app:redraw"),
                ("ctrl+t", "app:toggleTodos"),
                ("ctrl+o", "app:toggleTranscript"),
                // KAIROS row omitted (flag off).
                ("ctrl+shift+o", "app:toggleTeammatePreview"),
                ("ctrl+r", "history:search"),
                // QUICK_SEARCH + TERMINAL_PANEL rows omitted (flags off).
            ],
        ),
        block(
            "Chat",
            &[
                ("escape", "chat:cancel"),
                ("ctrl+x ctrl+k", "chat:killAgents"),
                (MODE_CYCLE_KEY, "chat:cycleMode"),
                ("meta+p", "chat:modelPicker"),
                ("meta+o", "chat:fastMode"),
                ("meta+t", "chat:thinkingToggle"),
                ("enter", "chat:submit"),
                ("up", "history:previous"),
                ("down", "history:next"),
                ("ctrl+_", "chat:undo"),
                ("ctrl+shift+-", "chat:undo"),
                ("ctrl+x ctrl+e", "chat:externalEditor"),
                ("ctrl+g", "chat:externalEditor"),
                ("ctrl+s", "chat:stash"),
                (IMAGE_PASTE_KEY, "chat:imagePaste"),
                // MESSAGE_ACTIONS + VOICE_MODE rows omitted (flags off).
            ],
        ),
        block(
            "Autocomplete",
            &[
                ("tab", "autocomplete:accept"),
                ("escape", "autocomplete:dismiss"),
                ("up", "autocomplete:previous"),
                ("down", "autocomplete:next"),
            ],
        ),
        block(
            "Settings",
            &[
                ("escape", "confirm:no"),
                ("up", "select:previous"),
                ("down", "select:next"),
                ("k", "select:previous"),
                ("j", "select:next"),
                ("ctrl+p", "select:previous"),
                ("ctrl+n", "select:next"),
                ("space", "select:accept"),
                ("enter", "settings:close"),
                ("/", "settings:search"),
                ("r", "settings:retry"),
            ],
        ),
        block(
            "Confirmation",
            &[
                ("y", "confirm:yes"),
                ("n", "confirm:no"),
                ("enter", "confirm:yes"),
                ("escape", "confirm:no"),
                ("up", "confirm:previous"),
                ("down", "confirm:next"),
                ("tab", "confirm:nextField"),
                ("space", "confirm:toggle"),
                ("shift+tab", "confirm:cycleMode"),
                ("ctrl+e", "confirm:toggleExplanation"),
                ("ctrl+d", "permission:toggleDebug"),
            ],
        ),
        block(
            "Tabs",
            &[
                ("tab", "tabs:next"),
                ("shift+tab", "tabs:previous"),
                ("right", "tabs:next"),
                ("left", "tabs:previous"),
            ],
        ),
        block(
            "Transcript",
            &[
                ("ctrl+e", "transcript:toggleShowAll"),
                ("ctrl+c", "transcript:exit"),
                ("escape", "transcript:exit"),
                ("q", "transcript:exit"),
            ],
        ),
        block(
            "HistorySearch",
            &[
                ("ctrl+r", "historySearch:next"),
                ("escape", "historySearch:accept"),
                ("tab", "historySearch:accept"),
                ("ctrl+c", "historySearch:cancel"),
                ("enter", "historySearch:execute"),
            ],
        ),
        block("Task", &[("ctrl+b", "task:background")]),
        block(
            "ThemePicker",
            &[("ctrl+t", "theme:toggleSyntaxHighlighting")],
        ),
        block(
            "Scroll",
            &[
                ("pageup", "scroll:pageUp"),
                ("pagedown", "scroll:pageDown"),
                ("wheelup", "scroll:lineUp"),
                ("wheeldown", "scroll:lineDown"),
                ("ctrl+home", "scroll:top"),
                ("ctrl+end", "scroll:bottom"),
                ("ctrl+shift+c", "selection:copy"),
                ("cmd+c", "selection:copy"),
            ],
        ),
        block("Help", &[("escape", "help:dismiss")]),
        block(
            "Attachments",
            &[
                ("right", "attachments:next"),
                ("left", "attachments:previous"),
                ("backspace", "attachments:remove"),
                ("delete", "attachments:remove"),
                ("down", "attachments:exit"),
                ("escape", "attachments:exit"),
            ],
        ),
        block(
            "Footer",
            &[
                ("up", "footer:up"),
                ("ctrl+p", "footer:up"),
                ("down", "footer:down"),
                ("ctrl+n", "footer:down"),
                ("right", "footer:next"),
                ("left", "footer:previous"),
                ("enter", "footer:openSelected"),
                ("escape", "footer:clearSelection"),
            ],
        ),
        block(
            "MessageSelector",
            &[
                ("up", "messageSelector:up"),
                ("down", "messageSelector:down"),
                ("k", "messageSelector:up"),
                ("j", "messageSelector:down"),
                ("ctrl+p", "messageSelector:up"),
                ("ctrl+n", "messageSelector:down"),
                ("ctrl+up", "messageSelector:top"),
                ("shift+up", "messageSelector:top"),
                ("meta+up", "messageSelector:top"),
                ("shift+k", "messageSelector:top"),
                ("ctrl+down", "messageSelector:bottom"),
                ("shift+down", "messageSelector:bottom"),
                ("meta+down", "messageSelector:bottom"),
                ("shift+j", "messageSelector:bottom"),
                ("enter", "messageSelector:select"),
            ],
        ),
        // MessageActions block omitted (MESSAGE_ACTIONS flag off).
        block(
            "DiffDialog",
            &[
                ("escape", "diff:dismiss"),
                ("left", "diff:previousSource"),
                ("right", "diff:nextSource"),
                ("up", "diff:previousFile"),
                ("down", "diff:nextFile"),
                ("enter", "diff:viewDetails"),
            ],
        ),
        block(
            "ModelPicker",
            &[
                ("left", "modelPicker:decreaseEffort"),
                ("right", "modelPicker:increaseEffort"),
            ],
        ),
        block(
            "Select",
            &[
                ("up", "select:previous"),
                ("down", "select:next"),
                ("j", "select:next"),
                ("k", "select:previous"),
                ("ctrl+n", "select:next"),
                ("ctrl+p", "select:previous"),
                ("enter", "select:accept"),
                ("escape", "select:cancel"),
            ],
        ),
        block(
            "Plugin",
            &[("space", "plugin:toggle"), ("i", "plugin:install")],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_block_has_reserved_rows() {
        let blocks = default_bindings();
        let global = blocks.iter().find(|b| b.context == "Global").unwrap();
        // Reserved (non-rebindable) rows are PRESENT here (unlike the template).
        assert_eq!(
            global.bindings.get("ctrl+c"),
            Some(&Some("app:interrupt".to_string()))
        );
        assert_eq!(
            global.bindings.get("ctrl+d"),
            Some(&Some("app:exit".to_string()))
        );
    }

    #[test]
    fn canonical_keys_present() {
        let blocks = default_bindings();
        let chat = blocks.iter().find(|b| b.context == "Chat").unwrap();
        assert_eq!(
            chat.bindings.get("ctrl+v"),
            Some(&Some("chat:imagePaste".to_string()))
        );
        assert_eq!(
            chat.bindings.get("shift+tab"),
            Some(&Some("chat:cycleMode".to_string()))
        );
    }
}
