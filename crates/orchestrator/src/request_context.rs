//! Request-only prompt context, owned by the invoking turn future.
//!
//! Generic context rows are projected next to their admitted user prompt after
//! history preparation and stay out of session history. A typed attachment
//! paired with a rendered fallback row is persisted under that row's identity
//! and retained for cancellation/replay.

use crate::{ConversationOrchestrator, OrchestratorError, TurnOutcome};
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, ContentBlock, ConversationMessage, MessageId,
};
use std::future::Future;
use tokio_util::sync::CancellationToken;

/// A prompt-ingress attachment row with the exact rendered cache Native writes
/// beside its typed payload. The message ID is reused as the JSONL attachment
/// UUID so retries and cold resume retain one identity.
#[derive(Clone, Debug)]
pub(crate) struct PromptIngressAttachment {
    pub(crate) message: ConversationMessage,
    pub(crate) payload: Utf16JsonProjection,
    pub(crate) rendered: Utf16JsonProjection,
}

/// A typed transcript sidecar associated with a request-context turn.
///
/// The rendered view is stored in JSONL beside the attachment payload. The
/// attached rendered row is retained in model history under this same ID so
/// cancellation and a later cold resume preserve the Native replay lane.
#[derive(Clone, Debug)]
pub struct RequestContextTranscriptAttachment {
    /// The model-visible fallback row's stable transcript identity.
    pub message_id: MessageId,
    /// Source-preserving payload stored under the JSONL `attachment` key.
    pub payload: Utf16JsonProjection,
    /// Exact rendered view stored under `rendered`.
    pub rendered: Utf16JsonProjection,
    /// Transcript role for the rendered view.
    pub rendered_role: &'static str,
}

struct PromptContext {
    prompt_id: std::sync::Mutex<MessageId>,
    rows: Vec<ConversationMessage>,
    /// Current-turn rows ordered around the request-only rows. Native places
    /// reminders first, then this request's fallback rows, then shared context.
    turn_projection: std::sync::Mutex<Option<(Vec<MessageId>, Vec<MessageId>)>>,
    transcript_attachment: std::sync::Mutex<Option<RequestContextTranscriptAttachment>>,
    /// The initial NAr/rle rows were persisted at prompt ingress and are
    /// staged at prompt ingress and persisted together after all current NAr
    /// producers have run.
    staged_ingress_attachments: std::sync::Mutex<Option<Vec<PromptIngressAttachment>>>,
    /// Producer results, the one placement selection, and append attempts for
    /// the complete ingress batch. Keeping empty producer results is important:
    /// retries must not consume a stateful producer twice just because its
    /// first result contained no attachment.
    ingress_batch: std::sync::Mutex<PromptIngressBatch>,
}

#[derive(Default)]
struct PromptIngressBatch {
    /// Attachment producers in the order they ran; empty vectors represent a
    /// producer that ran once and emitted no row.
    producer_rows: Vec<(String, Vec<PromptIngressAttachment>)>,
    /// Request-only Native API-system rows derived from the same producer
    /// batch. They are durable conversation messages but are not attachment
    /// candidates for the NAr placement selector.
    api_system_rows: Vec<(String, Vec<ConversationMessage>)>,
    /// Producers with a post-processing step (currently the deferred row's
    /// prompt attachment hook) are cached before that step and marked complete
    /// after it. A retry can finish the cached row without recomputing its diff.
    completed_producers: std::collections::HashSet<String>,
    /// Filled exactly once before the first durable attachment append.
    selected_before_closing_line_ids: Option<std::collections::HashSet<MessageId>>,
    /// A cancelable Batched preparation may be dropped before this boundary,
    /// but must finish once the initial append batch has begun.
    append_started: bool,
    /// Wakes a canceling Batched caller as soon as the initial append batch is
    /// durable, without waiting for unrelated preparation work after it.
    finalized_notify: std::sync::Arc<tokio::sync::Notify>,
    /// An append attempt is consumed before awaiting the best-effort writer so
    /// a cancelled retry cannot emit a duplicate row with the same UUID.
    append_attempted_ids: std::collections::HashSet<MessageId>,
    transcript_fallback_attempted: bool,
    finalized: bool,
}

