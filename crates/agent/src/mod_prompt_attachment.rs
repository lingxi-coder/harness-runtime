//! Screen persistent child-agent attachments in each outgoing model snapshot.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use hooks::mods::{ModError, ModHost};
use lingxi_core::types::{AgentId, ContentBlock, ConversationMessage, MessageId};
use serde_json::{Value, json};

#[derive(Default)]
pub(crate) struct ChildPromptAttachments {
    descriptors: HashMap<MessageId, (String, Value)>,
    answers: HashMap<MessageId, (Value, (u64, u64), u64, Option<String>)>,
}

impl ChildPromptAttachments {
    pub(crate) fn register(&mut self, message: &ConversationMessage, kind: &str, origin: Value) {
        self.descriptors
            .insert(message.id(), (kind.to_owned(), origin));
    }

    /// Restore only host-stamped tool.call attachments whose original body
    /// still matches the transcript message. The restore loader inserts these
    /// metadata markers from a dedicated transcript field, never from model
    /// text; the markers must not reach the provider.
    pub(crate) fn restore_from_history(&mut self, history: &mut Vec<ConversationMessage>) {
        let markers = history
            .iter()
            .filter_map(|message| match message {
                ConversationMessage::System {
                    content,
                    subtype: Some(subtype),
                    ..
                } if subtype == "mod_attachment_source" => {
                    let value =
                        lingxi_core::types::utf16_json::Utf16JsonProjection::parse(content).ok()?;
                    let id: MessageId =
                        serde_json::from_value(value.value.get("messageId")?.clone()).ok()?;
                    Some((id, value.subprojection("/attachment").ok()?))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        history.retain(|message| {
            !matches!(message, ConversationMessage::System { subtype: Some(subtype), .. }
                if subtype == "mod_attachment_source")
        });
        for (id, attachment) in markers {
            if attachment.value.get("type").and_then(Value::as_str)
                != Some("hook_additional_context")
                || attachment.value.get("hookName").and_then(Value::as_str) != Some("tool.call")
                || attachment.value.get("hookEvent").and_then(Value::as_str) != Some("PostToolUse")
                || !attachment
                    .value
                    .get("toolUseID")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id.ends_with("-context"))
            {
                continue;
            }
            let Some(content) = attachment.value.get("content").and_then(Value::as_array) else {
                continue;
            };
            let Some(content) = content
                .iter()
                .map(Value::as_str)
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            if content.is_empty() {
                continue;
            }
            let original = format!(
                "<system-reminder>\ntool.call hook additional context: {}\n</system-reminder>",
                content.join("\n")
            );
            if let Some(message) = history.iter().find(|message| {
                message.id() == id && message.is_meta() && message.text_content() == original
            }) {
                self.register(
                    message,
                    "hook_additional_context",
                    json!({"kind":"plugin","event":"tool.call"}),
                );
            }
        }
    }

    pub(crate) async fn screen(
        &mut self,
        messages: &mut Vec<ConversationMessage>,
        host: Option<&Arc<ModHost>>,
        cwd: &Path,
        agent_id: AgentId,
    ) {
        let Some(host) = host.filter(|host| host.has_event("prompt.attachment")) else {
            return;
        };
        if self.descriptors.is_empty() {
            return;
        }
        let session = host.bound_session();
        let generation = match session.as_ref() {
            Some(session) => session.prompt_attachment_generation().await,
            None => 0,
        };
        let identity = host.registration_identity();
        let source = std::mem::take(messages);
        for mut message in source {
            let message_id = message.id();
            let Some((kind, origin)) = self.descriptors.get(&message_id) else {
                messages.push(message);
                continue;
            };
            let ConversationMessage::User { content, .. } = &mut message else {
                messages.push(message);
                continue;
            };
            let [ContentBlock::Text { text, .. }] = content.as_mut_slice() else {
                messages.push(message);
                continue;
            };
            let inner = text
                .strip_prefix("<system-reminder>\n")
                .and_then(|body| body.strip_suffix("\n</system-reminder>"));
            let wrapped = inner.is_some();
            let body = inner.unwrap_or(text).to_owned();
            if body.trim().is_empty() {
                messages.push(message);
                continue;
            }
            let input = json!({
                "type":kind,
                "text":body,
                "origin":origin,
                "agentId":agent_id.as_uuid().to_string(),
            });
            let cached =
                self.answers
                    .get(&message_id)
                    .and_then(|(source, catalog, epoch, answer)| {
                        (source == &input && catalog == &identity && *epoch == generation)
                            .then_some(answer.clone())
                    });
            let answer = if let Some(answer) = cached {
                answer
            } else {
                let pinned_kind = kind.clone();
                let pinned_origin = origin.clone();
                let pinned_agent = agent_id.as_uuid().to_string();
                let core = move |forwarded: Value| {
                    let kind = pinned_kind.clone();
                    let origin = pinned_origin.clone();
                    let agent = pinned_agent.clone();
                    async move {
                        if forwarded.get("type").and_then(Value::as_str) != Some(kind.as_str())
                            || forwarded.get("origin") != Some(&origin)
                            || forwarded.get("agentId").and_then(Value::as_str)
                                != Some(agent.as_str())
                        {
                            return Err(ModError::Hook(
                                "prompt.attachment type, origin, and agentId are pinned".into(),
                            ));
                        }
                        let text = forwarded
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or_else(|| ModError::Hook("prompt.attachment needs text".into()))?;
                        Ok(json!({"text":text}))
                    }
                };
                let result = if let Some(session) = session.as_ref() {
                    let log_session = session.clone();
                    let toast_session = session.clone();
                    let status_session = session.clone();
                    host.dispatch_with_ui_at_session_cwd(
                        "prompt.attachment",
                        input.clone(),
                        session.as_ref(),
                        cwd,
                        core,
                        move |plugin, text| {
                            let session = log_session.clone();
                            async move { session.emit_mod_log(&plugin, &text).await }
                        },
                        move |plugin, text, timeout_ms| {
                            let session = toast_session.clone();
                            async move { session.emit_mod_toast(&plugin, &text, timeout_ms).await }
                        },
                        move |plugin, text| {
                            let session = status_session.clone();
                            async move { session.emit_mod_status(&plugin, text.as_deref()).await }
                        },
                    )
                    .await
                } else {
                    host.dispatch_with_log_at(
                        "prompt.attachment",
                        input.clone(),
                        cwd,
                        core,
                        |_, _| async {},
                    )
                    .await
                };
                let answer = match result {
                    Ok(result) if result.get("text") == Some(&Value::Null) => None,
                    Ok(result) => result
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    Err(error) => {
                        tracing::warn!(%error, "child prompt.attachment Mod failed");
                        Some(body)
                    }
                };
                let current_generation = match session.as_ref() {
                    Some(session) => session.prompt_attachment_generation().await,
                    None => 0,
                };
                if current_generation == generation {
                    self.answers
                        .insert(message_id, (input, identity, generation, answer.clone()));
                }
                answer
            };
            if let Some(answer) = answer {
                *text = if wrapped {
                    format!("<system-reminder>\n{answer}\n</system-reminder>")
                } else {
                    answer
                };
                messages.push(message);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hooks::mods::ModSessionContext;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn restored_source_marker_rebuilds_only_matching_tool_context_provenance() {
        let message = ConversationMessage::user_meta(
            MessageId::new(),
            "<system-reminder>\ntool.call hook additional context: first\nsecond\n</system-reminder>"
                .into(),
        );
        let attachment = json!({
            "type":"hook_additional_context",
            "content":["first","second"],
            "hookName":"tool.call",
            "toolUseID":"toolu_child-context",
            "hookEvent":"PostToolUse",
        });
        let marker = ConversationMessage::System {
            id: MessageId::new(),
            content: json!({"messageId":message.id(),"attachment":attachment}).to_string(),
            subtype: Some("mod_attachment_source".into()),
            compact_metadata: None,
            model_fallback: None,
            refusal_fallback: None,
        };
        let mut history = vec![marker.clone(), message.clone()];
        let mut attachments = ChildPromptAttachments::default();
        attachments.restore_from_history(&mut history);
        assert_eq!(history, vec![message.clone()]);
        assert_eq!(
            attachments.descriptors.get(&message.id()),
            Some(&(
                "hook_additional_context".into(),
                json!({"kind":"plugin","event":"tool.call"})
            ))
        );

        let altered = ConversationMessage::user_meta(message.id(), "different".into());
        let mut history = vec![marker, altered];
        let mut attachments = ChildPromptAttachments::default();
        attachments.restore_from_history(&mut history);
        assert!(attachments.descriptors.is_empty());
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn restored_source_marker_parses_exact_utf16_attachment_content() {
        let mut attachment = lingxi_core::types::utf16_json::Utf16JsonProjection::plain(json!({
            "type":"hook_additional_context",
            "content":["first�"],
            "hookName":"tool.call",
            "toolUseID":"toolu_child-context",
            "hookEvent":"PostToolUse",
        }));
        let mut content_units = "first".encode_utf16().collect::<Vec<_>>();
        content_units.push(0xd800);
        attachment
            .strings
            .push(lingxi_core::types::utf16_json::Utf16JsonString {
                pointer: "/content/0".into(),
                code_units: content_units.clone(),
            });
        let attachment_json = attachment.to_json_string().unwrap();
        let message_text_units = [
            "<system-reminder>\ntool.call hook additional context: "
                .encode_utf16()
                .collect::<Vec<_>>(),
            content_units,
            "\n</system-reminder>".encode_utf16().collect::<Vec<_>>(),
        ]
        .concat();
        let message = ConversationMessage::user_meta_js_utf16(
            MessageId::new(),
            String::from_utf16_lossy(&message_text_units),
            message_text_units,
        );
        let marker = ConversationMessage::System {
            id: MessageId::new(),
            content: format!(
                "{{\"messageId\":{},\"attachment\":{attachment_json}}}",
                serde_json::to_string(&message.id()).unwrap()
            ),
            subtype: Some("mod_attachment_source".into()),
            compact_metadata: None,
            model_fallback: None,
            refusal_fallback: None,
        };
        let mut history = vec![marker, message.clone()];
        let mut attachments = ChildPromptAttachments::default();

        attachments.restore_from_history(&mut history);

        assert_eq!(history, vec![message.clone()]);
        assert!(attachments.descriptors.contains_key(&message.id()));
    }

    struct Session {
        cwd: PathBuf,
        generation: AtomicU64,
    }

    #[async_trait::async_trait]
    impl ModSessionContext for Session {
        fn cwd(&self) -> PathBuf {
            self.cwd.clone()
        }

        fn root(&self) -> PathBuf {
            self.cwd.clone()
        }

        async fn model(&self) -> String {
            "test-model".into()
        }

        async fn id(&self) -> String {
            "test-session".into()
        }

        async fn turns(&self) -> u64 {
            1
        }

        async fn invalidate_prompt_attachment(&self) -> Result<(), ModError> {
            self.generation.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn prompt_attachment_generation(&self) -> u64 {
            self.generation.load(Ordering::SeqCst)
        }
    }

    #[tokio::test]
    async fn child_attachment_uses_agent_cwd_caches_and_rescreens_after_invalidation() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("attachment.js");
        std::fs::write(
            &module,
            r#"let calls = 0;
export function register(on) {
  on('prompt.attachment', { type: 'hook_additional_context' }, async ($, e, next) => {
    calls++;
    if (e.text.includes('drop')) return { text: null };
    const bottom = await next(e);
    return { ...bottom, text: `${e.origin.kind}:${e.origin.event}:${await $.session.cwd()}:${e.agentId}:${calls}` };
  });
  on('prompt.submit', async ($, e, next) => {
    await $.ui.invalidate('prompt.attachment');
    return next(e);
  });
}"#,
        )
        .unwrap();
        let host = ModHost::start(None).await.unwrap();
        host.load("child-attachment", dir.path(), &module, json!({}))
            .await
            .unwrap();
        let session: Arc<dyn ModSessionContext> = Arc::new(Session {
            cwd: dir.path().to_path_buf(),
            generation: AtomicU64::new(0),
        });
        host.attach_background_context(Arc::downgrade(&session));
        let child_cwd = dir.path().join("child");
        std::fs::create_dir(&child_cwd).unwrap();
        let agent_id = AgentId::new();
        let kept = ConversationMessage::user_meta(
            MessageId::new(),
            "<system-reminder>\ntool.call hook additional context: keep\n</system-reminder>".into(),
        );
        let dropped = ConversationMessage::user_meta(
            MessageId::new(),
            "<system-reminder>\ntool.call hook additional context: drop\n</system-reminder>".into(),
        );
        let mut attachments = ChildPromptAttachments::default();
        for message in [&kept, &dropped] {
            attachments.register(
                message,
                "hook_additional_context",
                json!({"kind":"plugin","event":"tool.call"}),
            );
        }
        let original = vec![kept.clone(), dropped.clone()];
        let mut first = original.clone();
        attachments
            .screen(&mut first, Some(&host), &child_cwd, agent_id)
            .await;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id(), kept.id());
        assert_eq!(
            first[0].text_content(),
            format!(
                "<system-reminder>\nplugin:tool.call:{}:{}:1\n</system-reminder>",
                child_cwd.display(),
                agent_id.as_uuid()
            )
        );
        let mut second = original.clone();
        attachments
            .screen(&mut second, Some(&host), &child_cwd, agent_id)
            .await;
        assert_eq!(second, first, "attachment answers are cached by message id");
        assert_eq!(original, vec![kept, dropped], "history stays original");

        host.dispatch_with_ui_at_session_cwd(
            "prompt.submit",
            json!({"text":"invalidate","origin":{"kind":"user"},"wait":false}),
            session.as_ref(),
            &child_cwd,
            |event| async move { Ok(event) },
            |_, _| async {},
            |_, _, _| async {},
            |_, _| async {},
        )
        .await
        .unwrap();
        let mut third = original;
        attachments
            .screen(&mut third, Some(&host), &child_cwd, agent_id)
            .await;
        assert!(third[0].text_content().ends_with(":3\n</system-reminder>"));
    }
}
