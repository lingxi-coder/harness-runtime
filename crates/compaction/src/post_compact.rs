//! Builds the post-compact message set: the boundary summary plus restored
//! file/skill attachments. Plans 09 (skills) and 10 (session) inject real data;
//! M1.7 ships the shape only.
//!
//! Also hosts [`run_post_compact_cleanup`] — the cache/state reset run after
//! every compaction (TS `src/services/compact/postCompactCleanup.ts`).

use crate::microcompact::reset_microcompact_state;
use crate::thresholds::{
    POST_COMPACT_MAX_FILES_TO_RESTORE, POST_COMPACT_MAX_TOKENS_PER_FILE,
    POST_COMPACT_MAX_TOKENS_PER_SKILL, POST_COMPACT_SKILLS_TOKEN_BUDGET, POST_COMPACT_TOKEN_BUDGET,
};
use crate::warning_state::clear_compact_warning_suppression;
use protocol::{ContentBlock, ConversationMessage};

/// Marker appended to skill content truncated to fit the per-skill budget.
///
/// 1:1 with claude-code v2.1.183 `Ael` (`bin/claude.exe` offset 203004352),
/// the suffix `o3p` appends when a skill's content exceeds
/// [`POST_COMPACT_MAX_TOKENS_PER_SKILL`].
pub const SKILL_TRUNCATION_MARKER: &str =
    "\n\n[... skill content truncated for compaction; use Read on the skill path if you need the full text]";

/// Cheap token estimate: `round(len / 4)`.
///
/// 1:1 with claude-code's `$f(e, t=4) = Math.round(e.length / 4)`
/// (`bin/claude.exe` offset 197027314) — the estimator the post-compact
/// restoration budgets against (`$f(Le(l))` / `$f(o.content)`). This rounds
/// (`(len + 2) / 4`), like
/// [`crate::grouping::estimate_tokens_for_range`], matching the binary's
/// `Math.round` for the per-file / per-skill / running-budget comparisons.
#[must_use]
pub fn estimate_content_tokens(content: &str) -> u64 {
    let utf16_len = utf16_code_units(content);
    // Math.round(len/4) = floor((len + 2) / 4) for the half-up rounding the
    // binary uses (`len.length` is always a non-negative integer).
    utf16_len.saturating_add(2) / 4
}

#[must_use]
fn utf16_code_units(content: &str) -> u64 {
    content.chars().map(utf16_scalar_units).sum()
}

const fn utf16_scalar_units(ch: char) -> u64 {
    if ch.len_utf16() == 1 {
        1
    } else {
        2
    }
}

#[must_use]
fn utf16_scalar_prefix_boundary(content: &str, max_units: u64) -> usize {
    let mut used = 0u64;
    let mut boundary = 0usize;
    for (idx, ch) in content.char_indices() {
        let char_units = utf16_scalar_units(ch);
        if used.saturating_add(char_units) > max_units {
            break;
        }
        used = used.saturating_add(char_units);
        boundary = idx + ch.len_utf8();
    }
    boundary
}

#[must_use]
fn utf16_units_vec(content: &str) -> Vec<u16> {
    content.encode_utf16().collect()
}

