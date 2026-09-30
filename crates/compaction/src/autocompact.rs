//! Autocompact — LLM-driven summarization layer.
//!
//! Plan 08 wires the optional `ForkedAgentRunner` path: when a runner +
//! `CacheSafeParamsSlot` are configured via
//! [`Autocompactor::with_forked_runner`], `compact` issues a forked agent
//! that shares the parent's prompt cache. The forked agent is driven by the
//! byte-faithful compaction prompt (see [`crate::prompt`]); its raw response
//! runs through [`crate::prompt::format_compact_summary`] (strip `<analysis>`,
//! rewrite `<summary>` → `Summary:`) and is wrapped by
//! [`crate::prompt::get_compact_user_summary_message`] into the continuation
//! message that becomes the post-compact history.
//!
//! When neither a runner nor a slot is configured (the orchestrator's default
//! construction), the layer falls back to a clearly-labelled deterministic
//! summary built through the *same* `format_compact_summary` /
//! `get_compact_user_summary_message` pipeline (no model call is possible
//! without a runner). The old `[stub-summary attempt=…]` marker is gone — see
//! the divergence note on [`Autocompactor::compact`].

use cost::Usage;
use lingxi_core::types::ConversationMessage;
use sidequery::{CacheSafeParamsSlot, ForkedAgentRequest, ForkedAgentRunner, QuerySource};
use std::sync::Arc;
use thiserror::Error;

/// Result of one autocompact pass.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// Estimated tokens before compaction.
    pub pre_compact_token_count: u64,
    /// Estimated tokens after compaction (advertised).
    pub post_compact_token_count: u64,
    /// Actual measured tokens after compaction (post-call).
    pub true_post_compact_token_count: u64,
    /// Token/cost usage incurred by the summarization call, if any.
    pub compaction_usage: Option<Usage>,
    /// Actual model inherited by the summary side-query.
    pub summary_model: String,
    /// Resulting messages (typically a single summary system message).
    pub summary_messages: Vec<ConversationMessage>,
    /// Trimmed model text, before the continuation wrapper, for `PostCompact`.
    pub raw_summary_text: String,
    /// #58: the verbatim tail of recent messages preserved across the
    /// compaction boundary (`messagesToPreserve` from `DRn`, usage-zeroed via
    /// [`crate::partial::zero_preserved_tail_usage`] = TS `k4e`). Empty on the
    /// full-replacement path (short conversation / no preservable tail), in
    /// which case post-compact history is byte-identical to before this finding.
    /// Otherwise this rides AFTER the summary in the `Iqn`
    /// `[boundaryMarker, ...summaryMessages, ...messagesToKeep, ...]` order.
    pub messages_to_preserve: Vec<ConversationMessage>,
}

/// Errors surfaced by the autocompact layer.
#[derive(Debug, Clone, Error)]
pub enum CompactionError {
    /// Underlying API call failed.
    #[error(transparent)]
    Api(#[from] llm_runtime::LlmError),
    /// Exhausted PTL retries without success.
    #[error("exhausted")]
    MaxRetriesExceeded,
    /// Autocompact does not apply to this state.
    #[error("not applicable")]
    NotApplicable,
    /// A manual `/compact` needs at least one completed exchange to summarize
    /// while preserving a valid recent tail.
    #[error("Not enough messages to compact.")]
    NotEnoughMessages,
    /// The provider returned no usable summary or an explicit API error.
    #[error("{0}")]
    Summary(String),
    /// Media still exceeds provider limits after one stripped retry.
    #[error("media_unstrippable")]
    MediaUnstrippable,
    /// Internal logic error.
    #[error("internal: {0}")]
    Internal(String),
}

/// Tunables for the autocompact layer.
pub struct AutocompactConfig {
    /// Model used for summarization.
    pub summary_model: String,
    /// Maximum output tokens for the summary.
    pub max_output_tokens: Option<u64>,
    /// User prompt instructing the summarizer.
    pub compact_user_prompt: String,
}

impl Default for AutocompactConfig {
    fn default() -> Self {
        Self {
            summary_model: "claude-opus-4-6".into(),
            max_output_tokens: None,
            // Byte-faithful base compact prompt (TS `getCompactPrompt(None)`),
            // including the no-tools preamble/trailer.
            compact_user_prompt: crate::prompt::get_compact_prompt(None),
        }
    }
}

/// Stateful autocompactor.
///
/// Two construction paths:
/// - [`Autocompactor::new`] / [`Autocompactor::default`] — no forked
///   runner, returns the M1.7 stub summary. Used by the orchestrator's
///   default wiring and the e2e test that does not need a real
///   summarization call.
/// - [`Autocompactor::with_forked_runner`] — Plan 08 path, routes through
///   the shared `ForkedAgentRunner` using the latest `CacheSafeParams`
///   from the supplied slot. Closes spec gap **C2**.
#[derive(Default)]
pub struct Autocompactor {
    /// Tunables; defaults to [`AutocompactConfig::default`].
    pub config: AutocompactConfig,
    forked_runner: Option<Arc<ForkedAgentRunner>>,
    cache_slot: Option<Arc<CacheSafeParamsSlot>>,
}

/// Overrides that make one `compact_impl` pass a `/rewind` message-selector
/// summarize instead of an ordinary compaction — oracle `zir`'s two deltas
/// against `Juy`/`Ejt`.
struct SelectorSummarize {
    /// `xer(instructions, direction)` — the direction-specific body.
    prompt: String,
    /// `pbe`'s `suppressFollowUpQuestions`.
    suppress_follow_up_questions: bool,
}

impl Autocompactor {
    /// Construct an autocompactor with default config and no forked runner
    /// (falls back to the M1.7 stub summary).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct an autocompactor that routes through `forked_runner`,
    /// reading the latest cache-safe prompt prefix from `cache_slot`. This
    /// is the Plan 08 production path.
    #[must_use]
    pub fn with_forked_runner(
        forked_runner: Arc<ForkedAgentRunner>,
        cache_slot: Arc<CacheSafeParamsSlot>,
    ) -> Self {
        Self {
            config: AutocompactConfig::default(),
            forked_runner: Some(forked_runner),
            cache_slot: Some(cache_slot),
        }
    }

