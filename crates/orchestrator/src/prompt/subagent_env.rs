//! Claude Code 2.1.319 child environment messages and initial prefix budget.
#![forbid(unsafe_code)]

use crate::prompt::env_meta::{detect_shell, knowledge_cutoff_for_model, marketing_name_for_model};
use agent::handle::{SubagentPromptContext, SubagentPromptTarget};
use std::path::{Path, PathBuf};

fn node_platform_name(rust_os: &str) -> &str {
    match rust_os {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

#[must_use]
pub fn boot_renderer(
    cwd: PathBuf,
) -> impl Fn(&SubagentPromptTarget, Option<&Path>) -> SubagentPromptContext + Send + Sync + 'static
{
    let is_git_repo = crate::prompt::git_status::probe(&cwd).is_some();
    let platform = node_platform_name(std::env::consts::OS).to_string();
    let shell = detect_shell();
    let os_version = crate::prompt::env_meta::os_version_string();
    move |target: &SubagentPromptTarget, cwd_override: Option<&Path>| {
        let (effective_cwd, in_worktree) = match cwd_override {
            Some(path) => (path, true),
            None => (cwd.as_path(), false),
        };
        subagent_env_block_for_target(
            target,
            effective_cwd,
            is_git_repo,
            &platform,
            &shell,
            &os_version,
            &[],
            in_worktree,
        )
    }
}

#[must_use]
pub fn subagent_env_block(
    model_id: &str,
    cwd: &Path,
    is_git_repo: bool,
    platform: &str,
    shell: &str,
    os_version: &str,
    additional_dirs: &[String],
    in_worktree: bool,
) -> SubagentPromptContext {
    let target = agent::prompt_policy::resolve_target(None, model_id, None, Default::default())
        .expect("unresolved prompt target is infallible");
    subagent_env_block_for_target(
        &target,
        cwd,
        is_git_repo,
        platform,
        shell,
        os_version,
        additional_dirs,
        in_worktree,
    )
}

#[must_use]
pub fn subagent_env_block_for_target(
    target: &SubagentPromptTarget,
    cwd: &Path,
    is_git_repo: bool,
    platform: &str,
    shell: &str,
    os_version: &str,
    additional_dirs: &[String],
    in_worktree: bool,
) -> SubagentPromptContext {
    subagent_prompt_context_for_target_date(
        target,
        cwd,
        is_git_repo,
        platform,
        shell,
        os_version,
        additional_dirs,
        in_worktree,
        &crate::prompt::env_meta::current_date_string(),
    )
}

#[cfg(test)]
fn subagent_prompt_context_for_date(
    model_id: &str,
    cwd: &Path,
    is_git_repo: bool,
    platform: &str,
    shell: &str,
    os_version: &str,
    additional_dirs: &[String],
    in_worktree: bool,
    date: &str,
) -> SubagentPromptContext {
    let target = agent::prompt_policy::resolve_target(None, model_id, None, Default::default())
        .expect("unresolved prompt target is infallible");
    subagent_prompt_context_for_target_date(
        &target,
        cwd,
        is_git_repo,
        platform,
        shell,
        os_version,
        additional_dirs,
        in_worktree,
        date,
    )
}

fn subagent_prompt_context_for_target_date(
    target: &SubagentPromptTarget,
    cwd: &Path,
    is_git_repo: bool,
    platform: &str,
    shell: &str,
    os_version: &str,
    additional_dirs: &[String],
    in_worktree: bool,
    date: &str,
) -> SubagentPromptContext {
    let model_id = target.model.as_str();
    let context = crate::prompt::SystemPromptContext {
        prompt_policy: Some(target.policy.clone()),
        native_system_turns: Some(target.supports_system_turns),
        cwd: cwd.to_path_buf(),
        platform: platform.to_owned(),
        model: model_id.to_owned(),
        model_marketing_name: marketing_name_for_model(model_id).map(str::to_owned),
        knowledge_cutoff: knowledge_cutoff_for_model(model_id).map(str::to_owned),
        shell: shell.to_owned(),
        os_version: os_version.to_owned(),
        git_status: is_git_repo.then(Default::default),
        in_worktree,
        file_tree: Default::default(),
        memory_files: Vec::new(),
        tool_names: Vec::new(),
        skills_available: false,
        is_interactive: false,
        memory_dir: None,
        exclude_dynamic_sections: false,
    };
    let mut additional_context = vec![
        crate::prompt::env_block::format_host_environment_with_additional_dirs(
            &context,
            additional_dirs,
        ),
    ];
    if matches!(
        target.policy.system,
        lingxi_core::host::prompt_policy::SystemPromptBinding::ClaudeCode
            | lingxi_core::host::prompt_policy::SystemPromptBinding::ClaudeChat
    ) {
        additional_context.push(
            crate::prompt::env_block::format_model_identity_with_rewrite(
                &context,
                lingxi_core::host::model_capabilities::lean_rewrite_for(
                    &target.policy.target.canonical_model,
                ),
            ),
        );
    } else {
        additional_context.push(format!("Model ID: {}.", target.policy.target.request_model));
    }
    additional_context.push(format!("Today's date is {date}."));
    SubagentPromptContext {
        additional_context,
        system_suffix: target
            .policy
            .reminders
            .is_native()
            .then(|| {
                crate::prompt::total_tokens::system_prompt_section(
                    &target.policy.target.canonical_model,
                )
            })
            .flatten(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_context_sections_match_319_live_captures() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/agent_child_prefix_319.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let rendered = subagent_prompt_context_for_date(
                case["model"].as_str().unwrap(),
                Path::new("<CWD>"),
                false,
                "darwin",
                "zsh",
                "<OS_VERSION>",
                &[],
                false,
                "<DATE>",
            );
            let expected = case["contextSections"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap().to_owned())
                .collect::<Vec<_>>();
            assert_eq!(rendered.additional_context, expected, "{}", case["model"]);
            assert_eq!(
                rendered.system_suffix.as_deref(),
                case["systemSuffix"].as_str(),
                "{}",
                case["model"]
            );
        }
    }

    #[test]
    fn child_environment_is_separate_from_the_system_suffix() {
        let context = subagent_prompt_context_for_date(
            "claude-opus-5",
            Path::new("/workspace"),
            false,
            "darwin",
            "zsh",
            "Darwin 27.2.0",
            &[],
            false,
            "2026-11-09",
        );
        assert_eq!(context.additional_context.len(), 3);
        assert!(context.additional_context[0].starts_with("# Environment\n"));
        assert!(
            context.additional_context[0].contains("\n - Primary working directory: /workspace")
        );
        assert_eq!(context.additional_context[1], "You are powered by the model named Opus 5. The exact model ID is claude-opus-5. Assistant knowledge cutoff is May 2026.");
        assert_eq!(context.additional_context[2], "Today's date is 2026-11-09.");
        assert_eq!(
            context.system_suffix.as_deref(),
            Some("<total_tokens>15000000 tokens left</total_tokens>")
        );
        assert!(!context.additional_context.join("\n").contains("<env>"));
    }

    #[test]
    fn models_without_total_token_capability_get_no_prefix_budget() {
        let context = subagent_prompt_context_for_date(
            "claude-sonnet-5-5",
            Path::new("/workspace"),
            false,
            "linux",
            "bash",
            "Linux 6.8",
            &[],
            false,
            "2026-11-09",
        );
        assert!(context.system_suffix.is_none());
        assert!(context.additional_context[1].contains("Sonnet 5.5"));
    }

    #[test]
    fn worktree_and_additional_directories_are_environment_context() {
        let context = subagent_prompt_context_for_date(
            "claude-sonnet-4-6",
            Path::new("/repo/.lingxi/worktrees/child"),
            true,
            "linux",
            "bash",
            "Linux 6.8",
            &["/additional".to_owned()],
            true,
            "2026-11-09",
        );
        assert!(context.additional_context[0].contains("This is a git worktree"));
        assert!(context.additional_context[0]
            .contains("\n - Additional working directories:\n  - /additional"));
    }
}
