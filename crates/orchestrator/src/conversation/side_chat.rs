//! Fixed, legal parent model context for lightweight RAM-only children.
use super::*;
use lingxi_core::types::ContentBlock;

/// A creation-time snapshot. Reading it never waits for the parent's turn gate
/// and never consumes its queued peer reports.
#[derive(Clone)]
pub struct SideChatParentSnapshot {
    /// Model and request configuration frozen at creation.
    pub config: OrchestratorConfig,
    /// Longest legal committed model-context prefix.
    pub history: Vec<ConversationMessage>,
    /// Provider profile paired with the acknowledged model.
    pub model_profile: Option<String>,
    /// Configured source identity and its route anchor at snapshot time.
    pub configured_model_source: Option<lingxi_core::host::ModelSourceBinding>,
    /// Raw source binding stored on the session at snapshot time.
    pub user_model_setting: Option<lingxi_core::host::ModelSourceBinding>,
    /// Whether configured state or an explicit session selection owns source
    /// projection for the snapshot's main route.
    pub user_model_setting_source: lingxi_core::session::SessionModelSettingSource,
    /// Acknowledged reasoning selection.
    pub reasoning: lingxi_core::host::ReasoningSelection,
    /// Acknowledged permission control mode.
    pub permission_mode: String,
    /// Independent initial working directory.
    pub cwd: std::path::PathBuf,
    /// Fixed parent instruction-file context kept in RAM.
    pub memory_files: Vec<crate::prompt::MemoryFile>,
}

/// Keep the longest prefix containing complete tool-use/result groups. An
/// assistant row is committed atomically, but its tools may still be running.
fn legal_prefix(history: &[ConversationMessage]) -> Vec<ConversationMessage> {
    let mut pending = HashSet::new();
    let mut complete = 0;
    for (index, message) in history.iter().enumerate() {
        match message {
            ConversationMessage::Assistant { content, .. } => {
                for block in content {
                    if let ContentBlock::ToolUse { id, .. } = block {
                        pending.insert(id.clone());
                    }
                }
            }
            ConversationMessage::User { content, .. } => {
                for block in content {
                    if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                        // Orphan results cannot be admitted to a fresh provider context.
                        if !pending.remove(tool_use_id) {
                            return history[..complete].to_vec();
                        }
                    }
                }
            }
            _ => {}
        }
        if pending.is_empty() {
            complete = index + 1;
        }
    }
    history[..complete].to_vec()
}

fn prompt_snapshot_system_source_text(
    snapshot: &lingxi_core::host::PromptSnapshot,
) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
    let boundary = lingxi_llm_client::providers::anthropic::system_prompt::DYNAMIC_BOUNDARY
        .encode_utf16()
        .collect::<Vec<_>>();
    let mut units = Vec::new();
    let mut joined = false;
    for (index, display) in snapshot.system_prompt.iter().enumerate() {
        let section = snapshot
            .system_prompt_utf16
            .get(index)
            .and_then(Option::as_ref)
            .cloned()
            .unwrap_or_else(|| display.encode_utf16().collect());
        if section == boundary {
            continue;
        }
        if joined {
            units.extend("\n\n".encode_utf16());
        }
        units.extend(section);
        joined = true;
    }
    lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
        String::from_utf16_lossy(&units),
        units,
    )
    .expect("joined prompt snapshot source is a root string")
}

impl ConversationOrchestrator {
    /// Restrict child model selection to ready host routes and its current route.
    pub fn with_side_chat_model_refs(mut self, mut models: Vec<String>) -> Self {
        models.sort();
        models.dedup();
        self.side_chat_model_refs = Some(models);
        self
    }

    /// Capture context without waiting for the parent turn gate or draining reports.
    pub async fn side_chat_parent_snapshot(&self) -> SideChatParentSnapshot {
        let session = self.session.lock().await;
        let configured_model_source = self
            .model_runtime
            .configured_model_source
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut config = self.config.clone();
        config.model = session.model.clone();
        config.effort = self
            .model_runtime
            .current_effort
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut snapshot = SideChatParentSnapshot {
            config,
            history: legal_prefix(&session.model_context_history()),
            model_profile: session.model_profile.clone(),
            configured_model_source,
            user_model_setting: session.user_model_setting.clone(),
            user_model_setting_source: session.user_model_setting_source,
            reasoning: self.current_reasoning_selection(),
            permission_mode: self
                .perms
                .permission_mode()
                .unwrap_or_else(|| "default".into()),
            cwd: self.current_cwd(),
            memory_files: Vec::new(),
        };
        drop(session);
        if let Some(prompt) = self.prompt_runtime.prompt_snapshot.lock().await.as_ref() {
            snapshot.config.system_prompt_override =
                Some(prompt_snapshot_system_source_text(prompt));
        }
        snapshot.memory_files = self.memory.load(&snapshot.cwd).await;
        snapshot
    }

