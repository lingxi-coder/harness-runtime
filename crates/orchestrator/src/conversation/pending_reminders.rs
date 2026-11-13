//! Transient producer results remain pending until a successful model step
//! actually used their message identities. They never enter durable history.

use super::ConversationOrchestrator;
use crate::request_context::PromptListingMembership;
use lingxi_core::types::ConversationMessage;
use std::sync::atomic::Ordering;

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Acknowledgement {
    Listing(PromptListingMembership),
    Memory(memory::surfacing::SurfacedMemory),
    Skills(Vec<skill::DiscoveredSkill>),
}

#[derive(Clone)]
pub(crate) struct PendingReminder {
    kind: &'static str,
    generation: u64,
    message: ConversationMessage,
    acknowledgement: Acknowledgement,
}

impl Acknowledgement {
    fn same_producer(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Memory(a), Self::Memory(b)) => a.path == b.path,
            (Self::Skills(_), Self::Skills(_)) => true,
            (
                Self::Listing(PromptListingMembership::Skills { .. }),
                Self::Listing(PromptListingMembership::Skills { .. }),
            ) => true,
            (
                Self::Listing(PromptListingMembership::Agents { .. }),
                Self::Listing(PromptListingMembership::Agents { .. }),
            ) => true,
            _ => false,
        }
    }
}

