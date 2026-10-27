//! Screen persistent child-agent attachments in each outgoing model snapshot.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use hooks::mods::{ModError, ModHost, ModSessionContext};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{AgentId, ContentBlock, ConversationMessage, MessageId};
use serde_json::{json, Value};

#[derive(Default)]
pub(crate) struct ChildPromptAttachments {
    descriptors: HashMap<MessageId, (String, Value)>,
    answers: HashMap<
        MessageId,
        (
            Utf16JsonProjection,
            (u64, u64),
            u64,
            Option<Utf16JsonProjection>,
        ),
    >,
}

fn content_text_units(block: &ContentBlock) -> Option<Vec<u16>> {
    match block {
        ContentBlock::Text { text, .. } => Some(text.encode_utf16().collect()),
        ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } => Some(utf16_code_units.clone()),
        _ => None,
    }
}

fn message_text_units(message: &ConversationMessage) -> Option<Vec<u16>> {
    match message {
        ConversationMessage::User { content, .. } => {
            let [block] = content.as_slice() else {
                return None;
            };
            content_text_units(block)
        }
        _ => None,
    }
}

fn unwrap_system_reminder(units: &[u16]) -> (Vec<u16>, bool) {
    let prefix = "<system-reminder>\n".encode_utf16().collect::<Vec<_>>();
    let suffix = "\n</system-reminder>".encode_utf16().collect::<Vec<_>>();
    if units.starts_with(&prefix)
        && units.ends_with(&suffix)
        && units.len() >= prefix.len() + suffix.len()
    {
        (
            units[prefix.len()..units.len() - suffix.len()].to_vec(),
            true,
        )
    } else {
        (units.to_vec(), false)
    }
}

