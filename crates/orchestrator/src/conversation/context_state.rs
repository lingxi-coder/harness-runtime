//! Conversation selections, effective query state, and the request inventory.
//!
//! The original transcript is retained. Profile changes rebuild the system
//! projection and retire scoped skill instructions at the outer query gate.

use super::ConversationOrchestrator;
use lingxi_core::host::orchestrator::{HandleError, OrchestratorHandle};
use lingxi_core::host::session_context::{
    ContextBlockKind, ContextBlockManifest, ContextManifest, ConversationMode, SessionModeSnapshot,
};
use lingxi_core::types::ConversationMessage;
use sha2::{Digest, Sha256};

/// A query and its detached profile-admission transaction jointly own this
/// lease. Dropping the caller cannot release the turn gate before admission
/// has finished durable and live publication.
pub(crate) type ProfileTurnGuard = std::sync::Arc<tokio::sync::OwnedMutexGuard<()>>;

#[derive(Default)]
pub(crate) struct ConversationContextRuntime {
    state: SessionModeSnapshot,
    active_epoch: u64,
    restored: bool,
    manifest: ContextManifest,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use lingxi_core::types::{utf16_json::Utf16JsonProjection as Source, MessageId};
    use std::sync::Arc;

    fn orchestrator_with_tools(tools: Arc<tool_api::ToolRegistry>) -> ConversationOrchestrator {
        ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            tools,
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
    }

    fn orchestrator() -> ConversationOrchestrator {
        orchestrator_with_tools(Arc::new(tool_api::ToolRegistry::new()))
    }

    #[tokio::test]
    async fn construction_fixes_mode_and_same_mode_check_does_not_change_epoch() {
        let code = orchestrator();
        let code_prompt = code.assemble_system_prompt_preview().await.unwrap();
        assert!(code_prompt.contains("command-line coding assistant"));
        let chat = orchestrator().with_conversation_mode(ConversationMode::Chat);
        let before = chat.conversation_profile().await;
        assert_eq!(
            chat.ensure_conversation_mode(ConversationMode::Chat)
                .await
                .unwrap(),
            before
        );
        assert!(chat
            .ensure_conversation_mode(ConversationMode::Code)
            .await
            .is_err());
        assert_eq!(chat.conversation_profile().await, before);
        assert_eq!(chat.profile_epoch().await, 0);
        let prompt = chat.assemble_system_prompt_preview().await.unwrap();
        assert!(prompt.contains("research, writing, learning"));
        assert!(!prompt.contains("command-line coding assistant"));
    }

