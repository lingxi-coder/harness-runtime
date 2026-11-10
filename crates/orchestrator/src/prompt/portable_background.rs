//! Provider-neutral background-session prompts from the selected bundle.

/// Portable bundles bind their own background text; dynamic facts and the
/// admitted tool catalog determine which instructions can actually execute.
pub(crate) fn portable_section(
    reminders: lingxi_core::host::prompt_policy::ReminderPromptBinding,
    session_kind: Option<&str>,
    job_dir: Option<&str>,
    isolation: Option<&str>,
    tools: &[String],
) -> Option<String> {
    if session_kind != Some("bg") {
        return None;
    }
    let job_dir = job_dir.filter(|directory| !directory.is_empty())?;
    let template = reminders.template_for("bg-session")?;
    let has_worktree = tools.iter().any(|tool| tool == "EnterWorktree");
    let isolation_guidance = match isolation {
        Some("none") => "Work in the current checkout; this job does not request worktree isolation.",
        Some("worktree") if has_worktree => reminders.template_for("bg-worktree-first").unwrap_or(""),
        _ if has_worktree => reminders.template_for("bg-before-edits").unwrap_or(""),
        _ => "Worktree isolation is unavailable in this tool catalog. Continue in the current checkout.",
    };
    let has_shell = tools
        .iter()
        .any(|tool| matches!(tool.as_str(), "Bash" | "Shell" | "PowerShell"));
    let git_guidance = if has_shell && has_worktree && isolation != Some("none") {
        reminders.template_for("bg-git").unwrap_or("")
    } else {
        ""
    };
    Some(
        template
            .replace("{isolation_guidance}", isolation_guidance.trim_end())
            .replace("{git_guidance}", git_guidance)
            .replace("{job_dir}", job_dir)
            .replace(
                "{tmp_dir}",
                &std::path::Path::new(job_dir).join("tmp").to_string_lossy(),
            )
            .replace("{dot_dir}", branding::DOT_DIR),
    )
}

pub(crate) fn portable_from_env(
    reminders: lingxi_core::host::prompt_policy::ReminderPromptBinding,
    tools: &[String],
) -> Option<String> {
    portable_section(
        reminders,
        std::env::var("LINGXI_SESSION_KIND").ok().as_deref(),
        std::env::var("LINGXI_JOB_DIR").ok().as_deref(),
        std::env::var("LINGXI_BG_ISOLATION").ok().as_deref(),
        tools,
    )
}

#[cfg(test)]
mod portable_tests {
    use super::*;
    use lingxi_core::host::{
        prompt_policy::{resolve_prompt_policy, PromptTarget},
        session_context::ConversationMode,
    };

    #[test]
    fn prompt_portable_background_obeys_isolation_and_available_tools() {
        for mode in [ConversationMode::Chat, ConversationMode::Code] {
            for model in ["gpt-5.5", "kimi-k2.5"] {
                let policy = resolve_prompt_policy(PromptTarget::unresolved(model, None, mode));
                for isolation in [Some("none"), Some("worktree"), None] {
                    for tools in [
                        vec![],
                        vec!["EnterWorktree".into()],
                        vec!["EnterWorktree".into(), "Bash".into()],
                    ] {
                        let body = portable_section(
                            policy.reminders,
                            Some("bg"),
                            Some("/jobs/example"),
                            isolation,
                            &tools,
                        )
                        .unwrap();
                        assert!(body.contains("/jobs/example/tmp"));
                        assert!(body.contains("somewhere durable"));
                        assert!(body.contains("Never push to main/master, force-push, or merge."));
                        assert_eq!(
                            body.contains("Call EnterWorktree as your first action"),
                            isolation == Some("worktree") && !tools.is_empty()
                        );
                        assert_eq!(
                            body.contains("Before editing repository files, call EnterWorktree"),
                            isolation.is_none() && !tools.is_empty()
                        );
                        assert_eq!(
                            body.contains("commit before finishing"),
                            isolation != Some("none") && tools.len() == 2
                        );
                        assert!(!body.contains("Claude"));
                    }
                }
                assert!(
                    portable_section(policy.reminders, None, Some("/jobs/example"), None, &[])
                        .is_none()
                );
                assert!(portable_section(policy.reminders, Some("bg"), None, None, &[]).is_none());
            }
        }
    }
}
