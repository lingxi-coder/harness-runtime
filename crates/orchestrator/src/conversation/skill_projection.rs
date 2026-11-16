//! Append-only model context for retired skill activations.
//!
//! The registry retains only effective activations. Historical instruction bytes
//! stay at their original positions, with a durable update declaring exactly
//! which earlier activations no longer apply.

use std::collections::{HashMap, HashSet};

use lingxi_core::host::orchestrator::OrchestratorHandle;
use lingxi_core::types::{utf16_json::Utf16JsonProjection, ConversationMessage, MessageId};

use super::ConversationOrchestrator;
use crate::OrchestratorError;

struct SkillProjectionRow {
    id: MessageId,
    origin: Option<Utf16JsonProjection>,
}

fn skill_projection_history(history: &[ConversationMessage]) -> Vec<SkillProjectionRow> {
    history
        .iter()
        .map(|message| SkillProjectionRow {
            id: message.id(),
            origin: match message {
                ConversationMessage::User {
                    origin: Some(origin),
                    ..
                } if origin.string_at("/type") == Some("skill_activation") => Some(origin.clone()),
                _ => None,
            },
        })
        .collect()
}

/// Recover exact retired source identities before replaying active registry
/// entries. Retaining historical model bytes must not resurrect their actions.
pub(super) fn retired_skill_sources(
    history: &[ConversationMessage],
    epoch: u64,
) -> HashSet<(MessageId, Vec<u16>)> {
    retired_sources(&skill_projection_history(history), epoch)
}

