//! The `/fewer-permission-prompts` bundled skill — a byte-faithful port of
//! Claude Code 2.1.205's inline-body bundled skill (name-var literal, body-var
//! `Ynb`). Default-on, `userInvocable:!0`, `requires:{workspace:!0}`.
//!
//! Its `getPromptForCommand` uses a DISTINCT append header —
//! `## Additional instructions from the user` (contrast /verify's `## User
//! Request`) — and the body is inline (no YAML frontmatter, used verbatim):
//!
//! ```js
//! async getPromptForCommand(e){ let t=Ynb();
//!   if(e) t+=`\n\n## Additional instructions from the user\n\n${e}`;
//!   return [{type:"text", text:t}] }
//! ```
//!
//! Branding: `.claude/` → `.lingxi/` and "Claude Code" → "LingXi" (body and
//! description).

use crate::BundledPromptFn;

/// The inline skill body (`Ynb`), verbatim from the 2.1.205 binary with the
/// branding substitutions above. Inline ⇒ no frontmatter strip, no `trimStart`.
const FEWER_PERMISSION_PROMPTS_BODY: &str = include_str!("fewer_permission_prompts_body.md");

/// The skill's `description`, verbatim (with `.claude/settings.json` rebranded).
pub(crate) const FEWER_PERMISSION_PROMPTS_DESCRIPTION: &str = "Scan your transcripts for common read-only Bash and MCP tool calls, then add a prioritized allowlist to project .lingxi/settings.json to reduce permission prompts.";

/// Dynamic prompt builder for `/fewer-permission-prompts`.
pub struct FewerPermissionPromptsPromptFn;

impl BundledPromptFn for FewerPermissionPromptsPromptFn {
    fn build(&self, args: &str) -> String {
        // Reference: `t=Ynb(); if(e) t+="\n\n## Additional instructions from the
        // user\n\n"+e`. Note the header differs from the file-based skills'
        // `## User Request`, and the reference tests raw truthiness (not trim).
        if args.is_empty() {
            FEWER_PERMISSION_PROMPTS_BODY.to_string()
        } else {
            format!("{FEWER_PERMISSION_PROMPTS_BODY}\n\n## Additional instructions from the user\n\n{args}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_is_inline_and_branded() {
        assert!(FEWER_PERMISSION_PROMPTS_BODY.starts_with("# Fewer Permission Prompts"));
        assert!(!FEWER_PERMISSION_PROMPTS_BODY.contains(".claude/"));
        assert!(!FEWER_PERMISSION_PROMPTS_BODY.contains("Claude Code"));
        assert!(FEWER_PERMISSION_PROMPTS_BODY.contains(".lingxi/settings.json"));
    }

    #[test]
    fn build_appends_additional_instructions_header() {
        assert_eq!(
            FewerPermissionPromptsPromptFn.build(""),
            FEWER_PERMISSION_PROMPTS_BODY
        );
        let out = FewerPermissionPromptsPromptFn.build("prefer git read-only");
        assert_eq!(
            out,
            format!("{FEWER_PERMISSION_PROMPTS_BODY}\n\n## Additional instructions from the user\n\nprefer git read-only")
        );
    }
}