#[must_use]
fn estimated_tokens_for_exact_utf16(display_text: &str, exact_utf16: Option<&[u16]>) -> u64 {
    exact_utf16.map_or_else(
        || estimate_content_tokens(display_text),
        |units| {
            u64::try_from(units.len())
                .unwrap_or(u64::MAX)
                .saturating_add(2)
                / 4
        },
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TruncatedText {
    display_text: String,
    exact_utf16: Option<Vec<u16>>,
}

/// Truncate `content` to fit within `max_tokens`, appending
/// [`SKILL_TRUNCATION_MARKER`] when it overflows.
///
/// 1:1 with `o3p(e, t)` (`bin/claude.exe` offset 203003699):
/// ```text
/// function o3p(e,t){ if($f(e)<=t) return e; let n=t*4-Ael.length; return e.slice(0,n)+Ael }
/// ```
/// When `$f(content) <= max_tokens` the content is returned verbatim; otherwise
/// it is sliced to `max_tokens*4 − marker.length` UTF-16 code units and the
/// marker appended. The returned [`TruncatedText`] always carries a valid UTF-8
/// `display_text`; when the exact JS `slice(0, keep)` result would end on the
/// first half of a surrogate pair, `display_text` snaps down to the previous
/// scalar value and `exact_utf16` preserves the true provider-visible UTF-16
/// wire image.
#[must_use]
fn truncate_content_with_marker_exact(
    content: &str,
    max_tokens: u64,
    marker: &str,
) -> TruncatedText {
    if estimate_content_tokens(content) <= max_tokens {
        return TruncatedText {
            display_text: content.to_string(),
            exact_utf16: None,
        };
    }
    let marker_len = utf16_code_units(marker);
    let keep = (max_tokens.saturating_mul(4)).saturating_sub(marker_len);
    let boundary = utf16_scalar_prefix_boundary(content, keep);
    let mut display_text = content[..boundary].to_string();
    display_text.push_str(marker);

    let mut exact_utf16 = None;
    let mut exact_units = utf16_units_vec(content);
    if let (Ok(total), Ok(keep)) = (
        usize::try_from(utf16_code_units(content)),
        usize::try_from(keep),
    ) {
        if keep <= total {
            exact_units.truncate(keep);
            exact_units.extend(marker.encode_utf16());
            if exact_units != display_text.encode_utf16().collect::<Vec<_>>() {
                exact_utf16 = Some(exact_units);
            }
        }
    }

    TruncatedText {
        display_text,
        exact_utf16,
    }
}

/// Truncate skill content to fit within `max_tokens`, appending
/// [`SKILL_TRUNCATION_MARKER`] when it overflows.
#[must_use]
pub fn truncate_skill_content(content: &str, max_tokens: u64) -> String {
    truncate_content_with_marker_exact(content, max_tokens, SKILL_TRUNCATION_MARKER).display_text
}

#[must_use]
fn truncate_skill_content_exact(content: &str, max_tokens: u64) -> TruncatedText {
    truncate_content_with_marker_exact(content, max_tokens, SKILL_TRUNCATION_MARKER)
}

/// A file eligible for post-compact restoration: the path, the content the
/// model last saw (or a fresh re-read supplied by the caller), and the
/// floor-truncated mtime used to sort most-recent-first.
///
/// Mirrors the `{filename, content, timestamp}` rows `Pqn` builds from
/// `Object.entries(readFileState)` (`bin/claude.exe` offset 203001477).
#[derive(Debug, Clone)]
pub struct FileRestoreCandidate {
    /// Absolute file path.
    pub path: std::path::PathBuf,
    /// File content (the snapshot the model saw, or a caller re-read).
    pub content: String,
    /// Floor-truncated mtime in ms; restoration sorts descending on this.
    pub timestamp_ms: i64,
}

/// A skill eligible for post-compact restoration.
///
/// Mirrors the `[key, {skillName, skillPath, content, invokedAt}]` entries `rRg`
/// iterates from the invoked-skill registry (`bin/claude.exe` v2.1.207 `rRg`).
/// The `key` is the registry key (`"{agentId}:{name}"`) so `rRg`'s `n_n`
/// write-backs can target the row this candidate came from.
#[derive(Debug, Clone)]
pub struct SkillRestoreCandidate {
    /// The registry key (`"{agentId}:{name}"`) this candidate came from — the
    /// target of the `n_n` write-back when the content is truncated or the
    /// budget overflows.
    pub key: String,
    /// Skill name.
    pub name: String,
    /// Skill source path.
    pub path: std::path::PathBuf,
    /// Full skill content (truncated per [`POST_COMPACT_MAX_TOKENS_PER_SKILL`]).
    pub content: String,
    /// Exact JS UTF-16 wire image for `content`, when it differs from the
    /// display-safe UTF-8 string.
    pub content_exact_utf16: Option<Vec<u16>>,
    /// When the skill was last invoked; restoration sorts descending on this.
    pub invoked_at_ms: i64,
}

/// An already-attached item consulted by `LQn` when deduping a skill's content
/// against the context already present at the compact boundary.
///
/// 1:1 with the two branches of `LQn(e,t)` (`bin/claude.exe` v2.1.207): an
/// `invoked_skills` attachment already carrying a skill's content
/// ([`Self::Attachment`]), or a plain message body whose `DYi` text extraction
/// equals the content ([`Self::Body`]).
#[derive(Debug, Clone)]
pub enum AttachedSkillContent {
    /// Content that already rode in a prior `invoked_skills` attachment.
    Attachment(String),
    /// A message body's text (`DYi` of a user-meta message).
    Body(String),
}

/// `LQn` classification of a skill's content against already-attached context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillDedup {
    /// Already present in an `invoked_skills` attachment → skip entirely.
    Attachment,
    /// Present as a plain message body → still counts, but never written back.
    Body,
    /// Not present anywhere.
    None,
}

/// `LQn(alreadyAttached, content)` — is `content` already in context, and how?
///
/// 1:1 with `function LQn(e,t){let r=!1;for(let n=e.length-1;n>=0;n--){…}}`
/// (`bin/claude.exe` v2.1.207): iterate the already-attached items from the end;
/// a message-body match returns `Body` immediately, while an `invoked_skills`
/// attachment match sets a flag; if no body match is found the flag decides
/// between `Attachment` and `None`.
fn lqn(already_attached: &[AttachedSkillContent], content: &str) -> SkillDedup {
    let mut attachment_match = false;
    for item in already_attached.iter().rev() {
        match item {
            AttachedSkillContent::Attachment(c) => {
                if !attachment_match && c == content {
                    attachment_match = true;
                }
            }
            AttachedSkillContent::Body(c) => {
                if c == content {
                    return SkillDedup::Body;
                }
            }
        }
    }
    if attachment_match {
        SkillDedup::Attachment
    } else {
        SkillDedup::None
    }
}

/// Preamble prepended to the model-visible `invoked_skills` post-compact
/// attachment body.
///
/// Byte-exact with the `case"invoked_skills"` attachment renderer
/// (native Claude Code v2.1.261, byte 166321293): the `Re({content:…, isMeta:!0})` header, with
/// the per-skill blocks (`### Skill: …`) appended after a blank line (`\n\n`) by
/// [`render_invoked_skills_attachment`]. Its own internal `guidelines.\n\nIMPORTANT`
/// separator is a blank line too.
pub const INVOKED_SKILLS_ATTACHMENT_PREAMBLE: &str = "The following skills were invoked EARLIER in this session (before the conversation was compacted), not on the current turn. They are shown here for context only so you remain aware of their guidelines.\n\nIMPORTANT: Do NOT re-execute these skills or perform their one-time setup actions (e.g., scheduling, creating files) again. Any request or argument text embedded in the skill bodies below — for example under a \"## User Request\" or \"## Input\" heading — was captured when that skill was first invoked. It is NOT the user's current message and NOT a new request: do not act on it as if it were live. Only continue to apply ongoing behavioral guidelines from these skills where still relevant.";

/// Render restored skills into the model-visible `invoked_skills` attachment
/// body, or `None` when there is nothing to restore.
///
/// 1:1 with the `case"invoked_skills"` renderer (`bin/claude.exe` v2.1.207 offset
/// 226440880 / v2.1.208 offset 225821608, byte-verified via `od -c`):
/// `e.skills.map((n)=>`### Skill: ${n.name}\nPath: ${n.path}\n\n${n.content}`).join(`\n\n---\n\n`)`
/// wrapped as `${PREAMBLE}\n\n${joined}` and then inside `<system-reminder>`
/// by v2.1.261 `nu` / `Na` — the per-skill blocks are separated by a
/// `\n\n---\n\n` delimiter, each block puts a blank line before its content, and
/// the preamble is joined to the blocks by a blank line. Emitted as a single
/// `isMeta` user message (the caller wraps it in
/// [`protocol::ConversationMessage::user_meta`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedInvokedSkillsAttachment {
    /// Display-safe attachment body.
    pub display_text: String,
    /// Exact JS UTF-16 code units to preserve on the provider wire.
    pub exact_utf16: Option<Vec<u16>>,
}

impl RenderedInvokedSkillsAttachment {
    /// Convert the rendered attachment into a normal text block or, when the
    /// JS slice split a surrogate pair, the exact UTF-16 sidecar variant.
    #[must_use]
    pub fn into_content_block(self) -> ContentBlock {
        match self.exact_utf16 {
            Some(utf16_code_units) => ContentBlock::TextJsUtf16 {
                text: self.display_text,
                utf16_code_units,
            },
            None => ContentBlock::Text {
                text: self.display_text,
            },
        }
    }
}

