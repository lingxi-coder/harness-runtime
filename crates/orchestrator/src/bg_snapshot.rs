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
//! The assistant completion reason must survive the copy: restored side queries
//! distinguish completed answers from an unfinished streamed response. The
//! assistant line's `model` is also part of the resumed state. The
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
use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
use lingxi_core::types::{ContentBlock, ConversationMessage};
use session::JsonlMessage;

fn source<T: serde::Serialize + ?Sized>(value: &T) -> Source {
    Source::parse(&serde_json::to_string(value).expect("typed transcript value"))
        .expect("valid transcript source")
}

fn source_content_blocks(content: &[ContentBlock]) -> Source {
    let blocks = content
        .iter()
        .map(|block| match block {
            ContentBlock::TextJsUtf16 {
                text,
                utf16_code_units,
                citations,
            } => {
                // JSONL stores the native text block and carries exact UTF-16
                // units in the row's private override map; do not persist the
                // in-memory `text_js_utf16` carrier as a different wire block.
                let text = Source::root_string(text.clone(), utf16_code_units.clone())
                    .expect("text block has a valid UTF-16 display projection");
                let mut projection = object(vec![("type", "text".into()), ("text", text)]);
                if let Some(citations) = citations {
                    let citations = citations
                        .clone()
                        .unwrap_or_else(|| Source::parse("null").expect("JSON null"));
                    projection
                        .set_field("citations", citations)
                        .expect("text block object");
                }
                projection
            }
            _ => source(block),
        })
        .collect();
    Source::array(blocks).expect("content blocks form a JSON array")
}

