//! `/team-onboarding` prompt command.
//!
//! The runtime owns local transcript discovery and prompt generation. This
//! module owns the source-shaped slash-command contract and keeps policy and
//! workspace checks separate from the data provider.

use crate::builtin_support::names::core_description;
use crate::model::{
    BuiltinCommandHandler, CommandResult, NativeCommandPolicyGate, NativeCommandRequirements,
    NativeCommandType,
};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::sync::Arc;

/// Host facts required by Native's `/team-onboarding` prompt factory.
///
/// The workspace requirement and the named local policy are independent. An
/// unknown fact stays unavailable instead of being inferred from the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TeamOnboardingCommandAvailability {
    /// The current workspace exists and satisfies the host's trust rule.
    pub workspace_available: Option<bool>,
    /// Local verdict for Native's `allow_team_onboarding` policy gate.
    pub allow_team_onboarding: Option<bool>,
}

impl TeamOnboardingCommandAvailability {
    /// The command may run only when both independent facts are explicitly
    /// allowed.
    #[must_use]
    pub const fn is_available(self) -> bool {
        matches!(self.workspace_available, Some(true))
            && matches!(self.allow_team_onboarding, Some(true))
    }
}

/// Runtime-owned local-data producer for `/team-onboarding`.
#[async_trait]
pub trait TeamOnboardingPromptProvider: Send + Sync {
    /// Current workspace and local policy facts used by discovery and by the
    /// final invocation check.
    fn command_availability(&self) -> TeamOnboardingCommandAvailability;

    /// Scan local, workspace-scoped history and produce the exact prompt text.
    async fn generate_prompt(&self) -> Result<Utf16JsonProjection, Utf16JsonProjection>;
}

/// `/team-onboarding` handler bound to a real host-local usage-data producer.
pub struct TeamOnboardingHandler {
    provider: Arc<dyn TeamOnboardingPromptProvider>,
}

impl std::fmt::Debug for TeamOnboardingHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TeamOnboardingHandler")
            .finish_non_exhaustive()
    }
}

impl TeamOnboardingHandler {
    /// Construct the handler with its runtime-owned local producer.
    #[must_use]
    pub fn new(provider: Arc<dyn TeamOnboardingPromptProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl BuiltinCommandHandler for TeamOnboardingHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        if !self.provider.command_availability().is_available() {
            return CommandResult::Done {
                display: Some("Team onboarding is unavailable for this workspace.".into()),
            };
        }

        match self.provider.generate_prompt().await {
            Ok(prompt) => CommandResult::InjectMessage { content: prompt },
            Err(display) => CommandResult::Done {
                display: Some(display),
            },
        }
    }

    fn name(&self) -> &str {
        "team-onboarding"
    }

    fn description(&self) -> &str {
        core_description("team-onboarding")
    }

    fn native_type(&self) -> Option<NativeCommandType> {
        Some(NativeCommandType::Prompt)
    }

    fn is_enabled(&self) -> bool {
        self.provider.command_availability().is_available()
    }

    fn disable_model_invocation(&self) -> bool {
        true
    }

    fn content_length(&self) -> Option<usize> {
        Some(0)
    }

    fn progress_message(&self) -> Option<Utf16JsonProjection> {
        Some("scanning usage data".into())
    }

    fn requires(&self) -> Option<NativeCommandRequirements> {
        Some(NativeCommandRequirements {
            workspace: Some(true),
        })
    }

    fn policy_gate(&self) -> Option<NativeCommandPolicyGate> {
        Some(NativeCommandPolicyGate {
            policy: "allow_team_onboarding".to_owned(),
        })
    }

    /// Native grants the prompt only its local guide-writing/listing tools.
    /// Its third `ShareOnboardingGuide` tool is a remote organization upload
    /// surface and is intentionally not connected by this local provider.
    fn allowed_tools(&self) -> &'static [&'static str] {
        &["Edit(ONBOARDING.md)", "Bash(ls *)"]
    }
}

