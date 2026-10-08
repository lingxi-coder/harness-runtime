use command_api::CommandRegistry;
use orchestrator::prompt::skill_listing::SkillListingEntry;
use tool_api::tool_trait::BashPrecommitSkills;

/// Resolve exact native skill names after the host's eligibility/allowlist projection.
pub(crate) fn bash_precommit_skills(
    registry: &CommandRegistry,
    entries: &[SkillListingEntry],
    include_code_review_suggestion: bool,
) -> BashPrecommitSkills {
    let visible = |name: &str| entries.iter().any(|entry| entry.name == name);
    let commands = registry.model_invocable_commands();
    let custom = |name: &str| {
        visible(name)
            && commands
                .iter()
                .find(|command| command.name == name)
                .is_some_and(|command| {
                    matches!(
                        command.loaded_from.as_deref(),
                        Some("skills" | "commands_DEPRECATED")
                    )
                })
    };
    BashPrecommitSkills {
        custom_verify: custom("verify"),
        custom_simplify: custom("simplify"),
        code_review: include_code_review_suggestion && visible("code-review"),
    }
}