fn retired_sources(history: &[SkillProjectionRow], epoch: u64) -> HashSet<(MessageId, Vec<u16>)> {
    history
        .iter()
        .filter_map(|row| row.origin.as_ref())
        .filter(|origin| origin.number_at("/context_epoch") == Some(epoch as f64))
        .flat_map(|origin| {
            (0..origin.array_len("/retired_activations").unwrap_or_default())
                .filter_map(|index| {
                    let pointer = format!("/retired_activations/{index}");
                    let id = origin.string_at(&format!("{pointer}/message_id"))?;
                    let id = uuid::Uuid::parse_str(id).ok().map(MessageId::from_uuid)?;
                    let name = origin.string_units(&format!("{pointer}/skill_name"))?;
                    Some((id, name))
                })
                .collect::<Vec<_>>()
        })
        .collect()
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

fn aggregate_origin(
    epoch: u64,
    skills: &[compaction::RestoredSkill],
    scope: Option<compaction::invoked_skills::InvokedSkillScopeRef<'_>>,
) -> Utf16JsonProjection {
    let mut origin = compaction::invoked_skills::skill_activation_origin(epoch);
    let sources = skills
        .iter()
        .map(|skill| {
            if let Some(scope) = scope {
                return compaction::invoked_skills::skill_source_metadata(
                    &skill.name,
                    &skill.path,
                    scope,
                );
            }
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

struct Retirement {
    source: MessageId,
    name: Utf16JsonProjection,
    active_replacement: bool,
}

fn retirement_body(retirements: &[Retirement]) -> String {
    if retirements.is_empty() {
        return String::new();
    }
    let mut body = String::from(
        "<system-reminder>\nSkill activation update. Earlier instructions and task arguments from the following invocations are historical context and no longer apply:\n",
    );
    for retirement in retirements {
        let name = retirement
            .name
            .to_json_string()
            .expect("skill name is a JSON string");
        let state = if retirement.active_replacement {
            "Use only its most recent active invocation."
        } else {
            "It has no active invocation in the current context; do not apply its earlier instructions or arguments."
        };
        body.push_str(&format!(
            "- {name}, invocation {}: {state}\n",
            retirement.source.as_uuid(),
        ));
    }
    body.push_str(
        "Other active skills remain unchanged. This update does not reload skills, rerun setup, or authorize repeating any earlier task.\n</system-reminder>",
    );
    body
}

impl ConversationOrchestrator {
    /// The turn gate serializes this refresh with request assembly and recovery.
    /// Publish updates only after durable admission, without changing old bytes.
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
                (!candidate.content.string_units("")?.is_empty()).then_some((name, index))
            })
            .collect::<HashMap<_, _>>();
        let (target_session, history, excluded) = {
            let session = self.session.lock().await;
            if session.session_id.to_string() != session_id {
                return Err(OrchestratorError::Internal(
                    "conversation changed during skill projection".into(),
                ));
            }
            (
                session.session_id,
                skill_projection_history(&session.history),
                session.model_context_excluded_messages.clone(),
            )
        };
        let acknowledged = retired_sources(&history, epoch);
        let visible = history
            .iter()
            .filter(|row| !excluded.contains(&row.id))
            .collect::<Vec<_>>();
        let individuals = visible
            .iter()
            .filter_map(|row| {
                let origin = row.origin.as_ref()?;
                (origin.number_at("/context_epoch") == Some(epoch as f64)).then(|| {
                    origin
                        .string_units("/skill_name")
                        .map(|name| (row.id, name))
                })?
            })
            .collect::<HashSet<_>>();
        let current_individuals = metadata
            .iter()
            .filter(|entry| entry.effective)
            .filter_map(|entry| {
                let name = entry.skill_name.string_units("")?;
                let owner = entry.message_id?;
                (candidate_names.contains_key(&name)
                    && individuals.contains(&(owner, name.clone())))
                .then_some((owner, name))
            })
            .collect::<HashSet<_>>();
        let mut covered = current_individuals
            .iter()
            .map(|(_, name)| name.clone())
            .collect::<HashSet<_>>();
        let mut retirements = Vec::new();
        let mut retirement_keys = acknowledged.clone();
        for row in visible {
            let Some(origin) = &row.origin else { continue };
            if origin.number_at("/context_epoch") != Some(epoch as f64) {
                continue;
            }
            let mut retire = |name: Vec<u16>| {
                if retirement_keys.insert((row.id, name.clone())) {
                    retirements.push(Retirement {
                        source: row.id,
                        name: Utf16JsonProjection::root_string(
                            String::from_utf16_lossy(&name),
                            name.clone(),
                        )
                        .expect("exact skill name"),
                        active_replacement: candidate_names.contains_key(&name),
                    });
                }
            };
            if let Some(name) = origin.string_units("/skill_name") {
                if retired.contains(&row.id)
                    || !current_individuals.contains(&(row.id, name.clone()))
                {
                    retire(name);
                }
                continue;
            }
            let Some(names) = skill_names(origin) else {
                continue;
            };
            let bodies = attachments.get(&row.id);
            for (position, name) in names.into_iter().enumerate() {
                if acknowledged.contains(&(row.id, name.clone())) {
                    continue;
                }
                let current = candidate_names.get(&name).map(|index| &candidates[*index]);
                let unchanged = current.is_some_and(|candidate| {
                    let path = candidate.path.to_string_lossy();
                    let same_path = origin.string_at(&format!("/skills/{position}/path"))
                        == Some(path.as_ref());
                    let body = bodies.and_then(|bodies| bodies.get(position));
                    let same_body = body.is_some_and(|body| {
                        compaction::truncate_skill_content(
                            &candidate.content,
                            compaction::thresholds::POST_COMPACT_MAX_TOKENS_PER_SKILL,
                        )
                        .semantically_eq(body)
                    });
                    let source = compaction::invoked_skills::skill_source_metadata(
                        &candidate.name,
                        &candidate.path,
                        scope,
                    );
                    let old_definition =
                        origin.string_at(&format!("/skills/{position}/definition_sha256"));
                    let same_definition = old_definition.is_none_or(|digest| {
                        source.string_at("/definition_sha256") == Some(digest)
                    });
                    same_path
                        && same_body
                        && same_definition
                        && !current_individuals
                            .iter()
                            .any(|(_, current)| current == &name)
                });
                if unchanged {
                    covered.insert(name);
                } else {
                    retire(name);
                }
            }
        }
        let selected = candidates
            .into_iter()
            .filter(|candidate| {
                self.prompt_runtime
                    .skill_listing
                    .as_ref()
                    .is_none_or(|provider| provider.is_skill_available(&candidate.name))
            })
            .filter(|candidate| {
                candidate.name.string_units("").is_some_and(|name| {
                    candidate_names.contains_key(&name) && !covered.contains(&name)
                })
            })
            .map(|mut candidate| {
                // Rendering cannot write back into live registry before commit.
                candidate.key.clear();
                candidate.activation_id = None;
                candidate
            })
            .collect::<Vec<_>>();
        let restored = compaction::restore_post_compact_skills(selected, &[]);
        if retirements.is_empty() && restored.is_empty() {
            return Ok(());
        }
        let mut units = retirement_body(&retirements)
            .encode_utf16()
            .collect::<Vec<_>>();
        if let Some(rendered) = compaction::render_invoked_skills_attachment(&restored) {
            if !units.is_empty() {
                units.extend("\n\n".encode_utf16());
            }
            units.extend(rendered.string_units("").expect("rendered skill body"));
        }
        let id = MessageId::new();
        let mut origin = aggregate_origin(epoch, &restored, Some(scope));
        origin
            .set_field(
                "retired_activations",
                Utf16JsonProjection::array(
                    retirements
                        .iter()
                        .map(|retirement| {
                            Utf16JsonProjection::object(vec![
                                (
                                    "message_id".into(),
                                    retirement.source.as_uuid().to_string().into(),
                                ),
                                ("skill_name".into(), retirement.name.clone()),
                            ])
                            .expect("retired skill identity")
                        })
                        .collect(),
                )
                .expect("retired skills form an array"),
            )
            .expect("skill context origin");
        let replacement =
            ConversationMessage::user_meta_js_utf16(id, String::from_utf16_lossy(&units), units)
                .with_origin(origin);
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
            if !restored.is_empty() {
                row.extra
                    .set_field(
                        "invokedSkillContents",
                        Utf16JsonProjection::array(
                            restored.iter().map(|skill| skill.content.clone()).collect(),
                        )
                        .expect("skill contents form an array"),
                    )
                    .expect("transcript extra metadata is an object");
            }
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
        }
        let mut session = self.session.lock().await;
        if session.session_id != target_session {
            return Err(OrchestratorError::Internal(
                "conversation changed during skill projection".into(),
            ));
        }
        if !restored.is_empty() {
            self.transcript
                .post_compact_skill_attachments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    id,
                    restored.iter().map(|skill| skill.content.clone()).collect(),
                );
        }
        session.history.push(replacement);
        Ok(())
    }
}

