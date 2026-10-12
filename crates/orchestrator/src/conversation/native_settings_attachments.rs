//! Native's session-level output-style and response-language deltas.
//!
//! Claude Code 2.1.295 `Sgn` reads the active style inventory and the effective
//! `settings.language`, then compares them with the last raw typed rows in
//! history. These are distinct from the ordinary per-turn `output_style`
//! reminder, which remains in `reminders.rs`.

use super::ConversationOrchestrator;
use crate::request_context::{rendered_system_message, PromptIngressAttachment};
use lingxi_core::host::{
    NativeSettingsPromptAttachment, NativeSettingsPromptAttachmentProvider,
    NativeSettingsPromptContext,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
use lingxi_core::types::{ConversationMessage, MessageId};
#[cfg(test)]
use std::collections::HashMap;
use std::ffi::OsStr;

const DEFAULT_OUTPUT_STYLE: &str = "default";
/// Native `N9` is shared by the style-name and language schemas; Zod measures
/// JavaScript string length in UTF-16 code units.
const NATIVE_SETTINGS_STRING_LIMIT: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
struct StyleInstructions {
    name: Vec<u16>,
    prompt: Vec<u16>,
}

fn utf16_source_string(units: &[u16]) -> Source {
    Source::root_string(String::from_utf16_lossy(units), units.to_vec())
        .expect("display form is the lossless or replacement-safe UTF-16 projection")
}

fn source_string(value: &str) -> Source {
    utf16_source_string(&value.encode_utf16().collect::<Vec<_>>())
}

fn source_null() -> Source {
    Source::parse("null").expect("static null is valid JSON")
}

fn append_utf16(target: &mut Vec<u16>, value: &str) {
    target.extend(value.encode_utf16());
}

/// Native's `escape_reminder_text` expressed over JavaScript UTF-16 units, so
/// malformed surrogate pairs remain exact in the rendered carrier.
fn escape_reminder_text_utf16(value: &[u16]) -> Vec<u16> {
    let mut escaped = Vec::with_capacity(value.len());
    for &unit in value {
        match unit {
            0x0026 => append_utf16(&mut escaped, "&amp;"),
            0x003c => append_utf16(&mut escaped, "&lt;"),
            0x003e => append_utf16(&mut escaped, "&gt;"),
            0x0000..=0x001f | 0x007f..=0x009f | 0x2028..=0x2029 => {
                append_utf16(&mut escaped, &format!("&#{unit};"));
            }
            _ => escaped.push(unit),
        }
    }
    escaped
}

fn json_object(fields: Vec<(String, Source)>) -> Source {
    Source::object(fields).expect("Native settings payload has unique object keys")
}

/// `wCe(previous,current,equal)`: a missing previous row emits only a present
/// current value; an existing row, including explicit null, emits on change.
fn changed_value<T: Clone + PartialEq>(
    previous: Option<Option<T>>,
    current: Option<T>,
) -> Option<Option<T>> {
    match previous {
        Some(previous) if previous == current => None,
        Some(_) => Some(current),
        None if current.is_some() => Some(current),
        None => None,
    }
}

fn parse_style_state(payload: &Source) -> Option<Option<StyleInstructions>> {
    let style = payload.at_pointer("/style")?;
    if style.is_null() {
        return Some(None);
    }
    let name = payload.string_units("/style/name")?;
    if name.len() > NATIVE_SETTINGS_STRING_LIMIT {
        return None;
    }
    let prompt = payload.string_units("/style/prompt")?;
    Some(Some(StyleInstructions { name, prompt }))
}

fn parse_language_state(payload: &Source) -> Option<Option<Vec<u16>>> {
    let language = payload.at_pointer("/language")?;
    if language.is_null() {
        return Some(None);
    }
    let units = payload.string_units("/language")?;
    (units.len() <= NATIVE_SETTINGS_STRING_LIMIT).then_some(Some(units))
}

#[cfg(test)]
fn latest_payload<'a>(
    history: &[ConversationMessage],
    attachments: &'a HashMap<MessageId, Source>,
    attachment_type: &str,
) -> Option<&'a Source> {
    history.iter().rev().find_map(|message| {
        attachments
            .get(&message.id())
            .filter(|payload| payload.string_at("/type") == Some(attachment_type))
    })
}

