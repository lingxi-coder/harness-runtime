//! Transcript serialization and tool/hook persistence side channels.

use super::*;

/// Metadata of the exact task whose scheduled fire is being recorded.
pub struct ScheduledLoopFire {
    pub fire_id: protocol::MessageId,
    /// Identity assigned when the task was scheduled.
    pub task_id: String,
    /// Exact cron expression of the fired task.
    pub cron: String,
    /// Display prompt, with default loop sentinels already resolved.
    pub prompt: String,
    /// Whether the task has the upstream `kind: "loop"` discriminator.
    pub task_kind_loop: bool,
}

impl ConversationOrchestrator {
    /// Persist Claude's fire envelope and separate meta user companion while
    /// holding the same ordering gate as foreground transcript writes.
    pub async fn append_scheduled_loop_wakeup(
        &self,
        message: String,
        companion: Option<String>,
        streak: u32,
        since_ms: u64,
        fire: ScheduledLoopFire,
    ) -> Result<(), platform_api::HandleError> {
        let _turn = self.turn_gate.lock().await;
        let session_id = self.session.lock().await.session_id;
        let mut payload = serde_json::json!({"message": message, "companion": companion,
            "streak": streak, "since_ms": since_ms, "taskId": fire.task_id,
            "cron": fire.cron, "prompt": scheduled_fire_prompt(&fire.prompt),
            "taskKindLoop": fire.task_kind_loop});
        if streak > 0 {
            let mut uuids = Vec::<String>::new();
            if let Some(writer) = &self.transcript.jsonl_writer {
                if let Ok(file) = writer
                    .filesystem_handle()
                    .read_file(&writer.active_path().to_string_lossy(), None, None)
                    .await
                {
                    let rows: Vec<serde_json::Value> = file
                        .content
                        .lines()
                        .filter_map(|line| serde_json::from_str(line).ok())
                        .collect();
                    if let Some(start) = rows.iter().rposition(|row| {
                        row["type"] == "system" && row["subtype"] == "scheduled_task_fire"
                    }) {
                        uuids = rows[start..]
                            .iter()
                            .filter_map(|row| row["uuid"].as_str().map(str::to_owned))
                            .collect();
                    }
                }
            }
            payload["foldedUuids"] = serde_json::json!(uuids);
        }
        let record = ConversationMessage::System {
            id: fire.fire_id,
            content: payload.to_string(),
            subtype: Some("scheduled_task_fire".into()),
            compact_metadata: None,
            refusal_fallback: None,
        };
        let result = self
            .persist_scheduled_record(&record, session_id, false)
            .await;
        {
            let mut session = self.session.lock().await;
            session.model_context_excluded_messages.insert(record.id());
            session.history.push(record);
        }
        if let Some(text) = companion {
            let row = ConversationMessage::user_meta(protocol::MessageId::new(), text);
            let companion_result = if result.is_ok() {
                self.persist_scheduled_record(&row, session_id, true).await
            } else {
                Ok(())
            };
            self.session.lock().await.history.push(row);
            result?;
            companion_result?;
        } else {
            result?;
        }
        Ok(())
    }

