//! The `/keybindings-help` bundled skill — port of Claude Code 2.1.267's
//! `$o()` registrar (`src_172124278.js` @173382).
//!
//! ```js
//! async getPromptForCommand(e){
//!   let o=as(), n=ls(), s=hs(),
//!       d=[ys,ws,bs,vs,ks,Cs,_s,Es,
//!          `## Reserved Shortcuts\n\n${s}`,
//!          `## Available Contexts\n\n${o}`,
//!          `## Available Actions\n\n${n}`];
//!   if(e) d.push(`## User Request\n\n${e}`);
//!   return [{type:"text",text:d.join("\n\n")}] }
//! ```
//!
//! The three trailing sections are generated from the LIVE keybinding tables,
//! not transcribed: contexts and their descriptions, every bindable action with
//! the default keys currently bound to it, and the reserved-shortcut list. A
//! transcribed copy would be a second list free to drift from the validator that
//! actually rejects bindings.
//!
//! `userInvocable: false` — model-invocable only, so it never appears in the
//! slash menu; the user route is `/keybindings`.

use crate::keybindings::default_bindings::default_bindings;
use crate::keybindings::reserved::{Severity, MACOS_RESERVED, NON_REBINDABLE, TERMINAL_RESERVED};
use crate::keybindings::schema::{
    KEYBINDING_ACTIONS, KEYBINDING_CONTEXTS, KEYBINDING_CONTEXT_DESCRIPTIONS,
};
use command_api::BundledPromptFn;
use std::collections::BTreeMap;

/// The eight static sections, joined (`ys` … `Es`).
const KEYBINDINGS_HELP_BODY: &str = include_str!("keybindings_help_body.md");

pub(crate) const KEYBINDINGS_HELP_DESCRIPTION: &str = "Use when the user wants to customize keyboard shortcuts, rebind keys, add chord bindings, or modify ~/.lingxi/keybindings.json. Examples: \"rebind ctrl+s\", \"add a chord shortcut\", \"change the submit key\", \"customize keybindings\".";

/// `ot(headers, rows)` — a markdown table.
fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut out = vec![
        format!("| {} |", headers.join(" | ")),
        format!(
            "| {} |",
            headers
                .iter()
                .map(|_| "---")
                .collect::<Vec<_>>()
                .join(" | ")
        ),
    ];
    out.extend(rows.iter().map(|r| format!("| {} |", r.join(" | "))));
    out.join("\n")
}

/// `as()` — every context with its description.
fn contexts_table() -> String {
    let descriptions: BTreeMap<&str, &str> =
        KEYBINDING_CONTEXT_DESCRIPTIONS.iter().copied().collect();
    let rows: Vec<Vec<String>> = KEYBINDING_CONTEXTS
        .iter()
        .map(|context| {
            vec![
                format!("`{context}`"),
                (*descriptions.get(context).unwrap_or(&"")).to_string(),
            ]
        })
        .collect();
    table(&["Context", "Description"], &rows)
}

/// `ls()` — every action with the default keys currently bound to it.
///
/// The key column is built by INVERTING the live default table, so an action
/// whose default moved shows its new key here without anyone editing prose.
fn actions_table() -> String {
    let mut by_action: BTreeMap<String, (Vec<String>, String)> = BTreeMap::new();
    for block in default_bindings() {
        for (key, action) in &block.bindings {
            let Some(action) = action else { continue };
            let entry = by_action
                .entry(action.clone())
                .or_insert_with(|| (Vec::new(), block.context.clone()));
            entry.0.push(key.clone());
        }
    }
    let rows: Vec<Vec<String>> = KEYBINDING_ACTIONS
        .iter()
        .map(|action| match by_action.get(*action) {
            Some((keys, context)) => vec![
                format!("`{action}`"),
                keys.iter()
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                context.clone(),
            ],
            None => vec![format!("`{action}`"), "(none)".to_string(), String::new()],
        })
        .collect();
    table(&["Action", "Default Key(s)", "Context"], &rows)
}

