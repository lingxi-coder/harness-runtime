//! Child-agent `turn.complete` facts, scoped to one turn-set.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hooks::mods::{ModHost, ModSessionContext};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{AgentId, ContentBlock as HostContentBlock, ConversationMessage};
use llm_runtime::HistoryResponse;
use serde_json::json;

pub(crate) struct ChildTurnComplete {
    host: Option<Arc<ModHost>>,
    session: Option<Arc<dyn ModSessionContext>>,
    cwd: PathBuf,
    agent_id: AgentId,
    turn_id: String,
    started_at: Instant,
    answer: Utf16JsonProjection,
    usage: Option<ChildUsage>,
    refusal: Option<Utf16JsonProjection>,
    status: Status,
    dispatched: bool,
    pending_physical: Vec<Arc<Mutex<Vec<HistoryResponse>>>>,
}

struct ChildUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
    model: String,
}

#[derive(Clone, Copy)]
enum Status {
    Answer,
    Error,
    Aborted,
}

// JavaScript trim includes BOM and excludes NEL, unlike Rust str::trim.
fn js_trim(text: &str) -> &str {
    text.trim_matches(|ch| {
        matches!(
            ch,
            '\u{0009}'..='\u{000D}'
                | '\u{0020}'
                | '\u{00A0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
        )
    })
}

fn is_js_trim_unit(unit: u16) -> bool {
    matches!(
        unit,
        0x0009..=0x000d
            | 0x0020
            | 0x00a0
            | 0x1680
            | 0x2000..=0x200a
            | 0x2028
            | 0x2029
            | 0x202f
            | 0x205f
            | 0x3000
            | 0xfeff
    )
}

fn trim_js_units(units: &[u16]) -> Vec<u16> {
    let start = units
        .iter()
        .position(|unit| !is_js_trim_unit(*unit))
        .unwrap_or(units.len());
    let end = units
        .iter()
        .rposition(|unit| !is_js_trim_unit(*unit))
        .map_or(start, |index| index + 1);
    units[start..end].to_vec()
}

fn source_text_from_units(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("display text matches exact UTF-16 source units")
}

fn join_text_units(blocks: impl IntoIterator<Item = Option<Vec<u16>>>) -> Vec<u16> {
    let mut joined = Vec::new();
    let mut first = true;
    for units in blocks.into_iter().flatten() {
        if !first {
            joined.push('\n' as u16);
        }
        joined.extend(units);
        first = false;
    }
    joined
}

fn host_block_text_units(block: &HostContentBlock) -> Option<Vec<u16>> {
    match block {
        HostContentBlock::Text { text, .. } => Some(text.encode_utf16().collect()),
        HostContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } => Some(utf16_code_units.clone()),
        HostContentBlock::ProviderContent { protocol, value }
            if protocol == "anthropic_messages" && value.string_at("/type") == Some("text") =>
        {
            value.string_units("/text")
        }
        _ => None,
    }
}

fn response_block_text_units(block: &llm_runtime::ContentBlock) -> Option<Vec<u16>> {
    match block {
        llm_runtime::ContentBlock::Text { text, .. } => Some(text.encode_utf16().collect()),
        llm_runtime::ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } => Some(utf16_code_units.clone()),
        llm_runtime::ContentBlock::ProviderContent { protocol, value }
            if protocol == "anthropic_messages"
                && value.string_at("/type").as_deref() == Some("text") =>
        {
            let source = value.raw_source_at("/text")?;
            Utf16JsonProjection::parse(&source).ok()?.string_units("")
        }
        _ => None,
    }
}

fn response_answer_source(content: &[llm_runtime::ContentBlock]) -> Utf16JsonProjection {
    let answer_units = join_text_units(content.iter().map(response_block_text_units));
    source_text_from_units(trim_js_units(&answer_units))
}

fn refusal_metadata(
    category: Option<&str>,
    explanation: Option<&Utf16JsonProjection>,
) -> Utf16JsonProjection {
    let null = Utf16JsonProjection::parse("null").expect("static null is valid JSON");
    Utf16JsonProjection::object(vec![
        (
            "category".into(),
            category
                .map(Utf16JsonProjection::from)
                .unwrap_or_else(|| null.clone()),
        ),
        ("explanation".into(), explanation.cloned().unwrap_or(null)),
    ])
    .expect("generated refusal details are a valid JSON object")
}

