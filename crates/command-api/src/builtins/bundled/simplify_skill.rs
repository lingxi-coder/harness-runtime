//! The `/simplify` bundled skill — a byte-faithful port of Claude Code
//! 2.1.205's inline-body bundled skill (registrar `eVp`, name-var
//! `cUr="simplify"`, body-var `Sob`).
//!
//! Unlike the file-based [`super::verify_skill`] / [`super::run_skill`], this is
//! an INLINE skill: its `getPromptForCommand` PREPENDS a `Review target:` line
//! rather than appending a `## User Request` block —
//!
//! ```js
//! async getPromptForCommand(e){ let t=e.trim();
//!   return [{type:"text", text:`${t?`Review target: \`${t}\`\n\n`:""}${Sob}`}] }
//! ```
//!
//! `Sob` is a template literal whose `${Hnr}` (Phase 0 preamble), `${fi}`
//! (the agent-launching tool name — `"Agent"`, which is also LingXi's agent
//! tool name, `tools::agent::AGENT_TOOL_NAME`), and `${Dnr}/${lst}/${cst}/${ust}`
//! (the four review angles, shared with `/code-review`) are all resolved into
//! `simplify_body.md`. No branding substitution is needed — the body has no
//! `.claude` paths and no "Claude Code" prose.

use crate::model::{source_concat, trim_source_text, BundledPromptFn, PromptPart};

/// The fully-resolved skill body (`Sob` with every `${…}` interpolation
/// substituted), verbatim from the 2.1.205 binary. `${fi}` resolved to the
/// `Agent` tool name, which is also LingXi's agent tool, so the reference is
/// correct as written.
const SIMPLIFY_BODY: &str = include_str!("simplify_body.md");

/// The skill's `description`, verbatim from the binary (`—` → `—`).
pub(crate) const SIMPLIFY_DESCRIPTION: &str = "Review the changed code for reuse, simplification, efficiency, and altitude cleanups, then apply the fixes. Quality only — it does not hunt for bugs; use /code-review for that.";

/// The skill's `argumentHint`, verbatim from the binary.
pub(crate) const SIMPLIFY_ARGUMENT_HINT: &str = "[<target>]";

/// Dynamic prompt builder for `/simplify` (reference `getPromptForCommand`).
pub struct SimplifyPromptFn;

impl BundledPromptFn for SimplifyPromptFn {
    fn build(
        &self,
        args: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        // Reference: `t=e.trim(); `${t?`Review target: \`${t}\`\n\n`:""}${Sob}``
        // — a non-empty (post-trim) argument PREPENDS a `Review target:` line;
        // an empty one returns the body unchanged.
        let target = trim_source_text(args);
        if target
            .string_units("")
            .is_some_and(|units| units.is_empty())
        {
            SIMPLIFY_BODY.into()
        } else {
            source_concat([
                PromptPart::Text("Review target: `"),
                PromptPart::Source(&target),
                PromptPart::Text("`\n\n"),
                PromptPart::Text(SIMPLIFY_BODY),
            ])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_is_the_resolved_inline_template() {
        // Opens with the literal backtick-quoted tagline (an inline-code span in
        // the markdown, not the template delimiter) and every interpolation is
        // resolved — no `${` remains, and the shared `Agent` tool name is inlined.
        assert!(SIMPLIFY_BODY
            .starts_with("`/simplify → 4 cleanup agents in parallel → apply the fixes`"));
        assert!(!SIMPLIFY_BODY.contains("${"));
        assert!(SIMPLIFY_BODY.contains("via the Agent tool"));
        // The four angles are present.
        for h in [
            "### Reuse",
            "### Simplification",
            "### Efficiency",
            "### Altitude",
        ] {
            assert!(SIMPLIFY_BODY.contains(h), "missing angle {h}");
        }
    }

    #[test]
    fn build_empty_arg_returns_body_verbatim() {
        assert_eq!(
            SimplifyPromptFn.build(&"".into()).string_at(""),
            Some(SIMPLIFY_BODY)
        );
        // A whitespace-only arg trims to empty ⇒ still just the body.
        assert_eq!(
            SimplifyPromptFn.build(&"   ".into()).string_at(""),
            Some(SIMPLIFY_BODY)
        );
    }

    #[test]
    fn build_with_target_prepends_review_target_line() {
        // Reference PREPENDS (contrast verify/run which append `## User Request`).
        let out = SimplifyPromptFn.build(&"pull/42".into());
        assert_eq!(
            out.string_at(""),
            Some(format!("Review target: `pull/42`\n\n{SIMPLIFY_BODY}").as_str())
        );
        // The arg is trimmed before wrapping.
        assert_eq!(SimplifyPromptFn.build(&"  pull/42  ".into()), out);
    }
}
