//! Session-memory extraction (spec §6.5 `SessionMemoryExtractor`).
//!
//! Captures durable notes from an in-flight or finished session and writes them
//! to `<configHome>/agents/session-memory/<session_id>.md`. That file is NOT
//! injected through a second path: the next session re-loads it through the
//! normal memory-load → selector → prefetch pipeline and it surfaces via the
//! SURFACING-owned `relevant_memory_reminder_messages`. So this module owns only
//! the WRITE + threshold-trigger side; it never adds a surfacing API of its own.
//!
//! 1:1 with the §6.5 sketch:
//!
//! ```text
//! pub struct SessionMemoryExtractor { config, last_extracted_message_id }
//! pub fn should_extract(&self, conversation) -> bool {
//!   let tokens = token_count_with_estimation(history);
//!   let grew = tokens - tokens_at_last_extraction;
//!   let calls = count_tool_calls_since(history, last_extracted_message_id);
//!   tokens >= config.minimum_message_tokens_to_init
//!     && grew >= config.minimum_tokens_between_update
//!     && (calls >= config.tool_calls_between_updates
//!       || !has_tool_calls_in_last_assistant_turn(history))
//! }
//! pub async fn extract(&mut self, conversation) -> Result<String, MemoryError>
//!   // Effect::ForkAgent — extract via forked agent
//! ```
//!
//! Gating is config-only: the sole gate is [`SessionMemoryConfig::enabled`]
//! (there is no `tengu_session_memory` flag in claude-code v2.1.251 — the stale
//! m3-02 plan invented one). `enabled == false` keeps the whole subsystem inert,
//! so the ~4000 locked fixtures stay byte-identical.

use crate::file::{MemoryError, MemoryFrontmatter, parse_markdown_with_frontmatter};
use lingxi_core::types::{ConversationMessage, MessageId};
use sidequery::{CacheSafeParams, ForkPurpose, ForkedAgentRequest, ForkedAgentRunner, QuerySource};
use std::path::{Path, PathBuf};

/// Configuration for the standalone session-memory extractor (spec §6.5).
///
/// `enabled` is the single master gate. Extraction requires the configured
/// context size, token growth and activity gates.
#[derive(Debug, Clone)]
pub struct SessionMemoryConfig {
    /// Master gate for the session-memory subsystem.
    pub enabled: bool,
    /// Minimum context size required before the first extraction. Claude Code
    /// 2.1.251 uses 10,000 tokens for this gate.
    pub minimum_message_tokens_to_init: u32,
    /// Minimum context growth required between extractions. Claude Code
    /// 2.1.251 uses 5,000 tokens for this gate.
    pub minimum_tokens_between_update: u32,
    /// Number of tool calls required between extractions. Claude Code
    /// 2.1.251 uses three tool calls, unless a natural no-tool assistant turn
    /// reaches the token-growth gate first.
    pub tool_calls_between_updates: u32,
}

impl Default for SessionMemoryConfig {
    fn default() -> Self {
        Self {
            // Inert by default — keeps the locked fixtures byte-identical until a
            // composition root opts in.
            enabled: false,
            minimum_message_tokens_to_init: 10_000,
            minimum_tokens_between_update: 5_000,
            tool_calls_between_updates: 3,
        }
    }
}

/// Distils a session into reusable memory notes and writes them to the
/// per-session memory file (spec §6.5).
#[derive(Debug, Clone)]
pub struct SessionMemoryExtractor {
    config: SessionMemoryConfig,
    /// Id of the last message covered by the previous extraction; drives the
    /// "tool calls since" window. `None` until the first extraction completes.
    last_extracted_message_id: Option<MessageId>,
    /// Most recently observed "tool calls since extraction" count while the
    /// extraction watermark was still visible. When compaction drops that
    /// watermark, this becomes the carried-forward baseline.
    observed_tool_calls_since_extraction: u32,
    /// After compaction removes the extraction watermark from retained history,
    /// count newly visible tool calls from this compacted tail onward.
    post_compaction_anchor_message_id: Option<MessageId>,
    /// Claude marks session memory initialized as soon as the initial token
    /// threshold is crossed, before the background extraction necessarily
    /// succeeds. Keep this separate from the extraction watermark.
    session_memory_initialized: bool,
    /// Context size recorded after the last successful extraction. This is the
    /// baseline for the minimum-token-growth gate.
    tokens_at_last_extraction: u64,
    /// Monotonic revision used to reject compact snapshots that span an
    /// intervening background extraction.
    state_revision: u64,
}

impl SessionMemoryExtractor {
    /// Build an extractor bound to `config`, with no prior extraction.
    #[must_use]
    pub fn new(config: SessionMemoryConfig) -> Self {
        Self {
            config,
            last_extracted_message_id: None,
            observed_tool_calls_since_extraction: 0,
            post_compaction_anchor_message_id: None,
            session_memory_initialized: false,
            tokens_at_last_extraction: 0,
            state_revision: 0,
        }
    }

    /// Borrow the active configuration.
    #[must_use]
    pub fn config(&self) -> &SessionMemoryConfig {
        &self.config
    }