    async fn persist_scheduled_record(
        &self,
        record: &ConversationMessage,
        session_id: protocol::SessionId,
        companion: bool,
    ) -> Result<(), platform_api::HandleError> {
        let Some(writer) = &self.transcript.jsonl_writer else {
            return Ok(());
        };
        let mut row = self.to_jsonl_message_with_inner_id(
            record,
            &session_id.as_uuid().to_string(),
            self.transcript.last_jsonl_uuid.lock().await.clone(),
            self.resolve_git_branch().await,
            Some(entrypoint_value()),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        if companion {
            row.extra
                .insert("turnCompanion".into(), serde_json::json!(true));
            if let ConversationMessage::User { content, .. } = record {
                let text = content
                    .iter()
                    .filter_map(|block| match block {
                        protocol::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                row.message = serde_json::json!({"role":"user", "content":text});
            }
        }
        writer.append(&row).await.map_err(|error| {
            platform_api::HandleError::ActionFailed(format!(
                "could not persist scheduled fire: {error}"
            ))
        })?;
        *self.transcript.last_jsonl_uuid.lock().await = Some(row.uuid);
        Ok(())
    }

    /// Build a deterministic assistant-block `uuid` for write-side per-block
    /// persistence.
    ///
    /// Streaming persists one JSONL line per content block (`content_block_stop`);
    /// this helper derives the outer `uuid` from the turn `inner_id`, zero-based
    /// block index, parent chain pointer, and a canonical per-block payload
    /// signature. This keeps the per-block chain reproducible while preserving
    /// a syntactically valid UUID v4-style outer shape.
    fn assistant_block_derived_uuid(
        turn_id: &str,
        block_index: usize,
        parent_uuid: Option<&str>,
        block: &protocol::ContentBlock,
    ) -> String {
        let block_index = u64::try_from(block_index).unwrap_or(u64::MAX);
        let parent_uuid = parent_uuid.unwrap_or("root");
        let block_signature = Self::assistant_block_signature(block);
        let mut hasher = Sha256::new();

        hasher.update(b"lingxi-assistant-block-v1");
        hasher.update(turn_id.as_bytes());
        hasher.update(b"|");
        hasher.update(block_index.to_le_bytes());
        hasher.update(b"|");
        hasher.update(parent_uuid.as_bytes());
        hasher.update(b"|");
        hasher.update(block_signature.as_bytes());
        let digest = hasher.finalize();

        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        uuid::Uuid::from_bytes(bytes).to_string()
    }

    fn assistant_block_signature(block: &protocol::ContentBlock) -> String {
        let mut payload = serde_json::to_value(block).unwrap_or(serde_json::Value::Null);
        Self::sort_json_object_keys(&mut payload);
        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
    }

    fn sort_json_object_keys(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(object) => {
                let mut entries: Vec<(String, serde_json::Value)> =
                    std::mem::take(object).into_iter().collect();
                for (_, val) in entries.iter_mut() {
                    Self::sort_json_object_keys(val);
                }
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                for (key, val) in entries {
                    object.insert(key, val);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    Self::sort_json_object_keys(value);
                }
            }
            serde_json::Value::String(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::Bool(_)
            | serde_json::Value::Null => {}
        }
    }

    /// Seed the JSONL parent-uuid chain pointer so the FIRST append after a
    /// resume chains via `parent_uuid` off the resumed transcript's tail
    /// (matching the M5-07 writer's chain semantics). Used by the CLI's
    /// resume-into-TUI seed alongside adopting the resumed history + id; without
    /// it the first appended message would be a chain orphan (recoverable, but
    /// this keeps the on-disk chain linear).
    /// Emit a `tool_result` SDK frame, or buffer it when the streaming driver
    /// has ordering active. Every dispatch-side emission goes through here.
    pub(crate) async fn emit_tool_result_frame(
        &self,
        id: &protocol::ToolUseId,
        tool: &str,
        model_text: &str,
        result: &serde_json::Value,
        denial_kind: Option<&str>,
    ) {
        if let Some(buf) = self.transcript.tool_frames.lock().await.as_mut() {
            buf.insert(
                id.to_string(),
                PendingToolFrame {
                    tool: tool.to_string(),
                    model_text: model_text.to_string(),
                    result: result.clone(),
                    denial_kind: denial_kind.map(str::to_string),
                },
            );
            return;
        }
        match denial_kind {
            Some(kind) => {
                self.output
                    .emit_tool_result_denied(id, tool, model_text, result, kind)
                    .await;
            }
            None => {
                self.output
                    .emit_tool_result(id, tool, model_text, result)
                    .await
            }
        }
    }

    /// Turn frame buffering on for the streaming driver, and off again.
    pub(crate) async fn set_tool_frame_buffering(&self, on: bool) {
        let abandoned = {
            let mut slot = self.transcript.tool_frames.lock().await;
            let old = std::mem::replace(&mut *slot, on.then(std::collections::HashMap::new));
            old.into_iter()
                .flat_map(|frames| frames.into_keys())
                .collect::<Vec<_>>()
        };
        // A non-empty old buffer means the stream terminated before those
        // results reached the received-order release/persist point. Discard
        // their parallel metadata too so a failed iteration cannot leak it
        // for the lifetime of the session.
        if !abandoned.is_empty() {
            let mut results = self.transcript.tool_use_results.lock().await;
            let mut denials = self.transcript.tool_denial_kinds.lock().await;
            let mut mcp_meta = self.transcript.tool_use_mcp_meta.lock().await;
            let mut turn_end = self.transcript.pending_tool_result_turn_end.lock().await;
            let mut sources = self.transcript.tool_source_assistant_uuids.lock().await;
            for id in abandoned {
                results.remove(&id);
                denials.remove(&id);
                mcp_meta.remove(&id);
                turn_end.remove(&id);
                sources.remove(&id);
            }
        }
    }

    /// Release one buffered frame, in the CALLER's order.
    ///
    /// `content` is the block's FINAL model-facing text, so a synthetic that
    /// replaced a cancelled tool's real outcome wins over whatever the dispatch
    /// buffered. A tool that never dispatched (queued, then cancelled) has no
    /// buffered frame and still gets one, which is the case that previously
    /// emitted nothing at all.
    pub(crate) async fn release_tool_frame(
        &self,
        id: &protocol::ToolUseId,
        tool: &str,
        content: &str,
        is_error: bool,
    ) {
        let id_key = id.to_string();
        let pending = self
            .transcript
            .tool_frames
            .lock()
            .await
            .as_mut()
            .and_then(|b| b.remove(&id_key));
        let result_from_side_table = self
            .transcript
            .tool_use_results
            .lock()
            .await
            .get(&id_key)
            .cloned();
        let denial_kind_from_side_table = self
            .transcript
            .tool_denial_kinds
            .lock()
            .await
            .get(&id_key)
            .cloned();
        let substituted = pending.as_ref().is_some_and(|p| p.model_text != content);
        let (tool, pending_result, pending_denial_kind) = match pending {
            // A substitution replaced the model-facing text, so the buffered
            // payload describes an outcome that was DISCARDED. claude-code's
            // synthetic carries a synthetic `toolUseResult` too, so the real
            // one must not reach the SDK.
            Some(p) => (p.tool, Some(p.result), p.denial_kind),
            // Never dispatched: synthesize the payload the dispatch would have
            // carried, matching the shape used by every other error result.
            None => (tool.to_string(), None, None),
        };
        let result = result_from_side_table
            .or_else(|| if substituted { None } else { pending_result })
            .unwrap_or_else(|| serde_json::json!({ "error": content }));
        let result = if is_error && !result.is_object() {
            serde_json::json!({ "error": content })
        } else {
            result
        };
        let denial_kind = denial_kind_from_side_table.or(pending_denial_kind);
        match denial_kind {
            Some(kind) => {
                self.output
                    .emit_tool_result_denied(id, &tool, content, &result, &kind)
                    .await;
            }
            None => {
                self.output
                    .emit_tool_result(id, &tool, content, &result)
                    .await
            }
        }
    }

    /// Record the `toolDenialKind` for a tool that was denied rather than run,
    /// so its `tool_result` user line carries the provenance when persisted.
    ///
    /// Values are claude's: `user-rejected`, `permission-rule`,
    /// `automode-blocked`, `automode-unavailable`, `automode-parsing-error`,
    /// plus the abort kinds `cancelled` / `interrupted`.
    pub(crate) async fn record_tool_denial_kind(&self, id: &protocol::ToolUseId, kind: &str) {
        // The `/loop` fold's `tool_denial` / `tool_abort` vetoes. This is the
        // single funnel every denial passes through, so counting here cannot
        // miss one the way a per-call-site count could.
        self.turn_span.note_denial(kind);
        self.transcript
            .tool_denial_kinds
            .lock()
            .await
            .insert(id.to_string(), kind.to_string());
    }

    /// Record a refused tool call for the stream-json `result` frame's
    /// `permission_denials` (oracle schema `LF`).
    ///
    /// Recorded at the SAME funnel as [`Self::record_tool_denial_kind`], and for
    /// the same reason its doc gives: every denial — rule, mode, plan,
    /// classifier, hook override, prompt-transport reject — passes through here,
    /// so a count taken here cannot miss one. Deriving the list from the
    /// `permission_denied` system event instead would miss the three cases
    /// claude-code's own schema doc says that event does not cover.
    ///
    /// Session-scoped and append-only: the result frame reports the whole run.
    pub(crate) async fn record_permission_denial(
        &self,
        tool_name: &str,
        id: &protocol::ToolUseId,
        tool_input: &serde_json::Value,
    ) {
        self.transcript
            .permission_denials
            .lock()
            .await
            .push(platform_api::PermissionDenial {
                tool_name: tool_name.to_string(),
                tool_use_id: id.to_string(),
                tool_input: tool_input.clone(),
            });
    }

    /// Every tool call refused this session, in order — read by the stream-json
    /// result builders.
    pub async fn permission_denials(&self) -> Vec<platform_api::PermissionDenial> {
        self.transcript.permission_denials.lock().await.clone()
    }

    /// Share the denial cell itself, so a transport can read the live list when
    /// it builds its terminal frame instead of being handed a snapshot it might
    /// take at the wrong moment (or forget to take at one emit site out of six).
    #[must_use]
    pub fn permission_denials_handle(
        &self,
    ) -> std::sync::Arc<tokio::sync::Mutex<Vec<platform_api::PermissionDenial>>> {
        std::sync::Arc::clone(&self.transcript.permission_denials)
    }

    /// Take the recorded kind for a message carrying EXACTLY ONE `tool_result`.
    ///
    /// The single-block guard is claude's own (`Tpr`): a user message with zero
    /// or several tool_results cannot attribute one message-level kind, so it
    /// gets none. Taking (rather than reading) keeps a denial from stamping a
    /// second line if the same result were ever persisted twice.
    async fn take_tool_denial_kind(&self, msg: &ConversationMessage) -> Option<String> {
        let only = Self::sole_tool_result_id(msg)?;
        self.transcript.tool_denial_kinds.lock().await.remove(&only)
    }

    /// claude's `Tpr` guard, factored out so every tool-result head key shares
    /// ONE definition: the id of the message's `tool_result` block when it
    /// carries EXACTLY ONE, else `None`.
    pub(super) fn sole_tool_result_id(msg: &ConversationMessage) -> Option<String> {
        let ConversationMessage::User { content, .. } = msg else {
            return None;
        };
        let mut results = content.iter().filter_map(|b| match b {
            protocol::ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id),
            _ => None,
        });
        match (results.next(), results.next()) {
            (Some(only), None) => Some(only.to_string()),
            _ => None,
        }
    }

    /// Record a tool's `toolUseResult` payload for its `tool_result` user line.
    ///
    /// `data` is claude's `se.data` on success (2.1.220 BIN off 235420375) —
    /// the RAW structured result, not the model-facing string — or the plain
    /// string `` `Error: ${message}` `` on the error/denial arms
    /// (BIN off 235424595 / 235400200 / 232972524 / …).
    pub(crate) async fn record_tool_use_result(
        &self,
        id: &protocol::ToolUseId,
        data: serde_json::Value,
    ) {
        self.transcript
            .tool_use_results
            .lock()
            .await
            .insert(id.to_string(), data);
    }

    /// Record an MCP tool's `mcpMeta` for its `tool_result` user line
    /// (2.1.220 BIN off 232969604 — verbatim on the main chain).
    pub(crate) async fn record_tool_use_mcp_meta(
        &self,
        id: &protocol::ToolUseId,
        meta: serde_json::Value,
    ) {
        self.transcript
            .tool_use_mcp_meta
            .lock()
            .await
            .insert(id.to_string(), meta);
    }

    /// Record that a successful tool result should end the current turn once
    /// its persisted/tool-hook boundary has completed.
    pub(crate) async fn record_pending_tool_result_turn_end(
        &self,
        id: &protocol::ToolUseId,
        turn_end: tool_api::tool_trait::ToolResultTurnEnd,
    ) {
        self.transcript
            .pending_tool_result_turn_end
            .lock()
            .await
            .insert(id.to_string(), turn_end);
    }

    /// Drop result metadata whose real tool outcome was replaced by a
    /// streaming synthetic. The synthetic is an error result and therefore
    /// carries neither the real MCP metadata nor its turn-end request.
    pub(crate) async fn clear_discarded_tool_result_metadata(&self, id: &protocol::ToolUseId) {
        let key = id.to_string();
        self.transcript.tool_use_mcp_meta.lock().await.remove(&key);
        self.transcript
            .pending_tool_result_turn_end
            .lock()
            .await
            .remove(&key);
    }

    /// Peek at the requesting assistant line for a result without consuming
    /// the value that transcript serialization must still write as
    /// `sourceToolAssistantUUID`.
    pub(crate) async fn source_tool_assistant_uuid(
        &self,
        id: &protocol::ToolUseId,
    ) -> Option<String> {
        self.transcript
            .tool_source_assistant_uuids
            .lock()
            .await
            .get(id.as_str())
            .cloned()
    }

    /// Queue one hook `attachment` payload produced while dispatching `id`.
    ///
    /// Flushed by [`Self::flush_hook_attachments`] right after that tool's
    /// `tool_result` line is written, which is where claude's own stream order
    /// puts it.
    pub(crate) async fn queue_hook_attachment(
        &self,
        id: &protocol::ToolUseId,
        payload: serde_json::Value,
    ) {
        self.transcript
            .pending_hook_attachments
            .lock()
            .await
            .entry(id.to_string())
            .or_default()
            .push(payload);
    }

    /// Persist (and drain) every attachment queued for `id`.
    pub(crate) async fn flush_hook_attachments(&self, id: &protocol::ToolUseId) {
        for payload in self.take_queued_hook_attachments(id).await {
            self.persist_hook_attachment_to_jsonl(payload).await;
        }
    }

    /// Take the recorded `toolUseResult` under the same single-block guard.
    async fn take_tool_use_result(&self, msg: &ConversationMessage) -> Option<serde_json::Value> {
        let only = Self::sole_tool_result_id(msg)?;
        self.transcript.tool_use_results.lock().await.remove(&only)
    }

    /// Take the recorded `mcpMeta` under the same single-block guard.
    async fn take_tool_use_mcp_meta(&self, msg: &ConversationMessage) -> Option<serde_json::Value> {
        let only = Self::sole_tool_result_id(msg)?;
        self.transcript.tool_use_mcp_meta.lock().await.remove(&only)
    }

    /// Whether this `tool_result` user message should persist `toolEndsTurn`.
    /// MCP `_meta` termination is represented solely by `mcpMeta`; the oracle
    /// writes this sibling only for a native `ToolResult.endsTurn`.
    async fn tool_result_message_ends_turn(&self, msg: &ConversationMessage) -> bool {
        let Some(only) = Self::sole_tool_result_id(msg) else {
            return false;
        };
        self.transcript
            .pending_tool_result_turn_end
            .lock()
            .await
            .get(&only)
            .is_some_and(|turn_end| {
                turn_end.source == tool_api::tool_trait::ToolResultTurnEndSource::Tool
            })
    }

    /// Drain pending tool-result turn-end requests for the supplied ids.
    ///
    /// Claude Code stores one `toolRequestedEndTurn` scalar and overwrites it
    /// whenever a later result also requests termination. Returning the last
    /// matching id therefore preserves both its source and the one-event
    /// telemetry cardinality for concurrent batches.
    pub(crate) async fn take_pending_tool_result_turn_ends(
        &self,
        ids: &[protocol::ToolUseId],
    ) -> Option<tool_api::tool_trait::ToolResultTurnEnd> {
        let mut pending = self.transcript.pending_tool_result_turn_end.lock().await;
        let mut selected = None;
        for id in ids {
            if let Some(turn_end) = pending.remove(&id.to_string()) {
                selected = Some(turn_end);
            }
        }
        selected
    }

    pub async fn seed_last_jsonl_uuid(&self, last_uuid: Option<String>) {
        *self.transcript.last_jsonl_uuid.lock().await = last_uuid;
    }

    /// Convert an in-memory `ConversationMessage` into a `JsonlMessage`.
    ///
    /// `parent_uuid` is the UUID of the prior persisted entry (None for the
    /// first turn). `cwd` is read from the LIVE `current_cwd()` cell (the
    /// post-`cd` shell cwd; falls back to `self.cwd` when no firer is wired).
    /// The `message` payload
    /// is the Anthropic-shaped inner object: for user/assistant we splat
    /// the content blocks via `serde_json::to_value` of the
    /// `ConversationMessage` and pull out the `content` array.
    ///
    /// Writer-field fidelity (§G gap 4) — mirrors `insertMessageChain`
    /// (`sessionStorage.ts:1039-1064`):
    /// - `git_branch`: the once-per-chain `getBranch()` value (`None` on a
    ///   non-repo), resolved by the caller and threaded in.
    /// - `entrypoint`: `getEntrypoint()` — `"cli"` for this engine (caller-supplied).
    /// - `prompt_id`: `getPromptId()` on `user` lines ONLY; `None` elsewhere. The
    ///   caller passes the in-flight turn's id and we apply it only to `user`.
    /// - `logical_parent_uuid`: compact-boundary back-link. The orchestrator's
    ///   append path is NOT a compaction boundary (compaction replays through a
    ///   separate engine), so this is always `None` here. See the
    ///   `persist_message_to_jsonl` note.
    #[cfg(test)]
    pub(crate) fn to_jsonl_message(
        &self,
        msg: &ConversationMessage,
        session_id: &str,
        parent_uuid: Option<String>,
        git_branch: Option<String>,
        entrypoint: Option<String>,
        prompt_id: Option<String>,
    ) -> session::JsonlMessage {
        self.to_jsonl_message_with_inner_id(
            msg,
            session_id,
            parent_uuid,
            git_branch,
            entrypoint,
            prompt_id,
            None,
            None,
            None,
            None,
            None,
        )
    }

    /// As [`Self::to_jsonl_message`], but allows stamping a shared inner
    /// `message.id` on the persisted line.
    ///
    /// claude-code's streaming writer emits one JSONL line per
    /// `content_block_stop`, each carrying a DISTINCT top-level `uuid` but the
    /// SAME inner Anthropic `message.id` (the `message_start` message id shared
    /// across all blocks of the turn — `claude.ts:1981, 2192-2203`). That shared
    /// inner id is what the loader's parallel-tool-result recovery groups
    /// siblings by (`loader::message_id` → `loader::recover_orphaned_parallel_tool_results`).
    /// When `inner_message_id` is `Some`, it is injected into the assistant
    /// line's inner `message` object as `"id"`. `None` reproduces the prior
    /// (no inner id) shape exactly.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn to_jsonl_message_with_inner_id(
        &self,
        msg: &ConversationMessage,
        session_id: &str,
        parent_uuid: Option<String>,
        git_branch: Option<String>,
        entrypoint: Option<String>,
        prompt_id: Option<String>,
        inner_message_id: Option<&str>,
        // The real-response persist path supplies the response `model` and the
        // raw Anthropic `usage` object, which makes the assistant line carry the
        // full BetaMessage envelope (`{id,type,role,content,model,stop_reason,
        // stop_sequence,usage}`, matching claude-code + the golden fixtures).
        // Both `None` (the synthetic / user / system path) keeps the prior
        // `{role,content}` inner shape.
        assistant_model: Option<&str>,
        assistant_usage: Option<&serde_json::Value>,
        // The Anthropic `request-id` response header for a REAL assistant line
        // → the top-level `requestId` field (via `extra`). `None` (synthetic /
        // user / system) omits it, matching claude-code's `requestId: undefined`.
        request_id: Option<&str>,
        // When `Some`, this is a synthetic api-error assistant line: stamp the
        // top-level `isApiErrorMessage`/`error`/`apiErrorStatus` envelope fields
        // (via `extra`) and apply any inner `stop_reason` override. `None`
        // (every non-api-error line) leaves the shape exactly as before.
        api_error: Option<&ApiErrorEnvelope>,
    ) -> session::JsonlMessage {
        let (kind, mut inner_message) = match msg {
            ConversationMessage::User { content, .. } => (
                "user",
                serde_json::json!({ "role": "user", "content": content }),
            ),
            ConversationMessage::Assistant {
                content,
                stop_reason,
                ..
            } => {
                let inner = if let Some(model) = assistant_model {
                    // Build in the BetaMessage key order (`id` first); the block
                    // below re-stamps the shared `id` idempotently.
                    let mut m = serde_json::Map::new();
                    if let Some(id) = inner_message_id {
                        m.insert("id".to_string(), serde_json::Value::String(id.to_string()));
                    }
                    m.insert(
                        "type".to_string(),
                        serde_json::Value::String("message".to_string()),
                    );
                    m.insert(
                        "role".to_string(),
                        serde_json::Value::String("assistant".to_string()),
                    );
                    m.insert("content".to_string(), serde_json::json!(content));
                    m.insert(
                        "model".to_string(),
                        serde_json::Value::String(model.to_string()),
                    );
                    m.insert(
                        "stop_reason".to_string(),
                        stop_reason
                            .clone()
                            .map_or(serde_json::Value::Null, serde_json::Value::String),
                    );
                    m.insert("stop_sequence".to_string(), serde_json::Value::Null);
                    m.insert(
                        "usage".to_string(),
                        assistant_usage.cloned().unwrap_or(serde_json::Value::Null),
                    );
                    serde_json::Value::Object(m)
                } else {
                    // SYNTHETIC assistant line. LingXi's only synthetic assistant
                    // persist is the terminal API-error line (conversation.rs
                    // ~4399). claude-code 2.1.238 builds it in `Mqm`
                    // (cc-238 @296633254), whose `message` literal is:
                    //   {diagnostics:null, id, container:null, model:yD,
                    //    role:"assistant", stop_details:null,
                    //    stop_reason:"stop_sequence", stop_sequence:"",
                    //    type:"message", usage:l, content, context_management:null}
                    //
                    // SC-05 — the previous note here was read off a **2.1.185**
                    // binary and was wrong for the current oracle on two counts:
                    //   * `diagnostics:null` exists and is the FIRST key.
                    //   * `usage` is NOT omitted. `Mqm`'s `usage` parameter has a
                    //     DEFAULT — a fully zeroed usage object — so the key is
                    //     always serialized; it never reaches `JSON.stringify` as
                    //     `undefined`. The old "usage is dropped" claim came from
                    //     `tc` passing no argument, which selects that default
                    //     rather than omitting the field.
                    // Key ORDER is load-bearing: these envelopes are compared
                    // byte-for-byte against recorded JSONL.
                    //
                    // `stop_reason` stays hardcoded `"stop_sequence"` (the refusal
                    // path overrides it); the real terminal reason lives in the
                    // OUTER apiError/error fields, which claude-code does not
                    // write into the persisted inner message. `model` is the
                    // `<synthetic>` sentinel (`WR`/`yD`).
                    let mut m = serde_json::Map::new();
                    m.insert("diagnostics".to_string(), serde_json::Value::Null);
                    m.insert(
                        "id".to_string(),
                        serde_json::Value::String(
                            inner_message_id
                                .map_or_else(|| msg.id().as_uuid().to_string(), str::to_string),
                        ),
                    );
                    m.insert("container".to_string(), serde_json::Value::Null);
                    m.insert(
                        "model".to_string(),
                        serde_json::Value::String("<synthetic>".to_string()),
                    );
                    m.insert(
                        "role".to_string(),
                        serde_json::Value::String("assistant".to_string()),
                    );
                    m.insert("stop_details".to_string(), serde_json::Value::Null);
                    m.insert(
                        "stop_reason".to_string(),
                        serde_json::Value::String(
                            // `ql`/`tc` leave the synthetic inner `stop_reason` as
                            // `"stop_sequence"`; the refusal `fje` path overrides
                            // it to `"refusal"` (verified on disk).
                            api_error
                                .and_then(|e| e.inner_stop_reason)
                                .unwrap_or("stop_sequence")
                                .to_string(),
                        ),
                    );
                    m.insert(
                        "stop_sequence".to_string(),
                        serde_json::Value::String(String::new()),
                    );
                    m.insert(
                        "type".to_string(),
                        serde_json::Value::String("message".to_string()),
                    );
                    // SC-05: `Mqm`'s default `usage` literal, key order included.
                    // `output_tokens_details` leads and is null — the same field
                    // 2.1.238 grew a `thinking_tokens` member on (see SC-01).
                    m.insert(
                        "usage".to_string(),
                        serde_json::json!({
                            "output_tokens_details": serde_json::Value::Null,
                            "input_tokens": 0,
                            "output_tokens": 0,
                            "cache_creation_input_tokens": 0,
                            "cache_read_input_tokens": 0,
                            "server_tool_use": {
                                "web_search_requests": 0,
                                "web_fetch_requests": 0
                            },
                            "service_tier": serde_json::Value::Null,
                            "cache_creation": {
                                "ephemeral_1h_input_tokens": 0,
                                "ephemeral_5m_input_tokens": 0
                            },
                            "inference_geo": serde_json::Value::Null,
                            "iterations": serde_json::Value::Null,
                            "speed": serde_json::Value::Null
                        }),
                    );
                    m.insert("content".to_string(), serde_json::json!(content));
                    m.insert("context_management".to_string(), serde_json::Value::Null);
                    // `stop_reason` from the ConversationMessage is intentionally
                    // not used here (the synthetic envelope hardcodes it).
                    let _ = stop_reason;
                    serde_json::Value::Object(m)
                };
                ("assistant", inner)
            }
            ConversationMessage::System { content, .. } => (
                "system",
                serde_json::json!({ "role": "system", "content": content }),
            ),
        };
        // Stamp the shared inner Anthropic `message.id` on assistant lines so the
        // loader's sibling-grouping (by inner `message.id`) reconstructs the DAG.
        if let (Some(id), Some(obj)) = (inner_message_id, inner_message.as_object_mut()) {
            obj.insert("id".to_string(), serde_json::Value::String(id.to_string()));
        }
        // `promptId` is a USER-line-only field (TS: `type === 'user' ?
        // getPromptId() : undefined`). Drop it on assistant/system lines even
        // when the caller passes one.
        let prompt_id = if kind == "user" { prompt_id } else { None };
        // Use the raw UUID (8-4-4-4-12 lowercase), NOT the `msg.id().to_string()`
        // form which carries the `"msg:"` prefix — that prefix would break the
        // byte-equivalent JSONL schema (see `JsonlMessage::uuid` doc) and the
        // `validate_uuid` regex.
        //
        // `isMeta` is a TOP-LEVEL envelope field in claude-code (a sibling of
        // `message`/`uuid`, emitted at `utils/messages.ts:765,810` and read as an
        // outer field at `session/src/jsonl/title.rs:101`). Emit it ONLY for a
        // meta user message (default-`false` is omitted), so normal lines — and
        // every existing golden fixture — keep their exact byte shape.
        let mut extra = serde_json::Map::new();
        if msg.is_meta() {
            extra.insert("isMeta".to_string(), serde_json::Value::Bool(true));
        }
        if let Some(contents) = self
            .transcript
            .post_compact_skill_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&msg.id())
            .cloned()
        {
            extra.insert(
                "invokedSkillContents".to_string(),
                serde_json::json!(contents),
            );
        }
        // Top-level `requestId` (the Anthropic `request-id` response header) —
        // claude-code persists it on REAL assistant lines only. The caller
        // passes `Some` from the per-block real-response path; `None` (synthetic
        // / user / system) omits it, matching `requestId: undefined`.
        if let Some(rid) = request_id {
            extra.insert(
                "requestId".to_string(),
                serde_json::Value::String(rid.to_string()),
            );
        }
        // Top-level `effort` (2.1.212): the session's resolved reasoning-effort
        // LEVEL string. claude-code spreads `...effort!==void 0&&{effort}` (the
        // `Y4n(effort).level`) as the last field of the in-memory assistant
        // message object, which persists verbatim into the transcript record —
        // so it lands on REAL assistant lines only, right after `timestamp` and
        // before the `userType`/`cwd` trailer (the serializer places it there).
        // Gated on a REAL response (`assistant_model.is_some()`) so synthetic
        // api-error assistant lines — which claude builds via a different builder
        // with no effort — stay byte-identical. `None` effort omits the field,
        // matching claude's `!==void 0` guard.
        if kind == "assistant" && assistant_model.is_some() {
            if let Some(effort) = self
                .model_runtime
                .current_effort
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                extra.insert(
                    "effort".to_string(),
                    serde_json::Value::String(effort.clone()),
                );
            }
            let selection = self
                .model_runtime
                .current_reasoning_selection
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if !matches!(selection, platform_api::ReasoningSelection::Automatic) {
                if let Ok(value) = serde_json::to_value(selection) {
                    extra.insert("reasoningSelection".to_string(), value);
                }
            }
        }
        if kind == "assistant" && assistant_model.is_some() {
            if let Some(settings) = crate::scheduled_turn::current() {
                // Actual response settings belong to this turn, not the next
                // human turn's persisted defaults. Replay retains the message
                // and its real attribution while ignoring these defaults.
                extra.insert("perTurnSettings".into(), serde_json::Value::Bool(true));
                extra.remove("effort");
                if let Some(effort) = settings.effort {
                    extra.insert("effort".into(), effort);
                }
                if let Ok(selection) = serde_json::to_value(settings.reasoning) {
                    extra.insert("reasoningSelection".into(), selection);
                }
            }
        }
        // Top-level api-error envelope (`createAssistantAPIErrorMessage`/`fje`):
        // `error` (omitted when the builder took no `error:` arg), the always-on
        // `isApiErrorMessage: true`, and `apiErrorStatus` (set only for an
        // `APIError` with a numeric status). On disk these sit between
        // `requestId` and `userType`. These flow through the `extra` channel;
        // `JsonlMessage`'s hand-written `Serialize` (session/jsonl/schema.rs)
        // now places them in claude's EXACT per-kind outer-key order
        // (api-error head: type, uuid, timestamp, message, requestId?, error?,
        // errorDetails?, truncatedAfterOutput?, isApiErrorMessage,
        // apiErrorStatus?) — presence + values + ORDER are 1:1. See
        // [`ApiErrorEnvelope`].
        if let Some(ae) = api_error {
            if let Some(cat) = ae.error {
                extra.insert(
                    "error".to_string(),
                    serde_json::Value::String(cat.to_string()),
                );
            }
            extra.insert(
                "isApiErrorMessage".to_string(),
                serde_json::Value::Bool(true),
            );
            if ae.truncated_after_output {
                extra.insert(
                    "truncatedAfterOutput".to_string(),
                    serde_json::Value::Bool(true),
                );
            }
            if let Some(status) = ae.api_error_status {
                extra.insert(
                    "apiErrorStatus".to_string(),
                    serde_json::Value::Number(status.into()),
                );
            }
        }
        if let ConversationMessage::System {
            content, subtype, ..
        } = msg
        {
            if subtype.as_deref() == Some("scheduled_task_fire") {
                if let Ok(payload) = serde_json::from_str::<serde_json::Value>(content) {
                    extra.insert("subtype".into(), serde_json::json!("scheduled_task_fire"));
                    extra.insert("content".into(), payload["message"].clone());
                    extra.insert("isMeta".into(), serde_json::json!(false));
                    for key in ["taskId", "cron", "prompt"] {
                        extra.insert(key.into(), payload[key].clone());
                    }
                    if payload["taskKindLoop"] == true {
                        extra.insert("taskKind".into(), serde_json::json!("loop"));
                    }
                    if payload["taskKindLoop"] == true {
                        extra.insert("cronKind".into(), serde_json::json!("loop"));
                    }
                    if payload["streak"].as_u64().unwrap_or(0) > 0 {
                        extra.insert("noOpStreak".into(), payload["streak"].clone());
                        let since = payload["since_ms"].as_i64().unwrap_or(0);
                        if let Some(time) =
                            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(since)
                        {
                            extra.insert(
                                "streakStartedAt".into(),
                                serde_json::json!(
                                    time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                                ),
                            );
                        }
                        if payload["foldedUuids"]
                            .as_array()
                            .is_some_and(|ids| !ids.is_empty())
                        {
                            extra.insert("foldedUuids".into(), payload["foldedUuids"].clone());
                        }
                    }
                    inner_message = serde_json::Value::Null;
                }
            }
        }
        session::JsonlMessage {
            message_type: kind.to_string(),
            uuid: msg.id().as_uuid().to_string(),
            parent_uuid,
            session_id: session_id.to_string(),
            timestamp: chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string(),
            // Per-line cwd readback — the LIVE session cwd (advanced by a Bash
            // `cd` via the shared `current_cwd` cell), NOT the static init cwd.
            // 1:1 with claude-code, which stamps `getCwd()` on every persisted
            // line and where `cd` mutates that single global cwd. Falls back to
            // the static `cwd` when no firer is wired (the cell never moves).
            cwd: self.current_cwd().to_string_lossy().into_owned(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            message: inner_message,
            is_sidechain: false,
            user_type: Some("external".to_string()),
            git_branch,
            entrypoint,
            // Plan-slug cache is not wired in this engine — TS reads
            // `getPlanSlugCache().get(sessionId)`, which is `undefined` for any
            // session without a stored plan slug. We have no such cache, so this
            // is always omitted (matches the common TS path).
            slug: None,
            prompt_id,
            // Always `None` from this append path — see the doc comment above and
            // the `persist_message_to_jsonl` note.
            logical_parent_uuid: None,
            extra,
        }
    }