pub(crate) fn child_turn_start_text(history: &[ConversationMessage]) -> Utf16JsonProjection {
    let units = history
        .iter()
        .rev()
        .find_map(|message| match message {
            ConversationMessage::User {
                content,
                is_meta: false,
                is_visible_in_transcript_only: false,
                ..
            } => Some(join_text_units(content.iter().map(host_block_text_units))),
            _ => None,
        })
        .unwrap_or_default();
    source_text_from_units(units)
}

pub(crate) async fn fire_child_turn_start(
    host: &Arc<ModHost>,
    cwd: &std::path::Path,
    text: &Utf16JsonProjection,
    turn_id: &str,
) {
    let mut input = Utf16JsonProjection::from_serde_value(&json!({"text":null,"turnId":turn_id}))
        .expect("generated turn.start input is valid JSON");
    input
        .set_field("text", text.clone())
        .expect("turn.start input is an object");
    let turn_id = turn_id.to_owned();
    let core = move |_: Option<Utf16JsonProjection>| {
        let turn_id = turn_id.clone();
        async move {
            Ok(Some(
                Utf16JsonProjection::from_serde_value(&json!({"turnId":turn_id}))
                    .expect("generated turn.start result is valid JSON"),
            ))
        }
    };
    let result = if let Some(session) = host.bound_session() {
        let log_session = session.clone();
        let toast_session = session.clone();
        let status_session = session.clone();
        host.dispatch_with_ui_at_session_cwd(
            "turn.start",
            input,
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
        host.dispatch_with_log_at("turn.start", input, cwd, core, |_, _| async {})
            .await
    };
    if let Err(error) = result {
        tracing::warn!(%error, "child turn.start Mod dispatch failed");
    }
}

impl ChildTurnComplete {
    pub(crate) fn new(
        host: Option<Arc<ModHost>>,
        cwd: PathBuf,
        agent_id: AgentId,
        turn_id: String,
    ) -> Self {
        let session = host.as_ref().and_then(|host| host.bound_session());
        Self {
            host,
            session,
            cwd,
            agent_id,
            turn_id,
            started_at: Instant::now(),
            answer: Utf16JsonProjection::from(""),
            usage: None,
            refusal: None,
            status: Status::Error,
            dispatched: false,
            pending_physical: Vec::new(),
        }
    }

    pub(crate) fn attach_physical(&mut self, responses: Arc<Mutex<Vec<HistoryResponse>>>) {
        self.pending_physical.push(responses);
    }

    pub(crate) fn drain_physical(&mut self) -> Vec<HistoryResponse> {
        let mut completed = Vec::new();
        for slot in &self.pending_physical {
            completed.extend(std::mem::take(
                &mut *slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            ));
        }
        for response in &completed {
            self.observe_usage(response);
        }
        completed
    }

    pub(crate) fn observe_usage(&mut self, response: &HistoryResponse) {
        let counts = response.usage.counts();
        let total = self.usage.get_or_insert_with(|| ChildUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            model: response.model.clone(),
        });
        total.input_tokens = total.input_tokens.saturating_add(counts.input_tokens);
        total.output_tokens = total.output_tokens.saturating_add(counts.output_tokens);
        total.cache_read_input_tokens = total
            .cache_read_input_tokens
            .saturating_add(counts.cache_read_tokens);
        total.cache_creation_input_tokens = total
            .cache_creation_input_tokens
            .saturating_add(counts.cache_write_tokens);
        total.model = response.model.clone();
    }

    pub(crate) fn observe(&mut self, response: &HistoryResponse, count_usage: bool) {
        if count_usage {
            self.observe_usage(response);
        }
        self.answer = response_answer_source(&response.content);
        self.refusal = (response.stop_reason.as_deref() == Some("refusal")).then(|| {
            let details = response.stop_details.as_ref();
            let category = details.and_then(|details| details.category());
            let explanation = details.and_then(|details| details.explanation());
            refusal_metadata(category.as_deref(), explanation.as_ref())
        });
    }

    pub(crate) fn answered(&mut self) {
        self.status = Status::Answer;
    }

    pub(crate) fn aborted(&mut self) {
        self.status = Status::Aborted;
    }

    pub(crate) fn dispatch(&mut self) {
        if self.dispatched {
            return;
        }
        self.dispatched = true;
        self.drain_physical();
        let Some(host) = self.host.take() else { return };
        let reason = match self.status {
            Status::Aborted => "aborted",
            _ if self.refusal.is_some() => "refusal",
            Status::Error => "error",
            Status::Answer => "answer",
        };
        let mut input = Utf16JsonProjection::from_serde_value(&json!({
            "answer": null,
            "durationMs":u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            "isAborted":matches!(self.status, Status::Aborted),
            "turnId":self.turn_id,
            "agentId":self.agent_id.to_string(),
            "reason":reason,
        }))
        .expect("generated turn.complete input is valid JSON");
        input
            .set_field("answer", self.answer.clone())
            .expect("turn.complete input is an object");
        if let Some(refusal) = self.refusal.take() {
            input
                .set_field("refusal", refusal)
                .expect("turn.complete input is an object");
        }
        if let Some(usage) = self.usage.take() {
            input
                .set_field(
                    "usage",
                    Utf16JsonProjection::from_serde_value(&json!({
                        "input_tokens":usage.input_tokens,
                        "output_tokens":usage.output_tokens,
                        "cache_read_input_tokens":usage.cache_read_input_tokens,
                        "cache_creation_input_tokens":usage.cache_creation_input_tokens,
                        "model":usage.model,
                    }))
                    .expect("generated turn usage is valid JSON"),
                )
                .expect("turn.complete input is an object");
        }
        let cwd = self.cwd.clone();
        let session = self.session.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let core = |event: Option<Utf16JsonProjection>| async move {
                    let null =
                        Utf16JsonProjection::parse("null").expect("static JSON null is valid");
                    let event = event.unwrap_or_else(|| null.clone());
                    let mut fields = vec![(
                        "text".to_owned(),
                        event
                            .subprojection("/answer")
                            .unwrap_or_else(|_| null.clone()),
                    )];
                    if let Ok(usage) = event.subprojection("/usage") {
                        fields.push(("usage".to_owned(), usage));
                    }
                    Ok(Some(
                        Utf16JsonProjection::object(fields)
                            .expect("generated turn.complete result is valid"),
                    ))
                };
                let result = if let Some(session) = session {
                    let log_session = session.clone();
                    let toast_session = session.clone();
                    let status_session = session.clone();
                    host.dispatch_with_ui_at_session_cwd(
                        "turn.complete",
                        input,
                        session.as_ref(),
                        &cwd,
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
                    host.dispatch_with_log_at("turn.complete", input, &cwd, core, |_, _| async {})
                        .await
                };
                if let Err(error) = result {
                    tracing::warn!(%error, "child turn.complete Mod dispatch failed");
                }
            });
        }
    }
}