fn style_body(style: Option<&StyleInstructions>) -> Vec<u16> {
    let Some(style) = style else {
        return "The output style was reset to the default. Respond in your usual style."
            .encode_utf16()
            .collect();
    };
    let mut body = "# Output Style: ".encode_utf16().collect::<Vec<_>>();
    body.extend(escape_reminder_text_utf16(&style.name));
    body.push(b'\n' as u16);
    body.extend_from_slice(&style.prompt);
    body
}

fn language_body(language: Option<&[u16]>) -> Vec<u16> {
    let Some(language) = language else {
        return "The language preference was cleared. Match the user's language."
            .encode_utf16()
            .collect();
    };
    let mut body = "# Language\nAlways respond in "
        .encode_utf16()
        .collect::<Vec<_>>();
    body.extend(escape_reminder_text_utf16(language));
    append_utf16(&mut body, ". Use ");
    body.extend(escape_reminder_text_utf16(language));
    append_utf16(
        &mut body,
        " for all explanations, comments, and communications with the user. Technical terms and code identifiers should remain in their original form.\nMaintain full orthographic correctness for ",
    );
    body.extend(escape_reminder_text_utf16(language));
    append_utf16(
        &mut body,
        ", including all required diacritical marks, accents, and special characters. Never substitute accented characters with their ASCII equivalents (e.g., never write \"nao\" for \"não\", \"fur\" for \"für\", or \"loeschen\" for \"löschen\").",
    );
    body
}

fn attachment(payload: Source, body_units: Vec<u16>) -> Option<NativeSettingsPromptAttachment> {
    let mut message_units = "<system-reminder>\n".encode_utf16().collect::<Vec<_>>();
    message_units.extend(body_units);
    append_utf16(&mut message_units, "\n</system-reminder>");
    let message = ConversationMessage::user_meta_js_utf16(
        MessageId::new(),
        String::from_utf16_lossy(&message_units),
        message_units,
    );
    let rendered = rendered_system_message(&message)?;
    Some(NativeSettingsPromptAttachment {
        message,
        payload,
        rendered,
    })
}

fn style_attachment(style: Option<&StyleInstructions>) -> Option<NativeSettingsPromptAttachment> {
    let style_value = match style {
        Some(style) => json_object(vec![
            ("name".into(), utf16_source_string(&style.name)),
            ("prompt".into(), utf16_source_string(&style.prompt)),
        ]),
        None => source_null(),
    };
    let payload = json_object(vec![
        ("type".into(), source_string("output_style_instructions")),
        ("style".into(), style_value),
    ]);
    attachment(payload, style_body(style))
}

fn language_attachment(language: Option<&[u16]>) -> Option<NativeSettingsPromptAttachment> {
    let language_value = language
        .map(utf16_source_string)
        .unwrap_or_else(source_null);
    let payload = json_object(vec![
        ("type".into(), source_string("language")),
        ("language".into(), language_value),
    ]);
    attachment(payload, language_body(language))
}

fn current_language(language: Option<&Source>) -> Result<Option<Vec<u16>>, ()> {
    let Some(language) = language else {
        return Ok(None);
    };
    let units = language.string_units("").ok_or(())?;
    if units.is_empty() {
        return Ok(None);
    }
    if units.len() > NATIVE_SETTINGS_STRING_LIMIT {
        return Err(());
    }
    Ok(Some(units))
}

fn output_style_setting_name(setting: Option<&Source>) -> Option<String> {
    let units = setting?.string_units("")?;
    String::from_utf16(&units).ok()
}

fn is_default_output_style_setting(setting: Option<&[u16]>) -> bool {
    match setting {
        None | Some([]) => true,
        Some(units) => units == DEFAULT_OUTPUT_STYLE.encode_utf16().collect::<Vec<_>>(),
    }
}