    /// Resolve the cwd's git branch ONCE and cache it — the parity analog of TS
    /// `getBranch()` (`sessionStorage.ts:1012-1019`), which is called per
    /// `insertMessageChain` and stamped on every line. We resolve lazily on the
    /// first append and memoize, so subsequent appends pay nothing.
    ///
    /// Reuses the loader's shell-git pattern (`std::process::Command`, no new
    /// dependency): `git rev-parse --abbrev-ref HEAD` in `self.cwd`. Returns
    /// `None` on ANY failure (git missing, not a repo, non-zero exit, detached
    /// HEAD reporting `"HEAD"`), matching TS's `try { getBranch() } catch {
    /// undefined }` — a `None` is then omitted from the JSONL line.
    pub(super) async fn resolve_git_branch(&self) -> Option<String> {
        {
            let cache = self.transcript.git_branch_cache.lock().await;
            if let Some(resolved) = cache.as_ref() {
                return resolved.clone();
            }
        }
        let resolved = git_branch_for_cwd(&self.cwd);
        *self.transcript.git_branch_cache.lock().await = Some(resolved.clone());
        resolved
    }

    /// The stable per-turn `promptId` for `msg` — the parity analog of
    /// `getPromptId()` (`sessionStorage.ts:1045`).
    ///
    /// TS stamps the SAME prompt id on the user prompt line AND every
    /// `tool_result` `user` line of the turn. We reproduce that through the
    /// single append chokepoint: a genuine new user prompt (a `user` message
    /// that is NOT a `tool_result` carrier) MINTS a fresh UUID into
    /// `current_prompt_id`; a `tool_result` `user` line REUSES the cached id; any
    /// non-`user` message returns `None` (the caller / `to_jsonl_message` also
    /// guards this, so the field never lands on assistant/system lines).
    async fn prompt_id_for_message(&self, msg: &ConversationMessage) -> Option<String> {
        let ConversationMessage::User { content, .. } = msg else {
            // Non-user line — no promptId (mirrors `type === 'user' ? … :
            // undefined`). Leave the cached turn id untouched.
            return None;
        };
        let is_tool_result_carrier = content
            .iter()
            .any(|b| matches!(b, protocol::ContentBlock::ToolResult { .. }));
        let mut slot = self.prompt_runtime.current_prompt_id.lock().await;
        if is_tool_result_carrier {
            // Continuation of the in-flight turn — reuse the current id. If none
            // exists yet (defensive: a tool_result persisted before any prompt),
            // mint one so the field is still populated.
            if slot.is_none() {
                *slot = Some(uuid::Uuid::new_v4().to_string());
            }
        } else {
            // Genuine new user prompt — start a fresh prompt id for this turn.
            *slot = Some(uuid::Uuid::new_v4().to_string());
        }
        slot.clone()
    }

