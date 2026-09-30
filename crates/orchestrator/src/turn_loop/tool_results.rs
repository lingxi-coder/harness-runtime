use crate::conversation::ConversationOrchestrator;
use lingxi_core::types::{ConversationMessage, ToolUseId};

/// HOOK.2 twin of [`dispatch_tool_uses`] that ALSO returns whether any
/// `PreToolUse` hook in this batch requested `continue:false`
/// (preventContinuation). The batched turn loop
/// ([`execute_one_turn_with_recovery_tracked`]) uses the flag to end the turn
/// step (TS `query.ts:1518-1521` `{ reason: 'hook_stopped' }`); the streaming
/// concurrent path keeps the plain [`dispatch_tool_uses`] wrapper.
#[allow(clippy::too_many_lines)]
/// `Gzg` (2.1.220 BIN off **230270568**, immediately above `F0u`):
///
/// ```text
/// if(!e)return!0;
/// if(typeof e==="string")return e.trim()==="";
/// if(!Array.isArray(e))return!1;
/// if(e.length===0)return!0;
/// return e.every(t=>typeof t==="object"&&"type"in t&&t.type==="text"
///                 &&"text"in t&&(typeof t.text!=="string"||t.text.trim()===""))
/// ```
///
/// The port's `tool_result` carries EITHER a plain string (`content_blocks ==
/// None`) or the verbatim block array — so the array arm is driven by
/// `content_blocks` and the string arm by `content`.
pub(super) fn tool_result_is_blank(
    content: &str,
    content_blocks: Option<&[serde_json::Value]>,
) -> bool {
    match content_blocks {
        None => content.trim().is_empty(),
        Some(blocks) => {
            blocks.is_empty()
                || blocks.iter().all(|b| {
                    b.get("type").and_then(serde_json::Value::as_str) == Some("text")
                        && b.get("text").is_some()
                        && b.get("text")
                            .and_then(serde_json::Value::as_str)
                            .is_none_or(|t| t.trim().is_empty())
                })
        }
    }
}

/// `U0u` (2.1.220 BIN off **230271605**): an array containing ANY `image` or
/// `document` block is never persisted, regardless of size.
pub(super) fn tool_result_has_media(content_blocks: Option<&[serde_json::Value]>) -> bool {
    content_blocks.is_some_and(|blocks| {
        blocks.iter().any(|b| {
            matches!(
                b.get("type").and_then(serde_json::Value::as_str),
                Some("image" | "document")
            )
        })
    })
}

/// `q0u` (2.1.220 BIN off **230271734**): a string's own length, or the sum of
/// the `text` block lengths in an array (non-text blocks contribute 0).
pub(super) fn tool_result_size(
    content: &str,
    content_blocks: Option<&[serde_json::Value]>,
) -> usize {
    match content_blocks {
        None => content.encode_utf16().count(),
        Some(blocks) => blocks
            .iter()
            .map(|b| {
                if b.get("type").and_then(serde_json::Value::as_str) == Some("text") {
                    b.get("text")
                        .and_then(serde_json::Value::as_str)
                        .map_or(0, |text| text.encode_utf16().count())
                } else {
                    0
                }
            })
            .sum(),
    }
}

/// A1 — port of claude-code `F0u` (2.1.220 BIN off **230270568**), the
/// post-processor every SUCCESSFUL `tool_result` passes through on its way to
/// the model. Guards run in the oracle's order:
///
/// 1. `Gzg` — a blank result becomes `` `(${toolName} completed with no output)` ``
///    and fires `tengu_tool_empty_result`.
/// 2. `U0u` — an image/document-bearing result is returned UNCHANGED.
/// 3. `o<=i` — only a STRICTLY larger body is persisted.
/// 4. A persist failure returns the ORIGINAL content (the error never reaches
///    the model).
/// 5. On success, `tengu_tool_result_persisted` is fired and the envelope is
///    substituted.
///
/// `threshold == None` is the oracle's `maxResultSizeChars: 1/0`
/// (`!Number.isFinite(t)` → early return), i.e. NEVER persist. A missing
/// `config_home` (library/test callers) is likewise a strict no-op for the
/// persistence arm — the blank-result arm still applies, since it needs no
/// filesystem.
/// Outcome of [`apply_tool_result_persistence`].
///
/// `replaced` is load-bearing, not informational. claude-code's `F0u` returns
/// `{...e, content: a}` where `content` is the ONE model-facing payload — a
/// string OR an array — so a substitution replaces the whole payload. LingXi
/// splits that payload across `ContentBlock::ToolResult`'s `content` string and
/// its `content_blocks` array, and the wire conversion prefers the array when
/// present (`llm-runtime/src/convert.rs`: `content_blocks.map_or_else(|| String(content), Array)`).
/// So substituting only `content` would leave the oversized array to win at the
/// wire: the file gets written, the telemetry fires, and the model still
/// receives the full payload. The caller MUST clear `content_blocks` whenever
/// this reports `true`.
pub(super) struct PersistenceOutcome {
    pub(super) utf16_code_units: Option<Vec<u16>>,
    pub(super) content: String,
    pub(super) replaced: bool,
}

