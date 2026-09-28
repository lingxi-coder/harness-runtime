//! The `/checkup` bundled skill — port of Claude Code 2.1.267's `No()`
//! registrar (`src_172124278.js`), whose body is `is()`.
//!
//! ```js
//! uo({ name:"doctor", aliases:["checkup"],
//!      isEnabled:()=>!a.DISABLE_DOCTOR_COMMAND,
//!      survivesBundledKillSwitch:!0, requires:{workspace:!0},
//!      terminalOriented:!0, userInvocable:!0, disableModelInvocation:!0,
//!      progressMessage:"running checkup",
//!      async getPromptForCommand(e){ let o=is();
//!        if(e) o+=`\n\n## Additional instructions from the user\n\n${e}`;
//!        return [{type:"text",text:o}] } })
//! ```
//!
//! ## Why this is `/checkup` and not `/doctor`
//!
//! Upstream registers it as `doctor` with `checkup` as an alias. This port
//! already ships a `/doctor` that is a DIFFERENT thing: a deterministic command
//! producing a structured `DoctorReport` DTO that the GUI clients render as a
//! screen (`client-adapter`'s `doctor_report_parity`). Upstream's is a
//! `terminalOriented` model-driven narrative. Both want the name; they serve
//! different surfaces and neither subsumes the other.
//!
//! Registering under upstream's own alias keeps both, breaks no client, and
//! avoids inventing a name: `checkup` is what upstream calls it too. Decision
//! taken by the user, 2026-09-10.
//!
//! ## What was cut, and why
//!
//! Two of upstream's ten checks are Anthropic-DISTRIBUTION diagnostics with no
//! counterpart here, and this port's accepted divergences already exclude the
//! Anthropic backend/distribution surface:
//!
//! * **Check 7 (version currency)** — removed whole. Every mechanism in it is
//!   Anthropic's: `npm view @anthropic-ai/claude-code`, the
//!   `downloads.claude.ai/claude-code-releases` endpoint, the `claude-code`
//!   Homebrew casks, and `claude update`. LingXi ships through none of them.
//! * **Check 0's first two bullets** (duplicate/leftover installs, native
//!   launcher missing from PATH) — same reason: they enumerate `~/.local/bin/claude`,
//!   npm-global `@anthropic-ai/claude-code`, and `installMethod`.
//!
//! Check 0's remaining three bullets — unparseable settings files, broken or
//! colliding agent definitions, malformed skill frontmatter — map exactly and
//! are kept. Checks 1-6 and 8-9 are unchanged apart from rebranding.
//!
//! Removing check 7 means the cross-references had to move with it: the check
//! list in the report format, the "checks 0 and 7" command note, the
//! consolidated-cleanup gate, and the data-sources header (which advertised
//! check 7 as the one permitted network call — this checkup now makes NO
//! network requests). [`tests::the_check_numbering_is_self_consistent`] pins
//! that, because a stale cross-reference is invisible to every other gate.
//!
//! ## Substrate that differs
//!
//! Every path the body names was verified against this port rather than
//! rebranded on faith:
//!
//! | upstream | here |
//! |---|---|
//! | `~/.claude.json` `skillUsage` | `~/.lingxi/skill_usage.json` (`crate::skill_usage`) |
//! | `~/.claude/projects/<cwd>/*.jsonl` | `~/.lingxi/projects/<cwd>[-<djb2>]/*.jsonl` (`session::jsonl::path`) |
//! | `.claude/{settings,agents,rules,skills}` | `.lingxi/…` (`branding::DOT_DIR`) |
//! | `getMaxMemoryCharacterCount` | `memory::MAX_MEMORY_CHARACTER_COUNT` |
//! | `claude plugin validate` / `claude mcp remove` | `lingxi-cli …` |
//!
//! ⚠️ `pluginUsage` has NO counterpart — this port tracks skill usage but not
//! plugin usage — so the body's plugin-usage guidance rests on transcript
//! evidence alone, which is the fallback upstream already specifies for
//! zero-count plugins.
//!
//! ⛔ `mcp__claude_ai_<connector>__` is left verbatim. It is the literal wire
//! prefix for claude.ai connectors, not branding; rebranding it would stop the
//! model matching real transcript entries.