    /// Persist a single message to the optional JSONL writer.
    ///
    /// Best-effort: write failures are logged via the telemetry
    /// `tengu_session_corrupted` event and emit one sanitized user-visible
    /// warning per session, but never fail the turn. On success, emits
    /// `tengu_session_appended` and updates the `last_jsonl_uuid` cache.
    pub(crate) async fn persist_message_to_jsonl(&self, msg: &ConversationMessage) {
        self.persist_message_to_jsonl_with_parent(msg, None).await;
    }

    /// Append an SDK/stream-json supplied assistant or system history entry to
    /// the live session and its transcript. This is intentionally not routed
    /// through a model turn: the next user message observes the seeded history
    /// exactly once and input ordering remains owned by the caller.
    pub async fn append_external_history_message(&self, msg: ConversationMessage) {
        // SDK history/Bash inputs arrive between model turns, but background
        // Fusion publication and session switches still run concurrently.
        // Own the same gate before choosing a session or transcript parent.
        // Do not acquire it inside persistence helpers: model turns already
        // hold it when calling those helpers.
        let _turn_guard = self.turn_gate.lock().await;
        let compact_metadata = match &msg {
            ConversationMessage::System {
                subtype: Some(subtype),
                compact_metadata: Some(metadata),
                ..
            } if subtype == "compact_boundary" => Some(metadata.clone()),
            _ => None,
        };
        {
            let mut session = self.session.lock().await;
            session.history.push(msg.clone());
        }
        if let Some(metadata) = compact_metadata {
            self.persist_compact_boundary_to_jsonl(&msg, &metadata)
                .await;
        } else {
            self.persist_message_to_jsonl(&msg).await;
        }
    }

