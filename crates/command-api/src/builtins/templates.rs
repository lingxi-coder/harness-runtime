//! Byte-locked text templates for `/init` and other commands.
//!
//! See plan M5-10 Task 0 step 1 for the source of [`OLD_INIT_PROMPT`].

/// 21-line markdown template that `/init` injects as the next user message.
///
/// Byte-locked from `claude-code/src/commands/init.ts:6-26` (the
/// `OLD_INIT_PROMPT` constant) — the legacy template used when the
/// `NEW_INIT` feature flag is off (which is the v0.6.0 default). When
/// `claude-code` upgrades and the template drifts, a future `LingXi`
/// release may need to refresh this constant; for v0.6.0 the M5 baseline
/// is frozen.
///
/// Backtick-fence handling: the original TS literal uses backslash-escaped
/// backticks for the embedded fenced code block. In Rust string literals
/// backticks are not special, so they appear unescaped here.
pub const OLD_INIT_PROMPT: &str = "Please analyze this codebase and create a LINGXI.md file, which will be given to future instances of LingXi to operate in this repository.

What to add:
1. Commands that will be commonly used, such as how to build, lint, and run tests. Include the necessary commands to develop in this codebase, such as how to run a single test.
2. High-level code architecture and structure so that future instances can be productive more quickly. Focus on the \"big picture\" architecture that requires reading multiple files to understand.

Usage notes:
- If there's already a LINGXI.md, suggest improvements to it.
- When you make the initial LINGXI.md, do not repeat yourself and do not include obvious instructions like \"Provide helpful error messages to users\", \"Write unit tests for all new utilities\", \"Never include sensitive information (API keys, tokens) in code or commits\".
- Avoid listing every component or file structure that can be easily discovered.
- Don't include generic development practices.
- If there are Cursor rules (in .cursor/rules/ or .cursorrules) or Copilot rules (in .github/copilot-instructions.md), make sure to include the important parts.
- If there is a README.md, make sure to include the important parts.
- Do not make up information such as \"Common Development Tasks\", \"Tips for Development\", \"Support and Documentation\" unless this is expressly included in other files that you read.
- Be sure to prefix the file with the following text:

```
# LINGXI.md

This file provides guidance to LingXi (claude.ai/code) when working with code in this repository.
```";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_init_prompt_starts_with_locked_first_line() {
        assert!(OLD_INIT_PROMPT.starts_with(
            "Please analyze this codebase and create a LINGXI.md file, \
             which will be given to future instances of LingXi \
             to operate in this repository."
                .replace("             ", "")
                .as_str()
        ) || OLD_INIT_PROMPT.starts_with(
            "Please analyze this codebase and create a LINGXI.md file, which will be given to future instances of LingXi to operate in this repository."
        ));
    }

    #[test]
    fn old_init_prompt_contains_lingxi_md_prefix_block() {
        assert!(OLD_INIT_PROMPT.contains("# LINGXI.md"));
        assert!(OLD_INIT_PROMPT.contains(
            "This file provides guidance to LingXi (claude.ai/code) \
             when working with code in this repository."
                .replace("             ", "")
                .as_str()
        ) || OLD_INIT_PROMPT.contains(
            "This file provides guidance to LingXi (claude.ai/code) when working with code in this repository."
        ));
    }

    #[test]
    fn old_init_prompt_line_count_locked() {
        // Counted from the TS source: lines 6..26 (inclusive) of init.ts.
        // The TS template literal uses literal newlines, so each source
        // line in 6..26 becomes one `\n` in the Rust string. There are
        // 21 source lines + final closing backtick line (no trailing
        // newline) so `.lines().count()` reports 21.
        let n = OLD_INIT_PROMPT.lines().count();
        assert_eq!(n, 21, "/init template line count drifted: {n}");
    }

    #[test]
    fn old_init_prompt_no_trailing_blank_line() {
        assert!(!OLD_INIT_PROMPT.ends_with("\n\n"));
    }

    #[test]
    fn old_init_prompt_ends_with_closing_fence() {
        assert!(
            OLD_INIT_PROMPT.ends_with("```"),
            "/init template should end with closing triple-backtick"
        );
    }

    #[test]
    fn old_init_prompt_byte_length_locked() {
        // Byte-locked length. If this drifts, refresh `parity_init_template.json`
        // alongside this constant. Locked 2026-05-28 (M5-10 T8 first-green).
        let n = OLD_INIT_PROMPT.len();
        assert_eq!(n, 1582, "/init template byte length drifted: {n}");
    }

    #[test]
    fn old_init_prompt_sha256_locked() {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(OLD_INIT_PROMPT.as_bytes());
        let digest = format!("{:x}", hasher.finalize());
        // Locked 2026-05-28 (M5-10 T8 first-green) against the
        // byte-frozen OLD_INIT_PROMPT. To refresh:
        // 1. Update the constant body.
        // 2. Run this test once; copy the actual digest from the failure.
        // 3. Paste below + into parity_init_template.json.
        assert_eq!(
            digest, "f379e92cdef57db5223a0af64d7bf3014b0c7ca4a5afa9f1deaf873635f2eb5a",
            "OLD_INIT_PROMPT byte-changed; expected hash drifted"
        );
    }
}