fn object(fields: Vec<(&str, Source)>) -> Source {
    Source::object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
    .expect("transcript object")
}

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
        let mut extra = Source::parse("{}").expect("empty metadata");
        let mut tool_use_result_projection = None;
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
                for (key, enabled) in [
                    ("isMeta", *is_meta),
                    ("isVisibleInTranscriptOnly", *is_visible_in_transcript_only),
                    ("isCompactSummary", *is_compact_summary),
                ] {
                    if enabled {
                        extra
                            .set_field(key, source(&true))
                            .expect("metadata object");
                    }
                }
                (
                    "user",
                    object(vec![
                        ("role", "user".into()),
                        ("content", source_content_blocks(content)),
                    ]),
                )
            }
            ConversationMessage::Assistant {
                id,
                content,
                stop_reason,
                resumable,
                ..
            } => {
                let mut inner = object(vec![
                    ("role", "assistant".into()),
                    ("content", source_content_blocks(content)),
                ]);
                let assistant_model = if conversation
                    .native_request_history
                    .synthetic_closing_ids
                    .contains(id)
                {
                    "<synthetic>"
                } else {
                    model
                };
                if !assistant_model.is_empty() {
                    inner
                        .set_field("model", assistant_model.into())
                        .expect("assistant object");
                }
                if let Some(reason) = stop_reason {
                    inner
                        .set_field("stop_reason", reason.as_str().into())
                        .expect("assistant object");
                }
                if let Some(resumable) = resumable {
                    inner
                        .set_field("resumable", resumable.clone())
                        .expect("assistant object");
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
                let mut compact_metadata = source(metadata);
                compact_metadata
                    .remove_field("logicalParentUuid")
                    .expect("compact metadata object");
                extra = object(vec![
                    ("subtype", subtype.as_str().into()),
                    ("content", content.clone()),
                    ("level", "info".into()),
                    ("compactMetadata", compact_metadata),
                ]);
                ("system", source(&()))
            }
            ConversationMessage::System {
                content,
                subtype: Some(subtype),
                refusal_fallback: Some(metadata),
                ..
            } if subtype == "model_refusal_fallback" => {
                extra = object(vec![
                    ("subtype", subtype.as_str().into()),
                    ("content", content.clone()),
                    ("level", "warning".into()),
                    ("isMeta", source(&false)),
                ]);
                extra
                    .merge_object_projection(&source(metadata))
                    .expect("refusal metadata object");
                if !extra.contains_property("apiRefusalExplanation") {
                    extra
                        .set_field("apiRefusalExplanation", source(&()))
                        .expect("metadata object");
                }
                ("system", source(&()))
            }
            ConversationMessage::System {
                content,
                subtype: Some(subtype),
                model_fallback: Some(metadata),
                ..
            } if subtype == "model_fallback" => {
                extra = object(vec![
                    ("subtype", subtype.as_str().into()),
                    ("content", content.clone()),
                    ("level", "warning".into()),
                    ("trigger", metadata.trigger.as_str().into()),
                    ("originalModel", metadata.original_model.as_str().into()),
                    ("fallbackModel", metadata.fallback_model.as_str().into()),
                    ("isMeta", source(&false)),
                ]);
                ("system", source(&()))
            }
            ConversationMessage::System { content, .. } => (
                "system",
                object(vec![
                    ("role", "system".into()),
                    ("content", content.clone()),
                ]),
            ),
        };
        let mut attachment_projection = None;
        if let Some(attachment) = attachments.get(&msg.id()) {
            kind = "attachment";
            attachment_projection = Some((*attachment).clone());
            inner = source(&());
            extra = Source::parse("{}").expect("empty attachment metadata");
        }
        if kind == "attachment"
            && conversation
                .native_request_history
                .before_closing_line_ids
                .contains(&msg.id())
            && attachment_projection
                .as_ref()
                .and_then(|payload| payload.string_at("/type"))
                .is_some_and(crate::conversation::closing_placement::is_tee_attachment_type)
        {
            extra
                .set_field("placement", Source::from("before_closing_line"))
                .expect("attachment metadata is an object");
        }
        if kind == "user" {
            if let Some(envelope) = conversation
                .native_request_history
                .task_delivery_envelopes
                .get(&msg.id())
            {
                for (key, projection) in [
                    (
                        "verifiedSlackHumanTurn",
                        &envelope.verified_slack_human_turn,
                    ),
                    ("promptSource", &envelope.prompt_source),
                ] {
                    if let Some(projection) = projection {
                        extra
                            .set_field(key, projection.clone())
                            .expect("user envelope metadata is an object");
                    }
                }
                if let Some(tool_use_result) = &envelope.tool_use_result {
                    tool_use_result_projection = Some(tool_use_result.clone());
                }
            }
            if let Some(task_delivery) = conversation
                .native_request_history
                .task_deliveries
                .get(&msg.id())
            {
                extra
                    .set_field("taskDelivery", task_delivery.clone())
                    .expect("user metadata is an object");
            }
        }
        // Bare 8-4-4-4-12 lowercase uuid (NOT the `msg:`-prefixed display form)
        // — the JSONL schema + `validate_uuid` regex require the raw uuid.
        let uuid = msg.id().as_uuid().to_string();
        let line_parent_uuid = if resets_chain {
            None
        } else {
            parent_uuid.take()
        };
        let mut line = JsonlMessage {
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

            attachment_projection,
            tool_use_result_projection,
            mcp_meta_projection: None,
            turn_companion: None,
            usage_limit_note: None,
            origin_projection: None,
            turn_origin_projection: None,
        };
        if let ConversationMessage::User {
            turn_companion,
            usage_limit_note,
            origin,
            turn_origin,
            ..
        } = msg
        {
            line.turn_companion = *turn_companion;
            line.usage_limit_note = *usage_limit_note;
            line.origin_projection = origin.clone();
            line.turn_origin_projection = turn_origin.clone();
        }
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
            native_request_history: lingxi_core::host::NativeRequestHistoryMetadata::default(),
        }
    }

    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::User {
            api_message_override: None,
            interrupted_by_shutdown: None,
            refused_message_id: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                citations: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
        }
    }

    fn assistant(text: &str) -> ConversationMessage {
        ConversationMessage::Assistant {
            per_turn_effort: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                citations: None,
            }],
            stop_reason: Some("end_turn".to_string()),

            resumable: None,
        }
    }

    #[test]
    fn fork_snapshot_preserves_source_text_metadata_and_attachment() {
        let id = lingxi_core::types::MessageId::new();
        let units = vec![0xd800, 0x03a9, 0xdc00];
        let mut message = ConversationMessage::user_meta_js_utf16(
            id,
            String::from_utf16_lossy(&units),
            units.clone(),
        );
        if let ConversationMessage::User {
            turn_companion,
            origin,
            ..
        } = &mut message
        {
            *turn_companion = Some(Some(true));
            *origin = Some(Source::parse(r#"{"kind":"fixture","text":"\ud800"}"#).unwrap());
        }
        let lines = history_to_jsonl_lines(
            &snapshot(&[message.clone()]),
            "00000000-0000-4000-8000-000000000001",
            "/tmp",
            "test",
            "model",
        );
        let row = source(&lines[0]);
        assert_eq!(row.string_units("/message/content/0/text"), Some(units));
        assert_eq!(row.string_units("/origin/text"), Some(vec![0xd800]));
        assert_eq!(lines[0].turn_companion, Some(Some(true)));

        let attachment = Source::parse(r#"{"type":"fixture","body":"\ud800"}"#).unwrap();
        let mut snapshot = snapshot(&[message]);
        snapshot
            .model_reminder_attachments
            .push((id, attachment.clone()));
        let lines = history_to_jsonl_lines(
            &snapshot,
            "00000000-0000-4000-8000-000000000001",
            "/tmp",
            "test",
            "model",
        );
        let row = source(&lines[0]);
        assert_eq!(row.string_at("/type"), Some("attachment"));
        assert_eq!(row.string_units("/attachment/body"), Some(vec![0xd800]));
        assert_eq!(lines[0].attachment_projection.as_ref(), Some(&attachment));
    }

    #[test]
    fn fork_snapshot_carries_native_placement_task_delivery_and_synthetic_identity() {
        let mut task_user = user("task output");
        if let ConversationMessage::User { origin, .. } = &mut task_user {
            *origin = Some(Source::parse(r#"{"kind":"human"}"#).unwrap());
        }
        let task_user_id = task_user.id();
        let closing = assistant("No response requested.");
        let closing_id = closing.id();
        let environment = ConversationMessage::user_meta(
            lingxi_core::types::MessageId::new(),
            "environment announcement".into(),
        );
        let environment_id = environment.id();
        let environment_payload = Source::parse(r#"{"type":"environment","value":"x"}"#).unwrap();
        let task_delivery =
            Source::parse(r#"{"taskId":"r12345678","status":"completed"}"#).unwrap();
        let mut conversation = snapshot(&[task_user, closing, environment]);
        conversation
            .model_reminder_attachments
            .push((environment_id, environment_payload.clone()));
        conversation
            .native_request_history
            .before_closing_line_ids
            .insert(environment_id);
        conversation
            .native_request_history
            .synthetic_closing_ids
            .insert(closing_id);
        conversation
            .native_request_history
            .task_deliveries
            .insert(task_user_id, task_delivery.clone());
        conversation
            .native_request_history
            .task_delivery_envelopes
            .insert(
                task_user_id,
                lingxi_core::host::TaskDeliveryHistoryEnvelope {
                    verified_slack_human_turn: Some(Source::parse("null").unwrap()),
                    prompt_source: Some(Source::parse("null").unwrap()),
                    tool_use_result: Some(Source::parse("null").unwrap()),
                },
            );

        let lines = history_to_jsonl_lines(
            &conversation,
            "00000000-0000-4000-8000-000000000001",
            "/tmp",
            "test",
            "claude-opus-4-8",
        );
        let task_row = lines
            .iter()
            .find(|row| row.message_type == "user")
            .expect("task user row");
        assert!(task_row
            .extra
            .subprojection("/taskDelivery")
            .unwrap()
            .semantically_eq(&task_delivery));
        assert_eq!(
            task_row
                .origin_projection
                .as_ref()
                .and_then(|origin| origin.string_at("/kind")),
            Some("human")
        );
        assert!(task_row
            .extra
            .subprojection("/verifiedSlackHumanTurn")
            .unwrap()
            .at_pointer("")
            .unwrap()
            .is_null());
        assert!(task_row
            .extra
            .subprojection("/promptSource")
            .unwrap()
            .at_pointer("")
            .unwrap()
            .is_null());
        assert!(task_row
            .tool_use_result_projection
            .as_ref()
            .unwrap()
            .at_pointer("")
            .unwrap()
            .is_null());
        let closing_row = lines
            .iter()
            .find(|row| row.message_type == "assistant")
            .expect("closing assistant row");
        assert_eq!(closing_row.message.string_at("/model"), Some("<synthetic>"));
        let environment_row = lines
            .iter()
            .find(|row| row.attachment_projection.is_some())
            .expect("environment attachment row");
        assert_eq!(
            environment_row.extra.string_at("/placement"),
            Some("before_closing_line")
        );
        assert_eq!(
            environment_row
                .attachment_projection
                .as_ref()
                .unwrap()
                .string_at("/placement"),
            None,
            "Native placement belongs to the outer JSONL row"
        );
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
        assert_eq!(lines[0].message.string_at("/role"), Some("user"));
        assert!(lines[0].message.array_len("/content").is_some());
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
            lines[1].extra.string_at("/subtype"),
            Some("compact_boundary")
        );
        assert_eq!(
            lines[2]
                .extra
                .at_pointer("/isCompactSummary")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_eq!(
            lines[2]
                .extra
                .at_pointer("/isVisibleInTranscriptOnly")
                .and_then(|value| value.as_bool()),
            Some(true)
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
            lines[1].message.string_at("/model"),
            Some("us.anthropic.claude-sonnet-4-5-20250929-v1:0")
        );
        assert!(lines[0].message.at_pointer("/model").is_none());
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
        assert!(matches!(
            state.history.last(),
            Some(ConversationMessage::Assistant { stop_reason: Some(reason), .. })
                if reason == "end_turn"
        ));
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
            api_system: None,
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
        assert!(lines[1].message.at_pointer("").unwrap().is_null());
        assert_eq!(lines[1].uuid, fallback_id.as_uuid().to_string());
        assert_eq!(
            lines[1].parent_uuid.as_deref(),
            Some(lines[0].uuid.as_str())
        );
        assert_eq!(
            lines[2].parent_uuid.as_deref(),
            Some(lines[1].uuid.as_str())
        );
        assert!(lines[1].extra.semantically_eq(&source(&serde_json::json!({
            "subtype":"model_fallback",
            "content":fallback_content,
            "level":"warning",
            "trigger":"overloaded",
            "originalModel":"claude-opus-4-8",
            "fallbackModel":"claude-sonnet-5",
            "isMeta":false
        }))));

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
            api_system: None,
                id,
                content,
                subtype: Some(subtype),
                model_fallback: Some(restored),
                ..
            } if *id == fallback_id
                && content.string_at("") == Some(fallback_content)
                && subtype.as_str() == "model_fallback"
                && restored == &metadata
        ));
    }

    #[test]
    fn model_fallback_metadata_is_not_promoted_for_other_system_subtypes() {
        use lingxi_core::types::{MessageId, ModelFallbackMetadata};

        let message = ConversationMessage::System {
            api_system: None,
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
        assert_eq!(lines[0].message.string_at("/role"), Some("system"));
        assert_eq!(
            lines[0].message.string_at("/content"),
            Some("ordinary system notice")
        );
        assert!(lines[0].extra.object_entries().unwrap().is_empty());
    }

    /// An empty parent model (defensive) omits the field entirely — the resume
    /// loader then keeps its own `DEFAULT_MODEL` seed rather than resuming onto
    /// a `""`/`<synthetic>`-style id that would fail `resolve_in`.
    #[test]
    fn empty_model_omits_the_field() {
        let history = vec![assistant("hello")];
        let lines = history_to_jsonl_lines(&snapshot(&history), "s", "/c", "1", "");
        assert!(lines[0].message.at_pointer("/model").is_none());
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
                _system_prompt: Option<Arc<lingxi_core::types::utf16_json::Utf16JsonProjection>>,
                _prompt: &lingxi_core::types::utf16_json::Utf16JsonProjection,
                model: &str,
            ) -> Result<Source, BgForkError> {
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
        let mcp_source =
            lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(&mcp).unwrap();
        let mcp_content = crate::prompt::mcp_instructions::render_source(&mcp_source).unwrap();
        let mcp_message = orch
            .persist_model_reminder(
                ConversationMessage::user_meta_js_utf16(
                    lingxi_core::types::MessageId::new(),
                    mcp_content.string_at("").unwrap().to_owned(),
                    mcp_content.string_units("").unwrap(),
                ),
                mcp_source.clone(),
            )
            .await;
        let token_text = "<total_tokens>42 tokens left</total_tokens>";
        let token_attachment =
            serde_json::json!({"type":"total_tokens_reminder", "text":token_text});
        let token_attachment_source =
            lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &token_attachment,
            )
            .unwrap();
        let token_message = orch
            .persist_model_reminder(
                ConversationMessage::user_meta(
                    lingxi_core::types::MessageId::new(),
                    format!("<system-reminder>\n{token_text}\n</system-reminder>"),
                ),
                token_attachment_source.clone(),
            )
            .await;
        OrchestratorHandle::fork_to_background_session(&orch, &"".into())
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
                assert!(lines[index].message.at_pointer("").unwrap().is_null());
                assert!(lines[index]
                    .attachment_projection
                    .as_ref()
                    .unwrap()
                    .semantically_eq(&source(payload)));
                assert_eq!(lines[index].uuid, message.id().as_uuid().to_string());
                assert!(!lines[index].extra.contains_property("isMeta"));
            }
            // The host writes serialized rows; replay consumes exactly those
            // rows, including raw server baselines and the original identities.
            let serialized = serde_json::to_string(&lines).unwrap();
            let decoded: Vec<JsonlMessage> = serde_json::from_str(&serialized).unwrap();
            let state = crate::resume::state_from_messages(uuid::Uuid::nil(), &decoded);
            assert_eq!(state.history, original);
            let attachments = crate::resume::model_reminder_attachments_from_messages(&decoded);
            assert!(attachments[&mcp_message.id()].semantically_eq(&mcp_source));
            assert!(attachments[&token_message.id()].semantically_eq(&token_attachment_source));
            let mut prior_row = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                r#"{"type":"attachment"}"#,
            )
            .unwrap();
            prior_row
                .set_field("attachment", mcp_source.clone())
                .unwrap();
            let prior = vec![prior_row];
            assert!(crate::prompt::mcp_instructions::attachment(
                &[(
                    "records".into(),
                    lingxi_core::types::utf16_json::Utf16JsonProjection::from(
                        "Read the project index first."
                    ),
                )],
                &["records".into()],
                &[],
                &prior,
                true,
            )
            .is_none());
            let removed =
                crate::prompt::mcp_instructions::attachment(&[], &[], &[], &prior, true).unwrap();
            assert_eq!(removed.string_at("/removedNames/0"), Some("records"));
        }
    }
}