fn native_js_env_truthy(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

fn native_settings_gates_enabled(
    bare: bool,
    simple: Option<&OsStr>,
    disable_attachments: Option<&OsStr>,
) -> bool {
    !bare && !native_js_env_truthy(simple) && !native_js_env_truthy(disable_attachments)
}

impl ConversationOrchestrator {
    /// Return Native's ordered `output_style_instructions` then `language`
    /// changes for this orchestrator query context.
    ///
    /// This mirrors `fPr`/`N9e`: the composition root supplies the actual
    /// `agentContext` identity, while configured bare mode, simple mode, and
    /// disabled attachments suppress the producer. In-process teammates and
    /// StateMachinePool children call the same provider with their own
    /// identities; ordinary Agent children remain non-main subagents, and a
    /// fork carrier alone does not establish the Native `bareFork` option.
    pub(crate) async fn native_settings_prompt_ingress_attachments(
        &self,
        prior_history: &[ConversationMessage],
    ) -> Vec<PromptIngressAttachment> {
        let prior_payloads = {
            let attachments = self
                .transcript
                .model_reminder_attachments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            prior_history
                .iter()
                .filter_map(|message| {
                    attachments
                        .get(&message.id())
                        .cloned()
                        .map(|payload| (message.id(), payload))
                })
                .collect::<Vec<_>>()
        };
        self.native_settings_prompt_attachments(
            NativeSettingsPromptContext {
                agent_context: self.prompt_runtime.native_settings_agent_context,
                bare_fork: None,
                isolated_context: None,
            },
            &prior_payloads,
        )
        .await
        .into_iter()
        .map(|row| PromptIngressAttachment {
            message: row.message,
            payload: row.payload,
            rendered: row.rendered,
        })
        .collect()
    }

    fn native_settings_prompt_ingress_enabled(&self, context: NativeSettingsPromptContext) -> bool {
        if !context.is_admitted() {
            return false;
        }
        // Read the live `/output-style` value once so a concurrent style
        // switch cannot make the setting and its resolved style disagree.
        let simple = std::env::var_os(branding::SIMPLE_ENV);
        let disable_attachments = std::env::var_os(branding::DISABLE_ATTACHMENTS_ENV);
        native_settings_gates_enabled(
            self.config.bare,
            simple.as_deref(),
            disable_attachments.as_deref(),
        )
    }
}

#[async_trait::async_trait]
impl NativeSettingsPromptAttachmentProvider for ConversationOrchestrator {
    async fn native_settings_prompt_attachments(
        &self,
        context: NativeSettingsPromptContext,
        prior_payloads: &[(MessageId, Source)],
    ) -> Vec<NativeSettingsPromptAttachment> {
        if !self.native_settings_prompt_ingress_enabled(context) {
            return Vec::new();
        }

        let previous_style = prior_payloads
            .iter()
            .rev()
            .find_map(|(_, payload)| {
                (payload.string_at("/type") == Some("output_style_instructions")).then_some(payload)
            })
            .and_then(parse_style_state);
        let previous_language = prior_payloads
            .iter()
            .rev()
            .find_map(|(_, payload)| {
                (payload.string_at("/type") == Some("language")).then_some(payload)
            })
            .and_then(parse_language_state);

        // Read the live `/output-style` value once so a concurrent style
        // switch cannot make the setting and its resolved style disagree.
        let style_setting = match self.prompt_runtime.live_output_style.lock().await.clone() {
            Some(refreshed) => refreshed,
            None => self.config.output_style.clone(),
        };
        let style_setting_units = style_setting
            .as_ref()
            .and_then(|setting| setting.string_units(""));
        let style_setting_name = output_style_setting_name(style_setting.as_ref());
        let resolved_style = if let Some(registry) = &self.prompt_runtime.output_style_registry {
            let registry = registry.read().await;
            registry.resolve(style_setting_name.as_deref()).or_else(|| {
                outputstyles::resolve_output_style(
                    style_setting_name.as_deref(),
                    &self.config.output_style_dirs,
                )
            })
        } else {
            outputstyles::resolve_output_style(
                style_setting_name.as_deref(),
                &self.config.output_style_dirs,
            )
        };
        let current_style = resolved_style.map(|style| StyleInstructions {
            name: style.name.encode_utf16().collect(),
            prompt: style.prompt.encode_utf16().collect(),
        });
        let current_style_is_valid = current_style.as_ref().map_or(true, |style| {
            style.name.len() <= NATIVE_SETTINGS_STRING_LIMIT
        });
        // Native sends an explicit null reset only for the default selection.
        // A stale/unknown non-default setting with no resolved style suppresses
        // the style delta entirely.
        let current_style_is_resolvable = current_style.is_some()
            || is_default_output_style_setting(style_setting_units.as_deref());
        let style_delta = (current_style_is_valid && current_style_is_resolvable)
            .then(|| changed_value(previous_style, current_style))
            .flatten();

        let language_setting = self.active_response_language().await;
        let language_delta = current_language(language_setting.as_ref())
            .ok()
            .and_then(|current| changed_value(previous_language, current));

        let mut rows: Vec<NativeSettingsPromptAttachment> = Vec::with_capacity(2);
        if let Some(style) = style_delta {
            if let Some(row) = style_attachment(style.as_ref()) {
                rows.push(row);
            }
        }
        if let Some(language) = language_delta {
            if let Some(row) = language_attachment(language.as_deref()) {
                rows.push(row);
            }
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_absent_values_are_silent_but_existing_values_can_reset() {
        assert_eq!(changed_value::<String>(None, None), None);
        assert_eq!(
            changed_value(None, Some("English".to_string())),
            Some(Some("English".into()))
        );
        assert_eq!(
            changed_value(Some(Some("English".to_string())), None),
            Some(None)
        );
        assert_eq!(changed_value(Some(None::<String>), None), None);
    }

    #[test]
    fn absent_empty_and_default_style_settings_are_all_default() {
        assert!(is_default_output_style_setting(None));
        assert!(is_default_output_style_setting(Some(&[])));
        assert!(is_default_output_style_setting(Some(
            &"default".encode_utf16().collect::<Vec<_>>()
        )));
        assert!(!is_default_output_style_setting(Some(
            &"unknown".encode_utf16().collect::<Vec<_>>()
        )));
        assert!(!is_default_output_style_setting(Some(&[0xd800])));
    }

    #[test]
    fn lone_surrogate_style_setting_is_not_lossily_resolved_as_replacement_name() {
        let unpaired = Source::parse(r#""\ud800""#).unwrap();
        let replacement = Source::parse(r#""�""#).unwrap();
        assert_eq!(output_style_setting_name(Some(&unpaired)), None);
        assert_eq!(
            output_style_setting_name(Some(&replacement)).as_deref(),
            Some("�")
        );

        let mut registry = outputstyles::OutputStyleRegistry::new();
        registry.register(outputstyles::OutputStyle {
            name: "�".into(),
            description: String::new(),
            source: outputstyles::OutputStyleSource::Builtin,
            frontmatter: outputstyles::OutputStyleFrontmatter {
                name: "�".into(),
                ..outputstyles::OutputStyleFrontmatter::default()
            },
            system_prompt_addendum: "replacement style".into(),
            source_path: None,
        });
        assert!(
            registry
                .resolve(output_style_setting_name(Some(&unpaired)).as_deref())
                .is_none(),
            "strict UTF-16 conversion prevents an unpaired setting from matching U+FFFD"
        );
    }

    #[test]
    fn typed_payloads_preserve_raw_shapes_and_explicit_null() {
        let style = StyleInstructions {
            name: "Concise".encode_utf16().collect(),
            prompt: "Short answers".encode_utf16().collect(),
        };
        let style = style_attachment(Some(&style)).unwrap();
        assert_eq!(
            style.payload.to_json_string().unwrap(),
            r#"{"type":"output_style_instructions","style":{"name":"Concise","prompt":"Short answers"}}"#
        );
        let reset = style_attachment(None).unwrap();
        assert_eq!(
            reset.payload.to_json_string().unwrap(),
            r#"{"type":"output_style_instructions","style":null}"#
        );
        assert_eq!(
            reset.message.text_content(),
            "<system-reminder>\nThe output style was reset to the default. Respond in your usual style.\n</system-reminder>"
        );

        let language =
            language_attachment(Some(&"日本語".encode_utf16().collect::<Vec<_>>())).unwrap();
        assert_eq!(
            language.payload.to_json_string().unwrap(),
            r#"{"type":"language","language":"日本語"}"#
        );
        let language_reset = language_attachment(None).unwrap();
        assert_eq!(
            language_reset.payload.to_json_string().unwrap(),
            r#"{"type":"language","language":null}"#
        );
        assert_eq!(
            language_reset.message.text_content(),
            "<system-reminder>\nThe language preference was cleared. Match the user's language.\n</system-reminder>"
        );
    }

    #[test]
    fn rendered_reminders_keep_exact_utf16_units() {
        let raw_name = vec![0xd800, b'<' as u16, b'&' as u16, b'\n' as u16];
        let raw_prompt = vec![0xdc00];
        let style = style_attachment(Some(&StyleInstructions {
            name: raw_name.clone(),
            prompt: raw_prompt.clone(),
        }))
        .unwrap();
        assert_eq!(
            style.payload.string_units("/style/name"),
            Some(raw_name.clone())
        );
        assert_eq!(
            style.payload.string_units("/style/prompt"),
            Some(raw_prompt.clone())
        );
        let mut expected = "<system-reminder>\n# Output Style: "
            .encode_utf16()
            .collect::<Vec<_>>();
        expected.push(0xd800);
        append_utf16(&mut expected, "&lt;&amp;&#10;\n");
        expected.push(0xdc00);
        append_utf16(&mut expected, "\n</system-reminder>");
        assert_eq!(
            style.rendered.string_units("/0/content"),
            Some(expected.clone())
        );
        let ConversationMessage::User { content, .. } = &style.message else {
            panic!("settings reminder must be a user message");
        };
        let lingxi_core::types::ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } = &content[0]
        else {
            panic!("settings reminder must retain its UTF-16 source");
        };
        assert_eq!(utf16_code_units, &expected);

        let raw_language = vec![0xd800];
        let language = language_attachment(Some(&raw_language)).unwrap();
        assert_eq!(
            language.payload.string_units("/language"),
            Some(raw_language.clone())
        );
        assert_eq!(
            language
                .rendered
                .string_units("/0/content")
                .unwrap()
                .into_iter()
                .filter(|unit| *unit == 0xd800)
                .count(),
            3
        );
    }

    #[test]
    fn native_string_limit_counts_utf16_units() {
        let accepted = "😀".repeat(NATIVE_SETTINGS_STRING_LIMIT / 2);
        let rejected = "😀".repeat(NATIVE_SETTINGS_STRING_LIMIT / 2 + 1);
        let accepted_source = Source::parse(&format!(r#""{accepted}""#)).unwrap();
        let rejected_source = Source::parse(&format!(r#""{rejected}""#)).unwrap();
        assert_eq!(
            current_language(Some(&accepted_source))
                .unwrap()
                .unwrap()
                .len(),
            NATIVE_SETTINGS_STRING_LIMIT
        );
        assert!(current_language(Some(&rejected_source)).is_err());

        let accepted_style = Source::parse(&format!(
            r#"{{"style":{{"name":"{accepted}","prompt":"ok"}}}}"#
        ))
        .unwrap();
        assert!(parse_style_state(&accepted_style).unwrap().is_some());
        let rejected_style = Source::parse(&format!(
            r#"{{"style":{{"name":"{rejected}","prompt":"ok"}}}}"#
        ))
        .unwrap();
        assert!(parse_style_state(&rejected_style).is_none());
    }

    #[test]
    fn settings_gates_match_javascript_environment_truthiness() {
        for value in ["0", "false", "no", "anything"] {
            assert!(!native_settings_gates_enabled(
                false,
                Some(OsStr::new(value)),
                None
            ));
            assert!(!native_settings_gates_enabled(
                false,
                None,
                Some(OsStr::new(value))
            ));
        }
        assert!(native_settings_gates_enabled(false, None, None));
        assert!(native_settings_gates_enabled(
            false,
            Some(OsStr::new("")),
            None
        ));
        assert!(native_settings_gates_enabled(
            false,
            None,
            Some(OsStr::new(""))
        ));
        assert!(!native_settings_gates_enabled(true, None, None));
    }

    #[test]
    fn latest_typed_row_controls_prior_state() {
        let first = ConversationMessage::user_meta(MessageId::new(), "first".into());
        let second = ConversationMessage::user_meta(MessageId::new(), "second".into());
        let first_id = first.id();
        let second_id = second.id();
        let attachments = HashMap::from([
            (
                first_id,
                Source::parse(r#"{"type":"language","language":"English"}"#).unwrap(),
            ),
            (
                second_id,
                Source::parse(r#"{"type":"language","language":null}"#).unwrap(),
            ),
        ]);
        let prior = vec![first, second];
        assert_eq!(
            latest_payload(&prior, &attachments, "language").and_then(parse_language_state),
            Some(None)
        );
        assert_eq!(
            changed_value(
                latest_payload(&prior, &attachments, "language").and_then(parse_language_state),
                Some("English".encode_utf16().collect()),
            ),
            Some(Some("English".encode_utf16().collect()))
        );
    }
}