    /// Persist with an optional explicit `parentUuid` override.
    ///
    /// The streaming executor passes the originating assistant message's UUID so
    /// each tool result parents to the assistant that requested it (TS
    /// `sourceToolAssistantUUID`), rather than the linear `last_jsonl_uuid` chain.
    ///
    /// When `parent_override` is `None`, behaves exactly as before (chain off
    /// `last_jsonl_uuid`). In BOTH cases the `last_jsonl_uuid` cache is advanced
    /// to this line's UUID so any subsequent non-overridden line chains correctly.
    pub(crate) async fn persist_message_to_jsonl_with_parent(
        &self,
        msg: &ConversationMessage,
        parent_override: Option<String>,
    ) {
        // Android Computer Use screenshots must reach the current model but
        // must not be written to the durable JSONL transcript. The tool marks
        // only those results with `_lingxi_ephemeral`; every existing desktop
        // and mobile result remains byte-identical.
        let sanitized = redact_ephemeral_tool_result_images(msg);
        self.persist_message_to_jsonl_inner(&sanitized, parent_override, None, false)
            .await;
    }

    /// Persist one queued user message's host-only envelope metadata. Keeping
    /// queue metadata out of ConversationMessage keeps it out of model input.
    pub(super) async fn persist_queued_message_to_jsonl(
        &self,
        msg: &ConversationMessage,
        input: &QueuedPromptInput,
    ) {
        let sanitized = redact_ephemeral_tool_result_images(msg);
        self.persist_message_to_jsonl_inner_with_queue_metadata(
            &sanitized,
            None,
            None,
            false,
            Some(input),
        )
        .await;
    }

    /// Persist a synthetic api-error assistant line, stamping the top-level
    /// `isApiErrorMessage`/`error`/`apiErrorStatus` envelope (and any inner
    /// `stop_reason` override) from `env`. 1:1 with claude-code's
    /// `createAssistantAPIErrorMessage` (`ql`/`tc`) and refusal (`fje`) lines.
    pub(crate) async fn persist_api_error_message_to_jsonl(
        &self,
        msg: &ConversationMessage,
        env: ApiErrorEnvelope,
    ) {
        self.persist_message_to_jsonl_inner(msg, None, Some(env), false)
            .await;
    }

    /// Record a best-effort transcript write failure. The raw error remains in
    /// the local diagnostic log; the upstream telemetry event has an empty
    /// payload and must not receive paths, session ids, or backend details.
    pub(super) async fn record_transcript_append_failure(
        &self,
        _session_id: &str,
        operation: &'static str,
        error: &(impl std::fmt::Display + ?Sized),
    ) {
        // CC 2.1.218 logs + emits telemetry on a transcript-append failure but
        // shows NO user-visible notice — the port's `TRANSCRIPT_PERSISTENCE_WARNING`
        // system notice was an invented surface. Keep the log + telemetry only.
        tracing::error!(error = %error, operation, "jsonl writer append failed");
        telemetry::emit_session_persistence_failed();
    }

    /// Persist ONE hook-run `attachment` transcript line.
    ///
    /// claude-code writes exactly one `type:"attachment"` line per hook run
    /// (26 048 such records mined from real 2.1.220 transcripts under
    /// `~/.claude/projects`). The outer envelope puts the payload BEFORE the
    /// discriminator — `parentUuid, isSidechain, attachment, type, uuid,
    /// timestamp, …trailer` — and carries NO inner `message`; that ordering is
    /// implemented by the attachment arm of
    /// [`session::jsonl::schema::JsonlMessage`]'s hand-written `Serialize`.
    ///
    /// `payload` is the value built by [`hooks::attachment`] (`hook_success` /
    /// `hook_non_blocking_error` / `hook_cancelled`). Best-effort like every
    /// other JSONL append; advances `last_jsonl_uuid` on success so the next
    /// line chains off it.
    pub async fn persist_hook_attachment_to_jsonl(&self, payload: serde_json::Value) {
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            return;
        };
        let session_id_str = self.session.lock().await.session_id.to_string();
        let parent_uuid = self.transcript.last_jsonl_uuid.lock().await.clone();
        let git_branch = self.resolve_git_branch().await;
        let mut extra = serde_json::Map::new();
        extra.insert("attachment".to_string(), payload);

        let jmsg = session::JsonlMessage {
            message_type: "attachment".to_string(),
            uuid: uuid::Uuid::new_v4().to_string(),
            parent_uuid,
            session_id: session_id_str.clone(),
            timestamp: chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string(),
            cwd: self.current_cwd().to_string_lossy().into_owned(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            // Attachment lines carry NO inner `message`.
            message: serde_json::Value::Null,
            is_sidechain: false,
            user_type: Some("external".to_string()),
            git_branch,
            entrypoint: Some(entrypoint_value()),
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra,
        };
        let line_uuid = jmsg.uuid.clone();
        match writer.append(&jmsg).await {
            Ok(()) => {
                *self.transcript.last_jsonl_uuid.lock().await = Some(line_uuid.clone());
                telemetry::emit_session_appended(&session_id_str, &line_uuid);
            }
            Err(e) => {
                self.record_transcript_append_failure(&session_id_str, "hook_attachment", &e)
                    .await;
            }
        }
    }