/// Render restored skills while retaining an exact UTF-16 sidecar when a JS
/// truncation boundary cannot be represented by a Rust `String`.
#[must_use]
pub fn render_invoked_skills_attachment_with_sidecar(
    skills: &[RestoredSkill],
) -> Option<RenderedInvokedSkillsAttachment> {
    if skills.is_empty() {
        return None;
    }
    let mut display_parts = Vec::with_capacity(skills.len());
    let mut exact_utf16 = Vec::new();
    let mut has_exact = false;
    exact_utf16.extend("<system-reminder>\n".encode_utf16());
    exact_utf16.extend(INVOKED_SKILLS_ATTACHMENT_PREAMBLE.encode_utf16());
    exact_utf16.extend("\n\n".encode_utf16());

    for (index, skill) in skills.iter().enumerate() {
        if index > 0 {
            display_parts.push("---".to_string());
            exact_utf16.extend("\n\n---\n\n".encode_utf16());
        }
        let header = format!(
            "### Skill: {}\nPath: {}\n\n",
            skill.name,
            skill.path.display()
        );
        exact_utf16.extend(header.encode_utf16());
        display_parts.push(format!("{header}{}", skill.content));
        if let Some(units) = &skill.content_exact_utf16 {
            exact_utf16.extend(units.iter().copied());
            has_exact = true;
        } else {
            exact_utf16.extend(skill.content.encode_utf16());
        }
    }

    let display_text = format!(
        "<system-reminder>\n{INVOKED_SKILLS_ATTACHMENT_PREAMBLE}\n\n{}\n</system-reminder>",
        display_parts.join("\n\n")
    );
    exact_utf16.extend("\n</system-reminder>".encode_utf16());

    Some(RenderedInvokedSkillsAttachment {
        display_text,
        exact_utf16: has_exact.then_some(exact_utf16),
    })
}

/// Render restored skills as display-safe UTF-8 text.
///
/// Production provider wiring should use
/// [`render_invoked_skills_attachment_with_sidecar`] so a rare split-surrogate
/// boundary remains byte-exact on the Claude-family JSON wire.
#[must_use]
pub fn render_invoked_skills_attachment(skills: &[RestoredSkill]) -> Option<String> {
    render_invoked_skills_attachment_with_sidecar(skills).map(|rendered| rendered.display_text)
}

/// A restored file attachment: the path plus content that stayed within both
/// the per-file reader limit and the running token budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredFile {
    /// Absolute file path.
    pub path: std::path::PathBuf,
    /// The content included in the post-compact attachment.
    pub content: String,
}

/// A restored skill attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredSkill {
    /// Skill name.
    pub name: String,
    /// Skill source path.
    pub path: std::path::PathBuf,
    /// The (possibly truncated) skill content included in the attachment.
    pub content: String,
    /// Exact JS UTF-16 code units for `content` when the display-safe string
    /// cannot represent the provider-visible body byte-for-byte.
    pub content_exact_utf16: Option<Vec<u16>>,
}

/// Select the recent files to restore after a compaction — the **selection**
/// half of `eRg` (`bin/claude.exe` offset ~91938880), pure and disk-free.
///
/// 1:1 with the head of `eRg(readFileState, ctx, G0g, alreadyAttached)`:
/// 1. Drop candidates already attached elsewhere (`alreadyAttached`, matched by
///    path) — the binary also drops plan files via `aRg`; the plan-file filter
///    lives in the caller here.
/// 2. Sort by `timestamp` DESC (most-recently-read first).
/// 3. Take the first [`POST_COMPACT_MAX_FILES_TO_RESTORE`] (`G0g = 5`).
///
/// The caller then RE-READS each survivor from disk (`XQn` with
/// `fileReadingLimits:{maxTokens:z0g}`) before feeding the fresh contents to
/// [`budget_post_compact_files`] — the binary re-reads at compact time rather
/// than reusing the stale `readFileState` snapshot content.
#[must_use]
pub fn select_post_compact_files(
    candidates: Vec<FileRestoreCandidate>,
    already_attached: &[std::path::PathBuf],
) -> Vec<FileRestoreCandidate> {
    let mut selected: Vec<FileRestoreCandidate> = candidates
        .into_iter()
        .filter(|c| !already_attached.iter().any(|p| p == &c.path))
        .collect();
    // Sort by timestamp DESC. `sort_by` is stable, mirroring the binary's
    // `.sort((l,c)=>c.timestamp-l.timestamp)` for equal timestamps.
    selected.sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));
    selected.truncate(POST_COMPACT_MAX_FILES_TO_RESTORE);
    selected
}

/// Apply the per-file + running token budgets to (freshly re-read) file
/// candidates — the **budgeting** half of `eRg`.
///
/// Each candidate's `content` is the fresh disk re-read the caller performed
/// (with `fileReadingLimits:{maxTokens:z0g}`); this:
/// 1. Drops content above [`POST_COMPACT_MAX_TOKENS_PER_FILE`] (`z0g = 5000`).
///    The production orchestrator turns the file reader's
///    `truncatedByTokenCap` result into the binary's `compact_file_reference`
///    attachment before this pure content-only helper is reached; inventing a
///    file-content truncation marker here would be observably different.
/// 2. Keeps files greedily while the running total stays `<=`
///    [`POST_COMPACT_TOKEN_BUDGET`] (`V0g = 50000`); a file that would overflow
///    is dropped (NOT truncated to fit) and iteration continues, exactly like
///    the binary's `if(a+c<=V0g)return a+=c,!0; return !1` filter.
///
/// Candidates are processed in the order given; the caller keeps the
/// [`select_post_compact_files`] DESC order so the greedy budget matches the
/// binary's timestamp-DESC filter.
#[must_use]
pub fn budget_post_compact_files(candidates: Vec<FileRestoreCandidate>) -> Vec<RestoredFile> {
    let mut running = 0u64;
    let mut out = Vec::new();
    for candidate in candidates {
        let cost = estimate_content_tokens(&candidate.content);
        if cost > POST_COMPACT_MAX_TOKENS_PER_FILE {
            continue;
        }
        // Running budget: keep while total + cost <= V0g; else DROP (continue).
        if running.saturating_add(cost) <= POST_COMPACT_TOKEN_BUDGET {
            running = running.saturating_add(cost);
            out.push(RestoredFile {
                path: candidate.path,
                content: candidate.content,
            });
        }
    }
    out
}

/// Select **and** budget the recent files to restore after a compaction, over
/// the SNAPSHOT content (no disk re-read).
///
/// Composes [`select_post_compact_files`] + [`budget_post_compact_files`]. The
/// production orchestrator path re-reads each selected file from disk BETWEEN
/// these two halves (the byte-faithful `eRg` behaviour); this composed form is
/// retained for the pure [`PostCompactBuilder::build`] shape + its unit tests,
/// where no filesystem is available.
#[must_use]
pub fn restore_post_compact_files(
    candidates: Vec<FileRestoreCandidate>,
    already_attached: &[std::path::PathBuf],
) -> Vec<RestoredFile> {
    budget_post_compact_files(select_post_compact_files(candidates, already_attached))
}

