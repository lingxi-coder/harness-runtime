//! The `/update-config` bundled skill — port of Claude Code 2.1.267's
//! `cn()` registrar (`src_172124278.js` @217693).
//!
//! Two prompt shapes, selected by the argument:
//!
//! ```js
//! async getPromptForCommand(e){
//!   if(e.startsWith("[hooks-only]")){ let d=e.slice(12).trim(), r=an+"\n\n"+ln;
//!     if(d) r+=`\n\n## Task\n\n${d}`; return [{type:"text",text:r}] }
//!   let o=Zle(zA(),{io:"input"}); BIt(o,!1);
//!   let n=b(o,null,2), s=Vs;
//!   s+=`\n\n## Full Settings JSON Schema\n\n\`\`\`json\n${n}\n\`\`\``;
//!   if(e) s+=`\n\n## User Request\n\n${e}`;
//!   return [{type:"text",text:s}] }
//! ```
//!
//! 🚨 The schema half is what makes this skill worth having: it hands the model
//! the REAL settings shape rather than a prose summary, so the skill cannot
//! describe a key the loader would reject. It is generated from the same
//! `SettingsJson` the loader parses (`schemars::schema_for!`), which is also how
//! the settings-merge test enumerates fields — one source, no second list to
//! drift.
//!
//! Branding: `Claude Code` → `LingXi`, `.claude/` → `.lingxi/`,
//! `claude --debug` → `lingxi-cli --debug` in all three bodies.

use command_api::BundledPromptFn;

/// The main body (`Vs`).
const UPDATE_CONFIG_BODY: &str = include_str!("update_config_body.md");
/// The hooks reference (`an`), used only by the `[hooks-only]` shape.
const HOOKS_REFERENCE: &str = include_str!("update_config_hooks_reference.md");
/// The hook-construction walkthrough (`ln`), likewise.
const HOOK_CONSTRUCTION: &str = include_str!("update_config_hook_construction.md");

/// The reference's `[hooks-only]` prefix, and the width `e.slice(12)` skips.
const HOOKS_ONLY_PREFIX: &str = "[hooks-only]";

pub(crate) const UPDATE_CONFIG_DESCRIPTION: &str = "Use this skill to configure the LingXi harness via settings.json. Automated behaviors (\"from now on when X\", \"each time X\", \"whenever X\", \"before/after X\") require hooks configured in settings.json - the harness executes these, not LingXi, so memory/preferences cannot fulfill them. Also use for: permissions (\"allow X\", \"add permission\", \"move permission to\"), env vars (\"set X=Y\"), hook troubleshooting, or any changes to settings.json/settings.local.json files. Examples: \"allow npm commands\", \"add bq permission to global settings\", \"move permission to user settings\", \"set DEBUG=true\", \"when LingXi stops show X\". For simple settings like theme/model, suggest the /config command.";

/// The live `settings.json` schema, pretty-printed.
///
/// Falls back to an empty object rather than failing the prompt: a skill that
/// cannot show the schema is still worth more than no skill.
fn settings_schema_json() -> String {
    serde_json::to_value(schemars::schema_for!(lingxi_core::settings::SettingsJson))
        .ok()
        .and_then(|schema| serde_json::to_string_pretty(&schema).ok())
        .unwrap_or_else(|| "{}".to_string())
}

/// Dynamic prompt builder for `/update-config`.
pub struct UpdateConfigPromptFn;

impl BundledPromptFn for UpdateConfigPromptFn {
    fn build(&self, args: &str) -> String {
        if let Some(rest) = args.strip_prefix(HOOKS_ONLY_PREFIX) {
            // The hooks-only shape skips the main body AND the schema — it is
            // the reference's narrow entry point for hook authoring.
            let task = rest.trim();
            let mut out = format!("{HOOKS_REFERENCE}\n\n{HOOK_CONSTRUCTION}");
            if !task.is_empty() {
                out.push_str(&format!("\n\n## Task\n\n{task}"));
            }
            return out;
        }
        let mut out = format!(
            "{UPDATE_CONFIG_BODY}\n\n## Full Settings JSON Schema\n\n```json\n{}\n```",
            settings_schema_json()
        );
        if !args.is_empty() {
            out.push_str(&format!("\n\n## User Request\n\n{args}"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bodies_are_branded() {
        for body in [UPDATE_CONFIG_BODY, HOOKS_REFERENCE, HOOK_CONSTRUCTION] {
            assert!(!body.contains("Claude Code"), "product name: {body:.60}");
            assert!(!body.contains(".claude/"), "config dir: {body:.60}");
            assert!(!body.contains("claude --debug"), "cli name: {body:.60}");
        }
        assert!(!UPDATE_CONFIG_DESCRIPTION.contains("Claude Code"));
    }

    /// The point of the skill: the model is handed the REAL settings shape.
    #[test]
    fn the_default_shape_carries_the_live_settings_schema() {
        let out = UpdateConfigPromptFn.build("");
        assert!(out.starts_with("# Update Config Skill"));
        assert!(out.contains("## Full Settings JSON Schema"));
        // A key that only exists because it is on the real `SettingsJson`.
        assert!(
            out.contains("permissions"),
            "the schema must come from the loader's own type"
        );
        assert!(
            !out.contains("## User Request"),
            "no args ⇒ no user-request section"
        );
    }

    #[test]
    fn a_request_is_appended_under_its_own_header() {
        let out = UpdateConfigPromptFn.build("allow npm commands");
        assert!(out.ends_with("\n\n## User Request\n\nallow npm commands"));
    }

    /// `[hooks-only]` is a DIFFERENT prompt: hooks reference + construction
    /// walkthrough, and deliberately no schema and no main body.
    #[test]
    fn the_hooks_only_shape_swaps_the_whole_prompt() {
        let out = UpdateConfigPromptFn.build("[hooks-only] run prettier after writes");
        assert!(out.starts_with("## Hooks Configuration"));
        assert!(out.contains("## Constructing a Hook"));
        assert!(
            !out.contains("## Full Settings JSON Schema"),
            "the hooks-only shape does not carry the schema"
        );
        assert!(out.ends_with("\n\n## Task\n\nrun prettier after writes"));
    }

    #[test]
    fn a_bare_hooks_only_marker_appends_no_task() {
        let out = UpdateConfigPromptFn.build("[hooks-only]");
        assert!(!out.contains("## Task"));
    }
}