/// Read the process-output spill identity emitted by the Bash tool.
///
/// 2.1.263 result data carries `persistedOutputPath` / `persistedOutputSize`.
/// Older in-flight results still used `outputTaskId` / `outputFilePath` /
/// `outputFileSize`; both shapes are accepted so a mid-upgrade transcript
/// keeps the same file. A partial object is rejected so the persistence
/// layer cannot fall back to a path that cannot be tied to the spill.
pub(super) fn process_output_file_from_data(
    data: &serde_json::Value,
) -> Option<lingxi_core::host::process::ProcessOutputFile> {
    let object = data.as_object()?;
    let path = object
        .get("persistedOutputPath")
        .or_else(|| object.get("outputFilePath"))?
        .as_str()?;
    let size = object
        .get("persistedOutputSize")
        .or_else(|| object.get("outputFileSize"))?
        .as_u64()?;
    let task_id = object
        .get("outputTaskId")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            std::path::Path::new(path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })?;
    if task_id.is_empty() || path.is_empty() {
        return None;
    }
    Some(lingxi_core::host::process::ProcessOutputFile {
        task_id,
        path: path.to_string(),
        size,
    })
}

/// Reuse a process runner's rooted output file when building the model-facing
/// `<persisted-output>` envelope. This keeps the process task identity/path
/// intact and, unlike the generic persistence arm, does not write the same
/// bytes a second time under the tool-use id.
pub(super) async fn apply_tool_result_persistence_with_process_output(
    orch: &ConversationOrchestrator,
    tool_name: &str,
    tool_use_id: &ToolUseId,
    threshold: Option<usize>,
    content: String,
    content_blocks: Option<&[serde_json::Value]>,
    output_file: Option<&lingxi_core::host::process::ProcessOutputFile>,
) -> PersistenceOutcome {
    use crate::tool_result_persistence as trp;

    if let Some(output_file) = output_file {
        // Keep the normal blank/media guards authoritative. In particular, a
        // large image result may have been captured through the same process
        // seam, but its structured media blocks must remain inline.
        if !tool_result_is_blank(&content, content_blocks) && !tool_result_has_media(content_blocks)
        {
            let (preview, content_has_more) = trp::preview_utf16(&content, trp::PREVIEW_CHARS);
            let original_size = usize::try_from(output_file.size).unwrap_or(usize::MAX);
            let exact_replacement = trp::wrap_utf16(
                original_size,
                &output_file.path,
                &preview,
                content_has_more || output_file.size > trp::PREVIEW_CHARS as u64,
                // TL-6, the externally-persisted arm: upstream's Bash
                // `mapToolResultToToolResultBlockParam` marks the envelope
                // truncated with `(size ?? 0) >= HY ? HY : undefined`, where
                // `HY = 67108864`. The spool stops at the same 64 MiB, so a
                // result that REACHED the cap is exactly the one that was cut.
                (output_file.size >= lingxi_core::host::task_output::MAX_PERSISTED_OUTPUT_BYTES)
                    .then_some(
                        usize::try_from(lingxi_core::host::task_output::MAX_PERSISTED_OUTPUT_BYTES)
                            .unwrap_or(usize::MAX),
                    ),
            );
            let replacement = String::from_utf16_lossy(&exact_replacement);
            tracing::info!(
                task_id = %output_file.task_id,
                path = %output_file.path,
                size = output_file.size,
                "Reused rooted process output for tool result persistence"
            );
            if let Some(bus) = orch.model_runtime.analytics_bus.as_ref() {
                #[allow(clippy::cast_possible_wrap)]
                fn int(v: usize) -> telemetry::AnalyticsValue {
                    telemetry::AnalyticsValue::Int(i64::try_from(v).unwrap_or(i64::MAX))
                }
                let mut metadata = telemetry::LogEventMetadata::new();
                metadata.insert(
                    "toolName".into(),
                    telemetry::AnalyticsValue::String(tool_name.to_string()),
                );
                metadata.insert("originalSizeBytes".into(), int(original_size));
                metadata.insert("persistedSizeBytes".into(), int(exact_replacement.len()));
                metadata.insert(
                    "estimatedOriginalTokens".into(),
                    int(original_size.div_ceil(trp::CHARS_PER_TOKEN)),
                );
                metadata.insert(
                    "estimatedPersistedTokens".into(),
                    int(exact_replacement.len().div_ceil(trp::CHARS_PER_TOKEN)),
                );
                metadata.insert(
                    "thresholdUsed".into(),
                    int(threshold.unwrap_or(original_size)),
                );
                bus.log_event("tengu_tool_result_persisted", metadata).await;
            }
            return PersistenceOutcome {
                utf16_code_units: String::from_utf16(&exact_replacement)
                    .is_err()
                    .then_some(exact_replacement),
                content: replacement,
                replaced: true,
            };
        }
    }

    apply_tool_result_persistence(
        orch,
        tool_name,
        tool_use_id,
        threshold,
        content,
        content_blocks,
    )
    .await
}