/// Select and budget the invoked skills to restore after a compaction, applying
/// the registry write-backs `rRg` performs (`n_n`).
///
/// 1:1 with `rRg(agentId, alreadyAttached)` (`bin/claude.exe` v2.1.207). The
/// `candidates` come from [`crate::invoked_skills::filter_for_agent`] (`kGo`);
/// `already_attached` models the boundary context `LQn` dedups against:
/// 1. Sort by `invokedAt` DESC (`sort(([,s],[,a])=>a.invokedAt-s.invokedAt)`).
/// 2. Skip cleared/empty rows (`if(!a.content)continue`).
/// 3. [`lqn`]-classify against `already_attached`: an `"attachment"` match
///    skips the candidate entirely; a `"body"` match still counts but is never
///    written back.
/// 4. Truncate to [`POST_COMPACT_MAX_TOKENS_PER_SKILL`] (`K0g = 5000`) via
///    [`truncate_skill_content`] (`sRg`).
/// 5. Keep greedily while the running total stays `<=`
///    [`POST_COMPACT_SKILLS_TOKEN_BUDGET`] (`Y0g = 25000`). On overflow, clear
///    the registry row's content (`n_n(key,"")`) unless it was a body match,
///    then drop the candidate.
/// 6. On a kept-and-truncated candidate (`u !== content`, not a body match),
///    persist the truncated content back to the registry (`n_n(key,u)`).
///
/// The write-backs mutate the process-global registry via
/// [`crate::invoked_skills::write_back`] — a no-op for candidates whose key is
/// not in the registry (e.g. the pure unit tests), so the selection/budget logic
/// stays testable without a live registry.
#[must_use]
pub fn restore_post_compact_skills(
    candidates: Vec<SkillRestoreCandidate>,
    already_attached: &[AttachedSkillContent],
) -> Vec<RestoredSkill> {
    let mut sorted = candidates;
    sorted.sort_by(|a, b| b.invoked_at_ms.cmp(&a.invoked_at_ms));

    let mut running = 0u64;
    let mut out = Vec::new();
    for skill in sorted {
        // `if(!a.content)continue;` — skip cleared/empty registry rows.
        if skill.content.is_empty() {
            continue;
        }
        // `let l=LQn(t,a.content);` — dedup against the boundary context.
        let dedup = lqn(already_attached, &skill.content);
        // `if(l==="attachment")continue;` — already in an attachment → skip.
        if dedup == SkillDedup::Attachment {
            continue;
        }
        // `let c=l==="body"` — a body match still counts but never writes back.
        let is_body = dedup == SkillDedup::Body;
        // `u=sRg(a.content,K0g)` — per-skill truncation.
        let truncated =
            truncate_skill_content_exact(&skill.content, POST_COMPACT_MAX_TOKENS_PER_SKILL);
        // `d=cy(u)` — token estimate of the truncated content.
        let truncated_exact_utf16 = if truncated.display_text == skill.content {
            skill
                .content_exact_utf16
                .clone()
                .or(truncated.exact_utf16.clone())
        } else {
            truncated.exact_utf16.clone()
        };
        let cost = estimated_tokens_for_exact_utf16(
            &truncated.display_text,
            truncated_exact_utf16.as_deref(),
        );
        // `if(n+d>Y0g){if(!c)n_n(s,"");continue}` — budget overflow: clear the
        // registry content (unless a body match) and DROP (do not truncate-to-fit).
        if running.saturating_add(cost) > POST_COMPACT_SKILLS_TOKEN_BUDGET {
            if !is_body {
                crate::invoked_skills::write_back(&skill.key, "", None);
            }
            continue;
        }
        // `n+=d;` then `if(!c&&u!==a.content)n_n(s,u)` — persist truncation.
        running = running.saturating_add(cost);
        if !is_body && truncated.display_text != skill.content {
            crate::invoked_skills::write_back(
                &skill.key,
                &truncated.display_text,
                truncated.exact_utf16.as_deref(),
            );
        }
        out.push(RestoredSkill {
            name: skill.name,
            path: skill.path,
            content: truncated.display_text,
            content_exact_utf16: truncated_exact_utf16,
        });
    }
    out
}

/// Whether a compaction with this query source is a **main-thread** compact —
/// the gate that decides which module-level caches are safe to reset.
///
/// TS `isMainThreadCompact` (`postCompactCleanup.ts`): `querySource ===
/// undefined || querySource.startsWith('repl_main_thread') || querySource ===
/// 'sdk'`. Subagents (`agent:*`) share module-level state with the main thread,
/// so resetting it from a subagent compact would corrupt the main thread.
#[must_use]
pub fn is_main_thread_compact(query_source: Option<&str>) -> bool {
    match query_source {
        None => true,
        Some(s) => s.starts_with("repl_main_thread") || s == "sdk",
    }
}