    /// `true` once at least one extraction has completed (spec §6.5
    /// `is_initialized`). The initial token gate has separate state.
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.last_extracted_message_id.is_some()
    }

    /// Borrow the last extraction watermark, if any.
    #[must_use]
    pub fn extraction_watermark(&self) -> Option<&MessageId> {
        self.last_extracted_message_id.as_ref()
    }

    /// The accumulated "tool calls since extraction" count currently tracked by
    /// this extractor, including any carried progress across compaction.
    #[must_use]
    pub fn pending_tool_calls(&self) -> u32 {
        self.observed_tool_calls_since_extraction
    }

    /// Revision of mutable extraction progress.
    #[must_use]
    pub fn state_revision(&self) -> u64 {
        self.state_revision
    }

    fn bump_state_revision(&mut self) {
        self.state_revision = self.state_revision.wrapping_add(1);
    }

    /// Mark a successful extraction as covering messages through
    /// `covered_through`, resetting any carried threshold progress.
    pub fn mark_extracted_through(&mut self, covered_through: Option<MessageId>) {
        self.last_extracted_message_id = covered_through;
        self.session_memory_initialized = true;
        self.observed_tool_calls_since_extraction = 0;
        self.post_compaction_anchor_message_id = None;
        self.bump_state_revision();
    }

    /// Reset conversation-scoped extraction state when an orchestrator clears
    /// or replaces its active session.
    pub fn reset(&mut self) {
        self.mark_extracted_through(None);
        self.session_memory_initialized = false;
        self.tokens_at_last_extraction = 0;
        self.bump_state_revision();
    }

    /// Record the estimated context size covered by a successful extraction.
    /// This is separate from [`Self::mark_extracted_through`] because that
    /// method is also used by resume/compaction bookkeeping without a history
    /// snapshot available.
    pub fn record_extraction_token_count(&mut self, current_token_count: u64) {
        self.tokens_at_last_extraction = current_token_count;
        self.bump_state_revision();
    }

    /// Context size recorded at the last successful extraction.
    #[must_use]
    pub fn tokens_at_last_extraction(&self) -> u64 {
        self.tokens_at_last_extraction
    }

    /// Preserve exact pre-compaction threshold progress and resume counting
    /// newly visible tool calls from the compacted tail afterwards.
    pub fn record_compaction_boundary(
        &mut self,
        compacted_tail_message_id: Option<MessageId>,
        tool_calls_since_extraction: u32,
    ) {
        self.observed_tool_calls_since_extraction = tool_calls_since_extraction;
        self.post_compaction_anchor_message_id = compacted_tail_message_id;
        self.bump_state_revision();
    }

    /// Whether enough context and activity have accrued since the last
    /// extraction to trigger another one (Claude Code's
    /// `shouldExtractMemory`).
    ///
    /// Gated on [`SessionMemoryConfig::enabled`] first. The initial token
    /// threshold marks the feature initialized before the asynchronous fork;
    /// every extraction then requires token growth and either the configured
    /// tool-call count or a natural assistant turn with no tool calls.
    #[must_use]
    pub fn should_extract(&mut self, history: &[ConversationMessage]) -> bool {
        if !self.config.enabled {
            return false;
        }
        let current_tokens = token_count_with_estimation(history);
        if !self.session_memory_initialized {
            if current_tokens < u64::from(self.config.minimum_message_tokens_to_init) {
                return false;
            }
            // Claude flips this state before starting the background fork.
            self.session_memory_initialized = true;
            self.bump_state_revision();
        }

        let token_growth = current_tokens.saturating_sub(self.tokens_at_last_extraction);
        if token_growth < u64::from(self.config.minimum_tokens_between_update) {
            return false;
        }

        let calls = self.pending_tool_calls_for_history(history);
        let has_tool_call_threshold = calls >= self.config.tool_calls_between_updates;
        let last_assistant_has_tool_calls = has_tool_calls_in_last_assistant_turn(history);
        has_tool_call_threshold || !last_assistant_has_tool_calls
    }

    /// Run one forked extraction over `history`, write the distilled notes to
    /// `<config_home>/agents/session-memory/<session_id>.md`, advance the
    /// extraction watermark, and return the written content.
    ///
    /// 1:1 with §6.5 `extract` (`Effect::ForkAgent`): the distillation runs as a
    /// single-turn forked agent off the parent's cache-safe prefix so the prompt
    /// cache hits, then the result is persisted as the session-memory file the
    /// next session re-loads through the normal selector/prefetch/surfacing path
    /// (this module adds NO second injection).
    ///
    /// `config_home` is the resolved `$LINGXI_CONFIG_DIR ?? $HOME/.lingxi`
    /// directory (see [`session_memory_path`]); the caller passes it so this
    /// stays testable with a tempdir.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::SelectorUnavailable`] if the forked extraction
    /// call fails, or [`MemoryError::Io`] if the file write fails.
    pub async fn extract(
        &mut self,
        runner: &ForkedAgentRunner,
        cache_safe_params: CacheSafeParams,
        session_id: &str,
        history: &[ConversationMessage],
        config_home: &Path,
    ) -> Result<String, MemoryError> {
        self.extract_with_coverage(
            runner,
            cache_safe_params,
            session_id,
            history.last().map(ConversationMessage::id),
            config_home,
        )
        .await
    }

    /// Run one forked extraction, persist the result, and mark only the
    /// message range that actually reached the extractor as covered.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::SelectorUnavailable`] if the forked extraction
    /// call fails, or [`MemoryError::Io`] if the file write fails.
    pub async fn extract_with_coverage(
        &mut self,
        runner: &ForkedAgentRunner,
        cache_safe_params: CacheSafeParams,
        session_id: &str,
        covered_through: Option<MessageId>,
        config_home: &Path,
    ) -> Result<String, MemoryError> {
        let content = Self::run_extraction(runner, cache_safe_params).await?;
        self.commit_extraction(&content, session_id, covered_through, config_home)?;
        Ok(content)
    }

    /// Run the model request for an extraction without mutating extractor
    /// state or writing its result. Callers can therefore release their state
    /// lock while the potentially slow side query is in flight, then validate
    /// the captured session/revision before committing.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::SelectorUnavailable`] when the forked request
    /// fails.
    pub async fn run_extraction(
        runner: &ForkedAgentRunner,
        cache_safe_params: CacheSafeParams,
    ) -> Result<String, MemoryError> {
        let request = ForkedAgentRequest {
            prompt_messages: vec![ConversationMessage::user(
                MessageId::new(),
                SESSION_MEMORY_EXTRACTION_PROMPT.to_string(),
            )],
            cache_safe_params,
            fork_label: "session-memory".to_string(),
            query_source: QuerySource::SessionMemoryExtraction,
            max_output_tokens: None,
        };

        let result = runner
            .run(request)
            .await
            .map_err(|e| MemoryError::SelectorUnavailable(e.to_string()))?;
        Ok(normalize_session_memory_document(&result.final_text))
    }

    /// Persist a previously generated extraction and advance its coverage
    /// watermark. The orchestrator calls this only after revalidating the
    /// session generation and extractor revision under its short state lock.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Io`] if the session-memory file cannot be written.
    pub fn commit_extraction(
        &mut self,
        content: &str,
        session_id: &str,
        covered_through: Option<MessageId>,
        config_home: &Path,
    ) -> Result<(), MemoryError> {
        let path = session_memory_path(config_home, session_id);
        write_session_memory(&path, content)?;

        // Advance the watermark only to the last message that actually reached
        // the extraction fork so unseen newer history is not marked extracted.
        self.mark_extracted_through(covered_through);
        Ok(())
    }

    /// The `ForkPurpose` tag this extractor forks under (telemetry/log shape).
    #[must_use]
    pub fn fork_purpose() -> ForkPurpose {
        ForkPurpose::SessionMemoryExtraction
    }

    fn realign_after_compaction_if_needed(&mut self, history: &[ConversationMessage]) {
        let Some(anchor) = self.active_count_anchor() else {
            return;
        };
        if history.iter().any(|message| &message.id() == anchor) {
            return;
        }
        let carried = if self.post_compaction_anchor_message_id.is_some() {
            self.pending_tool_calls_for_history_without_realign(history)
        } else {
            self.observed_tool_calls_since_extraction
        };
        self.observed_tool_calls_since_extraction = carried;
        self.post_compaction_anchor_message_id = history.last().map(ConversationMessage::id);
    }

    /// Return the exact threshold progress represented by `history`, carrying
    /// previously observed progress across a compaction boundary when the old
    /// extraction watermark is no longer present.
    #[must_use]
    pub fn pending_tool_calls_for_history(&mut self, history: &[ConversationMessage]) -> u32 {
        self.realign_after_compaction_if_needed(history);
        let total = self.pending_tool_calls_for_history_without_realign(history);
        self.observed_tool_calls_since_extraction = total;
        if self.post_compaction_anchor_message_id.is_some() {
            self.post_compaction_anchor_message_id = history.last().map(ConversationMessage::id);
        }
        total
    }

    fn pending_tool_calls_for_history_without_realign(
        &self,
        history: &[ConversationMessage],
    ) -> u32 {
        let visible = u32::try_from(count_tool_calls_since(history, self.active_count_anchor()))
            .unwrap_or(u32::MAX);
        if self.post_compaction_anchor_message_id.is_some() {
            self.observed_tool_calls_since_extraction
                .saturating_add(visible)
        } else {
            visible
        }
    }

    fn active_count_anchor(&self) -> Option<&MessageId> {
        self.post_compaction_anchor_message_id
            .as_ref()
            .or(self.last_extracted_message_id.as_ref())
    }
}

