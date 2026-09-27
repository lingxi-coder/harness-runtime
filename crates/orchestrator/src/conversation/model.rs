//! Model-call preparation, model controls, usage accounting, and error handling.

use super::*;

const PROMPT_SUGGESTION_XML_RE: &str = r"(?is)^<(suggestion|response|output|answer|result)>([\s\S]*)</(suggestion|response|output|answer|result)>$";
const PROMPT_SUGGESTION_LABEL_RE: &str = r"(?is)^\s*(suggested\s+(response|reply|input|prompt)|suggestion|response|reply|answer|output|result)\s*:\s*";
const PROMPT_SUGGESTION_SILENCE_RE: &str = r"(?i)\bsilence is\b|\bstay(s|ing)? silent\b";
const PROMPT_SUGGESTION_BARE_SILENCE_RE: &str = r"(?i)^\W*silence\W*$";
const PROMPT_SUGGESTION_WRAPPER_RE: &str = r#"^\(.*\)$|^\[.*\]$"#;
// JavaScript's non-`u` `\w` is ASCII-only; Rust regex defaults to Unicode.
const PROMPT_SUGGESTION_SPEAKER_RE: &str = r"^(?-u:\w)+:\s";
const PROMPT_SUGGESTION_MULTIPLE_SENTENCES_RE: &str = r"[.!?]\s+[A-Z]";
const PROMPT_SUGGESTION_EVALUATIVE_RE: &str = r"(?i)thanks|thank you|looks good|sounds good|that works|that worked|that's all|nice|great|perfect|makes sense|awesome|excellent";
const PROMPT_SUGGESTION_CLAUDE_VOICE_RE: &str = r"(?i)^(let me|i'll|i've|i'm|i can|i would|i think|i notice|here's|here is|here are|that's|this is|this will|you can|you should|you could|sure,|of course|certainly)";

const PROMPT_SUGGESTION_SINGLE_WORD_ALLOWLIST: &[&str] = &[
    "yes", "yeah", "yep", "yea", "yup", "sure", "ok", "okay", "push", "commit", "deploy", "stop",
    "continue", "check", "exit", "quit", "no",
];

fn last_response_is_api_error(history: &[ConversationMessage]) -> bool {
    history.iter().rev().find_map(|message| match message {
        ConversationMessage::Assistant { stop_reason, .. } => Some(matches!(
            stop_reason.as_deref(),
            Some("model_error" | "max_tokens" | "model_context_window_exceeded" | "refusal")
        )),
        _ => None,
    }) == Some(true)
}

fn parse_prompt_suggestion_response(raw: &str) -> Option<String> {
    let xml = regex::Regex::new(PROMPT_SUGGESTION_XML_RE).expect("valid xml regex");
    let labeled = regex::Regex::new(PROMPT_SUGGESTION_LABEL_RE).expect("valid label regex");
    let silence = regex::Regex::new(PROMPT_SUGGESTION_SILENCE_RE).expect("valid silence regex");
    let bare_silence =
        regex::Regex::new(PROMPT_SUGGESTION_BARE_SILENCE_RE).expect("valid bare silence regex");
    let wrappers = regex::Regex::new(PROMPT_SUGGESTION_WRAPPER_RE).expect("valid wrapper regex");
    let speaker = regex::Regex::new(PROMPT_SUGGESTION_SPEAKER_RE).expect("valid speaker regex");
    let multiple_sentences = regex::Regex::new(PROMPT_SUGGESTION_MULTIPLE_SENTENCES_RE)
        .expect("valid multiple-sentences regex");
    let evaluative =
        regex::Regex::new(PROMPT_SUGGESTION_EVALUATIVE_RE).expect("valid evaluative regex");
    let claude_voice =
        regex::Regex::new(PROMPT_SUGGESTION_CLAUDE_VOICE_RE).expect("valid claude-voice regex");

    let mut suggestion = raw.trim();
    if suggestion.is_empty() {
        return None;
    }
    if let Some(caps) = xml.captures(suggestion) {
        let opening = caps.get(1).map_or("", |m| m.as_str());
        let inner = caps.get(2).map_or("", |m| m.as_str());
        let closing = caps.get(3).map_or("", |m| m.as_str());
        let nested_lower = format!("</{}>", opening.to_ascii_lowercase());
        let nested_upper = format!("</{}>", opening.to_ascii_uppercase());
        if opening.eq_ignore_ascii_case(closing)
            && !inner.contains(&nested_lower)
            && !inner.contains(&nested_upper)
        {
            suggestion = inner.trim();
        }
    }
    let labeled_str = labeled.replace(suggestion, "").into_owned();
    suggestion = labeled_str.trim();
    if suggestion.is_empty() {
        return None;
    }

    let lowered = suggestion.to_ascii_lowercase();
    if matches!(
        lowered.as_str(),
        "done" | "nothing found" | "nothing found."
    ) || silence.is_match(suggestion)
        || bare_silence.is_match(suggestion)
        || lowered.starts_with("nothing to suggest")
        || lowered.starts_with("no suggestion")
        || wrappers.is_match(suggestion)
        || lowered.starts_with("api error:")
        || lowered.starts_with("prompt is too long")
        || lowered.starts_with("request timed out")
        || lowered.starts_with("invalid api key")
        || lowered.starts_with("image was too large")
        || speaker.is_match(suggestion)
        || prompt_suggestion_word_weight(suggestion) > 12
        || suggestion.encode_utf16().count() >= 100
        || multiple_sentences.is_match(suggestion)
        || suggestion.contains('\n')
        || suggestion.contains('*')
        || evaluative.is_match(suggestion)
        || claude_voice.is_match(suggestion)
    {
        return None;
    }

    let weighted_words = prompt_suggestion_word_weight(suggestion);
    if weighted_words < 2 && !suggestion.starts_with('/') {
        let cjk = prompt_suggestion_cjk_count(suggestion);
        if cjk > 0 {
            if cjk < 2 {
                return None;
            }
        } else if !PROMPT_SUGGESTION_SINGLE_WORD_ALLOWLIST.contains(&lowered.as_str()) {
            return None;
        }
    }

    Some(suggestion.to_string())
}

fn prompt_suggestion_script_counts(text: &str) -> (u32, u32, u32, u32) {
    let han = regex::Regex::new(r"\p{Han}").expect("han script");
    let phonetic = regex::Regex::new(
        r"[\p{Hiragana}\p{Katakana}\u{30FC}\u{FF70}\p{Thai}\p{Lao}\p{Khmer}\p{Myanmar}]",
    )
    .expect("phonetic scripts");
    let hangul = regex::Regex::new(r"\p{Hangul}").expect("hangul script");
    let letter_or_number = regex::Regex::new(r"[\p{L}\p{N}]").expect("letter or number");
    let mut han_n = 0;
    let mut phonetic_n = 0;
    let mut hangul_n = 0;
    let mut other_n = 0;
    let mut buf = [0u8; 4];
    for c in text.chars() {
        let s = c.encode_utf8(&mut buf);
        if han.is_match(s) {
            han_n += 1;
        } else if phonetic.is_match(s) {
            phonetic_n += 1;
        } else if hangul.is_match(s) {
            hangul_n += 1;
        } else if letter_or_number.is_match(s) {
            other_n += 1;
        }
    }
    (han_n, phonetic_n, hangul_n, other_n)
}

/// Oracle `PQn`: whitespace tokens, with Han/kana/Thai weighted so CJK
/// suggestions are not dropped as "one English word".
fn prompt_suggestion_word_weight(text: &str) -> usize {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return 0;
    }
    let mut weight = 0.0_f64;
    for token in trimmed.split_whitespace() {
        let (han, phonetic, hangul, other) = prompt_suggestion_script_counts(token);
        weight += if han == 0 && phonetic == 0 {
            1.0
        } else {
            let mixed = if other + hangul > 0 { 1.0 } else { 0.0 };
            mixed + f64::from(han) / 2.0 + f64::from(phonetic) / 4.0
        };
    }
    weight.ceil() as usize
}

/// Oracle `RQn`: Han + kana/Thai + Hangul count. A CJK token with ≥2 of these
/// letters is kept even when the whitespace word count is 1.
fn prompt_suggestion_cjk_count(text: &str) -> u32 {
    let (han, phonetic, hangul, _) = prompt_suggestion_script_counts(text);
    han + phonetic + hangul
}

fn is_cross_device(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(17) | Some(18))
}

fn superseded_sidecar_path(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| std::io::Error::other("session sidecar path is not UTF-8"))?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    for attempt in 0..1000_u32 {
        let suffix = if attempt == 0 {
            format!(".superseded-{millis}")
        } else {
            format!(".superseded-{millis}-{attempt}")
        };
        let candidate = path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(format!("{name}{suffix}"));
        if std::fs::symlink_metadata(&candidate).is_err() {
            std::fs::rename(path, &candidate)?;
            return Ok(candidate);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a superseded session sidecar name",
    ))
}

fn copy_session_sidecar(source: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir(target)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o700))?;
    }
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_session_sidecar(&source_path, &target_path)?;
        } else if file_type.is_file() {
            std::fs::copy(source_path, target_path)?;
        } else {
            return Err(std::io::Error::other("unsupported session sidecar entry"));
        }
    }
    Ok(())
}

#[derive(Debug)]
enum SidecarCopyError {
    Copy(std::io::Error),
    Cleanup(std::io::Error),
}

/// Run the cross-device copy and source cleanup as two distinct phases.  A
/// successful copy is already a complete, authoritative destination; failure
/// while removing the old source must therefore leave that destination in
/// place rather than entering the copy-failure rollback path.
fn copy_then_cleanup_with<C, R>(
    source: &std::path::Path,
    target: &std::path::Path,
    copy: C,
    remove_source: R,
) -> Result<(), SidecarCopyError>
where
    C: FnOnce(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
    R: FnOnce(&std::path::Path) -> std::io::Result<()>,
{
    copy(source, target).map_err(SidecarCopyError::Copy)?;
    remove_source(source).map_err(SidecarCopyError::Cleanup)
}

/// Validate a path below its rooted projects directory without following a
/// symlinked component. The sidecar paths are derived from the config home,
/// but the filesystem can be changed independently between `/cd` requests;
/// inspect every existing component before any quarantine or move operation.
fn sidecar_path_is_rooted(root: &std::path::Path, candidate: &std::path::Path) -> bool {
    let Ok(root_metadata) = std::fs::symlink_metadata(root) else {
        return false;
    };
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return false;
    }
    let Ok(relative) = candidate.strip_prefix(root) else {
        return false;
    };
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return false;
    }

    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return false,
        };
        if metadata.file_type().is_symlink() {
            return false;
        }
    }
    true
}

/// Move the complete per-session sidecar directory best-effort.  The current
/// runtime primarily stores `tool-results` here, but Claude keeps all
/// session-scoped artifacts under this directory; moving the directory as a
/// unit avoids stranding future attachment/state files after `/cd`.
fn move_session_sidecar_best_effort(source: &std::path::Path, target: &std::path::Path) {
    if source == target {
        return;
    }

    // Both paths are `<projects>/<sanitized-cwd>/<session-id>`. Keep the
    // source and destination under the same rooted projects directory and
    // inspect the source entry itself with `symlink_metadata`: `Path::is_dir`
    // follows a symlink and could otherwise move/quarantine an arbitrary tree.
    let Some(source_root) = source.parent().and_then(std::path::Path::parent) else {
        return;
    };
    let Some(target_root) = target.parent().and_then(std::path::Path::parent) else {
        return;
    };
    if source_root != target_root
        || !sidecar_path_is_rooted(source_root, source)
        || !sidecar_path_is_rooted(source_root, target)
    {
        return;
    }
    let Ok(source_metadata) = std::fs::symlink_metadata(source) else {
        return;
    };
    if source_metadata.file_type().is_symlink() || !source_metadata.is_dir() {
        return;
    }
    if let Some(parent) = target.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            tracing::warn!(
                source = %source.display(),
                target = %target.display(),
                %error,
                "failed to create session sidecar relocation parent"
            );
            return;
        }
    }

    // Re-check after creating missing destination parents. This closes the
    // ordinary check-then-create window without ever following a newly
    // introduced symlink in the rooted path.
    if !sidecar_path_is_rooted(source_root, source) || !sidecar_path_is_rooted(source_root, target)
    {
        return;
    }

    let superseded = if std::fs::symlink_metadata(target).is_ok() {
        match superseded_sidecar_path(target) {
            Ok(path) => Some(path),
            Err(error) => {
                tracing::warn!(
                    source = %source.display(),
                    target = %target.display(),
                    %error,
                    "failed to quarantine occupied session sidecar"
                );
                return;
            }
        }
    } else {
        None
    };

    let move_result = match std::fs::rename(source, target) {
        Ok(()) => Ok(()),
        Err(error) if is_cross_device(&error) => {
            // A config directory can span mounts (notably desktop app
            // containers), so copy into the empty destination and remove the
            // source only after every regular child copied successfully.  Once
            // the copy succeeds, cleanup is best-effort and MUST NOT delete
            // the complete destination if source removal reports an error.
            match copy_then_cleanup_with(
                source,
                target,
                copy_session_sidecar,
                |path: &std::path::Path| std::fs::remove_dir_all(path),
            ) {
                Ok(()) => Ok(()),
                Err(SidecarCopyError::Copy(error)) => Err(error),
                Err(SidecarCopyError::Cleanup(error)) => {
                    tracing::warn!(
                        source = %source.display(),
                        target = %target.display(),
                        %error,
                        "session sidecar copied but source cleanup failed; keeping destination"
                    );
                    Ok(())
                }
            }
        }
        Err(error) => Err(error),
    };
    if let Err(error) = move_result {
        let _ = std::fs::remove_dir_all(target);
        if let Some(superseded) = superseded.as_ref() {
            let _ = std::fs::rename(superseded, target);
        }
        tracing::warn!(
            source = %source.display(),
            target = %target.display(),
            %error,
            "failed to move session sidecar"
        );
    }
}

impl ConversationOrchestrator {
    /// Seed a provider profile for the model already selected at construction.
    /// This is initialization, not a model switch, so it deliberately does not
    /// emit Pre/PostModelSwitch hooks.
    pub async fn seed_initial_model_profile(&self, model: &str, profile: &str) {
        let mut session = self.session.lock().await;
        if session.history.is_empty() && session.model == model {
            session.model_profile = Some(profile.to_string());
        } else {
            tracing::warn!(
                current_model = %session.model,
                requested_model = %model,
                "ignored late or mismatched initial model profile seed"
            );
        }
    }