/// Install the real handler only when the host supplied both required facts.
/// The disabled source-shaped fallback registered by `register_all` remains in
/// place otherwise.
pub fn register_team_onboarding_command(
    registry: &mut crate::CommandRegistry,
    provider: Arc<dyn TeamOnboardingPromptProvider>,
) -> bool {
    if !provider.command_availability().is_available() {
        return false;
    }
    registry.register_builtin_handler(Arc::new(TeamOnboardingHandler::new(provider)));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin_support::SourceTextTestExt;
    use crate::model::NativeCommandPolicyGate;
    use std::sync::Mutex;

    struct FakeProvider {
        availability: TeamOnboardingCommandAvailability,
        prompt: Result<Utf16JsonProjection, Utf16JsonProjection>,
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl TeamOnboardingPromptProvider for FakeProvider {
        fn command_availability(&self) -> TeamOnboardingCommandAvailability {
            self.availability
        }

        async fn generate_prompt(&self) -> Result<Utf16JsonProjection, Utf16JsonProjection> {
            *self.calls.lock().unwrap() += 1;
            self.prompt.clone()
        }
    }

    fn available() -> TeamOnboardingCommandAvailability {
        TeamOnboardingCommandAvailability {
            workspace_available: Some(true),
            allow_team_onboarding: Some(true),
        }
    }

    fn args(raw_args: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "team-onboarding".into(),
            raw_args: raw_args.into(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn injects_provider_prompt_and_keeps_native_metadata() {
        let provider = Arc::new(FakeProvider {
            availability: available(),
            prompt: Ok("complete local onboarding prompt".into()),
            calls: Mutex::new(0),
        });
        let handler = TeamOnboardingHandler::new(provider.clone());
        let result = handler.handle(&args("ignored user text")).await;
        let CommandResult::InjectMessage { content } = result else {
            panic!("expected injected prompt, got {result:?}");
        };
        assert_eq!(
            content.string_at(""),
            Some("complete local onboarding prompt")
        );
        assert_eq!(*provider.calls.lock().unwrap(), 1);
        assert_eq!(handler.native_type(), Some(NativeCommandType::Prompt));
        assert!(handler.disable_model_invocation());
        assert_eq!(handler.content_length(), Some(0));
        assert_eq!(
            handler.progress_message().unwrap().string_at(""),
            Some("scanning usage data")
        );
        assert_eq!(handler.requires().unwrap().workspace, Some(true));
        assert_eq!(
            handler.policy_gate(),
            Some(NativeCommandPolicyGate {
                policy: "allow_team_onboarding".to_owned(),
            })
        );
        assert_eq!(
            handler.allowed_tools(),
            &["Edit(ONBOARDING.md)", "Bash(ls *)"]
        );
    }

    #[tokio::test]
    async fn unknown_or_denied_policy_and_workspace_facts_fail_closed() {
        for availability in [
            TeamOnboardingCommandAvailability::default(),
            TeamOnboardingCommandAvailability {
                workspace_available: Some(true),
                allow_team_onboarding: Some(false),
            },
            TeamOnboardingCommandAvailability {
                workspace_available: Some(false),
                allow_team_onboarding: Some(true),
            },
        ] {
            assert!(!availability.is_available());
            let provider = Arc::new(FakeProvider {
                availability,
                prompt: Ok("must not be read".into()),
                calls: Mutex::new(0),
            });
            let result = TeamOnboardingHandler::new(provider.clone())
                .handle(&args(""))
                .await;
            assert!(matches!(result, CommandResult::Done { .. }));
            assert_eq!(*provider.calls.lock().unwrap(), 0);
        }
    }

    #[tokio::test]
    async fn provider_errors_are_displayed_without_becoming_prompts() {
        let provider = Arc::new(FakeProvider {
            availability: available(),
            prompt: Err("Could not read local session data.".into()),
            calls: Mutex::new(0),
        });
        let result = TeamOnboardingHandler::new(provider).handle(&args("")).await;
        let CommandResult::Done {
            display: Some(display),
        } = result
        else {
            panic!("expected provider error display, got {result:?}");
        };
        assert!(display.contains("Could not read local session data."));
    }
}