    /// Compact `messages` into a summary.
    ///
    /// When a forked runner is configured, issues a forked-agent call driven
    /// by the byte-faithful compaction prompt and shares the parent's prompt
    /// cache via the latest `CacheSafeParams` snapshot. The raw model text is
    /// run through [`crate::prompt::format_compact_summary`] then wrapped by
    /// [`crate::prompt::get_compact_user_summary_message`] (with
    /// `suppress_follow_up_questions = true`, `transcript_path = None`) — the
    /// TS `compact.ts` summary-request path.
    ///
    /// The summary is a user message carrying `isCompactSummary` and
    /// `isVisibleInTranscriptOnly`, matching Claude Code's compact-history
    /// representation. The user-facing text is byte-for-byte the TS string.
    ///
    /// **Fallback (no runner/slot).** Without a runner there is no model to
    /// call, so the layer cannot produce a real summary. Rather than ship a
    /// model-call result, it emits a clearly-labelled deterministic
    /// continuation message built through the *same* formatting pipeline. The
    /// production CLI path always wires `with_forked_runner`, so this fallback
    /// never reaches a wired binary. This replaces the prior
    /// `[stub-summary attempt=…]` placeholder.
    ///
    /// # Errors
    ///
    /// Returns [`CompactionError::Internal`] when the configured runner is
    /// present but the slot is empty, or the forked call fails.
    #[allow(clippy::cast_possible_truncation)]
    pub async fn compact(
        &self,
        messages: Vec<ConversationMessage>,
    ) -> Result<CompactionResult, CompactionError> {
        self.compact_with_instructions(messages, None).await
    }