tokio::task_local! {
    static REQUEST_CONTEXT: PromptContext;
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct PromptIngressAppendTestGate {
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
    claimed: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl PromptIngressAppendTestGate {
    pub(crate) async fn wait_until_entered(&self) {
        self.entered.notified().await;
    }

    pub(crate) fn resume(&self) {
        self.resume.notify_one();
    }

    async fn pause_once(&self) {
        if self.claimed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        self.entered.notify_one();
        self.resume.notified().await;
    }
}

#[cfg(test)]
tokio::task_local! {
    pub(crate) static PROMPT_INGRESS_APPEND_TEST_GATE: std::sync::Arc<PromptIngressAppendTestGate>;
}

#[cfg(test)]
pub(crate) async fn pause_after_first_ingress_append_for_test() {
    if let Ok(gate) = PROMPT_INGRESS_APPEND_TEST_GATE.try_with(std::sync::Arc::clone) {
        gate.pause_once().await;
    }
}

impl ConversationOrchestrator {
    /// Run a prompt with model-only context immediately before its user row.
    /// The scope follows this future through queue admission, retries, and
    /// fallback; cancellation or dropping the future cannot leave shared rows.
    pub async fn run_turn_streaming_with_request_context(
        &self,
        prompt: impl Into<Utf16JsonProjection>,
        context: Vec<Utf16JsonProjection>,
        transcript_attachment: Option<RequestContextTranscriptAttachment>,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, OrchestratorError> {
        let prompt_id = MessageId::new();
        let rows = context_rows(context, transcript_attachment.as_ref());
        REQUEST_CONTEXT
            .scope(
                PromptContext {
                    prompt_id: std::sync::Mutex::new(prompt_id),
                    rows,
                    turn_projection: std::sync::Mutex::new(None),
                    transcript_attachment: std::sync::Mutex::new(transcript_attachment),
                    staged_ingress_attachments: std::sync::Mutex::new(None),
                    ingress_batch: std::sync::Mutex::new(PromptIngressBatch::default()),
                },
                self.run_turn_streaming_with_cancel_image_sources_and_message_id(
                    prompt,
                    Vec::new(),
                    cancel,
                    Some(prompt_id),
                ),
            )
            .await
    }

    /// Batched counterpart using the same future-owned, nonpersistent context.
    pub async fn run_turn_with_request_context(
        &self,
        prompt: impl Into<Utf16JsonProjection>,
        context: Vec<Utf16JsonProjection>,
        transcript_attachment: Option<RequestContextTranscriptAttachment>,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, OrchestratorError> {
        let prompt_id = MessageId::new();
        REQUEST_CONTEXT
            .scope(
                PromptContext {
                    prompt_id: std::sync::Mutex::new(prompt_id),
                    rows: context_rows(context, transcript_attachment.as_ref()),
                    turn_projection: std::sync::Mutex::new(None),
                    transcript_attachment: std::sync::Mutex::new(transcript_attachment),
                    staged_ingress_attachments: std::sync::Mutex::new(None),
                    ingress_batch: std::sync::Mutex::new(PromptIngressBatch::default()),
                },
                self.run_turn_with_cancel_image_sources_and_message_id(
                    prompt,
                    Vec::new(),
                    cancel,
                    Some(prompt_id),
                ),
            )
            .await
    }
}

/// Position of an attachment producer in Native's ordinary `NAr` fan-out.
/// Stable sorting preserves per-producer/source order for multiple rows of the
/// same type while ensuring the durable first batch and its placement input use
/// the same source ordering.
fn native_ingress_source_order(row: &PromptIngressAttachment) -> u16 {
    match row.payload.string_at("/type") {
        Some("queued_command") => 0,
        Some("sandbox_instructions") => 10,
        Some("environment") => 20,
        Some("model") => 30,
        Some("output_style_instructions") => 40,
        Some("language") => 41,
        Some("deferred_tools_delta") => 50,
        Some("agent_listing_delta") => 60,
        Some("mcp_instructions_delta") => 70,
        Some("mcp_dropped_tools_delta") => 80,
        Some("edited_text_file") => 90,
        Some("skill_listing") => 100,
        Some("proactivity") => 110,
        Some("auto_mode") => 120,
        // The policy lane's withdrawn-row extension follows the Native
        // auto-mode row; it is not part of the Native `tee` allowlist.
        Some("auto_mode_exit") => 121,
        Some("total_tokens_reminder") => 130,
        Some("budget_usd") => 140,
        // Preserve producer call order for current rows without a Native NAr
        // counterpart; they still pass through the single placement selector.
        _ => 1000,
    }
}

fn sort_prompt_ingress_source_order(rows: &mut [PromptIngressAttachment]) {
    rows.sort_by_key(native_ingress_source_order);
}

/// Install per-turn ingress state for ordinary turns while preserving an
/// enclosing request-context scope. The user-row append fills the actual
/// prompt ID before preparation projects any staged rows.
pub(crate) async fn scope_if_missing<F: Future>(future: F) -> F::Output {
    if REQUEST_CONTEXT.try_with(|_| ()).is_ok() {
        future.await
    } else {
        REQUEST_CONTEXT
            .scope(
                PromptContext {
                    prompt_id: std::sync::Mutex::new(MessageId::new()),
                    rows: Vec::new(),
                    turn_projection: std::sync::Mutex::new(None),
                    transcript_attachment: std::sync::Mutex::new(None),
                    staged_ingress_attachments: std::sync::Mutex::new(None),
                    ingress_batch: std::sync::Mutex::new(PromptIngressBatch::default()),
                },
                future,
            )
            .await
    }
}

/// Stage the common typed prompt-ingress rows after the original user row is
/// appended. Native collects the complete NAr batch before its first durable
/// attachment append, so placement is selected later after MCP/deferred
/// producers have contributed their rows.
pub(crate) async fn persist_prompt_ingress_after_user(
    orchestrator: &ConversationOrchestrator,
    prompt_id: MessageId,
    is_regular_user_prompt: bool,
) {
    if !is_native_nar_ingress(is_regular_user_prompt) {
        return;
    }
    let _ = REQUEST_CONTEXT.try_with(|context| {
        *context
            .prompt_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = prompt_id;
    });

    // Request-context scopes need an identity-stable projection for PTL and
    // fallback rebuilds. A sealed empty batch is distinct from no staging;
    // normal turns already retain these durable rows in session history.
    let staged_already = REQUEST_CONTEXT
        .try_with(|context| {
            context
                .staged_ingress_attachments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some()
        })
        .unwrap_or(false);
    if !staged_already {
        let mut staged = Vec::new();
        staged.extend(orchestrator.environment_prompt_ingress_attachments().await);
        if let Some(model) = orchestrator.model_prompt_ingress_attachment().await {
            staged.push(model);
        }
        staged.extend(orchestrator.prompt_ingress_listing_attachments().await);
        if let Some(reminder) = orchestrator
            .total_tokens_prompt_ingress_attachment(true)
            .await
        {
            staged.push(reminder);
        }
        if let Some(reminder) = orchestrator.budget_usd_prompt_ingress_attachment().await {
            staged.push(reminder);
        }

        for attachment in &staged {
            orchestrator.stage_prompt_ingress_reminder(attachment).await;
        }

        let _ = REQUEST_CONTEXT.try_with(|context| {
            *context
                .staged_ingress_attachments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(staged);
        });
    }
}

/// Native `NAr` also runs for a real user request that is being sent through
/// the typed unknown-command fallback. That fallback is carried as a user-role
/// transcript attachment, so retain it as an explicit collector entry even if
/// the ordinary prompt classifier does not consider the current tail a regular
/// user prompt. Other metadata rows do not open the ingress collector.
pub(crate) fn is_native_nar_ingress(is_regular_user_prompt: bool) -> bool {
    is_regular_user_prompt
        || REQUEST_CONTEXT
            .try_with(|context| {
                context
                    .transcript_attachment
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                    .is_some_and(|attachment| {
                        attachment.payload.string_at("/type") == Some("unknown_command_fallback")
                    })
            })
            .unwrap_or(false)
}

/// Select and persist the complete first user-prompt attachment batch once.
/// All stateful producers cache their result in the task-local batch before
/// this point, including empty results.
pub(crate) async fn finalize_prompt_ingress_batch(
    orchestrator: &ConversationOrchestrator,
    cancel: Option<&CancellationToken>,
) {
    let enabled = std::env::var("CLAUDE_CODE_SESSION_START_ANNOUNCEMENTS_BEFORE_PROMPT")
        .ok()
        .is_some_and(|value| !value.is_empty());
    finalize_prompt_ingress_batch_with_enabled(orchestrator, enabled, cancel).await;
}

#[cfg(test)]
pub(crate) async fn finalize_prompt_ingress_batch_for_test(
    orchestrator: &ConversationOrchestrator,
    enabled: bool,
) {
    finalize_prompt_ingress_batch_with_enabled(orchestrator, enabled, None).await;
}

#[cfg(test)]
pub(crate) async fn finalize_prompt_ingress_batch_with_cancel_for_test(
    orchestrator: &ConversationOrchestrator,
    enabled: bool,
    cancel: &CancellationToken,
) {
    finalize_prompt_ingress_batch_with_enabled(orchestrator, enabled, Some(cancel)).await;
}

async fn finalize_prompt_ingress_batch_with_enabled(
    orchestrator: &ConversationOrchestrator,
    enabled: bool,
    cancel: Option<&CancellationToken>,
) {
    let Some((prompt_id, mut staged, api_system_rows)) = REQUEST_CONTEXT
        .try_with(|context| {
            let batch = context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if batch.finalized {
                return None;
            }
            let base_rows = context
                .staged_ingress_attachments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let base_rows = base_rows.as_ref()?;
            let staged = base_rows
                .iter()
                .cloned()
                .chain(
                    batch
                        .producer_rows
                        .iter()
                        .flat_map(|(_, rows)| rows.iter().cloned()),
                )
                .collect::<Vec<_>>();
            let api_system_rows = batch
                .api_system_rows
                .iter()
                .flat_map(|(_, rows)| rows.iter().cloned())
                .collect::<Vec<_>>();
            Some((
                *context
                    .prompt_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                staged,
                api_system_rows,
            ))
        })
        .ok()
        .flatten()
    else {
        // No request-owned ingress batch exists on ordinary continuation steps;
        // their typed reminders use the ordinary direct-persistence path.
        return;
    };
    sort_prompt_ingress_source_order(&mut staged);

    let sdk_query_source = crate::config::sanitize_query_source(&orchestrator.config.query_source)
        == crate::config::QUERY_SOURCE_SDK;
    let (prior_history, user_message, model, profile, trusted_closing_ids) = {
        let session = orchestrator.session.lock().await;
        let current_index = session
            .history
            .iter()
            .position(|message| message.id() == prompt_id);
        let Some(current_index) = current_index else {
            return;
        };
        (
            session.history[..current_index].to_vec(),
            session.history[current_index].clone(),
            session.model.clone(),
            session.model_profile.clone(),
            session.native_request_history.synthetic_closing_ids.clone(),
        )
    };
    for attachment in &staged {
        orchestrator.stage_prompt_ingress_reminder(attachment).await;
    }
    let mut typed_attachment_ids = std::collections::HashSet::new();
    let mut raw_attachment_types = std::collections::HashMap::new();
    {
        let attachments = orchestrator
            .transcript
            .model_reminder_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (id, payload) in attachments.iter() {
            typed_attachment_ids.insert(*id);
            if let Some(attachment_type) = payload.string_at("/type") {
                raw_attachment_types.insert(*id, attachment_type.to_owned());
            }
        }
    }

    for attachment in &staged {
        let id = attachment.message.id();
        typed_attachment_ids.insert(id);
        if let Some(attachment_type) = attachment.payload.string_at("/type") {
            raw_attachment_types.insert(id, attachment_type.to_owned());
        }
    }
    let current_turn_messages = std::iter::once(user_message)
        .chain(staged.iter().map(|attachment| attachment.message.clone()))
        .collect::<Vec<_>>();
    let model_supported = enabled
        && sdk_query_source
        && orchestrator
            .api
            .supports_native_mid_conversation_system(&model, profile.as_deref());
    let selected = REQUEST_CONTEXT
        .try_with(|context| {
            let mut batch = context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            batch
                .selected_before_closing_line_ids
                .get_or_insert_with(|| {
                    super::conversation::closing_placement::select_current_before_closing_line_ids(
                        enabled && sdk_query_source,
                        model_supported,
                        &prior_history,
                        &current_turn_messages,
                        &typed_attachment_ids,
                        &raw_attachment_types,
                        &trusted_closing_ids,
                    )
                })
                .clone()
        })
        .ok()
        .unwrap_or_default();
    if !selected.is_empty() {
        orchestrator
            .session
            .lock()
            .await
            .native_request_history
            .before_closing_line_ids
            .extend(selected);
    }

    // Cancellation at the placement boundary must leave the selected ID set
    // intact while deferring every durable append. A retry in this same ingress
    // scope reuses that selection and the producer caches above.
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        return;
    }

    // API-system tool changes are request-normalization rows, not Native NAr
    // attachments. Even though they are excluded from the placement selector,
    // their first durable write must follow that one complete-batch decision.
    // The commit boundary covers them together with the attachment rows so a
    // cancellation cannot leave the transcript tip ahead of staged delta state.
    for message in api_system_rows {
        let should_append = REQUEST_CONTEXT
            .try_with(|context| {
                context
                    .ingress_batch
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .append_attempted_ids
                    .insert(message.id())
            })
            .unwrap_or(true);
        if should_append {
            let _ = REQUEST_CONTEXT.try_with(|context| {
                context
                    .ingress_batch
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .append_started = true;
            });
            orchestrator.persist_message_to_jsonl(&message).await;
        }
    }

    for attachment in &staged {
        let should_append = REQUEST_CONTEXT
            .try_with(|context| {
                context
                    .ingress_batch
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .append_attempted_ids
                    .insert(attachment.message.id())
            })
            .unwrap_or(true);
        if should_append {
            let _ = REQUEST_CONTEXT.try_with(|context| {
                context
                    .ingress_batch
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .append_started = true;
            });
            orchestrator
                .persist_staged_prompt_ingress_reminder(attachment)
                .await;
        }
    }

    let attachment = REQUEST_CONTEXT
        .try_with(|context| {
            let mut batch = context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if batch.transcript_fallback_attempted {
                return None;
            }
            let attachment = context
                .transcript_attachment
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            batch.transcript_fallback_attempted = attachment.is_some();
            attachment
        })
        .ok()
        .flatten();
    if let Some(attachment) = attachment {
        let _ = REQUEST_CONTEXT.try_with(|context| {
            context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .append_started = true;
        });
        let attachment_kind = attachment.payload.string_at("/type").map(str::to_owned);
        let model_row = REQUEST_CONTEXT
            .try_with(|context| {
                context
                    .rows
                    .iter()
                    .find(|row| row.id() == attachment.message_id)
                    .cloned()
            })
            .ok()
            .flatten();
        if let Some(model_row) = model_row {
            let mut session = orchestrator.session.lock().await;
            orchestrator
                .transcript
                .model_reminder_attachments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(model_row.id(), attachment.payload.clone());
            if !session
                .history
                .iter()
                .any(|message| message.id() == model_row.id())
            {
                session.history.push(model_row.clone());
            }
            drop(session);
            orchestrator
                .persist_request_context_attachment_to_jsonl(attachment)
                .await;
            if let Some(kind) = attachment_kind {
                orchestrator
                    .register_mod_persisted_attachment(
                        &model_row,
                        &kind,
                        serde_json::json!({"kind":"engine"}),
                    )
                    .await;
            }
        } else {
            orchestrator
                .persist_request_context_attachment_to_jsonl(attachment)
                .await;
        }
    }
    let _ = REQUEST_CONTEXT.try_with(|context| {
        let mut batch = context
            .ingress_batch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        batch.finalized = true;
        batch.finalized_notify.notify_one();
    });
}

/// Read the sealed initial ingress batch for each preparation attempt. `None`
/// means this request has no ingress scope or staging has not run yet;
/// `Some(vec![])` means the eligible producers ran and emitted no rows. The
/// task-local owns the batch for the full turn scope, so a cancelled/failed
/// preparation can be retried with the same IDs.
pub(crate) fn staged_prompt_ingress_attachments() -> Option<Vec<PromptIngressAttachment>> {
    REQUEST_CONTEXT
        .try_with(|context| {
            let base = context
                .staged_ingress_attachments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()?;
            let batch = context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut rows = base
                .into_iter()
                .chain(
                    batch
                        .producer_rows
                        .iter()
                        .flat_map(|(_, rows)| rows.iter().cloned()),
                )
                .collect::<Vec<_>>();
            sort_prompt_ingress_source_order(&mut rows);
            Some(rows)
        })
        .unwrap_or(None)
}

/// Read a producer's cached typed result. `Some(vec![])` means it ran once and
/// emitted no rows; `None` means the producer has not run in this task scope.
pub(crate) fn prompt_ingress_producer_rows(producer: &str) -> Option<Vec<PromptIngressAttachment>> {
    REQUEST_CONTEXT
        .try_with(|context| {
            context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .producer_rows
                .iter()
                .find(|(name, _)| name == producer)
                .map(|(_, rows)| rows.clone())
        })
        .unwrap_or(None)
}

/// Cache model-only API-system changes produced from the initial Native NAr
/// rows. The rows are staged in history now and durably appended by the same
/// first-batch finalizer, after placement has been selected for all typed
/// attachments. They are deliberately excluded from attachment placement.
pub(crate) async fn prompt_ingress_api_system_rows_or_else<F>(
    orchestrator: &ConversationOrchestrator,
    producer: &str,
    produce: F,
) -> Vec<ConversationMessage>
where
    F: FnOnce() -> Vec<ConversationMessage>,
{
    let cached = REQUEST_CONTEXT
        .try_with(|context| {
            context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .api_system_rows
                .iter()
                .find(|(name, _)| name == producer)
                .map(|(_, rows)| rows.clone())
        })
        .unwrap_or(None);
    let rows = if let Some(rows) = cached {
        rows
    } else {
        let produced = produce();
        REQUEST_CONTEXT
            .try_with(|context| {
                let mut batch = context
                    .ingress_batch
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if batch.finalized {
                    return produced.clone();
                }
                if let Some((_, rows)) = batch
                    .api_system_rows
                    .iter()
                    .find(|(name, _)| name == producer)
                {
                    return rows.clone();
                }
                batch
                    .api_system_rows
                    .push((producer.to_owned(), produced.clone()));
                produced.clone()
            })
            .unwrap_or_else(|_| produced.clone())
    };

    if !rows.is_empty() {
        let prompt_id = match REQUEST_CONTEXT.try_with(|context| {
            *context
                .prompt_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }) {
            Ok(prompt_id) => prompt_id,
            Err(_) => return rows,
        };
        let mut session = orchestrator.session.lock().await;
        let mut insert_at = session
            .history
            .iter()
            .position(|message| message.id() == prompt_id)
            .map_or(session.history.len(), |index| index + 1);
        for row in &rows {
            if session
                .history
                .iter()
                .any(|message| message.id() == row.id())
            {
                continue;
            }
            session.history.insert(insert_at, row.clone());
            insert_at += 1;
        }
    }
    rows
}

/// Record one producer result before any later async hook or durable append can
/// yield. The first result wins, so retries reuse identical message IDs and do
/// not consume stateful producer state again.
pub(crate) async fn cache_prompt_ingress_producer_rows(
    orchestrator: &ConversationOrchestrator,
    producer: &str,
    rows: Vec<PromptIngressAttachment>,
) -> Option<Vec<PromptIngressAttachment>> {
    cache_prompt_ingress_producer_rows_with_completion(orchestrator, producer, rows, true).await
}

pub(crate) async fn cache_prompt_ingress_producer_rows_pending(
    orchestrator: &ConversationOrchestrator,
    producer: &str,
    rows: Vec<PromptIngressAttachment>,
) -> Option<Vec<PromptIngressAttachment>> {
    cache_prompt_ingress_producer_rows_with_completion(orchestrator, producer, rows, false).await
}

async fn cache_prompt_ingress_producer_rows_with_completion(
    orchestrator: &ConversationOrchestrator,
    producer: &str,
    rows: Vec<PromptIngressAttachment>,
    completed: bool,
) -> Option<Vec<PromptIngressAttachment>> {
    let stored = REQUEST_CONTEXT
        .try_with(|context| {
            if context
                .staged_ingress_attachments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none()
            {
                return None;
            }
            let mut batch = context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if batch.finalized {
                return None;
            }
            if let Some(existing_index) = batch
                .producer_rows
                .iter()
                .position(|(name, _)| name == producer)
            {
                let existing = batch.producer_rows[existing_index].1.clone();
                if completed {
                    batch.completed_producers.insert(producer.to_owned());
                }
                return Some((false, existing));
            }
            batch
                .producer_rows
                .push((producer.to_owned(), rows.clone()));
            if completed {
                batch.completed_producers.insert(producer.to_owned());
            }
            Some((true, rows))
        })
        .ok()
        .flatten()?;
    if stored.0 {
        for row in &stored.1 {
            orchestrator.stage_prompt_ingress_reminder(row).await;
        }
    }
    Some(stored.1)
}

pub(crate) fn prompt_ingress_producer_is_complete(producer: &str) -> bool {
    REQUEST_CONTEXT
        .try_with(|context| {
            context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .completed_producers
                .contains(producer)
        })
        .unwrap_or(false)
}

pub(crate) async fn complete_prompt_ingress_producer_rows(
    orchestrator: &ConversationOrchestrator,
    producer: &str,
    rows: Vec<PromptIngressAttachment>,
) -> Option<Vec<PromptIngressAttachment>> {
    let (found_producer, stored) = REQUEST_CONTEXT
        .try_with(|context| {
            let mut batch = context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(index) = batch
                .producer_rows
                .iter_mut()
                .position(|(name, _)| name == producer)
            else {
                return (false, Vec::new());
            };
            batch.producer_rows[index].1 = rows.clone();
            batch.completed_producers.insert(producer.to_owned());
            (true, rows)
        })
        .ok()?;
    if !found_producer {
        return None;
    }
    for row in &stored {
        orchestrator.stage_prompt_ingress_reminder(row).await;
    }
    Some(stored)
}

/// Run one ingress producer only when its task-local result has not already
/// been recorded. The result is cached synchronously before this helper awaits
/// any history staging, so retries keep the same IDs and state transition.
pub(crate) async fn prompt_ingress_producer_rows_or_else<F, Fut>(
    orchestrator: &ConversationOrchestrator,
    producer: &str,
    produce: F,
) -> Vec<PromptIngressAttachment>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Vec<PromptIngressAttachment>>,
{
    if let Some(rows) = prompt_ingress_producer_rows(producer) {
        return rows;
    }
    let rows = produce().await;
    cache_prompt_ingress_producer_rows(orchestrator, producer, rows.clone())
        .await
        .unwrap_or(rows)
}

/// Find one typed row in the request-owned ingress batch. A retry after the
/// first preparation reuses this identity and its already-consumed producer
/// state instead of computing the attachment again.
pub(crate) fn staged_prompt_ingress_attachment(
    attachment_type: &str,
) -> Option<PromptIngressAttachment> {
    staged_prompt_ingress_attachments()?
        .into_iter()
        .find(|row| row.payload.string_at("/type") == Some(attachment_type))
}

/// Whether initial ingress producers have already been consumed for this task
/// scope. This includes a finalized empty batch, so a retry cannot run a
/// consume-once producer a second time.
pub(crate) fn prompt_ingress_batch_finalized() -> bool {
    REQUEST_CONTEXT
        .try_with(|context| {
            context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .finalized
        })
        .unwrap_or(false)
}

/// Whether the initial durable attachment batch has crossed its cancellation
/// boundary. Batched preparation must finish the finalizer after this point.
pub(crate) fn prompt_ingress_batch_append_started() -> bool {
    REQUEST_CONTEXT
        .try_with(|context| {
            context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .append_started
        })
        .unwrap_or(false)
}

/// Wait only for the initial batch's durable append boundary. Batched callers
/// can then return cancellation without waiting for the remaining preparation
/// snapshot work.
pub(crate) async fn wait_for_prompt_ingress_batch_finalized() {
    let Ok(notify) = REQUEST_CONTEXT.try_with(|context| {
        std::sync::Arc::clone(
            &context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .finalized_notify,
        )
    }) else {
        return;
    };
    loop {
        let notified = notify.notified();
        tokio::pin!(notified);
        if prompt_ingress_batch_finalized() {
            return;
        }
        notified.await;
    }
}

/// Snapshot the conversation strictly before this task-local user's row. Native
/// NAr producers diff against this prefix; current-turn staged rows must never
/// become their own prior attachment state.
pub(crate) async fn prior_history_for_current_prompt(
    orchestrator: &ConversationOrchestrator,
) -> Vec<ConversationMessage> {
    let Some(prompt_id) = REQUEST_CONTEXT
        .try_with(|context| {
            *context
                .prompt_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        })
        .ok()
    else {
        return Vec::new();
    };
    let session = orchestrator.session.lock().await;
    let Some(current_index) = session
        .history
        .iter()
        .position(|message| message.id() == prompt_id)
    else {
        return Vec::new();
    };
    session.history[..current_index].to_vec()
}

/// Preserve a reminder's full UTF-16 rendered view as Native's JSONL cache.
pub(crate) fn rendered_system_message(
    message: &ConversationMessage,
) -> Option<Utf16JsonProjection> {
    let ConversationMessage::User { content, .. } = message else {
        return None;
    };
    let text = match content.first()? {
        ContentBlock::Text { text, .. } => {
            Utf16JsonProjection::root_string(text.clone(), text.encode_utf16().collect()).ok()?
        }
        ContentBlock::TextJsUtf16 {
            text,
            utf16_code_units,
            ..
        } => Utf16JsonProjection::root_string(text.clone(), utf16_code_units.clone()).ok()?,
        _ => return None,
    };
    let rendered_line = Utf16JsonProjection::object(vec![("content".into(), text)]).ok()?;
    Utf16JsonProjection::array(vec![rendered_line]).ok()
}

fn context_rows(
    mut context: Vec<Utf16JsonProjection>,
    transcript_attachment: Option<&RequestContextTranscriptAttachment>,
) -> Vec<ConversationMessage> {
    if context.is_empty() {
        if let Some(attachment) = transcript_attachment {
            if let Ok(content) = attachment.rendered.subprojection("/0/content") {
                context.push(content);
            }
        }
    }
    context
        .into_iter()
        .enumerate()
        .map(|(index, text)| {
            let row_attachment = (index == 0).then_some(transcript_attachment).flatten();
            let id = if let Some(attachment) = row_attachment {
                attachment.message_id
            } else {
                MessageId::new()
            };
            let is_meta = row_attachment
                .map(|attachment| attachment.rendered_role == "system")
                .unwrap_or(true);
            let text_units = text.string_units("").unwrap_or_default();
            let text = text.string_at("").unwrap_or_default().to_owned();
            let mut row = if is_meta {
                ConversationMessage::user_meta(id, text.clone())
            } else {
                ConversationMessage::user(id, text.clone())
            };
            if let ConversationMessage::User { content, .. } = &mut row {
                *content = vec![ContentBlock::TextJsUtf16 {
                    text,
                    utf16_code_units: text_units,
                    citations: None,
                }];
            }
            row
        })
        .collect()
}

/// Record this turn's physical message ordering around the request-owned
/// rows. The IDs are stable across PTL and fallback history reconstruction.
pub(crate) fn set_turn_projection(
    before_request_rows: Vec<MessageId>,
    after_request_rows: Vec<MessageId>,
) {
    let _ = REQUEST_CONTEXT.try_with(|context| {
        *context
            .turn_projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((before_request_rows, after_request_rows));
    });
}

/// Stop relocating durable request rows when the turn has moved past its
/// initial regular-user preparation. Generic request-only rows are still
/// reattached; rows already in history keep their durable position.
pub(crate) fn clear_turn_projection() {
    let _ = REQUEST_CONTEXT.try_with(|context| {
        *context
            .turn_projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    });
}

/// Attach only when the corresponding real user row survived preparation.
/// Re-applying to an already prepared snapshot preserves each context row once
/// and keeps the Native reminder → request-only → shared-context order.
pub(crate) fn attach(messages: &mut Vec<ConversationMessage>) {
    let _ = REQUEST_CONTEXT.try_with(|context| {
        let prompt_id = *context
            .prompt_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !messages.iter().any(|row| row.id() == prompt_id) {
            let request_ids = context
                .rows
                .iter()
                .map(ConversationMessage::id)
                .collect::<std::collections::HashSet<_>>();
            messages.retain(|row| !request_ids.contains(&row.id()));
            return;
        }

        let projection = context
            .turn_projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some((before_request, after_request)) = projection else {
            let existing = messages
                .iter()
                .map(ConversationMessage::id)
                .collect::<std::collections::HashSet<_>>();
            let request_rows = context
                .rows
                .iter()
                .filter(|row| !existing.contains(&row.id()))
                .cloned()
                .collect::<Vec<_>>();
            if let Some(index) = messages.iter().position(|row| row.id() == prompt_id) {
                messages.splice(index..index, request_rows);
            }
            return;
        };
        let mut ordered_ids = before_request;
        ordered_ids.extend(context.rows.iter().map(ConversationMessage::id));
        ordered_ids.extend(after_request);
        let mut seen = std::collections::HashSet::new();
        ordered_ids.retain(|id| seen.insert(*id));
        let ids = ordered_ids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        let ordered_rows = ordered_ids
            .iter()
            .filter_map(|id| {
                context
                    .rows
                    .iter()
                    .find(|row| row.id() == *id)
                    .cloned()
                    .or_else(|| messages.iter().find(|row| row.id() == *id).cloned())
            })
            .collect::<Vec<_>>();
        messages.retain(|row| !ids.contains(&row.id()));
        if let Some(index) = messages.iter().position(|row| row.id() == prompt_id) {
            messages.splice(index..index, ordered_rows);
        }
    });
}

#[cfg(test)]
#[path = "request_context_staging_test.rs"]
mod staging_tests;
