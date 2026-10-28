//! Model projection for retired skill instructions.
//!
//! The conversation transcript stays intact. If a retained aggregate contains
//! an obsolete activation, the projection replaces that aggregate with the
//! still-effective members that are not already represented elsewhere.

use std::collections::{HashMap, HashSet};

use lingxi_core::host::orchestrator::OrchestratorHandle;
use lingxi_core::types::{utf16_json::Utf16JsonProjection, ConversationMessage, MessageId};

use super::ConversationOrchestrator;
use crate::OrchestratorError;

struct Aggregate {
    id: MessageId,
    names: Vec<Vec<u16>>,
    visible: bool,
    stale: bool,
}

fn skill_names(origin: &Utf16JsonProjection) -> Option<Vec<Vec<u16>>> {
    let count = origin.array_len("/skills")?;
    if count == 0 {
        return None;
    }
    (0..count)
        .map(|index| origin.string_units(&format!("/skills/{index}/name")))
        .collect()
}

fn aggregate_origin(epoch: u64, skills: &[compaction::RestoredSkill]) -> Utf16JsonProjection {
    let mut origin = compaction::invoked_skills::skill_activation_origin(epoch);
    let sources = skills
        .iter()
        .map(|skill| {
            Utf16JsonProjection::object(vec![
                ("name".into(), skill.name.clone()),
                (
                    "path".into(),
                    skill.path.to_string_lossy().to_string().into(),
                ),
            ])
            .expect("skill source metadata has unique keys")
        })
        .collect();
    origin
        .set_field(
            "skills",
            Utf16JsonProjection::array(sources).expect("skill sources are an array"),
        )
        .expect("skill activation origin is an object");
    origin
}