/// Count assistant tool-call blocks in `history` that occur *after* the message
/// with id `since` (exclusive). When `since` is `None` (no prior extraction),
/// every tool call in `history` counts.
///
/// Counts individual `ToolUse` blocks (an assistant turn may request several),
/// matching the "tool calls since" intent of §6.5 — a multi-tool turn moves the
/// threshold by more than one.
#[must_use]
pub fn count_tool_calls_since(history: &[ConversationMessage], since: Option<&MessageId>) -> usize {
    // Skip everything up to and including the `since` message; count tool-use
    // blocks in the remainder.
    let tail: &[ConversationMessage] = match since {
        Some(id) => match history.iter().position(|m| &m.id() == id) {
            Some(idx) => &history[idx + 1..],
            // A missing watermark means the caller has crossed a compaction
            // boundary. Fail closed here; `should_extract` realigns its mutable
            // watermark to the compacted tail before a future count.
            None => &[],
        },
        None => history,
    };
    tail.iter().map(|m| m.tool_calls().len()).sum()
}

/// Match Claude Code's rough token estimate for session-memory thresholds.
/// Text uses JavaScript's UTF-16 string length; media is charged at the
/// conservative fixed 2,000-token estimate used by the compaction pipeline;
/// structured blocks use their serialized size so tool arguments are not
/// silently treated as zero-length.
#[must_use]
pub fn token_count_with_estimation(history: &[ConversationMessage]) -> u64 {
    history.iter().map(estimate_message_tokens).sum()
}