use crate::BundledPromptFn;

/// `is()` — the checkup body, rebranded and with the two distribution checks cut.
const CHECKUP_BODY: &str = include_str!("checkup_body.md");

/// Upstream's `description`, minus the two cut checks' clauses.
pub(crate) const CHECKUP_DESCRIPTION: &str = "Health-check the user's LingXi setup and fix issues: diagnose setup health from local data (unparseable settings files, broken or colliding agent definitions, skills whose frontmatter fails to parse); find unused skills, MCP servers, and plugins versus their context cost and disable dead weight; deduplicate local LINGXI.md files against checked-in ones; trim checked-in LINGXI.md files by cutting content a session could derive from the codebase (directory layouts, tech-stack lists, architecture overviews) while keeping gotchas, rationale, and non-standard conventions; migrate always-loaded LINGXI.md guidance into lazy skills and nested LINGXI.md files; flag slow hooks and context-heavy extensions; make auto mode the default permission mode; and pre-approve frequently denied read-only commands. Use when the user asks for a checkup, audit, tune-up, or cleanup of their LingXi setup or configuration.";

pub(crate) const CHECKUP_MENU_DESCRIPTION: &str =
    "Health-check your setup and fix issues: unused extensions, duplicated or bloated memory files, slow hooks, permissions";

/// Upstream `progressMessage`. ⚠️ Not wired: `SlashCommand` carries no
/// progress-message field in this port. Kept so the copy survives until it can
/// be, rather than being lost in the port.
#[allow(dead_code)]
pub(crate) const CHECKUP_PROGRESS_MESSAGE: &str = "running checkup";

/// `getPromptForCommand(e)` — the body, plus the user's extra instructions.
pub struct CheckupPromptFn;