impl ConversationOrchestrator {
    /// Refresh the model-only view after a skill is reinvoked. The caller owns
    /// the turn gate and calls this before taking a request/compaction snapshot.
    /// This never loads a skill or runs hooks, shell substitutions, or setup.
    pub(crate) async fn refresh_effective_skill_projection(&self) -> Result<(), OrchestratorError> {
        let session_id = self.current_session_id().await.to_string();
        let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_id), None);
        let epoch = compaction::invoked_skills::context_epoch(scope);
        let retired = compaction::invoked_skills::expired_message_ids_for_scope(scope);
        let candidates = compaction::invoked_skills::filter_for_scope(scope);
        let metadata = compaction::invoked_skills::activation_metadata_for_scope(scope);
        let attachments = self
            .transcript
            .post_compact_skill_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let candidate_names = candidates
            .iter()
            .enumerate()
            .filter_map(|(index, candidate)| {
                let name = candidate.name.string_units("")?;
                let body = candidate.content.string_units("")?;
                (!body.is_empty()).then_some((name, index))
            })
            .collect::<HashMap<_, _>>();

        let (target_session, history, mut excluded) = {
            let session = self.session.lock().await;
            if session.session_id.to_string() != session_id {
                return Err(OrchestratorError::Internal(
                    "conversation changed during skill projection".into(),
                ));
            }
            (
                session.session_id,
                session.history.clone(),
                session.model_context_excluded_messages.clone(),
            )
        };
        excluded.extend(retired.iter().copied());
        let mut aggregates = Vec::new();
        for message in &history {
            let ConversationMessage::User {
                id,
                origin: Some(origin),
                ..
            } = message
            else {
                continue;
            };
            if origin.string_at("/type") != Some("skill_activation")
                || origin.number_at("/context_epoch") != Some(epoch as f64)
            {
                continue;
            }
            let Some(names) = skill_names(origin) else {
                continue;
            };
            let bodies = attachments.get(id);
            let stale = retired.contains(id)
                || bodies.is_none_or(|bodies| {
                    bodies.len() != names.len()
                        || names.iter().zip(bodies).enumerate().any(
                            |(position, (name, previous))| {
                                let Some(index) = candidate_names.get(name) else {
                                    return true;
                                };
                                let path = candidates[*index].path.to_string_lossy();
                                if origin.string_at(&format!("/skills/{position}/path"))
                                    != Some(path.as_ref())
                                {
                                    return true;
                                }
                                let current = compaction::truncate_skill_content(
                                    &candidates[*index].content,
                                    compaction::thresholds::POST_COMPACT_MAX_TOKENS_PER_SKILL,
                                );
                                !current.semantically_eq(previous)
                            },
                        )
                });
            aggregates.push(Aggregate {
                id: *id,
                names,
                visible: !excluded.contains(id),
                stale,
            });
        }

        for aggregate in &aggregates {
            if aggregate.visible && aggregate.stale {
                excluded.insert(aggregate.id);
            }
        }
        let visible_ids = history
            .iter()
            .filter(|row| !excluded.contains(&row.id()))
            .map(ConversationMessage::id)
            .collect::<HashSet<_>>();
        let mut covered = aggregates
            .iter()
            .filter(|aggregate| aggregate.visible && !aggregate.stale)
            .flat_map(|aggregate| aggregate.names.iter().cloned())
            .collect::<HashSet<_>>();
        for entry in metadata.iter().filter(|entry| entry.effective) {
            let Some(owner_id) = entry.message_id.filter(|id| visible_ids.contains(id)) else {
                continue;
            };
            let Some(name) = entry.skill_name.string_units("") else {
                continue;
            };
            let is_current_individual = history.iter().any(|row| {
                matches!(row, ConversationMessage::User { id, origin: Some(origin), .. }
                    if *id == owner_id
                        && origin.string_at("/type") == Some("skill_activation")
                        && origin.string_units("/skill_name").as_ref() == Some(&name))
            });
            if is_current_individual {
                covered.insert(name);
            }
        }
        let needed = aggregates
            .iter()
            .filter(|aggregate| !aggregate.visible || aggregate.stale)
            .flat_map(|aggregate| aggregate.names.iter().cloned())
            .filter(|name| candidate_names.contains_key(name) && !covered.contains(name))
            .collect::<HashSet<_>>();
        let selected = candidates
            .into_iter()
            .filter(|candidate| {
                candidate
                    .name
                    .string_units("")
                    .is_some_and(|name| needed.contains(&name))
            })
            .map(|mut candidate| {
                // Budgeting is provisional until the replacement is durable.
                // An empty key cannot match a registry row, so write-backs in
                // the renderer cannot mutate live activations on failure.
                candidate.key.clear();
                candidate.activation_id = None;
                candidate
            })
            .collect::<Vec<_>>();
        let restored = compaction::restore_post_compact_skills(selected, &[]);
        let Some(rendered) = compaction::render_invoked_skills_attachment(&restored) else {
            let mut session = self.session.lock().await;
            if session.session_id != target_session {
                return Err(OrchestratorError::Internal(
                    "conversation changed during skill projection".into(),
                ));
            }
            session.model_context_excluded_messages.extend(excluded);
            return Ok(());
        };
        let id = MessageId::new();
        let mut replacement = ConversationMessage::user_meta_js_utf16(
            id,
            rendered
                .string_at("")
                .expect("rendered skill attachment is text")
                .to_owned(),
            rendered
                .string_units("")
                .expect("rendered skill attachment is text"),
        );
        if let ConversationMessage::User { origin, .. } = &mut replacement {
            *origin = Some(aggregate_origin(epoch, &restored));
        }
        // Give session.append its normal host admission, but never accept a
        // rewrite that breaks the metadata/body correspondence of this passive
        // aggregate. Neither history nor exclusions have been published yet.
        let accepted = self
            .mod_session_append_row(&replacement, None, false, None)
            .await;
        if accepted != replacement {
            return Err(OrchestratorError::Internal(
                "session.append rewrote a skill projection attachment".into(),
            ));
        }
        self.check_storage_pause().await?;
        if let Some(writer) = self.transcript.jsonl_writer.as_ref() {
            let target_bare = target_session.as_uuid().to_string();
            let active_path = writer.active_path();
            if active_path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| uuid::Uuid::parse_str(stem).ok())
                .is_some_and(|id| id != target_session.as_uuid())
            {
                return Err(OrchestratorError::Internal(
                    "conversation changed before skill projection persistence".into(),
                ));
            }
            let parent = self.transcript.last_jsonl_uuid.lock().await.clone();
            let mut row = self.to_jsonl_message_with_inner_id(
                &replacement,
                &target_bare,
                parent,
                self.resolve_git_branch().await,
                Some(super::entrypoint_value()),
                None,
                None,
                None,
                None,
                None,
                None,
            );
            row.extra
                .set_field(
                    "invokedSkillContents",
                    Utf16JsonProjection::array(
                        restored.iter().map(|skill| skill.content.clone()).collect(),
                    )
                    .expect("skill contents form an array"),
                )
                .expect("transcript extra metadata is an object");
            if let Err(error) = self.append_transcript_row(writer, &row).await {
                self.record_transcript_append_failure(&target_bare, "skill_projection", &error)
                    .await;
                return Err(OrchestratorError::Internal(format!(
                    "could not persist skill projection: {error}"
                )));
            }
            if writer.active_path() != active_path {
                return Err(OrchestratorError::Internal(
                    "conversation changed during skill projection persistence".into(),
                ));
            }
            *self.transcript.last_jsonl_uuid.lock().await = Some(row.uuid);
        }
        let mut session = self.session.lock().await;
        if session.session_id != target_session {
            return Err(OrchestratorError::Internal(
                "conversation changed during skill projection".into(),
            ));
        }
        self.transcript
            .post_compact_skill_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id,
                restored.iter().map(|skill| skill.content.clone()).collect(),
            );
        session.model_context_excluded_messages.extend(excluded);
        session.history.push(replacement);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use crate::OrchestratorConfig;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn orchestrator() -> ConversationOrchestrator {
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/work"),
        )
    }

    fn individual(id: MessageId, name: &str, body: &str) -> ConversationMessage {
        ConversationMessage::user(id, body.into()).with_origin(
            compaction::invoked_skills::skill_activation_origin_with_skill(
                0,
                &Utf16JsonProjection::from(name),
                Path::new("/skills"),
            ),
        )
    }

    fn register(
        scope: compaction::invoked_skills::InvokedSkillScopeRef<'_>,
        id: MessageId,
        name: &str,
        body: &str,
    ) {
        compaction::invoked_skills::register_activation_with_message(
            &name.into(),
            Path::new("/skills"),
            &body.into(),
            None,
            scope,
            id,
        );
    }

    fn aggregate(id: MessageId, skills: &[compaction::RestoredSkill]) -> ConversationMessage {
        let body = compaction::render_invoked_skills_attachment(skills).unwrap();
        ConversationMessage::user_meta_js_utf16(
            id,
            body.string_at("").unwrap().to_string(),
            body.string_units("").unwrap(),
        )
        .with_origin(aggregate_origin(0, skills))
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn reinvocation_excludes_only_the_old_host_instruction() {
        let _guard = compaction::invoked_skills::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        compaction::invoked_skills::reset_for_test();
        let orch = orchestrator();
        let session_id = orch.current_session_id().await.to_string();
        let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_id), None);
        let old_id = MessageId::new();
        let new_id = MessageId::new();
        let quoted = ConversationMessage::user(MessageId::new(), "old instructions".into());
        register(scope, old_id, "skill", "old instructions");
        register(scope, new_id, "skill", "new instructions");
        {
            let mut session = orch.session.lock().await;
            session.history.extend([
                individual(old_id, "skill", "old instructions"),
                quoted.clone(),
                individual(new_id, "skill", "new instructions"),
            ]);
        }
        orch.refresh_effective_skill_projection().await.unwrap();
        let session = orch.session.lock().await;
        let model = session.model_context_history();
        assert_eq!(session.history.len(), 3, "transcript rows remain present");
        assert!(!model.iter().any(|row| row.id() == old_id));
        assert!(model.contains(&quoted), "identical user evidence survives");
        assert!(model.iter().any(|row| row.id() == new_id));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn stale_aggregate_preserves_other_effective_skills_across_preparations() {
        let _guard = compaction::invoked_skills::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for shared_aggregate_owner in [false, true] {
            compaction::invoked_skills::reset_for_test();
            let orch = orchestrator();
            let session_id = orch.current_session_id().await.to_string();
            let scope =
                compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_id), None);
            let aggregate_id = MessageId::new();
            let owner_a = if shared_aggregate_owner {
                aggregate_id
            } else {
                MessageId::new()
            };
            let owner_b = if shared_aggregate_owner {
                aggregate_id
            } else {
                MessageId::new()
            };
            register(scope, owner_a, "a", "old a");
            register(scope, owner_b, "b", "ongoing b");
            let skills = vec![
                compaction::RestoredSkill {
                    name: "a".into(),
                    path: PathBuf::from("/skills"),
                    content: "old a".into(),
                },
                compaction::RestoredSkill {
                    name: "b".into(),
                    path: PathBuf::from("/skills"),
                    content: "ongoing b".into(),
                },
            ];
            orch.transcript
                .post_compact_skill_attachments
                .lock()
                .unwrap()
                .insert(
                    aggregate_id,
                    skills.iter().map(|skill| skill.content.clone()).collect(),
                );
            let updated_a_id = MessageId::new();
            let quote = ConversationMessage::user(MessageId::new(), "old a".into());
            register(scope, updated_a_id, "a", "new a");
            {
                let mut session = orch.session.lock().await;
                session.history.extend([
                    aggregate(aggregate_id, &skills),
                    quote.clone(),
                    individual(updated_a_id, "a", "new a"),
                ]);
            }
            orch.refresh_effective_skill_projection().await.unwrap();
            let replacement_id = {
                let session = orch.session.lock().await;
                let model = session.model_context_history();
                assert!(!model.iter().any(|row| row.id() == aggregate_id));
                assert!(model.contains(&quote));
                assert!(model.iter().any(|row| row.id() == updated_a_id));
                let replacement = model
                    .iter()
                    .find(|row| row.is_meta())
                    .expect("b reattached");
                assert!(replacement.text_content().contains("ongoing b"));
                assert!(!replacement.text_content().contains("old a"));
                assert!(replacement
                    .text_content()
                    .contains("Do NOT re-execute these skills"));
                replacement.id()
            };
            orch.refresh_effective_skill_projection().await.unwrap();
            {
                let session = orch.session.lock().await;
                assert_eq!(session.history.len(), 4, "the replacement is stable");
                assert!(session
                    .model_context_history()
                    .iter()
                    .any(|row| row.id() == replacement_id));
            }

            let updated_b_id = MessageId::new();
            register(scope, updated_b_id, "b", "new b");
            orch.session
                .lock()
                .await
                .history
                .push(individual(updated_b_id, "b", "new b"));
            orch.refresh_effective_skill_projection().await.unwrap();
            let session = orch.session.lock().await;
            let model = session.model_context_history();
            assert!(!model.iter().any(|row| row.id() == replacement_id));
            assert!(model.iter().any(|row| row.id() == updated_a_id));
            assert!(model.iter().any(|row| row.id() == updated_b_id));
            assert!(model.contains(&quote));
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn replacement_is_persisted_before_publication_and_failure_keeps_old_projection() {
        let _guard = compaction::invoked_skills::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (fail, named_path) in [(false, false), (false, true), (true, false)] {
            compaction::invoked_skills::reset_for_test();
            let dir = tempfile::tempdir().unwrap();
            let target = lingxi_core::types::SessionId::new();
            let filename = if named_path {
                "transcript.jsonl".to_string()
            } else {
                format!("{}.jsonl", target.as_uuid())
            };
            let path = dir.path().join(filename);
            if fail {
                std::fs::create_dir(&path).unwrap();
            }
            let fs = Arc::new(platform_posix::fs::PosixFileSystem::new(dir.path().into()));
            let writer = Arc::new(session::JsonlWriter::new(path.clone(), fs));
            let orch = orchestrator()
                .with_session_id(target)
                .with_jsonl_writer(writer);
            let session_id = target.to_string();
            let scope =
                compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_id), None);
            let aggregate_id = MessageId::new();
            register(scope, aggregate_id, "a", "old a");
            register(scope, aggregate_id, "b", "ongoing b");
            let skills = vec![
                compaction::RestoredSkill {
                    name: "a".into(),
                    path: PathBuf::from("/skills"),
                    content: "old a".into(),
                },
                compaction::RestoredSkill {
                    name: "b".into(),
                    path: PathBuf::from("/skills"),
                    content: "ongoing b".into(),
                },
            ];
            orch.transcript
                .post_compact_skill_attachments
                .lock()
                .unwrap()
                .insert(
                    aggregate_id,
                    skills.iter().map(|skill| skill.content.clone()).collect(),
                );
            let updated_id = MessageId::new();
            register(scope, updated_id, "a", "new a");
            orch.session.lock().await.history.extend([
                aggregate(aggregate_id, &skills),
                individual(updated_id, "a", "new a"),
            ]);

            let result = orch.refresh_effective_skill_projection().await;
            let session = orch.session.lock().await;
            if fail {
                assert!(result.is_err(), "append failure must abort preparation");
                assert_eq!(session.history.len(), 2);
                assert!(!session
                    .model_context_excluded_messages
                    .contains(&aggregate_id));
                assert_eq!(
                    orch.transcript
                        .post_compact_skill_attachments
                        .lock()
                        .unwrap()
                        .len(),
                    1
                );
                continue;
            }
            result.unwrap();
            assert!(session
                .model_context_excluded_messages
                .contains(&aggregate_id));
            let replacement_id = session.history.last().unwrap().id();
            drop(session);
            let saved = std::fs::read_to_string(&path).unwrap();
            let rows = saved
                .lines()
                .map(|line| serde_json::from_str::<session::JsonlMessage>(line).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].uuid, replacement_id.as_uuid().to_string());
            let recovered = crate::resume::post_compact_skill_attachments_from_messages(&rows);
            assert_eq!(
                recovered.get(&replacement_id),
                Some(&vec![Utf16JsonProjection::from("ongoing b")])
            );
            let cold = crate::resume::state_from_messages(target.as_uuid(), &rows);
            assert!(cold
                .history
                .iter()
                .any(|row| row.id() == replacement_id && row.text_content().contains("ongoing b")));
        }
    }
}