    #[tokio::test]
    async fn same_mode_check_preserves_current_skill_context() {
        let orch = orchestrator();
        let skill_id = MessageId::new();
        let skill = ConversationMessage::user_meta(skill_id, "current skill".into())
            .with_origin(compaction::invoked_skills::skill_activation_origin(0));
        orch.session.lock().await.history.push(skill);
        let before = orch.conversation_profile().await;
        orch.ensure_conversation_mode(ConversationMode::Code)
            .await
            .unwrap();
        assert!(orch
            .ensure_conversation_mode(ConversationMode::Chat)
            .await
            .is_err());
        assert_eq!(orch.conversation_profile().await, before);
        assert_eq!(orch.profile_epoch().await, 0);
        assert!(orch
            .session
            .lock()
            .await
            .model_context_history()
            .iter()
            .any(|row| row.id() == skill_id));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn captured_profile_keeps_mode_and_instruction_epoch_without_a_writer() {
        let _registry_guard = compaction::invoked_skills::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let session_id = lingxi_core::types::SessionId::new();
        let old_id = MessageId::new();
        let current_id = MessageId::new();
        let user_id = MessageId::new();
        let excluded_id = MessageId::new();
        let sid = session_id.as_uuid();
        let rows = [
            serde_json::json!({"type":"fixed-session-mode","sessionId":sid,"sessionMode":"chat","instructionEpoch":2}),
            serde_json::json!({"type":"user","uuid":old_id.as_uuid(),"sessionId":sid,"parentUuid":null,"isMeta":true,"origin":{"type":"skill_activation","context_epoch":1,"skill_name":"old"},"message":{"role":"user","content":"same text"}}),
            serde_json::json!({"type":"user","uuid":current_id.as_uuid(),"sessionId":sid,"parentUuid":old_id.as_uuid(),"isMeta":true,"origin":{"type":"skill_activation","context_epoch":2,"skill_name":"current"},"message":{"role":"user","content":"current instruction"}}),
            serde_json::json!({"type":"user","uuid":user_id.as_uuid(),"sessionId":sid,"parentUuid":current_id.as_uuid(),"message":{"role":"user","content":"same text"}}),
            serde_json::json!({"type":"user","uuid":excluded_id.as_uuid(),"sessionId":sid,"parentUuid":user_id.as_uuid(),"isModelContextExcluded":true,"message":{"role":"user","content":"excluded evidence"}}),
        ];
        let source = rows
            .iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>();
        let loaded = session::jsonl::route_lines(&source);
        let (messages, _) = session::jsonl::build_conversation_chain(&loaded, &sid.to_string());
        let orch = orchestrator()
            .with_conversation_mode(ConversationMode::Chat)
            .with_session_id(session_id);
        orch.restore_captured_session_history(
            sid,
            &messages,
            &loaded.messages_in_order,
            false,
            permission::PermissionMode::Default,
            ConversationMode::Chat,
            &source,
        )
        .await
        .unwrap();

        let mode = orch.conversation_profile().await;
        assert_eq!(mode.mode, ConversationMode::Chat);
        assert_eq!(orch.profile_epoch().await, 2);
        let history = orch.session.lock().await;
        assert_eq!(history.history.len(), 4);
        let model = history.model_context_history();
        assert!(model.iter().any(|row| row.id() == current_id));
        assert!(model.iter().any(|row| row.id() == user_id));
        assert!(!model
            .iter()
            .any(|row| row.id() == old_id || row.id() == excluded_id));
        drop(history);
        let scoped_id = session_id.to_string();
        let active = compaction::invoked_skills::filter_for_scope(
            compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&scoped_id), None),
        );
        assert!(active
            .iter()
            .any(|skill| skill.name.string_at("") == Some("current")));

        orch.ensure_conversation_mode(ConversationMode::Chat)
            .await
            .unwrap();
        assert!(orch
            .ensure_conversation_mode(ConversationMode::Code)
            .await
            .is_err());
        assert_eq!(orch.profile_epoch().await, 2);
        let model = orch.session.lock().await.model_context_history();
        assert!(model.iter().any(|row| row.id() == current_id));
        assert!(model.iter().any(|row| row.id() == user_id));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn conversation_profiles_initial_restore_preserves_current_epoch_skill() {
        let _registry_guard = compaction::invoked_skills::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let orch = orchestrator();
        let id = MessageId::new();
        let mut row = ConversationMessage::user_meta(id, "Current task guidance".to_string());
        if let ConversationMessage::User { origin, .. } = &mut row {
            *origin = Some(
                compaction::invoked_skills::skill_activation_origin_with_skill(
                    0,
                    &Source::from("current"),
                    std::path::Path::new("/skills/current"),
                ),
            );
        }
        orch.session.lock().await.history.push(row);
        orch.restore_conversation_profile(ConversationMode::Code)
            .await
            .unwrap();
        assert!(orch
            .session
            .lock()
            .await
            .model_context_history()
            .iter()
            .any(|row| row.id() == id));
        let scope_session = orch.current_session_id().await.to_string();
        let active = compaction::invoked_skills::filter_for_scope(
            compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&scope_session), None),
        );
        assert!(active
            .iter()
            .any(|skill| skill.name.string_at("") == Some("current")));
    }
}

impl ConversationContextRuntime {
    fn bind(&mut self, session_id: String) {
        if self.state.session_id != session_id {
            self.state = SessionModeSnapshot {
                session_id,
                mode: self.state.mode,
            };
            self.active_epoch = 0;
            self.restored = false;
            self.manifest = ContextManifest::default();
        }
    }
}

fn block(
    id: String,
    source: &str,
    kind: ContextBlockKind,
    scope: &str,
    reason: &str,
    bytes: &[u8],
) -> ContextBlockManifest {
    ContextBlockManifest {
        id,
        source: source.into(),
        version: format!("{:x}", Sha256::digest(bytes)),
        kind,
        scope: scope.into(),
        reason: reason.into(),
        bytes: bytes.len() as u64,
    }
}