/// Persist every tool result a keep-recent microcompact is about to clear, and
/// return the `<persisted-output>…</persisted-output>` substitution for each.
///
/// Best-effort per candidate: a failed write simply leaves that id out of the
/// map, and the clear then substitutes the bare placeholder — upstream's
/// `persist(...) ?? Z2e`. An absent `config_home` (tests, minimal embedders)
/// skips the whole step, which is the pre-CMP-2 behaviour exactly.
pub(super) async fn persist_keep_recent_clears(
    orch: &ConversationOrchestrator,
    messages: &[ConversationMessage],
) -> std::collections::HashMap<ToolUseId, String> {
    use crate::tool_result_persistence as trp;

    let mut out = std::collections::HashMap::new();
    let Some(home) = orch.config_home.as_ref() else {
        return out;
    };
    let candidates = compaction::microcompact::keep_recent_persist_candidates(
        messages,
        compaction::context_hint::CONTEXT_HINT_KEEP_RECENT,
    );
    if candidates.is_empty() {
        return out;
    }
    let session_uuid = {
        let session = orch.session.lock().await;
        session.session_id.as_uuid().to_string()
    };
    let dir = trp::tool_results_dir(home, &orch.current_cwd().to_string_lossy(), &session_uuid);
    for (tool_use_id, content) in candidates {
        match trp::persist(
            home,
            &dir,
            tool_use_id.as_str(),
            &content,
            false,
            trp::MAX_PERSIST_UTF16_UNITS,
        )
        .await
        {
            Ok(persisted) => {
                out.insert(
                    tool_use_id,
                    trp::microcompact_replacement(
                        &persisted.filepath.to_string_lossy(),
                        persisted.truncated_at,
                    ),
                );
            }
            Err(error) => {
                tracing::debug!(
                    tool_use_id = %tool_use_id.as_str(),
                    %error,
                    "keep-recent clear could not persist a tool result; using the bare placeholder"
                );
            }
        }
    }
    out
}