impl Drop for ChildTurnComplete {
    fn drop(&mut self) {
        self.dispatch();
    }
}

#[cfg(test)]
mod tests {
    use super::{child_turn_start_text, js_trim, refusal_metadata, response_answer_source};
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use lingxi_core::types::{ContentBlock as HostContentBlock, ConversationMessage, MessageId};

    #[test]
    fn answer_trim_uses_javascript_whitespace() {
        assert_eq!(js_trim("\u{feff} answer \u{feff}"), "answer");
        assert_eq!(js_trim("\u{0085}answer\u{0085}"), "\u{0085}answer\u{0085}");
    }

    #[test]
    fn refusal_metadata_keeps_lone_utf16_explanation_units() {
        let explanation = Utf16JsonProjection::parse(r#""before\ud800after""#).unwrap();
        let metadata = refusal_metadata(Some("cyber"), Some(&explanation));
        assert_eq!(metadata.string_at("/category"), Some("cyber"));
        assert_eq!(
            metadata.string_units("/explanation"),
            explanation.string_units("")
        );
    }

    #[test]
    fn turn_start_text_keeps_lone_utf16_units_from_user_history() {
        let history = vec![ConversationMessage::User {
            id: MessageId::new(),
            api_message_override: None,
            interrupted_by_shutdown: None,
            refused_message_id: None,
            content: vec![HostContentBlock::TextJsUtf16 {
                text: "�".into(),
                utf16_code_units: vec![0xd800],
                citations: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
        }];
        assert_eq!(
            child_turn_start_text(&history).string_units(""),
            Some(vec![0xd800])
        );
    }

    #[test]
    fn turn_complete_answer_keeps_lone_utf16_units_from_provider_text() {
        let answer = response_answer_source(&[llm_runtime::ContentBlock::TextJsUtf16 {
            text: "�".into(),
            utf16_code_units: vec![0xd800],
            citations: None,
            cache_control: None,
        }]);
        assert_eq!(answer.string_units(""), Some(vec![0xd800]));
    }
}
