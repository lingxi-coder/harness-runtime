//! Conversation-snapshot writer for the 2.1.212 `/fork` (`vAd`) background
//! session copy.
//!
//! [`history_to_jsonl_lines`] converts a live in-memory conversation into the
//! `<uuid>.jsonl` transcript line shape the RESUME loader
//! ([`crate::resume::replay_session_state`] → `session::jsonl::load_session`)
//! reads back, so a backgrounded worker that resumes the copied session sees
//! the parent conversation.
//!
//! This is a self-contained, loader-compatible projection — NOT the full
//! byte-golden `ConversationOrchestrator::to_jsonl_message` envelope (which
//! needs live per-turn state: response usage, request-id, effort, api-error
//! flags). The resume loader reconstructs `ConversationMessage`s from the line
//! `type` + inner `message.{role,content}` + the `uuid`/`parentUuid` chain, all
//! of which this projection emits faithfully. The richer per-turn usage/error
//! metadata is irrelevant to a fresh COPY (there is no in-flight response to
//! attribute), so reproducing it here would only duplicate that method's logic.
//!
//! One per-turn field is NOT metadata-only: the assistant line's `model`. The
//! resume loader ([`crate::resume::state_from_messages`]) restores the session's
//! active model from the LAST assistant line's `message.model` (real transcripts
//! carry it — `conversation.rs`'s `to_jsonl_message` writes it), and
//! `seed_orchestrator_session` copies that into `session.model`. Emitting the
//! assistant lines WITHOUT `model` therefore makes a forked background session
//! silently resume on `DEFAULT_MODEL` (`claude-opus-4-8`), losing the parent's
//! active model (e.g. after `/model sonnet`, or a cross-provider model — whose
//! provider profile the resume path re-derives from the model id, so restoring
//! the id restores the routing too). `ConversationMessage::Assistant` carries no
//! per-line model, so the caller threads the parent's CURRENT session model in
//! and this writer stamps it onto every assistant line — the last one is what
//! the loader reads, so the copy resumes on the parent's model, not the default.
//!
//! The composition root (`apps/cli`) owns the actual write (it holds the
//! `session::jsonl::JsonlWriter` + resolved `projects/<sanitize(cwd)>/…` path);
//! this helper lives in `orchestrator` so the `lingxi_core::types::ConversationMessage`
//! → `session::JsonlMessage` mapping stays next to `to_jsonl_message`.

use lingxi_core::host::bg_session_forker::BgSessionSnapshot;
use lingxi_core::types::ConversationMessage;
use session::JsonlMessage;