pub(super) async fn apply_tool_result_persistence(
    orch: &ConversationOrchestrator,
    tool_name: &str,
    tool_use_id: &ToolUseId,
    threshold: Option<usize>,
    content: String,
    content_blocks: Option<&[serde_json::Value]>,
) -> PersistenceOutcome {
    use crate::tool_result_persistence as trp;

    if tool_result_is_blank(&content, content_blocks) {
        if let Some(bus) = orch.model_runtime.analytics_bus.as_ref() {
            let mut metadata = telemetry::LogEventMetadata::new();
            metadata.insert(
                "toolName".into(),
                telemetry::AnalyticsValue::String(tool_name.to_string()),
            );
            bus.log_event("tengu_tool_empty_result", metadata).await;
        }
        return PersistenceOutcome {
            utf16_code_units: None,
            content: format!("({tool_name} completed with no output)"),
            replaced: true,
        };
    }
    if tool_result_has_media(content_blocks) {
        return PersistenceOutcome {
            utf16_code_units: None,
            content,
            replaced: false,
        };
    }
    let Some(threshold) = threshold else {
        return PersistenceOutcome {
            utf16_code_units: None,
            content,
            replaced: false,
        };
    };
    let size = tool_result_size(&content, content_blocks);
    if size <= threshold {
        return PersistenceOutcome {
            utf16_code_units: None,
            content,
            replaced: false,
        };
    }
    let Some(home) = orch.config_home.as_ref() else {
        return PersistenceOutcome {
            utf16_code_units: None,
            content,
            replaced: false,
        };
    };

    // `x2e` serializes an ARRAY body with `JSON.stringify(e,null,2)` and a
    // string body verbatim; the extension follows (`kKr`).
    let (body, is_json) = match content_blocks {
        Some(blocks) => match serde_json::to_string_pretty(blocks) {
            Ok(s) => (s, true),
            // `e.some(l=>l.type!=="text")` already returned an error above in
            // the oracle; an unserializable array is the same "leave it alone".
            Err(_) => {
                return PersistenceOutcome {
                    utf16_code_units: None,
                    content,
                    replaced: false,
                }
            }
        },
        None => (content.clone(), false),
    };

    let session_uuid = {
        let session = orch.session.lock().await;
        session.session_id.as_uuid().to_string()
    };
    let dir = trp::tool_results_dir(home, &orch.current_cwd().to_string_lossy(), &session_uuid);
    // The on-disk stem is the port's INTERNAL `ToolUseId`, matching the
    // oracle's `${e.tool_use_id}.txt` — claude-code's internal block-param id
    // likewise differs from the `toolu_…` id it records in the transcript.
    let persisted = match trp::persist(
        home,
        &dir,
        tool_use_id.as_str(),
        &body,
        is_json,
        trp::MAX_PERSIST_UTF16_UNITS,
    )
    .await
    {
        Ok(p) => p,
        Err(msg) => {
            tracing::error!(
                path = %dir.join(tool_use_id.as_str()).display(),
                "Failed to persist tool result: {msg}"
            );
            return PersistenceOutcome {
                utf16_code_units: None,
                content,
                replaced: false,
            };
        }
    };
    let path_display = persisted.filepath.display().to_string();
    tracing::info!(
        "Persisted tool result to {path_display} ({})",
        trp::format_bytes(persisted.original_size)
    );
    let exact_replacement = trp::wrap_utf16(
        persisted.original_size,
        &path_display,
        &persisted.preview_utf16,
        persisted.has_more,
        persisted.truncated_at,
    );
    let replacement = String::from_utf16_lossy(&exact_replacement);
    if let Some(bus) = orch.model_runtime.analytics_bus.as_ref() {
        #[allow(clippy::cast_possible_wrap)]
        fn int(v: usize) -> telemetry::AnalyticsValue {
            telemetry::AnalyticsValue::Int(i64::try_from(v).unwrap_or(i64::MAX))
        }
        let mut metadata = telemetry::LogEventMetadata::new();
        metadata.insert(
            "toolName".into(),
            telemetry::AnalyticsValue::String(tool_name.to_string()),
        );
        metadata.insert("originalSizeBytes".into(), int(persisted.original_size));
        metadata.insert("persistedSizeBytes".into(), int(exact_replacement.len()));
        metadata.insert(
            "estimatedOriginalTokens".into(),
            int(persisted.original_size.div_ceil(trp::CHARS_PER_TOKEN)),
        );
        metadata.insert(
            "estimatedPersistedTokens".into(),
            int(exact_replacement.len().div_ceil(trp::CHARS_PER_TOKEN)),
        );
        metadata.insert("thresholdUsed".into(), int(threshold));
        bus.log_event("tengu_tool_result_persisted", metadata).await;
    }
    PersistenceOutcome {
        utf16_code_units: String::from_utf16(&exact_replacement)
            .is_err()
            .then_some(exact_replacement),
        content: replacement,
        replaced: true,
    }
}