    /// Compact `messages`, appending optional caller/hook instructions to the
    /// byte-faithful compaction prompt.
    ///
    /// The ordinary automatic path calls [`Self::compact`] and therefore keeps
    /// the configured base prompt. Successful `PreCompact` hook stdout uses
    /// this seam so its instructions reach the summarizer exactly once
    /// (manual `/compact <focus>` routes through
    /// [`Self::compact_manual_with_instructions`] instead).
    pub async fn compact_with_instructions(
        &self,
        messages: Vec<ConversationMessage>,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult, CompactionError> {
        self.compact_impl(messages, custom_instructions, false, None, None)
            .await
    }

    /// Run the explicit `/compact` manual path.
    ///
    /// VERIFIED against the 2.1.211 binary (`edy → Juy → mXi → Nto`): the
    /// manual compactor is the SAME group compactor as the reactive path —
    /// it starts with `s = 1` (the LAST API-round group preserved verbatim,
    /// `messagesToPreserve: m.flat()`), and errors `too_few_groups` when the
    /// summarize prefix contains no assistant message. (The full-history
    /// `messagesToKeep: []` path — `Pto` — is only ever called with
    /// `isAutoCompact: !0`.) Where the manual path DOES differ from auto:
    /// - no wired summarizer is a hard error (an explicit command must never
    ///   fake success), and
    /// - no valid preserved-tail split is `Not enough messages to compact.`
    ///   (auto falls back to full replacement instead).
    pub async fn compact_manual_with_instructions(
        &self,
        messages: Vec<ConversationMessage>,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult, CompactionError> {
        self.compact_impl(messages, custom_instructions, true, None, None)
            .await
    }

    /// Summarize ONE SIDE of a `/rewind` message-selector split — oracle `zir`.
    ///
    /// `context` is what the summarizer sees, which is NOT always what is being
    /// summarized: [`crate::selector::SummarizeSplit::summarizer_context`]
    /// hands `up_to` only the summarized half and `from` the whole conversation
    /// (oracle `Ze = S==="up_to" ? V : e`). The prompt tells the model which
    /// part of that context to summarize.
    ///
    /// Returns an empty `messages_to_preserve`: the caller owns the kept half
    /// and the assembly order, because only it knows which SIDE survives.
    ///
    /// # Errors
    /// [`CompactionError::Internal`] when no forked summarizer is wired — this
    /// is an explicit user command, so it must never fall back to the
    /// deterministic no-model stub and report success.
    pub async fn summarize_selection(
        &self,
        context: Vec<ConversationMessage>,
        custom_instructions: Option<&str>,
        direction: crate::prompt::SummarizeDirection,
    ) -> Result<CompactionResult, CompactionError> {
        if !self
            .forked_runner
            .as_ref()
            .is_some_and(|runner| runner.has_side_query_client())
        {
            return Err(CompactionError::Internal(
                "no forked summarizer wired".into(),
            ));
        }
        let overrides = SelectorSummarize {
            prompt: crate::prompt::get_summarize_prompt(custom_instructions, direction),
            // `pbe(Je, {suppressFollowUpQuestions: !1, …})`. Every automatic and
            // `/compact` path passes `!0`; this one passes FALSE, because the
            // user asked for a summary and is still sitting in the conversation
            // — the "Resume directly, do not acknowledge the summary"
            // continuation would be addressed to nobody.
            suppress_follow_up_questions: false,
        };
        self.compact_impl(context, custom_instructions, false, None, Some(overrides))
            .await
    }

    /// Rescue a provider overflow, preserving recent rounds and using the
    /// reported token gap to choose the first summarize prefix (2.1.261 x0e).
    pub async fn compact_reactive_with_instructions(
        &self,
        messages: Vec<ConversationMessage>,
        custom_instructions: Option<&str>,
        initial_token_gap: Option<u64>,
    ) -> Result<CompactionResult, CompactionError> {
        self.compact_impl(messages, custom_instructions, true, initial_token_gap, None)
            .await
    }

    async fn compact_impl(
        &self,
        messages: Vec<ConversationMessage>,
        custom_instructions: Option<&str>,
        preserve_tail: bool,
        initial_token_gap: Option<u64>,
        selector: Option<SelectorSummarize>,
    ) -> Result<CompactionResult, CompactionError> {
        let pre = crate::grouping::estimate_tokens_for_range(&messages);
        if preserve_tail
            && !self
                .forked_runner
                .as_ref()
                .is_some_and(|runner| runner.has_side_query_client())
        {
            return Err(CompactionError::Internal(
                "no forked summarizer wired".into(),
            ));
        }

        let groups = crate::grouping::group_messages_by_api_round(&messages);
        if preserve_tail && groups.len() < 2 {
            return Err(CompactionError::NotEnoughMessages);
        }
        let group_tokens: Vec<_> = groups.iter().map(|group| group.estimated_tokens).collect();
        let mut groups_preserved = usize::from(preserve_tail);
        if preserve_tail && groups.len() > 3 {
            if let Some(gap) = initial_token_gap {
                let remaining_gap = gap.saturating_sub(*group_tokens.last().unwrap_or(&0));
                if remaining_gap > 0 {
                    groups_preserved += preserved_group_step(
                        &group_tokens[..groups.len() - 1],
                        Some(remaining_gap),
                    );
                }
            }
        }
        let split_prefix = |preserved: usize| {
            let split_at = groups
                .get(groups.len().saturating_sub(preserved))
                .map_or(0, |group| group.start);
            let prefix = &messages[..split_at];
            prefix
                .iter()
                .any(|message| matches!(message, ConversationMessage::Assistant { .. }))
                .then_some(split_at)
        };
        let mut split_at = if preserve_tail {
            split_prefix(groups_preserved).ok_or(CompactionError::NotEnoughMessages)?
        } else {
            messages.len()
        };

        if let (Some(runner), Some(slot)) = (&self.forked_runner, &self.cache_slot) {
            let mut cache_params = slot
                .get_last()
                .await
                .ok_or_else(|| CompactionError::Internal("no cache-safe params".into()))?;
            let summary_model = if cache_params
                .tool_use_options
                .main_loop_model
                .trim()
                .is_empty()
            {
                self.config.summary_model.clone()
            } else {
                cache_params.tool_use_options.main_loop_model.clone()
            };
            let mut summarize = messages[..split_at].to_vec();
            let mut stripped_media = false;
            let mut head_truncations = 0;
            // The message-selector path brings its own body (`xer`); every other
            // path uses the base prompt, prebuilt unless focus text was given.
            let prompt = if let Some(overrides) = selector.as_ref() {
                overrides.prompt.clone()
            } else if custom_instructions
                .is_some_and(|text| !crate::prompt::trim_compact_text(text).is_empty())
            {
                crate::prompt::get_compact_prompt(custom_instructions)
            } else {
                self.config.compact_user_prompt.clone()
            };
            // Ejt reuses its prompt, while each PCo attempt creates a new row.
            let mut summary_request =
                ConversationMessage::user(lingxi_core::types::MessageId::new(), prompt.clone());
            let result = loop {
                if preserve_tail {
                    summary_request = ConversationMessage::user(
                        lingxi_core::types::MessageId::new(),
                        prompt.clone(),
                    );
                }
                cache_params.fork_context_messages = if stripped_media {
                    crate::strip_media::strip_images_from_messages(summarize.clone())
                } else {
                    summarize.clone()
                };
                let req = ForkedAgentRequest {
                    prompt_messages: vec![summary_request.clone()],
                    cache_safe_params: cache_params.clone(),
                    fork_label: if preserve_tail {
                        "reactive-compact"
                    } else {
                        "compact"
                    }
                    .into(),
                    query_source: QuerySource::Compaction,
                    max_output_tokens: self
                        .config
                        .max_output_tokens
                        .map(|value| u32::try_from(value).unwrap_or(u32::MAX)),
                };
                let response = runner.run(req).await;
                let token_gap = match response {
                    Ok(result) => {
                        if !result
                            .final_text
                            .starts_with(crate::prompt_too_long::PROMPT_TOO_LONG_ERROR_MESSAGE)
                        {
                            break result;
                        }
                        crate::prompt_too_long::prompt_too_long_token_gap(&result.final_text)
                    }
                    Err(sidequery::ForkError::Api(sidequery::SideQueryError::Api(
                        llm_runtime::LlmError::ContextOverflow { token_gap },
                    ))) => token_gap,
                    Err(sidequery::ForkError::Api(sidequery::SideQueryError::Api(error)))
                        if is_media_compaction_error(&error) =>
                    {
                        if stripped_media {
                            return Err(CompactionError::MediaUnstrippable);
                        }
                        stripped_media = true;
                        continue;
                    }
                    Err(sidequery::ForkError::Api(sidequery::SideQueryError::Api(error))) => {
                        return Err(CompactionError::Api(error));
                    }
                    Err(error) => return Err(CompactionError::Summary(error.to_string())),
                };

                if preserve_tail {
                    // x0e keeps more trailing rounds after PTL. The oldest
                    // conversation remains in every request and in the result.
                    let summarized_groups = groups.len().saturating_sub(groups_preserved);
                    groups_preserved += preserved_group_step(
                        &group_tokens[..summarized_groups],
                        (token_gap > 0).then_some(token_gap),
                    );
                    split_at = split_prefix(groups_preserved)
                        .ok_or(CompactionError::MaxRetriesExceeded)?;
                    summarize = messages[..split_at].to_vec();
                } else {
                    // Ejt full automatic compaction retries only the summary
                    // request, with up to three oldest-group truncations.
                    if head_truncations >= crate::thresholds::MAX_PTL_RETRIES {
                        return Err(CompactionError::MaxRetriesExceeded);
                    }
                    summarize = crate::ptl_retry::truncate_head_for_ptl_retry(summarize, token_gap)
                        .ok_or(CompactionError::MaxRetriesExceeded)?;
                    head_truncations += 1;
                }
            };
            let raw_summary_text = crate::prompt::trim_compact_text(&result.final_text).to_owned();
            if raw_summary_text.is_empty() {
                return Err(CompactionError::Summary(if preserve_tail {
                    "summarization produced empty response".into()
                } else {
                    "Failed to generate conversation summary - response did not contain valid text content".into()
                }));
            }
            let summary_text = crate::prompt::get_compact_user_summary_message_with(
                &raw_summary_text,
                selector
                    .as_ref()
                    .map_or(true, |overrides| overrides.suppress_follow_up_questions),
                cache_params
                    .transcript_path
                    .as_deref()
                    .and_then(std::path::Path::to_str),
                // `pbe`'s `recentMessagesPreserved`: NO oracle caller in 2.1.270
                // sets it (checked at all three `pbe(` sites), so passing
                // `false` here is parity, not an omission — the parameter
                // exists because the function is ported whole.
                false,
                // `headTruncated`: a PTL retry dropped the oldest messages, so
                // the summary does not cover them and the model must be told.
                head_truncations > 0,
            );
            let summary_messages = vec![ConversationMessage::compact_summary(
                lingxi_core::types::MessageId::new(),
                summary_text,
            )];
            return Ok(CompactionResult {
                pre_compact_token_count: pre,
                post_compact_token_count: crate::grouping::estimate_tokens_for_range(
                    &summary_messages,
                ),
                true_post_compact_token_count: result.usage.tokens.input,
                compaction_usage: Some(result.usage),
                summary_model,
                summary_messages,
                raw_summary_text,
                messages_to_preserve: if preserve_tail {
                    crate::partial::zero_preserved_tail_usage(messages[split_at..].to_vec())
                } else {
                    Vec::new()
                },
            });
        }

        // Default construction remains a deterministic automatic-test seam.
        // User-requested/manual and overflow rescue always require a real model.
        let raw_summary_text = format!("<summary>\n[autocompact fallback: no forked summarizer wired; {} messages elided]\n</summary>", messages.len());
        let summary_text =
            crate::prompt::get_compact_user_summary_message(&raw_summary_text, true, None, false);
        let summary_messages = vec![ConversationMessage::compact_summary(
            lingxi_core::types::MessageId::new(),
            summary_text,
        )];
        let post = crate::grouping::estimate_tokens_for_range(&summary_messages);
        Ok(CompactionResult {
            pre_compact_token_count: pre,
            post_compact_token_count: post,
            true_post_compact_token_count: post,
            compaction_usage: Some(Usage::default()),
            summary_model: self.config.summary_model.clone(),
            summary_messages,
            raw_summary_text,
            messages_to_preserve: Vec::new(),
        })
    }
}

/// 2.1.261 rPn: sum from the summarize tail, falling back to half the
/// remaining groups when the reported gap would consume nearly all of them.
fn preserved_group_step(tokens: &[u64], gap: Option<u64>) -> usize {
    let Some(gap) = gap else {
        return 1;
    };
    let mut total = 0_u64;
    let mut count = 0;
    for &tokens in tokens.iter().rev() {
        total = total.saturating_add(tokens);
        count += 1;
        if total >= gap {
            break;
        }
    }
    if count >= tokens.len().saturating_sub(1) {
        (tokens.len() / 2).max(1)
    } else {
        count
    }
}

fn is_media_compaction_error(error: &llm_runtime::LlmError) -> bool {
    // Non-vision summary routes reject raw history media before sending HTTP.
    // Reuse the bounded media retry, retaining text and prior MediaAnalysis.
    if let llm_runtime::LlmError::UnsupportedCapability { capability } = error {
        return matches!(capability.as_str(), "vision" | "documents");
    }
    if matches!(error, llm_runtime::LlmError::RequestTooLarge) {
        return true;
    }
    let llm_runtime::LlmError::InvalidRequest { message } = error else {
        return false;
    };
    let message = message.to_lowercase();
    // Oracle wSo/yIe media classifiers, including structured provider reason tags.
    [
        "request_too_large",
        "image_block",
        "document_block",
        "media_budget",
        "could not process image",
        "image exceeds",
        "image dimensions exceed",
        "image does not match the provided media type",
        "image cannot be empty",
        "exceeds api limit",
        "images exceed the api limit",
        "unable to resize image",
        "unable to compress image",
        "image file is empty",
        "could not process pdf",
        "pdf pages",
        "the pdf specified was not valid",
        "the pdf specified is password protected",
        "pdf cannot be empty",
        "too much media",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lingxi_core::types::{ContentBlock, MessageId, ToolUseId};
    use serde_json::json;
    use sidequery::{
        CacheSafeParams, SideQueryClient, SideQueryError, SideQueryRequest, SideQueryResponse,
    };
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;
    use tool_api::context::ToolUseOptions;

    /// Mock `SideQueryClient` returning a canned raw summary string and
    /// recording the request it was handed.
    struct MockClient {
        seen: Mutex<Option<SideQueryRequest>>,
        canned_text: String,
    }

    #[async_trait]
    impl SideQueryClient for MockClient {
        async fn query(
            &self,
            request: SideQueryRequest,
        ) -> Result<SideQueryResponse, SideQueryError> {
            *self.seen.lock().unwrap() = Some(request);
            Ok(SideQueryResponse {
                text: Some(self.canned_text.clone()),
                structured: None,
                tool_calls: Vec::new(),
                usage: Usage::default(),
                stop_reason: Some("end_turn".into()),
                retry_count: 0,
            })
        }
    }

    fn user_msg(text: &str) -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), text.into())
    }