impl ConversationOrchestrator {
    pub(crate) fn pending_step_acknowledgements(
        &self,
        messages: &[ConversationMessage],
    ) -> Vec<PendingReminder> {
        let ids = messages
            .iter()
            .map(ConversationMessage::id)
            .collect::<std::collections::HashSet<_>>();
        self.prompt_runtime
            .pending_step_reminders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|row| ids.contains(&row.message.id()))
            .cloned()
            .collect()
    }

    pub(crate) fn restore_step_acknowledgements(&self, rows: Vec<PendingReminder>) {
        let generation = self
            .prompt_runtime
            .listing_membership_generation
            .load(Ordering::Acquire);
        let mut pending = self
            .prompt_runtime
            .pending_step_reminders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for row in rows {
            if row.generation == generation
                && !pending
                    .iter()
                    .any(|current| current.message.id() == row.message.id())
            {
                pending.push(row);
            }
        }
    }

    pub(crate) fn retain_pending_step_reminder(
        &self,
        kind: &'static str,
        mut message: ConversationMessage,
        acknowledgement: Acknowledgement,
    ) -> ConversationMessage {
        let generation = self
            .prompt_runtime
            .listing_membership_generation
            .load(Ordering::Acquire);
        let mut pending = self
            .prompt_runtime
            .pending_step_reminders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = pending.iter().position(|row| {
            row.kind == kind && row.acknowledgement.same_producer(&acknowledgement)
        });
        if let Some(index) = previous {
            let old = &pending[index];
            let new_id = message.id();
            if old.generation == generation && old.acknowledgement == acknowledgement {
                if let ConversationMessage::User { id, .. } = &mut message {
                    *id = old.message.id();
                }
                // Compare exact serialized source too; display text alone loses
                // UTF-16 sidecars and would reuse a changed candidate's UUID.
                if serde_json::to_vec(&message).ok() == serde_json::to_vec(&old.message).ok() {
                    return old.message.clone();
                }
                if let ConversationMessage::User { id, .. } = &mut message {
                    *id = new_id;
                }
            }
            pending[index] = PendingReminder {
                kind,
                generation,
                message: message.clone(),
                acknowledgement,
            };
            return message;
        }
        pending.push(PendingReminder {
            kind,
            generation,
            message: message.clone(),
            acknowledgement,
        });
        message
    }

    pub(crate) fn pending_surfaced_memories(&self) -> Vec<memory::surfacing::SurfacedMemory> {
        let generation = self
            .prompt_runtime
            .listing_membership_generation
            .load(Ordering::Acquire);
        self.prompt_runtime
            .pending_step_reminders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(|row| match &row.acknowledgement {
                Acknowledgement::Memory(memory) if row.generation == generation => {
                    Some(memory.clone())
                }
                _ => None,
            })
            .collect()
    }

    pub(crate) fn retain_pending_memory_paths(
        &self,
        memories: &[memory::surfacing::SurfacedMemory],
    ) {
        self.prompt_runtime
            .pending_step_reminders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|row| {
                !matches!(&row.acknowledgement, Acknowledgement::Memory(memory)
                if !memories.iter().any(|fresh| fresh.path == memory.path))
            });
    }

    pub(crate) fn pending_discovered_skills(&self) -> Option<Vec<skill::DiscoveredSkill>> {
        let generation = self
            .prompt_runtime
            .listing_membership_generation
            .load(Ordering::Acquire);
        self.prompt_runtime
            .pending_step_reminders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find_map(|row| match &row.acknowledgement {
                Acknowledgement::Skills(skills) if row.generation == generation => {
                    Some(skills.clone())
                }
                _ => None,
            })
    }

    pub(crate) fn discard_pending_step_reminder(&self, kind: &'static str) {
        self.prompt_runtime
            .pending_step_reminders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|row| row.kind != kind);
    }

    pub(crate) async fn acknowledge_step_reminders(&self, request: &[ConversationMessage]) {
        let ids = request
            .iter()
            .map(ConversationMessage::id)
            .collect::<std::collections::HashSet<_>>();
        let candidates = self
            .prompt_runtime
            .pending_step_reminders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|row| ids.contains(&row.message.id()))
            .cloned()
            .collect::<Vec<_>>();
        for row in candidates {
            if row.generation
                != self
                    .prompt_runtime
                    .listing_membership_generation
                    .load(Ordering::Acquire)
            {
                continue;
            }
            match row.acknowledgement {
                Acknowledgement::Listing(membership) => {
                    self.commit_prompt_listing_membership(membership).await
                }
                Acknowledgement::Memory(memory) => {
                    let mut surfaced = self.prompt_runtime.surfaced_memory_paths.lock().await;
                    if row.generation
                        == self
                            .prompt_runtime
                            .listing_membership_generation
                            .load(Ordering::Acquire)
                    {
                        surfaced.insert(memory.path);
                    }
                }
                Acknowledgement::Skills(names) => {
                    let mut surfaced = self.prompt_runtime.surfaced_skill_names.lock().await;
                    if row.generation
                        == self
                            .prompt_runtime
                            .listing_membership_generation
                            .load(Ordering::Acquire)
                    {
                        surfaced.extend(names.into_iter().map(|skill| skill.name));
                    }
                }
            }
            // Never remove a recovery copy before the asynchronous set update.
            self.prompt_runtime
                .pending_step_reminders
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|pending| pending.message.id() != row.message.id());
        }
    }

    pub(crate) fn discard_pending_surfacing(&self) {
        self.prompt_runtime
            .pending_step_reminders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|row| matches!(row.acknowledgement, Acknowledgement::Listing(_)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use lingxi_core::types::MessageId;
    use std::sync::Arc;

    fn orchestrator(cwd: &std::path::Path) -> ConversationOrchestrator {
        ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.to_owned(),
        )
    }

    #[tokio::test]
    async fn cancelled_memory_survivors_keep_order_preamble_and_ids_after_a_real_read() {
        use tool_api::Tool;
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let orch = orchestrator(&cwd);
        let memories = ["a", "b", "c"].map(|name| memory::surfacing::SurfacedMemory {
            path: cwd.join(format!("{name}.md")),
            content: name.into(),
            age_days: 0,
            mtime: std::time::SystemTime::UNIX_EPOCH,
        });
        let mut old = Vec::new();
        for (memory, text) in memories
            .iter()
            .zip(memory::surfacing::render_surfacing_messages(&memories))
        {
            old.push(orch.retain_pending_step_reminder(
                "relevant_memories",
                ConversationMessage::user_meta(MessageId::new(), text),
                Acknowledgement::Memory(memory.clone()),
            ));
        }
        std::fs::write(&memories[0].path, "current A loaded through Read").unwrap();
        let fs = Arc::new(platform_posix::fs::PosixFileSystem::new(cwd.clone()));
        let mut context = tool_api::test_support::ctx_for_file_tools(
            fs,
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![cwd],
        );
        context.read_file_state = orch.prompt_runtime.read_state_map.clone();
        tool_file::FileReadTool::new(context)
            .call(
                lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                    &serde_json::json!({"file_path":memories[0].path}),
                )
                .unwrap(),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        let first = crate::request_context::scope_if_missing(&orch, async {
            Ok(orch.relevant_memory_reminder_messages().await)
        })
        .await
        .unwrap();
        assert_eq!(first.len(), 2);
        assert!(first[0]
            .text_content()
            .starts_with("Retrieved for possible relevance"));
        assert!(first[0].text_content().contains("b.md"));
        assert!(first[1].text_content().contains("c.md"));
        assert_ne!(
            first[0].id(),
            old[1].id(),
            "changed first-survivor rendering gets a fresh identity"
        );
        assert_eq!(first[1].id(), old[2].id());
        let second = crate::request_context::scope_if_missing(&orch, async {
            Ok(orch.relevant_memory_reminder_messages().await)
        })
        .await
        .unwrap();
        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&second).unwrap()
        );
        assert!(orch
            .prompt_runtime
            .surfaced_memory_paths
            .lock()
            .await
            .is_empty());
        orch.acknowledge_step_reminders(&second).await;
        assert_eq!(
            orch.prompt_runtime.surfaced_memory_paths.lock().await.len(),
            2
        );
        assert!(orch.pending_surfaced_memories().is_empty());
    }

    struct Skills(std::sync::atomic::AtomicBool);
    #[async_trait::async_trait]
    impl crate::prompt::skill_listing::SkillListingProvider for Skills {
        async fn skill_listing_snapshot(
            &self,
        ) -> Result<
            crate::prompt::skill_listing::SkillListingSnapshot,
            lingxi_core::settings::SettingsError,
        > {
            Ok(crate::prompt::skill_listing::SkillListingSnapshot {
                entries: Vec::new(),
                budget_settings: Default::default(),
            })
        }
        fn is_skill_available(
            &self,
            name: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        ) -> bool {
            name.string_at("") != Some("a") || !self.0.load(Ordering::Acquire)
        }
    }

    #[tokio::test]
    async fn cancelled_discovery_refilters_disabled_skills_before_replaying_and_acknowledging() {
        let root = tempfile::tempdir().unwrap();
        let provider = Arc::new(Skills(std::sync::atomic::AtomicBool::new(false)));
        let orch = orchestrator(root.path()).with_skill_listing(provider.clone());
        let skills = ["a", "b"]
            .map(|name| skill::DiscoveredSkill {
                name: name.into(),
                description: format!("description-{name}").into(),
                short_id: None,
            })
            .to_vec();
        let text = skill::render_skill_discovery_block(&skills).unwrap();
        let old = orch.retain_pending_step_reminder(
            "skill_discovery",
            ConversationMessage::user_meta(MessageId::new(), text.string_at("").unwrap().into()),
            Acknowledgement::Skills(skills),
        );
        provider.0.store(true, Ordering::Release);
        let fresh = crate::request_context::scope_if_missing(&orch, async {
            Ok(orch.skill_discovery_reminder_message().await.unwrap())
        })
        .await
        .unwrap();
        assert!(!fresh.text_content().contains("description-a"));
        assert!(fresh.text_content().contains("description-b"));
        assert_ne!(fresh.id(), old.id());
        let retry = crate::request_context::scope_if_missing(&orch, async {
            Ok(orch.skill_discovery_reminder_message().await.unwrap())
        })
        .await
        .unwrap();
        assert_eq!(
            serde_json::to_vec(&fresh).unwrap(),
            serde_json::to_vec(&retry).unwrap()
        );
        orch.acknowledge_step_reminders(&[retry]).await;
        assert_eq!(
            *orch.prompt_runtime.surfaced_skill_names.lock().await,
            ["b".to_owned()].into_iter().collect()
        );
        assert!(orch.pending_discovered_skills().is_none());
    }
}