/// Serialize a successful tool result's data into the model-facing string.
///
/// Mirrors claude-code's per-tool `mapToolResultToToolResultBlockParam`: the
/// model sees the tool's OWN string, never a JSON dump of the output object. A
/// tool exposes that string via `model_content` (used when it must differ from
/// the TUI payload — e.g. Read's cat -n + reminders, where the TUI shows raw
/// content) or, failing that, the verbatim `content` string (Bash stdout,
/// Edit/Write confirmations, where the model and TUI strings coincide). Tools
/// that expose neither fall back to the JSON object — the legacy behavior, kept
/// for structured-only results that have no human-facing string.
///
/// The full `result.data` object still flows to the TUI (`emit_tool_result`)
/// and the `PostToolUse` hook unchanged; only the model-facing string is derived
/// here.
pub(super) fn tool_result_to_model_text(data: &serde_json::Value) -> String {
    data.get("model_content")
        .and_then(|v| v.as_str())
        .or_else(|| data.get("content").and_then(|v| v.as_str()))
        // `result` is WebFetch's content field (claude-code's WebFetch result
        // `data` names the model-facing markdown `result`, byte-faithful to the
        // binary's `{bytes,code,codeText,result,durationMs,url}`). Without this
        // arm a WebFetch result (no `content`/`model_content`) would fall through
        // to the JSON dump below and show the model the whole object.
        .or_else(|| data.get("result").and_then(|v| v.as_str()))
        .map_or_else(
            || serde_json::to_string(data).unwrap_or_else(|_| "<unserializable>".into()),
            std::string::ToString::to_string,
        )
}

/// Map ToolSearch's matched names to Anthropic `tool_reference` blocks. Empty
/// results stay on the text path (`model_content` carries the upstream copy).
pub(super) fn tool_search_reference_blocks(
    data: &serde_json::Value,
) -> Option<Vec<serde_json::Value>> {
    let matches = data.get("matches")?.as_array()?;
    if matches.is_empty() {
        return None;
    }
    let blocks: Vec<serde_json::Value> = matches
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(|name| {
            serde_json::json!({
                "type": "tool_reference",
                "tool_name": name,
            })
        })
        .collect();
    (!blocks.is_empty()).then_some(blocks)
}

/// The binary result-mapper's `case "image"` (`mapToolResultToToolResultBlockParam`):
/// a `{type:"image", file:{base64, type, …}}` result — Read on an image file, or a
/// rendered PDF page (`tbo`) — becomes a tool_result whose content is the image
/// block array `[{type:"image", source:{type:"base64", data, media_type}}]`,
/// emitted VERBATIM on egress via `ContentBlock::ToolResult.content_blocks`.
/// `None` for every other result shape (strict no-op: the wire form falls back to
/// the text `content` exactly as before).
///
/// The rule itself lives in `tool_api::tool_result_media` because the subagent
/// runner needs the identical answer, and it is in another crate. This stays a
/// named function so the call chain above and the oracle note it carries are
/// untouched.
pub(super) fn image_tool_result_blocks(data: &serde_json::Value) -> Option<Vec<serde_json::Value>> {
    tool_api::tool_result_media::image_content_blocks(data)
}

/// The binary's Bash image mapper `hKn`: an `{isImage:true, stdout:<data-URI>}`
/// result becomes a tool_result whose content is `[{type:"image", source:
/// {type:"base64", media_type:<SNIFFED>, data}}]` — the media type comes from
/// magic-byte sniffing of the DECODED payload (`Wfe`), NOT the URI's claimed
/// type; `data` is the URI's original base64. Any miss (no isImage, no data-URI
/// match on `/^data:([^;]+);base64,(.+)$/`, undecodable base64, unrecognized
/// magic) returns `None` — the tool_result falls back to the text `content`,
/// exactly the binary's `if(g)` fall-through.
pub(super) fn bash_image_tool_result_blocks(
    data: &serde_json::Value,
) -> Option<Vec<serde_json::Value>> {
    use base64::Engine as _;
    if data.get("isImage") != Some(&serde_json::Value::Bool(true)) {
        return None;
    }
    let stdout = data.get("stdout").and_then(serde_json::Value::as_str)?;
    // `Xyu`: /^data:([^;]+);base64,(.+)$/ on the trimmed string.
    let rest = stdout.trim().strip_prefix("data:")?;
    let semi = rest.find(';')?;
    if semi == 0 {
        return None;
    }
    let payload = rest[semi..].strip_prefix(";base64,")?;
    if payload.is_empty() {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    // `Wfe` — magic-byte sniff (shared impl in tool-api, same fn the Bash
    // tool's image gate uses, so gate and mapper always agree).
    let media_type = tool_api::util::image_sniff::sniff_image_media_type(&bytes)?;
    Some(vec![serde_json::json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": media_type,
            "data": payload,
        },
    })])
}
