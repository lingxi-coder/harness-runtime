//! The `/code-review` bundled skill — a port of Claude Code 2.1.205's most
//! complex bundled skill (name-var `Lee`, assembler `wrb`). It reviews the
//! working diff for correctness bugs + reuse/simplification/efficiency/altitude/
//! conventions cleanups, scaling the finder angles by effort level.
//!
//! Distinct from LingXi's builtin `/review` (which reviews a GitHub PR — its
//! description literally points here: "for your working diff use /code-review").
//!
//! # Faithful default-path port
//! The reference `getPromptForCommand` (`wrb`) has two context-dependent routes
//! that BOTH default OFF in a stock install and are unreachable through
//! LingXi's pure `args -> String` [`BundledPromptFn`] interface:
//! - the **workflow route** (`Irb`: needs a statsig flag + high+ effort + the
//!   workflow tool) — default false;
//! - the **report_findings tool** path (`Srb`: needs the tool + a flag) —
//!   default false ⇒ the plain-text findings output (`ICd`).
//!
//! The builder replicates the **plain-text review body**, which is byte-exact
//! for the reachable cases. The command descriptor wraps that body in the
//! normal `context: fork` + background skill path, so the review executes in
//! an independent child transcript instead of growing the main transcript.
//! Three pieces of `wrb` depend on runtime context that `build(args)` does not
//! receive, and are handled as documented below:
//! - **effort default** — `wrb` uses the session reasoning effort
//!   (`Zy(ctx)`); when unset it falls back to `"medium"`. `build` has no session
//!   context, so a `/code-review` with no explicit level uses `"medium"` (the
//!   reference's own fallback). An explicit level (`/code-review high`) is exact.
//! - **`T.text` finder-budget note** — empty at `medium` (so the default is
//!   exact) and model-dependent at `high`/`xhigh`/`max`; omitted (no model
//!   context) — an explicit high+ prompt is the effort body without that note.
//! - **`ultra` + unrecognized-effort preambles** (`Hrb`) — context-dependent
//!   fallback notes; `ultra` maps to the `max` body (the `ultra` subcommand
//!   routes to `/ultrareview` at the dispatcher anyway), and an unrecognized
//!   first token is treated as the target (no warning note).
//!
//! Branding: `CLAUDE.md` → `LINGXI.md` and `.claude/` → `.lingxi/` in the bodies
//! (the Conventions angle); the `--comment`/`--fix` sections need none. The
//! agent-launching tool is `"Agent"` = LingXi's `AGENT_TOOL_NAME`.

use command_api::BundledPromptFn;

const BODY_LOW: &str = include_str!("code_review_body_low.md");
const BODY_MEDIUM: &str = include_str!("code_review_body_medium.md");
const BODY_HIGH: &str = include_str!("code_review_body_high.md");
const BODY_XHIGH: &str = include_str!("code_review_body_xhigh.md");
const BODY_MAX: &str = include_str!("code_review_body_max.md");
/// Appended for `--comment` (reference `vWp`); starts with its own `\n\n`.
const COMMENT_SECTION: &str = include_str!("code_review_comment.md");
/// Appended for `--fix` (reference `CWp(false)`, report_findings absent);
/// starts with its own `\n\n`.
const FIX_SECTION: &str = include_str!("code_review_fix.md");

/// The skill's `description` (binary var `Crb`), with the `${MGt()?…}` ultra
/// clause resolved away (`MGt()` defaults false — ultra unavailable).
pub(crate) const CODE_REVIEW_DESCRIPTION: &str = "Review the current diff for correctness bugs and reuse/simplification/efficiency cleanups at the given effort level (low/medium: fewer, high-confidence findings; high→max: broader coverage, may include uncertain findings). Pass --comment to post findings as inline PR comments, or --fix to apply the findings to the working tree after the review.";

/// The skill's `argumentHint` (binary var `Arb`, default form; `MGt()` false so
/// no `|ultra`).
pub(crate) const CODE_REVIEW_ARGUMENT_HINT: &str =
    "[low|medium|high|xhigh|max] [--fix] [--comment] [<target>]";

fn is_level(s: &str) -> bool {
    matches!(s, "low" | "medium" | "high" | "xhigh" | "max")
}

fn body_for(effort: &str) -> &'static str {
    match effort {
        "low" => BODY_LOW,
        "high" => BODY_HIGH,
        "xhigh" => BODY_XHIGH,
        "max" => BODY_MAX,
        // "medium" and the no-session-context default.
        _ => BODY_MEDIUM,
    }
}

/// Parsed `/code-review` args (port of `wWp`/`ywo`/`iyt`).
struct Parsed {
    effort: String,
    target: String,
    comment: bool,
    fix: bool,
}

