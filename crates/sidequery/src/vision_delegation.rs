//! Bounded batch-image analysis for non-vision primary models.

use crate::purposes::QuerySource;
use crate::side_query::{SideQueryClient, SideQueryError, SideQueryRequest, SideQueryResponse};
use base64::Engine as _;
use hooks::SsrfGuard;
use lingxi_core::host::{HttpError, HttpTransport};
use lingxi_core::types::{
    is_nested_media_value, ContentBlock, ConversationMessage, DocumentSource, HttpMethod,
    HttpRequest, ImageSource, MediaAnalysis, MediaObservation, MessageId, MessageRole,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::BuildHasher;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::task::JoinSet;
use url::Url;

/// Version of the delegate prompt and persisted analysis schema.
pub const PROMPT_VERSION: u32 = 1;
/// Maximum retained media items in one provider-facing history snapshot.
pub const MAX_MEDIA_PER_REQUEST: usize = 100;
/// Maximum images sent in one delegate API call.
pub const MAX_MEDIA_PER_QUERY: usize = 20;
/// Maximum decoded base64 bytes sent in one delegate API call.
pub const MAX_DECODED_BYTES_PER_QUERY: usize = 24 * 1024 * 1024;

const CURRENT_TEXT_LIMIT: usize = 8 * 1024;
const PRIOR_TEXT_LIMIT: usize = 8 * 1024;
const TOOL_TEXT_LIMIT: usize = 2 * 1024;
const REMOTE_FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_REMOTE_REDIRECTS: usize = 5;

/// One collected image plus its stable identity and bounded tool context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisionMedia {
    /// SHA-256 identity of decoded bytes or the prefixed URL.
    pub fingerprint: String,
    /// Stable position label within the collected packet.
    pub label: String,
    /// Provider-ready image block.
    pub block: ContentBlock,
    /// Decoded byte count used by the batching limit.
    pub decoded_bytes_len: usize,
    /// Producing tool name for tool-result images.
    pub tool_name: Option<String>,
    /// Bounded model-visible tool result summary.
    pub tool_summary: Option<String>,
}

/// Media item accepted by the delegation batcher.
pub type DelegationMedia = VisionMedia;

/// The retained media set and the matching non-vision history rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDelegation {
    /// Retained images in provider order.
    pub media: Vec<DelegationMedia>,
    /// Matching history with images replaced by references.
    pub rewritten_messages: Vec<ConversationMessage>,
}

/// One bounded vision side-query request assembled by a host.
#[derive(Debug, Clone)]
pub struct VisionPacket {
    /// Stable user-question identity.
    pub question_key: String,
    /// Delegate model id.
    pub model: String,
    /// Same-provider profile used for routing and credentials.
    pub profile: Option<String>,
    /// Current real user text.
    pub current_user_text: String,
    /// Up to two preceding non-meta text messages.
    pub prior_text_context: Vec<(MessageRole, String)>,
    /// Messages containing only the images that require analysis.
    pub media_messages: Vec<ConversationMessage>,
}

/// Aggregated delegate result and accounting metadata.
#[derive(Debug, Clone)]
pub struct VisionDelegationResult {
    /// Persistable structured analysis.
    pub analysis: MediaAnalysis,
    /// Aggregate token usage across all batches.
    pub usage: cost::Usage,
    /// Wall-clock duration across the delegated operation.
    pub elapsed: Duration,
    /// Provider retry count observed by the delegation client.
    pub retry_count: u32,
    /// Actual number of delegate API batches.
    pub api_calls: u32,
}

/// Executes bounded image-analysis batches with at most two concurrent calls.
pub struct VisionDelegationService {
    client: Arc<dyn SideQueryClient>,
    http: Arc<dyn HttpTransport>,
    ssrf_guard: SsrfGuard,
}

impl VisionDelegationService {
    /// Construct a service over the shared provider side-query client.
    #[must_use]
    pub fn new(client: Arc<dyn SideQueryClient>) -> Self {
        Self {
            client,
            http: Arc::new(platform_common::ReqwestHttp::new()),
            ssrf_guard: SsrfGuard::with_defaults(),
        }
    }

    #[cfg(test)]
    fn new_with_http(
        client: Arc<dyn SideQueryClient>,
        http: Arc<dyn HttpTransport>,
        ssrf_guard: SsrfGuard,
    ) -> Self {
        Self {
            client,
            http,
            ssrf_guard,
        }
    }