fn rough_token_count(length: usize) -> u64 {
    // JavaScript Math.round(length / 4), expressed without floating point.
    u64::try_from(length.saturating_add(2) / 4).unwrap_or(u64::MAX)
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn estimate_message_tokens(message: &ConversationMessage) -> u64 {
    match message {
        ConversationMessage::User { content, .. }
        | ConversationMessage::Assistant { content, .. } => {
            content.iter().map(estimate_block_tokens).sum()
        }
        ConversationMessage::System { content, .. } => rough_token_count(utf16_len(content)),
    }
}

fn estimate_block_tokens(block: &lingxi_core::types::ContentBlock) -> u64 {
    use lingxi_core::types::ContentBlock;

    match block {
        ContentBlock::ProviderContent { value, .. } => rough_token_count(json_len(value)),
        ContentBlock::Text { text, .. } => rough_token_count(utf16_len(text)),
        ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } => rough_token_count(utf16_code_units.len()),
        ContentBlock::ToolUse { name, input, .. } => {
            rough_token_count(utf16_len(name).saturating_add(json_len(input)))
        }
        ContentBlock::ToolResult {
            content,
            content_blocks,
            ..
        } => content_blocks.as_ref().map_or_else(
            || rough_token_count(utf16_len(content)),
            |blocks| {
                blocks
                    .iter()
                    .map(|value| rough_token_count(json_len(value)))
                    .sum()
            },
        ),
        ContentBlock::Thinking { thinking, .. } => rough_token_count(utf16_len(thinking)),
        ContentBlock::RedactedThinking { data } => rough_token_count(utf16_len(data)),
        ContentBlock::Image { .. } | ContentBlock::Document { .. } => 2_000,
        ContentBlock::ServerToolUse { name, input, .. } => {
            rough_token_count(utf16_len(name).saturating_add(json_len(input)))
        }
        ContentBlock::ConnectorText { connector_text, .. } => {
            rough_token_count(utf16_len(connector_text))
        }
        ContentBlock::AdvisorToolResult { content, .. } => rough_token_count(json_len(content)),
        ContentBlock::MediaAnalysis { analysis } => rough_token_count(json_len(analysis)),
    }
}

fn json_len<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_string(value)
        .map(|serialized| utf16_len(&serialized))
        .unwrap_or_default()
}

fn has_tool_calls_in_last_assistant_turn(history: &[ConversationMessage]) -> bool {
    history
        .iter()
        .rev()
        .find_map(|message| match message {
            ConversationMessage::Assistant { .. } => Some(message.has_tool_use()),
            _ => None,
        })
        .unwrap_or(false)
}

/// Resolve the on-disk path for a session's memory file:
/// `<config_home>/agents/session-memory/<session_id>.md`.
///
/// The `agents/session-memory/*.md` layout matches the existing
/// `detect_session_file_type` reader (`tools/file/src/read.rs`), which classifies
/// files under `<configHome>/.../session-memory/*.md` as `"session_memory"`.
#[must_use]
pub fn session_memory_path(config_home: &Path, session_id: &str) -> PathBuf {
    let safe_session_id = sanitize_session_id(session_id);
    config_home
        .join("agents")
        .join("session-memory")
        .join(format!("{safe_session_id}.md"))
}

/// Convert the external/session-facing identifier into a portable filename.
/// `SessionId::Display` is `sess:<uuid>`; the prefix and colon are not part of
/// the durable identity and the colon is illegal in Windows filenames.
pub(crate) fn sanitize_session_id(session_id: &str) -> String {
    let raw = session_id.strip_prefix("sess:").unwrap_or(session_id);
    let mut safe = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            safe.push(ch);
        } else {
            safe.push('_');
        }
    }
    if safe.is_empty() {
        "unknown-session".to_string()
    } else {
        safe
    }
}