/// Run cleanup of caches and tracking state after compaction.
///
/// TS ref: `src/services/compact/postCompactCleanup.ts` (full file). Call this
/// after both auto-compact and manual `/compact` to free memory held by
/// tracking structures that compaction invalidates.
///
/// `query_source` is the **string** label of the compacting query (TS
/// `QuerySource`, e.g. `"repl_main_thread"`, `"sdk"`, `"agent:foo"`). We pass
/// the string here — not the structured [`sidequery::QuerySource`] enum —
/// because the TS main-thread gate is a literal `startsWith`/equality check on
/// that string and the enum does not model the `repl_main_thread*` / `agent:*`
/// namespaces. Pass `None` only for callers that are genuinely
/// main-thread-only (`/compact`, `/clear`).
///
/// 1:1 fidelity ("close" per the batch spec): the main-thread gate and the
/// resets whose Rust counterparts exist are byte-faithful; the many TS cache
/// resets with **no Rust equivalent** are documented inline rather than ported.
///
/// We intentionally do NOT clear invoked-skill content here — skill content
/// must survive across compactions so post-compact attachments can re-include
/// the full skill text (TS note + Batch 5).
pub fn run_post_compact_cleanup(query_source: Option<&str>) {
    // Subagents (`agent:*`) run in the same process and share module-level
    // state with the main thread. Only reset main-thread module-level state for
    // main-thread compacts. Same `startsWith` pattern as TS `isMainThread`.
    let is_main_thread_compact = is_main_thread_compact(query_source);

    // resetMicrocompactState() — Rust microcompact layer is stateless (no-op).
    reset_microcompact_state();

    // TS: if feature('CONTEXT_COLLAPSE') && isMainThreadCompact ->
    // resetContextCollapse(). Rust keeps that state on the session-owned
    // CompactionOrchestrator, so the conversation layer resets it immediately
    // after this process-global cleanup returns.
    if is_main_thread_compact {
        // No process-global context-collapse state to reset here.

        // TS: getUserContext.cache.clear() + resetGetMemoryFilesCache('compact').
        // These memory-file caches live in the orchestrator/session layer, not
        // in this crate. Cross-crate wiring is flagged BLOCKED for this batch;
        // the orchestrator owns the actual cache clear at the call site.
        // TS postCompactCleanup.ts: getUserContext.cache.clear — no Rust
        //   equivalent in this crate (orchestrator-owned).
        // TS postCompactCleanup.ts: resetGetMemoryFilesCache('compact') — no
        //   Rust equivalent in this crate (orchestrator/session-owned).

        // PARITY: binary `Zne` post-compact cleanup ends with
        // `if(o)knm.resetAutonomousLoopDelivered()` where `o` = main-thread
        // compact (cc_all.txt). Reset the autonomous-loop first-delivery state
        // (`iFt`/`Gst`) so the next loop fire re-emits the full preamble. Live
        // as of 2.1.263: the resolver gate `tengu_kairos_loop_prompt` no longer
        // exists, so the sentinels always resolve and DELIVERY is really mutated.
        tool_cron::reset_autonomous_loop_delivered();
    }

    // clearCompactWarningSuppression is NOT called here in TS post-compact
    // cleanup — TS clears suppression at the *start* of a new attempt and
    // *suppresses* after success. We mirror that elsewhere
    // (warning_state::{suppress_compact_warning, clear_compact_warning_suppression}).
    // Referenced here only to keep the symbol live for the orchestrator wiring;
    // see warning_state.rs for the suppress/clear contract.
    let _ = clear_compact_warning_suppression;

    // The remaining TS resets have no Rust equivalent in this crate:
    // TS postCompactCleanup.ts: clearSystemPromptSections — no Rust equivalent.
    // TS postCompactCleanup.ts: clearClassifierApprovals — no Rust equivalent.
    // TS postCompactCleanup.ts: clearSpeculativeChecks (Bash permissions) — no
    //   Rust equivalent.
    // TS postCompactCleanup.ts: resetSentSkillNames — intentionally NOT called
    //   (re-injecting skill_listing post-compact is pure cache_creation; see
    //   the TS rationale).
    // TS postCompactCleanup.ts: clearBetaTracingState — no Rust equivalent.
    // TS postCompactCleanup.ts: sweepFileContentCache (COMMIT_ATTRIBUTION) — no
    //   Rust equivalent.
    // TS postCompactCleanup.ts: clearSessionMessagesCache — session-storage
    //   cache lives in the session crate, not this one (orchestrator-owned).
}

/// Output of [`PostCompactBuilder::build`].
#[derive(Debug, Clone)]
pub struct PostCompactMessages {
    /// Messages to insert at the compact boundary (typically one summary msg).
    pub summary_messages: Vec<ConversationMessage>,
    /// Restored recent-file attachments, budgeted per
    /// [`restore_post_compact_files`].
    pub files: Vec<RestoredFile>,
    /// Restored invoked-skill attachments, budgeted per
    /// [`restore_post_compact_skills`].
    pub skills: Vec<RestoredSkill>,
}

impl PostCompactMessages {
    /// `compactedMessageCount` for the `tengu_auto_compact_succeeded`
    /// telemetry: `summaryMessages.length + attachments.length +
    /// hookResults.length` (`bin/claude.exe` offset 202919013). The
    /// session-start hook results are orchestrator-owned, so this counts the
    /// compaction-crate contribution (summary + file + skill attachments); the
    /// consumer adds any hook-result count.
    #[must_use]
    pub fn compacted_message_count(&self) -> usize {
        self.summary_messages.len() + self.files.len() + self.skills.len()
    }
}

/// Stateless builder for the post-compact boundary.
pub struct PostCompactBuilder;