    /// Seed only model context. The child UI replay is independently owned by
    /// its event sink, so inherited messages never appear in its timeline.
    pub async fn seed_side_chat(&self, snapshot: &SideChatParentSnapshot) -> Result<(), String> {
        if !self.side_chat || self.has_jsonl_writer() {
            return Err("side context requires an isolated RAM-only orchestrator".into());
        }
        let mut session = self.session.lock().await;
        if !session.history.is_empty() {
            return Err("side context is already seeded".into());
        }
        session.history = snapshot.history.clone();
        session.history.push(ConversationMessage::user_meta(MessageId::new(),
            "This is an independent side conversation with a fixed parent context. Only this conversation's advertised tools and inline skills are available. Background tasks, agents, teams, workflows, MCP, hooks, worktrees, and nested /side are unavailable. User file changes remain after this conversation closes.".into()));
        session.model = snapshot.config.model.clone();
        session.model_profile = snapshot.model_profile.clone();
        session.user_model_setting = snapshot.user_model_setting.clone();
        session.user_model_setting_source = snapshot.user_model_setting_source;
        drop(session);
        *self
            .model_runtime
            .configured_model_source
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            snapshot.configured_model_source.clone();
        self.initialize_reasoning_selection_for_model(
            &snapshot.config.model,
            snapshot.model_profile.as_deref(),
            snapshot.reasoning.clone(),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use llm_runtime::{
        HistoryResponse, LlmError, MediaRoute, PricingModelRef, ProviderId, ResolvedRoute,
    };
    use std::sync::Arc;

    struct NativeSourceRouteApi;

    #[async_trait]
    impl OrchestratorApiClient for NativeSourceRouteApi {
        async fn messages_create(
            &self,
            _request: OrchestratorApiRequest,
        ) -> Result<HistoryResponse, LlmError> {
            Err(LlmError::ModelUnavailable)
        }

        fn resolve_media_route(
            &self,
            model: &str,
            profile: Option<&str>,
        ) -> Result<MediaRoute, LlmError> {
            let provider_id = ProviderId::AnthropicFirstParty;
            let profile_name = profile.unwrap_or("side-chat-profile").to_owned();
            Ok(MediaRoute {
                main: ResolvedRoute {
                    provider_id: provider_id.clone(),
                    profile_name,
                    request_model: model.to_owned(),
                    display_model: model.to_owned(),
                    pricing_model: PricingModelRef {
                        pricing_provider_id: provider_id,
                        billing_model: model.to_owned(),
                        request_model: model.to_owned(),
                        display_model: model.to_owned(),
                    },
                    capabilities: llm_runtime::Capabilities::default(),
                    connection_chain: Vec::new(),
                    failover: Default::default(),
                },
                vision_delegate: None,
            })
        }
    }

    fn new_orchestrator(side_chat: bool) -> ConversationOrchestrator {
        let mut config = OrchestratorConfig::default();
        config.model = "claude-side-chat-model".into();
        let api: Arc<dyn OrchestratorApiClient> = Arc::new(NativeSourceRouteApi);
        let streaming_api: Arc<dyn StreamingApiClient> =
            Arc::new(crate::test_support::MockStreamingApiClient::empty());
        let tools = Arc::new(tool_api::registry::ToolRegistry::new());
        let hooks = crate::test_support::noop_hook_executor();
        let perms = Arc::new(crate::test_support::NoOpPermissionGate);
        let output = Arc::new(crate::test_support::MockOutputStream::new());
        let memory = Arc::new(crate::test_support::StaticMemoryProvider::empty());
        let cwd = std::env::temp_dir();
        if side_chat {
            ConversationOrchestrator::new_side_chat(
                config,
                api,
                streaming_api,
                tools,
                hooks,
                perms,
                output,
                memory,
                cwd,
            )
        } else {
            ConversationOrchestrator::new_with_streaming(
                config,
                api,
                streaming_api,
                tools,
                hooks,
                perms,
                output,
                memory,
                cwd,
            )
        }
    }

    fn binding(label: &str) -> lingxi_core::host::ModelSourceBinding {
        let units = label.encode_utf16().chain([0xD800]).collect::<Vec<_>>();
        let source = Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
            .expect("lone-surrogate model-source projection");
        lingxi_core::host::ModelSourceBinding {
            source: source.clone(),
            effective_source: source,
            route_anchor: lingxi_core::host::ModelRouteAnchor {
                model: "claude-side-chat-model".into(),
                profile: Some("side-chat-profile".into()),
            },
        }
    }

    #[tokio::test]
    async fn side_chat_snapshot_freezes_model_source_bindings_and_keeps_route_anchor() {
        use lingxi_core::session::SessionModelSettingSource::{Configured, UserSelection};

        for owner in [Configured, UserSelection] {
            let parent = new_orchestrator(false);
            {
                let mut session = parent.session.lock().await;
                session.model_profile = Some("side-chat-profile".into());
                session.user_model_setting_source = owner;
                session.user_model_setting = Some(binding(if owner == Configured {
                    "configured-active"
                } else {
                    "user-active"
                }));
            }
            let original_configured = binding("configured-original");
            *parent
                .model_runtime
                .configured_model_source
                .write()
                .expect("configured source lock") = Some(original_configured.clone());

            let snapshot = parent.side_chat_parent_snapshot().await;
            let original_user = snapshot.user_model_setting.clone();
            let original_owner = snapshot.user_model_setting_source;
            assert_eq!(
                snapshot.configured_model_source,
                Some(original_configured.clone())
            );
            assert_eq!(original_owner, owner);

            // Changes to the parent after capture must not leak into the child.
            let changed_parent_configured = binding("configured-parent-updated");
            *parent
                .model_runtime
                .configured_model_source
                .write()
                .expect("configured source lock") = Some(changed_parent_configured.clone());
            let changed_parent_user = binding("user-parent-updated");
            parent.session.lock().await.user_model_setting = Some(changed_parent_user.clone());

            let child = new_orchestrator(true);
            child.seed_side_chat(&snapshot).await.expect("seed child");
            assert_eq!(
                *child
                    .model_runtime
                    .configured_model_source
                    .read()
                    .expect("child configured source lock"),
                Some(original_configured.clone()),
            );
            {
                let session = child.session.lock().await;
                assert_eq!(session.user_model_setting, original_user);
                assert_eq!(session.user_model_setting_source, original_owner);
            }

            let expected = if owner == Configured {
                original_configured.effective_source.string_units("")
            } else {
                original_user
                    .as_ref()
                    .and_then(|user| user.effective_source.string_units(""))
            };
            let attached = child
                .main_model_source_for_request("claude-side-chat-model", Some("side-chat-profile"))
                .await
                .expect("active source requires a raw model-source carrier");
            assert_eq!(attached.string_units(""), expected);
            assert!(
                child
                    .main_model_source_for_request(
                        "claude-side-chat-fallback",
                        Some("side-chat-profile"),
                    )
                    .await
                    .is_none(),
                "a different route cannot inherit the snapshot source"
            );

            // Mutating the child must not write through to either parent owner.
            let child_change = binding("child-source-change");
            *child
                .model_runtime
                .configured_model_source
                .write()
                .expect("child configured source lock") = Some(child_change.clone());
            child.session.lock().await.user_model_setting = Some(child_change);
            assert_eq!(
                *parent
                    .model_runtime
                    .configured_model_source
                    .read()
                    .expect("parent configured source lock"),
                Some(changed_parent_configured),
            );
            assert_eq!(
                parent.session.lock().await.user_model_setting,
                Some(changed_parent_user),
            );
        }
    }

    #[tokio::test]
    async fn side_chat_snapshot_keeps_exact_prompt_snapshot_source_units() {
        let parent = new_orchestrator(false);
        let mut exact_units = "second static section".encode_utf16().collect::<Vec<_>>();
        exact_units.push(0xD800);
        *parent.prompt_runtime.prompt_snapshot.lock().await =
            Some(lingxi_core::host::PromptSnapshot {
                system_prompt: vec![
                    "first static section".into(),
                    lingxi_llm_client::providers::anthropic::system_prompt::DYNAMIC_BOUNDARY.into(),
                    String::from_utf16_lossy(&exact_units),
                ],
                system_prompt_utf16: vec![None, None, Some(exact_units.clone())],
                ..Default::default()
            });

        let snapshot = parent.side_chat_parent_snapshot().await;
        let source = snapshot
            .config
            .system_prompt_override
            .expect("prompt snapshot becomes a child system prompt");
        let expected = "first static section\n\n"
            .encode_utf16()
            .chain(exact_units)
            .collect::<Vec<_>>();
        assert_eq!(source.string_units(""), Some(expected));
    }
}