    /// Persist before retry, while the rejected snapshot is still the transcript
    /// tail. The scope owns these handles so lazy streams can await durability
    /// after the caller's task-local scope has ended.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn persist_thinking_recovery_snapshot(
        writer: Arc<JsonlWriter>,
        last_uuid: Arc<Mutex<Option<String>>>,
        session: Arc<Mutex<SessionState>>,
        expected_session: protocol::SessionId,
        cwd: std::path::PathBuf,
        git_branch: Option<String>,
        mut ranges: std::collections::HashMap<MessageId, usize>,
    ) {
        let mut state = session.lock().await;
        // An in-place resume may replace the owning session while an older
        // stream is being cancelled. Its recovery cannot extend the new chain.
        if state.session_id != expected_session {
            return;
        }
        ranges.retain(|id, _| state.history.iter().any(|message| message.id() == *id));
        if !ranges.iter().any(|(id, from)| {
            state
                .thinking_stripped_messages
                .get(id)
                .is_none_or(|current| from < current)
        }) {
            return;
        }
        let mut parent = last_uuid.lock().await;
        let session_id = expected_session.to_string();
        let row = session::JsonlMessage {
            message_type: "attachment".into(),
            uuid: uuid::Uuid::new_v4().to_string(),
            parent_uuid: parent.clone(),
            session_id: session_id.clone(),
            timestamp: chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string(),
            cwd: cwd.to_string_lossy().into_owned(),
            version: env!("CARGO_PKG_VERSION").into(),
            message: serde_json::Value::Null,
            is_sidechain: false,
            user_type: Some("external".into()),
            git_branch,
            entrypoint: Some(entrypoint_value()),
            slug: None,
            prompt_id: None,
            logical_parent_uuid: None,
            extra: [(
                "attachment".into(),
                serde_json::json!({
                    "type": "thinking_stripped", "scope": "all"
                }),
            )]
            .into_iter()
            .collect(),
        };
        match writer.append(&row).await {
            Ok(()) => {
                *parent = Some(row.uuid.clone());
                // Updating the session only after durable append also prevents
                // the normal post-call synchronization from writing a duplicate.
                state.thinking_signature_stripped = true;
                for (id, from) in ranges {
                    state
                        .thinking_stripped_messages
                        .entry(id)
                        .and_modify(|current| *current = (*current).min(from))
                        .or_insert(from);
                }
                telemetry::emit_session_appended(&session_id, &row.uuid);
            }
            Err(error) => {
                tracing::error!(%error, "thinking recovery transcript append failed");
                telemetry::emit_session_persistence_failed();
            }
        }
    }

    /// Atomically persist oversized hook output below this session's
    /// root-confined `tool-results` directory and return the attachment copy.
    pub(crate) async fn persist_large_hook_output(&self, text: &str) -> Option<String> {
        let config_home = self.config_home.clone()?;
        let (session_uuid, cwd) = {
            let session = self.session.lock().await;
            (
                session.session_id.as_uuid().to_string(),
                self.current_cwd().to_string_lossy().into_owned(),
            )
        };
        let relative = std::path::PathBuf::from("projects")
            .join(session::jsonl::path::project_dir_name(&cwd))
            .join(session_uuid)
            .join("tool-results")
            .join(format!("hook-{}.txt", uuid::Uuid::new_v4()));
        let absolute = config_home.join(&relative);
        let bytes = text.as_bytes().to_vec();
        let write = tokio::task::spawn_blocking(move || {
            platform_api::rooted_fs::atomic_write(
                &config_home,
                &relative,
                &bytes,
                platform_api::AtomicWriteOptions {
                    overwrite: false,
                    ..platform_api::AtomicWriteOptions::default()
                },
            )
        })
        .await;
        match write {
            Ok(Ok(())) => Some(format!("(Full output saved to: {})", absolute.display())),
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "failed to persist oversized hook output");
                None
            }
            Err(error) => {
                tracing::warn!(error = %error, "oversized hook output writer task failed");
                None
            }
        }
    }

    /// Retract a rejected attempt from live display, active history and disk.
    pub(crate) async fn discard_retry_attempt(&self, assistant_id: MessageId) {
        self.session
            .lock()
            .await
            .history
            .retain(|message| message.id() != assistant_id);
        self.output.emit_message_retracted(&assistant_id).await;
        if let Some(writer) = self.transcript.jsonl_writer.as_ref() {
            match writer
                .remove_retry_attempt(&assistant_id.as_uuid().to_string())
                .await
            {
                Ok(tail) => *self.transcript.last_jsonl_uuid.lock().await = tail,
                Err(error) => tracing::warn!(%error, "failed to remove rejected retry attempt"),
            }
        }
    }

    /// Persist the latest active-goal snapshot as a transcript metadata line.
    ///
    /// This keeps `/goal` resumable on non-compacted transcripts; compact
    /// boundaries also snapshot the active goal in `compactMetadata` so a later
    /// compaction cannot summarize away the only copy.
    pub(crate) async fn persist_active_goal_state_to_jsonl(
        &self,
        active_goal: Option<&lingxi_core::session::ActiveGoalState>,
    ) {
        let status = if active_goal.is_some() {
            platform_api::GoalStatusKind::Set
        } else {
            platform_api::GoalStatusKind::Cleared
        };
        self.persist_goal_status_attachment(status, active_goal)
            .await;
    }

    pub(super) async fn persist_goal_status_attachment(
        &self,
        status: platform_api::GoalStatusKind,
        active_goal: Option<&lingxi_core::session::ActiveGoalState>,
    ) {
        let Some(goal) = active_goal else {
            return;
        };
        let total_tokens = self.snapshot_cost_real().await.total_tokens;
        let duration_ms = std::time::SystemTime::now()
            .duration_since(goal.set_at)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let snapshot = platform_api::ActiveGoalSnapshot {
            condition: goal.condition.clone(),
            set_at: goal.set_at,
            last_reason: goal.last_reason.clone(),
            iterations: goal.iterations,
            tokens_at_start: goal.tokens_at_start,
        };
        let condition = goal.condition.clone();
        let reason = goal.last_reason.clone();
        let tokens = total_tokens.saturating_sub(goal.tokens_at_start);
        let attachment = match status {
            platform_api::GoalStatusKind::Set => {
                platform_api::GoalStatusAttachment::sentinel_set(condition, Some(snapshot))
            }
            platform_api::GoalStatusKind::Cleared => {
                platform_api::GoalStatusAttachment::sentinel_cleared(condition)
            }
            platform_api::GoalStatusKind::Achieved => platform_api::GoalStatusAttachment::achieved(
                condition,
                reason,
                goal.iterations,
                duration_ms,
                tokens,
            ),
            platform_api::GoalStatusKind::Failed => platform_api::GoalStatusAttachment::failed(
                condition,
                reason,
                goal.iterations,
                duration_ms,
                tokens,
            ),
            // The goal survives a not-met turn, so the resume snapshot rides
            // along with it; upstream's record carries only condition+reason.
            platform_api::GoalStatusKind::NotMet => {
                platform_api::GoalStatusAttachment::not_met(condition, reason, Some(snapshot))
            }
        };
        match serde_json::to_value(attachment) {
            Ok(value) => self.persist_hook_attachment_to_jsonl(value).await,
            Err(error) => tracing::warn!(%error, "failed to encode goal status attachment"),
        }
    }

    /// Shared append body for [`Self::persist_message_to_jsonl_with_parent`],
    /// [`Self::persist_api_error_message_to_jsonl`] and
    /// [`Self::persist_compact_summary_to_jsonl`].
    pub(super) async fn persist_message_to_jsonl_inner(
        &self,
        msg: &ConversationMessage,
        parent_override: Option<String>,
        api_error: Option<ApiErrorEnvelope>,
        compact_summary: bool,
    ) {
        self.persist_message_to_jsonl_inner_with_queue_metadata(
            msg,
            parent_override,
            api_error,
            compact_summary,
            None,
        )
        .await;
    }

    async fn persist_message_to_jsonl_inner_with_queue_metadata(
        &self,
        msg: &ConversationMessage,
        parent_override: Option<String>,
        api_error: Option<ApiErrorEnvelope>,
        compact_summary: bool,
        queued_input: Option<&QueuedPromptInput>,
    ) {
        self.note_assistant_commit(msg).await;
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            // These side tables live only until the corresponding tool-result
            // line is persisted. In in-memory/no-writer sessions there is no
            // line to consume them below, so drain them here after callers have
            // had their pre-persist audience-note read.
            let _ = self.take_tool_use_result(msg).await;
            let _ = self.take_tool_denial_kind(msg).await;
            let _ = self.take_tool_use_mcp_meta(msg).await;
            let _ = self.take_source_tool_assistant_uuid(msg).await;
            return;
        };
        let (session_id_str, plan_mode, parent_uuid) = {
            let session = self.session.lock().await;
            let parent = match parent_override {
                Some(p) => Some(p),
                None => self.transcript.last_jsonl_uuid.lock().await.clone(),
            };
            (session.session_id.to_string(), session.plan_mode, parent)
        };
        // Writer-field fidelity (§G gap 4):
        // - gitBranch: once-per-session `getBranch()` (cached).
        // - entrypoint: `getEntrypoint()` → `CLAUDE_CODE_ENTRYPOINT` env or "cli"
        //   (mirrors the UA builder, `model/user_agent.rs:73`).
        // - promptId: `getPromptId()` on USER lines. We mint a fresh id when this
        //   is a genuine new user prompt and reuse it for the turn's tool_result
        //   `user` lines, matching TS where `getPromptId()` is stable across a
        //   turn. `to_jsonl_message` drops it on non-user lines.
        let git_branch = self.resolve_git_branch().await;
        let entrypoint = Some(entrypoint_value());
        let prompt_id = self.prompt_id_for_message(msg).await;
        let mut jmsg = self.to_jsonl_message_with_inner_id(
            msg,
            &session_id_str,
            parent_uuid,
            git_branch,
            entrypoint,
            prompt_id,
            None,
            None,
            None,
            None,
            api_error.as_ref(),
        );
        if matches!(msg, ConversationMessage::User { .. }) {
            if let Some(input) = queued_input {
                if let Some(priority) = &input.queue_priority {
                    jmsg.extra.insert(
                        "queuePriority".into(),
                        serde_json::Value::String(priority.clone()),
                    );
                }
                if let Some(task_id) = &input.scheduled_task_id {
                    jmsg.extra.insert(
                        "scheduledTaskId".into(),
                        serde_json::Value::String(task_id.clone()),
                    );
                    if let Some(fire_id) = &input.scheduled_fire_id {
                        jmsg.extra.insert(
                            "scheduledFireId".into(),
                            serde_json::Value::String(fire_id.clone()),
                        );
                    }
                }
            }
            let permission_mode = if plan_mode {
                "plan".to_string()
            } else {
                self.permission_mode()
                    .unwrap_or_else(|| "default".to_string())
            };
            jmsg.extra.insert(
                "permissionMode".to_string(),
                serde_json::Value::String(permission_mode),
            );
        }
        // Compaction summary user line: stamp the top-level envelope flags in
        // claude's on-disk order (`isVisibleInTranscriptOnly` before
        // `isCompactSummary`, between `message` and `uuid` — the schema's user
        // arm emits them there).
        // Denial provenance: claude stamps `toolDenialKind` on the tool_result
        // user line for a tool that was denied rather than run. The schema's
        // tool-result head places it after `timestamp` and before the common
        // trailer; the exactly-one-tool_result guard lives in
        // `take_tool_denial_kind`.
        //
        // O1: the same line also carries `toolUseResult` (the tool's raw
        // structured result / the `Error: …` string), the MCP `mcpMeta`
        // sibling, and `sourceToolAssistantUUID` (the assistant line that
        // carried the `tool_use`). All four share the single-block guard and
        // are emitted in `TOOL_RESULT_HEAD_EXTRA` order regardless of the
        // order they are inserted here.
        if let Some(result) = self.take_tool_use_result(msg).await {
            jmsg.extra.insert("toolUseResult".to_string(), result);
        }
        if let Some(kind) = self.take_tool_denial_kind(msg).await {
            jmsg.extra.insert(
                "toolDenialKind".to_string(),
                serde_json::Value::String(kind),
            );
        }
        if let Some(meta) = self.take_tool_use_mcp_meta(msg).await {
            jmsg.extra.insert("mcpMeta".to_string(), meta);
        }
        if self.tool_result_message_ends_turn(msg).await {
            jmsg.extra
                .insert("toolEndsTurn".to_string(), serde_json::Value::Bool(true));
        }
        if let Some(src) = self.take_source_tool_assistant_uuid(msg).await {
            jmsg.extra.insert(
                "sourceToolAssistantUUID".to_string(),
                serde_json::Value::String(src),
            );
        }
        if msg.is_visible_in_transcript_only() || compact_summary {
            jmsg.extra.insert(
                "isVisibleInTranscriptOnly".to_string(),
                serde_json::Value::Bool(true),
            );
        }
        if msg.is_compact_summary() || compact_summary {
            jmsg.extra.insert(
                "isCompactSummary".to_string(),
                serde_json::Value::Bool(true),
            );
        }
        let uuid_for_chain = jmsg.uuid.clone();
        match writer.append(&jmsg).await {
            Ok(()) => {
                *self.transcript.last_jsonl_uuid.lock().await = Some(uuid_for_chain.clone());
                telemetry::emit_session_appended(&session_id_str, &uuid_for_chain);
            }
            Err(e) => {
                self.record_transcript_append_failure(&session_id_str, "message", &e)
                    .await;
            }
        }
    }

    /// Persist a meta user message to one exact session transcript and mark it
    /// as excluded from model context. This never retargets the live writer.
    pub(crate) async fn persist_model_excluded_meta_to_session(
        &self,
        target_session: protocol::SessionId,
        msg: &ConversationMessage,
    ) -> Result<Option<String>, platform_api::HandleError> {
        self.persist_conversation_message_to_session(target_session, msg, true)
            .await
    }

    pub(crate) async fn persist_conversation_message_to_session(
        &self,
        target_session: protocol::SessionId,
        msg: &ConversationMessage,
        exclude_from_model: bool,
    ) -> Result<Option<String>, platform_api::HandleError> {
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            return Ok(None);
        };

        let target_uuid = target_session.as_uuid();
        let target_bare = target_uuid.to_string();
        let cwd = self.cwd.to_string_lossy().into_owned();
        let durable = writer.durable_transcript_enabled();
        let target_path = if durable {
            writer.session_target_path(target_session).ok_or_else(|| {
                platform_api::HandleError::ActionFailed(format!(
                    "durable transcript target is not bound for session {target_bare}"
                ))
            })?
        } else {
            match self.config_home.as_ref() {
                Some(home) => {
                    match session::jsonl::resolve_session_path_across_worktrees(
                        home,
                        &cwd,
                        target_uuid,
                    )
                    .await
                    {
                        Ok(path) => path,
                        Err(
                            session::jsonl::LoaderError::SessionNotFound { .. }
                            | session::jsonl::LoaderError::EmptyDirectory,
                        ) => session::jsonl::session_path(home, &cwd, &target_bare),
                        Err(error) => {
                            return Err(platform_api::HandleError::ActionFailed(format!(
                                "could not resolve target session transcript: {error}"
                            )))
                        }
                    }
                }
                None => {
                    let current = self.session.lock().await.session_id;
                    if current != target_session {
                        return Err(platform_api::HandleError::ActionFailed(
                            "target-session persistence requires a configured session store".into(),
                        ));
                    }
                    writer.active_path()
                }
            }
        };

        // Production resolves parentage inside the pinned transcript
        // transaction. Compatibility writers retain the historical loader
        // lookup because they have no durable target map/lock.
        let parent_uuid = if durable {
            None
        } else {
            let fs = writer.filesystem_handle();
            match self.config_home.as_ref() {
                Some(home) => {
                    match session::jsonl::load_session_across_worktrees(home, &cwd, target_uuid, fs)
                        .await
                    {
                        Ok(messages) => messages.last().map(|message| message.uuid.clone()),
                        Err(
                            session::jsonl::LoaderError::SessionNotFound { .. }
                            | session::jsonl::LoaderError::EmptyDirectory,
                        ) => None,
                        Err(error) => {
                            return Err(platform_api::HandleError::ActionFailed(format!(
                                "could not read target session transcript: {error}"
                            )))
                        }
                    }
                }
                None => self.transcript.last_jsonl_uuid.lock().await.clone(),
            }
        };

        let persisted_session_id = if durable {
            target_bare.clone()
        } else {
            target_session.to_string()
        };
        let mut persisted = self.to_jsonl_message_with_inner_id(
            msg,
            &persisted_session_id,
            parent_uuid,
            self.resolve_git_branch().await,
            Some(entrypoint_value()),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        if exclude_from_model {
            persisted.extra.insert(
                "isModelContextExcluded".to_string(),
                serde_json::Value::Bool(true),
            );
        }
        if durable {
            persisted.cwd = writer
                .session_target_cwd(target_session)
                .unwrap_or_else(|| self.current_cwd())
                .to_string_lossy()
                .into_owned();
        }
        if matches!(msg, ConversationMessage::User { .. }) {
            persisted.extra.insert(
                "permissionMode".to_string(),
                serde_json::Value::String(
                    self.permission_mode()
                        .unwrap_or_else(|| "default".to_string()),
                ),
            );
        }
        let uuid = persisted.uuid.clone();
        writer
            .append_to_path(&target_path, &persisted)
            .await
            .map_err(|error| {
                platform_api::HandleError::ActionFailed(format!(
                    "could not append target session transcript: {error}"
                ))
            })?;
        telemetry::emit_session_appended(&target_session.to_string(), &uuid);
        Ok(Some(uuid))
    }

    /// Persist an assistant turn as ONE single-block JSONL line PER content block
    /// (claude-code's per-`content_block_stop` writer — `claude.ts:2171-2211`).
    ///
    /// claude-code builds an `AssistantMessage` at each `content_block_stop` from
    /// a SINGLE content block (`content: normalizeContentFromAPI([contentBlock])`)
    /// with distinct top-level `uuid`s and the SAME inner `message.id` shared
    /// across all blocks of the turn. So an assistant turn `[text, tool_use A,
    /// tool_use B]` becomes THREE assistant JSONL lines: one shared inner
    /// `message.id`, three distinct top-level `uuid`s, one block each.
    ///
    /// This is a WRITE-side (transcript) split ONLY — the caller keeps the single
    /// merged `ConversationMessage::Assistant` in `session.history` for
    /// request-building (the Anthropic request needs one assistant turn carrying
    /// all blocks). The write-side top-level `uuid` is now derived from the
    /// turn id / block index / parent chain (instead of `MessageId::new()`), and
    /// the originating turn's id (`msg.id().as_uuid()`) is injected as the shared
    /// inner `message.id` so the loader's sibling-grouping reconstructs the DAG.
    ///
    /// Returns a `tool_use_id -> that block's line uuid` map so the caller can
    /// parent EACH `tool_result` to ITS specific `tool_use` line (TS
    /// `sourceToolAssistantUUID`), not one shared per-turn parent. On an
    /// assistant with no `tool_use` blocks the map is empty. The lines chain off
    /// `last_jsonl_uuid` (advancing it per line), so the LAST block's uuid ends
    /// up as `last_jsonl_uuid` and any subsequent non-tool message chains
    /// correctly. An empty-content assistant persists nothing (no line, empty
    /// map) — faithful to streaming, which never emits a zero-block turn.
    pub(crate) async fn persist_assistant_per_block(
        &self,
        msg: &ConversationMessage,
        // Raw Anthropic `usage` object for the BetaMessage envelope (the codec's
        // `Usage::provider_metadata`); `None` writes `usage: null`.
        usage: Option<&serde_json::Value>,
        // The Anthropic `request-id` response header for this turn → the
        // top-level `requestId` on every per-block assistant line. `None` when
        // the adapter recorded no request-id (e.g. a mock that does not surface
        // headers) — the line then omits `requestId`, like claude-code.
        request_id: Option<&str>,
    ) -> std::collections::HashMap<protocol::ToolUseId, String> {
        self.note_assistant_commit(msg).await;
        let mut map: std::collections::HashMap<protocol::ToolUseId, String> =
            std::collections::HashMap::new();
        let ConversationMessage::Assistant {
            id: turn_id,
            content,
            stop_reason,
        } = msg
        else {
            // Defensive: non-assistant messages fall back to the normal single
            // line (no split applies). Should not happen in practice.
            self.persist_message_to_jsonl(msg).await;
            return map;
        };
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            return map;
        };
        // Shared inner Anthropic `message.id` for every block of this turn.
        let inner_id = turn_id.as_uuid().to_string();

        let (session_id_str, mut model, mut model_profile) = {
            let s = self.session.lock().await;
            (
                s.session_id.to_string(),
                s.model.clone(),
                s.model_profile.clone(),
            )
        };
        if let Some(settings) = crate::scheduled_turn::current() {
            model = settings.model;
            model_profile = Some(settings.provider);
        }
        let git_branch = self.resolve_git_branch().await;
        let entrypoint = Some(entrypoint_value());

        for (block_index, block) in content.iter().enumerate() {
            // Build a synthetic SINGLE-block assistant message. The outer
            // JSONL `uuid` is derived from chain context below; the inner
            // message id remains shared for sibling grouping.
            let single = ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![block.clone()],
                stop_reason: stop_reason.clone(),
            };
            let parent_uuid = self.transcript.last_jsonl_uuid.lock().await.clone();
            let derived_uuid = Self::assistant_block_derived_uuid(
                &inner_id,
                block_index,
                parent_uuid.as_deref(),
                block,
            );
            // Assistant lines never carry a promptId (it is a user-only field).
            let mut jmsg = self.to_jsonl_message_with_inner_id(
                &single,
                &session_id_str,
                parent_uuid,
                git_branch.clone(),
                entrypoint.clone(),
                None,
                Some(&inner_id),
                Some(&model),
                usage,
                request_id,
                None,
            );
            jmsg.uuid = derived_uuid;
            jmsg.extra.insert(
                "modelProfile".to_string(),
                model_profile
                    .as_ref()
                    .map_or(serde_json::Value::Null, |profile| {
                        serde_json::Value::String(profile.clone())
                    }),
            );
            let line_uuid = jmsg.uuid.clone();
            match writer.append(&jmsg).await {
                Ok(()) => {
                    *self.transcript.last_jsonl_uuid.lock().await = Some(line_uuid.clone());
                    telemetry::emit_session_appended(&session_id_str, &line_uuid);
                }
                Err(e) => {
                    self.record_transcript_append_failure(&session_id_str, "assistant_block", &e)
                        .await;
                    // Skip recording this block's uuid in the map — the caller's
                    // fallback (prior single-parent) will be used for any
                    // tool_result that can't find its parent.
                    continue;
                }
            }
            if let protocol::ContentBlock::ToolUse { id, .. } = block {
                // O1: the SAME uuid claude stamps as `sourceToolAssistantUUID`
                // on this tool's `tool_result` user line (and from which its
                // `parentUuid` is derived — BIN off 237862200). Recording it
                // here keeps both turn-loop paths correct without touching any
                // call site.
                self.record_source_tool_assistant_uuid(id, line_uuid.clone())
                    .await;
                map.insert(id.clone(), line_uuid);
            }
        }
        map
    }

    /// Persist a NON-streaming (batched) assistant turn as ONE merged JSONL line
    /// carrying the full BetaMessage envelope — the real `model` (the live
    /// session model, same source [`Self::persist_assistant_per_block`] uses),
    /// the Anthropic `usage` object, and the `requestId`. This is the batched
    /// counterpart of `persist_assistant_per_block`: claude-code's non-streaming
    /// handler (`claude.ts:2571`) emits one merged `AssistantMessage` WITH the
    /// response model/usage, and the batched `run_turn` path must match.
    ///
    /// Previously the batched path used the model-less
    /// [`Self::persist_message_to_jsonl`], so every real `--print` / `--bg`
    /// reply was recorded as `model:"<synthetic>"` with `usage` dropped (cost
    /// lost, telemetry/resume mis-attributed) even though the API call
    /// succeeded. Genuine SYNTHETIC api-error lines still use the model-less
    /// path (`persist_api_error_message_to_jsonl` / `persist_message_to_jsonl`).
    pub(crate) async fn persist_assistant_merged(
        &self,
        msg: &ConversationMessage,
        // Typed response usage → the Anthropic `usage` JSON (via
        // `assistant_usage_value`). `None` writes `usage: null`.
        usage: Option<&llm_runtime::Usage>,
        request_id: Option<&str>,
    ) {
        self.note_assistant_commit(msg).await;
        let ConversationMessage::Assistant { id: turn_id, .. } = msg else {
            // Defensive: non-assistant messages take the plain single-line path.
            self.persist_message_to_jsonl(msg).await;
            return;
        };
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            return;
        };
        let inner_id = turn_id.as_uuid().to_string();
        let (session_id_str, mut model, mut model_profile) = {
            let s = self.session.lock().await;
            (
                s.session_id.to_string(),
                s.model.clone(),
                s.model_profile.clone(),
            )
        };
        if let Some(settings) = crate::scheduled_turn::current() {
            model = settings.model;
            model_profile = Some(settings.provider);
        }
        let git_branch = self.resolve_git_branch().await;
        let entrypoint = Some(entrypoint_value());
        let parent_uuid = self.transcript.last_jsonl_uuid.lock().await.clone();
        let usage_json = usage.map(assistant_usage_value);
        let mut jmsg = self.to_jsonl_message_with_inner_id(
            msg,
            &session_id_str,
            parent_uuid,
            git_branch,
            entrypoint,
            None,
            Some(&inner_id),
            Some(&model),
            usage_json.as_ref(),
            request_id,
            None,
        );
        jmsg.extra.insert(
            "modelProfile".to_string(),
            model_profile.map_or(serde_json::Value::Null, serde_json::Value::String),
        );
        let line_uuid = jmsg.uuid.clone();
        match writer.append(&jmsg).await {
            Ok(()) => {
                *self.transcript.last_jsonl_uuid.lock().await = Some(line_uuid.clone());
                telemetry::emit_session_appended(&session_id_str, &line_uuid);
                // O1: the batched path writes ONE merged line, so every
                // `tool_use` block in it shares that line's uuid as its
                // `sourceToolAssistantUUID`.
                if let ConversationMessage::Assistant { content, .. } = msg {
                    for block in content {
                        if let protocol::ContentBlock::ToolUse { id, .. } = block {
                            self.record_source_tool_assistant_uuid(id, line_uuid.clone())
                                .await;
                        }
                    }
                }
            }
            Err(e) => {
                self.record_transcript_append_failure(&session_id_str, "assistant_merged", &e)
                    .await;
            }
        }
    }

    /// Update the out-of-band assistant timestamp without changing message wire
    /// shape. Every production assistant commit passes through one of the JSONL
    /// persistence seams, including writer-less runtimes.
    async fn note_assistant_commit(&self, msg: &ConversationMessage) {
        if matches!(msg, ConversationMessage::Assistant { .. }) {
            self.session.lock().await.message_timing.last_assistant_at =
                Some(std::time::SystemTime::now());
        }
    }
}