/// `hs()` — the reserved-shortcut list, in upstream's three subsections.
fn reserved_shortcuts() -> String {
    let mut out = vec!["### Non-rebindable (errors)".to_string()];
    for shortcut in NON_REBINDABLE {
        out.push(format!("- `{}` — {}", shortcut.key, shortcut.reason));
    }
    out.push(String::new());
    out.push("### Terminal reserved (errors/warnings)".to_string());
    for shortcut in TERMINAL_RESERVED {
        let effect = if shortcut.severity == Severity::Error {
            "will not work"
        } else {
            "may conflict"
        };
        out.push(format!(
            "- `{}` — {} ({effect})",
            shortcut.key, shortcut.reason
        ));
    }
    out.push(String::new());
    out.push("### macOS reserved (errors)".to_string());
    for shortcut in MACOS_RESERVED {
        out.push(format!("- `{}` — {}", shortcut.key, shortcut.reason));
    }
    out.join("\n")
}

/// Dynamic prompt builder for `keybindings-help`.
pub struct KeybindingsHelpPromptFn;

impl BundledPromptFn for KeybindingsHelpPromptFn {
    fn build(&self, args: &str) -> String {
        let mut sections = vec![
            KEYBINDINGS_HELP_BODY.to_string(),
            format!("## Reserved Shortcuts\n\n{}", reserved_shortcuts()),
            format!("## Available Contexts\n\n{}", contexts_table()),
            format!("## Available Actions\n\n{}", actions_table()),
        ];
        if !args.is_empty() {
            sections.push(format!("## User Request\n\n{args}"));
        }
        sections.join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_is_branded() {
        assert!(!KEYBINDINGS_HELP_BODY.contains("Claude Code"));
        assert!(!KEYBINDINGS_HELP_BODY.contains("~/.claude/"));
        assert!(KEYBINDINGS_HELP_BODY.contains("~/.lingxi/keybindings.json"));
        assert!(!KEYBINDINGS_HELP_DESCRIPTION.contains("~/.claude/"));
    }

    /// The three trailing sections come from the live tables. If they were
    /// transcribed, a context added to the validator would not appear here.
    #[test]
    fn the_tables_are_generated_from_the_live_keybinding_data() {
        let out = KeybindingsHelpPromptFn.build("");
        for context in KEYBINDING_CONTEXTS {
            assert!(
                out.contains(&format!("`{context}`")),
                "context {context} must be listed"
            );
        }
        for action in KEYBINDING_ACTIONS {
            assert!(
                out.contains(&format!("`{action}`")),
                "action {action} must be listed"
            );
        }
        for shortcut in NON_REBINDABLE {
            assert!(
                out.contains(shortcut.reason),
                "reserved {} must be listed",
                shortcut.key
            );
        }
    }

    /// A bound action shows its real default key, not `(none)` — the inversion
    /// of the default table is what makes the column true.
    #[test]
    fn a_bound_action_reports_its_default_key() {
        let table = actions_table();
        let row = table
            .lines()
            .find(|l| l.contains("`app:toggleTodos`"))
            .expect("app:toggleTodos is a real action");
        // Inspect the KEY CELL. Asserting the row merely "contains a backtick"
        // passes on the action name alone, so an inverted table that lost every
        // key would still look right.
        let key_cell = row
            .split('|')
            .nth(2)
            .expect("| action | keys | context |")
            .trim();
        assert!(
            !key_cell.is_empty() && key_cell != "(none)",
            "a defaulted action must show its real key, got {key_cell:?} in {row}"
        );
        assert!(
            key_cell.starts_with('`'),
            "keys are rendered in backticks: {key_cell:?}"
        );
    }

    #[test]
    fn a_request_is_appended_last() {
        let out = KeybindingsHelpPromptFn.build("rebind ctrl+s");
        assert!(out.ends_with("\n\n## User Request\n\nrebind ctrl+s"));
        assert!(!KeybindingsHelpPromptFn
            .build("")
            .contains("## User Request"));
    }

    #[test]
    fn the_section_order_matches_upstream() {
        let out = KeybindingsHelpPromptFn.build("");
        let reserved = out.find("## Reserved Shortcuts").expect("reserved");
        let contexts = out.find("## Available Contexts").expect("contexts");
        let actions = out.find("## Available Actions").expect("actions");
        assert!(reserved < contexts && contexts < actions);
    }
}
