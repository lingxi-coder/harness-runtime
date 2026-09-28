//! The `/verify` bundled skill — a byte-faithful port of Claude Code 2.1.205's
//! bundled `verify` skill, extracted from the 2.1.205 binary (`SKILL_MD`
//! template literal for name-var `Mee="verify"`). The only branding
//! substitution is `.claude/` → `.lingxi/` (LingXi's intentional config-dir
//! rebrand); there is no "Claude Code" product prose in the body.
//!
//! Registered as a [`SlashCommandKind::Bundled`] so it surfaces in the
//! completion popup and dispatches through the registry exactly like `/loop`.
//!
//! The prompt mirrors the reference `getPromptForCommand`
//! (`Lu({… getPromptForCommand(e){ let r=[$g(SKILL_MD).content.trimStart()];
//! if(e) r.push(\`## User Request\n\n${e}\`); return [{type:"text",
//! text:r.join("\n\n")}] }})`): the frontmatter-stripped, `trimStart`ed body,
//! plus a `## User Request` block appended when invoked with an argument.
//!
//! [`SlashCommandKind::Bundled`]: crate::SlashCommandKind::Bundled

use crate::BundledPromptFn;

/// The frontmatter-stripped skill body (`$g(SKILL_MD).content.trimStart()`),
/// verbatim from the 2.1.205 binary with `.claude/` rebranded to `.lingxi/`.
/// Held as an asset so this module stays readable; `include_str!` embeds the
/// exact bytes (trailing newline preserved, matching the binary).
const VERIFY_BODY: &str = include_str!("verify_body.md");

/// The skill's frontmatter `description`, verbatim from the binary — the
/// user-facing popup description and the model-facing when-to-invoke guidance.
pub(crate) const VERIFY_DESCRIPTION: &str = "Verify that a code change actually does what it's supposed to by exercising it end-to-end and observing behavior — drive the affected flow, not just tests or typecheck. Run before committing nontrivial changes; bootstraps this repo's project verify skill if none exists yet. Don't invoke it on a diff that only touches tests, docs, or other code with no runtime surface to drive (a change to product source always has one) — there's nothing to observe.";

/// Dynamic prompt builder for `/verify` (reference `getPromptForCommand`).
pub struct VerifyPromptFn;

impl BundledPromptFn for VerifyPromptFn {
    fn build(&self, args: &str) -> String {
        // Reference: `r=[content.trimStart()]; if(e) r.push("## User Request\n\n"+e);
        // r.join("\n\n")`. `content` is pre-`trimStart`ed at build time
        // (`VERIFY_BODY`). A non-empty argument appends the user-request block
        // (joined with the reference's unconditional `\n\n`); an empty one
        // returns the body unchanged (a 1-element join is the element itself).
        if args.is_empty() {
            VERIFY_BODY.to_string()
        } else {
            format!("{VERIFY_BODY}\n\n## User Request\n\n{args}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_is_the_frontmatter_stripped_binary_content() {
        // The asset is the `SKILL_MD` body after `trimStart` — it opens with the
        // first line of prose (no leading `---` frontmatter, no leading blank)
        // and preserves the binary's trailing newline.
        assert!(VERIFY_BODY.starts_with("**Verification is runtime observation.**"));
        assert!(VERIFY_BODY.ends_with("don't interpret.\n"));
        // Branding: `.claude/` was rebranded; no stray original path remains.
        assert!(!VERIFY_BODY.contains(".claude/"));
        assert!(VERIFY_BODY.contains(".lingxi/skills/"));
    }

    #[test]
    fn build_empty_arg_returns_body_verbatim() {
        // Reference: a 1-element `[content].join("\n\n")` is the element itself.
        assert_eq!(VerifyPromptFn.build(""), VERIFY_BODY);
    }

    #[test]
    fn build_with_arg_appends_user_request_block() {
        // Reference: `[content, "## User Request\n\n"+e].join("\n\n")` — the join
        // inserts `\n\n` regardless of the body's own trailing newline.
        let out = VerifyPromptFn.build("does the login flow still work?");
        assert_eq!(
            out,
            format!("{VERIFY_BODY}\n\n## User Request\n\ndoes the login flow still work?")
        );
        // Concretely: body ends "…\n", then the join's "\n\n", so three newlines
        // precede the heading.
        assert!(out.contains("don't interpret.\n\n\n## User Request\n\ndoes the login"));
    }
}
