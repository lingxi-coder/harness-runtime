use command_api::dispatcher::SkillPromptTransformer;
use command_api::CommandRegistry;
use hooks::mods::{ModDispatchScope, ModSessionContext};
use hooks::HookRegistry;
use lingxi_core::host::OutputStream;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use orchestrator::prompt::skill_listing::SkillListingEntry;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;
use tool_api::tool_trait::BashPrecommitSkills;

/// Apply the currently loaded `skill.prompt` Mod chain to a completed prompt.
/// The registry is consulted for every invocation so module load/unload takes
/// effect without rebuilding the slash dispatcher.
pub(crate) fn mod_skill_prompt_transformer(
    hook_registry: Arc<RwLock<HookRegistry>>,
    session: Arc<dyn ModSessionContext>,
    output: Arc<dyn OutputStream>,
) -> SkillPromptTransformer {
    Arc::new(move |skill, text| {
        let hook_registry = hook_registry.clone();
        let session = session.clone();
        let output = output.clone();
        Box::pin(async move {
            let host = hook_registry.read().await.mod_host();
            let Some(host) = host.filter(|host| host.has_event("skill.prompt")) else {
                return Ok(text);
            };

            let input = prompt_event_projection(skill, text.clone())
                .map_err(|error| prompt_projection_error(error.to_string()))?;
            let cwd = session.cwd();
            let log_output = output.clone();
            let toast_output = output.clone();
            let status_output = output;
            let outcome = host
                .dispatch_with_utf16_at_context_scope(
                    "skill.prompt",
                    input,
                    &cwd,
                    Some(session.as_ref()),
                    ModDispatchScope::default(),
                    lingxi_core::host::task_registry::FieldPresence::Missing,
                    |input| async move { Ok(input) },
                    move |plugin, text| {
                        let output = log_output.clone();
                        async move { output.emit_mod_log(&plugin, &text).await }
                    },
                    move |plugin, text, timeout_ms| {
                        let output = toast_output.clone();
                        async move { output.emit_mod_toast(&plugin, &text, timeout_ms).await }
                    },
                    move |plugin, text| {
                        let output = status_output.clone();
                        async move { output.emit_mod_status(&plugin, text.as_ref()).await }
                    },
                )
                .await
                .map_err(|error| prompt_projection_error(error.to_string()))?;

            let result = outcome.result_projection;
            result
                .validate()
                .map_err(|error| prompt_projection_error(error.to_string()))?;
            let transformed = result.subprojection("/text").map_err(|_| {
                prompt_projection_error("skill.prompt result must include string text".into())
            })?;
            let Some(transformed_units) = transformed.string_units("") else {
                return Err(prompt_projection_error(
                    "skill.prompt result must include string text".into(),
                ));
            };
            if text.string_units("").as_deref() == Some(transformed_units.as_slice()) {
                Ok(text)
            } else {
                Ok(transformed)
            }
        })
    })
}

fn prompt_event_projection(
    skill: Utf16JsonProjection,
    text: Utf16JsonProjection,
) -> Result<Utf16JsonProjection, lingxi_core::types::utf16_json::Utf16JsonProjectionError> {
    let mut input = Utf16JsonProjection::plain(json!({"skill":"","text":""}));
    input.set_field("skill", skill)?;
    input.set_field("text", text)?;
    Ok(input)
}

fn prompt_projection_error(error: String) -> Utf16JsonProjection {
    Utf16JsonProjection::plain(json!(error))
}