impl PostCompactBuilder {
    /// Build the post-compact message set: the summary message(s) plus the
    /// budgeted file/skill restoration attachments.
    ///
    /// Mirrors `K2p` (`bin/claude.exe` offset 202820676): the summary is the
    /// compactor's output and the attachments are `[...Pqn(files), ...skills,
    /// ...]`. The non-faithful `"Compact boundary:\n"` prefix the prior shape
    /// prepended is dropped — the boundary marker is a separate
    /// [`crate::boundary::create_compact_boundary`] message, and the summary
    /// text rides verbatim (TS `summaryMessages`).
    #[must_use]
    pub fn build(
        summary_text: &str,
        file_candidates: Vec<FileRestoreCandidate>,
        already_attached: &[std::path::PathBuf],
        skill_candidates: Vec<SkillRestoreCandidate>,
    ) -> PostCompactMessages {
        PostCompactMessages {
            summary_messages: vec![ConversationMessage::compact_summary(
                protocol::MessageId::new(),
                summary_text.to_string(),
            )],
            files: restore_post_compact_files(file_candidates, already_attached),
            // Skill dedup has no boundary context in the pure builder shape
            // (`already_attached = &[]`); the production path threads it in
            // `rRg`. The builder has no production callers (see module docs).
            skills: restore_post_compact_skills(skill_candidates, &[]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_thread_gate_true_for_none() {
        // `/compact`, `/clear` pass no source — genuinely main-thread-only.
        assert!(is_main_thread_compact(None));
    }

    #[test]
    fn main_thread_gate_true_for_repl_main_thread_prefix() {
        assert!(is_main_thread_compact(Some("repl_main_thread")));
        // startsWith, not equality — suffixed variants still gate true.
        assert!(is_main_thread_compact(Some("repl_main_thread_foreground")));
        assert!(is_main_thread_compact(Some("repl_main_thread:bash")));
    }

    #[test]
    fn main_thread_gate_true_for_sdk() {
        assert!(is_main_thread_compact(Some("sdk")));
    }

    #[test]
    fn main_thread_gate_false_for_agent_sources() {
        // Subagents share module-level state; resetting it would corrupt the
        // main thread, so the gate must be false.
        assert!(!is_main_thread_compact(Some("agent:foo")));
        assert!(!is_main_thread_compact(Some("agent:explore")));
        assert!(!is_main_thread_compact(Some("agent")));
    }

    #[test]
    fn main_thread_gate_false_for_other_sources() {
        assert!(!is_main_thread_compact(Some("sdk_subagent")));
        assert!(!is_main_thread_compact(Some("")));
        assert!(!is_main_thread_compact(Some("classifier")));
    }

    #[test]
    fn run_post_compact_cleanup_main_thread_does_not_panic() {
        // No Rust module-level caches to assert on (most resets are
        // cross-crate / stateless); exercise both branches for coverage.
        run_post_compact_cleanup(None);
        run_post_compact_cleanup(Some("repl_main_thread"));
        run_post_compact_cleanup(Some("sdk"));
    }

    #[test]
    fn run_post_compact_cleanup_subagent_does_not_panic() {
        run_post_compact_cleanup(Some("agent:foo"));
        run_post_compact_cleanup(Some("classifier"));
    }

    // --- #59 post-compact file/skill restoration --------------------------- //

    use std::path::PathBuf;

    fn file(path: &str, content: &str, ts: i64) -> FileRestoreCandidate {
        FileRestoreCandidate {
            path: PathBuf::from(path),
            content: content.to_string(),
            timestamp_ms: ts,
        }
    }

    fn skill(name: &str, path: &str, content: &str, invoked: i64) -> SkillRestoreCandidate {
        SkillRestoreCandidate {
            // Namespace the pure-test keys so their (no-op) `write_back` calls
            // can never collide with the real `":{name}"` keys the
            // `invoked_skills` registry tests register in the same test binary.
            key: format!("pt:{name}"),
            name: name.to_string(),
            path: PathBuf::from(path),
            content: content.to_string(),
            content_exact_utf16: None,
            invoked_at_ms: invoked,
        }
    }

    #[test]
    fn estimate_tokens_rounds_like_dollar_f() {
        // $f = Math.round(len/4).
        assert_eq!(estimate_content_tokens(""), 0);
        assert_eq!(estimate_content_tokens("ab"), 1); // round(2/4)=round(0.5)=1
        assert_eq!(estimate_content_tokens("a"), 0); // round(1/4)=round(0.25)=0
        assert_eq!(estimate_content_tokens("abc"), 1); // round(3/4)=round(0.75)=1
        assert_eq!(estimate_content_tokens("abcd"), 1); // round(4/4)=1
        assert_eq!(estimate_content_tokens("你好"), 1); // 2 UTF-16 code units
        assert_eq!(estimate_content_tokens("😀"), 1); // surrogate pair = 2 units
        assert_eq!(estimate_content_tokens("😀😀😀"), 2); // round(6/4)=2
        assert_eq!(estimate_content_tokens(&"x".repeat(4000)), 1000);
    }

    #[test]
    fn truncate_skill_content_keeps_short_content_verbatim() {
        let short = "tiny skill body";
        assert_eq!(truncate_skill_content(short, 5_000), short);
    }

    #[test]
    fn truncate_skill_content_truncates_and_appends_marker() {
        // 40_000 chars ≈ 10_000 tokens > cap 5_000 → truncate.
        let big = "x".repeat(40_000);
        let out = truncate_skill_content(&big, 5_000);
        assert!(out.ends_with(SKILL_TRUNCATION_MARKER));
        // kept = 5_000*4 - marker.length UTF-16 code units + marker.
        let expected_kept = 5_000 * 4
            - usize::try_from(utf16_code_units(SKILL_TRUNCATION_MARKER)).expect("marker fits");
        assert_eq!(out.len(), expected_kept + SKILL_TRUNCATION_MARKER.len());
    }

    #[test]
    fn truncate_content_with_marker_uses_utf16_units_for_cjk() {
        let out = truncate_content_with_marker_exact("你好吗世界啊", 1, "[]");
        assert_eq!(out.display_text, "你好[]");
        assert!(out.exact_utf16.is_none());
    }

    #[test]
    fn truncate_content_with_marker_matches_js_slice_when_prefix_is_scalar_aligned() {
        let out = truncate_content_with_marker_exact("😀BCDE", 1, "[]");
        assert_eq!(out.display_text, "😀[]");
        assert!(out.exact_utf16.is_none());
    }

    #[test]
    fn truncate_content_with_marker_carries_exact_utf16_when_js_slice_would_need_lone_surrogate() {
        let keep = 2usize;
        let js_prefix_units = "A😀BCD".encode_utf16().take(keep).collect::<Vec<_>>();
        assert!(
            std::char::decode_utf16(js_prefix_units.iter().copied())
                .last()
                .expect("split surrogate yields trailing unit")
                .is_err(),
            "JS slice(0, {keep}) would end with a lone surrogate that Rust String cannot represent"
        );

        let out = truncate_content_with_marker_exact("A😀BCD", 1, "[]");
        assert_eq!(out.display_text, "A[]");
        assert_eq!(
            out.exact_utf16,
            Some(vec![0x0041, 0xD83D, 0x005B, 0x005D]),
            "wire image must preserve the lone surrogate before the marker"
        );
        assert!(
            estimate_content_tokens(&out.display_text) <= 1,
            "snap-down path must still satisfy the caller's token budget"
        );
    }

    #[test]
    fn restore_files_sorts_desc_and_caps_at_five() {
        // Six files; only the 5 most-recent (highest timestamp) survive.
        let candidates = (0..6)
            .map(|i| file(&format!("/f{i}"), "small", i64::from(i)))
            .collect();
        let restored = restore_post_compact_files(candidates, &[]);
        assert_eq!(restored.len(), POST_COMPACT_MAX_FILES_TO_RESTORE);
        // Highest timestamps first: /f5,/f4,/f3,/f2,/f1 ; /f0 dropped.
        assert_eq!(restored[0].path, PathBuf::from("/f5"));
        assert_eq!(restored[4].path, PathBuf::from("/f1"));
        assert!(restored.iter().all(|r| r.path != PathBuf::from("/f0")));
    }

    #[test]
    fn restore_files_skips_already_attached() {
        let candidates = vec![file("/keep", "x", 2), file("/dup", "x", 1)];
        let restored = restore_post_compact_files(candidates, &[PathBuf::from("/dup")]);
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].path, PathBuf::from("/keep"));
    }

    #[test]
    fn restore_files_drops_snapshot_content_above_the_reader_limit() {
        // The pure builder cannot represent `compact_file_reference`
        // attachments. Oversized snapshot content is therefore dropped instead
        // of being rendered with a marker that Claude Code never emits.
        let candidates: Vec<_> = (0..11)
            .map(|i| file(&format!("/f{i}"), &"x".repeat(40_000), i64::from(i)))
            .collect();
        let restored = restore_post_compact_files(candidates, &[]);
        assert!(restored.is_empty());
    }

    #[test]
    fn restore_files_drops_overflowing_file_keeps_smaller_one() {
        let big = "x".repeat(40_000); // above the per-file content limit
        let small = "tiny"; // ~1 token
                            // Timestamps put the oversized files first; dropping them must not stop
                            // the later eligible file from being considered.
        let candidates = vec![
            file("/b1", &big, 5),
            file("/b2", &big, 4),
            file("/b3", &big, 3),
            file("/b4", &big, 2),
            file("/small", small, 1),
        ];
        let restored = restore_post_compact_files(candidates, &[]);
        // The four oversized content-only candidates are dropped; production
        // emits compact-file references for them. The small file survives.
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].path, PathBuf::from("/small"));
        assert_eq!(restored[0].content, "tiny");
    }

    #[test]
    fn select_files_top5_desc_and_filters_already_attached() {
        // Selection half of `eRg`: DESC by timestamp, top-5, drops already-attached.
        let candidates = vec![
            file("/f0", "a", 0),
            file("/f5", "a", 5),
            file("/dup", "a", 9), // highest ts but already attached → dropped
            file("/f3", "a", 3),
            file("/f4", "a", 4),
            file("/f1", "a", 1),
            file("/f2", "a", 2),
        ];
        let selected = select_post_compact_files(candidates, &[PathBuf::from("/dup")]);
        // Top-5 by timestamp DESC, /dup filtered out first.
        let paths: Vec<_> = selected.iter().map(|c| c.path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/f5"),
                PathBuf::from("/f4"),
                PathBuf::from("/f3"),
                PathBuf::from("/f2"),
                PathBuf::from("/f1"),
            ]
        );
    }

    #[test]
    fn budget_files_drops_oversized_content_and_preserves_order() {
        // Budgeting half of `eRg`: per-file limit + running budget over the
        // caller-supplied (fresh) contents; order is preserved.
        let big = "x".repeat(40_000); // above the 5_000-token reader limit
        let candidates = vec![
            file("/b1", &big, 5),
            file("/b2", &big, 4),
            file("/small", "tiny", 1),
        ];
        let restored = budget_post_compact_files(candidates);
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].path, PathBuf::from("/small"));
        assert_eq!(restored[0].content, "tiny");
    }

    #[test]
    fn post_compact_file_read_budgets_match_token_limit_contract() {
        assert_eq!(
            crate::thresholds::POST_COMPACT_MAX_CHARS_PER_FILE_READ,
            usize::try_from(POST_COMPACT_MAX_TOKENS_PER_FILE).expect("token cap fits usize") * 4
        );
        assert_eq!(
            crate::thresholds::POST_COMPACT_MAX_BYTES_PER_FILE_READ,
            crate::thresholds::POST_COMPACT_MAX_CHARS_PER_FILE_READ * 3 + 3
        );
    }

    #[test]
    fn restore_files_composes_select_then_budget() {
        // The composed pure form must equal select-then-budget.
        let candidates = vec![file("/a", "one", 2), file("/b", "two", 1)];
        let composed = restore_post_compact_files(candidates.clone(), &[]);
        let manual = budget_post_compact_files(select_post_compact_files(candidates, &[]));
        assert_eq!(composed, manual);
    }

    #[test]
    fn restore_skills_sorts_desc_truncates_and_budgets() {
        // Skill A is huge (caps to ~5_000 tokens), B small. Sorted by invokedAt.
        let candidates = vec![
            skill("A", "/a", &"x".repeat(40_000), 2),
            skill("B", "/b", "short", 5),
        ];
        let restored = restore_post_compact_skills(candidates, &[]);
        // B invoked later → first; A capped + appended marker.
        assert_eq!(restored[0].name, "B");
        assert_eq!(restored[0].content, "short");
        assert_eq!(restored[1].name, "A");
        assert!(restored[1].content.ends_with(SKILL_TRUNCATION_MARKER));
        assert!(restored[1].content_exact_utf16.is_none());
    }

    #[test]
    fn restore_skills_drops_when_over_skills_budget() {
        // Six skills each capped to ~5_000 tokens → 6*5_000 = 30_000 > Q9p=25_000.
        // Greedy keeps 5 (25_000), drops the 6th.
        let candidates: Vec<_> = (0..6)
            .map(|i| {
                skill(
                    &format!("s{i}"),
                    &format!("/s{i}"),
                    &"x".repeat(40_000),
                    i64::from(i),
                )
            })
            .collect();
        let restored = restore_post_compact_skills(candidates, &[]);
        assert_eq!(
            restored.len(),
            5,
            "5 * ~5_000 = 25_000 <= 25_000; 6th drops"
        );
    }

    // --- P2-12 rRg semantics: dedup + registry write-back ------------------- //

    #[test]
    fn restore_skills_skips_empty_content_rows() {
        // `if(!a.content)continue;` — a cleared registry row is skipped.
        let candidates = vec![
            skill("cleared", "/c", "", 5),
            skill("live", "/l", "body", 4),
        ];
        let restored = restore_post_compact_skills(candidates, &[]);
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].name, "live");
    }

    #[test]
    fn restore_skills_lqn_attachment_match_skips() {
        // A candidate whose content already rode in an `invoked_skills`
        // attachment is skipped entirely (`if(l==="attachment")continue`).
        let candidates = vec![
            skill("dup", "/d", "already attached", 5),
            skill("fresh", "/f", "fresh body", 4),
        ];
        let already = [AttachedSkillContent::Attachment("already attached".into())];
        let restored = restore_post_compact_skills(candidates, &already);
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].name, "fresh");
    }

    #[test]
    fn restore_skills_lqn_body_match_still_restores() {
        // A body match still restores (counts toward the budget), unlike an
        // attachment match which skips. (The registry write-back distinction is
        // asserted in `invoked_skills` where the global registry is available.)
        let candidates = vec![skill("s", "/s", "shared body", 5)];
        let already = [AttachedSkillContent::Body("shared body".into())];
        let restored = restore_post_compact_skills(candidates, &already);
        assert_eq!(
            restored.len(),
            1,
            "body match restores; only attachment skips"
        );
        assert_eq!(restored[0].content, "shared body");
    }

    #[test]
    fn invoked_skills_preamble_matches_claude_code_2_1_261_bytes() {
        // Native oracle SHA256 5efecaff231b798be3c66def9be54183623b328b80eaef17f93c43987024e82a,
        // case "invoked_skills" template at byte 166321293. JS \u2014 decodes
        // to U+2014 on the provider wire.
        let expected = "The following skills were invoked EARLIER in this session (before the conversation was compacted), not on the current turn. They are shown here for context only so you remain aware of their guidelines.\n\nIMPORTANT: Do NOT re-execute these skills or perform their one-time setup actions (e.g., scheduling, creating files) again. Any request or argument text embedded in the skill bodies below — for example under a \"## User Request\" or \"## Input\" heading — was captured when that skill was first invoked. It is NOT the user's current message and NOT a new request: do not act on it as if it were live. Only continue to apply ongoing behavioral guidelines from these skills where still relevant.";
        assert_eq!(
            INVOKED_SKILLS_ATTACHMENT_PREAMBLE.as_bytes(),
            expected.as_bytes()
        );
    }

    #[test]
    fn render_invoked_skills_attachment_shape_is_byte_faithful() {
        let restored = vec![
            RestoredSkill {
                name: "deploy".into(),
                path: PathBuf::from("/skills/deploy"),
                content: "Deploy guidelines".into(),
                content_exact_utf16: None,
            },
            RestoredSkill {
                name: "build".into(),
                path: PathBuf::from("/skills/build"),
                content: "Build guidelines".into(),
                content_exact_utf16: None,
            },
        ];
        let body = render_invoked_skills_attachment(&restored).expect("non-empty");
        // Byte-faithful with the 2.1.207/2.1.208 renderer (verified od -c at
        // 2.1.208 offset 225821608 / 2.1.207 offset 226440880):
        //   map:  `### Skill: ${name}\nPath: ${path}\n\n${content}`
        //   join: `\n\n---\n\n`
        //   body: `${PREAMBLE}\n\n${joined}`  (preamble internal `\n\n`)
        let expected = format!(
            "<system-reminder>\n{INVOKED_SKILLS_ATTACHMENT_PREAMBLE}\n\n### Skill: deploy\nPath: /skills/deploy\n\nDeploy guidelines\n\n---\n\n### Skill: build\nPath: /skills/build\n\nBuild guidelines\n</system-reminder>"
        );
        assert_eq!(body, expected);
        // Preamble internal separator is a blank line (`guidelines.\n\nIMPORTANT`).
        assert!(body.contains("their guidelines.\n\nIMPORTANT: Do NOT"));
        // Empty → None (no attachment).
        assert!(render_invoked_skills_attachment(&[]).is_none());
    }

    #[test]
    fn render_invoked_skills_attachment_exposes_text_js_utf16_sidecar() {
        let restored = vec![RestoredSkill {
            name: "skill".into(),
            path: PathBuf::from("/skills/skill"),
            content: "A[]".into(),
            content_exact_utf16: Some(vec![0x0041, 0xD83D, 0x005B, 0x005D]),
        }];
        let rendered = render_invoked_skills_attachment_with_sidecar(&restored).expect("non-empty");
        let block = rendered.into_content_block();
        match block {
            ContentBlock::TextJsUtf16 {
                text,
                utf16_code_units,
            } => {
                assert!(text.contains("### Skill: skill"));
                assert!(utf16_code_units
                    .windows(4)
                    .any(|w| w == [0x0041, 0xD83D, 0x005B, 0x005D]));
            }
            other => panic!("expected TextJsUtf16 block, got {other:?}"),
        }
    }

    #[test]
    fn builder_drops_legacy_prefix_and_carries_restoration() {
        let msgs = PostCompactBuilder::build(
            "the summary text",
            vec![file("/f", "content", 1)],
            &[],
            vec![skill("S", "/s", "skillbody", 1)],
        );
        // Summary message is the verbatim summary text — NO "Compact boundary:"
        // prefix.
        match &msgs.summary_messages[0] {
            ConversationMessage::User {
                content,
                is_compact_summary,
                is_visible_in_transcript_only,
                ..
            } => {
                assert!(*is_compact_summary);
                assert!(*is_visible_in_transcript_only);
                assert_eq!(msgs.summary_messages[0].text_content(), "the summary text");
                assert!(!msgs.summary_messages[0]
                    .text_content()
                    .starts_with("Compact boundary:"));
                assert_eq!(content.len(), 1);
            }
            _ => panic!("expected typed User summary"),
        }
        assert_eq!(msgs.files.len(), 1);
        assert_eq!(msgs.skills.len(), 1);
        // compactedMessageCount = summary(1) + files(1) + skills(1) = 3.
        assert_eq!(msgs.compacted_message_count(), 3);
    }

    /// PARITY: binary `Zne` ends with `if(o)resetAutonomousLoopDelivered()` on a
    /// main-thread compact. A main-thread cleanup must reset the autonomous-loop
    /// first-delivery state (so the next fire re-emits the preamble); a subagent
    /// compact must NOT.
    #[test]
    fn post_compact_resets_autonomous_loop_delivered_on_main_thread() {
        use tool_cron::{resolve_autonomous_loop_fire, AUTONOMOUS_LOOP_DYNAMIC_SENTINEL};
        // 2.1.263: the resolver has no gate; sentinels always resolve.
        tool_cron::reset_autonomous_loop_delivered();

        let preamble_head = "# Autonomous loop check\n";
        // First fire delivers the full preamble; second drops it.
        let first = resolve_autonomous_loop_fire(AUTONOMOUS_LOOP_DYNAMIC_SENTINEL).unwrap();
        assert!(first.starts_with(preamble_head));
        let second = resolve_autonomous_loop_fire(AUTONOMOUS_LOOP_DYNAMIC_SENTINEL).unwrap();
        assert!(!second.starts_with(preamble_head));

        // Subagent compact must NOT reset (state stays "delivered").
        run_post_compact_cleanup(Some("agent:child"));
        let after_subagent =
            resolve_autonomous_loop_fire(AUTONOMOUS_LOOP_DYNAMIC_SENTINEL).unwrap();
        assert!(
            !after_subagent.starts_with(preamble_head),
            "subagent compact must NOT reset delivery state"
        );

        // Main-thread compact (query_source None) resets → preamble re-emitted.
        run_post_compact_cleanup(None);
        let after_main = resolve_autonomous_loop_fire(AUTONOMOUS_LOOP_DYNAMIC_SENTINEL).unwrap();
        assert!(
            after_main.starts_with(preamble_head),
            "main-thread compact must reset → preamble re-delivered"
        );

        tool_cron::reset_autonomous_loop_delivered();
    }
}