#[cfg(test)]
#[path = "skill_projection_cache_tests.rs"]
mod cache_tests;

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

    fn replacement_has_body(content: &[lingxi_core::types::ContentBlock], body: &str) -> bool {
        content.iter().any(|block| matches!(block, lingxi_core::types::ContentBlock::TextJsUtf16 { text, .. } | lingxi_core::types::ContentBlock::Text { text, .. } if text.contains(body)))
    }

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
        .with_origin(aggregate_origin(0, skills, None))
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn reinvocation_retires_only_the_old_host_instruction_without_rewriting_history() {
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
        assert_eq!(session.history.len(), 4, "one durable retirement update");
        assert!(model.iter().any(|row| row.id() == old_id));
        assert!(
            retired_skill_sources(&model, 0).contains(&(old_id, "skill".encode_utf16().collect()))
        );
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
                assert!(model.iter().any(|row| row.id() == aggregate_id));
                assert!(model.contains(&quote));
                assert!(model.iter().any(|row| row.id() == updated_a_id));
                let replacement = model.iter().last().expect("retirement appended");
                assert!(
                    !replacement.text_content().contains("ongoing b"),
                    "unchanged B is not reattached"
                );
                assert!(!replacement.text_content().contains("old a"));
                assert!(retired_skill_sources(&model, 0)
                    .contains(&(aggregate_id, "a".encode_utf16().collect())));
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
            assert!(model.iter().any(|row| row.id() == replacement_id));
            assert!(model.iter().any(|row| row.id() == updated_a_id));
            assert!(model.iter().any(|row| row.id() == updated_b_id));
            assert!(model.contains(&quote));
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn skill_projection_preserves_exact_utf16_origins_with_large_unrelated_history() {
        let _guard = compaction::invoked_skills::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        compaction::invoked_skills::reset_for_test();
        let orch = orchestrator();
        let session_id = orch.current_session_id().await.to_string();
        let scope = compaction::invoked_skills::InvokedSkillScopeRef::new(Some(&session_id), None);
        let aggregate_id = MessageId::new();
        let updated_id = MessageId::new();
        let exact_name = Utf16JsonProjection::parse(r#""\ud800""#).unwrap();
        register(scope, aggregate_id, "a", "old a");
        compaction::invoked_skills::register_activation_with_message(
            &exact_name,
            Path::new("/skills"),
            &"retained body".into(),
            None,
            scope,
            aggregate_id,
        );
        register(scope, updated_id, "a", "new a");
        let skills = vec![
            compaction::RestoredSkill {
                name: "a".into(),
                path: PathBuf::from("/skills"),
                content: "old a".into(),
            },
            compaction::RestoredSkill {
                name: exact_name,
                path: PathBuf::from("/skills"),
                content: "retained body".into(),
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
        let unrelated_id = MessageId::new();
        orch.session.lock().await.history.extend([
            ConversationMessage::user(unrelated_id, "x".repeat(1 << 20)),
            aggregate(aggregate_id, &skills),
            individual(updated_id, "a", "new a"),
        ]);

        orch.refresh_effective_skill_projection().await.unwrap();
        let replacement_id = {
            let session = orch.session.lock().await;
            assert_eq!(session.history.len(), 4);
            assert!(!session
                .model_context_excluded_messages
                .contains(&aggregate_id));
            let ConversationMessage::User {
                id,
                origin: Some(origin),
                content,
                ..
            } = session.history.last().unwrap()
            else {
                panic!("the retained skill has a replacement attachment");
            };
            assert_eq!(
                origin.string_at("/retired_activations/0/skill_name"),
                Some("a")
            );
            assert!(
                !replacement_has_body(content, "retained body"),
                "unchanged exact skill is not reattached"
            );
            assert!(
                matches!(session.history.iter().find(|row| row.id() == aggregate_id).unwrap(), ConversationMessage::User { content, .. }
                if matches!(&content[0], lingxi_core::types::ContentBlock::TextJsUtf16 { utf16_code_units, .. } if utf16_code_units.contains(&0xd800)))
            );
            assert_eq!(session.history[0].id(), unrelated_id);
            *id
        };
        orch.refresh_effective_skill_projection().await.unwrap();
        let session = orch.session.lock().await;
        assert_eq!(session.history.len(), 4, "metadata refresh is stable");
        assert_eq!(session.history.last().unwrap().id(), replacement_id);
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
            assert!(!session
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
            assert!(
                !recovered.contains_key(&replacement_id),
                "unchanged B is not reattached"
            );
            let cold = crate::resume::state_from_messages(target.as_uuid(), &rows);
            assert!(retired_skill_sources(&cold.history, 0)
                .contains(&(aggregate_id, "a".encode_utf16().collect())));
        }
    }
}