    fn tool_use_options() -> ToolUseOptions {
        ToolUseOptions {
            debug: false,
            verbose: false,
            main_loop_model: "test".into(),
            model_profile: None,
            max_budget_nano_usd: None,
            mcp_clients: vec![],
            is_non_interactive_session: false,
            custom_system_prompt: None,
            append_system_prompt: None,
        }
    }

    fn cache_safe_params(prefix: Vec<ConversationMessage>) -> CacheSafeParams {
        CacheSafeParams {
            system_prompt: std::sync::Arc::from("PARENT SYSTEM PROMPT"),
            user_context: HashMap::new(),
            system_context: HashMap::new(),
            user_context_message: None,
            tool_use_options: tool_use_options(),
            fork_context_messages: prefix,
            transcript_path: None,
            generation: 0,
            tools: Vec::new(),
            effort: None,
        }
    }

    /// Build an `Autocompactor` wired to a mock client returning `canned_text`,
    /// with a `CacheSafeParamsSlot` pre-populated from `prefix`. Returns the
    /// compactor plus the shared mock client (to assert the sent request).
    async fn wired(
        canned_text: &str,
        prefix: Vec<ConversationMessage>,
    ) -> (Autocompactor, Arc<MockClient>) {
        let client = Arc::new(MockClient {
            seen: Mutex::new(None),
            canned_text: canned_text.into(),
        });
        let runner = Arc::new(
            ForkedAgentRunner::new()
                .with_side_query_client(client.clone(), "claude-opus-4-6".into()),
        );
        let slot = Arc::new(CacheSafeParamsSlot::new());
        slot.save(cache_safe_params(prefix)).await;
        (Autocompactor::with_forked_runner(runner, slot), client)
    }

    /// Mock returning a queue of canned texts (one per call) and recording the
    /// message count of every request, so the PTL retry tests can assert both the
    /// retry count and that each retry's prompt shrank.
    struct SeqMockClient {
        texts: Mutex<VecDeque<Result<String, llm_runtime::LlmError>>>,
        seen_lens: Mutex<Vec<usize>>,
        seen: Mutex<Vec<SideQueryRequest>>,
    }

    #[async_trait]
    impl SideQueryClient for SeqMockClient {
        async fn query(
            &self,
            request: SideQueryRequest,
        ) -> Result<SideQueryResponse, SideQueryError> {
            self.seen_lens.lock().unwrap().push(request.messages.len());
            self.seen.lock().unwrap().push(request);
            let text = self
                .texts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(String::new()))
                .map_err(SideQueryError::Api)?;
            Ok(SideQueryResponse {
                text: Some(text),
                structured: None,
                tool_calls: Vec::new(),
                usage: Usage::default(),
                stop_reason: Some("end_turn".into()),
                retry_count: 0,
            })
        }
    }

    /// Wire an autocompactor to a `SeqMockClient` that yields `texts` in order,
    /// with the fork-context prefix pre-populated from `prefix`.
    async fn wired_seq(
        texts: Vec<String>,
        prefix: Vec<ConversationMessage>,
    ) -> (Autocompactor, Arc<SeqMockClient>) {
        let client = Arc::new(SeqMockClient {
            texts: Mutex::new(texts.into_iter().map(Ok).collect()),
            seen_lens: Mutex::new(Vec::new()),
            seen: Mutex::new(Vec::new()),
        });
        let runner = Arc::new(
            ForkedAgentRunner::new()
                .with_side_query_client(client.clone(), "claude-opus-4-6".into()),
        );
        let slot = Arc::new(CacheSafeParamsSlot::new());
        slot.save(cache_safe_params(prefix)).await;
        (Autocompactor::with_forked_runner(runner, slot), client)
    }

    /// Assistant message carrying a tool-use block under `id` (opens a group).
    fn assistant_tool(id: MessageId, tool: &str) -> ConversationMessage {
        ConversationMessage::Assistant {
            id,
            content: vec![ContentBlock::ToolUse {
                id: ToolUseId::new(),
                name: tool.into(),
                input: json!({}),
                provider_id: None,
            }],
            stop_reason: None,
        }
    }

