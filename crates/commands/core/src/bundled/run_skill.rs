//! The `/run` bundled skill — a byte-faithful port of Claude Code 2.1.205's
//! bundled `run` skill (name-var in the binary's skill table). Like
//! [`super::verify_skill`] it is a static, self-contained SKILL.md; the only
//! branding substitution is `.claude/` → `.lingxi/`.
//!
//! Prompt = the frontmatter-stripped, `trimStart`ed body plus a `## User
//! Request` block appended when invoked with an argument, mirroring the
//! reference `getPromptForCommand` (see [`super::verify_skill`] for the shape).

use command_api::BundledPromptFn;

/// The frontmatter-stripped skill body (`$g(SKILL_MD).content.trimStart()`),
/// verbatim from the 2.1.205 binary with `.claude/` rebranded to `.lingxi/`.
const RUN_BODY: &str = include_str!("run_body.md");

/// The skill's frontmatter `description`, verbatim from the binary.
pub(crate) const RUN_DESCRIPTION: &str = "Launch and drive this project's app to see a change working. Use when asked to run, start, or screenshot the app, or to confirm a change works in the real app (not just tests). First looks for a project skill that already covers launching the app; otherwise falls back to built-in patterns per project type (CLI, server, TUI, Electron, browser-driven, library).";

/// Dynamic prompt builder for `/run` (reference `getPromptForCommand`).
pub struct RunPromptFn;

impl BundledPromptFn for RunPromptFn {
    fn build(&self, args: &str) -> String {
        if args.is_empty() {
            RUN_BODY.to_string()
        } else {
            format!("{RUN_BODY}\n\n## User Request\n\n{args}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_is_frontmatter_stripped_and_branded() {
        assert!(RUN_BODY.starts_with("**Running means launching the actual app"));
        assert!(RUN_BODY.ends_with("If it just worked, don't.\n"));
        assert!(!RUN_BODY.contains(".claude/"));
        assert!(RUN_BODY.contains(".lingxi/skills/"));
    }

    #[test]
    fn build_empty_returns_body_and_arg_appends_user_request() {
        assert_eq!(RunPromptFn.build(""), RUN_BODY);
        let out = RunPromptFn.build("start the dev server");
        assert_eq!(
            out,
            format!("{RUN_BODY}\n\n## User Request\n\nstart the dev server")
        );
    }
}