/// ISO-8601 UTC timestamp with millisecond precision (`new Date().toISOString()`
/// shape) — matches [`ConversationOrchestrator::to_jsonl_message`]'s timestamp
/// format so the copied lines are indistinguishable from live-written ones.
fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Project `conversation` into resume-loader-compatible JSONL lines for a NEW
/// session `session_uuid` (the bare uuid, no `sess:` prefix), stamping `cwd`
/// and `version` on every line. Lines are `parentUuid`-chained in arrival
/// order (first line's `parentUuid` is `null`), mirroring the live append
/// chain the loader's branch-aware DAG walk expects.
///
/// `model` is the parent session's CURRENT active model, stamped onto every
/// assistant line's `message.model` (an empty string ⇒ omitted). The resume
/// loader restores `session.model` from the LAST assistant line's `model`
/// (see [`crate::resume::state_from_messages`]), so passing it here makes the
/// forked background session resume on the parent's model instead of the
/// `DEFAULT_MODEL` seed — matching how ordinary `--resume` preserves the model
/// from real (model-carrying) assistant lines.
#[must_use]
pub fn history_to_jsonl_lines(
    conversation: &BgSessionSnapshot,
    session_uuid: &str,
    cwd: &str,
    version: &str,
    model: &str,
) -> Vec<JsonlMessage> {
    let ts = now_iso();
    let mut lines = Vec::with_capacity(conversation.history.len());
    let attachments: std::collections::HashMap<_, _> = conversation
        .model_reminder_attachments
        .iter()
        .map(|(id, attachment)| (*id, attachment))
        .collect();
    let mut parent_uuid: Option<String> = None;
    for msg in &conversation.history {
        let mut extra = serde_json::Map::new();
        let mut logical_parent_uuid = None;
        let mut resets_chain = false;
        let (mut kind, mut inner) = match msg {
            ConversationMessage::User {
                content,
                is_meta,
                is_compact_summary,
                is_visible_in_transcript_only,
                ..
            } => {
                if *is_meta {
                    extra.insert("isMeta".to_string(), serde_json::Value::Bool(true));
                }
                if *is_visible_in_transcript_only {
                    extra.insert(
                        "isVisibleInTranscriptOnly".to_string(),
                        serde_json::Value::Bool(true),
                    );
                }
                if *is_compact_summary {
                    extra.insert(
                        "isCompactSummary".to_string(),
                        serde_json::Value::Bool(true),
                    );
                }
                (
                    "user",
                    serde_json::json!({ "role": "user", "content": content }),
                )
            }
            ConversationMessage::Assistant { content, .. } => {
                let mut inner = serde_json::json!({ "role": "assistant", "content": content });
                // Stamp the parent's active model so `state_from_messages`
                // restores it on resume (real assistant lines carry `model`;
                // `ConversationMessage::Assistant` has none to recover per-line,
                // so the caller threads the live session model in). An empty
                // model would resume as `<synthetic>`-style noise, so omit it.
                if !model.is_empty() {
                    inner["model"] = serde_json::Value::String(model.to_string());
                }
                ("assistant", inner)
            }
            ConversationMessage::System {
                content,
                subtype: Some(subtype),
                compact_metadata: Some(metadata),
                ..
            } if subtype == "compact_boundary" => {
                resets_chain = true;
                logical_parent_uuid = metadata
                    .logical_parent_uuid
                    .clone()
                    .or_else(|| parent_uuid.clone());
                let mut compact_metadata =
                    serde_json::to_value(metadata).unwrap_or_else(|_| serde_json::json!({}));
                if let Some(object) = compact_metadata.as_object_mut() {
                    object.remove("logicalParentUuid");
                }
                extra.insert(
                    "subtype".to_string(),
                    serde_json::Value::String(subtype.clone()),
                );
                extra.insert(
                    "content".to_string(),
                    serde_json::Value::String(content.clone()),
                );
                extra.insert(
                    "level".to_string(),
                    serde_json::Value::String("info".to_string()),
                );
                extra.insert("compactMetadata".to_string(), compact_metadata);
                ("system", serde_json::Value::Null)
            }
            ConversationMessage::System {
                content,
                subtype: Some(subtype),
                refusal_fallback: Some(metadata),
                ..
            } if subtype == "model_refusal_fallback" => {
                extra.insert("subtype".into(), serde_json::json!(subtype));
                extra.insert("content".into(), serde_json::json!(content));
                extra.insert("level".into(), serde_json::json!("warning"));
                extra.insert("isMeta".into(), serde_json::json!(false));
                if let serde_json::Value::Object(fields) =
                    serde_json::to_value(metadata).expect("refusal metadata is JSON")
                {
                    extra.extend(fields);
                }
                extra
                    .entry("apiRefusalExplanation")
                    .or_insert(serde_json::Value::Null);
                ("system", serde_json::Value::Null)
            }
            ConversationMessage::System {
                content,
                subtype: Some(subtype),
                model_fallback: Some(metadata),
                ..
            } if subtype == "model_fallback" => {
                // Keep the parent transcript's typed fallback marker in the
                // same flattened system envelope as the ordinary writer.
                // The subtype guard is intentional: unrelated system messages
                // may carry metadata but must stay ordinary system rows.
                extra.insert("subtype".into(), serde_json::json!(subtype));
                extra.insert("content".into(), serde_json::json!(content));
                extra.insert("level".into(), serde_json::json!("warning"));
                extra.insert("trigger".into(), serde_json::json!(metadata.trigger));
                extra.insert(
                    "originalModel".into(),
                    serde_json::json!(metadata.original_model),
                );
                extra.insert(
                    "fallbackModel".into(),
                    serde_json::json!(metadata.fallback_model),
                );
                extra.insert("isMeta".into(), serde_json::json!(false));
                ("system", serde_json::Value::Null)
            }
            ConversationMessage::System { content, .. } => (
                "system",
                serde_json::json!({ "role": "system", "content": content }),
            ),
        };
        if let Some(attachment) = attachments.get(&msg.id()) {
            kind = "attachment";
            inner = serde_json::Value::Null;
            extra.clear();
            extra.insert("attachment".into(), (*attachment).clone());
        }
        // Bare 8-4-4-4-12 lowercase uuid (NOT the `msg:`-prefixed display form)
        // — the JSONL schema + `validate_uuid` regex require the raw uuid.
        let uuid = msg.id().as_uuid().to_string();
        let line_parent_uuid = if resets_chain {
            None
        } else {
            parent_uuid.take()
        };
        let line = JsonlMessage {
            message_type: kind.to_string(),
            uuid: uuid.clone(),
            parent_uuid: line_parent_uuid,
            session_id: session_uuid.to_string(),
            timestamp: match msg {
                ConversationMessage::System {
                    refusal_fallback: Some(metadata),
                    ..
                } => metadata.notice_timestamp.clone(),
                _ => None,
            }
            .unwrap_or_else(|| ts.clone()),
            cwd: cwd.to_string(),
            version: version.to_string(),
            message: inner,
            is_sidechain: false,
            user_type: Some("external".to_string()),
            git_branch: None,
            entrypoint: Some("cli".to_string()),
            slug: None,
            prompt_id: None,
            logical_parent_uuid,
            extra,
        };
        parent_uuid = Some(uuid);
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::{ContentBlock, ConversationMessage};

    fn snapshot(history: &[ConversationMessage]) -> BgSessionSnapshot {
        BgSessionSnapshot {
            history: history.to_vec(),
            model_reminder_attachments: Vec::new(),
        }
    }

    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::User {
            id: lingxi_core::types::MessageId::new(),
            content: vec![ContentBlock::Text {
                text: text.to_string(), citations: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        }
    }

    fn assistant(text: &str) -> ConversationMessage {
        ConversationMessage::Assistant {
            id: lingxi_core::types::MessageId::new(),
            content: vec![ContentBlock::Text {
                text: text.to_string(), citations: None,
            }],
            stop_reason: Some("end_turn".to_string()),
        }
    }

    #[test]
    fn chains_parent_uuids_in_order() {
        let history = vec![user("hi"), assistant("hello"), user("more")];
        let lines =
            history_to_jsonl_lines(&snapshot(&history), "sess-uuid", "/tmp/proj", "9.9.9", "");
        assert_eq!(lines.len(), 3);
        // First line has no parent.
        assert_eq!(lines[0].parent_uuid, None);
        // Each subsequent line chains to the prior line's uuid.
        assert_eq!(
            lines[1].parent_uuid.as_deref(),
            Some(lines[0].uuid.as_str())
        );
        assert_eq!(
            lines[2].parent_uuid.as_deref(),
            Some(lines[1].uuid.as_str())
        );
        // Kinds + trailer fields.
        assert_eq!(lines[0].message_type, "user");
        assert_eq!(lines[1].message_type, "assistant");
        assert_eq!(lines[0].session_id, "sess-uuid");
        assert_eq!(lines[0].cwd, "/tmp/proj");
        assert_eq!(lines[0].version, "9.9.9");
        assert_eq!(lines[0].user_type.as_deref(), Some("external"));
    }

    #[test]
    fn empty_history_yields_no_lines() {
        assert!(history_to_jsonl_lines(&snapshot(&[]), "s", "/c", "1", "").is_empty());
    }

    #[test]
    fn inner_message_carries_role_and_content() {
        let lines = history_to_jsonl_lines(&snapshot(&[user("hi")]), "s", "/c", "1", "");
        assert_eq!(lines[0].message["role"], "user");
        assert!(lines[0].message["content"].is_array());
    }

    #[test]
    fn compact_types_survive_background_snapshot_and_resume() {
        let old = user("old");
        let (boundary, _) = compaction::create_compact_boundary(
            compaction::CompactTrigger::Manual,
            42,
            Some(old.id()),
            None,
            None,
            &[],
        );
        let summary = ConversationMessage::compact_summary(
            lingxi_core::types::MessageId::new(),
            "Summary:\nS".to_string(),
        );
        let history = vec![old, boundary.clone(), summary.clone()];
        let lines = history_to_jsonl_lines(&snapshot(&history), "s", "/c", "1", "");

        assert_eq!(lines[1].parent_uuid, None);
        assert_eq!(
            lines[1]
                .extra
                .get("subtype")
                .and_then(|value| value.as_str()),
            Some("compact_boundary")
        );
        assert_eq!(
            lines[2].extra.get("isCompactSummary"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(
            lines[2].extra.get("isVisibleInTranscriptOnly"),
            Some(&serde_json::Value::Bool(true))
        );

        let state = crate::resume::state_from_messages(uuid::Uuid::nil(), &lines);
        assert_eq!(state.history[1], boundary);
        assert_eq!(state.history[2], summary);
    }

    /// Regression (BGF-1): the parent's active model is stamped onto assistant
    /// lines so the fork copy resumes on THAT model, not `DEFAULT_MODEL`. With no
    /// `model` field the resume loader falls back to the launch default,
    /// silently downgrading a `/model sonnet` (or cross-provider) session.
    #[test]
    fn assistant_lines_carry_the_parent_model() {
        let history = vec![user("hi"), assistant("hello")];
        let lines = history_to_jsonl_lines(
            &snapshot(&history),
            "s",
            "/c",
            "1",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        );
        // Assistant line carries the parent's model; user line does not.
        assert_eq!(
            lines[1].message["model"],
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0"
        );
        assert!(lines[0].message.get("model").is_none());
    }

    /// The stamped model survives a full snapshot → resume round-trip:
    /// `state_from_messages` restores it as the resumed session's active model
    /// (the exact path a backgrounded `/fork` copy takes on boot), so a
    /// non-default parent model is NOT overwritten by `DEFAULT_MODEL`.
    #[test]
    fn resume_restores_forked_parent_model_not_default() {
        let model = "us.anthropic.claude-sonnet-4-5-20250929-v1:0";
        let history = vec![user("hi"), assistant("hello")];
        let lines = history_to_jsonl_lines(&snapshot(&history), "s", "/c", "1", model);
        let sid = uuid::Uuid::nil();
        let state = crate::resume::state_from_messages(sid, &lines);
        assert_eq!(state.model, model, "resume must restore the parent's model");
        assert_ne!(
            state.model,
            crate::config::DEFAULT_MODEL,
            "must NOT fall back to the launch default"
        );
    }

    #[test]
    fn model_fallback_envelope_round_trips_without_changing_parent_model() {
        use lingxi_core::types::{MessageId, ModelFallbackMetadata};

        let fallback_id = MessageId::new();
        let fallback_content = "The selected model was overloaded; continuing with Sonnet.";
        let metadata = ModelFallbackMetadata {
            trigger: "overloaded".into(),
            original_model: "claude-opus-4-8".into(),
            fallback_model: "claude-sonnet-5".into(),
        };
        let fallback_message = ConversationMessage::System {
            id: fallback_id,
            content: fallback_content.into(),
            subtype: Some("model_fallback".into()),
            compact_metadata: None,
            model_fallback: Some(metadata.clone()),
            refusal_fallback: None,
        };
        let parent_model = "claude-opus-4-8";
        let history = vec![user("do the task"), fallback_message, assistant("done")];
        let lines = history_to_jsonl_lines(&snapshot(&history), "s", "/c", "1", parent_model);

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].message_type, "system");
        assert!(lines[1].message.is_null());
        assert_eq!(lines[1].uuid, fallback_id.as_uuid().to_string());
        assert_eq!(
            lines[1].parent_uuid.as_deref(),
            Some(lines[0].uuid.as_str())
        );
        assert_eq!(
            lines[2].parent_uuid.as_deref(),
            Some(lines[1].uuid.as_str())
        );
        assert_eq!(
            serde_json::Value::Object(lines[1].extra.clone()),
            serde_json::json!({
                "subtype":"model_fallback",
                "content":fallback_content,
                "level":"warning",
                "trigger":"overloaded",
                "originalModel":"claude-opus-4-8",
                "fallbackModel":"claude-sonnet-5",
                "isMeta":false
            })
        );

        // Exercise the actual flattened JSONL serializer and reader shape before
        // the same resume loader used for a forked background session.
        let jsonl = lines
            .iter()
            .map(|line| serde_json::to_string(line))
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        let fallback_json = jsonl.lines().nth(1).unwrap();
        assert!(
            fallback_json.starts_with(&format!(
                "{{\"parentUuid\":\"{}\",\"isSidechain\":false,\"type\":\"system\",\
                 \"subtype\":\"model_fallback\",\"content\":\"{fallback_content}\",\
                 \"level\":\"warning\",\"trigger\":\"overloaded\",\
                 \"originalModel\":\"claude-opus-4-8\",\
                 \"fallbackModel\":\"claude-sonnet-5\",\"isMeta\":false,\
                 \"uuid\":\"{}\",\"timestamp\":\"",
                lines[0].uuid,
                fallback_id.as_uuid()
            )),
            "model-fallback JSONL keeps the native flattened system envelope: {fallback_json}"
        );
        assert!(!fallback_json.contains("\"message\":"));

        let decoded = jsonl
            .lines()
            .map(serde_json::from_str::<session::JsonlMessage>)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let resumed = crate::resume::state_from_messages(uuid::Uuid::nil(), &decoded);
        assert_eq!(resumed.model, parent_model);
        assert_ne!(
            resumed.model, metadata.fallback_model,
            "the marker records the served fallback but must not replace the parent session route"
        );
        let resumed_fallback = resumed
            .history
            .iter()
            .find(|message| message.id() == fallback_id)
            .expect("resume restores the typed model-fallback system row");
        assert!(matches!(
            resumed_fallback,
            ConversationMessage::System {
                id,
                content,
                subtype: Some(subtype),
                model_fallback: Some(restored),
                ..
            } if *id == fallback_id
                && content.as_str() == fallback_content
                && subtype.as_str() == "model_fallback"
                && restored == &metadata
        ));
    }

    #[test]
    fn model_fallback_metadata_is_not_promoted_for_other_system_subtypes() {
        use lingxi_core::types::{MessageId, ModelFallbackMetadata};

        let message = ConversationMessage::System {
            id: MessageId::new(),
            content: "ordinary system notice".into(),
            subtype: Some("other_notice".into()),
            compact_metadata: None,
            model_fallback: Some(ModelFallbackMetadata {
                trigger: "overloaded".into(),
                original_model: "claude-opus-4-8".into(),
                fallback_model: "claude-sonnet-5".into(),
            }),
            refusal_fallback: None,
        };
        let lines = history_to_jsonl_lines(&snapshot(&[message]), "s", "/c", "1", "");

        assert_eq!(lines[0].message_type, "system");
        assert_eq!(lines[0].message["role"], "system");
        assert_eq!(lines[0].message["content"], "ordinary system notice");
        assert!(lines[0].extra.is_empty());
    }

    /// An empty parent model (defensive) omits the field entirely — the resume
    /// loader then keeps its own `DEFAULT_MODEL` seed rather than resuming onto
    /// a `""`/`<synthetic>`-style id that would fail `resolve_in`.
    #[test]
    fn empty_model_omits_the_field() {
        let history = vec![assistant("hello")];
        let lines = history_to_jsonl_lines(&snapshot(&history), "s", "/c", "1", "");
        assert!(lines[0].message.get("model").is_none());
    }

    #[tokio::test]
    async fn fork_and_background_handles_preserve_structured_reminders_through_resume() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use lingxi_core::host::bg_session_forker::{BgForkError, BgSessionForker};
        use lingxi_core::host::OrchestratorHandle;
        use std::sync::{Arc, Mutex};
        use tool_api::registry::ToolRegistry;

        struct CaptureForker(Mutex<Vec<Vec<JsonlMessage>>>);
        #[async_trait::async_trait]
        impl BgSessionForker for CaptureForker {
            async fn fork_to_background(
                &self,
                conversation: &BgSessionSnapshot,
                _system_prompt: Option<Arc<str>>,
                _prompt: &str,
                model: &str,
            ) -> Result<String, BgForkError> {
                self.0.lock().unwrap().push(history_to_jsonl_lines(
                    conversation,
                    &uuid::Uuid::nil().to_string(),
                    "/tmp",
                    "test",
                    model,
                ));
                Ok("forked".into())
            }

            async fn resume_to_background(&self, _session_id: &str) -> Result<String, BgForkError> {
                unreachable!("test exercises live copies")
            }
        }

        let forker = Arc::new(CaptureForker(Mutex::new(Vec::new())));
        let orch = crate::ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_bg_session_forker(forker.clone());
        orch.session().lock().await.history.push(user("original"));
        let mcp = serde_json::json!({
            "type":"mcp_instructions_delta",
            "addedNames":["records"],
            "addedBlocks":["## records\nRead the project index first."],
            "addedServerInstructions":["Read the project index first."],
            "removedNames":[],
        });
        let mcp_message = orch
            .persist_model_reminder(
                ConversationMessage::user_meta(
                    lingxi_core::types::MessageId::new(),
                    crate::prompt::mcp_instructions::render(&mcp).unwrap(),
                ),
                mcp.clone(),
            )
            .await;
        let token_text = "<total_tokens>42 tokens left</total_tokens>";
        let token_attachment =
            serde_json::json!({"type":"total_tokens_reminder", "text":token_text});
        let token_message = orch
            .persist_model_reminder(
                ConversationMessage::user_meta(
                    lingxi_core::types::MessageId::new(),
                    format!("<system-reminder>\n{token_text}\n</system-reminder>"),
                ),
                token_attachment.clone(),
            )
            .await;
        OrchestratorHandle::fork_to_background_session(&orch, "")
            .await
            .unwrap();
        OrchestratorHandle::background_conversation(&orch, Default::default())
            .await
            .unwrap();
        let captures = forker.0.lock().unwrap().clone();
        assert_eq!(captures.len(), 2);
        let original = orch.session().lock().await.history.clone();
        for lines in captures {
            for (index, payload, message) in [
                (1, &mcp, &mcp_message),
                (2, &token_attachment, &token_message),
            ] {
                assert_eq!(lines[index].message_type, "attachment");
                assert!(lines[index].message.is_null());
                assert_eq!(lines[index].extra["attachment"], *payload);
                assert_eq!(lines[index].uuid, message.id().as_uuid().to_string());
                assert!(!lines[index].extra.contains_key("isMeta"));
            }
            // The host writes serialized rows; replay consumes exactly those
            // rows, including raw server baselines and the original identities.
            let serialized = serde_json::to_string(&lines).unwrap();
            let decoded: Vec<JsonlMessage> = serde_json::from_str(&serialized).unwrap();
            let state = crate::resume::state_from_messages(uuid::Uuid::nil(), &decoded);
            assert_eq!(state.history, original);
            let attachments = crate::resume::model_reminder_attachments_from_messages(&decoded);
            assert_eq!(attachments[&mcp_message.id()], mcp);
            assert_eq!(attachments[&token_message.id()], token_attachment);
            let prior: Vec<serde_json::Value> = decoded
                .iter()
                .map(|line| serde_json::to_value(line).unwrap())
                .collect();
            assert!(crate::prompt::mcp_instructions::attachment(
                &[("records".into(), "Read the project index first.".into())],
                &["records".into()],
                &[],
                &prior,
                true,
            )
            .is_none());
            let removed =
                crate::prompt::mcp_instructions::attachment(&[], &[], &[], &prior, true).unwrap();
            assert_eq!(removed["removedNames"], serde_json::json!(["records"]));
        }
    }
}