/// Ensure persisted session memories carry the metadata required by the
/// selector. The extractor is asked for frontmatter, but this fallback keeps
/// malformed/legacy model output discoverable instead of silently producing an
/// opaque UUID file with empty selector metadata.
fn normalize_session_memory_document(raw: &str) -> String {
    let (mut frontmatter, body) = parse_markdown_with_frontmatter(raw)
        .unwrap_or_else(|_| (MemoryFrontmatter::default(), raw.to_string()));
    if frontmatter.memory_type.is_empty() {
        frontmatter.memory_type = "session_memory".to_string();
    }
    if frontmatter.description.trim().is_empty() {
        frontmatter.description = body
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| line.trim_start_matches('#').trim())
            .map_or_else(
                || "Durable notes extracted from a previous session".to_string(),
                |line| line.chars().take(160).collect(),
            );
    }
    if frontmatter.when_to_use.is_none() {
        frontmatter.when_to_use =
            Some("When a future session needs durable context from this session.".to_string());
    }
    if frontmatter.tags.is_empty() {
        frontmatter.tags.push("session".to_string());
    }
    let yaml = serde_yaml::to_string(&frontmatter).unwrap_or_default();
    format!("---\n{yaml}---\n{body}")
}

/// Write `content` to `path`, creating parent directories as needed.
///
/// # Errors
///
/// Returns [`MemoryError::Io`] if directory creation or the write fails.
fn write_session_memory(path: &Path, content: &str) -> Result<(), MemoryError> {
    let parent = path
        .parent()
        .ok_or_else(|| MemoryError::Io("session-memory path has no parent".to_string()))?;
    std::fs::create_dir_all(parent).map_err(|error| MemoryError::Io(error.to_string()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| MemoryError::Io("session-memory path has no file name".to_string()))?;
    let mut options = lingxi_core::host::rooted_fs::AtomicWriteOptions::default();
    #[cfg(unix)]
    if let Ok(metadata) = std::fs::metadata(path) {
        use std::os::unix::fs::PermissionsExt as _;
        options.file_mode = metadata.permissions().mode() & 0o777;
    }
    lingxi_core::host::rooted_fs::atomic_write(
        parent,
        Path::new(file_name),
        content.as_bytes(),
        options,
    )
    .map_err(|error| MemoryError::Io(error.to_string()))
}

/// Resolve the config-home directory used for session-memory writes:
/// `$LINGXI_CONFIG_DIR` when set (claude-code `??`: an empty value is honored
/// verbatim → cwd-relative) else `$HOME/.lingxi` else `$USERPROFILE/.lingxi`
/// else a bare `.lingxi`.
///
/// Delegates the `$LINGXI_CONFIG_DIR`-vs-home resolution to the canonical
/// [`crate::lingxi_md::user_config_dir`]; this fn only resolves the fallback home
/// from `$HOME`/`$USERPROFILE` (the memory crate has no `dirs` dependency).
/// `Path::new("").join(".lingxi") == ".lingxi"`, so the no-home case stays the
/// bare `.lingxi` form.
#[must_use]
pub fn config_home_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map_or_else(|| PathBuf::from(""), PathBuf::from);
    crate::lingxi_md::user_config_dir(&home)
}