impl BundledPromptFn for CheckupPromptFn {
    fn build(&self, args: &str) -> String {
        let body = CHECKUP_BODY.trim_end_matches('\n');
        if args.is_empty() {
            body.to_string()
        } else {
            format!("{body}\n\n## Additional instructions from the user\n\n{args}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 🚨 Rebranding a path can turn a TRUE statement about the upstream repo
    /// into a FALSE one about this one. `readOnlyValidation.ts` was rewritten to
    /// say those files "live in the LingXi repo" — they do not exist here, and
    /// that bullet gates permission-rule minting. Any repo-relative source path
    /// the body names must actually exist.
    #[test]
    fn every_repo_path_the_body_names_exists() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("command-api -> crates");
        let mut missing = Vec::new();
        for token in CHECKUP_BODY.split('`') {
            let candidate = token.trim();
            let looks_like_source = candidate.ends_with(".ts")
                || candidate.ends_with(".rs")
                || candidate.starts_with("src/");
            if looks_like_source && !root.join(candidate).exists() {
                missing.push(candidate.to_string());
            }
        }
        assert!(
            missing.is_empty(),
            "the body names source paths that do not exist in this repo: {missing:?}"
        );
    }

    /// The local memory file this port actually loads is `LINGXI.local.md`
    /// (`branding::MEMORY_LOCAL_FILE`). Naming the upstream one sends checks 2
    /// and 3 hunting for a file that is never loaded here.
    #[test]
    fn the_body_names_the_local_memory_file_this_port_loads() {
        assert!(!CHECKUP_BODY.contains("CLAUDE.local.md"));
        assert!(CHECKUP_BODY.contains(branding::MEMORY_LOCAL_FILE));
    }

    #[test]
    fn the_body_is_branded() {
        for stale in [
            "Claude Code",
            "CLAUDE.md",
            "~/.claude",
            ".claude/",
            "claude doctor",
        ] {
            assert!(
                !CHECKUP_BODY.contains(stale),
                "{stale:?} survived the rebrand"
            );
            assert!(
                !CHECKUP_DESCRIPTION.contains(stale),
                "{stale:?} in description"
            );
        }
        assert!(CHECKUP_BODY.contains("LINGXI.md"));
        assert!(CHECKUP_BODY.contains("~/.lingxi/projects"));
    }

    /// ⛔ The one `claude` string that MUST survive: the MCP wire prefix for
    /// claude.ai connectors. Rebranding it would stop the model matching real
    /// transcript entries, and the branding test above would happily pass.
    #[test]
    fn the_claude_ai_mcp_wire_prefix_is_left_verbatim() {
        assert!(
            CHECKUP_BODY.contains("mcp__claude_ai_<connector>__"),
            "the claude.ai connector prefix is wire format, not branding"
        );
    }

    /// Removing check 7 meant moving every cross-reference to it. Nothing else
    /// can catch a stale one: prose has no compiler.
    #[test]
    fn the_check_numbering_is_self_consistent() {
        let headings: Vec<&str> = CHECKUP_BODY
            .lines()
            .filter_map(|l| l.strip_prefix("## Check "))
            .map(|l| l.split_whitespace().next().unwrap_or(""))
            .collect();
        assert_eq!(
            headings,
            ["0", "1", "2", "3", "4", "5", "6", "8", "9"],
            "check 7 is Anthropic-distribution-only and must stay removed"
        );
        // 🚨 Asserting the absence of ONE spelling is how the first version of
        // this test missed a live one: the body still said "checks 0-4 and 7",
        // which contains neither "check 7" nor "Check 7". Match the NUMBER in
        // any check-listing context instead.
        for stale in [
            "check 7",
            "Check 7",
            "checks 0-4 and 7",
            "0, 1, 2, 3, 4, 7",
            "and 7:",
            "4 and 7",
        ] {
            assert!(
                !CHECKUP_BODY.contains(stale),
                "{stale:?} references the removed check 7"
            );
        }
        // Every check number the body mentions in a list must be one that exists.
        let listed: std::collections::BTreeSet<&str> = CHECKUP_BODY
            .match_indices("checks ")
            .map(|(i, _)| &CHECKUP_BODY[i..(i + 24).min(CHECKUP_BODY.len())])
            .filter(|window| window.contains('7'))
            .collect();
        assert!(
            listed.is_empty(),
            "a `checks …` listing still names 7: {listed:?}"
        );
        // The actionable-checks list must name exactly the non-warning checks.
        assert!(
            CHECKUP_BODY.contains("(0, 1, 2, 3, 4, 8, 9)"),
            "the report format's check list must match the headings present"
        );
        // Checks 5 and 6 are warnings, not proposals.
        assert!(CHECKUP_BODY.contains("**Warnings** (checks 5 and 6)"));
    }

    /// The body promises "no network requests" now that check 7 is gone. If a
    /// future edit reintroduces a fetch, this is the line that contradicts it.
    #[test]
    fn the_body_claims_no_network_access() {
        assert!(CHECKUP_BODY.contains("this checkup makes NO network requests"));
        for endpoint in [
            "downloads.claude.ai",
            "registry.npmjs.org",
            "formulae.brew.sh",
        ] {
            assert!(
                !CHECKUP_BODY.contains(endpoint),
                "{endpoint:?} is an Anthropic-distribution lookup that check 7 owned"
            );
        }
    }

    #[test]
    fn user_instructions_are_appended_last() {
        let out = CheckupPromptFn.build("focus on MCP servers");
        assert!(out.ends_with("## Additional instructions from the user\n\nfocus on MCP servers"));
        assert!(!CheckupPromptFn
            .build("")
            .contains("## Additional instructions from the user"));
    }
}
