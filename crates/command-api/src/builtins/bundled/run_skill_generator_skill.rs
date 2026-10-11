//! The `/run-skill-generator` bundled skill — a byte-faithful port of Claude
//! Code 2.1.205's file-based bundled skill (registrar `hab`, `SKILL_MD` body-var
//! `gYp`, description-var `mab`).
//!
//! Same file-based prompt shape as [`super::verify_skill`] / [`super::run_skill`]
//! (frontmatter-stripped, `trimStart`ed body + `## User Request` on arg), but
//! registered `disableModelInvocation:!0` — a USER-only slash command the model
//! may not invoke.
//!
//! Branding: `.claude/` → `.lingxi/` and the one "Claude Code" (product) →
//! "LingXi"; standalone "Claude" (the agent) is kept, matching LingXi's
//! convention elsewhere (`init_verifiers`).

use crate::model::{source_concat, BundledPromptFn, PromptPart};

/// The frontmatter-stripped skill body (`$g(SKILL_MD).content.trimStart()`),
/// verbatim from the 2.1.205 binary with the branding substitutions above.
const RUN_SKILL_GENERATOR_BODY: &str = include_str!("run_skill_generator_body.md");

/// The skill's `description` (binary var `mab`), verbatim.
pub(crate) const RUN_SKILL_GENERATOR_DESCRIPTION: &str = "Author or improve the run-<unit> skill — a per-project skill that tells agents how to build, launch, and drive this project's app. Use when the user asks to set up the project, get it running, write run instructions, or verify build/run steps work from a clean environment.";

/// Dynamic prompt builder for `/run-skill-generator` (reference
/// `getPromptForCommand`, identical to [`super::verify_skill`]).
pub struct RunSkillGeneratorPromptFn;

impl BundledPromptFn for RunSkillGeneratorPromptFn {
    fn build(
        &self,
        args: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        if args.string_units("").is_some_and(|units| units.is_empty()) {
            RUN_SKILL_GENERATOR_BODY.into()
        } else {
            let prefix = format!("{RUN_SKILL_GENERATOR_BODY}\n\n## User Request\n\n");
            source_concat([PromptPart::Text(&prefix), PromptPart::Source(args)])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_is_frontmatter_stripped_and_branded() {
        assert!(RUN_SKILL_GENERATOR_BODY.starts_with("Your job is to produce a **skill**"));
        // Branding applied.
        assert!(!RUN_SKILL_GENERATOR_BODY.contains(".claude/"));
        assert!(!RUN_SKILL_GENERATOR_BODY.contains("Claude Code"));
        assert!(RUN_SKILL_GENERATOR_BODY.contains(".lingxi/skills/"));
        assert!(RUN_SKILL_GENERATOR_BODY.contains("LingXi **natively discovers**"));
        // The agent name "Claude" is preserved (not a product reference).
        assert!(RUN_SKILL_GENERATOR_BODY.contains("lets Claude auto-load it"));
    }

    #[test]
    fn build_empty_returns_body_and_arg_appends_user_request() {
        assert_eq!(
            RunSkillGeneratorPromptFn.build(&"".into()).string_at(""),
            Some(RUN_SKILL_GENERATOR_BODY)
        );
        let expected =
            format!("{RUN_SKILL_GENERATOR_BODY}\n\n## User Request\n\nfor the billing app");
        assert_eq!(
            RunSkillGeneratorPromptFn
                .build(&"for the billing app".into())
                .string_at(""),
            Some(expected.as_str())
        );
    }
}
