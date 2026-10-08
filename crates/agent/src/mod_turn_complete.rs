//! Child-agent `turn.complete` facts, scoped to one turn-set.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hooks::mods::{ModHost, ModSessionContext};
use lingxi_core::types::{AgentId, ContentBlock as HostContentBlock, ConversationMessage};
use llm_runtime::HistoryResponse;
use serde_json::{Value, json};

pub(crate) struct ChildTurnComplete {
    host: Option<Arc<ModHost>>,
    session: Option<Arc<dyn ModSessionContext>>,
    cwd: PathBuf,
    agent_id: AgentId,
    turn_id: String,
    started_at: Instant,
    answer: String,
    usage: Option<ChildUsage>,
    refusal: Option<Value>,
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

pub(crate) fn child_turn_start_text(history: &[ConversationMessage]) -> String {
    history
        .iter()
        .rev()
        .find_map(|message| match message {
            ConversationMessage::User {
                content,
                is_meta: false,
                is_visible_in_transcript_only: false,
                ..
            } => Some(
                content
                    .iter()
                    .filter_map(HostContentBlock::visible_text)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

pub(crate) async fn fire_child_turn_start(
    host: &Arc<ModHost>,
    cwd: &std::path::Path,
    text: &str,
    turn_id: &str,
) {
    let input = json!({"text":text,"turnId":turn_id});
    let turn_id = turn_id.to_owned();
    let core = move |_: Value| {
        let turn_id = turn_id.clone();
        async move { Ok(json!({"turnId":turn_id})) }
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
            answer: String::new(),
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
        self.answer = js_trim(
            &response
                .content
                .iter()
                .filter_map(llm_runtime::ContentBlock::visible_text)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .to_owned();
        self.refusal = (response.stop_reason.as_deref() == Some("refusal")).then(|| {
            json!({
                "category":response.stop_details.as_ref().and_then(|details| details.category.as_deref()),
                "explanation":response.stop_details.as_ref().and_then(|details| details.explanation.as_deref()),
            })
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
        let mut input = json!({
            "answer":self.answer,
            "durationMs":u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            "isAborted":matches!(self.status, Status::Aborted),
            "turnId":self.turn_id,
            "agentId":self.agent_id.to_string(),
            "reason":reason,
        });
        if let Some(refusal) = self.refusal.take() {
            input["refusal"] = refusal;
        }
        if let Some(usage) = self.usage.take() {
            input["usage"] = json!({
                "input_tokens":usage.input_tokens,
                "output_tokens":usage.output_tokens,
                "cache_read_input_tokens":usage.cache_read_input_tokens,
                "cache_creation_input_tokens":usage.cache_creation_input_tokens,
                "model":usage.model,
            });
        }
        let cwd = self.cwd.clone();
        let session = self.session.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let core = |event: Value| async move {
                    let mut result = json!({"text":event["answer"]});
                    if let Some(usage) = event.get("usage") {
                        result["usage"] = usage.clone();
                    }
                    Ok(result)
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
    use super::js_trim;

    #[test]
    fn answer_trim_uses_javascript_whitespace() {
        assert_eq!(js_trim("\u{feff} answer \u{feff}"), "answer");
        assert_eq!(js_trim("\u{0085}answer\u{0085}"), "\u{0085}answer\u{0085}");
    }
}