    /// `tengu_api_success` `timeSinceLastApiCallMs`: ms since the previous
    /// successful API call, then record this call's timestamp. Returns `None`
    /// on the first call (claude `W=G!==null?Math.max(0,Math.round(M-G)):void 0`).
    #[allow(clippy::cast_sign_loss)]
    pub(crate) fn record_api_call_gap_ms(&self) -> Option<u64> {
        use std::sync::atomic::Ordering;
        let now_ms = i64::try_from(
            self.model_runtime
                .session_started_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .elapsed()
                .as_millis(),
        )
        .unwrap_or(i64::MAX);
        let prev = self
            .model_runtime
            .last_api_call_at_ms
            .swap(now_ms, Ordering::SeqCst);
        // `prev < 0` is the `-1` sentinel = no prior call → OMIT the field.
        (prev >= 0).then(|| (now_ms - prev).max(0) as u64)
    }

    /// Drop a recorded kind whose block never reaches persistence.
    ///
    /// Needed because `take_tool_denial_kind` REMOVES on read: when the
    /// streaming executor substitutes a synthetic result for a tool that
    /// already recorded a kind at dispatch, the recorded kind must either be
    /// rewritten to the synthetic's own kind or dropped, or it would both stamp
    /// the wrong provenance and leak an entry for the rest of the session.
    pub(crate) async fn remove_tool_denial_kind(&self, id: &protocol::ToolUseId) {
        self.transcript
            .tool_denial_kinds
            .lock()
            .await
            .remove(&id.to_string());
    }

    /// Record the ASSISTANT line uuid that carried this `tool_use` block —
    /// claude's `sourceToolAssistantUUID`.
    pub(crate) async fn record_source_tool_assistant_uuid(
        &self,
        id: &protocol::ToolUseId,
        assistant_line_uuid: String,
    ) {
        self.transcript
            .tool_source_assistant_uuids
            .lock()
            .await
            .insert(id.to_string(), assistant_line_uuid);
    }

    /// Remove and return the payloads queued for `id`, in production order.
    pub(crate) async fn take_queued_hook_attachments(
        &self,
        id: &protocol::ToolUseId,
    ) -> Vec<serde_json::Value> {
        self.transcript
            .pending_hook_attachments
            .lock()
            .await
            .remove(&id.to_string())
            .unwrap_or_default()
    }

    /// Take the recorded `sourceToolAssistantUUID` under the same guard.
    ///
    /// Emitted ONLY on a map HIT: `run_turn`'s parent derivation falls back to
    /// the turn's last assistant block uuid when the id is missing (a
    /// defensive, in-practice-unreachable branch), and writing a uuid that is
    /// not the tool_use's own line would be worse than omitting the key.
    pub(super) async fn take_source_tool_assistant_uuid(
        &self,
        msg: &ConversationMessage,
    ) -> Option<String> {
        let only = Self::sole_tool_result_id(msg)?;
        self.transcript
            .tool_source_assistant_uuids
            .lock()
            .await
            .remove(&only)
    }

    /// The configured session project root, independent of shell directory changes.
    pub fn project_root(&self) -> std::path::PathBuf {
        self.session_cwd.project_root()
    }

    /// The CURRENT working directory for hook payloads (the post-`cd` shell cwd
    /// when a firer is wired, else the static init `cwd`). Clones out of the
    /// shared cell so no lock is held across an await; a poisoned lock falls
    /// back to the static `cwd`.
    pub fn current_cwd(&self) -> std::path::PathBuf {
        self.current_cwd
            .lock()
            .map_or_else(|_| self.cwd.clone(), |g| g.clone())
    }