impl ConversationOrchestrator {
    /// Initialize a host's profile without changing execution permissions.
    #[must_use]
    pub fn with_conversation_mode(self, mode: ConversationMode) -> Self {
        // Builders run before the orchestrator is shared with an executor.
        let mut context = self
            .conversation_context
            .try_lock()
            .expect("unshared profile builder");
        context.state.mode = mode;
        drop(context);
        self
    }

    pub(crate) async fn conversation_profile(&self) -> SessionModeSnapshot {
        // Hooks may inspect the effective profile while the session activation
        // transaction owns the administrative gate. Read the identity only
        // after obtaining the context guard, so an old read cannot rebind a
        // freshly restored snapshot to a previous session.
        let mut context = self.conversation_context.lock().await;
        let session_id = self.current_session_id().await.as_uuid().to_string();
        context.bind(session_id);
        context.state.clone()
    }

    /// Verify the host-selected mode without changing the owning session.
    pub async fn ensure_conversation_mode(
        &self,
        mode: ConversationMode,
    ) -> Result<SessionModeSnapshot, HandleError> {
        let snapshot = self.conversation_profile().await;
        if snapshot.mode != mode {
            return Err(HandleError::ActionFailed(
                "session mode is fixed at creation".into(),
            ));
        }
        Ok(snapshot)
    }

    /// Persist and validate the mode chosen by the unshared builder. This is
    /// called before startup hooks, after installing the UUID and its writer.
    pub async fn initialize_conversation_mode(&self) -> Result<SessionModeSnapshot, HandleError> {
        let _turn = self.turn_gate.lock().await;
        let _transition = self.conversation_transition.lock().await;
        self.initialize_conversation_mode_locked().await
    }

    /// The session-switch owner already holds the turn and transition gates.
    pub(crate) async fn initialize_conversation_mode_locked(
        &self,
    ) -> Result<SessionModeSnapshot, HandleError> {
        let session_id = self.current_session_id().await.as_uuid().to_string();
        let (expected, instruction_epoch) = {
            let mut context = self.conversation_context.lock().await;
            context.bind(session_id.clone());
            (context.state.mode, context.active_epoch)
        };
        if let Some(writer) = &self.transcript.jsonl_writer {
            if writer
                .active_path()
                .file_stem()
                .and_then(|name| name.to_str())
                .and_then(|stem| uuid::Uuid::parse_str(stem).ok())
                != uuid::Uuid::parse_str(&session_id).ok()
            {
                return Err(HandleError::ActionFailed(
                    "session changed before mode initialization".into(),
                ));
            }
            writer
                .initialize_fixed_session_mode(&session_id, expected, instruction_epoch)
                .await
                .map_err(|error| {
                    HandleError::ActionFailed(format!("persist fixed session mode: {error}"))
                })?;
        }
        self.restore_conversation_profile_state_locked(expected, instruction_epoch)
            .await
    }

    pub(crate) async fn restore_conversation_profile(
        &self,
        mode: ConversationMode,
    ) -> Result<SessionModeSnapshot, HandleError> {
        let _turn = self.turn_gate.lock().await;
        let _transition = self.conversation_transition.lock().await;
        self.restore_conversation_profile_locked(mode).await
    }

    /// Restore the already validated fixed mode from a captured transcript.
    pub(crate) async fn restore_captured_conversation_profile(
        &self,
        mode: ConversationMode,
        instruction_epoch: u64,
    ) -> Result<SessionModeSnapshot, HandleError> {
        let _turn = self.turn_gate.lock().await;
        let _transition = self.conversation_transition.lock().await;
        self.restore_conversation_profile_state_locked(mode, instruction_epoch)
            .await
    }

    /// Restore inside the transaction that publishes the mounted session.
    pub(crate) async fn restore_conversation_profile_locked(
        &self,
        mode: ConversationMode,
    ) -> Result<SessionModeSnapshot, HandleError> {
        let instruction_epoch = if let Some(writer) = &self.transcript.jsonl_writer {
            let persisted = writer
                .persisted_fixed_session_profile()
                .await
                .map_err(HandleError::ActionFailed)?
                .ok_or_else(|| HandleError::ActionFailed("missing fixed session mode".into()))?;
            if persisted.mode != mode {
                return Err(HandleError::ActionFailed(
                    "persisted session mode does not match the initialized session".into(),
                ));
            }
            persisted.instruction_epoch
        } else {
            self.profile_epoch().await
        };
        self.restore_conversation_profile_state_locked(mode, instruction_epoch)
            .await
    }

