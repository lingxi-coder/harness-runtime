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
            snapshot.config.system_prompt_override = Some(prompt.system_prompt.join("\n\n"));
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
        drop(session);
        self.initialize_reasoning_selection_for_model(
            &snapshot.config.model,
            snapshot.model_profile.as_deref(),
            snapshot.reasoning.clone(),
        );
        Ok(())
    }
}