    /// Retarget the shared transcript writer for a newly mounted session id
    /// without relocating the previous session's file. Hot clear/resume uses
    /// this after destination cost hydration succeeds and before publishing
    /// the replacement conversation identity; ordinary `/cd` continues to use
    /// [`Self::retarget_transcript_for_cwd`], which performs a relocation.
    pub async fn retarget_transcript_for_session(
        &self,
        session_id: protocol::SessionId,
    ) -> Result<Option<std::path::PathBuf>, String> {
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            return Ok(None);
        };
        let Some(config_home) = self.config_home.as_ref() else {
            return Ok(Some(writer.active_path()));
        };
        let target = session::jsonl::path::session_path(
            config_home,
            &self.current_cwd().to_string_lossy(),
            &session_id.as_uuid().to_string(),
        );
        // Compatibility helper for direct/legacy callers. Production hot
        // clear/resume captures the destination authority first and uses
        // `activate_session_target_with_durable_lock` inside its owned commit,
        // alongside the prepared cost token and conversation identity.
        if writer.durable_transcript_enabled() {
            writer
                .activate_session_target(session_id, target.clone(), self.current_cwd())
                .map_err(|error| {
                    format!("activate transcript for session {session_id}: {error}")
                })?;
        } else {
            // Compatibility writers have no cross-process/session authority;
            // retain their historical in-process retarget behavior.
            writer.retarget(target.clone()).await;
        }
        Ok(Some(target))
    }

    /// Reflect a Fusion terminal that was already persisted by the host-owned
    /// durable recorder into the live history, without appending a second
    /// transcript row.  The recorder writes the immutable JSONL envelope first
    /// under the shared session transaction; this method only updates the
    /// in-memory model context when the originating session is still active.
    /// A hot clear/resume therefore leaves the durable result in its original
    /// session while the newly active session does not accidentally inherit it.
    pub async fn record_persisted_fusion_meta(
        &self,
        session_id: protocol::SessionId,
        message_uuid: &str,
        text: String,
    ) -> Result<(), String> {
        let message_id = protocol::MessageId::parse_prefixed(message_uuid)
            .ok_or_else(|| format!("invalid persisted Fusion message uuid {message_uuid:?}"))?;
        let _turn_guard = self.turn_gate.lock().await;
        let mut session = self.session.lock().await;
        if session.session_id != session_id {
            return Ok(());
        }
        if session
            .history
            .iter()
            .any(|message| message.id() == message_id)
        {
            return Ok(());
        }
        let message = protocol::ConversationMessage::user_meta(message_id, text);
        session.model_context_excluded_messages.insert(message_id);
        session.history.push(message);
        Ok(())
    }

    /// Serialize a background Fusion append with foreground chain ownership.
    /// The recorder's detached delivery task owns this future through fsync;
    /// its public timeout must not cancel the append/cursor transaction.
    /// Off-session delivery still uses its pinned target but never advances
    /// the currently selected session's cursor. Duplicate delivery likewise
    /// cannot rewind a cursor that has moved past the original durable row.
    pub async fn append_fusion_transcript(
        &self,
        writer: &session::jsonl::JsonlWriter,
        session_id: protocol::SessionId,
        delivery_id: &str,
        payload: serde_json::Value,
    ) -> Result<session::jsonl::TranscriptAppendOutcome, session::jsonl::TranscriptWriterError>
    {
        let uuid = payload
            .get("uuid")
            .and_then(serde_json::Value::as_str)
            .filter(|uuid| !uuid.is_empty())
            .ok_or(session::jsonl::TranscriptWriterError::MissingMessageUuid)?
            .to_string();
        let _turn = self.turn_gate.lock().await;
        let result = writer
            .append_json_once_durable_for_session_with_tip(session_id, delivery_id, payload)
            .await;
        self.reconcile_fusion_transcript_append(session_id, uuid, result)
            .await
    }

    /// Called only while holding `turn_gate`, including after a failed fresh
    /// append. A known complete visible row must enter the parent chain before
    /// a foreground turn can run, even while publication remains unacknowledged.
    pub(super) async fn reconcile_fusion_transcript_append(
        &self,
        session_id: protocol::SessionId,
        uuid: String,
        result: Result<
            (session::jsonl::TranscriptAppendOutcome, bool),
            session::jsonl::TranscriptWriterError,
        >,
    ) -> Result<session::jsonl::TranscriptAppendOutcome, session::jsonl::TranscriptWriterError>
    {
        // A write can reach the file before fsync reports failure. Its retry
        // is AlreadyPresent, but still must repair the unadvanced cursor if
        // the same locked scan proves no later UUID exists. Older duplicates
        // cannot rewind the chain once a foreground append has advanced it.
        let is_visible_tip = matches!(
            &result,
            Ok((_, true))
                | Err(session::jsonl::TranscriptWriterError::WrittenButNotDurable(
                    _
                ))
        );
        if is_visible_tip && self.session.lock().await.session_id == session_id {
            *self.transcript.last_jsonl_uuid.lock().await = Some(uuid);
        }
        // Advancing a visible cursor is not a durability acknowledgement.
        // Preserve the error so the recorder cannot mark this row Published.
        result.map(|(appended, _)| appended)
    }

    /// Persist a session-cwd move before directing future transcript appends
    /// into the cwd's project directory. The writer rehomes an existing
    /// transcript before appending the relocation marker; the complete
    /// per-session sidecar directory follows best-effort after that transaction.
    /// This is intentionally a no-op when session persistence is disabled (or
    /// the config home was not wired), preserving the in-memory/library setup.
    /// On successful persistence returns the writer's published transcript
    /// path, so a background owner can refresh its durable launch identity
    /// only after the transcript move has completed.
    pub async fn retarget_transcript_for_cwd(
        &self,
        cwd: &std::path::Path,
    ) -> Result<Option<std::path::PathBuf>, String> {
        let Some(writer) = self.transcript.jsonl_writer.as_ref() else {
            return Ok(None);
        };
        let Some(config_home) = self.config_home.as_ref() else {
            return Ok(None);
        };
        let session_id = self.session.lock().await.session_id;
        let session_uuid = session_id.as_uuid().to_string();
        let cwd = cwd.to_string_lossy().into_owned();
        let target = session::jsonl::path::session_path(config_home, &cwd, &session_uuid);
        let previous = writer.active_path();
        writer
            .retarget_with_relocation(target.clone(), &session_uuid, &cwd)
            .await
            .map_err(|error| format!("retarget transcript for cwd {cwd}: {error}"))?;
        // Hook/MCP output files and other session-scoped artifacts live beside
        // the transcript under `<session-id>/`. Keep the whole sidecar
        // directory with the rehomed transcript when the project directory
        // changes. This is deliberately best-effort: the transcript move is
        // the transactional operation; a sidecar move failure is logged but
        // must not strand the conversation in a half-accepted `/cd`.
        if previous != target {
            let old_sidecar = previous
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join(&session_uuid);
            let new_sidecar = target
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join(&session_uuid);
            move_session_sidecar_best_effort(&old_sidecar, &new_sidecar);
        }
        Ok(Some(writer.active_path()))
    }

    /// Deterministically derive this session's transcript path from the resolved
    /// claude-home + cwd + session id — the parity analog of claude-code's
    /// `getTranscriptPathForSession(sessionId)` (`utils/sessionStorage.ts:207`),
    /// which `createBaseHookInput` (`utils/hooks.ts:322`) ALWAYS stamps onto every
    /// hook payload. Shape: `<config_home>/projects/<sanitize(cwd)>/<uuid>.jsonl`
    /// (= `session::jsonl::path::session_path`). The id is formatted as the BARE
    /// uuid (`SessionId::as_uuid`) to match claude-code's `${sessionId}.jsonl` and
    /// the on-disk JSONL filename the writer/loader use — NOT the `sess:`-prefixed
    /// [`std::fmt::Display`] form. Returns an empty path when no `config_home` is
    /// wired (test/library builds that also wire no writer), preserving the prior
    /// `""` hook field for those.
    #[must_use]
    pub(crate) fn computed_transcript_path(&self, session_id: &SessionId) -> std::path::PathBuf {
        match &self.config_home {
            Some(home) => session::jsonl::path::session_path(
                home,
                &self.cwd.to_string_lossy(),
                &session_id.as_uuid().to_string(),
            ),
            None => std::path::PathBuf::new(),
        }
    }

    /// Set the mid-turn input source on a SHARED orchestrator (`&self`), so the
    /// bridge can wire its per-connection queue adapter AFTER `harness_runtime::desktop::build`
    /// returns the orchestrator as an `Arc`. Set-once: a second call is ignored
    /// (the first wiring wins). See [`Self::with_mid_turn_input`].
    pub fn set_mid_turn_input(
        &self,
        source: Arc<dyn crate::prompt::mid_turn_input::MidTurnInputSource>,
    ) {
        let _ = self.mid_turn_input.set(source);
    }

    /// Set the abort-reason flag on a SHARED orchestrator (`&self`). Set-once.
    /// See [`Self::with_cancel_reason`].
    pub fn set_cancel_reason(&self, flag: crate::prompt::mid_turn_input::CancelReasonFlag) {
        let _ = self.cancel_reason.set(flag);
    }

    /// Byte-exact `/recap` prompt (probed from the 2.1.198 binary). Sent as the
    /// single user turn of the isolated recap side query. Kept in lockstep with
    /// the byte-audit copy in `command-core`'s `recap.rs` test.
    pub(crate) const RECAP_PROMPT: &str = "The user stepped away and is coming back. Recap in under 40 words, 1-2 plain sentences, no markdown. Lead with the overall goal and current task, then the one next action. Skip root-cause narrative, fix internals, secondary to-dos, and em-dash tangents.";

    /// Real `/recap` body — a read-only, tool-denied, single-turn side query,
    /// cancelable via a [`CancellationToken`]. HISTORY-INERT by construction:
    /// unlike [`Self::force_compact_with_cancel`], it reuses the
    /// [`sidequery::ForkedAgentRunner`] directly (the SAME single-turn primitive
    /// the autocompact summarizer uses) and NEVER touches `session.history`,
    /// `apply_post_compact`, `save_cache_safe_params`, or the pre/post-compact
    /// hooks. The runner replays `cache_safe_params.fork_context_messages` +
    /// the recap prompt, exposes no tools, and issues exactly one query.
    ///
    /// On cancel returns `Ok(RecapOutcome::Cancelled)` (a fixed-string outcome),
    /// not `Err`. An empty cache-safe slot (no successful turn recorded yet — a
    /// resumed session whose transcript loaded but which has run no live turn)
    /// maps to `Err(ActionFailed)` rather than a panic.
    pub(crate) async fn generate_recap_query(
        &self,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<platform_api::RecapOutcome, platform_api::HandleError> {
        let runner = self
            .recap_runner
            .clone()
            .ok_or_else(|| platform_api::HandleError::ActionFailed("recap unavailable".into()))?;
        let params = self
            .model_runtime
            .cache_safe_slot
            .as_ref()
            .ok_or_else(|| {
                platform_api::HandleError::ActionFailed("recap: no cache-safe slot".into())
            })?
            .get_last()
            .await
            .ok_or_else(|| {
                platform_api::HandleError::ActionFailed("recap: no cache-safe params".into())
            })?;

        // Fast-path cancel (deterministic even when the runner completes
        // synchronously, e.g. the stub side-query path), mirroring
        // `force_compact_with_cancel`.
        if cancel.is_cancelled() {
            return Ok(platform_api::RecapOutcome::Cancelled);
        }

        let req = sidequery::ForkedAgentRequest {
            prompt_messages: vec![ConversationMessage::user(
                MessageId::new(),
                Self::RECAP_PROMPT.to_string(),
            )],
            cache_safe_params: params,
            fork_label: "recap".into(),
            query_source: sidequery::QuerySource::Custom("recap".into()),
            max_output_tokens: Some(256),
        };

        tokio::select! {
            biased;
            () = cancel.cancelled() => Ok(platform_api::RecapOutcome::Cancelled),
            r = runner.run(req) => match r {
                Ok(res) => Ok(platform_api::RecapOutcome::Text(res.final_text.trim().to_string())),
                Err(e) => Err(platform_api::HandleError::ActionFailed(e.to_string())),
            }
        }
    }

    /// Claude Code 2.1.217's `rename_generate_name` prompt. The response is a
    /// JSON object with a single `name` field; this side query is history-inert
    /// and tool-less, sharing the same fork runner as `/recap`.
    pub(crate) const SESSION_NAME_PROMPT: &str = "Generate a short kebab-case name (2-4 words) that captures the main topic of this conversation. Use lowercase words separated by hyphens. Examples: \"fix-login-bug\", \"add-auth-feature\", \"refactor-api-client\", \"debug-test-failures\". Return JSON with a \"name\" field.";
    pub(crate) const PROMPT_SUGGESTION_PROMPT: &str = r#"[SUGGESTION MODE: Suggest what the user might naturally type next into Claude Code.]

FIRST: Look at the user's recent messages and original request.

Your job is to predict what THEY would type - not what you think they should do.

THE TEST: Would they think "I was just about to type that"?

EXAMPLES:
User asked "fix the bug and run tests", bug is fixed → "run the tests"
After code written → "try it out"
Claude offers options → suggest the one the user would likely pick, based on conversation
Claude asks to continue → "yes" or "go ahead"
Task complete, obvious follow-up → "commit this" or "push it"
After error or misunderstanding → silence (let them assess/correct)

Be specific: "run the tests" beats "continue".

NEVER SUGGEST:
- Evaluative ("looks good", "thanks")
- Questions ("what about...?")
- Claude-voice ("Let me...", "I'll...", "Here's...")
- New ideas they didn't ask about
- Multiple sentences

Stay silent if the next step isn't obvious from what the user said.

Stay silent if a suggestion could be unsafe or inappropriate — including any sensitive topic (security incidents, credentials, harm, private data). Even when the user is doing legitimate security or cybersecurity work, do not predict potentially unsafe actions.

Format: 2-12 words, match the user's style. Or nothing.

Reply with ONLY the suggestion, no quotes or explanation."#;

    pub(crate) async fn generate_session_name_query(
        &self,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Option<String>, platform_api::HandleError> {
        let runner = self.recap_runner.clone().ok_or_else(|| {
            platform_api::HandleError::ActionFailed("session name generation unavailable".into())
        })?;
        let Some(params) = self
            .model_runtime
            .cache_safe_slot
            .as_ref()
            .ok_or_else(|| {
                platform_api::HandleError::ActionFailed(
                    "session name generation unavailable".into(),
                )
            })?
            .get_last()
            .await
        else {
            return Ok(None);
        };

        if cancel.is_cancelled() {
            return Ok(None);
        }
        let req = sidequery::ForkedAgentRequest {
            prompt_messages: vec![ConversationMessage::user(
                MessageId::new(),
                Self::SESSION_NAME_PROMPT.to_string(),
            )],
            cache_safe_params: params,
            fork_label: "rename".into(),
            query_source: sidequery::QuerySource::Custom("rename_generate_name".into()),
            max_output_tokens: Some(128),
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => Ok(None),
            result = runner.run(req) => match result {
                Ok(result) => parse_generated_session_name(&result.final_text)
                    .map(Some)
                    .ok_or_else(|| platform_api::HandleError::ActionFailed(
                        "session name response did not contain a non-empty name".into()
                    )),
                Err(error) => Err(platform_api::HandleError::ActionFailed(error.to_string())),
            }
        }
    }

    /// Run the history-inert post-turn prompt-suggestion side query.
    pub async fn generate_prompt_suggestion_query(
        &self,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Option<String>, platform_api::HandleError> {
        let runner = self.recap_runner.clone().ok_or_else(|| {
            platform_api::HandleError::ActionFailed(
                "prompt suggestion generation unavailable".into(),
            )
        })?;
        let Some(mut params) = self
            .model_runtime
            .cache_safe_slot
            .as_ref()
            .ok_or_else(|| {
                platform_api::HandleError::ActionFailed(
                    "prompt suggestion generation unavailable".into(),
                )
            })?
            .get_last()
            .await
        else {
            return Ok(None);
        };

        // The cache-safe slot is captured before the current assistant reply is
        // appended. Prompt suggestions run after the turn, so extend the
        // prefix with the live tail exactly as the oracle's post-turn `tce(e)`
        // snapshot does.
        let history = self.session.lock().await.model_context_history();
        extend_session_memory_fork_context(&mut params.fork_context_messages, &history);
        if history
            .iter()
            .filter(|message| matches!(message, ConversationMessage::Assistant { .. }))
            .count()
            < 2
        {
            return Ok(None);
        }
        if last_response_is_api_error(&history) {
            return Ok(None);
        }
        if self.permission_mode().as_deref() == Some("plan") {
            return Ok(None);
        }
        if self
            .snapshot_cost_real()
            .await
            .current_usage
            .is_some_and(|usage| {
                usage
                    .input_tokens
                    .saturating_add(usage.cache_creation_input_tokens)
                    .saturating_add(usage.output_tokens)
                    > 10_000
            })
        {
            return Ok(None);
        }

        if cancel.is_cancelled() {
            return Ok(None);
        }

        let req = sidequery::ForkedAgentRequest {
            prompt_messages: vec![ConversationMessage::user(
                MessageId::new(),
                Self::PROMPT_SUGGESTION_PROMPT.to_string(),
            )],
            cache_safe_params: params,
            fork_label: "prompt_suggestion".into(),
            query_source: sidequery::QuerySource::PromptSuggestion,
            max_output_tokens: None,
        };

        tokio::select! {
            biased;
            () = cancel.cancelled() => Ok(None),
            result = runner.run(req) => match result {
                Ok(result) => Ok(parse_prompt_suggestion_response(&result.final_text)),
                Err(error) => Err(platform_api::HandleError::ActionFailed(error.to_string())),
            }
        }
    }

    /// Byte-faithful `/btw` side-question wrapper, ported verbatim from
    /// claude-code `utils/sideQuestion.ts`: the `<system-reminder>` that turns
    /// the shared context into a one-off, tool-denied answer. Prepended (with a
    /// blank line) to the user's question as the single user turn of the
    /// isolated side query.
    pub(crate) const SIDE_QUESTION_SYSTEM_REMINDER: &str = "<system-reminder>This is a side question from the user. You must answer this question directly in a single response.\n\nIMPORTANT CONTEXT:\n- You are a separate, lightweight agent spawned to answer this one question\n- The main agent is NOT interrupted - it continues working independently in the background\n- You share the conversation context but are a completely separate instance\n- Do NOT reference being interrupted or what you were \"previously doing\" - that framing is incorrect\n\nCRITICAL CONSTRAINTS:\n- You have NO tools available - you cannot read files, run commands, search, or take any actions\n- This is a one-off response - there will be no follow-up turns\n- You can ONLY provide information based on what you already know from the conversation context\n- NEVER say things like \"Let me try...\", \"I'll now...\", \"Let me check...\", or promise to take any action\n- If you don't know the answer, say so - do not offer to look it up or investigate\n\nSimply answer the question with the information you have.</system-reminder>";

    /// Read-only, single-turn `/btw` query using the same fork runner as recap.
    /// Never changes history, the main cache-safe slot, or compaction hooks.
    /// Tool schemas preserve the cache prefix, but tool calls are denied and
    /// the one-shot runner never starts a follow-up loop.
    ///
    /// Prefer the captured prefix. Before the first live turn after resume,
    /// rebuild from current session state like cc's
    /// `buildSideQuestionFallbackParams`, without writing to the main slot.
    pub(crate) async fn answer_side_question_query(
        &self,
        question: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<platform_api::RecapOutcome, platform_api::HandleError> {
        let runner = self.recap_runner.clone().ok_or_else(|| {
            platform_api::HandleError::ActionFailed("side question unavailable".into())
        })?;
        if cancel.is_cancelled() {
            return Ok(platform_api::RecapOutcome::Cancelled);
        }
        let saved = match self.model_runtime.cache_safe_slot.as_ref() {
            Some(slot) => slot.get_last().await,
            None => None,
        };
        // Match the interactive `/btw` builder: frozen system/context bytes,
        // but current tools, model options, and post-compaction messages.
        let system = match saved.as_ref() {
            Some(params) => params.system_prompt.to_string(),
            None => self.effective_system_prompt().await,
        };
        let model = self.session.lock().await.model.clone();
        let tools = self.build_wire_tools().await;
        let mut params = self
            .build_cache_safe_params(Some(&system), &model, &tools)
            .await;
        if let Some(saved) = saved {
            params.user_context = saved.user_context;
            params.system_context = saved.system_context;
            params.user_context_message = saved.user_context_message;
            // A captured slot is the authoritative parent request for cache
            // identity. Keep its ordered schemas even if the live registry
            // has changed while the session was idle.
            params.tools = saved.tools;
        }
        // A partial streamed assistant reply is not a complete prefix.
        if matches!(
            params.fork_context_messages.last(),
            Some(ConversationMessage::Assistant {
                stop_reason: None,
                ..
            })
        ) {
            params.fork_context_messages.pop();
        }

        let wrapped = format!("{}\n\n{}", Self::SIDE_QUESTION_SYSTEM_REMINDER, question);
        let req = sidequery::ForkedAgentRequest {
            prompt_messages: vec![ConversationMessage::user(MessageId::new(), wrapped)],
            cache_safe_params: params,
            fork_label: "side_question".into(),
            query_source: sidequery::QuerySource::Custom("side_question".into()),
            // One turn like cc's `runSideQuestion` (maxTurns=1); `None` uses
            // the runner's standard single-turn output cap.
            max_output_tokens: None,
        };

        tokio::select! {
            biased;
            () = cancel.cancelled() => Ok(platform_api::RecapOutcome::Cancelled),
            r = runner.run(req) => match r {
                Ok(res) => {
                    // Claude's extractor prefers any non-empty text block and
                    // only surfaces a denied tool call when the assistant had
                    // no text at all. Preserve that ordering for mixed
                    // [thinking, text, tool_use] responses.
                    let text = res.final_text.trim().to_string();
                    if text.is_empty() {
                        if let Some(tool) = res
                            .tool_calls
                            .first()
                            .and_then(|call| call.get("name"))
                            .and_then(serde_json::Value::as_str)
                        {
                            return Ok(platform_api::RecapOutcome::Text(format!(
                                "(The model tried to call {tool} instead of answering directly. Try rephrasing or ask in the main conversation.)"
                            )));
                        }
                    }
                    Ok(platform_api::RecapOutcome::Text(text))
                }
                Err(sidequery::ForkError::Api(error)) => Ok(platform_api::RecapOutcome::Text(
                    format!("(API error: {error})"),
                )),
                Err(e) => Err(platform_api::HandleError::ActionFailed(e.to_string())),
            }
        }
    }

    /// Snapshot the cache-safe prompt prefix into the wired slot after a
    /// successful API call (In-Loop Compaction Batch 6).
    ///
    /// Strict no-op when no slot is wired (every test + any binary that has not
    /// wired the forked runner), so it adds zero work — and crucially no history
    /// clone — off the production path. When wired, it stores the CURRENT
    /// `session.history` as `fork_context_messages`. Callers seed it before a
    /// request so first-call overflow can compact, then refresh it after a
    /// successful call before appending the assistant reply. Transient request
    /// reminders stay outside this stored history prefix.
    ///
    /// Freeze the tools from this request too: rebuilding or removing them in
    /// a compaction fork changes the cached prefix.
    pub(crate) async fn save_cache_safe_params(
        &self,
        system: Option<&str>,
        model: &str,
        tools: &[serde_json::Value],
    ) {
        let Some(slot) = self.model_runtime.cache_safe_slot.as_ref() else {
            return;
        };
        slot.save(self.build_cache_safe_params(system, model, tools).await)
            .await;
    }

    /// Assemble a prefix without publishing it as a main-thread request.
    async fn build_cache_safe_params(
        &self,
        system: Option<&str>,
        model: &str,
        tools: &[serde_json::Value],
    ) -> sidequery::CacheSafeParams {
        let (fork_context_messages, session_id, model_profile) = {
            let s = self.session.lock().await;
            (
                s.model_context_history(),
                s.session_id,
                s.model_profile.clone(),
            )
        };
        let transcript_path = self
            .transcript
            .jsonl_writer
            .as_ref()
            .map(|writer| writer.active_path())
            .unwrap_or_else(|| self.computed_transcript_path(&session_id));
        let effort = self
            .model_runtime
            .current_effort
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .map(serde_json::Value::String);
        let user_context_message = self.additional_context_message().await;
        sidequery::CacheSafeParams {
            system_prompt: system.unwrap_or("").into(),
            tools: tools.to_vec(),
            effort,
            user_context: std::collections::HashMap::new(),
            system_context: std::collections::HashMap::new(),
            user_context_message,
            tool_use_options: tool_api::ToolUseOptions {
                debug: false,
                verbose: false,
                main_loop_model: model.to_string(),
                model_profile,
                max_budget_nano_usd: None,
                mcp_clients: Vec::new(),
                is_non_interactive_session: false,
                custom_system_prompt: None,
                append_system_prompt: None,
            },
            fork_context_messages,
            transcript_path: (!transcript_path.as_os_str().is_empty()).then_some(transcript_path),
            // Overwritten by the slot on save; the value here is irrelevant.
            generation: 0,
        }
    }

    /// Seed a resumed session before its first proactive summary can run.
    pub(crate) async fn seed_compact_cache_safe_params(&self, system: Option<&str>) {
        if self.model_runtime.cache_safe_slot.is_none() {
            return;
        }
        let model = self.session.lock().await.model.clone();
        let tools = self.build_wire_tools().await;
        self.save_cache_safe_params(system, &model, &tools).await;
    }

    /// FORK (codex #5 follow-up): record the rendered system-prompt bytes this
    /// turn handed the model, so a fork-subagent spawn dispatched later in the
    /// SAME turn can thread them onto its child (cache-identical prefix, claude
    /// `AgentTool.tsx:622-623`). Called by the turn drivers right after a
    /// successful API call (the `save_cache_safe_params` site). `None`/empty
    /// system collapses to `None` (a turn with no system prompt records nothing).
    pub(crate) async fn save_current_turn_system_prompt(&self, system: Option<&str>) {
        *self.prompt_runtime.current_turn_system_prompt.lock().await =
            system.filter(|s| !s.is_empty()).map(ToString::to_string);
    }

    /// FORK (codex #5 follow-up): the rendered system prompt recorded by the most
    /// recent successful turn ([`Self::save_current_turn_system_prompt`]), or
    /// `None` before the first successful turn / when that turn had no system
    /// prompt. Read by the fork dispatch path to seed each tool's
    /// `ToolUseContext::fork_parent_system_prompt`.
    pub(crate) async fn current_turn_system_prompt(&self) -> Option<String> {
        self.prompt_runtime
            .current_turn_system_prompt
            .lock()
            .await
            .clone()
    }

    /// Task 8 (llm-runtime future-work batch 3): forward the API client's
    /// latest unified rate-limit header snapshot to
    /// [`platform_api::OutputStream::emit_rate_limit`], emitting ONLY when it
    /// differs from the last emitted value (emit-on-change dedup against
    /// [`Self::last_emitted_rate_limit`]).
    ///
    /// Called by the turn drivers after each completed API call — the
    /// batched/cancelable seam in `turn_loop::execute_one_turn_with_recovery_tracked`
    /// and the streaming seam in `try_run_turn_streaming` (both right after
    /// `save_cache_safe_params`, the existing "API call succeeded" point).
    /// `self.api` and `self.streaming_api` are the same `ProviderApiAdapter`
    /// in production, and the adapter records headers on both
    /// `drive_non_stream` and the `drive_stream` connect-success path, so
    /// reading `self.api` covers both drivers.
    ///
    /// A strict no-op when the client has no snapshot (the default
    /// `last_rate_limit_full()` returns `None` — mocks / non-Anthropic
    /// providers), so all pre-existing fixtures see zero extra events.
    pub(crate) async fn emit_rate_limit_if_changed(&self) {
        let Some(info) = self.api.last_rate_limit_full() else {
            return;
        };
        let mut last = self.model_runtime.last_emitted_rate_limit.lock().await;
        if last.as_ref() == Some(&info) {
            return;
        }
        self.output
            .emit_rate_limit(
                info.status.as_deref(),
                info.rate_limit_type.as_deref(),
                info.utilization,
                info.resets_at,
                info.claim_resets_at,
                info.overage_status.as_deref(),
                info.overage_resets_at,
                info.overage_disabled_reason.as_deref(),
                info.fallback_available,
                info.upgrade_paths.as_deref(),
                info.credits_required,
            )
            .await;
        *last = Some(info);
    }

    /// Task 2 (llm-runtime future-work batch 5): forward the API client's
    /// latest RAW per-window utilization snapshot to
    /// [`platform_api::OutputStream::emit_raw_utilization`] when it CHANGED since
    /// the last emit. The empty snapshot is significant: it clears a
    /// previously-rendered utilization window in downstream clients.
    ///
    /// Called immediately next to [`Self::emit_rate_limit_if_changed`] at
    /// both turn-driver seams (the batched/cancelable funnel in
    /// `turn_loop::execute_one_turn_with_recovery_tracked` and the streaming
    /// seam in `try_run_turn_streaming`), reading the same
    /// `self.api`-cached snapshot source.
    ///
    /// TS updates `rawUtilization` unconditionally on every headers pass
    /// (`claudeAiLimits.ts:476` and the 429 path `:500`) because it is module
    /// state polled by `getRawUtilization()`. Our event channel emits only on
    /// change to avoid spamming the stream, while still forwarding the empty
    /// snapshot as `(None, None, None, None)` so event-driven clients observe
    /// the same state transition.
    ///
    /// Atomic-window invariant: each window contributes either both `Some`
    /// values or both `None` — guaranteed by construction, since
    /// [`crate::model::rate_limit::RawWindow`] only exists with both fields.
    pub(crate) async fn emit_raw_utilization_if_changed(&self) {
        let Some(raw) = self.api.last_raw_utilization() else {
            return;
        };
        let mut last = self.model_runtime.last_emitted_raw_utilization.lock().await;
        if last.as_ref() == Some(&raw) {
            return;
        }
        self.output
            .emit_raw_utilization(
                raw.five_hour.map(|w| w.utilization),
                raw.five_hour.map(|w| w.resets_at),
                raw.seven_day.map(|w| w.utilization),
                raw.seven_day.map(|w| w.resets_at),
            )
            .await;
        *last = Some(raw);
    }

    /// Read the current cost state from the wired tracker, if any.
    /// Returns `None` if no tracker was attached. Exposed so future M7
    /// renderers (per-model breakdown view) can access
    /// `CostState.per_model_usage` without going through the leaf-friendly
    /// [`platform_api::CostSnapshot`] projection. (M6-06)
    pub async fn cost_state(&self) -> Option<cost::CostState> {
        let t = self.model_runtime.cost_tracker.as_ref()?;
        Some(t.snapshot().await)
    }

    /// Seed the wired [`cost::CostTracker`]'s cumulative total from a restored
    /// session (resume). No-op when no tracker is wired. Paired with the CLI
    /// mount's project-config `lastCost`/`lastSessionId` persistence so a
    /// `--resume`d session's footer continues from the prior accumulated cost
    /// instead of resetting to `$0.0000` (claude-code `restoreCostStateForSession`).
    pub async fn restore_session_cost(&self, total_nano_usd: u64) {
        // Keep the target id on one side of a clear/resume boundary. Callers
        // generally invoke this during boot, but the handle is public and may
        // be used while the runtime is live. Hydration addresses the captured
        // cell directly; it must never retarget the active projection itself.
        let _turn_guard = self.turn_gate.lock().await;
        if let Some(tracker) = self.model_runtime.cost_tracker.as_ref() {
            let session_id = self.session.lock().await.session_id;
            tracker
                .restore_total_for_session(session_id, total_nano_usd)
                .await;
        }
    }

    /// Project the wired [`cost::CostTracker`] state onto the
    /// leaf-friendly [`platform_api::CostSnapshot`]. Used by both the
    /// trait method `snapshot_cost` and the per-turn end-of-turn emitter
    /// (`OutputStream::emit_end_turn`). (M6-06)
    ///
    /// If no tracker is wired, returns a zero-valued snapshot keyed to
    /// the current session id (backward-compat shape).
    pub async fn snapshot_cost_real(&self) -> platform_api::CostSnapshot {
        let session_id = self.session.lock().await.session_id;
        let loops = match &self.model_runtime.loop_usage {
            Some(provider) => provider.usage_rows().await,
            None => Vec::new(),
        };
        let Some(tracker) = self.model_runtime.cost_tracker.as_ref() else {
            return platform_api::CostSnapshot {
                session_id,
                loops,
                ..platform_api::CostSnapshot::default()
            };
        };
        // Pin the tracker view to the session id captured above. Clear/resume
        // can switch the active projection between awaits; reading the active
        // tracker here would otherwise return session B's costs labeled as A.
        let state = tracker.scoped(session_id).snapshot().await;
        // Sum per-model usage into aggregate token counters. api_calls comes
        // from our own counter because cost::Usage does not carry a
        // per-call count (its `add()` merges token totals only).
        let (mut input_tokens, mut output_tokens, mut cache_read_tokens, mut cache_creation_tokens) =
            (0u64, 0u64, 0u64, 0u64);
        for entry in state.per_model_usage.values() {
            input_tokens = input_tokens.saturating_add(entry.usage.tokens.input);
            output_tokens = output_tokens.saturating_add(entry.usage.tokens.output);
            cache_read_tokens = cache_read_tokens.saturating_add(entry.cache_read_input_tokens);
            cache_creation_tokens =
                cache_creation_tokens.saturating_add(entry.cache_creation_input_tokens);
        }
        let api_calls = self
            .model_runtime
            .api_calls_recorded
            .load(std::sync::atomic::Ordering::SeqCst);
        #[allow(clippy::cast_precision_loss)]
        let total_usd = (state.total_nano_usd as f64) / 1_000_000_000.0;
        let session_duration = self
            .model_runtime
            .session_started_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed();
        // CLI-4: render the prompt-cache line here, where both the ledger and
        // `cost::render` are in scope. `None` before the first response, and
        // the line is then omitted rather than shown empty.
        let prompt_cache_line = {
            let ledger = self.model_runtime.prompt_cache_ledger.lock().await;
            let now_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
            cost::render::prompt_cache_line(
                &ledger.summary(now_ms),
                ledger.estimate_recache_tokens(),
                now_ms,
            )
        };
        platform_api::CostSnapshot {
            session_id,
            prompt_cache_line,
            total_nano_usd: state.total_nano_usd,
            total_tokens: input_tokens.saturating_add(output_tokens),
            total_usd,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            api_calls,
            session_duration,
            api_duration: std::time::Duration::from_millis(state.total_api_duration_ms),
            code_lines_added: state.total_lines_added,
            code_lines_removed: state.total_lines_removed,
            by_model: state
                .per_model_usage
                .values()
                .map(|mu| platform_api::orchestrator::ModelUsageRow {
                    model: mu.model_ref.model.clone(),
                    // (cc 2.1.218) `n.provider=n_(r)` — the serving provider,
                    // pre-stringified so the transport row stays cost-free.
                    provider: Some(mu.model_ref.provider.usage_wire_name()),
                    total_nano_usd: mu.cost_nano_usd,
                    input_tokens: mu.usage.tokens.input,
                    output_tokens: mu.usage.tokens.output,
                    cache_read_input_tokens: mu.cache_read_input_tokens,
                    cache_creation_input_tokens: mu.cache_creation_input_tokens,
                })
                .collect(),
            unknown_models: !state.unpriced_models.is_empty(),
            current_usage: state
                .last_usage
                .map(|usage| platform_api::CurrentUsageSnapshot {
                    input_tokens: usage.tokens.input,
                    output_tokens: usage.tokens.output,
                    cache_read_input_tokens: state.last_cache_read_input_tokens,
                    cache_creation_input_tokens: state.last_cache_creation_input_tokens,
                }),
            loops,
        }
    }

    /// True when a `max_budget_nano_usd` cost ceiling is set AND the session's
    /// cumulative cost has reached it — 1:1 with claude-code
    /// `getTotalCost() >= maxBudgetUsd` (`QueryEngine.ts:972`). Always false when
    /// no cap is set, OR when no [`cost::CostTracker`] is wired (the cap cannot
    /// be enforced without cost tracking — a headless `--max-budget` run, which
    /// wires the tracker, is the primary consumer). Checked at each turn-loop
    /// iteration so a run stops once it crosses the ceiling.
    pub(super) async fn over_budget(&self) -> bool {
        let Some(budget) = self.config.max_budget_nano_usd else {
            return false;
        };
        let Some(tracker) = self.model_runtime.cost_tracker.as_ref() else {
            return false;
        };
        tracker.snapshot().await.total_nano_usd >= budget
    }

    /// A3: construct a fresh [`BudgetTracker`] for this turn IFF the
    /// token-budget feature is enabled AND a positive budget is configured.
    ///
    /// Returns `None` (the parity default) when
    /// [`OrchestratorConfig::enable_token_budget`] is `false` or
    /// [`OrchestratorConfig::token_budget`] is `None`/`Some(0)` — in which case
    /// the turn drivers skip the budget check entirely and stop at the first
    /// `end_turn`, preserving the locked turn-loop behaviour.
    pub(super) fn new_budget_tracker(&self) -> Option<BudgetTracker> {
        if self.config.enable_token_budget && matches!(self.config.token_budget, Some(b) if b > 0) {
            Some(BudgetTracker::new())
        } else {
            None
        }
    }

    /// A3: consult the token budget at a natural end-of-turn.
    ///
    /// Returns `true` if the loop should CONTINUE (a continuation nudge was
    /// injected as a meta user message and the A1 recovery count was reset per
    /// `query.ts:1332`); `false` if the loop should stop (budget off, agent
    /// context, threshold reached, or diminishing returns).
    ///
    /// 1:1 with TS `query.ts:1308-1355`: gated by `feature('TOKEN_BUDGET')`,
    /// drives [`check_token_budget`], and on `continue` appends a meta user
    /// message carrying the byte-exact `getBudgetContinuationMessage` nudge.
    /// The completion telemetry is emitted as a `tracing` event on stop.
    /// The continuation nudge is injected as a META user message
    /// ([`Self::inject_meta_user_message`] / [`ConversationMessage::user_meta`])
    /// into both the live session history and the JSONL persistence stream,
    /// where it persists with top-level `isMeta:true`. The same META injection
    /// pattern is shared by the malformed-tool-use retry (#77), thinking-only
    /// (#78), and max-output-tokens recovery nudges.
    /// Finding #80: refusal → fallback-model swap (claude-code `bin/claude.exe`
    /// offset ~205871579, `vr === "refusal" && rc !== void 0`). When the active
    /// turn's response has `stop_reason == "refusal"`, a
    /// [`OrchestratorConfig::refusal_fallback_model`] is configured, AND the
    /// once-per-session latch ([`Self::refusal_fallback_latched`]) is not yet set:
    ///
    /// 1. set the latch (so the fallback fires at most once per session — the
    ///    binary's `refusalFallbackModelLatch` makes the override sticky);
    /// 2. persistently swap the session model to the fallback (the binary's
    ///    `setAppState mainLoopModel = fallbackModel` + `jT(fallbackModel)` —
    ///    every subsequent turn re-snapshots `session.model`, so the swap sticks);
    /// 3. surface the user-visible warning on the output stream (the binary's
    ///    `type:"system", subtype:"model_refusal_fallback", level:"warning",
    ///    content: Bwn(originalModel, fallbackModel, category)`), reproduced
    ///    byte-exact for the common `category == "other"` shape.
    ///
    /// Returns `true` when the swap happened (the caller must retry/continue the
    /// turn against the fallback model), `false` when no fallback is configured or
    /// the latch is already set (the caller preserves today's terminal behavior).
    /// The user-visible refusal-fallback line (2.1.206 `VPn(e,t,r)` for the
    /// common `category == "other"` path: the generic `$7m` prefix, then
    /// `Switched to {marketing name}`, then the feedback line).
    ///
    /// Lives here rather than inline so the emit site and the tests read the
    /// same bytes. The cyber/bio "intentionally broad" variant still needs the
    /// refusal category routed through — see the typed notice's
    /// `api_refusal_category`, which now carries it.
    pub(crate) fn refusal_warning_text(fallback: &str) -> String {
        let display = crate::prompt::env_meta::marketing_name_for_model(fallback)
            .map(String::from)
            .unwrap_or_else(|| fallback.to_string());
        format!(
            "This model's safeguards flagged this message. \
This sometimes happens with safe, normal conversations. Switched to {display}. \
Send feedback with /feedback or learn more: https://support.claude.com/en/articles/15363606"
        )
    }

    pub(crate) async fn maybe_swap_to_refusal_fallback(&self) -> bool {
        if crate::scheduled_turn::current().is_some() {
            return false;
        }
        // The CASCADE: an ordered chain of models, each tried as the previous
        // one refuses. An empty chain falls back to the historical single
        // `refusal_fallback_model`, which is exactly a one-element chain — so
        // the default path is byte-identical to before the cascade existed.
        let chain = self.config.refusal_chain();
        let current_model = { self.session.lock().await.model.clone() };
        let notice_uuid = uuid::Uuid::new_v4().to_string();
        // Routing, the once-per-session latch, the tried set and the notice
        // accumulate/collapse pair all live in `platform_api::refusal_driver`,
        // because the subagent runner needs to behave identically and cannot
        // depend on this crate.
        let hop = {
            let mut cascade = self.model_runtime.refusal_cascade.lock().await;
            cascade.next_hop(&chain, &current_model, notice_uuid.clone())
        };
        // Report every stage the walk passed over. A chain that silently
        // degraded to its last entry is otherwise indistinguishable from one
        // that worked first try.
        for report in hop.as_ref().map_or(&[][..], |h| &h.declines) {
            tracing::info!(
                event = "tengu_refusal_fallback_route_declined",
                reason = report.as_str(),
            );
        }
        let Some(hop) = hop else {
            return false;
        };
        let fallback = hop.fallback_model;
        // Persistently swap the session model to the fallback.
        let (original_model, original_profile) = {
            let mut s = self.session.lock().await;
            let prev = std::mem::replace(&mut s.model, fallback.clone());
            let previous_profile = s.model_profile.take();
            // The fallback model has no associated provider profile (mirrors the
            // overload-fallback re-issue, which passes `profile = None`).
            (prev, previous_profile)
        };
        if original_model != fallback || original_profile.is_some() {
            self.run_post_model_switch_hooks(&original_model, &fallback, None, None, "auto")
                .await;
        }
        // User-visible warning. 2.1.206 `VPn(e,t,r)` =
        //   `${f_t(r) ? mmi(e) : hmi(e,r)} Switched to ${Mf(t)}. ${bxr(e)}`
        // for the common `category == "other"` path.
        let emitted = hop.notices;
        for e in emitted {
            if e.suppressed_count > 0 {
                tracing::info!(
                    event = "tengu_refusal_fallback_notice_collapsed",
                    suppressed_count = e.suppressed_count,
                    emitted_via = e.emitted_via.as_str(),
                );
            }
            let content = Self::refusal_warning_text(&e.banner.serving_model);
            self.output.emit_text(&content).await;
            // claude-code carries this notice as a TYPED system message in the
            // conversation (`Dcr`, `src_163219561.js`), not only as banner text.
            // Emitting it on the stream alone made it ephemeral: it never
            // reached the transcript, so it was gone on resume and nothing
            // could see WHICH earlier notices a later hop superseded —
            // `retractedMessageUuids` had no carrier.
            //
            // `convert_messages` drops every `System` before the wire
            // (`llm-runtime/src/convert.rs`), so this is transcript + TUI only
            // and never becomes model context.
            let notice_msg = protocol::ConversationMessage::System {
                id: protocol::MessageId::new(),
                content,
                subtype: Some("model_refusal_fallback".to_string()),
                compact_metadata: None,
                refusal_fallback: Some(protocol::RefusalFallbackMetadata {
                    trigger: "refusal".to_string(),
                    direction: "retry".to_string(),
                    // This port's cascade persistently swaps the SESSION model
                    // (see the latch above), which is upstream's `swapSession`
                    // arm — `scope: Mt ? "session" : "local"`.
                    scope: Some("session".to_string()),
                    original_model: e.banner.origin_model.clone(),
                    fallback_model: e.banner.serving_model.clone(),
                    request_id: e.banner.request_id.clone(),
                    api_refusal_category: e.banner.api_refusal_category.clone(),
                    retracted_message_uuids: e.banner.retracted_message_uuids.clone(),
                    refused_user_message_uuid: e.banner.refused_user_message_uuid.clone(),
                }),
            };
            self.session.lock().await.history.push(notice_msg.clone());
            self.persist_message_to_jsonl(&notice_msg).await;
        }
        // Success-path analytics — inline event name (NOT a locked telemetry
        // const), so the 347-entry `ALL_EVENT_NAMES` fixture lock is unperturbed.
        tracing::info!(
            event = "tengu_refusal_fallback_triggered",
            original_model = %original_model,
            fallback_model = %fallback,
            trigger = "refusal",
        );
        true
    }

    /// Git env-block probe + `gitStatus:` attachment, frozen from the first
    /// resolved host-side cwd for the conversation lifetime.
    pub(super) async fn cached_git_status(
        &self,
        cwd: &std::path::Path,
    ) -> (Option<crate::prompt::GitStatus>, Option<String>) {
        {
            let cache = self.prompt_runtime.git_status_snapshot.lock().await;
            if let Some(snap) = cache.as_ref() {
                return (snap.probe.clone(), snap.block.clone());
            }
        }
        let probe = crate::prompt::git_status::probe(cwd);
        let block = crate::prompt::git_status::render_git_status_block(cwd);
        *self.prompt_runtime.git_status_snapshot.lock().await = Some(GitStatusSnapshot {
            probe: probe.clone(),
            block: block.clone(),
        });
        (probe, block)
    }

    /// Schedule a best-effort startup Responses WebSocket prewarm.
    ///
    /// The prewarm uses the current model/profile, assembled system prompt, and
    /// current tool list with an empty conversation history. A later first turn
    /// can then reuse the returned `response.id` when its request is a strict
    /// extension of this prefix. Failures are intentionally ignored.
    pub fn spawn_startup_responses_websocket_prewarm(self: &Arc<Self>) {
        self.abort_startup_responses_websocket_prewarm();
        let orchestrator = Arc::clone(self);
        let handle = tokio::spawn(async move {
            let (model, profile) = {
                let session = orchestrator.session.lock().await;
                (session.model.clone(), session.model_profile.clone())
            };
            let system = orchestrator.build_system_prompt().await;
            let tools = orchestrator.build_wire_tools().await;
            let _ = orchestrator
                .api
                .prewarm_responses_websocket(
                    &model,
                    profile.as_deref(),
                    Some(&system),
                    Vec::new(),
                    tools,
                )
                .await;
        });
        *self
            .lifecycle_runtime
            .startup_responses_websocket_prewarm
            .lock()
            .expect("startup responses websocket prewarm") = Some(handle);
    }

    /// Abort any pending startup Responses WebSocket prewarm task.
    pub fn abort_startup_responses_websocket_prewarm(&self) {
        if let Some(handle) = self
            .lifecycle_runtime
            .startup_responses_websocket_prewarm
            .lock()
            .expect("startup responses websocket prewarm")
            .take()
        {
            handle.abort();
        }
    }

    /// Task 6 (llm-runtime future-work batch 5): re-map a terminal
    /// `RateLimited` error onto the limits-specific copy the API client
    /// composed from the 429's own unified headers (claude-code
    /// `errors.ts:480-524`). Applied by every public turn driver so each
    /// consumer of the error's `Display` (CLI stderr, TUI scrollback,
    /// `client-adapter` `ClientEvent::Error`) sees the
    /// `"You've hit your … limit · resets …"` copy instead of the generic
    /// `"api call failed: rate limited"`. No-op for non-429 errors and when
    /// the 429 carried no unified headers.
    /// Text for a graceful `model_error` surface. Verbatim for parity EXCEPT a
    /// `ModelUnavailable` (a provider 404): enrich it with the model id and a
    /// `/model` hint so the user knows WHICH model the provider rejected and how
    /// to switch — the bare "model unavailable" was opaque, especially with a
    /// stale third-party catalog entry (multi-provider UX).
    pub(crate) async fn model_error_text(&self, err: &LlmError) -> String {
        match err {
            LlmError::ModelUnavailable => {
                let model = self.session.lock().await.model.clone();
                format!(
                    "model unavailable: the provider does not serve '{model}' (HTTP 404). Pick another model with /model."
                )
            }
            // 413 request-too-large (accumulated images/attachments): render the
            // byte-exact `$Vi()` notice instead of the opaque "request too large".
            LlmError::RequestTooLarge => request_too_large_notice(self.prompt_is_interactive()),
            // Billing (`Flp`: `yu({content:LYr,error:"billing_error"})`) and
            // prompt-too-long (`content:Jq`) both render BARE — no `API Error:`
            // prefix. Both used to fall through to `Display`, i.e. the words
            // "quota exceeded" and "context overflow".
            LlmError::QuotaExceeded => crate::api_error_copy::CREDIT_BALANCE_TOO_LOW.to_string(),
            // Dead OAuth session (`e instanceof qQt`): the IdP REJECTED the
            // stored refresh token, so no retry can help and only a fresh
            // sign-in will. Keyed on the variant, mirroring the oracle's
            // instanceof check rather than sniffing message text.
            LlmError::OAuthRefreshDead => {
                crate::api_error_copy::oauth_refresh_dead_text(self.prompt_is_interactive())
                    .to_string()
            }
            // Revoked OAuth token (`Uke`): a 403 whose message names it. Checked
            // BEFORE the x-api-key branch, matching the oracle's order, and
            // split on interactivity because a non-interactive caller cannot
            // run the auth command.
            //
            // The non-interactive half names a product, so it takes the LIVE
            // provider profile: this renderer is shared by every provider, and
            // the oracle's hardcoded "Claude" would be wrong for a session
            // routed elsewhere.
            LlmError::Authentication { .. } | LlmError::PermissionDenied { .. }
                if crate::api_error_copy::is_oauth_revoked(
                    err.http_status(),
                    err.provider_message().unwrap_or_default(),
                ) =>
            {
                let profile = self.session.lock().await.model_profile.clone();
                crate::api_error_copy::oauth_revoked_text(
                    self.prompt_is_interactive(),
                    profile.as_deref(),
                )
            }
            // Org policy turned the SUBSCRIPTION path off (`de_()` → `ce_`, a
            // 401/403 naming it). The oracle checks this BEFORE the API-key
            // disablement below, and the two are mirror images: this one sends
            // the user to an API key, that one sends them to sign-in. Ordering
            // them wrongly would hand a blocked user the remedy their org just
            // disabled.
            LlmError::Authentication { .. } | LlmError::PermissionDenied { .. }
                if crate::api_error_copy::is_oauth_org_not_allowed(
                    err.http_status(),
                    err.provider_message().unwrap_or_default(),
                ) =>
            {
                crate::api_error_copy::OAUTH_ORG_NOT_ALLOWED.to_string()
            }
            // Org policy turned API-key auth off (403 naming it). Checked
            // before the generic credential branch, and names the specific
            // thing THIS user has to unset.
            LlmError::Authentication { .. } | LlmError::PermissionDenied { .. }
                if crate::api_error_copy::is_api_key_auth_disabled(
                    err.http_status(),
                    err.provider_message().unwrap_or_default(),
                ) =>
            {
                let profile = self.session.lock().await.model_profile.clone();
                crate::api_error_copy::api_key_auth_disabled_text(
                    &self.config.credential_origin,
                    self.config.has_oauth_token,
                    profile.as_deref(),
                )
            }
            // Credential rejection. The oracle gates this on the MESSAGE naming
            // `x-api-key`, not on the status, then splits on where the key came
            // from: an env var or `apiKeyHelper` gets "fix that", everything
            // else gets "/login" — because /login cannot fix an external key.
            //
            // A 401/403 that does NOT name the header falls through to the
            // variant's own text, matching the oracle's outer `if`.
            LlmError::Authentication { .. } | LlmError::PermissionDenied { .. }
                if crate::api_error_copy::mentions_api_key_header(
                    err.provider_message().unwrap_or_default(),
                ) =>
            {
                // A cloud-hosted route names ITS credential problem instead —
                // "run gcloud auth ..." is useful advice, "/login" is not. The
                // 401-vs-other split inside is only decidable because the status
                // now survives in the message.
                // `if(qOu()) return UOu` comes FIRST in the oracle: on a
                // remote session the failure is reported as possibly transient
                // before anything looks at the credential's source.
                if crate::api_error_copy::is_remote_session() {
                    return crate::api_error_copy::AUTH_TRANSIENT.to_string();
                }
                // `xn()==="gateway"` comes next in the oracle. It IS reachable:
                // `Mt.gatewayAuth` is bootstrapped from the environment
                // (`CLAUDE_CODE_USE_GATEWAY` + `ANTHROPIC_BASE_URL` +
                // `ANTHROPIC_AUTH_TOKEN`), so the route resolves without any
                // runtime credential object. When a gateway fronts the provider
                // and IT cannot authenticate upstream, no credential the user
                // holds is at fault — say so instead of sending them to /connect.
                let route = self
                    .config
                    .error_route
                    .clone()
                    .unwrap_or_else(crate::api_error_copy::ErrorRouteTag::from_env);
                if matches!(route, crate::api_error_copy::ErrorRouteTag::Gateway) {
                    return crate::api_error_copy::GATEWAY_UPSTREAM_AUTH_FAILED.to_string();
                }
                crate::api_error_copy::cloud_credential_text(&route, err.http_status())
                    .unwrap_or_else(|| {
                        crate::api_error_copy::credential_rejected_text(
                            &self.config.credential_origin,
                        )
                        .to_string()
                    })
            }
            // TERMINAL 401/403 arm (@230607344). Every specific auth branch
            // above declined, so the oracle still renders an `API Error:` line
            // carrying the provider detail rather than falling through to the
            // variant's bare `Display` ("authentication failed" /
            // "permission denied"), which is what this port used to show.
            LlmError::Authentication { .. } | LlmError::PermissionDenied { .. } => {
                crate::api_error_copy::auth_failed_fallback(
                    self.prompt_is_interactive(),
                    // Oracle: `let i = sir(e)` — the SAME normalizer the retry
                    // banner uses, so both surfaces agree.
                    &llm_runtime::error_display_text(err),
                )
            }
            LlmError::ContextOverflow { .. } => crate::api_error_copy::PROMPT_TOO_LONG.to_string(),
            // 429: the oracle renders `API Error: Request rejected (429) · …`,
            // pulling the detail out of the JSON body the decoder stringified
            // into the message. This used to fall through to `Display`, which is
            // the bare words "rate limited".
            //
            // Two first-party variants are NOT selected here and are documented
            // as such in `api_error_copy`: the `Server is temporarily limiting
            // requests` label and the `hpo()` status-page/gateway suffix both
            // need provider-route plumbing this layer does not have. The
            // fallback clause only shows when the body carries no detail, which
            // a real 429 does.
            LlmError::RateLimited { .. } => {
                let raw = err.to_string();
                let source = self.api.last_rate_limit_error_message().unwrap_or(raw);
                if crate::api_error_copy::is_long_context_credit_message(&source) {
                    crate::api_error_copy::usage_credits_required_for_1m_context(false)
                } else {
                    // `let p = i ? le_ : "Request rejected (429)"`.
                    //
                    // INFERRED, not read off `eir`'s body: `i = eir(ii())` gates
                    // three branches here — the 1M-credits clamp, the
                    // overage-disabled-reason lookup, and this label — all of
                    // which are subscription concepts, so it reads as "this is a
                    // claude.ai subscriber". `is_subscriber` is LingXi's
                    // equivalent and is already threaded from the composition
                    // root. The distinction matters: telling a subscriber their
                    // request was "rejected" implies they hit their own limit,
                    // which is exactly what `le_` exists to deny.
                    let label = if self.config.is_subscriber {
                        crate::api_error_copy::SERVER_LIMITING
                    } else {
                        crate::api_error_copy::REQUEST_REJECTED_429
                    };
                    crate::api_error_copy::rate_limited_text(
                        &source,
                        label,
                        &crate::api_error_copy::capacity_fallback(Some(
                            &self
                                .config
                                .error_route
                                .clone()
                                .unwrap_or_else(crate::api_error_copy::ErrorRouteTag::from_env),
                        )),
                    )
                }
            }
            other => other.to_string(),
        }
    }

    pub(super) fn enrich_api_error(&self, err: OrchestratorError) -> OrchestratorError {
        enrich_rate_limited_error(err, self.api.last_rate_limit_error_message())
    }

    /// Advance the session-scoped Ultracode state at the one prompt-ingress
    /// seam shared by batched, streaming, and cancelable drivers.
    pub(super) async fn append_ultracode_attachments(&self, prompt: &str) {
        use tool_api::tool_trait::ToolStaticContext;
        use tool_workflow::{UltracodeConfig, UltracodeGate, UltracodeState};

        let workflows_enabled = self
            .tools
            .available_tools(&ToolStaticContext::default())
            .iter()
            .any(|tool| tool.name() == tool_workflow::TOOL_NAME);
        let effort = self
            .model_runtime
            .current_effort
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let is_meta_turn = prompt.trim_start().starts_with('/');
        let attachments = {
            let mut session = self.session.lock().await;
            let mut state = UltracodeState {
                active: session.ultracode_active,
                non_meta_turns_since_reminder: session.ultracode_non_meta_turns_since_reminder,
            };
            let attachments = state.advance(
                UltracodeGate {
                    model: &session.model,
                    effort: effort.as_deref(),
                    workflows_enabled,
                },
                UltracodeConfig {
                    feature_flag_cadence: self.config.ultracode_feature_flag_cadence,
                    product_default_cadence: self.config.ultracode_product_default_cadence,
                    keyword_trigger_enabled: self.config.workflow_keyword_trigger_enabled,
                },
                prompt,
                is_meta_turn,
            );
            session.ultracode_active = state.active;
            session.ultracode_non_meta_turns_since_reminder = state.non_meta_turns_since_reminder;
            attachments
        };

        for attachment in attachments {
            let message = ConversationMessage::user_meta(
                MessageId::new(),
                format!("<system-reminder>\n{}\n</system-reminder>", attachment.text),
            );
            self.session.lock().await.history.push(message.clone());
            self.persist_message_to_jsonl(&message).await;
        }
    }

    pub(super) async fn sync_goal_checkin_idle_task(&self) {
        let should_run = self.lifecycle_runtime.stop_hook_snapshot.is_some()
            && crate::prompt::goal_checkin::checkin_interval_ms() > 0
            && self.session.lock().await.active_goal.is_some()
            && self
                .lifecycle_runtime
                .goal_checkin
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .deferred_since
                .is_some();

        if should_run {
            if self
                .lifecycle_runtime
                .goal_checkin_idle_running
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return;
            }
            let Some(provider) = self.lifecycle_runtime.stop_hook_snapshot.clone() else {
                return;
            };
            let generation = self
                .lifecycle_runtime
                .goal_checkin_idle_generation
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
                .saturating_add(1);
            self.lifecycle_runtime
                .goal_checkin_idle_running
                .store(true, std::sync::atomic::Ordering::Release);
            let owner = self.lifecycle_runtime.goal_retry_owner.get().cloned();
            let session = Arc::clone(&self.session);
            let turn_gate = Arc::clone(&self.turn_gate);
            let goal_checkin = Arc::clone(&self.lifecycle_runtime.goal_checkin);
            let running = Arc::clone(&self.lifecycle_runtime.goal_checkin_idle_running);
            let generation_counter =
                Arc::clone(&self.lifecycle_runtime.goal_checkin_idle_generation);
            let analytics_bus = self.model_runtime.analytics_bus.clone();
            let handle = tokio::spawn(async move {
                ConversationOrchestrator::run_goal_checkin_idle_loop(
                    provider,
                    owner,
                    session,
                    turn_gate,
                    goal_checkin,
                    running,
                    generation_counter,
                    generation,
                    analytics_bus,
                )
                .await;
            });
            *self
                .lifecycle_runtime
                .goal_checkin_idle_task
                .lock()
                .expect("goal checkin idle task") = Some(handle);
            return;
        }

        self.lifecycle_runtime
            .goal_checkin_idle_running
            .store(false, std::sync::atomic::Ordering::Release);
        self.lifecycle_runtime
            .goal_checkin_idle_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if let Some(handle) = self
            .lifecycle_runtime
            .goal_checkin_idle_task
            .lock()
            .expect("goal checkin idle task")
            .take()
        {
            handle.abort();
        }
    }

    /// Whether the current in-memory session history already contains this raw
    /// top-level JSONL UUID. Structured stdin replay uses this to suppress
    /// duplicate user turns across process restarts / reattach flows.
    pub async fn session_contains_message_uuid(&self, raw_uuid: &str) -> bool {
        let Some(id) = MessageId::parse_prefixed(raw_uuid) else {
            return false;
        };
        let session = self.session.lock().await;
        session.history.iter().any(|message| message.id() == id)
    }

    /// The `bash_output_audience_note` attachment (2.1.238, gate `kpm`
    /// @294267076, emission @294300924), or `None` when the gate says no.
    ///
    /// Unlike the per-turn reminders this one belongs to the message STREAM: it
    /// follows the `tool_result` line for a Bash call whose stdout is longer
    /// than the few lines the user's terminal showed. Gated on the model
    /// capability `bash_output_audience_note` / the
    /// `CLAUDE_CODE_BASH_OUTPUT_AUDIENCE_NOTE` env var, which the port has no
    /// capability table for ⇒ **default OFF**.
    pub(crate) async fn bash_output_audience_note_message(
        &self,
        tool_use_id: &protocol::ToolUseId,
    ) -> Option<ConversationMessage> {
        if !crate::prompt::bash_output_note::is_enabled() {
            return None;
        }
        let tool_name = self.tool_name_for_use_id(tool_use_id).await?;
        let data = self
            .transcript
            .tool_use_results
            .lock()
            .await
            .get(&tool_use_id.to_string())
            .cloned();
        if !crate::prompt::bash_output_note::should_attach(
            &tool_name,
            data.as_ref(),
            self.config.interactive_session,
        ) {
            return None;
        }
        let content = format!(
            "<system-reminder>\n{}\n</system-reminder>",
            crate::prompt::bash_output_note::BASH_OUTPUT_AUDIENCE_NOTE
        );
        Some(ConversationMessage::user_meta(MessageId::new(), content))
    }

    /// Update the thinking policy for the next API request.
    pub fn set_thinking_config(&self, thinking: llm_runtime::model::thinking::ThinkingConfig) {
        self.api.set_thinking_config(thinking);
    }

    pub(crate) fn scope_api_session<'a, F: std::future::Future + 'a>(
        &'a self,
        non_interactive: bool,
        future: F,
    ) -> impl std::future::Future<Output = F::Output> + 'a {
        // Keep the large turn future on the heap before building this context
        // wrapper; otherwise each generic async layer copies its full state.
        let future = Box::pin(future);
        async move {
            let scope = self
                .transcript
                .thinking_recovery
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(writer) = self.transcript.jsonl_writer.clone() {
                let session = self.session.clone();
                let expected_session = session.lock().await.session_id;
                let last_uuid = self.transcript.last_jsonl_uuid.clone();
                let cwd = self.current_cwd();
                let git_branch = self.resolve_git_branch().await;
                scope.set_recorder(Arc::new(move |ranges| {
                    let writer = writer.clone();
                    let session = session.clone();
                    let last_uuid = last_uuid.clone();
                    let cwd = cwd.clone();
                    let git_branch = git_branch.clone();
                    Box::pin(async move {
                        Self::persist_thinking_recovery_snapshot(
                            writer,
                            last_uuid,
                            session,
                            expected_session,
                            cwd,
                            git_branch,
                            ranges,
                        )
                        .await;
                    })
                }));
            }
            llm_runtime::thinking_scope::scope_thinking_recovery(
                scope,
                platform_api::session_flags::scope_non_interactive_session(non_interactive, future),
            )
            .await
        }
    }

    /// Restore rejected historical identities onto both API clients.
    pub(crate) async fn sync_thinking_signature_strip_flag_to_api(&self) {
        let messages = self.session.lock().await.thinking_stripped_messages.clone();
        self.scope_api_session(!self.prompt_is_interactive(), async {
            self.api.set_thinking_stripped_messages(messages.clone());
            self.streaming_api.set_thinking_stripped_messages(messages);
        })
        .await;
    }

    /// Persist a marker for each newly rejected history snapshot, before the
    /// response is appended. Later responses remain outside the marker's scope.
    pub(crate) async fn persist_thinking_signature_strip_latch(&self) {
        let (mut messages, streaming_messages) = self
            .scope_api_session(!self.prompt_is_interactive(), async {
                (
                    self.api.thinking_stripped_messages(),
                    self.streaming_api.thinking_stripped_messages(),
                )
            })
            .await;
        for (id, from) in streaming_messages {
            messages
                .entry(id)
                .and_modify(|current| *current = (*current).min(from))
                .or_insert(from);
        }
        {
            let mut session = self.session.lock().await;
            // Desktop shares one service with workers and side queries. A
            // worker-only rejection must not write a marker on the main chain.
            messages.retain(|id, _| session.history.iter().any(|message| message.id() == *id));
            let changed = messages.iter().any(|(id, from)| {
                session
                    .thinking_stripped_messages
                    .get(id)
                    .is_none_or(|current| from < current)
            });
            if !changed {
                return;
            }
            session.thinking_signature_stripped = true;
            for (id, from) in messages {
                session
                    .thinking_stripped_messages
                    .entry(id)
                    .and_modify(|current| *current = (*current).min(from))
                    .or_insert(from);
            }
        }
        self.persist_hook_attachment_to_jsonl(serde_json::json!({
            "type": "thinking_stripped",
            "scope": "all",
        }))
        .await;
    }

    /// Update how the attached output sink presents subsequent thinking blocks.
    pub fn set_thinking_display(&self, mode: Option<&str>) {
        self.output.set_thinking_display(mode);
    }

    /// Update the effort carried by the next API request and by subsequently
    /// persisted assistant transcript rows.
    pub fn set_effort(&self, effort: Option<String>) {
        self.model_runtime
            .current_effort_explicit
            .store(true, std::sync::atomic::Ordering::Release);
        *self
            .model_runtime
            .current_reasoning_selection
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = effort
            .clone()
            .map(|id| platform_api::ReasoningSelection::Level { id })
            .unwrap_or(platform_api::ReasoningSelection::Automatic);
        self.apply_effort(effort);
    }

    /// Restore transcript effort only when no launch/control override owns the
    /// live value. Unlike [`Self::set_effort`], inheritance deliberately does
    /// not pin the value, so a later resume can adopt or clear it again.
    pub(crate) fn restore_effort_from_resume(
        &self,
        model: &str,
        provider_id: Option<&str>,
        effort: Option<String>,
    ) {
        if self
            .model_runtime
            .current_effort_explicit
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        // Whatever this leaves behind belongs to the transcript just resumed,
        // not to this session — see `current_effort_from_resume`.
        self.model_runtime
            .current_effort_from_resume
            .store(true, std::sync::atomic::Ordering::Release);
        let selection = effort
            .clone()
            .map(|id| platform_api::ReasoningSelection::Level { id })
            .unwrap_or(platform_api::ReasoningSelection::Automatic);
        let (validated, thinking, provider_effort, legacy_effort) =
            self.reasoning_request_state(model, provider_id, &selection);
        *self
            .model_runtime
            .current_reasoning_selection
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = validated;
        *self
            .model_runtime
            .current_effort
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = legacy_effort;
        self.api.set_thinking_config(thinking);
        self.api.set_effort(provider_effort);
    }

    /// Restore a structured transcript selection without claiming it as an
    /// explicit live override. Older transcripts continue through
    /// `restore_effort_from_resume`; newer rows retain toggles and budgets.
    pub(crate) fn restore_reasoning_selection_from_resume(
        &self,
        model: &str,
        provider_id: Option<&str>,
        selection: platform_api::ReasoningSelection,
    ) {
        if self
            .model_runtime
            .current_effort_explicit
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        // Same provenance as the effort restorer above: this value belongs to
        // the resumed transcript, not to this session.
        self.model_runtime
            .current_effort_from_resume
            .store(true, std::sync::atomic::Ordering::Release);
        let (validated, thinking, provider_effort, legacy_effort) =
            self.reasoning_request_state(model, provider_id, &selection);
        *self
            .model_runtime
            .current_reasoning_selection
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = validated;
        *self
            .model_runtime
            .current_effort
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = legacy_effort;
        self.api.set_thinking_config(thinking);
        self.api.set_effort(provider_effort);
    }

    fn apply_effort(&self, effort: Option<String>) {
        *self
            .model_runtime
            .current_effort
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = effort.clone();
        self.api.set_effort(effort.map(serde_json::Value::String));
    }

    #[must_use]
    pub fn current_reasoning_selection(&self) -> platform_api::ReasoningSelection {
        if let Some(settings) = crate::scheduled_turn::current() {
            return settings.reasoning;
        }
        self.model_runtime
            .current_reasoning_selection
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Project the llm-runtime's request-facing capability registry into the
    /// provider-neutral controls DTO. Keeping this conversion at the
    /// orchestrator seam means the same catalog that validates/encodes a
    /// request also drives the mobile UI; unknown/custom profiles remain
    /// Auto-only because they have no verified adapter contract.
    fn reasoning_spec_for_model(
        &self,
        model: &str,
        provider_id: Option<&str>,
    ) -> platform_api::ReasoningControlSpec {
        let mut matches = self
            .api
            .list_model_listings()
            .into_iter()
            .filter(|listing| {
                listing.request_model == model
                    && provider_id.is_none_or(|provider| listing.provider_id == provider)
            });
        if let Some(first) = matches.next() {
            if provider_id.is_some() || matches.next().is_none() {
                return first.reasoning;
            }
        }
        // A missing profile is the legacy/builtin route for known Anthropic
        // models. Infer it only from the shared model-capability registry;
        // arbitrary or custom ids remain Auto-only instead of inheriting
        // Anthropic controls by name.
        let inferred_provider = provider_id.or_else(|| {
            platform_api::model_capabilities::has_capability(
                model,
                platform_api::model_capabilities::ModelCapability::Effort,
            )
            .then_some("builtin")
        });
        let (protocol, base_url) = match inferred_provider.unwrap_or_default() {
            "anthropic" | "builtin" => (
                llm_runtime::ProtocolFamily::AnthropicMessages,
                "https://api.anthropic.com",
            ),
            "openai" => (
                llm_runtime::ProtocolFamily::OpenAiResponses,
                "https://api.openai.com/v1",
            ),
            "openai-chatgpt" => (
                llm_runtime::ProtocolFamily::OpenAiResponses,
                "https://chatgpt.com/backend-api/codex",
            ),
            "gemini" => (
                llm_runtime::ProtocolFamily::GeminiGenerateContent,
                "https://generativelanguage.googleapis.com/v1beta",
            ),
            "deepseek" => (
                llm_runtime::ProtocolFamily::OpenAiChat,
                "https://api.deepseek.com",
            ),
            "kimi" => (
                llm_runtime::ProtocolFamily::OpenAiChat,
                "https://api.moonshot.cn/v1",
            ),
            "kimi-code" => (
                llm_runtime::ProtocolFamily::OpenAiChat,
                "https://api.kimi.com/coding/v1",
            ),
            "openrouter" => (
                llm_runtime::ProtocolFamily::OpenAiChat,
                "https://openrouter.ai/api/v1",
            ),
            _ => return platform_api::reasoning_control_spec_for_model(model, inferred_provider),
        };

        let raw = llm_runtime::reasoning_controls::reasoning_control_spec(
            llm_runtime::reasoning_controls::ReasoningTarget {
                profile_name: inferred_provider,
                protocol: &protocol,
                base_url,
                model,
            },
        );
        let mandatory = raw
            .mandatory_selection
            .as_ref()
            .map(|selection| match selection {
                llm_runtime::reasoning_controls::ReasoningSelection::Automatic => {
                    platform_api::ReasoningSelection::Automatic
                }
                llm_runtime::reasoning_controls::ReasoningSelection::Disabled => {
                    platform_api::ReasoningSelection::Disabled
                }
                llm_runtime::reasoning_controls::ReasoningSelection::Enabled => {
                    platform_api::ReasoningSelection::Enabled
                }
                llm_runtime::reasoning_controls::ReasoningSelection::Level(id) => {
                    platform_api::ReasoningSelection::Level { id: id.clone() }
                }
                llm_runtime::reasoning_controls::ReasoningSelection::TokenBudget(tokens) => {
                    platform_api::ReasoningSelection::TokenBudget {
                        tokens: u64::from(*tokens),
                    }
                }
            });

        let mut available = Vec::new();
        if let Some(mandatory) = &mandatory {
            available.push(mandatory.clone());
        } else {
            available.push(platform_api::ReasoningSelection::Automatic);
            if raw.can_disable {
                available.push(platform_api::ReasoningSelection::Disabled);
            }
            if raw.can_enable {
                available.push(platform_api::ReasoningSelection::Enabled);
            }
            available.extend(
                raw.levels
                    .iter()
                    .cloned()
                    .map(|id| platform_api::ReasoningSelection::Level { id }),
            );
        }

        let auto_only = available.len() == 1
            && matches!(
                available.first(),
                Some(platform_api::ReasoningSelection::Automatic)
            )
            && raw.token_budget.is_none();
        platform_api::ReasoningControlSpec {
            available,
            selections_persistable: mandatory.is_none(),
            budget_range: raw
                .token_budget
                .map(|range| platform_api::ReasoningBudgetRange {
                    min_tokens: range.min,
                    max_tokens: range.max,
                    supports_dynamic: false,
                    supports_disabled: raw.can_disable,
                }),
            provider_default: mandatory.unwrap_or(platform_api::ReasoningSelection::Automatic),
            forced: raw.mandatory_selection.is_some(),
            modifiable: raw.mandatory_selection.is_none() && !auto_only,
            disabled_reason: if raw.mandatory_selection.is_some() {
                Some("reasoning_required".to_string())
            } else if auto_only {
                Some("reasoning_unavailable".to_string())
            } else {
                None
            },
        }
    }

    fn validate_reasoning_selection(
        &self,
        selection: &platform_api::ReasoningSelection,
        model: &str,
        provider_id: Option<&str>,
    ) -> platform_api::ReasoningSelection {
        let spec = self.reasoning_spec_for_model(model, provider_id);
        let supported = match selection {
            platform_api::ReasoningSelection::Automatic => true,
            platform_api::ReasoningSelection::TokenBudget { tokens } => {
                spec.budget_range.as_ref().is_some_and(|range| {
                    (*tokens >= u64::from(range.min_tokens)
                        && *tokens <= u64::from(range.max_tokens))
                        || (range.supports_disabled && *tokens == 0)
                })
            }
            other => spec.available.iter().any(|candidate| candidate == other),
        };
        if spec.forced && !spec.modifiable {
            spec.provider_default
        } else if supported {
            selection.clone()
        } else {
            platform_api::ReasoningSelection::Automatic
        }
    }

    fn reasoning_request_state(
        &self,
        model: &str,
        provider_id: Option<&str>,
        selection: &platform_api::ReasoningSelection,
    ) -> (
        platform_api::ReasoningSelection,
        llm_runtime::model::thinking::ThinkingConfig,
        Option<serde_json::Value>,
        Option<String>,
    ) {
        use llm_runtime::model::thinking::ThinkingConfig;
        let validated = self.validate_reasoning_selection(selection, model, provider_id);
        let effort_level = |id: &str| Some(serde_json::Value::String(id.to_string()));
        let legacy = |id: &str| Some(id.to_string());
        let provider_id = provider_id.or_else(|| {
            platform_api::model_capabilities::has_capability(
                model,
                platform_api::model_capabilities::ModelCapability::Effort,
            )
            .then_some("builtin")
        });
        let provider_id = provider_id.unwrap_or_default();
        let model_lc = model.to_ascii_lowercase();

        match provider_id {
            "anthropic" | "builtin" => match &validated {
                platform_api::ReasoningSelection::Automatic => {
                    (validated, ThinkingConfig::Automatic, None, None)
                }
                platform_api::ReasoningSelection::Disabled => {
                    (validated, ThinkingConfig::Disabled, None, None)
                }
                platform_api::ReasoningSelection::TokenBudget { tokens } => (
                    validated.clone(),
                    ThinkingConfig::Enabled {
                        budget_tokens: (*tokens).try_into().unwrap_or(u32::MAX),
                    },
                    None,
                    None,
                ),
                platform_api::ReasoningSelection::Level { id } => (
                    validated.clone(),
                    ThinkingConfig::Adaptive,
                    effort_level(id.as_str()),
                    legacy(id.as_str()),
                ),
                platform_api::ReasoningSelection::Enabled => {
                    (validated, ThinkingConfig::Adaptive, None, None)
                }
            },
            "openai" | "openai-chatgpt" => match &validated {
                platform_api::ReasoningSelection::Automatic => {
                    (validated, ThinkingConfig::Automatic, None, None)
                }
                platform_api::ReasoningSelection::Level { id } => (
                    validated.clone(),
                    ThinkingConfig::Adaptive,
                    effort_level(id.as_str()),
                    legacy(id.as_str()),
                ),
                platform_api::ReasoningSelection::Disabled => {
                    // Responses API uses the explicit `none` effort value to
                    // distinguish a user-off override from provider Auto.
                    (
                        validated,
                        ThinkingConfig::Disabled,
                        effort_level("none"),
                        None,
                    )
                }
                platform_api::ReasoningSelection::Enabled => {
                    (validated, ThinkingConfig::Adaptive, None, None)
                }
                platform_api::ReasoningSelection::TokenBudget { .. } => {
                    (validated, ThinkingConfig::Adaptive, None, None)
                }
            },
            "gemini" => {
                if model_lc.starts_with("gemini-3.") {
                    match &validated {
                        platform_api::ReasoningSelection::Automatic => {
                            (validated, ThinkingConfig::Automatic, None, None)
                        }
                        platform_api::ReasoningSelection::Level { id } => (
                            validated.clone(),
                            ThinkingConfig::Adaptive,
                            effort_level(id.as_str()),
                            legacy(id.as_str()),
                        ),
                        platform_api::ReasoningSelection::Disabled => {
                            (validated, ThinkingConfig::Disabled, None, None)
                        }
                        platform_api::ReasoningSelection::Enabled => {
                            (validated, ThinkingConfig::Adaptive, None, None)
                        }
                        platform_api::ReasoningSelection::TokenBudget { .. } => {
                            (validated, ThinkingConfig::Adaptive, None, None)
                        }
                    }
                } else {
                    match &validated {
                        platform_api::ReasoningSelection::Automatic => {
                            (validated, ThinkingConfig::Automatic, None, None)
                        }
                        platform_api::ReasoningSelection::TokenBudget { tokens } => (
                            validated.clone(),
                            ThinkingConfig::Enabled {
                                budget_tokens: (*tokens).try_into().unwrap_or(u32::MAX),
                            },
                            None,
                            None,
                        ),
                        platform_api::ReasoningSelection::Disabled => {
                            (validated, ThinkingConfig::Disabled, None, None)
                        }
                        platform_api::ReasoningSelection::Enabled => {
                            (validated, ThinkingConfig::Adaptive, None, None)
                        }
                        platform_api::ReasoningSelection::Level { .. } => {
                            (validated, ThinkingConfig::Adaptive, None, None)
                        }
                    }
                }
            }
            "deepseek" => match &validated {
                platform_api::ReasoningSelection::Automatic => {
                    (validated, ThinkingConfig::Automatic, None, None)
                }
                platform_api::ReasoningSelection::Disabled => (
                    validated,
                    ThinkingConfig::Disabled,
                    effort_level("off"),
                    None,
                ),
                platform_api::ReasoningSelection::Level { id } => (
                    validated.clone(),
                    ThinkingConfig::Adaptive,
                    effort_level(id.as_str()),
                    legacy(id.as_str()),
                ),
                platform_api::ReasoningSelection::Enabled => {
                    (validated, ThinkingConfig::Adaptive, None, None)
                }
                platform_api::ReasoningSelection::TokenBudget { .. } => {
                    (validated, ThinkingConfig::Adaptive, None, None)
                }
            },
            "kimi" | "kimi-code" => {
                if matches!(model_lc.as_str(), "kimi-k3" | "k3" | "k3-256k") {
                    match &validated {
                        platform_api::ReasoningSelection::Automatic => {
                            (validated, ThinkingConfig::Automatic, None, None)
                        }
                        platform_api::ReasoningSelection::Level { id } => (
                            validated.clone(),
                            ThinkingConfig::Adaptive,
                            effort_level(id.as_str()),
                            legacy(id.as_str()),
                        ),
                        platform_api::ReasoningSelection::Disabled => {
                            (validated, ThinkingConfig::Disabled, None, None)
                        }
                        platform_api::ReasoningSelection::Enabled => {
                            (validated, ThinkingConfig::Adaptive, None, None)
                        }
                        platform_api::ReasoningSelection::TokenBudget { .. } => {
                            (validated, ThinkingConfig::Adaptive, None, None)
                        }
                    }
                } else {
                    match &validated {
                        platform_api::ReasoningSelection::Automatic => {
                            (validated, ThinkingConfig::Automatic, None, None)
                        }
                        platform_api::ReasoningSelection::Disabled => (
                            validated,
                            ThinkingConfig::Disabled,
                            effort_level("off"),
                            None,
                        ),
                        platform_api::ReasoningSelection::Enabled => (
                            validated,
                            ThinkingConfig::Adaptive,
                            effort_level("on"),
                            None,
                        ),
                        platform_api::ReasoningSelection::Level { id } => (
                            validated.clone(),
                            ThinkingConfig::Adaptive,
                            effort_level(id.as_str()),
                            legacy(id.as_str()),
                        ),
                        platform_api::ReasoningSelection::TokenBudget { tokens } => (
                            validated.clone(),
                            ThinkingConfig::Enabled {
                                budget_tokens: (*tokens).try_into().unwrap_or(u32::MAX),
                            },
                            None,
                            None,
                        ),
                    }
                }
            }
            _ => match &validated {
                platform_api::ReasoningSelection::Automatic => {
                    (validated, ThinkingConfig::Automatic, None, None)
                }
                _ => (validated, ThinkingConfig::Adaptive, None, None),
            },
        }
    }

    pub fn set_reasoning_selection_for_model(
        &self,
        model: &str,
        provider_id: Option<&str>,
        selection: platform_api::ReasoningSelection,
    ) -> platform_api::ReasoningSelection {
        self.model_runtime
            .current_effort_explicit
            .store(true, std::sync::atomic::Ordering::Release);
        // An explicit live choice is this session's own, so a later resume
        // with no metadata must leave it alone.
        self.model_runtime
            .current_effort_from_resume
            .store(false, std::sync::atomic::Ordering::Release);
        let (validated, thinking, effort, legacy_effort) =
            self.reasoning_request_state(model, provider_id, &selection);
        *self
            .model_runtime
            .current_reasoning_selection
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = validated.clone();
        *self
            .model_runtime
            .current_effort
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = legacy_effort.clone();
        self.api.set_thinking_config(thinking);
        self.api.set_effort(effort);
        validated
    }

    /// Seed a persisted new-session default without marking it as an explicit
    /// session override.  This lets resume restore a transcript's selection
    /// while still applying the user's default to a genuinely new session.
    pub fn initialize_reasoning_selection_for_model(
        &self,
        model: &str,
        provider_id: Option<&str>,
        selection: platform_api::ReasoningSelection,
    ) -> platform_api::ReasoningSelection {
        let (validated, thinking, effort, legacy_effort) =
            self.reasoning_request_state(model, provider_id, &selection);
        self.model_runtime
            .current_effort_explicit
            .store(false, std::sync::atomic::Ordering::Release);
        // A persisted application default is this session's own too —
        // `current_effort_explicit` cannot tell these two apart.
        self.model_runtime
            .current_effort_from_resume
            .store(false, std::sync::atomic::Ordering::Release);

        *self
            .model_runtime
            .current_reasoning_selection
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = validated.clone();
        *self
            .model_runtime
            .current_effort
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = legacy_effort;
        self.api.set_thinking_config(thinking);
        self.api.set_effort(effort);
        validated
    }

    #[must_use]
    pub fn conversation_controls_for_model(
        &self,
        model: &str,
        provider_id: Option<&str>,
    ) -> platform_api::ConversationControls {
        let reasoning_spec = self.reasoning_spec_for_model(model, provider_id);
        let requested_reasoning = self.current_reasoning_selection();
        let effective_reasoning =
            self.validate_reasoning_selection(&requested_reasoning, model, provider_id);
        let requested_permission = self
            .permission_mode()
            .unwrap_or_else(|| "default".to_string());
        let effective_permission = requested_permission.clone();
        let permission_modes = [
            "default",
            "acceptEdits",
            "plan",
            "auto",
            "dontAsk",
            "bypassPermissions",
        ]
        .into_iter()
        .map(|mode| {
            let unavailable =
                mode == "bypassPermissions" && !self.perms.can_request_bypass_permissions();
            platform_api::PermissionModeAvailability {
                mode: mode.to_string(),
                available: !unavailable,
                disabled_reason: unavailable.then(|| "not_yet_available".to_string()),
            }
        })
        .collect();
        platform_api::ConversationControls {
            model_reference: platform_api::qualified_model_ref(model, provider_id),
            permission: platform_api::PermissionControlState {
                requested: requested_permission,
                effective: effective_permission,
                modes: permission_modes,
            },
            requested_reasoning_selection: requested_reasoning,
            effective_reasoning_selection: effective_reasoning,
            reasoning_spec,
        }
    }

    /// Execute a saved automation using request-local settings and the existing
    /// turn gate. No session model/reasoning default is changed or persisted.
    pub async fn run_scheduled_turn(
        &self,
        prompt: &str,
        model: &str,
        reasoning: platform_api::ReasoningSelection,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, String> {
        let turn_guard = self
            .turn_gate
            .try_lock()
            .map_err(|_| "busy:Target session is running".to_string())?;
        let settings = self.scheduled_turn_settings(model, reasoning)?;
        self.run_scheduled_turn_locked(&turn_guard, prompt, settings, cancel)
            .await
    }

    /// Pin a scheduled turn to an exact live session. The same gate protects
    /// target validation, the host's durable binding, execution and result capture.
    pub async fn run_scheduled_turn_in_session<F>(
        &self,
        expected_session: protocol::SessionId,
        prompt: &str,
        model: &str,
        reasoning: platform_api::ReasoningSelection,
        cancel: CancellationToken,
        before_start: F,
    ) -> Result<(TurnOutcome, protocol::SessionId, String), String>
    where
        F: std::future::Future<Output = Result<(), String>> + Send,
    {
        let turn_guard = self
            .turn_gate
            .try_lock()
            .map_err(|_| "busy:Target session is running".to_string())?;
        if self.session.lock().await.session_id != expected_session {
            return Err("busy:Target session changed before execution".into());
        }
        let settings = self.scheduled_turn_settings(model, reasoning)?;
        before_start.await?;
        let outcome = self
            .run_scheduled_turn_locked(&turn_guard, prompt, settings, cancel)
            .await?;
        let session = self.session.lock().await;
        let summary = session
            .history
            .iter()
            .rev()
            .find(|message| matches!(message, protocol::ConversationMessage::Assistant { .. }))
            .map(protocol::ConversationMessage::text_content)
            .unwrap_or_default();
        Ok((outcome, session.session_id, summary))
    }

    fn scheduled_turn_settings(
        &self,
        model: &str,
        reasoning: platform_api::ReasoningSelection,
    ) -> Result<crate::scheduled_turn::ScheduledSettings, String> {
        let listing = self
            .api
            .list_model_listings()
            .into_iter()
            .find(|row| {
                platform_api::qualified_model_ref(&row.request_model, Some(&row.provider_id))
                    == model
            })
            .ok_or_else(|| "paused:Scheduled model is unavailable; choose a model".to_string())?;
        let (validated, thinking, effort, _) = self.reasoning_request_state(
            &listing.request_model,
            Some(&listing.provider_id),
            &reasoning,
        );
        if validated != reasoning {
            return Err("paused:Scheduled reasoning setting is no longer supported".into());
        }
        Ok(crate::scheduled_turn::ScheduledSettings {
            model: listing.request_model,
            provider: listing.provider_id,
            reasoning,
            thinking,
            effort,
        })
    }

    async fn run_scheduled_turn_locked(
        &self,
        turn_guard: &tokio::sync::MutexGuard<'_, ()>,
        prompt: &str,
        settings: crate::scheduled_turn::ScheduledSettings,
        cancel: CancellationToken,
    ) -> Result<TurnOutcome, String> {
        // A scheduled run drives the LIVE session's output stream, so the client
        // has to learn a turn started — otherwise the composer stays unlocked,
        // no Stop button appears, and any permission prompt this turn raises
        // belongs to a turn the client was never told about.
        self.output.emit_turn_started().await;
        crate::scheduled_turn::SETTINGS
            .scope(
                settings,
                self.run_turn_streaming_with_origin_locked(
                    turn_guard,
                    prompt,
                    Vec::new(),
                    cancel,
                    None,
                    false,
                ),
            )
            .await
            .map_err(|error| error.to_string())
    }

    /// Apply a LIVE session permission-mode change (stream-json
    /// `set_permission_mode` control_request). Delegates to the gate's
    /// [`platform_api::PermissionGate::set_permission_mode`]; only the enforcing
    /// `PolicyPermissionGate` actually mutates (other gates no-op). Returns the
    /// gate's validation error string on an invalid / disallowed mode.
    pub async fn set_permission_mode(&self, mode: &str) -> Result<(), String> {
        self.perms.set_permission_mode(mode).await?;
        // Tool dispatch and plan reminders also consult session.plan_mode.
        // Synchronize it with the accepted live gate mode so a user can leave
        // a plan entered by EnterPlanMode without retaining its write block.
        let mut session = self.session.lock().await;
        if let Some(live_mode) = self.perms.permission_mode() {
            let plan_mode = live_mode == "plan";
            if session.plan_mode != plan_mode {
                session.plan_mode = plan_mode;
                if plan_mode {
                    session.plan_reminder_shown = false;
                    session.plan_mode_exit_pending = false;
                } else {
                    session.plan_mode_exited = true;
                    session.plan_mode_exit_pending = true;
                }
            }
        }
        Ok(())
    }

    /// Apply or clear the LIVE per-MCP-server permission-mode override
    /// (stream-json `set_mcp_permission_mode_override` control_request).
    pub async fn set_mcp_permission_mode_override(
        &self,
        server_name: &str,
        mode: Option<&str>,
    ) -> Result<(), String> {
        self.perms
            .set_mcp_permission_mode_override(server_name, mode)
            .await
    }

    /// Return the enforcing gate's live permission-mode wire id.
    #[must_use]
    pub fn permission_mode(&self) -> Option<String> {
        self.perms.permission_mode()
    }
}

#[cfg(test)]
#[path = "tests/live_permission_mode_tests.rs"]
mod live_permission_mode_tests;

#[cfg(test)]
mod session_sidecar_tests {
    use super::{
        copy_then_cleanup_with, last_response_is_api_error, move_session_sidecar_best_effort,
        parse_prompt_suggestion_response, SidecarCopyError,
    };
    use protocol::{ContentBlock, ConversationMessage, MessageId};

    #[test]
    fn occupied_sidecar_destination_is_quarantined_while_source_moves_as_a_unit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let old_project = temp.path().join("old-project");
        let new_project = temp.path().join("new-project");
        let session_id = "11111111-2222-4333-8444-555555555555";
        let source = old_project.join(session_id);
        let target = new_project.join(session_id);
        std::fs::create_dir_all(source.join("tool-results")).expect("source sidecar");
        std::fs::create_dir_all(&target).expect("occupied target sidecar");
        std::fs::write(source.join("session-state.json"), "current").expect("source state");
        std::fs::write(source.join("tool-results").join("result.txt"), "result")
            .expect("source result");
        std::fs::write(target.join("stale.json"), "stale").expect("stale state");

        move_session_sidecar_best_effort(&source, &target);

        assert!(!source.exists(), "the complete source sidecar is rehomed");
        assert_eq!(
            std::fs::read_to_string(target.join("session-state.json")).expect("moved state"),
            "current"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("tool-results").join("result.txt"))
                .expect("moved result"),
            "result"
        );
        assert!(
            !target.join("stale.json").exists(),
            "an occupied destination must not be merged over the moved sidecar"
        );
        let quarantined = std::fs::read_dir(&new_project)
            .expect("new project")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .find(|name| name.starts_with(&format!("{session_id}.superseded-")));
        assert!(
            quarantined.is_some(),
            "stale destination is retained for recovery"
        );
        let stale_path = new_project.join(quarantined.expect("quarantined path"));
        assert_eq!(
            std::fs::read_to_string(stale_path.join("stale.json")).expect("quarantined state"),
            "stale"
        );
    }

    #[test]
    fn copied_sidecar_survives_injected_source_cleanup_failure() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        std::fs::create_dir_all(&source).expect("source");

        let result = copy_then_cleanup_with(
            &source,
            &target,
            |_source, target| {
                std::fs::create_dir_all(target)?;
                std::fs::write(target.join("complete.txt"), "authoritative")
            },
            |_source| Err(std::io::Error::other("injected cleanup failure")),
        );

        assert!(matches!(result, Err(SidecarCopyError::Cleanup(_))));
        assert_eq!(
            std::fs::read_to_string(target.join("complete.txt")).expect("keep copied target"),
            "authoritative",
            "cleanup failure must not delete a complete destination copy"
        );
        assert!(source.exists(), "source debris remains recoverable");
    }

    #[cfg(unix)]
    #[test]
    fn source_symlink_is_rejected_before_target_quarantine() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let projects = temp.path().join("projects");
        let old_project = projects.join("old-project");
        let new_project = projects.join("new-project");
        let session_id = "22222222-3333-4444-8555-666666666666";
        let source = old_project.join(session_id);
        let real_source = temp.path().join("outside-source");
        let target = new_project.join(session_id);
        std::fs::create_dir_all(&real_source).expect("real source");
        std::fs::write(real_source.join("current.txt"), "current").expect("source data");
        std::fs::create_dir_all(&target).expect("occupied target");
        std::fs::write(target.join("stale.txt"), "stale").expect("target data");
        std::fs::create_dir_all(&old_project).expect("old project");
        symlink(&real_source, &source).expect("source symlink");

        move_session_sidecar_best_effort(&source, &target);

        assert!(
            std::fs::symlink_metadata(&source)
                .expect("source entry")
                .file_type()
                .is_symlink(),
            "the source symlink must not be followed or moved"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("stale.txt")).expect("target remains"),
            "stale"
        );
        assert!(
            !new_project
                .read_dir()
                .expect("new project")
                .filter_map(Result::ok)
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{session_id}.superseded-"))),
            "an unsafe source must not quarantine the target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_sidecar_parent_is_rejected_before_target_quarantine() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let projects = temp.path().join("projects");
        let old_project = projects.join("old-project");
        let new_project = projects.join("new-project");
        let outside = temp.path().join("outside");
        let session_id = "33333333-4444-4555-8666-777777777777";
        let source = old_project.join(session_id);
        let target = new_project.join(session_id);
        std::fs::create_dir_all(outside.join(session_id)).expect("outside source");
        std::fs::write(outside.join(session_id).join("current.txt"), "current")
            .expect("source data");
        std::fs::create_dir_all(&projects).expect("projects");
        std::fs::create_dir_all(&target).expect("occupied target");
        std::fs::write(target.join("stale.txt"), "stale").expect("target data");
        symlink(&outside, &old_project).expect("old project symlink");

        move_session_sidecar_best_effort(&source, &target);

        assert_eq!(
            std::fs::read_to_string(target.join("stale.txt")).expect("target remains"),
            "stale"
        );
        assert!(
            !new_project
                .read_dir()
                .expect("new project")
                .filter_map(Result::ok)
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{session_id}.superseded-"))),
            "a symlinked parent must not quarantine the target"
        );
    }

    #[test]
    fn prompt_suggestion_parser_unwraps_xml_and_labels() {
        assert_eq!(
            parse_prompt_suggestion_response(
                "<suggestion>Suggested prompt: How should I verify this change?</suggestion>"
            )
            .as_deref(),
            Some("How should I verify this change?")
        );
    }

    #[test]
    fn prompt_suggestion_parser_drops_silence_sentinels() {
        for raw in [
            "nothing to suggest",
            "no suggestion",
            "(silence)",
            "[silence]",
            "<suggestion>stay silent</suggestion>",
        ] {
            assert_eq!(parse_prompt_suggestion_response(raw), None, "{raw}");
        }
    }

    #[test]
    fn prompt_suggestion_parser_rejects_oracle_suppression_shapes() {
        for raw in [
            "User: Can you add a regression test?",
            "(run the tests)",
            "thanks, looks good",
            "Let me run the tests",
            "one",
            "This is one sentence. Another starts here",
            "run\nall tests",
        ] {
            assert_eq!(parse_prompt_suggestion_response(raw), None, "{raw}");
        }
        assert_eq!(
            parse_prompt_suggestion_response("yes").as_deref(),
            Some("yes")
        );
        assert_eq!(
            parse_prompt_suggestion_response("/commit").as_deref(),
            Some("/commit")
        );
        assert_eq!(
            parse_prompt_suggestion_response("用户: 继续测试").as_deref(),
            Some("用户: 继续测试"),
            "the oracle's JavaScript \\w speaker prefix is ASCII-only"
        );
        assert_eq!(
            parse_prompt_suggestion_response("继续").as_deref(),
            Some("继续"),
            "a two-Han token is kept even though whitespace word-count is 1"
        );
        assert_eq!(
            parse_prompt_suggestion_response("テスト").as_deref(),
            Some("テスト"),
            "katakana is a phonetic script and counts toward the CJK keep"
        );
        assert_eq!(
            parse_prompt_suggestion_response("한글").as_deref(),
            Some("한글")
        );
        assert_eq!(
            parse_prompt_suggestion_response("好"),
            None,
            "a single Han letter is still too few"
        );
    }

    #[test]
    fn prompt_suggestion_gate_recognizes_synthetic_api_error_assistants() {
        let assistant = |reason: &str| ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: "response".to_string(),
            }],
            stop_reason: Some(reason.to_string()),
        };
        assert!(last_response_is_api_error(&[assistant("model_error")]));
        assert!(last_response_is_api_error(&[assistant("refusal")]));
        assert!(!last_response_is_api_error(&[assistant("end_turn")]));
    }
}

#[cfg(test)]
mod side_question_reminder_tests {
    use crate::ConversationOrchestrator;

    /// Keep the reminder byte-identical to the checked-in Claude Code source.
    #[test]
    fn the_reminder_matches_claude_code_source() {
        let r = ConversationOrchestrator::SIDE_QUESTION_SYSTEM_REMINDER;
        assert!(!r.contains("- Do NOT write tool calls or tool output as text"));
    }

    #[test]
    fn the_reminder_keeps_the_source_constraint_order() {
        let r = ConversationOrchestrator::SIDE_QUESTION_SYSTEM_REMINDER;
        let no_tools = r
            .find("- You have NO tools available")
            .expect("no-tools bullet");
        let one_off = r
            .find("- This is a one-off response")
            .expect("one-off bullet");
        assert!(
            no_tools < one_off,
            "source order must be: no-tools, one-off"
        );
    }
}