    fn tool_result_msg() -> ConversationMessage {
        ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: ToolUseId::new(),
                content: "ok".into(),
                is_error: false,
                provider_tool_use_id: None,
                content_blocks: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        }
    }

    #[tokio::test]
    async fn compact_wraps_forked_summary_through_format_pipeline() {
        // Spec test plan: mock SideQueryClient returning
        // `<analysis>x</analysis><summary>S</summary>` → wrapped output
        // contains `Summary:\nS` and the continuation sentence.
        let (compactor, _client) = wired(
            "<analysis>scratch thoughts</analysis><summary>S</summary>",
            vec![],
        )
        .await;

        let result = compactor
            .compact(vec![user_msg("hello"), user_msg("world")])
            .await
            .expect("wired compact succeeds");

        assert_eq!(result.summary_messages.len(), 1);
        // Divergence: emitted as a User message (TS uses isCompactSummary).
        let msg = &result.summary_messages[0];
        assert!(
            matches!(msg, ConversationMessage::User { .. }),
            "summary should be a User message, got {msg:?}"
        );
        let text = msg.text_content();

        // <analysis> stripped, <summary> → Summary:\nS.
        assert!(
            !text.contains("scratch thoughts"),
            "analysis not stripped: {text}"
        );
        assert!(
            !text.contains("<analysis>"),
            "raw analysis tag leaked: {text}"
        );
        assert!(
            !text.contains("<summary>"),
            "raw summary tag leaked: {text}"
        );
        assert!(
            text.contains("Summary:\nS"),
            "missing unwrapped summary: {text}"
        );

        // Continuation preamble + suppress-follow-up sentence are byte-faithful.
        assert!(text.starts_with(
            "This session is being continued from a previous conversation that ran out of context."
        ));
        assert!(text.contains(
            "Continue the conversation from where it left off without asking the user any further questions."
        ));
        assert!(text.contains("Pick up the last task as if the break never happened."));
    }

    #[tokio::test]
    async fn compact_sends_byte_faithful_compact_prompt_after_cache_prefix() {
        let (compactor, client) = wired("<summary>ok</summary>", vec![user_msg("PREFIX-A")]).await;

        compactor
            .compact(vec![user_msg("CURRENT-HISTORY")])
            .await
            .expect("wired compact succeeds");

        let sent = client.seen.lock().unwrap().clone().expect("client called");
        // The forked request replays the cache prefix first, then the compact
        // prompt (the byte-faithful `get_compact_prompt(None)`).
        let texts: Vec<String> = sent
            .messages
            .iter()
            .map(ConversationMessage::text_content)
            .collect();
        assert_eq!(texts.len(), 2, "prefix + compact prompt");
        assert_eq!(
            texts[0], "CURRENT-HISTORY",
            "the summary must replay the live history, not the stale slot clone"
        );
        assert_eq!(texts[1], crate::prompt::get_compact_prompt(None));
        // Parent system prompt replayed verbatim for the cache hit.
        assert_eq!(sent.system_prompt.as_deref(), Some("PARENT SYSTEM PROMPT"));
        // Usage from the forked call is carried through.
    }

    #[tokio::test]
    async fn compact_appends_manual_focus_to_prompt() {
        let (compactor, client) = wired("<summary>ok</summary>", vec![]).await;

        compactor
            .compact_with_instructions(vec![user_msg("current")], Some("Focus on the Rust changes"))
            .await
            .expect("wired compact succeeds");

        let sent = client.seen.lock().unwrap().clone().expect("client called");
        let prompt = sent.messages.last().expect("prompt message").text_content();
        assert_eq!(
            prompt,
            crate::prompt::get_compact_prompt(Some("Focus on the Rust changes"))
        );
    }

    /// Binary-verified (`Juy → mXi → Nto`, `s = 1`): the MANUAL path summarizes
    /// the group prefix and preserves the last API-round group verbatim —
    /// `[q1],[a1,q2],[a2]` → summarize `[q1,a1,q2]`, preserve `[a2]` — with the
    /// "Recent messages are preserved verbatim." sentence in the continuation.
    #[tokio::test]
    async fn manual_compact_summarizes_prefix_and_preserves_last_group() {
        let history = vec![
            user_msg("q1"),
            assistant_text("first reply"),
            user_msg("q2"),
            assistant_text("second reply"),
        ];
        let (compactor, client) = wired("<summary>ok</summary>", history.clone()).await;

        let result = compactor
            .compact_manual_with_instructions(history.clone(), None)
            .await
            .expect("manual compact succeeds");

        assert_eq!(
            result.messages_to_preserve.len(),
            1,
            "the last assistant-led group is preserved verbatim"
        );
        assert_eq!(
            result.messages_to_preserve[0].text_content(),
            "second reply"
        );
        let sent = client.seen.lock().unwrap().clone().expect("client called");
        let replayed: Vec<String> = sent.messages[..sent.messages.len() - 1]
            .iter()
            .map(ConversationMessage::text_content)
            .collect();
        let expected: Vec<String> = history[..3]
            .iter()
            .map(ConversationMessage::text_content)
            .collect();
        assert_eq!(
            replayed, expected,
            "the summarizer sees only the summarize prefix, never the preserved tail"
        );
        assert!(!result.summary_messages[0]
            .text_content()
            .contains("Recent messages are preserved verbatim."));
    }

    // ===== `/rewind` message-selector summarize (oracle `zir`) ==============

    /// The direction must reach the wire. Both options run the SAME code path,
    /// so a direction that is accepted and then ignored produces a plausible
    /// summary of the wrong half with every other assertion still green.
    #[tokio::test]
    async fn summarize_selection_sends_the_direction_specific_prompt() {
        use crate::prompt::SummarizeDirection;

        for direction in [SummarizeDirection::UpTo, SummarizeDirection::From] {
            let (compactor, client) = wired("<summary>ok</summary>", vec![]).await;
            compactor
                .summarize_selection(vec![user_msg("history")], None, direction)
                .await
                .expect("wired summarize succeeds");
            let sent = client.seen.lock().unwrap().clone().expect("client called");
            let prompt = sent.messages.last().expect("prompt message").text_content();
            assert_eq!(
                prompt,
                crate::prompt::get_summarize_prompt(None, direction),
                "{direction:?} must send its own body"
            );
            assert_ne!(
                prompt,
                crate::prompt::get_compact_prompt(None),
                "{direction:?} must not fall back to the base compact prompt"
            );
        }
    }

    #[tokio::test]
    async fn summarize_selection_appends_the_users_context() {
        use crate::prompt::SummarizeDirection;
        let (compactor, client) = wired("<summary>ok</summary>", vec![]).await;
        compactor
            .summarize_selection(
                vec![user_msg("history")],
                Some("keep the migration notes"),
                SummarizeDirection::From,
            )
            .await
            .expect("wired summarize succeeds");
        let sent = client.seen.lock().unwrap().clone().expect("client called");
        assert_eq!(
            sent.messages.last().unwrap().text_content(),
            crate::prompt::get_summarize_prompt(
                Some("keep the migration notes"),
                SummarizeDirection::From
            )
        );
    }

    /// `pbe(Je, {suppressFollowUpQuestions: !1, …})` — the ONE `pbe` call site
    /// in the oracle that passes false.
    ///
    /// The user asked for this summary and is still sitting in the
    /// conversation; "Resume directly — do not acknowledge the summary" is
    /// addressed to a session that is being RESUMED, which this is not.
    #[tokio::test]
    async fn a_selector_summary_does_not_carry_the_resume_continuation() {
        use crate::prompt::SummarizeDirection;
        let (compactor, _client) = wired("<summary>ok</summary>", vec![]).await;
        let result = compactor
            .summarize_selection(vec![user_msg("history")], None, SummarizeDirection::UpTo)
            .await
            .expect("wired summarize succeeds");
        let text = result.summary_messages[0].text_content();
        assert!(
            text.starts_with(
                "This session is being continued from a previous conversation that ran out of context."
            ),
            "the summary body is still `pbe`'s: {text}"
        );
        assert!(
            !text.contains("Continue the conversation from where it left off"),
            "the resume continuation must NOT be appended on this path: {text}"
        );
        assert!(
            result.messages_to_preserve.is_empty(),
            "the caller owns the kept half — this layer must not also claim one"
        );
    }

    /// An explicit user command must never report a summary no model produced.
    #[tokio::test]
    async fn summarize_selection_without_a_real_client_is_a_hard_error() {
        use crate::prompt::SummarizeDirection;
        let err = Autocompactor::new()
            .summarize_selection(
                vec![user_msg("q"), assistant_text("answer")],
                None,
                SummarizeDirection::From,
            )
            .await
            .expect_err("an unwired summarizer must fail, not stub a summary");
        assert!(
            matches!(err, CompactionError::Internal(ref detail) if detail.contains("no forked summarizer")),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn manual_compact_without_a_real_client_never_returns_fake_success() {
        let err = Autocompactor::new()
            .compact_manual_with_instructions(vec![user_msg("q"), assistant_text("answer")], None)
            .await
            .expect_err("manual compact requires a real summarizer");
        assert!(err.to_string().contains("no forked summarizer wired"));
    }

    /// Manual empty-response failure surfaces `Error during compaction:
    /// summarization produced empty response` (CC `Juy` catch over `pIg`'s
    /// detail) — NOT the auto path's `Failed to generate…` string.
    #[tokio::test]
    async fn manual_compact_rejects_an_empty_model_response() {
        let history = vec![
            user_msg("q1"),
            assistant_text("first reply"),
            user_msg("q2"),
            assistant_text("second reply"),
        ];
        let (compactor, _client) = wired("", history.clone()).await;
        let err = compactor
            .compact_manual_with_instructions(history, None)
            .await
            .expect_err("an empty model response is not a compact summary");
        assert!(err
            .to_string()
            .contains("summarization produced empty response"));
    }

    #[tokio::test]
    async fn compact_passthrough_when_model_returns_no_tags() {
        // A model that ignored the <analysis>/<summary> structure still yields
        // a valid continuation message (formatCompactSummary passthrough).
        let (compactor, _client) = wired("plain summary text", vec![]).await;
        let result = compactor.compact(vec![user_msg("x")]).await.unwrap();
        let text = result.summary_messages[0].text_content();
        assert!(text.contains("plain summary text"));
        assert!(text.contains("Continue the conversation from where it left off"));
    }

    #[tokio::test]
    async fn compact_errors_when_runner_wired_but_slot_empty() {
        let client = Arc::new(MockClient {
            seen: Mutex::new(None),
            canned_text: "<summary>S</summary>".into(),
        });
        let runner = Arc::new(ForkedAgentRunner::new().with_side_query_client(client, "m".into()));
        // Empty slot — never saved.
        let slot = Arc::new(CacheSafeParamsSlot::new());
        let compactor = Autocompactor::with_forked_runner(runner, slot);

        let err = compactor
            .compact(vec![user_msg("x")])
            .await
            .expect_err("empty slot surfaces an error");
        match err {
            CompactionError::Internal(msg) => {
                assert!(
                    msg.contains("no cache-safe params"),
                    "unexpected msg: {msg}"
                );
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn compact_fallback_without_runner_no_stub_marker() {
        // Default construction (no forked runner): deterministic fallback,
        // built through the SAME continuation pipeline, with NO `[stub-summary]`.
        let compactor = Autocompactor::new();
        let result = compactor
            .compact(vec![user_msg("a"), user_msg("b")])
            .await
            .expect("fallback compact succeeds");

        assert_eq!(result.summary_messages.len(), 1);
        let msg = &result.summary_messages[0];
        assert!(
            matches!(msg, ConversationMessage::User { .. }),
            "fallback summary should be a User message"
        );
        let text = msg.text_content();
        assert!(
            !text.contains("[stub-summary"),
            "the old stub marker must not ship: {text}"
        );
        assert!(text.starts_with(
            "This session is being continued from a previous conversation that ran out of context."
        ));
        assert!(text.contains("Continue the conversation from where it left off"));
        // The fallback's <summary> wrapper is consumed by formatCompactSummary.
        assert!(!text.contains("<summary>"));
    }

    #[test]
    fn default_config_uses_byte_faithful_compact_prompt() {
        let cfg = AutocompactConfig::default();
        assert_eq!(
            cfg.compact_user_prompt,
            crate::prompt::get_compact_prompt(None)
        );
        // Sanity: the prior placeholder prompt is gone.
        assert!(!cfg.compact_user_prompt.contains("Output ONLY the summary."));
        assert!(cfg
            .compact_user_prompt
            .contains("Your task is to create a detailed summary of the conversation so far"));
    }

    // --- COMPACT.2: prompt-too-long retry loop --------------------------- //

    #[tokio::test]
    async fn compact_ptl_retry_truncates_and_succeeds() {
        // First summary call returns prompt-too-long; the loop drops the oldest
        // API-round group from the fork context and retries, and the second call
        // returns a real summary (TS `compact.ts:445-491`). Prefix groups:
        // [u,u,u] [aA,result] [aB,result] → 3 groups; the unknown gap drops the
        // 3-message preamble (group 0), so the retried prompt is strictly shorter.
        let id_a = MessageId::new();
        let id_b = MessageId::new();
        let prefix = vec![
            user_msg("preamble line one"),
            user_msg("preamble line two"),
            user_msg("preamble line three"),
            assistant_tool(id_a, "Read"),
            tool_result_msg(),
            assistant_tool(id_b, "Bash"),
            tool_result_msg(),
        ];
        let (compactor, client) = wired_seq(
            vec![
                "Prompt is too long".into(),
                "<summary>RETRIED-OK</summary>".into(),
            ],
            prefix.clone(),
        )
        .await;

        let result = compactor
            .compact(prefix)
            .await
            .expect("PTL retry then success");

        let text = result.summary_messages[0].text_content();
        assert!(
            text.contains("Summary:\nRETRIED-OK"),
            "retried summary not surfaced: {text}"
        );

        let lens = client.seen_lens.lock().unwrap().clone();
        assert_eq!(lens.len(), 2, "exactly one PTL retry (two summary calls)");
        assert!(
            lens[1] < lens[0],
            "the retry must send a SHORTER prompt after head-truncation: {lens:?}"
        );
    }

    #[tokio::test]
    async fn compact_ptl_retry_exhausts_to_max_retries_error() {
        // Every summary call returns prompt-too-long: the loop truncates until
        // there is nothing safe left to drop, then surfaces MaxRetriesExceeded
        // (TS throws ERROR_MESSAGE_PROMPT_TOO_LONG).
        let id_a = MessageId::new();
        let id_b = MessageId::new();
        let prefix = vec![
            user_msg("preamble"),
            assistant_tool(id_a, "Read"),
            tool_result_msg(),
            assistant_tool(id_b, "Bash"),
            tool_result_msg(),
        ];
        let (compactor, client) =
            wired_seq(vec!["Prompt is too long".into(); 6], prefix.clone()).await;

        let err = compactor
            .compact(prefix)
            .await
            .expect_err("PTL exhaustion must error");
        assert!(
            matches!(err, CompactionError::MaxRetriesExceeded),
            "expected MaxRetriesExceeded, got {err:?}"
        );
        // It actually retried (more than the single initial attempt) before
        // giving up — bounded by what truncation can shed.
        assert!(
            client.seen_lens.lock().unwrap().len() >= 2,
            "should attempt at least one truncated retry before exhausting"
        );
    }

    // --- #58: preserved recent-message tail ------------------------------ //

    fn assistant_text(text: &str) -> ConversationMessage {
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: Some("end_turn".into()),
        }
    }

    #[tokio::test]
    async fn manual_compact_rejects_whitespace_summary() {
        let history = vec![
            user_msg("q1"),
            assistant_text("a1"),
            user_msg("q2"),
            assistant_text("a2"),
        ];
        let (compactor, _) = wired("\u{feff} \n\t", history.clone()).await;
        let error = compactor
            .compact_manual_with_instructions(history, None)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "summarization produced empty response");
    }

    #[tokio::test]
    async fn manual_ptl_retry_grows_tail_without_dropping_the_oldest_prefix() {
        let history = vec![
            user_msg("oldest request"),
            assistant_text("a1"),
            user_msg("q2"),
            assistant_text("a2"),
            user_msg("q3"),
            assistant_text("a3"),
        ];
        let (compactor, client) = wired_seq(
            vec![
                "Prompt is too long".into(),
                "  <summary>kept</summary>\n".into(),
            ],
            history.clone(),
        )
        .await;
        let result = compactor
            .compact_manual_with_instructions(history, None)
            .await
            .unwrap();
        let requests = client.seen.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_ne!(
            requests[0].messages.last().unwrap().id(),
            requests[1].messages.last().unwrap().id()
        );
        assert_eq!(requests[1].messages[0].text_content(), "oldest request");
        assert_eq!(requests[1].messages[1].text_content(), "a1");
        assert_eq!(
            result
                .messages_to_preserve
                .iter()
                .map(ConversationMessage::text_content)
                .collect::<Vec<_>>(),
            vec!["a2", "q3", "a3"]
        );
        assert_eq!(result.raw_summary_text, "<summary>kept</summary>");
        assert!(!result.summary_messages[0]
            .text_content()
            .contains("Recent messages are preserved verbatim."));
    }

    #[tokio::test]
    async fn reactive_gap_seeds_tail_before_first_summary_request() {
        let history = vec![
            user_msg("oldest"),
            assistant_text("a1"),
            user_msg("q2"),
            assistant_text("a2"),
            user_msg("q3"),
            assistant_text("a3"),
        ];
        let (compactor, client) = wired("<summary>ok</summary>", history.clone()).await;
        let result = compactor
            .compact_reactive_with_instructions(history, None, Some(u64::MAX))
            .await
            .unwrap();
        assert_eq!(result.messages_to_preserve.len(), 3);
        let request = client.seen.lock().unwrap();
        assert_eq!(request.as_ref().unwrap().messages.len(), 4);
        assert_eq!(
            request.as_ref().unwrap().messages[0].text_content(),
            "oldest"
        );
    }

    #[tokio::test]
    async fn media_recovery_retries_same_prefix_once_with_stripped_images() {
        let image = ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ContentBlock::Image {
                source: lingxi_core::types::ImageSource::Url {
                    url: "https://example.invalid/image.png".into(),
                },
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        let history = vec![
            user_msg("q1"),
            assistant_text("a1"),
            image.clone(),
            assistant_text("a2"),
        ];
        let (compactor, client) = wired_seq(Vec::new(), history.clone()).await;
        *client.texts.lock().unwrap() = VecDeque::from(vec![
            Err(llm_runtime::LlmError::RequestTooLarge),
            Ok("<summary>ok</summary>".into()),
        ]);
        let result = compactor
            .compact_manual_with_instructions(history, None)
            .await
            .unwrap();
        let requests = client.seen.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].messages[2], image);
        assert_eq!(requests[1].messages[2].text_content(), "[image]");
        assert_eq!(requests[0].messages[0], requests[1].messages[0]);
        assert_eq!(result.messages_to_preserve[0].text_content(), "a2");
    }

    #[tokio::test]
    async fn unsupported_media_capability_recovers_for_manual_and_auto_compaction() {
        for capability in ["vision", "documents"] {
            for manual in [false, true] {
                let media = if capability == "vision" {
                    serde_json::json!({"type": "image", "source": {"type": "url", "url": "https://example.invalid/image.png"}})
                } else {
                    serde_json::json!({"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "cGRm"}})
                };
                let mut attachment = user_msg("attachment context");
                if let ConversationMessage::User { content, .. } = &mut attachment {
                    content.push(serde_json::from_value(media).unwrap());
                }
                let history = vec![
                    user_msg("q1"),
                    assistant_text("image evidence already described"),
                    attachment.clone(),
                    assistant_text("a2"),
                ];
                let (compactor, client) = wired_seq(Vec::new(), history.clone()).await;
                *client.texts.lock().unwrap() = VecDeque::from(vec![
                    Err(llm_runtime::LlmError::UnsupportedCapability {
                        capability: capability.into(),
                    }),
                    Ok("<summary>ok</summary>".into()),
                ]);
                let result = if manual {
                    compactor
                        .compact_manual_with_instructions(history.clone(), None)
                        .await
                } else {
                    compactor.compact(history.clone()).await
                }
                .unwrap();
                let requests = client.seen.lock().unwrap();
                assert_eq!(requests.len(), 2);
                assert_eq!(requests[0].messages[2], attachment);
                assert_eq!(requests[1].messages[0], history[0]);
                assert_eq!(requests[1].messages[1], history[1]);
                let expected = if capability == "vision" {
                    "[image]"
                } else {
                    "[document]"
                };
                assert!(requests[1].messages[2].text_content().contains(expected));
                assert!(requests[1].messages[2]
                    .text_content()
                    .contains("attachment context"));
                assert_eq!(
                    result.messages_to_preserve,
                    if manual {
                        vec![history[3].clone()]
                    } else {
                        vec![]
                    }
                );
            }
        }
    }

    #[test]
    fn unrelated_unsupported_capabilities_do_not_trigger_media_recovery() {
        for capability in ["tools", "reasoning", "structured_output"] {
            assert!(!is_media_compaction_error(
                &llm_runtime::LlmError::UnsupportedCapability {
                    capability: capability.into(),
                }
            ));
        }
    }

    #[tokio::test]
    async fn repeated_media_failure_is_terminal_without_a_third_request() {
        let history = vec![
            user_msg("q1"),
            assistant_text("a1"),
            user_msg("q2"),
            assistant_text("a2"),
        ];
        let (compactor, client) = wired_seq(Vec::new(), history.clone()).await;
        *client.texts.lock().unwrap() =
            VecDeque::from(vec![Err(llm_runtime::LlmError::RequestTooLarge); 2]);
        let error = compactor
            .compact_manual_with_instructions(history, None)
            .await
            .unwrap_err();
        assert!(matches!(error, CompactionError::MediaUnstrippable));
        assert_eq!(client.seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn typed_overflow_uses_the_same_preserved_tail_ladder() {
        let history = vec![
            user_msg("oldest"),
            assistant_text("a1"),
            user_msg("q2"),
            assistant_text("a2"),
            user_msg("q3"),
            assistant_text("a3"),
        ];
        let (compactor, client) = wired_seq(Vec::new(), history.clone()).await;
        *client.texts.lock().unwrap() = VecDeque::from(vec![
            Err(llm_runtime::LlmError::ContextOverflow { token_gap: 1 }),
            Ok("<summary>ok</summary>".into()),
        ]);
        let result = compactor
            .compact_manual_with_instructions(history, None)
            .await
            .unwrap();
        assert_eq!(result.messages_to_preserve.len(), 3);
        assert_eq!(
            client.seen.lock().unwrap()[1].messages[0].text_content(),
            "oldest"
        );
    }

    #[tokio::test]
    async fn full_auto_compact_summarizes_all_rounds() {
        let history = vec![
            user_msg("q1"),
            assistant_text("a1"),
            user_msg("q2"),
            assistant_text("a2"),
        ];
        let (compactor, client) = wired("<summary>all</summary>", history.clone()).await;
        let result = compactor.compact(history.clone()).await.unwrap();
        let request = client.seen.lock().unwrap();
        assert_eq!(request.as_ref().unwrap().messages[..history.len()], history);
        assert!(result.messages_to_preserve.is_empty());
    }

    #[tokio::test]
    async fn compact_preserves_recent_tail_for_long_conversation() {
        // A multi-group conversation: [u(q1), aA, u(q2), aB] → three API-round
        // groups (boundary before each new assistant id). `DRn`/select_preserved_tail
        // preserves the last group (the final assistant reply) and summarizes the
        // prefix. The forked summarizer only sees the prefix; the tail is carried
        // out verbatim in `messages_to_preserve`.
        let prefix = vec![
            user_msg("q1"),
            assistant_text("first reply"),
            user_msg("q2"),
            assistant_text("second reply"),
        ];
        let (compactor, _client) = wired("<summary>S</summary>", prefix.clone()).await;

        let result = compactor
            .compact_manual_with_instructions(prefix, None)
            .await
            .expect("wired compact");

        // The preserved tail is non-empty (the last API round).
        assert!(
            !result.messages_to_preserve.is_empty(),
            "a long conversation must preserve a recent tail"
        );
        // The summary (leading) is a single User message; the tail follows.
        assert_eq!(result.summary_messages.len(), 1);
        // The continuation message carries the preserved-tail sentence.
        let summary_text = result.summary_messages[0].text_content();
        assert!(
            !summary_text.contains("Recent messages are preserved verbatim."),
            "261 preserves the tail without adding the legacy sentence: {summary_text}"
        );
        // The preserved tail is the final assistant reply, carried verbatim.
        let tail_texts: Vec<String> = result
            .messages_to_preserve
            .iter()
            .map(ConversationMessage::text_content)
            .collect();
        assert!(
            tail_texts.iter().any(|t| t == "second reply"),
            "the most recent assistant reply must be preserved verbatim: {tail_texts:?}"
        );
    }

    #[tokio::test]
    async fn compact_summarizer_sees_only_the_prefix_when_tail_preserved() {
        // The forked summarizer must replay only the summarize PREFIX (TS
        // `_kd(A,...)`), with the preserved tail dropped from `fork_context_messages`.
        let prefix = vec![
            user_msg("q1"),
            assistant_text("first reply"),
            user_msg("q2"),
            assistant_text("second reply"),
        ];
        let (compactor, client) = wired("<summary>S</summary>", prefix.clone()).await;

        let result = compactor
            .compact_manual_with_instructions(prefix.clone(), None)
            .await
            .expect("compact");
        let keep = result.messages_to_preserve.len();
        assert!(keep > 0, "precondition: a tail was preserved");

        let sent = client.seen.lock().unwrap().clone().expect("client called");
        // The forked request replays (prefix − preserved_tail) then the compact
        // prompt. So the replayed-context count is `4 − keep`, plus the prompt.
        let replayed = sent.messages.len() - 1; // last is the compact prompt
        assert_eq!(
            replayed,
            prefix.len() - keep,
            "summarizer must see only the prefix (tail dropped): replayed={replayed}, keep={keep}"
        );
        // The compact prompt is still last.
        assert_eq!(
            sent.messages.last().unwrap().text_content(),
            crate::prompt::get_compact_prompt(None)
        );
    }

    #[tokio::test]
    async fn compact_short_conversation_has_no_preserved_tail() {
        // A short conversation (fewer than two API-round groups) cannot be
        // split: `messages_to_preserve` is empty and the summary carries NO
        // preserved-tail sentence — byte-identical to before #58.
        let prefix = vec![user_msg("only one user message")];
        let (compactor, _client) = wired("<summary>S</summary>", prefix.clone()).await;

        let result = compactor.compact(prefix).await.expect("compact");
        assert!(
            result.messages_to_preserve.is_empty(),
            "short conversation must NOT preserve a tail"
        );
        let summary_text = result.summary_messages[0].text_content();
        assert!(
            !summary_text.contains("Recent messages are preserved verbatim."),
            "no preserved-tail sentence on the full-replacement path: {summary_text}"
        );
    }

    #[tokio::test]
    async fn compact_no_assistant_prefix_has_no_preserved_tail() {
        // [u, u, aA]: only ONE assistant. select_preserved_tail's i=1 split
        // preserves the last group [aA] but then the summarize prefix [u,u] has
        // NO assistant → no valid split → full replacement, empty tail.
        let prefix = vec![user_msg("a"), user_msg("b"), assistant_text("only reply")];
        let (compactor, _client) = wired("<summary>S</summary>", prefix.clone()).await;

        let result = compactor.compact(prefix).await.expect("compact");
        assert!(
            result.messages_to_preserve.is_empty(),
            "a prefix with no assistant message cannot partial-compact"
        );
    }
}