/// Resolve exact native skill names after the host's eligibility/allowlist projection.
pub(crate) fn bash_precommit_skills(
    registry: &CommandRegistry,
    entries: &[SkillListingEntry],
    include_code_review_suggestion: bool,
) -> BashPrecommitSkills {
    let visible = |name: &str| {
        entries.iter().any(|entry| {
            entry
                .name
                .string_units("")
                .is_some_and(|units| units.iter().copied().eq(name.encode_utf16()))
        })
    };
    let commands = registry.model_invocable_commands();
    let custom = |name: &str| {
        visible(name)
            && commands
                .iter()
                .find(|command| command.name.string_at("") == Some(name))
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

#[cfg(test)]
mod mod_skill_prompt_tests {
    use super::*;
    use hooks::mods::ModHost;
    use lingxi_core::host::CostSnapshot;
    use std::path::PathBuf;

    struct TestSession(PathBuf);

    #[async_trait::async_trait]
    impl ModSessionContext for TestSession {
        fn cwd(&self) -> PathBuf {
            self.0.clone()
        }

        fn root(&self) -> PathBuf {
            self.0.clone()
        }

        async fn model(&self) -> String {
            "claude-sonnet-4-5".into()
        }

        async fn id(&self) -> String {
            "skill-prompt-test-session".into()
        }

        async fn turns(&self) -> u64 {
            0
        }
    }

    struct TestOutput;

    #[async_trait::async_trait]
    impl OutputStream for TestOutput {
        async fn emit_text(&self, _text: &Utf16JsonProjection) {}

        async fn emit_tool_call(
            &self,
            _id: &lingxi_core::types::ToolUseId,
            _tool: &str,
            _input: &Utf16JsonProjection,
        ) {
        }

        async fn emit_tool_result(
            &self,
            _id: &lingxi_core::types::ToolUseId,
            _tool: &str,
            _model_text: &Utf16JsonProjection,
            _result: &Utf16JsonProjection,
            _mcp_meta: Option<&Utf16JsonProjection>,
        ) {
        }

        async fn emit_end_turn(&self, _stop_reason: &str, _cost: &CostSnapshot) {}
    }

    fn text_projection(units: &[u16]) -> Utf16JsonProjection {
        Utf16JsonProjection::root_string(String::from_utf16_lossy(units), units.to_vec()).unwrap()
    }

    fn transformer(registry: Arc<RwLock<HookRegistry>>) -> SkillPromptTransformer {
        let cwd = std::env::current_dir().unwrap();
        mod_skill_prompt_transformer(registry, Arc::new(TestSession(cwd)), Arc::new(TestOutput))
    }

    async fn load_mod(source: &str, storage_id: &str) -> (tempfile::TempDir, Arc<ModHost>) {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("register.js");
        std::fs::write(&module, source).unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load(
            storage_id,
            dir.path(),
            &module,
            Utf16JsonProjection::plain(json!({})),
        )
        .await
        .unwrap();
        (dir, host)
    }

    #[tokio::test]
    async fn absent_host_and_host_without_skill_prompt_are_identity() {
        let registry = Arc::new(RwLock::new(HookRegistry::new()));
        let transformer = transformer(registry.clone());
        let units: Vec<u16> = "Keep exact ".encode_utf16().chain([0xd800]).collect();
        let skill = text_projection(&"verify".encode_utf16().collect::<Vec<_>>());
        let text = text_projection(&units);
        let absent = transformer(skill.clone(), text.clone()).await.unwrap();
        assert_eq!(absent.string_units(""), Some(units.clone()));

        let (_dir, host) = load_mod(
            r#"export function register(on) {
              on('turn.step', async function* (_api, event, next) {
                const below = next(event);
                for await (const chunk of below) yield chunk;
                return await below.result;
              });
            }"#,
            "other-event",
        )
        .await;
        assert!(!host.has_event("skill.prompt"));
        registry.write().await.set_mod_host(host);
        let other_event = transformer(skill, text).await.unwrap();
        assert_eq!(other_event.string_units(""), Some(units));
    }

    #[tokio::test]
    async fn matching_mod_rewrites_exact_text_and_unload_is_live() {
        let registry = Arc::new(RwLock::new(HookRegistry::new()));
        let (_dir, host) = load_mod(
            r#"export function register(on) {
              on('skill.prompt', async (_api, event) => ({
                ...event,
                text: event.text + ' [mod]',
              }));
            }"#,
            "skill-prompt",
        )
        .await;
        assert!(host.has_event("skill.prompt"));
        let transformer = transformer(registry.clone());
        registry.write().await.set_mod_host(host.clone());
        let skill = text_projection(&"verify".encode_utf16().collect::<Vec<_>>());
        let input_units: Vec<u16> = "Before ".encode_utf16().chain([0xd800]).collect();
        let transformed = transformer(skill.clone(), text_projection(&input_units))
            .await
            .unwrap();
        let expected: Vec<u16> = input_units
            .iter()
            .copied()
            .chain(" [mod]".encode_utf16())
            .collect();
        assert_eq!(transformed.string_units(""), Some(expected));

        host.unload("skill-prompt").await.unwrap();
        assert!(!host.has_event("skill.prompt"));
        let after_unload = transformer(skill, text_projection(&input_units))
            .await
            .unwrap();
        assert_eq!(after_unload.string_units(""), Some(input_units));
    }

    #[tokio::test]
    async fn unmatched_skill_prompt_handler_keeps_the_original_body() {
        let registry = Arc::new(RwLock::new(HookRegistry::new()));
        let (_dir, host) = load_mod(
            r#"export function register(on) {
              on('skill.prompt', { skill: 'other-skill' }, (_api, event) => ({
                ...event,
                text: 'must not run',
              }));
            }"#,
            "unmatched-skill-prompt",
        )
        .await;
        assert!(host.has_event("skill.prompt"));
        registry.write().await.set_mod_host(host);
        let transformer = transformer(registry);
        let skill = text_projection(&"verify".encode_utf16().collect::<Vec<_>>());
        let input_units: Vec<u16> = "Original".encode_utf16().collect();
        let result = transformer(skill, text_projection(&input_units))
            .await
            .unwrap();
        assert_eq!(result.string_units(""), Some(input_units));
    }

    #[tokio::test]
    async fn matched_skill_prompt_handler_without_text_falls_back_to_original_body() {
        let registry = Arc::new(RwLock::new(HookRegistry::new()));
        let (_dir, host) = load_mod(
            r#"export function register(on) {
              on('skill.prompt', () => ({ skill: 'verify' }));
            }"#,
            "missing-skill-prompt-text",
        )
        .await;
        registry.write().await.set_mod_host(host);
        let transformer = transformer(registry);
        let skill = text_projection(&"verify".encode_utf16().collect::<Vec<_>>());
        let text = text_projection(&"Original".encode_utf16().collect::<Vec<_>>());
        let result = transformer(skill, text).await.unwrap();
        assert_eq!(
            result.string_units(""),
            Some("Original".encode_utf16().collect())
        );
    }

    #[tokio::test]
    async fn matched_skill_prompt_handler_with_non_string_text_falls_back_to_original_body() {
        let registry = Arc::new(RwLock::new(HookRegistry::new()));
        let (_dir, host) = load_mod(
            r#"export function register(on) {
              on('skill.prompt', () => ({ text: 42 }));
            }"#,
            "non-string-skill-prompt-text",
        )
        .await;
        registry.write().await.set_mod_host(host);
        let transformer = transformer(registry);
        let skill = text_projection(&"verify".encode_utf16().collect::<Vec<_>>());
        let text = text_projection(&"Original".encode_utf16().collect::<Vec<_>>());
        let result = transformer(skill, text).await.unwrap();
        assert_eq!(
            result.string_units(""),
            Some("Original".encode_utf16().collect())
        );
    }
}