/// Distillation instruction handed to the forked extractor. Kept terse — the
/// fork replays the parent's full cache-safe prefix (the conversation), so this
/// only needs to steer the distillation, not restate the history.
const SESSION_MEMORY_EXTRACTION_PROMPT: &str = "Distil durable, reusable notes from this session that a future session should remember — stable facts about the project, conventions, tooling, and decisions. Omit transient state and anything already obvious from the code. Respond with ONE concise markdown document and no surrounding commentary. It MUST begin with YAML frontmatter containing memory_type: session_memory, a useful one-line description, when_to_use, and tags, followed by the notes body.";

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::{ContentBlock, MessageId, ToolUseId};
    use std::sync::Arc;

    fn enabled_config(
        minimum_message_tokens_to_init: u32,
        minimum_tokens_between_update: u32,
        tool_calls_between_updates: u32,
    ) -> SessionMemoryConfig {
        SessionMemoryConfig {
            enabled: true,
            minimum_message_tokens_to_init,
            minimum_tokens_between_update,
            tool_calls_between_updates,
        }
    }

    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), text.to_string())
    }

    /// An assistant message requesting `n` tool calls.
    fn assistant_tools(n: usize) -> ConversationMessage {
        let content = (0..n)
            .map(|i| ContentBlock::ToolUse { input_projection: None,
                id: ToolUseId::new(),
                provider_id: None,
                name: format!("Tool{i}"),
                input: serde_json::json!({}),
            })
            .collect();
        ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content,
            stop_reason: Some("tool_use".into()),
        }
    }

    #[test]
    fn default_config_is_inert() {
        let c = SessionMemoryConfig::default();
        assert!(
            !c.enabled,
            "must be off by default (locked fixtures stay byte-identical)"
        );
        assert_eq!(c.minimum_message_tokens_to_init, 10_000);
        assert_eq!(c.minimum_tokens_between_update, 5_000);
        assert_eq!(c.tool_calls_between_updates, 3);
    }

    #[test]
    fn disabled_extractor_never_fires() {
        // Even with a flood of tool calls, a disabled config never extracts.
        let mut ex = SessionMemoryExtractor::new(SessionMemoryConfig::default());
        let history = vec![assistant_tools(100)];
        assert!(!ex.should_extract(&history));
    }

    #[test]
    fn reset_clears_watermark_and_carried_progress() {
        let mut ex = SessionMemoryExtractor::new(enabled_config(0, 0, 3));
        let watermark = user("watermark");
        ex.mark_extracted_through(Some(watermark.id()));
        ex.record_compaction_boundary(Some(user("tail").id()), 2);

        ex.reset();

        assert!(!ex.is_initialized());
        assert_eq!(ex.pending_tool_calls(), 0);
        assert!(ex.post_compaction_anchor_message_id.is_none());
        assert_eq!(ex.tokens_at_last_extraction(), 0);
    }

    #[test]
    fn should_extract_requires_configured_tool_calls_before_first_extraction() {
        let mut ex = SessionMemoryExtractor::new(enabled_config(0, 0, 3));
        assert!(!ex.is_initialized());
        // Two tool calls do not reach the configured activity gate.
        assert!(!ex.should_extract(&[assistant_tools(2)]));
        assert!(ex.session_memory_initialized);
        // Three tool calls reach the same gate before any completed extraction.
        assert!(ex.should_extract(&[assistant_tools(3)]));
    }

    #[test]
    fn should_extract_requires_configured_tool_calls_after_first_extraction() {
        let mut ex = SessionMemoryExtractor::new(enabled_config(0, 0, 2));
        // Simulate a completed first extraction by setting the watermark.
        let watermark = user("watermark");
        ex.mark_extracted_through(Some(watermark.id()));
        assert!(ex.is_initialized());

        // History: watermark, then 1 tool call (since the watermark) => below
        // the configured activity gate of two.
        let one = assistant_tools(1);
        let history = vec![watermark.clone(), one];
        assert!(!ex.should_extract(&history));

        // Add another tool call after the watermark to reach the same gate.
        let history2 = vec![watermark, assistant_tools(1), assistant_tools(1)];
        assert!(ex.should_extract(&history2));
    }

    #[test]
    fn zero_token_gates_allow_natural_no_tool_assistant_turns() {
        let mut ex = SessionMemoryExtractor::new(enabled_config(0, 0, 3));
        let tool_turn = assistant_tools(1);
        assert!(!ex.should_extract(std::slice::from_ref(&tool_turn)));

        let natural_turn = ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: "done".to_string(),
                citations: None,
            }],
            stop_reason: Some("end_turn".to_string()),
        };
        let history = vec![tool_turn, natural_turn];
        assert!(ex.should_extract(&history));

        ex.mark_extracted_through(Some(history[0].id()));
        assert!(ex.should_extract(&history));
        assert_eq!(ex.pending_tool_calls(), 0);
    }

    #[test]
    fn token_gates_match_claude_initialization_and_activity_rules() {
        let mut ex = SessionMemoryExtractor::new(enabled_config(5, 3, 2));
        let short = vec![user("1234567890123456")]; // 4 rough tokens.
        assert!(!ex.should_extract(&short));
        assert!(!ex.session_memory_initialized);

        // Crossing the initialization gate marks the feature initialized, but
        // one tool call is still below the activity threshold.
        let mut one_call = vec![user("12345678901234567890")]; // 5 tokens.
        one_call.push(assistant_tools(1));
        assert!(!ex.should_extract(&one_call));
        assert!(ex.session_memory_initialized);

        // A second tool call satisfies the activity gate while the token gate
        // remains anchored to the pre-extraction baseline of zero.
        one_call.push(assistant_tools(1));
        assert!(ex.should_extract(&one_call));
    }

    #[test]
    fn token_growth_is_required_even_when_tool_call_threshold_is_met() {
        let mut ex = SessionMemoryExtractor::new(enabled_config(1, 5, 1));
        let watermark = user("watermark");
        let base = vec![watermark.clone(), assistant_tools(1)];
        ex.mark_extracted_through(Some(watermark.id()));
        ex.record_extraction_token_count(token_count_with_estimation(&base));

        // The new call reaches the activity gate, but adds fewer than five
        // tokens, so Claude's always-required growth gate keeps extraction off.
        let small_growth = vec![watermark.clone(), assistant_tools(1), assistant_tools(1)];
        assert!(token_count_with_estimation(&small_growth) < ex.tokens_at_last_extraction() + 5);
        assert!(!ex.should_extract(&small_growth));

        // A sufficiently large user turn crosses the growth gate while the
        // earlier tool calls still satisfy the activity gate.
        let large_text = "x".repeat(20);
        let large_growth = vec![
            watermark,
            assistant_tools(1),
            assistant_tools(1),
            user(&large_text),
        ];
        assert!(token_count_with_estimation(&large_growth) >= ex.tokens_at_last_extraction() + 5);
        assert!(ex.should_extract(&large_growth));
    }

    #[test]
    fn token_growth_allows_a_natural_no_tool_assistant_turn() {
        let mut ex = SessionMemoryExtractor::new(enabled_config(1, 5, 99));
        let watermark = user("watermark");
        let base = vec![watermark.clone(), assistant_tools(1)];
        ex.mark_extracted_through(Some(watermark.id()));
        ex.record_extraction_token_count(token_count_with_estimation(&base));

        let large_text = "x".repeat(20);
        let natural_turn = vec![
            watermark,
            assistant_tools(1),
            user(&large_text),
            ConversationMessage::Assistant { per_turn_effort: None,
                id: MessageId::new(),
                content: vec![ContentBlock::Text {
                    text: "done".to_string(),
                    citations: None,
                }],
                stop_reason: Some("end_turn".to_string()),
            },
        ];
        assert!(token_count_with_estimation(&natural_turn) >= ex.tokens_at_last_extraction() + 5);
        assert!(ex.should_extract(&natural_turn));
    }

    #[test]
    fn count_tool_calls_since_counts_after_watermark_only() {
        let before = assistant_tools(5);
        let mark = user("mark");
        let after = assistant_tools(2);
        let history = vec![before, mark.clone(), after];
        // Everything (None watermark) counts both assistant turns.
        assert_eq!(count_tool_calls_since(&history, None), 7);
        // Only what comes after `mark` counts.
        assert_eq!(count_tool_calls_since(&history, Some(&mark.id())), 2);
    }

    #[test]
    fn missing_watermark_preserves_pending_progress_across_compaction() {
        let mut ex = SessionMemoryExtractor::new(enabled_config(0, 0, 3));
        let watermark = user("watermark");
        ex.mark_extracted_through(Some(watermark.id()));

        let pre_compact = vec![watermark, assistant_tools(1), assistant_tools(1)];
        assert!(!ex.should_extract(&pre_compact));
        assert_eq!(ex.pending_tool_calls(), 2);

        // A retained tool turn keeps the activity gate active; its calls are
        // already covered by the carried progress and must not be recounted.
        let compacted = vec![assistant_tools(1), user("compacted tail")];
        assert!(!ex.should_extract(&compacted));
        assert_eq!(ex.pending_tool_calls(), 2);
        assert_eq!(
            ex.post_compaction_anchor_message_id,
            compacted.last().map(ConversationMessage::id)
        );

        let mut next = compacted;
        next.push(assistant_tools(1));
        assert!(ex.should_extract(&next));
        assert_eq!(ex.pending_tool_calls(), 3);
    }

    #[test]
    fn record_compaction_boundary_resumes_from_compacted_tail() {
        let mut ex = SessionMemoryExtractor::new(enabled_config(0, 0, 3));
        let watermark = user("watermark");
        let compacted_tail = assistant_tools(1);
        ex.mark_extracted_through(Some(watermark.id()));
        let revision_before_compaction = ex.state_revision();
        ex.record_compaction_boundary(Some(compacted_tail.id()), 2);

        assert_ne!(
            ex.state_revision(),
            revision_before_compaction,
            "an in-flight extraction must reject its pre-compaction state snapshot"
        );

        assert!(!ex.should_extract(std::slice::from_ref(&compacted_tail)));
        assert_eq!(ex.pending_tool_calls(), 2);

        let after = vec![compacted_tail, assistant_tools(1)];
        assert!(ex.should_extract(&after));
        assert_eq!(ex.pending_tool_calls(), 3);
        assert!(
            ex.should_extract(&after),
            "re-checking unchanged history is stable"
        );
        assert_eq!(ex.pending_tool_calls(), 3, "must not double-count the tail");
    }

    #[test]
    fn session_memory_path_uses_agents_session_memory_layout() {
        let p = session_memory_path(Path::new("/cfg"), "abc123");
        assert_eq!(p, PathBuf::from("/cfg/agents/session-memory/abc123.md"));
    }

    #[tokio::test]
    async fn extract_writes_file_and_advances_watermark() {
        use async_trait::async_trait;
        use sidequery::{SideQueryClient, SideQueryError, SideQueryRequest, SideQueryResponse};
        use tool_api::context::ToolUseOptions;

        // A side-query client that returns the distilled notes verbatim.
        struct Client;
        #[async_trait]
        impl SideQueryClient for Client {
            async fn query(
                &self,
                _req: SideQueryRequest,
            ) -> Result<SideQueryResponse, SideQueryError> {
                Ok(SideQueryResponse {
                    text: Some("DISTILLED NOTES".into()),
                    structured: None,
                    tool_calls: Vec::new(),
                    usage: cost::Usage::default(),
                    stop_reason: Some("end_turn".into()),
                    retry_count: 0,
                })
            }
        }

        let runner = ForkedAgentRunner::new()
            .with_side_query_client(Arc::new(Client), "claude-haiku-4-5".into());

        let cache_safe = CacheSafeParams {
            system_prompt: Arc::from("SYS"),
            user_context: std::collections::HashMap::new(),
            system_context: std::collections::HashMap::new(),
            user_context_message: None,
            tool_use_options: ToolUseOptions {
                debug: false,
                verbose: false,
                main_loop_model: "claude-haiku-4-5".into(),
                model_profile: None,
                max_budget_nano_usd: None,
                mcp_clients: vec![],
                is_non_interactive_session: false,
                custom_system_prompt: None,
                append_system_prompt: None,
            },
            tools: Vec::new(),
            effort: None,
            fork_context_messages: vec![],
            transcript_path: None,
            generation: 1,
        };

        let dir = tempfile::tempdir().unwrap();
        let mut ex = SessionMemoryExtractor::new(enabled_config(0, 0, 1));
        let history = vec![assistant_tools(1), user("last")];
        let last_id = history.last().unwrap().id();

        let written = ex
            .extract(&runner, cache_safe, "sess-xyz", &history, dir.path())
            .await
            .expect("extract succeeds");

        assert!(written.starts_with("---\n"));
        let (frontmatter, body) = parse_markdown_with_frontmatter(&written).unwrap();
        assert_eq!(frontmatter.memory_type, "session_memory");
        assert_eq!(frontmatter.description, "DISTILLED NOTES");
        assert_eq!(body, "DISTILLED NOTES");
        // File landed under agents/session-memory/<id>.md.
        let path = session_memory_path(dir.path(), "sess-xyz");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
        // Watermark advanced to the last message so the next window starts fresh.
        assert_eq!(ex.last_extracted_message_id, Some(last_id));
        assert!(ex.is_initialized());
        assert_eq!(ex.pending_tool_calls(), 0);
    }

    #[test]
    fn session_memory_path_sanitizes_display_ids_for_all_platforms() {
        let path = session_memory_path(
            Path::new("/cfg"),
            "sess:01234567-89ab-cdef-0123-456789abcdef",
        );
        assert_eq!(
            path,
            PathBuf::from("/cfg/agents/session-memory/01234567-89ab-cdef-0123-456789abcdef.md")
        );
    }

    #[test]
    fn normalize_session_memory_document_adds_selector_metadata_to_plain_output() {
        let normalized =
            normalize_session_memory_document("# Project convention\nUse cargo test.\n");
        let (frontmatter, body) = parse_markdown_with_frontmatter(&normalized).unwrap();
        assert_eq!(frontmatter.memory_type, "session_memory");
        assert_eq!(frontmatter.description, "Project convention");
        assert!(frontmatter.when_to_use.is_some());
        assert_eq!(frontmatter.tags, vec!["session"]);
        assert_eq!(body, "# Project convention\nUse cargo test.\n");
    }

    #[tokio::test]
    async fn extract_with_coverage_only_advances_to_visible_watermark() {
        use async_trait::async_trait;
        use sidequery::{SideQueryClient, SideQueryError, SideQueryRequest, SideQueryResponse};
        use tool_api::context::ToolUseOptions;

        struct Client;
        #[async_trait]
        impl SideQueryClient for Client {
            async fn query(
                &self,
                _req: SideQueryRequest,
            ) -> Result<SideQueryResponse, SideQueryError> {
                Ok(SideQueryResponse {
                    text: Some("DISTILLED NOTES".into()),
                    structured: None,
                    tool_calls: Vec::new(),
                    usage: cost::Usage::default(),
                    stop_reason: Some("end_turn".into()),
                    retry_count: 0,
                })
            }
        }

        let runner = ForkedAgentRunner::new()
            .with_side_query_client(Arc::new(Client), "claude-haiku-4-5".into());
        let cache_safe = CacheSafeParams {
            system_prompt: Arc::from("SYS"),
            user_context: std::collections::HashMap::new(),
            system_context: std::collections::HashMap::new(),
            user_context_message: None,
            tool_use_options: ToolUseOptions {
                debug: false,
                verbose: false,
                main_loop_model: "claude-haiku-4-5".into(),
                model_profile: None,
                max_budget_nano_usd: None,
                mcp_clients: vec![],
                is_non_interactive_session: false,
                custom_system_prompt: None,
                append_system_prompt: None,
            },
            tools: Vec::new(),
            effort: None,
            fork_context_messages: vec![],
            transcript_path: None,
            generation: 1,
        };

        let dir = tempfile::tempdir().unwrap();
        let mut ex = SessionMemoryExtractor::new(enabled_config(0, 0, 1));
        let visible = user("visible");
        let unseen = user("unseen");

        ex.extract_with_coverage(
            &runner,
            cache_safe,
            "sess-xyz",
            Some(visible.id()),
            dir.path(),
        )
        .await
        .expect("extract succeeds");

        assert_eq!(ex.extraction_watermark(), Some(&visible.id()));
        assert_ne!(ex.extraction_watermark(), Some(&unseen.id()));
        assert_eq!(ex.pending_tool_calls(), 0);
    }

    #[test]
    fn atomic_write_rejects_a_directory_destination() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blocked.md");
        std::fs::create_dir(&path).unwrap();

        write_session_memory(&path, "notes").expect_err("directory replacement must fail");
        assert!(path.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sess.md");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        write_session_memory(&path, "new").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }

    #[test]
    fn config_home_dir_honors_override_then_home() {
        // We avoid mutating real process env across threads beyond a scoped check.
        let prev = std::env::var_os(branding::CONFIG_DIR_ENV);
        std::env::set_var(branding::CONFIG_DIR_ENV, "/explicit/cfg");
        assert_eq!(config_home_dir(), PathBuf::from("/explicit/cfg"));
        // A set-but-EMPTY string is honored verbatim (claude-code `??`).
        std::env::set_var(branding::CONFIG_DIR_ENV, "");
        assert_eq!(config_home_dir(), PathBuf::from(""));
        match prev {
            Some(v) => std::env::set_var(branding::CONFIG_DIR_ENV, v),
            None => std::env::remove_var(branding::CONFIG_DIR_ENV),
        }
    }
}