    /// Analyze all retained images in `packet` and merge results in input order.
    #[expect(
        clippy::too_many_lines,
        reason = "the bounded two-slot scheduler is kept together so cancellation and accounting stay atomic"
    )]
    pub async fn analyze(
        &self,
        packet: VisionPacket,
    ) -> Result<VisionDelegationResult, SideQueryError> {
        let started = Instant::now();
        let mut pending_batches =
            VecDeque::from(prepare_media_for_nonvision(&packet.media_messages, true)?.media);
        if pending_batches.is_empty() {
            return Ok(VisionDelegationResult {
                analysis: MediaAnalysis {
                    question_key: packet.question_key,
                    media_fingerprints: Vec::new(),
                    model: packet.model,
                    prompt_version: PROMPT_VERSION,
                    created_at: SystemTime::now(),
                    task_findings: Vec::new(),
                    media: Vec::new(),
                    cross_media_findings: Vec::new(),
                    truncated: false,
                },
                usage: cost::Usage::default(),
                elapsed: started.elapsed(),
                retry_count: 0,
                api_calls: 0,
            });
        }

        let mut join_set: JoinSet<(
            usize,
            Vec<DelegationMedia>,
            Result<SideQueryResponse, SideQueryError>,
        )> = JoinSet::new();
        let mut next_batch_index = 0usize;
        let mut ordered: Vec<(usize, MediaAnalysis)> = Vec::new();
        let mut first_error: Option<SideQueryError> = None;
        let mut api_calls = 0u32;
        let mut usage = cost::Usage::default();
        let mut retry_count = 0u32;

        for _ in 0..2 {
            match self.materialize_next_batch(&mut pending_batches).await {
                Ok(Some(batch)) => {
                    spawn_query(
                        &mut join_set,
                        Arc::clone(&self.client),
                        packet.clone(),
                        next_batch_index,
                        batch,
                    );
                    next_batch_index += 1;
                    api_calls = api_calls.saturating_add(1);
                }
                Ok(None) => break,
                Err(error) => {
                    first_error = Some(error);
                    join_set.abort_all();
                    break;
                }
            }
        }

        while let Some(joined) = join_set.join_next().await {
            match joined {
                Ok((index, batch, Ok(response))) => {
                    usage.add(&response.usage);
                    retry_count = retry_count.saturating_add(response.retry_count);
                    match parse_response(&packet.question_key, &packet.model, &batch, response) {
                        Ok(parsed) => ordered.push((index, parsed)),
                        Err(error) => {
                            if first_error.is_none() {
                                first_error = Some(error);
                                join_set.abort_all();
                            }
                        }
                    }
                }
                Ok((_index, _batch, Err(error))) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                        join_set.abort_all();
                    }
                }
                Err(error) => {
                    if error.is_cancelled() {
                        continue;
                    }
                    if first_error.is_none() {
                        first_error = Some(SideQueryError::Api(llm_runtime::LlmError::Transport {
                            message: format!("vision delegation task failed: {error}"),
                        }));
                        join_set.abort_all();
                    }
                }
            }
            if first_error.is_none() {
                match self.materialize_next_batch(&mut pending_batches).await {
                    Ok(Some(next_batch)) => {
                        api_calls = api_calls.saturating_add(1);
                        spawn_query(
                            &mut join_set,
                            Arc::clone(&self.client),
                            packet.clone(),
                            next_batch_index,
                            next_batch,
                        );
                        next_batch_index += 1;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(error);
                            join_set.abort_all();
                        }
                    }
                }
            }
        }
        ordered.sort_by_key(|(index, _)| *index);
        if let Some(error) = first_error {
            return Err(SideQueryError::Partial {
                source: Box::new(error),
                usage,
                elapsed: started.elapsed(),
                retry_count,
                api_calls,
            });
        }

        let mut task_findings = Vec::new();
        let mut media = Vec::new();
        let mut cross_media_findings = Vec::new();
        let mut truncated = false;
        let mut media_fingerprints = Vec::new();

        for (_, parsed) in ordered {
            media_fingerprints.extend(parsed.media_fingerprints);
            task_findings.extend(parsed.task_findings);
            media.extend(parsed.media);
            cross_media_findings.extend(parsed.cross_media_findings);
            truncated |= parsed.truncated;
        }

        Ok(VisionDelegationResult {
            analysis: MediaAnalysis {
                question_key: packet.question_key,
                media_fingerprints,
                model: packet.model,
                prompt_version: PROMPT_VERSION,
                created_at: SystemTime::now(),
                task_findings,
                media,
                cross_media_findings,
                truncated,
            },
            usage,
            elapsed: started.elapsed(),
            retry_count,
            api_calls,
        })
    }

    async fn materialize_next_batch(
        &self,
        pending: &mut VecDeque<DelegationMedia>,
    ) -> Result<Option<Vec<DelegationMedia>>, SideQueryError> {
        if pending.is_empty() {
            return Ok(None);
        }
        let mut current = Vec::new();
        let mut current_bytes = 0usize;
        while let Some(mut item) = pending.pop_front() {
            if let ContentBlock::Image {
                source: ImageSource::Url { url },
            } = &item.block
            {
                let fetched_image = self.fetch_remote_image(url).await?;
                item.decoded_bytes_len = fetched_image.decoded_bytes_len;
                item.block = ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: fetched_image.media_type,
                        data: fetched_image.data,
                    },
                };
            }
            if item.decoded_bytes_len > MAX_DECODED_BYTES_PER_QUERY {
                return Err(SideQueryError::Api(
                    llm_runtime::LlmError::MediaDelegationUnavailable {
                        message: format!(
                            "Image '{}' exceeds the 24 MiB delegation limit.",
                            item.label
                        ),
                    },
                ));
            }
            let would_overflow = current.len() >= MAX_MEDIA_PER_QUERY
                || current_bytes.saturating_add(item.decoded_bytes_len)
                    > MAX_DECODED_BYTES_PER_QUERY;
            if would_overflow && !current.is_empty() {
                pending.push_front(item);
                return Ok(Some(current));
            }
            current_bytes = current_bytes.saturating_add(item.decoded_bytes_len);
            current.push(item);
        }
        Ok((!current.is_empty()).then_some(current))
    }

    async fn fetch_remote_image(&self, url: &str) -> Result<FetchedImage, SideQueryError> {
        let mut current = Url::parse(url).map_err(|_| delegate_url_error())?;
        for redirect_index in 0..=MAX_REMOTE_REDIRECTS {
            let resolved = self
                .ssrf_guard
                .resolve_url(current.as_ref())
                .await
                .map_err(|error| map_ssrf_error(&error))?;
            validate_public_target(&current, resolved.as_ref())?;
            let response = self
                .http
                .stream_raw_bytes_with_meta_no_follow_with_resolved_addrs(
                    HttpRequest {
                        method: HttpMethod::Get,
                        url: current.to_string(),
                        headers: vec![("accept".into(), "image/*".into())],
                        body: None,
                        body_bytes: None,
                        timeout: Some(REMOTE_FETCH_TIMEOUT),
                    },
                    resolved,
                )
                .await
                .map_err(|error| map_http_error(&error))?;
            if (300..400).contains(&response.status) {
                if redirect_index == MAX_REMOTE_REDIRECTS {
                    return Err(remote_fetch_error(
                        "image URL exceeded the redirect limit for vision delegation",
                    ));
                }
                let location = response
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("location"))
                    .map(|(_, value)| value.as_str())
                    .ok_or_else(|| {
                        remote_fetch_error(
                            "image URL redirect response was missing a Location header",
                        )
                    })?;
                current = current.join(location).map_err(|_| delegate_url_error())?;
                continue;
            }
            if !(200..300).contains(&response.status) {
                return Err(remote_fetch_error(&format!(
                    "image URL fetch returned HTTP {} during vision delegation",
                    response.status
                )));
            }
            let media_type = supported_image_media_type(&response.headers)?;
            if let Some(length) = header_u64(&response.headers, "content-length") {
                if usize::try_from(length).unwrap_or(usize::MAX) > MAX_DECODED_BYTES_PER_QUERY {
                    return Err(remote_fetch_error(
                        "image URL exceeds the 24 MiB delegation limit",
                    ));
                }
            }
            let mut bytes = Vec::new();
            let mut stream = response.stream;
            while let Some(chunk) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
                let chunk = chunk.map_err(|error| map_http_error(&error))?;
                if bytes.len().saturating_add(chunk.len()) > MAX_DECODED_BYTES_PER_QUERY {
                    return Err(remote_fetch_error(
                        "image URL exceeds the 24 MiB delegation limit",
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            return Ok(FetchedImage {
                media_type,
                data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                decoded_bytes_len: bytes.len(),
            });
        }
        Err(remote_fetch_error(
            "image URL exceeded the redirect limit for vision delegation",
        ))
    }
}

#[derive(Debug)]
struct FetchedImage {
    media_type: String,
    data: String,
    decoded_bytes_len: usize,
}

/// Collect ordered fingerprints from the same newest-100 set used by rewrite.
pub fn collect_media_fingerprints(
    messages: &[ConversationMessage],
) -> Result<Vec<String>, SideQueryError> {
    collect_media(messages).map(|media| media.into_iter().map(|item| item.fingerprint).collect())
}

/// Collect provider-ready images from top-level and tool-result blocks.
pub fn collect_media(
    messages: &[ConversationMessage],
) -> Result<Vec<DelegationMedia>, SideQueryError> {
    let pruned = prune_to_latest_media(messages.to_vec(), MAX_MEDIA_PER_REQUEST);
    collect_media_from_messages(&pruned, false)
}

/// Collect and rewrite the exact same newest-100 media set.
pub fn prepare_delegation(
    messages: &[ConversationMessage],
) -> Result<PreparedDelegation, SideQueryError> {
    let pruned = prune_to_latest_media(messages.to_vec(), MAX_MEDIA_PER_REQUEST);
    Ok(PreparedDelegation {
        media: collect_media_from_messages(&pruned, false)?,
        rewritten_messages: rewrite_media_for_nonvision_internal(&pruned, false)?,
    })
}

/// Prepare a non-vision main-model snapshot while optionally preserving native
/// document blocks. Images are always collected and rewritten; unsupported
/// documents fail before any provider call.
pub fn prepare_media_for_nonvision(
    messages: &[ConversationMessage],
    documents_supported: bool,
) -> Result<PreparedDelegation, SideQueryError> {
    let pruned = prune_to_latest_media(messages.to_vec(), MAX_MEDIA_PER_REQUEST);
    Ok(PreparedDelegation {
        media: collect_media_from_messages(&pruned, documents_supported)?,
        rewritten_messages: rewrite_media_for_nonvision_internal(&pruned, documents_supported)?,
    })
}

/// Keep only images whose fingerprints are in `wanted`, preserving surrounding
/// message and tool-result context for the delegate prompt.
pub fn filter_messages_to_fingerprints<S: BuildHasher>(
    messages: &[ConversationMessage],
    wanted: &HashSet<String, S>,
) -> Result<Vec<ConversationMessage>, SideQueryError> {
    messages
        .iter()
        .cloned()
        .map(|message| match message {
            ConversationMessage::User {
                id,
                content,
                is_meta,
                is_compact_summary,
                is_visible_in_transcript_only,
            } => Ok(ConversationMessage::User {
                id,
                content: filter_blocks_to_fingerprints(content, wanted)?,
                is_meta,
                is_compact_summary,
                is_visible_in_transcript_only,
            }),
            ConversationMessage::Assistant {
                id,
                content,
                stop_reason,
            } => Ok(ConversationMessage::Assistant {
                id,
                content: filter_blocks_to_fingerprints(content, wanted)?,
                stop_reason,
            }),
            ConversationMessage::System { .. } => Ok(message),
        })
        .collect()
}

fn filter_blocks_to_fingerprints<S: BuildHasher>(
    blocks: Vec<ContentBlock>,
    wanted: &HashSet<String, S>,
) -> Result<Vec<ContentBlock>, SideQueryError> {
    let mut out = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            ContentBlock::Image { source } => {
                if wanted.contains(&fingerprint_for_source(&source)?) {
                    out.push(ContentBlock::Image { source });
                }
            }
            ContentBlock::Document { .. } => {}
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                provider_tool_use_id,
                content_blocks: Some(content_blocks),
            } => {
                let filtered = content_blocks
                    .into_iter()
                    .filter_map(|value| match extract_image_source(&value) {
                        Some(source) => match fingerprint_for_source(&source) {
                            Ok(fingerprint) if wanted.contains(&fingerprint) => Some(Ok(value)),
                            Ok(_) => None,
                            Err(error) => Some(Err(error)),
                        },
                        None if is_document_value(&value) => None,
                        None => Some(Ok(value)),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                out.push(ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    provider_tool_use_id,
                    content_blocks: Some(filtered),
                });
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

/// Return fingerprints covered by analyses for one question/model/prompt version.
#[must_use]
pub fn covered_fingerprints(
    analyses: &[MediaAnalysis],
    question_key: &str,
    model: &str,
) -> HashSet<String> {
    analyses
        .iter()
        .filter(|analysis| {
            analysis.question_key == question_key
                && analysis.model == model
                && analysis.prompt_version == PROMPT_VERSION
        })
        .flat_map(|analysis| analysis.media_fingerprints.iter().cloned())
        .collect()
}

/// Rewrite retained images into textual references for a non-vision model.
pub fn rewrite_media_for_nonvision(
    messages: &[ConversationMessage],
) -> Result<Vec<ConversationMessage>, SideQueryError> {
    let pruned = prune_to_latest_media(messages.to_vec(), MAX_MEDIA_PER_REQUEST);
    rewrite_media_for_nonvision_internal(&pruned, false)
}

fn collect_media_from_messages(
    messages: &[ConversationMessage],
    documents_supported: bool,
) -> Result<Vec<DelegationMedia>, SideQueryError> {
    let mut tool_names = HashMap::<String, String>::new();
    let mut out = Vec::new();
    for message in messages {
        let content = match message {
            ConversationMessage::User { content, .. }
            | ConversationMessage::Assistant { content, .. } => content,
            ConversationMessage::System { .. } => continue,
        };
        for block in content {
            match block {
                ContentBlock::ToolUse { id, name, .. } => {
                    tool_names.insert(id.to_string(), name.clone());
                }
                ContentBlock::Image { source } => {
                    out.push(DelegationMedia {
                        fingerprint: fingerprint_for_source(source)?,
                        label: format!("image_{}", out.len() + 1),
                        block: ContentBlock::Image {
                            source: source.clone(),
                        },
                        decoded_bytes_len: decoded_len(source)?,
                        tool_name: None,
                        tool_summary: None,
                    });
                }
                ContentBlock::Document { source } => {
                    if !documents_supported {
                        return Err(document_unsupported(source));
                    }
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    content_blocks: Some(blocks),
                    ..
                } => {
                    let tool_name = tool_names.get(&tool_use_id.to_string()).cloned();
                    let summary = Some(tool_result_summary(content, blocks));
                    for value in blocks {
                        if let Some(source) = extract_image_source(value) {
                            out.push(DelegationMedia {
                                fingerprint: fingerprint_for_source(&source)?,
                                label: format!("image_{}", out.len() + 1),
                                decoded_bytes_len: decoded_len(&source)?,
                                block: ContentBlock::Image { source },
                                tool_name: tool_name.clone(),
                                tool_summary: summary.clone(),
                            });
                        } else if is_document_value(value) && !documents_supported {
                            return Err(document_value_unsupported());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

fn prune_to_latest_media(
    mut messages: Vec<ConversationMessage>,
    limit: usize,
) -> Vec<ConversationMessage> {
    let total = count_media(&messages);
    if total <= limit {
        return messages;
    }
    let mut to_remove = total - limit;
    for message in &mut messages {
        if to_remove == 0 {
            break;
        }
        let content = match message {
            ConversationMessage::User { content, .. }
            | ConversationMessage::Assistant { content, .. } => content,
            ConversationMessage::System { .. } => continue,
        };
        for block in content.iter_mut() {
            if to_remove == 0 {
                break;
            }
            if let ContentBlock::ToolResult {
                content_blocks: Some(blocks),
                ..
            } = block
            {
                blocks.retain(|value| {
                    if to_remove > 0 && is_nested_media_value(value) {
                        to_remove -= 1;
                        false
                    } else {
                        true
                    }
                });
            }
        }
        content.retain(|block| {
            if to_remove > 0
                && matches!(
                    block,
                    ContentBlock::Image { .. } | ContentBlock::Document { .. }
                )
            {
                to_remove -= 1;
                false
            } else {
                true
            }
        });
    }
    messages
}

fn count_media(messages: &[ConversationMessage]) -> usize {
    messages
        .iter()
        .map(|message| match message {
            ConversationMessage::User { content, .. }
            | ConversationMessage::Assistant { content, .. } => content
                .iter()
                .map(|block| match block {
                    ContentBlock::Image { .. } | ContentBlock::Document { .. } => 1,
                    ContentBlock::ToolResult {
                        content_blocks: Some(blocks),
                        ..
                    } => blocks
                        .iter()
                        .filter(|value| is_nested_media_value(value))
                        .count(),
                    _ => 0,
                })
                .sum::<usize>(),
            ConversationMessage::System { .. } => 0,
        })
        .sum()
}

fn spawn_query(
    join_set: &mut JoinSet<(
        usize,
        Vec<DelegationMedia>,
        Result<SideQueryResponse, SideQueryError>,
    )>,
    client: Arc<dyn SideQueryClient>,
    packet: VisionPacket,
    index: usize,
    batch: Vec<DelegationMedia>,
) {
    join_set.spawn(async move {
        let response = client
            .query(SideQueryRequest {
                model_attempt: None,
                model: packet.model.clone(),
                profile: packet.profile.clone(),
                system_prompt: Some(system_prompt().to_string()),
                messages: build_messages(&packet, &batch),
                tools: Vec::new(),
                tool_choice: None,
                output_format: Some(serde_json::json!({"type":"json_object"})),
                max_tokens: 8192,
                max_retries: 0,
                temperature: Some(0.0),
                thinking: None,
                effort: None,
                stop_sequences: Vec::new(),
                query_source: QuerySource::VisionDelegation,
                skip_system_prompt_prefix: true,
            })
            .await;
        (index, batch, response)
    });
}

fn rewrite_media_for_nonvision_internal(
    messages: &[ConversationMessage],
    documents_supported: bool,
) -> Result<Vec<ConversationMessage>, SideQueryError> {
    messages
        .iter()
        .cloned()
        .map(|message| rewrite_message_for_nonvision(message, documents_supported))
        .collect()
}

fn rewrite_message_for_nonvision(
    message: ConversationMessage,
    documents_supported: bool,
) -> Result<ConversationMessage, SideQueryError> {
    match message {
        ConversationMessage::User {
            id,
            content,
            is_meta,
            is_compact_summary,
            is_visible_in_transcript_only,
        } => Ok(ConversationMessage::User {
            id,
            content: rewrite_blocks_for_nonvision(content, documents_supported)?,
            is_meta,
            is_compact_summary,
            is_visible_in_transcript_only,
        }),
        ConversationMessage::Assistant {
            id,
            content,
            stop_reason,
        } => Ok(ConversationMessage::Assistant {
            id,
            content: rewrite_blocks_for_nonvision(content, documents_supported)?,
            stop_reason,
        }),
        ConversationMessage::System { .. } => Ok(message),
    }
}

fn rewrite_blocks_for_nonvision(
    blocks: Vec<ContentBlock>,
    documents_supported: bool,
) -> Result<Vec<ContentBlock>, SideQueryError> {
    let mut rewritten = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            ContentBlock::Image { source } => {
                rewritten.push(ContentBlock::Text {
                    text: placeholder(&fingerprint_for_source(&source)?),
                });
            }
            ContentBlock::Document { source } if !documents_supported => {
                return Err(document_unsupported(&source));
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                provider_tool_use_id,
                content_blocks: Some(blocks),
            } => {
                rewritten.push(ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    provider_tool_use_id,
                    content_blocks: Some(
                        blocks
                            .into_iter()
                            .map(|value| rewrite_tool_result_media(value, documents_supported))
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                });
            }
            other => rewritten.push(other),
        }
    }
    Ok(rewritten)
}

fn rewrite_tool_result_media(
    value: Value,
    documents_supported: bool,
) -> Result<Value, SideQueryError> {
    if let Some(source) = extract_image_source(&value) {
        return Ok(serde_json::json!({
            "type": "text",
            "text": placeholder(&fingerprint_for_source(&source)?),
        }));
    }
    if is_document_value(&value) && !documents_supported {
        return Err(document_value_unsupported());
    }
    Ok(value)
}

#[cfg(test)]
fn batch_media(media: Vec<DelegationMedia>) -> Result<Vec<Vec<DelegationMedia>>, SideQueryError> {
    let mut batches = Vec::new();
    let mut current = Vec::new();
    let mut bytes = 0usize;
    for item in media {
        if item.decoded_bytes_len > MAX_DECODED_BYTES_PER_QUERY {
            return Err(SideQueryError::Api(
                llm_runtime::LlmError::MediaDelegationUnavailable {
                    message: format!(
                        "Image '{}' exceeds the 24 MiB delegation limit.",
                        item.label
                    ),
                },
            ));
        }
        let would_overflow = current.len() >= MAX_MEDIA_PER_QUERY
            || bytes.saturating_add(item.decoded_bytes_len) > MAX_DECODED_BYTES_PER_QUERY;
        if would_overflow && !current.is_empty() {
            batches.push(current);
            current = Vec::new();
            bytes = 0;
        }
        bytes = bytes.saturating_add(item.decoded_bytes_len);
        current.push(item);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    Ok(batches)
}

fn build_messages(packet: &VisionPacket, batch: &[DelegationMedia]) -> Vec<ConversationMessage> {
    let mut messages = Vec::new();
    let prior = packet
        .prior_text_context
        .iter()
        .filter(|(_, text)| !text.is_empty())
        .collect::<Vec<_>>();
    let mut remaining_prior_bytes = PRIOR_TEXT_LIMIT;
    for (index, (role, text)) in prior.iter().enumerate() {
        if text.is_empty() {
            continue;
        }
        let remaining_messages = prior.len().saturating_sub(index).max(1);
        let budget = remaining_prior_bytes / remaining_messages;
        let text = truncate_head_tail(text, budget);
        remaining_prior_bytes = remaining_prior_bytes.saturating_sub(text.len());
        messages.push(match role {
            MessageRole::User => ConversationMessage::user(MessageId::new(), text),
            MessageRole::Assistant => ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![ContentBlock::Text { text }],
                stop_reason: None,
            },
            MessageRole::System => continue,
        });
    }

    let mut content = vec![ContentBlock::Text {
        text: format!(
            "Current user request:\n{}\n\nAnalyze the attached images and answer in JSON only.",
            truncate_head_tail(&packet.current_user_text, CURRENT_TEXT_LIMIT)
        ),
    }];
    for item in batch {
        let mut label = format!("{} ({})", item.label, item.fingerprint);
        if let Some(tool_name) = &item.tool_name {
            label.push_str(&format!("\nTool: {tool_name}"));
        }
        if let Some(summary) = &item.tool_summary {
            if !summary.is_empty() {
                label.push_str(&format!("\nTool summary: {summary}"));
            }
        }
        content.push(ContentBlock::Text { text: label });
        content.push(item.block.clone());
    }
    messages.push(ConversationMessage::User {
        id: MessageId::new(),
        content,
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    });
    messages
}

fn parse_response(
    question_key: &str,
    model: &str,
    batch: &[DelegationMedia],
    response: SideQueryResponse,
) -> Result<MediaAnalysis, SideQueryError> {
    let stopped_at_limit = matches!(
        response.stop_reason.as_deref(),
        Some("max_tokens" | "max_output_tokens" | "length")
    );
    let payload = response
        .structured
        .and_then(|value| serde_json::from_value::<DelegatePayload>(value).ok())
        .or_else(|| {
            response.text.as_ref().and_then(|text| {
                let text = text.trim();
                (!text.is_empty()).then(|| DelegatePayload {
                    task_findings: vec![text.to_string()],
                    media: Vec::new(),
                    cross_media_findings: Vec::new(),
                    truncated: false,
                })
            })
        })
        .ok_or_else(|| {
            SideQueryError::Api(llm_runtime::LlmError::MediaDelegationUnavailable {
                message: "Vision delegate returned an empty response.".to_string(),
            })
        })?;

    let defaults = batch
        .iter()
        .map(|item| MediaObservation {
            fingerprint: item.fingerprint.clone(),
            label: item.label.clone(),
            description: String::new(),
            ocr: None,
            relevant_facts: Vec::new(),
            uncertainty: None,
        })
        .collect::<Vec<_>>();
    let by_fingerprint = payload
        .media
        .into_iter()
        .map(|item| (item.fingerprint.clone(), item))
        .collect::<HashMap<_, _>>();

    Ok(MediaAnalysis {
        question_key: question_key.to_string(),
        media_fingerprints: batch.iter().map(|item| item.fingerprint.clone()).collect(),
        model: model.to_string(),
        prompt_version: PROMPT_VERSION,
        created_at: SystemTime::now(),
        task_findings: payload.task_findings,
        media: defaults
            .into_iter()
            .map(|default| match by_fingerprint.get(&default.fingerprint) {
                Some(found) => MediaObservation {
                    fingerprint: default.fingerprint,
                    label: if found.label.is_empty() {
                        default.label
                    } else {
                        found.label.clone()
                    },
                    description: found.description.clone(),
                    ocr: found.ocr.clone(),
                    relevant_facts: found.relevant_facts.clone(),
                    uncertainty: found.uncertainty.clone(),
                },
                None => default,
            })
            .collect(),
        cross_media_findings: payload.cross_media_findings,
        truncated: payload.truncated || stopped_at_limit,
    })
}

fn extract_image_source(value: &Value) -> Option<ImageSource> {
    let kind = value.get("type")?.as_str()?;
    match kind {
        "image" => value
            .get("source")
            .and_then(|source| serde_json::from_value::<ImageSource>(source.clone()).ok()),
        "image_url" => value
            .get("url")
            .and_then(Value::as_str)
            .or_else(|| {
                value
                    .get("image_url")
                    .and_then(|payload| payload.get("url"))
                    .and_then(Value::as_str)
            })
            .map(|url| ImageSource::Url {
                url: url.to_string(),
            }),
        _ => None,
    }
}

fn tool_result_summary(content: &str, blocks: &[Value]) -> String {
    let structured = blocks
        .iter()
        .filter_map(|value| {
            (value.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| value.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect::<Vec<_>>()
        .join("\n");
    truncate_head_tail(
        if structured.is_empty() {
            content
        } else {
            structured.as_str()
        },
        TOOL_TEXT_LIMIT,
    )
}

fn is_document_value(value: &Value) -> bool {
    matches!(value.get("type").and_then(Value::as_str), Some("document"))
}

fn fingerprint_for_source(source: &ImageSource) -> Result<String, SideQueryError> {
    let mut hasher = Sha256::new();
    match source {
        ImageSource::Base64 {
            media_type, data, ..
        } => {
            validate_base64_media_type(media_type)?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|error| {
                    SideQueryError::Api(llm_runtime::LlmError::InvalidRequest {
                        message: format!("invalid base64 image payload: {error}"),
                    })
                })?;
            hasher.update(bytes);
        }
        ImageSource::Url { url } => {
            validate_delegate_url(url)?;
            hasher.update(b"url:");
            hasher.update(url.as_bytes());
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn decoded_len(source: &ImageSource) -> Result<usize, SideQueryError> {
    match source {
        ImageSource::Base64 {
            media_type, data, ..
        } => {
            validate_base64_media_type(media_type)?;
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .map(|bytes| bytes.len())
                .map_err(|error| {
                    SideQueryError::Api(llm_runtime::LlmError::InvalidRequest {
                        message: format!("invalid base64 image payload: {error}"),
                    })
                })
        }
        ImageSource::Url { url } => {
            validate_delegate_url(url)?;
            Ok(0)
        }
    }
}

fn validate_base64_media_type(media_type: &str) -> Result<(), SideQueryError> {
    let normalized = media_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if matches!(
        normalized.as_str(),
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    ) {
        return Ok(());
    }
    Err(SideQueryError::Api(
        llm_runtime::LlmError::MediaDelegationUnavailable {
            message: "only JPEG, PNG, GIF, and WebP images can be delegated".to_string(),
        },
    ))
}

fn validate_delegate_url(url: &str) -> Result<(), SideQueryError> {
    let parsed = Url::parse(url).map_err(|_| delegate_url_error())?;
    if !parsed.scheme().eq_ignore_ascii_case("http")
        && !parsed.scheme().eq_ignore_ascii_case("https")
    {
        return Err(delegate_url_error());
    }
    if parsed.host_str().unwrap_or_default().is_empty()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(delegate_url_error());
    }
    match parsed.host() {
        Some(url::Host::Domain(host))
            if host.eq_ignore_ascii_case("localhost")
                || host.rsplit_once('.').is_some_and(|(_, suffix)| {
                    suffix.eq_ignore_ascii_case("local") || suffix.eq_ignore_ascii_case("internal")
                }) =>
        {
            Err(delegate_url_error())
        }
        Some(url::Host::Domain(_)) => Ok(()),
        Some(url::Host::Ipv4(ip)) => validate_public_ip(std::net::IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => validate_public_ip(std::net::IpAddr::V6(ip)),
        None => Err(delegate_url_error()),
    }
}

fn supported_image_media_type(headers: &[(String, String)]) -> Result<String, SideQueryError> {
    let raw = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.as_str())
        .ok_or_else(|| {
            remote_fetch_error("image URL response was missing a Content-Type header")
        })?;
    let media_type = raw
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if matches!(
        media_type.as_str(),
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    ) {
        return Ok(media_type);
    }
    Err(remote_fetch_error(
        "image URL response was not a supported JPEG, PNG, GIF, or WebP image",
    ))
}

fn validate_public_target(
    url: &Url,
    resolved: Option<&lingxi_core::host::ResolvedAddressOverride>,
) -> Result<(), SideQueryError> {
    if let Some(resolved) = resolved {
        for addr in &resolved.addrs {
            validate_public_ip(addr.ip())?;
        }
        return Ok(());
    }
    match url.host() {
        Some(url::Host::Ipv4(ip)) => validate_public_ip(std::net::IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => validate_public_ip(std::net::IpAddr::V6(ip)),
        Some(url::Host::Domain(_)) => Ok(()),
        None => Err(delegate_url_error()),
    }
}

fn validate_public_ip(ip: std::net::IpAddr) -> Result<(), SideQueryError> {
    let blocked = match ip {
        std::net::IpAddr::V4(ip) => {
            let octets = ip.octets();
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_broadcast()
                || ip.is_documentation()
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] >= 240 && ip != std::net::Ipv4Addr::BROADCAST)
                || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        }
        std::net::IpAddr::V6(ip) => {
            let segments = ip.segments();
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00
                || (segments[0] & 0xffc0) == 0xfe80
                || segments[0] == 0x2001 && segments[1] == 0x0db8
                || (segments[0] == 0x64 && segments[1] == 0xff9b)
                || (segments[0] == 0
                    && segments[1] == 0
                    && segments[2] == 0
                    && segments[3] == 0
                    && segments[4] == 0
                    && segments[5] == 0xffff)
        }
    };
    if blocked {
        return Err(remote_fetch_error(&format!(
            "image URL resolved to a non-public address: {ip}"
        )));
    }
    Ok(())
}

fn header_u64(headers: &[(String, String)], name: &str) -> Option<u64> {
    headers
        .iter()
        .find(|(header_name, _)| header_name.eq_ignore_ascii_case(name))
        .and_then(|(_, value)| value.parse::<u64>().ok())
}

fn map_ssrf_error(error: &hooks::SsrfError) -> SideQueryError {
    remote_fetch_error(&format!("image URL failed SSRF validation: {error}"))
}

fn map_http_error(error: &HttpError) -> SideQueryError {
    remote_fetch_error(&format!("image URL fetch failed: {error}"))
}

fn remote_fetch_error(message: &str) -> SideQueryError {
    SideQueryError::Api(llm_runtime::LlmError::MediaDelegationUnavailable {
        message: message.to_string(),
    })
}

fn delegate_url_error() -> SideQueryError {
    SideQueryError::Api(llm_runtime::LlmError::MediaDelegationUnavailable {
        message: "only public http(s) image URLs can be sent to the vision delegate".to_string(),
    })
}

fn document_unsupported(source: &DocumentSource) -> SideQueryError {
    let media_type = match source {
        DocumentSource::Base64 { media_type, .. } => media_type.as_str(),
    };
    SideQueryError::Api(llm_runtime::LlmError::MediaDelegationUnavailable {
        message: format!(
            "{media_type} delegation is not supported yet; switch to a document-capable model."
        ),
    })
}

fn document_value_unsupported() -> SideQueryError {
    SideQueryError::Api(llm_runtime::LlmError::MediaDelegationUnavailable {
        message: "PDF delegation is not supported yet; switch to a document-capable model."
            .to_string(),
    })
}

fn placeholder(fingerprint: &str) -> String {
    format!(
        "[Image {}; see media analysis]",
        &fingerprint[..fingerprint.len().min(8)]
    )
}

fn truncate_head_tail(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let head_budget = limit / 2;
    let tail_budget = limit.saturating_sub(head_budget + 5);
    format!(
        "{}\n...\n{}",
        utf8_prefix(text, head_budget),
        utf8_suffix(text, tail_budget)
    )
}

fn utf8_prefix(text: &str, limit: usize) -> &str {
    let mut end = limit.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn utf8_suffix(text: &str, limit: usize) -> &str {
    let mut start = text.len().saturating_sub(limit);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn system_prompt() -> &'static str {
    "You are a vision delegate for an internal coding assistant.
Return JSON only with this exact top-level shape:
{
  \"task_findings\": [string],
  \"media\": [{
    \"fingerprint\": string,
    \"label\": string,
    \"description\": string,
    \"ocr\": string | null,
    \"relevant_facts\": [string],
    \"uncertainty\": string | null
  }],
  \"cross_media_findings\": [string],
  \"truncated\": boolean
}
Be precise, avoid speculation, and leave strings empty when unknown."
}

#[derive(Debug, Default, Deserialize)]
struct DelegatePayload {
    #[serde(default)]
    task_findings: Vec<String>,
    #[serde(default)]
    media: Vec<DelegateObservation>,
    #[serde(default)]
    cross_media_findings: Vec<String>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Debug, Default, Deserialize)]
struct DelegateObservation {
    fingerprint: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    ocr: Option<String>,
    #[serde(default)]
    relevant_facts: Vec<String>,
    #[serde(default)]
    uncertainty: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lingxi_core::host::http::SseStream;
    use lingxi_core::types::{HttpResponse, ToolUseId};
    use std::collections::{HashMap, VecDeque};
    use std::future::pending;
    use std::net::SocketAddr;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };
    use tokio::sync::{oneshot, Notify};
    use tokio::time::{timeout, Duration};

    struct FakeSideQueryClient {
        responses: Mutex<Vec<Result<SideQueryResponse, SideQueryError>>>,
        requests: Mutex<Vec<SideQueryRequest>>,
    }

    impl FakeSideQueryClient {
        fn new(responses: Vec<Result<SideQueryResponse, SideQueryError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl SideQueryClient for FakeSideQueryClient {
        async fn query(
            &self,
            request: SideQueryRequest,
        ) -> Result<SideQueryResponse, SideQueryError> {
            self.requests.lock().unwrap().push(request);
            self.responses.lock().unwrap().remove(0)
        }
    }

    struct AbortAwareSideQueryClient {
        requests: Mutex<Vec<SideQueryRequest>>,
        calls: AtomicUsize,
        second_started: Notify,
        second_aborted: Mutex<Option<oneshot::Sender<()>>>,
    }

    impl AbortAwareSideQueryClient {
        fn new(second_aborted: oneshot::Sender<()>) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
                second_started: Notify::new(),
                second_aborted: Mutex::new(Some(second_aborted)),
            }
        }
    }

    struct DropSignal(Option<oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[async_trait]
    impl SideQueryClient for AbortAwareSideQueryClient {
        async fn query(
            &self,
            request: SideQueryRequest,
        ) -> Result<SideQueryResponse, SideQueryError> {
            self.requests.lock().unwrap().push(request);
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    self.second_started.notified().await;
                    Err(SideQueryError::Api(llm_runtime::LlmError::Transport {
                        message: "delegate unavailable".into(),
                    }))
                }
                1 => {
                    self.second_started.notify_one();
                    let guard = DropSignal(self.second_aborted.lock().unwrap().take());
                    pending::<()>().await;
                    drop(guard);
                    unreachable!("aborted batch should not complete");
                }
                other => panic!("unexpected extra query {other}"),
            }
        }
    }

    type ResolverAnswer = Result<Vec<SocketAddr>, String>;
    type ResolverEntry<'a> = ((&'a str, u16), ResolverAnswer);

    struct StaticResolver {
        answers: Mutex<HashMap<(String, u16), ResolverAnswer>>,
    }

    impl StaticResolver {
        fn with_answers(entries: Vec<ResolverEntry<'_>>) -> Self {
            let mut answers = HashMap::new();
            for ((host, port), answer) in entries {
                answers.insert((host.to_string(), port), answer);
            }
            Self {
                answers: Mutex::new(answers),
            }
        }
    }

    #[async_trait]
    impl hooks::DnsResolver for StaticResolver {
        async fn lookup_host(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
            self.answers
                .lock()
                .unwrap()
                .get(&(host.to_string(), port))
                .cloned()
                .unwrap_or_else(|| Err(format!("missing dns answer for {host}:{port}")))
        }
    }

    struct StaticHttpTransport {
        responses: Mutex<VecDeque<HttpResponse>>,
        resolved: Mutex<Vec<Option<lingxi_core::host::ResolvedAddressOverride>>>,
    }

    impl StaticHttpTransport {
        fn new(responses: Vec<HttpResponse>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
                resolved: Mutex::new(Vec::new()),
            }
        }

        fn resolved_calls(&self) -> Vec<Option<lingxi_core::host::ResolvedAddressOverride>> {
            self.resolved.lock().unwrap().clone()
        }
    }

    struct VecOnceStream(Option<Result<Vec<u8>, HttpError>>);

    impl futures_core::Stream for VecOnceStream {
        type Item = Result<Vec<u8>, HttpError>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::task::Poll::Ready(self.0.take())
        }
    }

    #[async_trait]
    impl HttpTransport for StaticHttpTransport {
        async fn request(
            &self,
            _req: HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, HttpError> {
            Err(HttpError::InvalidRequest("unexpected request".into()))
        }

        async fn stream_sse(&self, _req: HttpRequest) -> Result<SseStream, HttpError> {
            Err(HttpError::InvalidRequest("unexpected stream_sse".into()))
        }

        async fn stream_raw_bytes_with_meta_no_follow_with_resolved_addrs(
            &self,
            _req: HttpRequest,
            resolved: Option<lingxi_core::host::ResolvedAddressOverride>,
        ) -> Result<lingxi_core::host::RawByteStreamWithMeta, HttpError> {
            self.resolved.lock().unwrap().push(resolved);
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("missing scripted response");
            let bytes = response.body_bytes.clone();
            Ok(lingxi_core::host::RawByteStreamWithMeta {
                status: response.status,
                headers: response.headers,
                stream: Box::pin(VecOnceStream(Some(Ok(bytes)))),
            })
        }
    }

    fn user_message(content: Vec<ContentBlock>) -> ConversationMessage {
        ConversationMessage::User {
            id: MessageId::new(),
            content,
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        }
    }

    fn image_block(data: &str) -> ContentBlock {
        ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".into(),
                data: data.into(),
            },
        }
    }

    #[test]
    fn collect_media_supports_top_level_url_and_nested_tool_images() {
        let messages = vec![
            user_message(vec![ContentBlock::Image {
                source: ImageSource::Url {
                    url: "https://example.com/top.png".into(),
                },
            }]),
            user_message(vec![ContentBlock::ToolResult {
                tool_use_id: ToolUseId::new(),
                content: "tool output".into(),
                is_error: false,
                provider_tool_use_id: None,
                content_blocks: Some(vec![serde_json::json!({
                    "type": "image_url",
                    "url": "https://example.com/nested.png"
                })]),
            }]),
        ];
        let media = collect_media(&messages).unwrap();
        assert_eq!(media.len(), 2);
        assert_eq!(media[0].label, "image_1");
        assert_eq!(media[1].label, "image_2");
    }

    #[test]
    fn collect_media_rejects_documents() {
        let messages = vec![user_message(vec![ContentBlock::Document {
            source: DocumentSource::Base64 {
                media_type: "application/pdf".into(),
                data: "YWI=".into(),
            },
        }])];
        assert!(collect_media(&messages).is_err());
    }

    #[test]
    fn main_prepare_preserves_documents_when_natively_supported() {
        let messages = vec![user_message(vec![
            ContentBlock::Document {
                source: DocumentSource::Base64 {
                    media_type: "application/pdf".into(),
                    data: "YWI=".into(),
                },
            },
            image_block("YWI="),
        ])];
        let prepared = prepare_media_for_nonvision(&messages, true).unwrap();
        assert_eq!(prepared.media.len(), 1);
        let ConversationMessage::User { content, .. } = &prepared.rewritten_messages[0] else {
            panic!("expected user message");
        };
        assert!(matches!(content[0], ContentBlock::Document { .. }));
        assert!(matches!(content[1], ContentBlock::Text { .. }));
    }

    #[test]
    fn base64_and_url_fingerprints_differ() {
        let base64 = fingerprint_for_source(&ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "YWI=".into(),
        })
        .unwrap();
        let url = fingerprint_for_source(&ImageSource::Url {
            url: "https://example.com/x.png".into(),
        })
        .unwrap();
        assert_ne!(base64, url);
    }

    #[test]
    fn delegate_rejects_unsupported_base64_image_media_types() {
        let error = fingerprint_for_source(&ImageSource::Base64 {
            media_type: "image/svg+xml".into(),
            data: "YWI=".into(),
        })
        .expect_err("v1 delegation must only accept raster image formats");
        assert!(error
            .to_string()
            .contains("only JPEG, PNG, GIF, and WebP images can be delegated"));
    }

    #[test]
    fn delegate_rejects_non_http_image_urls() {
        let error = fingerprint_for_source(&ImageSource::Url {
            url: "data:image/png;base64,YWI=".into(),
        })
        .expect_err("data URLs must not bypass the decoded-size boundary");
        assert!(error.to_string().contains("public http(s)"));
    }

    #[test]
    fn delegate_rejects_private_or_malformed_image_urls() {
        for url in [
            "http://localhost/image.png",
            "http://127.0.0.1/image.png",
            "http://[::1]/image.png",
            "https://",
        ] {
            assert!(
                fingerprint_for_source(&ImageSource::Url { url: url.into() }).is_err(),
                "{url} must not be delegated"
            );
        }
    }

    #[test]
    fn tool_summary_reads_structured_text_blocks() {
        let summary = tool_result_summary(
            "flat fallback",
            &[
                serde_json::json!({"type": "text", "text": "first"}),
                serde_json::json!({"type": "image_url", "url": "https://example.com/a"}),
                serde_json::json!({"type": "text", "text": "second"}),
            ],
        );
        assert_eq!(summary, "first\nsecond");
    }

    #[test]
    fn batching_respects_count_limit() {
        let item = DelegationMedia {
            fingerprint: "fp".into(),
            label: "image".into(),
            block: image_block("YWI="),
            decoded_bytes_len: 1,
            tool_name: None,
            tool_summary: None,
        };
        let batches = batch_media(vec![item; MAX_MEDIA_PER_QUERY + 1]).unwrap();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), MAX_MEDIA_PER_QUERY);
        assert_eq!(batches[1].len(), 1);
    }

    #[test]
    fn filtering_keeps_only_requested_top_level_and_tool_images() {
        let first = image_block("YWI=");
        let second = image_block("YWM=");
        let wanted = collect_media(&[user_message(vec![second.clone()])]).unwrap()[0]
            .fingerprint
            .clone();
        let messages = vec![user_message(vec![first, second])];
        let filtered = filter_messages_to_fingerprints(
            &messages,
            &std::iter::once(wanted).collect::<HashSet<_>>(),
        )
        .unwrap();
        assert_eq!(collect_media(&filtered).unwrap().len(), 1);
    }

    #[test]
    fn prior_context_is_capped_to_eight_kibibytes_total() {
        let packet = VisionPacket {
            question_key: "q".into(),
            model: "delegate".into(),
            profile: None,
            current_user_text: "current".into(),
            prior_text_context: vec![
                (MessageRole::User, "你".repeat(4_000)),
                (MessageRole::Assistant, "好".repeat(4_000)),
            ],
            media_messages: vec![],
        };
        let messages = build_messages(&packet, &[]);
        let prior_bytes = messages[..messages.len() - 1]
            .iter()
            .map(|message| message.text_content().len())
            .sum::<usize>();
        assert!(prior_bytes <= PRIOR_TEXT_LIMIT);
    }

    #[test]
    fn rewrite_replaces_top_level_and_nested_media() {
        let messages = vec![user_message(vec![
            image_block("YWI="),
            ContentBlock::ToolResult {
                tool_use_id: ToolUseId::new(),
                content: String::new(),
                is_error: false,
                provider_tool_use_id: None,
                content_blocks: Some(vec![serde_json::json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "YWM="
                    }
                })]),
            },
        ])];
        let rewritten = rewrite_media_for_nonvision(&messages).unwrap();
        let ConversationMessage::User { content, .. } = &rewritten[0] else {
            panic!("expected user");
        };
        assert!(
            matches!(&content[0], ContentBlock::Text { text } if text.contains("see media analysis"))
        );
        let ContentBlock::ToolResult {
            content_blocks: Some(blocks),
            ..
        } = &content[1]
        else {
            panic!("expected tool result");
        };
        assert_eq!(blocks[0]["type"], "text");
    }

    #[test]
    fn prepare_delegation_keeps_only_latest_hundred_media() {
        let messages = (0..101)
            .map(|_| user_message(vec![image_block("YWI=")]))
            .collect::<Vec<_>>();
        let prepared = prepare_delegation(&messages).unwrap();
        assert_eq!(prepared.media.len(), MAX_MEDIA_PER_REQUEST);
        assert_eq!(count_media(&prepared.rewritten_messages), 0);
    }

    #[test]
    fn covered_fingerprints_matches_question_model_and_version() {
        let set = covered_fingerprints(
            &[MediaAnalysis {
                question_key: "q".into(),
                media_fingerprints: vec!["fp".into()],
                model: "delegate".into(),
                prompt_version: PROMPT_VERSION,
                created_at: SystemTime::now(),
                task_findings: Vec::new(),
                media: Vec::new(),
                cross_media_findings: Vec::new(),
                truncated: false,
            }],
            "q",
            "delegate",
        );
        assert!(set.contains("fp"));
    }

    #[tokio::test]
    async fn analyze_fetches_url_images_before_delegate_call() {
        let sidequery = Arc::new(FakeSideQueryClient::new(vec![Ok(SideQueryResponse {
            text: Some("summary only".into()),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })]));
        let http = Arc::new(StaticHttpTransport::new(vec![
            lingxi_core::types::HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "image/png".into())],
                body: String::new(),
                body_bytes: b"ab".to_vec(),
            },
        ]));
        let resolver = StaticResolver::with_answers(vec![(
            ("example.com", 443),
            Ok(vec!["93.184.216.34:443".parse().unwrap()]),
        )]);
        let service = VisionDelegationService::new_with_http(
            sidequery.clone(),
            http.clone(),
            SsrfGuard::with_resolver(resolver),
        );
        service
            .analyze(VisionPacket {
                question_key: "q".into(),
                model: "delegate".into(),
                profile: None,
                current_user_text: "What is shown?".into(),
                prior_text_context: Vec::new(),
                media_messages: vec![user_message(vec![ContentBlock::Image {
                    source: ImageSource::Url {
                        url: "https://example.com/image.png".into(),
                    },
                }])],
            })
            .await
            .unwrap();
        let requests = sidequery.requests.lock().unwrap();
        let ConversationMessage::User { content, .. } = requests[0].messages.last().unwrap() else {
            panic!("expected delegate user message");
        };
        assert!(content.iter().any(|block| matches!(
            block,
            ContentBlock::Image {
                source: ImageSource::Base64 { media_type, data }
            } if media_type == "image/png" && data == "YWI="
        )));
        let pinned_calls = http.resolved_calls();
        assert_eq!(pinned_calls.len(), 1);
        assert_eq!(
            pinned_calls[0].as_ref().unwrap().domain,
            "example.com",
            "delegate fetch must pin DNS answers for the request"
        );
    }

    #[tokio::test]
    async fn analyze_rejects_private_redirect_targets() {
        let sidequery = Arc::new(FakeSideQueryClient::new(Vec::new()));
        let http = Arc::new(StaticHttpTransport::new(vec![
            lingxi_core::types::HttpResponse {
                status: 302,
                headers: vec![(
                    "location".into(),
                    "http://internal.example/image.png".into(),
                )],
                body: String::new(),
                body_bytes: Vec::new(),
            },
        ]));
        let resolver = StaticResolver::with_answers(vec![
            (
                ("public.example", 443),
                Ok(vec!["93.184.216.34:443".parse().unwrap()]),
            ),
            (
                ("internal.example", 80),
                Ok(vec!["127.0.0.1:80".parse().unwrap()]),
            ),
        ]);
        let service = VisionDelegationService::new_with_http(
            sidequery,
            http,
            SsrfGuard::with_resolver(resolver),
        );
        let error = service
            .analyze(VisionPacket {
                question_key: "q".into(),
                model: "delegate".into(),
                profile: None,
                current_user_text: "What is shown?".into(),
                prior_text_context: Vec::new(),
                media_messages: vec![user_message(vec![ContentBlock::Image {
                    source: ImageSource::Url {
                        url: "https://public.example/image.png".into(),
                    },
                }])],
            })
            .await
            .expect_err("private redirect targets must be blocked");
        assert!(error.to_string().contains("SSRF validation"));
    }

    #[tokio::test]
    async fn analyze_rejects_non_public_ip_literal_targets() {
        let sidequery = Arc::new(FakeSideQueryClient::new(Vec::new()));
        let http = Arc::new(StaticHttpTransport::new(Vec::new()));
        let service =
            VisionDelegationService::new_with_http(sidequery, http, SsrfGuard::with_defaults());
        for url in [
            "http://100.64.0.1/image.png",
            "http://198.18.0.1/image.png",
            "http://[::ffff:10.0.0.1]/image.png",
            "http://[64:ff9b::c000:201]/image.png",
        ] {
            let error = service
                .fetch_remote_image(url)
                .await
                .expect_err("non-public targets must be rejected");
            assert!(
                error.to_string().contains("non-public address"),
                "{url} should fail the local public-IP check"
            );
        }
    }

    #[tokio::test]
    async fn analyze_rejects_oversized_url_images() {
        let sidequery = Arc::new(FakeSideQueryClient::new(Vec::new()));
        let http = Arc::new(StaticHttpTransport::new(vec![
            lingxi_core::types::HttpResponse {
                status: 200,
                headers: vec![
                    ("content-type".into(), "image/png".into()),
                    (
                        "content-length".into(),
                        (MAX_DECODED_BYTES_PER_QUERY + 1).to_string(),
                    ),
                ],
                body: String::new(),
                body_bytes: vec![0; 8],
            },
        ]));
        let resolver = StaticResolver::with_answers(vec![(
            ("example.com", 443),
            Ok(vec!["93.184.216.34:443".parse().unwrap()]),
        )]);
        let service = VisionDelegationService::new_with_http(
            sidequery,
            http,
            SsrfGuard::with_resolver(resolver),
        );
        let error = service
            .analyze(VisionPacket {
                question_key: "q".into(),
                model: "delegate".into(),
                profile: None,
                current_user_text: "What is shown?".into(),
                prior_text_context: Vec::new(),
                media_messages: vec![user_message(vec![ContentBlock::Image {
                    source: ImageSource::Url {
                        url: "https://example.com/image.png".into(),
                    },
                }])],
            })
            .await
            .expect_err("oversized URL images must be rejected");
        assert!(error.to_string().contains("24 MiB"));
    }

    #[tokio::test]
    async fn analyze_parses_structured_json() {
        let fp = fingerprint_for_source(&ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "YWI=".into(),
        })
        .unwrap();
        let client = Arc::new(FakeSideQueryClient::new(vec![Ok(SideQueryResponse {
            text: None,
            structured: Some(serde_json::json!({
                "task_findings": ["receipt"],
                "media": [{
                    "fingerprint": fp,
                    "label": "image_1",
                    "description": "desc",
                    "relevant_facts": ["fact"]
                }],
                "cross_media_findings": [],
                "truncated": false
            })),
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("max_tokens".into()),
            retry_count: 2,
        })]));
        let service = VisionDelegationService::new(client);
        let result = service
            .analyze(VisionPacket {
                question_key: "q".into(),
                model: "delegate".into(),
                profile: Some("deepseek".into()),
                current_user_text: "What is shown?".into(),
                prior_text_context: Vec::new(),
                media_messages: vec![user_message(vec![image_block("YWI=")])],
            })
            .await
            .unwrap();
        assert_eq!(result.analysis.task_findings, vec!["receipt"]);
        assert!(result.analysis.truncated);
        assert_eq!(result.retry_count, 2);
    }

    #[tokio::test]
    async fn analyze_sums_retries_across_batches() {
        let response = |retry_count| SideQueryResponse {
            text: Some("batch summary".into()),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count,
        };
        let client = Arc::new(FakeSideQueryClient::new(vec![
            Ok(response(1)),
            Ok(response(2)),
        ]));
        let service = VisionDelegationService::new(client);
        let result = service
            .analyze(VisionPacket {
                question_key: "q".into(),
                model: "delegate".into(),
                profile: None,
                current_user_text: "compare".into(),
                prior_text_context: Vec::new(),
                media_messages: vec![user_message(
                    (0..=MAX_MEDIA_PER_QUERY)
                        .map(|_| image_block("YWI="))
                        .collect(),
                )],
            })
            .await
            .unwrap();
        assert_eq!(result.api_calls, 2);
        assert_eq!(result.retry_count, 3);
    }

    #[tokio::test]
    async fn analyze_reports_partial_batch_accounting_without_persisting() {
        let client = Arc::new(FakeSideQueryClient::new(vec![
            Ok(SideQueryResponse {
                text: Some("batch summary".into()),
                structured: None,
                tool_calls: Vec::new(),
                usage: cost::Usage::default(),
                stop_reason: Some("end_turn".into()),
                retry_count: 1,
            }),
            Err(SideQueryError::Api(llm_runtime::LlmError::Transport {
                message: "delegate unavailable".into(),
            })),
        ]));
        let service = VisionDelegationService::new(client);
        let error = service
            .analyze(VisionPacket {
                question_key: "q".into(),
                model: "delegate".into(),
                profile: None,
                current_user_text: "compare".into(),
                prior_text_context: Vec::new(),
                media_messages: vec![user_message(
                    (0..=MAX_MEDIA_PER_QUERY)
                        .map(|_| image_block("YWI="))
                        .collect(),
                )],
            })
            .await
            .expect_err("one failed batch must fail the whole analysis");
        match error {
            SideQueryError::Partial {
                api_calls,
                retry_count,
                ..
            } => {
                assert_eq!(api_calls, 2);
                assert_eq!(retry_count, 1);
            }
            other => panic!("expected partial accounting, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn analyze_aborts_in_flight_batches_after_first_failure() {
        let (aborted_tx, aborted_rx) = oneshot::channel();
        let client = Arc::new(AbortAwareSideQueryClient::new(aborted_tx));
        let service = VisionDelegationService::new(client);
        let error = timeout(
            Duration::from_secs(1),
            service.analyze(VisionPacket {
                question_key: "q".into(),
                model: "delegate".into(),
                profile: None,
                current_user_text: "compare".into(),
                prior_text_context: Vec::new(),
                media_messages: vec![user_message(
                    (0..=MAX_MEDIA_PER_QUERY)
                        .map(|_| image_block("YWI="))
                        .collect(),
                )],
            }),
        )
        .await
        .expect("analysis should not wait for an aborted batch")
        .expect_err("one failed batch must fail the whole analysis");
        match error {
            SideQueryError::Partial { api_calls, .. } => assert_eq!(api_calls, 2),
            other => panic!("expected partial accounting, got {other:?}"),
        }
        timeout(Duration::from_secs(1), aborted_rx)
            .await
            .expect("aborted batch must drop promptly")
            .expect("abort signal should be delivered");
    }

    #[tokio::test]
    async fn analyze_falls_back_to_text() {
        let client = Arc::new(FakeSideQueryClient::new(vec![Ok(SideQueryResponse {
            text: Some("summary only".into()),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })]));
        let service = VisionDelegationService::new(client);
        let result = service
            .analyze(VisionPacket {
                question_key: "q".into(),
                model: "delegate".into(),
                profile: None,
                current_user_text: "What is shown?".into(),
                prior_text_context: Vec::new(),
                media_messages: vec![user_message(vec![image_block("YWI=")])],
            })
            .await
            .unwrap();
        assert_eq!(result.analysis.task_findings, vec!["summary only"]);
    }

    #[tokio::test]
    async fn analyze_rejects_empty_delegate_response() {
        let client = Arc::new(FakeSideQueryClient::new(vec![Ok(SideQueryResponse {
            text: Some(String::new()),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })]));
        let service = VisionDelegationService::new(client);
        let error = service
            .analyze(VisionPacket {
                question_key: "q".into(),
                model: "delegate".into(),
                profile: None,
                current_user_text: "What is shown?".into(),
                prior_text_context: Vec::new(),
                media_messages: vec![user_message(vec![image_block("YWI=")])],
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("empty response"));
    }
}