fn replace_content_text(block: &mut ContentBlock, units: Vec<u16>) {
    let citations = match block {
        ContentBlock::Text { citations, .. } | ContentBlock::TextJsUtf16 { citations, .. } => {
            citations.clone()
        }
        _ => return,
    };
    let display = String::from_utf16_lossy(&units);
    *block = match String::from_utf16(&units) {
        Ok(text) => ContentBlock::Text { text, citations },
        Err(_) => ContentBlock::TextJsUtf16 {
            text: display,
            utf16_code_units: units,
            citations,
        },
    };
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
                    let units = content.string_units("")?;
                    let value = Utf16JsonProjection::parse_utf16_json(&units).ok()?;
                    let id_source = value.subprojection("/messageId").ok()?;
                    let id: MessageId =
                        serde_json::from_str(&id_source.to_json_string().ok()?).ok()?;
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
            if attachment.string_at("/type") != Some("hook_additional_context")
                || attachment.string_at("/hookName") != Some("tool.call")
                || attachment.string_at("/hookEvent") != Some("PostToolUse")
                || !attachment
                    .string_at("/toolUseID")
                    .is_some_and(|id| id.ends_with("-context"))
            {
                continue;
            }
            let Some(content_len) = attachment.array_len("/content") else {
                continue;
            };
            let Some(content_units) = (0..content_len)
                .map(|index| attachment.string_units(&format!("/content/{index}")))
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            if content_units.is_empty() {
                continue;
            }
            let mut original_units = "<system-reminder>\ntool.call hook additional context: "
                .encode_utf16()
                .collect::<Vec<_>>();
            for (index, units) in content_units.iter().enumerate() {
                if index > 0 {
                    original_units.push(b'\n' as u16);
                }
                original_units.extend(units);
            }
            original_units.extend("\n</system-reminder>".encode_utf16());
            if let Some(message) = history.iter().find(|message| {
                message.id() == id
                    && message.is_meta()
                    && message_text_units(message).as_deref() == Some(original_units.as_slice())
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
        session: Option<Arc<dyn ModSessionContext>>,
        cwd: &Path,
        agent_id: AgentId,
    ) {
        let Some(host) = host.filter(|host| host.has_event("prompt.attachment")) else {
            return;
        };
        if self.descriptors.is_empty() {
            return;
        }
        let generation = match session.as_ref() {
            Some(session) => {
                let Ok(_lease) = session.acquire_session_work_lease().await else {
                    return;
                };
                session.prompt_attachment_generation().await
            }
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
            let [block] = content.as_mut_slice() else {
                messages.push(message);
                continue;
            };
            let Some(original_units) = content_text_units(block) else {
                messages.push(message);
                continue;
            };
            let (body_units, wrapped) = unwrap_system_reminder(&original_units);
            if body_units.iter().all(|unit| {
                char::from_u32(u32::from(*unit))
                    .is_some_and(lingxi_core::host::subagent_output_guard::is_js_space)
            }) {
                messages.push(message);
                continue;
            }
            let body =
                Utf16JsonProjection::root_string(String::from_utf16_lossy(&body_units), body_units)
                    .expect("prompt attachment body is a root string");
            let mut input = Utf16JsonProjection::from_serde_value(&json!({
                "type":kind,
                "text":"",
                "origin":origin,
                "agentId":agent_id.as_uuid().to_string(),
            }))
            .expect("generated prompt.attachment input is valid JSON");
            input
                .set_field("text", body.clone())
                .expect("prompt.attachment input is an object");
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
                let pinned_origin = Utf16JsonProjection::from_serde_value(origin)
                    .expect("generated prompt.attachment origin is valid JSON");
                let pinned_agent = agent_id.as_uuid().to_string();
                let fallback_input = input.clone();
                let core = move |forwarded: Option<Utf16JsonProjection>| {
                    let kind = pinned_kind.clone();
                    let origin = pinned_origin.clone();
                    let agent = pinned_agent.clone();
                    let fallback_input = fallback_input.clone();
                    async move {
                        let forwarded = forwarded.unwrap_or(fallback_input);
                        if forwarded.string_at("/type") != Some(kind.as_str())
                            || forwarded.subprojection("/origin").ok() != Some(origin)
                            || forwarded.string_at("/agentId") != Some(agent.as_str())
                        {
                            return Err(ModError::Hook(
                                "prompt.attachment type, origin, and agentId are pinned".into(),
                            ));
                        }
                        let text = forwarded
                            .subprojection("/text")
                            .ok()
                            .filter(|text| text.string_units("").is_some())
                            .ok_or_else(|| ModError::Hook("prompt.attachment needs text".into()))?;
                        Ok(Some(
                            Utf16JsonProjection::object(vec![("text".into(), text)])
                                .expect("generated prompt.attachment result is valid"),
                        ))
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
                            async move { session.emit_mod_log(&plugin, text).await }
                        },
                        move |plugin, text, timeout_ms| {
                            let session = toast_session.clone();
                            async move { session.emit_mod_toast(&plugin, text, timeout_ms).await }
                        },
                        move |plugin, text| {
                            let session = status_session.clone();
                            async move { session.emit_mod_status(&plugin, text).await }
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
                    Ok(result)
                        if result
                            .get_property("text")
                            .is_some_and(|text| text.is_null()) =>
                    {
                        None
                    }
                    Ok(result) => result
                        .subprojection("/text")
                        .ok()
                        .filter(|text| text.string_units("").is_some()),
                    Err(error) => {
                        tracing::warn!(%error, "child prompt.attachment Mod failed");
                        Some(body.clone())
                    }
                };
                let current_generation = match session.as_ref() {
                    Some(session) => match session.acquire_session_work_lease().await {
                        Ok(_lease) => Some(session.prompt_attachment_generation().await),
                        Err(_) => None,
                    },
                    None => Some(0),
                };
                if current_generation == Some(generation) {
                    self.answers
                        .insert(message_id, (input, identity, generation, answer.clone()));
                }
                answer
            };
            if let Some(answer) = answer {
                let mut units = if wrapped {
                    "<system-reminder>\n".encode_utf16().collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                units.extend(answer.string_units("").unwrap_or_default());
                if wrapped {
                    units.extend("\n</system-reminder>".encode_utf16());
                }
                replace_content_text(block, units);
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
            content: json!({"messageId":message.id(),"attachment":attachment})
                .to_string()
                .into(),
            subtype: Some("mod_attachment_source".into()),
            api_system: None,
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
        let mut attachment =
            lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(&json!({
                "type":"hook_additional_context",
                "content":["first�"],
                "hookName":"tool.call",
                "toolUseID":"toolu_child-context",
                "hookEvent":"PostToolUse",
            }))
            .unwrap();
        let mut content_units = "first".encode_utf16().collect::<Vec<_>>();
        content_units.push(0xd800);
        attachment
            .set_string_units("/content/0", &content_units)
            .unwrap();
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
            )
            .into(),
            subtype: Some("mod_attachment_source".into()),
            api_system: None,
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
        host.load(
            "child-attachment",
            dir.path(),
            &module,
            Utf16JsonProjection::from_serde_value(&json!({})).unwrap(),
        )
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
            .screen(
                &mut first,
                Some(&host),
                host.bound_session(),
                &child_cwd,
                agent_id,
            )
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
            .screen(
                &mut second,
                Some(&host),
                host.bound_session(),
                &child_cwd,
                agent_id,
            )
            .await;
        assert_eq!(second, first, "attachment answers are cached by message id");
        assert_eq!(original, vec![kept, dropped], "history stays original");

        host.dispatch_with_ui_at_session_cwd(
            "prompt.submit",
            Utf16JsonProjection::from_serde_value(&json!({
                "text":"invalidate",
                "origin":{"kind":"user"},
                "wait":false,
            }))
            .unwrap(),
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
            .screen(
                &mut third,
                Some(&host),
                host.bound_session(),
                &child_cwd,
                agent_id,
            )
            .await;
        assert!(third[0].text_content().ends_with(":3\n</system-reminder>"));
    }
}