    async fn restore_conversation_profile_state_locked(
        &self,
        mode: ConversationMode,
        instruction_epoch: u64,
    ) -> Result<SessionModeSnapshot, HandleError> {
        let session_id = self.current_session_id().await.as_uuid().to_string();
        let mut context = self.conversation_context.lock().await;
        let same_session = context.state.session_id == session_id;
        if context.restored
            && same_session
            && context.state.mode == mode
            && context.active_epoch == instruction_epoch
        {
            return Ok(context.state.clone());
        }
        let epoch = instruction_epoch;
        let registry_session_id = self.current_session_id().await.to_string();
        let registry_scope =
            compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&registry_session_id), None);
        let retire = (same_session
            && (context.state.mode != mode || context.active_epoch != epoch))
            || (!compaction::invoked_skills::filter_for_scope(registry_scope).is_empty()
                && compaction::invoked_skills::context_epoch(registry_scope) != epoch);
        self.reset_profile_projection(&session_id, epoch, retire, true)
            .await;
        self.restore_effective_skill_activations(epoch).await;
        context.active_epoch = epoch;
        context.restored = true;
        context.state = SessionModeSnapshot { session_id, mode };
        context.manifest = ContextManifest::default();
        Ok(context.state.clone())
    }

    /// Called once per admitted outer query, before any prompt is assembled.
    pub(crate) async fn admit_conversation_profile(
        &self,
        _turn_guard: &ProfileTurnGuard,
    ) -> Result<(), HandleError> {
        let session_id = self.current_session_id().await;
        let mut context = self.conversation_context.lock().await;
        context.bind(session_id.as_uuid().to_string());
        let scope_id = session_id.to_string();
        compaction::invoked_skills::set_context_epoch(
            compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&scope_id), None),
            context.active_epoch,
        );
        Ok(())
    }

    /// The caller owns turn_gate. Permission grants and existing background
    /// owners are independent of this model-context transition.
    async fn reset_profile_projection(
        &self,
        _session_id: &str,
        epoch: u64,
        retire: bool,
        preserve_matching_prompt: bool,
    ) {
        // The cached prefix contains both profile instructions and skill
        // messages. Side queries can consume it without rebuilding history, so
        // it must not outlive the profile that produced it.
        self.cancel_idle_compaction();
        if let Some(slot) = self.model_runtime.cache_safe_slot.as_ref() {
            slot.invalidate().await;
        }
        if retire {
            if let Some(handle) = self.compaction_runtime.session_memory.as_ref() {
                // A previously launched extraction owns its old request, but
                // must not publish that result into the new profile's memory.
                // The commit path holds this lock while checking generation.
                let _extractor = handle.extractor.lock().await;
                handle
                    .generation
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            }
        }
        // Skill scopes use the engine's prefixed SessionId; client events and
        // transcript filenames use its canonical UUID.
        let scoped_session_id = self.current_session_id().await.to_string();
        let session_id = scoped_session_id.as_str();
        let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(session_id), None);
        if retire {
            compaction::invoked_skills::expire_for_mode_change(session_id);
        }
        compaction::invoked_skills::set_context_epoch(scope, epoch);
        let retired = compaction::invoked_skills::expired_message_ids_for_scope(
            compaction::invoked_skills::InvokedSkillScopeRef::new(Some(session_id), None),
        );
        let restored_skill_rows = self
            .transcript
            .post_compact_skill_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .copied()
            .collect::<Vec<_>>();
        {
            let mut session = self.session.lock().await;
            session.model_context_excluded_messages.extend(retired);
            if retire {
                let legacy_restored = restored_skill_rows.into_iter().filter(|id| {
                    session.history.iter().find(|row| row.id() == *id).is_none_or(|row| !matches!(row, ConversationMessage::User { origin: Some(origin), .. } if origin.string_at("/type") == Some("skill_activation")))
                }).collect::<Vec<_>>();
                session
                    .model_context_excluded_messages
                    .extend(legacy_restored);
            }
            // Only host-owned envelope provenance is inspected. Identical text
            // quoted in an attachment or tool result is never reclassified.
            let obsolete = session
                .history
                .iter()
                .filter_map(|message| match message {
                    ConversationMessage::User {
                        id,
                        origin: Some(origin),
                        ..
                    } if origin.string_at("/type") == Some("skill_activation") => {
                        let marker = origin
                            .to_json_string()
                            .ok()
                            .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok());
                        let previous_epoch = marker
                            .as_ref()
                            .and_then(|value| value.get("context_epoch"))
                            .and_then(serde_json::Value::as_u64);
                        (previous_epoch != Some(epoch)).then_some(*id)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            session.model_context_excluded_messages.extend(obsolete);
        }
        let snapshot_epoch = *self.prompt_runtime.prompt_snapshot_epoch.lock().await;
        if !preserve_matching_prompt || snapshot_epoch.unwrap_or(0) != epoch {
            *self.prompt_runtime.prompt_snapshot.lock().await = None;
            *self.prompt_runtime.context_rendering_hint.lock().await = None;
            *self.prompt_runtime.prompt_snapshot_epoch.lock().await = None;
        }
        *self.prompt_runtime.current_turn_system_prompt.lock().await = None;
        *self
            .prompt_runtime
            .pending_prompt_source_vector
            .lock()
            .await = None;
        if !preserve_matching_prompt {
            self.prompt_runtime
                .prompt_snapshot_resume
                .store(false, std::sync::atomic::Ordering::Release);
        }
        let mut cache = self.prompt_runtime.mod_prompt_sections.lock().await;
        cache.generation = cache.generation.wrapping_add(1);
        cache.context = None;
        cache.answers.clear();
    }

    pub(crate) async fn current_context_manifest(&self) -> ContextManifest {
        let state = self.conversation_profile().await;
        let context = self.conversation_context.lock().await;
        let mut manifest = context.manifest.clone();
        manifest.projected &= manifest.session == state;
        manifest.session = state;
        manifest
    }

    pub(crate) async fn profile_epoch(&self) -> u64 {
        self.conversation_context.lock().await.active_epoch
    }

    pub(crate) async fn stamp_skill_prompt_activation(
        &self,
        input: &super::QueuedPromptInput,
        message: &mut ConversationMessage,
    ) {
        let Some(skill) = input.skill_identity.as_ref() else {
            return;
        };
        let session_id = self.current_session_id().await.to_string();
        let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_id), None);
        let epoch = compaction::invoked_skills::context_epoch(scope);
        let activation = compaction::invoked_skills::skill_activation_origin_with_skill(
            epoch,
            &skill.name,
            &skill.path,
        );
        if let ConversationMessage::User { origin, .. } = message {
            *origin = Some(activation);
        }
    }

    /// Passive rehydration of host-stamped instructions. No loader, hook,
    /// embedded command, setup step or user action is executed here.
    async fn restore_effective_skill_activations(&self, epoch: u64) {
        use lingxi_core::types::{utf16_json::Utf16JsonProjection as Source, ContentBlock};
        let session_id = self.current_session_id().await.to_string();
        let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_id), None);
        let history = self.session.lock().await.model_context_history();
        let attachments = self
            .transcript
            .post_compact_skill_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for message in history {
            let accepted_message = message.clone();
            let ConversationMessage::User {
                id,
                content,
                origin: Some(origin),
                ..
            } = message
            else {
                continue;
            };
            if origin.string_at("/type") != Some("skill_activation") {
                continue;
            }
            let marker_epoch = origin
                .to_json_string()
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .and_then(|value| {
                    value
                        .get("context_epoch")
                        .and_then(serde_json::Value::as_u64)
                });
            if marker_epoch != Some(epoch) {
                continue;
            }
            if let Ok(name) = origin.subprojection("/skill_name") {
                if compaction::invoked_skills::register_accepted_skill_message(
                    &accepted_message,
                    scope,
                ) {
                    continue;
                }
                let units = content
                    .iter()
                    .flat_map(|block| match block {
                        ContentBlock::Text { text, .. } => text.encode_utf16().collect::<Vec<_>>(),
                        ContentBlock::TextJsUtf16 {
                            utf16_code_units, ..
                        } => utf16_code_units.clone(),
                        _ => Vec::new(),
                    })
                    .collect::<Vec<_>>();
                let body = Source::root_string(String::from_utf16_lossy(&units), units)
                    .expect("restored skill source is text");
                let path = origin.string_at("/skill_path").unwrap_or("");
                compaction::invoked_skills::register_activation_with_message(
                    &name,
                    std::path::Path::new(path),
                    &body,
                    None,
                    scope,
                    id,
                );
            } else if let Some(bodies) = attachments.get(&id) {
                for (index, body) in bodies.iter().enumerate() {
                    if let Ok(name) = origin.subprojection(&format!("/skills/{index}/name")) {
                        let path = origin
                            .string_at(&format!("/skills/{index}/path"))
                            .unwrap_or("");
                        compaction::invoked_skills::register_restored_skill(
                            &name,
                            std::path::Path::new(path),
                            body,
                            origin.string_at(&format!("/skills/{index}/definition_sha256")),
                            scope,
                            id,
                        );
                    }
                }
            }
        }
    }

    pub(crate) async fn record_context_projection(
        &self,
        system: Option<&lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput>,
        history: &[ConversationMessage],
        tools: &[lingxi_core::host::SourceToolWireDefinition],
    ) {
        let session = self.conversation_profile().await;
        let mut blocks = Vec::with_capacity(history.len() + 2);
        if let Some(system) = system {
            let source = Self::system_prompt_source_text(system);
            if let Ok(bytes) = source.to_json_string() {
                blocks.push(block(
                    "system".into(),
                    "host_prompt_assembler",
                    ContextBlockKind::Instruction,
                    "session",
                    "current behavior, user/project rules and host guidance",
                    bytes.as_bytes(),
                ));
            }
        }
        for tool in tools {
            let bytes = format!(
                "{}\n{}\n{}\n{}",
                tool.description.to_json_string().unwrap_or_default(),
                tool.input_schema.to_json_string().unwrap_or_default(),
                tool.strict,
                tool.defer_loading
            );
            blocks.push(block(
                format!("tool:{}", tool.name),
                "eligible_tool_registry",
                ContextBlockKind::Capability,
                "request",
                if tool.defer_loading {
                    "deferred schema; discover when needed"
                } else {
                    "schema present; execution still requires authorization"
                },
                bytes.as_bytes(),
            ));
        }
        let injected = self.session.lock().await.injected_message_sources.clone();
        for message in history {
            let (source, kind) = match message {
                ConversationMessage::User {
                    origin: Some(origin),
                    ..
                } if origin.string_at("/type") == Some("skill_activation") => {
                    ("active_skill", ContextBlockKind::Instruction)
                }
                ConversationMessage::User {
                    content,
                    is_meta: false,
                    id,
                    ..
                } if !injected.contains_key(id)
                    && !content.iter().any(|block| {
                        matches!(block, lingxi_core::types::ContentBlock::ToolResult { .. })
                    }) =>
                {
                    ("user_request", ContextBlockKind::Instruction)
                }
                ConversationMessage::User { is_meta: false, .. } => {
                    ("user_or_tool_input", ContextBlockKind::Evidence)
                }
                ConversationMessage::User { .. } => {
                    ("host_context_attachment", ContextBlockKind::State)
                }
                ConversationMessage::Assistant { .. } => {
                    ("model_output", ContextBlockKind::Evidence)
                }
                ConversationMessage::System { .. } => ("host_state", ContextBlockKind::State),
            };
            if let Ok(bytes) = serde_json::to_vec(message) {
                blocks.push(block(
                    format!("message:{}", message.id()),
                    source,
                    kind,
                    "conversation",
                    "retained in the prepared model projection",
                    &bytes,
                ));
            }
        }
        if blocks.len() > 384 {
            let remaining = blocks.split_off(383);
            let total_bytes = remaining.iter().map(|block| block.bytes).sum::<u64>();
            let versions = remaining
                .iter()
                .map(|block| block.version.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let mut aggregate = block(
                "additional_context".into(),
                "prepared_projection",
                ContextBlockKind::State,
                "request",
                &format!(
                    "{} additional blocks summarized by fingerprint",
                    remaining.len()
                ),
                versions.as_bytes(),
            );
            aggregate.bytes = total_bytes;
            blocks.push(aggregate);
        }
        self.conversation_context.lock().await.manifest = ContextManifest {
            projected: true,
            session,
            blocks,
        };
    }
}
