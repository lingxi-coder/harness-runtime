//! Claude Code 2.1.319 background-session prompts, selected by explicit host facts.

#[must_use]
pub fn section(
    session_kind: Option<&str>,
    job_dir: Option<&str>,
    isolation: Option<&str>,
) -> Option<String> {
    section_with_rewrite(session_kind, job_dir, isolation, false)
}

#[must_use]
pub fn section_with_rewrite(
    session_kind: Option<&str>,
    job_dir: Option<&str>,
    isolation: Option<&str>,
    rewrite: bool,
) -> Option<String> {
    if session_kind != Some("bg") {
        return None;
    }
    let job_dir = job_dir.filter(|dir| !dir.is_empty())?;
    let template = match (rewrite, isolation) {
        (false, Some("none")) => include_str!("templates/bg_standard_in_place.md"),
        (false, Some("worktree")) => include_str!("templates/bg_standard_worktree_first.md"),
        (false, _) => include_str!("templates/bg_standard_worktree_before_edits.md"),
        (true, Some("none")) => include_str!("templates/bg_rewritten_in_place.md"),
        (true, Some("worktree")) => include_str!("templates/bg_rewritten_worktree_first.md"),
        (true, _) => include_str!("templates/bg_rewritten_worktree_before_edits.md"),
    };
    let tmp_dir = std::path::Path::new(job_dir).join("tmp");
    Some(
        template
            .replace("{{tmp_dir}}", &tmp_dir.to_string_lossy())
            .replace("{{dot_dir}}", branding::DOT_DIR),
    )
}

#[must_use]
pub fn from_env() -> Option<String> {
    from_env_with_rewrite(false)
}

#[must_use]
pub fn from_env_with_rewrite(rewrite: bool) -> Option<String> {
    let kind = std::env::var("LINGXI_SESSION_KIND").ok();
    let job_dir = std::env::var("LINGXI_JOB_DIR").ok();
    let isolation = std::env::var("LINGXI_BG_ISOLATION")
        .ok()
        .filter(|value| value == "worktree" || value == "none");
    section_with_rewrite(
        kind.as_deref(),
        job_dir.as_deref(),
        isolation.as_deref(),
        rewrite,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_sessions_get_no_section() {
        for rewrite in [false, true] {
            assert_eq!(
                section_with_rewrite(None, Some("/jobs/example"), None, rewrite),
                None
            );
            assert_eq!(
                section_with_rewrite(Some("interactive"), Some("/jobs/example"), None, rewrite),
                None
            );
            assert_eq!(section_with_rewrite(Some("bg"), None, None, rewrite), None);
            assert_eq!(
                section_with_rewrite(Some("bg"), Some(""), None, rewrite),
                None
            );
        }
    }

    #[test]
    fn background_prompts_match_319_native_builders() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/agent_reminders_319.json"
        ))
        .unwrap();
        for case in fixture["background"].as_array().unwrap() {
            let isolation = match case["isolation"].as_str().unwrap() {
                "in-place" => Some("none"),
                "worktree-first" => Some("worktree"),
                _ => None,
            };
            let actual = section_with_rewrite(
                Some("bg"),
                Some("/jobs/example"),
                isolation,
                case["rewrite"].as_bool().unwrap(),
            )
            .unwrap();
            assert_eq!(actual, case["body"].as_str().unwrap(), "{case}");
            assert!(actual.contains("anything the user should keep belongs somewhere durable"));
            assert!(actual.contains("If you're running as a subagent"));
            if isolation != Some("none") {
                assert!(actual.contains("Never push to main/master, force-push, or merge."));
            }
        }
    }
}