/// Claude 2.1.270 `npe`: strip ANSI, normalize whitespace, remove invisible
/// controls, then truncate without splitting a UTF-16 surrogate pair.
fn scheduled_fire_prompt(prompt: &str) -> String {
    use std::sync::LazyLock;
    static ANSI: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
        r"[\x{001B}\x{009B}][\[\]()#;?]*(?:(?:(?:(?:;[-a-zA-Z0-9/#&.:=?%@~_]+)*|[a-zA-Z0-9]+(?:;[-a-zA-Z0-9/#&.:=?%@~_]*)*)?(?:\x{0007}|\x{001B}\x{005C}|\x{009C}))|(?:(?:[0-9]{1,4}(?:;[0-9]{0,4})*)?[0-9A-PR-TZcf-nq-uy=><~]))"
    ).expect("oracle ANSI pattern")
    });
    static CONTROLS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"[\p{Cc}\p{Cf}\x{2028}\x{2029}]").expect("control pattern")
    });
    let stripped = ANSI.replace_all(prompt, "");
    let stripped: String = stripped
        .chars()
        .filter(|c| {
            !matches!(*c as u32,
        0..=8 | 14..=31 | 127..=159)
        })
        .collect();
    static SPACE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
        r"[ \t\n\v\f\r\x{00A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]+"
    ).expect("ECMAScript whitespace")
    });
    let normalized = SPACE.replace_all(&stripped, " ");
    let ansi_clean = ANSI.replace_all(&normalized, "");
    let clean = CONTROLS.replace_all(&ansi_clean, "");
    let mut units = 0;
    clean
        .trim()
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= 200
        })
        .collect()
}

#[cfg(test)]
#[test]
fn scheduled_fire_prompt_matches_oracle_sanitization_and_utf16_limit() {
    let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
        "../../../session/tests/fixtures/loop-2.1.270/prompt.json"
    ))
    .unwrap();
    for case in cases {
        assert_eq!(
            scheduled_fire_prompt(case["input"].as_str().unwrap()),
            case["expected"].as_str().unwrap()
        );
    }
}