/// Port of `wWp(e)`: strip the `--comment`/`--fix` flags, then read the leading
/// effort level (or `ultra`), with the remainder as the review target.
fn parse_args(raw: &str) -> Parsed {
    let trimmed = raw.trim();
    // `ywo` rawFirstToken = first token of the trimmed RAW (before flag strip);
    // only used for the `ultra` check.
    let raw_first = trimmed.split_whitespace().next().unwrap_or("");
    // Strip the only two flags (whole-word). Token-based: realistic targets have
    // no significant internal whitespace, so this matches the reference regex.
    let mut comment = false;
    let mut fix = false;
    let rest: Vec<&str> = trimmed
        .split_whitespace()
        .filter(|t| match *t {
            "--comment" => {
                comment = true;
                false
            }
            "--fix" => {
                fix = true;
                false
            }
            _ => true,
        })
        .collect();
    let target_after_first = || rest.iter().skip(1).copied().collect::<Vec<_>>().join(" ");

    if raw_first.eq_ignore_ascii_case("ultra") {
        // ultraFallback → `l="max"`; the fallback preamble is context-dependent
        // and omitted (see module docs).
        return Parsed {
            effort: "max".into(),
            target: target_after_first(),
            comment,
            fix,
        };
    }
    // `iyt`: lowercase, alias `med`→`medium`, valid iff one of the 5 levels.
    let first = rest.first().copied().unwrap_or("");
    let lvl = first.to_ascii_lowercase();
    let lvl = if lvl == "med" {
        "medium".to_string()
    } else {
        lvl
    };
    if is_level(&lvl) {
        return Parsed {
            effort: lvl,
            target: target_after_first(),
            comment,
            fix,
        };
    }
    // explicit undefined → `"medium"` (no session context), target = whole rest.
    Parsed {
        effort: "medium".into(),
        target: rest.join(" "),
        comment,
        fix,
    }
}

/// Dynamic prompt builder for `/code-review` (reference `wrb`, child body).
pub struct CodeReviewPromptFn;

impl BundledPromptFn for CodeReviewPromptFn {
    fn build(&self, args: &str) -> String {
        let p = parse_args(args);
        // Reference inline assembly: `v + EWp(effort) + (comment?vWp) + (fix?CWp)`
        // (the `Hrb` preamble and `T.text` are empty for the reachable default).
        let mut out = String::new();
        if !p.target.is_empty() {
            out.push_str("Review target: `");
            out.push_str(&p.target);
            out.push_str("`\n\n");
        }
        out.push_str(body_for(&p.effort));
        if p.comment {
            out.push_str(COMMENT_SECTION);
        }
        if p.fix {
            out.push_str(FIX_SECTION);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_are_branded_and_resolved() {
        for (eff, body) in [
            ("low", BODY_LOW),
            ("medium", BODY_MEDIUM),
            ("high", BODY_HIGH),
            ("xhigh", BODY_XHIGH),
            ("max", BODY_MAX),
        ] {
            assert!(!body.contains("${"), "{eff} has unresolved interpolation");
            assert!(!body.contains("CLAUDE.md"), "{eff} unbranded CLAUDE.md");
            assert!(!body.contains(".claude/"), "{eff} unbranded .claude/");
            assert!(body.starts_with(&format!("`{eff} effort")), "{eff} tagline");
        }
        // The Conventions angle rebrands to LINGXI.md; the Agent tool is correct.
        assert!(BODY_MEDIUM.contains("LINGXI.md"));
        assert!(BODY_MEDIUM.contains("via the Agent tool"));
    }

    #[test]
    fn default_is_the_medium_body() {
        // No args ⇒ medium effort, no target — exactly the medium body.
        assert_eq!(CodeReviewPromptFn.build(""), BODY_MEDIUM);
        assert_eq!(CodeReviewPromptFn.build("   "), BODY_MEDIUM);
    }

    #[test]
    fn explicit_effort_selects_the_body() {
        assert_eq!(CodeReviewPromptFn.build("low"), BODY_LOW);
        assert_eq!(CodeReviewPromptFn.build("high"), BODY_HIGH);
        assert_eq!(CodeReviewPromptFn.build("xhigh"), BODY_XHIGH);
        assert_eq!(CodeReviewPromptFn.build("max"), BODY_MAX);
        // `med` aliases to medium; case-insensitive.
        assert_eq!(CodeReviewPromptFn.build("med"), BODY_MEDIUM);
        assert_eq!(CodeReviewPromptFn.build("HIGH"), BODY_HIGH);
        // ultra maps to the max body.
        assert_eq!(CodeReviewPromptFn.build("ultra"), BODY_MAX);
    }

    #[test]
    fn target_prepends_review_target_line() {
        assert_eq!(
            CodeReviewPromptFn.build("pull/42"),
            format!("Review target: `pull/42`\n\n{BODY_MEDIUM}")
        );
        // Level + target.
        assert_eq!(
            CodeReviewPromptFn.build("high src/foo.rs"),
            format!("Review target: `src/foo.rs`\n\n{BODY_HIGH}")
        );
    }

    #[test]
    fn flags_append_their_sections_and_are_stripped_from_target() {
        // --comment / --fix appended; not treated as the target.
        assert_eq!(
            CodeReviewPromptFn.build("--comment"),
            format!("{BODY_MEDIUM}{COMMENT_SECTION}")
        );
        assert_eq!(
            CodeReviewPromptFn.build("--fix"),
            format!("{BODY_MEDIUM}{FIX_SECTION}")
        );
        // Level + flags + target, both sections in order.
        assert_eq!(
            CodeReviewPromptFn.build("high --fix --comment pull/9"),
            format!("Review target: `pull/9`\n\n{BODY_HIGH}{COMMENT_SECTION}{FIX_SECTION}")
        );
    }
}
