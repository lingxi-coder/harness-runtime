//! The `/batch` bundled skill — a byte-faithful port of Claude Code 2.1.205's
//! parallel-work orchestration skill (name-var literal `"batch"`, template
//! `_rb`). Plans a large mechanical change and executes it across 5–30 isolated
//! worktree agents that each open a PR.
//!
//! `userInvocable:!0`, `disableModelInvocation:!0` (user-only), `isEnabled` =
//! is-git-repo (a runtime check). The reference `getPromptForCommand`:
//!
//! ```js
//! getPromptForCommand(e){ let t=e.trim();
//!   if(!t) return [{text:Trb}];                 // usage
//!   if(!await BE()) return [{text:brb}];        // not a git repo → error
//!   return [{text:_rb(t)}]; }                    // else → the template with the instruction
//! ```
//!
//! The template's interpolations all resolve to tools LingXi has: `${fi}`=Agent,
//! `${nme}`=EnterPlanMode, `${vm}`=AskUserQuestion, and (via `${yrb}`)
//! `${Xy}`=Skill; the unit-count bounds `${gWp}`=5 / `${yWp}`=30. No branding is
//! needed. `${e}` (the instruction) is substituted at build time.
//!
//! LingXi's pure `args -> String` [`BundledPromptFn`] has no git context, so the
//! `await BE()` not-a-git-repo guard (`brb`) can't run in `build`; the template
//! is emitted for any non-empty instruction (documented; the no-arg usage path
//! is exact).

use crate::model::{source_concat, trim_source_text, BundledPromptFn, PromptPart};

/// The orchestration template (`_rb`), fully resolved except the `${e}`
/// instruction placeholder, substituted in [`BatchPromptFn::build`].
const BATCH_TEMPLATE: &str = include_str!("batch_body.md");

/// No-argument usage message (binary var `Trb`).
const BATCH_USAGE: &str = "Provide an instruction describing the batch change you want to make.\n\nExamples:\n  /batch migrate from react to vue\n  /batch replace all uses of lodash with native equivalents\n  /batch add type annotations to all untyped function parameters";

/// The skill's `description`, verbatim from the binary.
pub(crate) const BATCH_DESCRIPTION: &str = "Research and plan a large-scale change, then execute it in parallel across 5–30 isolated worktree agents that each open a PR.";

/// The skill's `whenToUse`, verbatim from the binary.
pub(crate) const BATCH_WHEN_TO_USE: &str = "Use when the user wants to make a sweeping, mechanical change across many files (migrations, refactors, bulk renames) that can be decomposed into independent parallel units.";

/// The skill's `argumentHint`, verbatim from the binary.
pub(crate) const BATCH_ARGUMENT_HINT: &str = "<instruction>";

/// Dynamic prompt builder for `/batch` (reference `getPromptForCommand`).
pub struct BatchPromptFn;

impl BundledPromptFn for BatchPromptFn {
    fn build(
        &self,
        args: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        let target = trim_source_text(args);
        if target
            .string_units("")
            .is_some_and(|units| units.is_empty())
        {
            BATCH_USAGE.into()
        } else {
            let (prefix, suffix) = BATCH_TEMPLATE
                .split_once("${e}")
                .expect("batch template has one instruction interpolation");
            source_concat([
                PromptPart::Text(prefix),
                PromptPart::Source(&target),
                PromptPart::Text(suffix),
            ])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceTextTestExt;

    #[test]
    fn template_is_resolved_except_the_instruction() {
        // Every interpolation but the `${e}` instruction is pre-resolved.
        assert!(BATCH_TEMPLATE.contains("${e}"));
        for stray in [
            "${fi}", "${nme}", "${vm}", "${yrb}", "${gWp}", "${yWp}", "${Xy}",
        ] {
            assert!(!BATCH_TEMPLATE.contains(stray), "unresolved {stray}");
        }
        // Tool names resolved to LingXi's own.
        assert!(BATCH_TEMPLATE.contains("`Agent`") || BATCH_TEMPLATE.contains("Agent"));
        assert!(BATCH_TEMPLATE.contains("EnterPlanMode"));
        assert!(BATCH_TEMPLATE.contains("AskUserQuestion"));
        assert!(BATCH_TEMPLATE.contains("skill: \"code-review\""));
        // 5–30 unit bounds inlined.
        assert!(BATCH_TEMPLATE.contains('5') && BATCH_TEMPLATE.contains("30"));
    }

    #[test]
    fn empty_arg_returns_usage() {
        assert_eq!(
            BatchPromptFn.build(&"".into()).string_at(""),
            Some(BATCH_USAGE)
        );
        assert_eq!(
            BatchPromptFn.build(&"   ".into()).string_at(""),
            Some(BATCH_USAGE)
        );
        assert!(BatchPromptFn
            .build(&"".into())
            .starts_with("Provide an instruction"));
    }

    #[test]
    fn instruction_is_substituted_into_the_template() {
        let out = BatchPromptFn.build(&"migrate lodash to native".into());
        assert!(!out.contains("${e}"));
        assert!(out.contains("## User Instruction\n\nmigrate lodash to native"));
        assert!(out.starts_with("# Batch: Parallel Work Orchestration"));
    }
}
