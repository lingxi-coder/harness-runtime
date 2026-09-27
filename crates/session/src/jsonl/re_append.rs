//! Session-metadata re-append — 1:1 port of claude-code 2.1.220's
//! `Isp.reAppendSessionMetadata` / `Isp.planReAppendSessionMetadata`.
//!
//! # Why this exists
//!
//! Every fast session-index reader in claude-code (and in `LingXi`:
//! [`crate::jsonl::reader::JsonlReader::read_lite`], the picker catalog) only
//! scans a bounded window of the transcript. Metadata sidecar records
//! (`custom-title`, `mode`, `worktree-state`, …) are written ONCE, at the
//! moment they change — so in a long session they scroll out of that window
//! and the session silently loses its title / mode / worktree state as far as
//! every tail-scanning reader is concerned.
//!
//! claude-code fixes this by periodically REBUILDING the whole metadata record
//! set from in-memory state and appending it again at the end of the file, with
//! a dedup pass so nothing is written when the tail already carries an
//! identical record.
//!
//! # Oracle provenance (2.1.220 Mach-O, readable JS region)
//!
//! | Symbol | Offset | Role |
//! |---|---|---|
//! | `reAppendSessionMetadata(e=!1,t=!1)` | 237852347 | sync entry, method on class `Isp` |
//! | `reAppendSessionMetadataAsync(e=!1,t=!1)` | 237852577 | async entry (single joined append) |
//! | `planReAppendSessionMetadata(e,t,r)` | 237853135 | adopt-back + rebuild + dedup |
//! | `jBy(e)` / `WBy(e)` | 237892252 / 237893153 | 64 KiB tail readers, `""` on any error |
//! | `TC(e,t)` | 226610554 | last-occurrence quoted-scalar extractor |
//! | `sRt(e,t,r)` | 226610818 | backwards line scan + `JSON.parse` |
//! | `GOi(e)` | 226610241 | JSON string unescape (no-op when no `\`) |
//! | `ma(e,t)` | 225924719 | UTF-16 truncate with surrogate guard |
//! | `kI=65536` | 226619695 | tail window; `kI/2` = 32768 = backstop + dedup budget |
//! | counter increment | 237850612 | `bytesSinceMetadataReAppend += byteLength(payload)` |
//!
//! # Argument polarity (verified from the body, NOT from any name)
//!
//! Both flags are *skip* flags — getting them backwards silently clobbers
//! resumed session titles with stale 64 KiB-tail values:
//! - arg1 `skip_title_adopt` — `true` SUPPRESSES re-reading `custom-title` /
//!   `ai-title` out of the tail into in-memory state. The resume trigger
//!   (`adoptResumedSessionFile`, offset 237870566) passes `true` because the
//!   resume loader already hydrated titles from the FULL file.
//! - arg2 `skip_dedup` — `true` FORCES the write (post-compaction paths use it,
//!   because compaction just destroyed the tail).

use crate::jsonl::reader::is_valid_atis_latch;
use crate::jsonl::LITE_READ_BUF_SIZE;
use serde_json::{Map, Value};
use std::path::Path;

/// Byte budget that gates both the periodic backstop and the dedup scan —
/// `kI/2` where `kI = 65536` (oracle 226619695).
///
/// Backstop: once this many bytes have been appended to the current session
/// file since the last re-append, the metadata set is re-appended (oracle
/// 237851998, tail of `drainQueuesOnce`, with `skip_dedup = true`).
pub const METADATA_REAPPEND_BACKSTOP_BYTES: usize = LITE_READ_BUF_SIZE / 2;

// ── SC-08: the reclamation half now exists ──────────────────────────────────
//
// This module is the GROWTH side: every backstop firing appends a fresh copy of
// the whole metadata set, so a long session accumulates superseded sidecar
// records. Upstream pairs it with `performCompactTranscript` (cc-238.js
// @296788068), which rewrites `<sid>.jsonl` through
// `<sid>.jsonl.compact.tmp.<8 hex>`, dropping the records the compaction policy
// table `aqT` marks superseded, then re-appends the metadata set with
// `skip_dedup = true`.
//
// That half is ported in [`crate::jsonl::transcript_compact`] and driven from
// [`crate::jsonl::writer::JsonlWriter::maybe_compact_transcript`], which the
// ordinary append path calls — armed both by the 20 MiB byte backstop and by
// the moment a `compact_boundary` line is persisted. It is gated on
// `LINGXI_TRANSCRIPT_LOCAL_GC`, mirroring upstream's `localGcEnabled`, which is
// also off in a default install.

/// `normalizeLastPrompt` truncation width, in UTF-16 code units
/// (oracle: `t.length>200?ma(t,200)…`).
const LAST_PROMPT_MAX_UTF16: usize = 200;

/// In-memory mirror of the oracle's `currentSession*` fields on class `Isp` —
/// the sole input to [`plan_re_append`]'s rebuild step.
///
/// Every `Option<String>` field is subject to JS truthiness at the oracle:
/// `Some(String::new())` is FALSY and suppresses its record, exactly like `""`
/// in `if(this.currentSessionTitle)`. Use [`SessionMetadataState::default`] and
/// set only what the session actually has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionMetadataState {
    /// `currentSessionTitle` → `{type:"custom-title",customTitle,sessionId}`.
    pub title: Option<String>,
    /// `currentSessionAiTitle` → `{type:"ai-title",aiTitle,sessionId}`.
    pub ai_title: Option<String>,
    /// `currentSessionTag` → `{type:"tag",tag,sessionId}`.
    pub tag: Option<String>,
    /// `currentSessionRelocatedCwd` → `{type:"relocated",relocatedCwd,sessionId}`.
    ///
    /// Additionally gated on the tail being non-empty at the oracle
    /// (`if(this.currentSessionRelocatedCwd&&e!=="")`), so a brand-new/missing
    /// transcript never gets a `relocated` record.
    pub relocated_cwd: Option<String>,
    /// `currentSessionLastPrompt` — the `lastPrompt` half of the `last-prompt`
    /// record. Omitted from the record when falsy.
    pub last_prompt: Option<String>,
    /// `currentSessionLeafUuid` — the `leafUuid` half of the `last-prompt`
    /// record. Its mere presence (even empty) makes the record eligible.
    pub leaf_uuid: Option<String>,
    /// `currentSessionAgentName` → `{type:"agent-name",agentName,sessionId}`.
    pub agent_name: Option<String>,
    /// `currentSessionAgentColor` → `{type:"agent-color",agentColor,sessionId}`.
    pub agent_color: Option<String>,
    /// `currentSessionAgentSetting` → `{type:"agent-setting",agentSetting,sessionId}`.
    ///
    /// The oracle carries a bare string here. `LingXi`'s
    /// [`crate::jsonl::writer::JsonlWriter::append_agent_setting_snapshot`]
    /// writes an ADDITIVE `agentSnapshot` sibling; such a record will never
    /// dedup-match this plan entry (different key set), so a re-append emits
    /// the plain Claude-compatible form alongside it. That is intentional —
    /// the snapshot is a `LingXi` extension and is re-emitted by its own writer.
    pub agent_setting: Option<String>,
    /// `currentSessionMode` → `{type:"mode",mode,sessionId}`.
    pub mode: Option<String>,
    /// `currentSessionPermissionMode` → `{type:"permission-mode",permissionMode,sessionId}`.
    pub permission_mode: Option<String>,
    /// LingXi mobile capability profile → `{type:"session-mode",sessionMode,sessionId}`.
    pub session_mode: Option<String>,
    /// `currentSessionIsolationLatch` → `{type:"isolation-latch",side,sessionId}`.
    /// Note the wire key is `side`, NOT `isolationLatch`.
    pub isolation_latch: Option<String>,
    /// `currentSessionAtisLatch` → `{type:"atis-latch",atis,sessionId}`.
    /// `Some("")` is meaningful: the oracle gates on `!== undefined`, and
    /// its `/^[\x21-\x7e]*$/` validator accepts the empty string.
    pub atis: Option<String>,
    /// `currentSessionWorktree` → `{type:"worktree-state",worktreeSession,sessionId}`.
    ///
    /// Gated on `!== undefined`, NOT on truthiness: an explicit
    /// `Some(Value::Null)` (the `ExitWorktree` clear record) IS emitted.
    pub worktree: Option<Value>,
    /// `currentSessionPrNumber` — gated on `!== undefined`; the record also
    /// requires truthy [`Self::pr_url`] and [`Self::pr_repository`].
    pub pr_number: Option<u64>,
    /// `currentSessionPrUrl`.
    pub pr_url: Option<String>,
    /// `currentSessionPrRepository`.
    pub pr_repository: Option<String>,
    /// `currentSessionBridgeId` → gates the whole `bridge-session` record.
    pub bridge_id: Option<String>,
    /// `currentSessionBridgeSeq` — serialized as `lastSequenceNum`, `?? 0`.
    pub bridge_seq: Option<u64>,
    /// `currentSessionBridgeDialogKinds` — emitted as `declaredDialogKinds`
    /// only when non-empty (`?.length &&`).
    pub bridge_dialog_kinds: Vec<String>,
    /// `currentSessionBridgeGroupingId` — emitted as `sessionGroupingId` when truthy.
    pub bridge_grouping_id: Option<String>,
}

/// The outcome of [`plan_re_append`]: the metadata records to append, in the
/// oracle's fixed order, already filtered by the dedup pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReAppendPlan {
    /// Records to append verbatim, one JSONL line each.
    pub entries: Vec<Value>,
}

impl ReAppendPlan {
    /// `true` when there is nothing to write — the oracle's async path returns
    /// early on `entries.length===0` without touching the file.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Serialize to the exact bytes the oracle appends: `jsonlJoin` (`eIl`,
    /// offset 225910055) = `JSON.stringify(entry) + "\n"` concatenated.
    #[must_use]
    pub fn to_jsonl(&self) -> String {
        let mut out = String::new();
        for entry in &self.entries {
            out.push_str(&serde_json::to_string(entry).unwrap_or_default());
            out.push('\n');
        }
        out
    }
}

/// Read the last `min(65536, size)` bytes of `path` — 1:1 with the oracle's
/// `jBy` (237892252) and its `fs/promises` twin `WBy` (237893153).
///
/// Returns `String::new()` on ANY error (missing file included) — the oracle
/// swallows every failure in a bare `catch{return""}`. The window is a raw BYTE
/// window and is NOT aligned to a line boundary, so the first element of
/// `split('\n')` is usually a partial line; every consumer here tolerates that
/// because it only ever uses last-match scans and per-line parses.
///
/// A window that starts mid-UTF-8-sequence yields U+FFFD for the truncated
/// prefix, matching Node's `Buffer#toString('utf8')`.
#[must_use]
pub fn read_tail(path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(meta) = file.metadata() else {
        return String::new();
    };
    let size = meta.len();
    let window = LITE_READ_BUF_SIZE as u64;
    let start = size.saturating_sub(window);
    let len = std::cmp::min(window, size - start);
    if len == 0 {
        return String::new();
    }
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    // `len <= LITE_READ_BUF_SIZE` by construction, so this never truncates.
    let Ok(len) = usize::try_from(len) else {
        return String::new();
    };
    let mut buf = vec![0u8; len];
    // `readSync` is a single read; a short read simply yields fewer bytes
    // (`o.toString("utf8",0,i)`), so we mirror that rather than read_exact.
    let Ok(filled) = file.read(&mut buf) else {
        return String::new();
    };
    buf.truncate(filled);
    String::from_utf8_lossy(&buf).into_owned()
}

/// JS truthiness for an optional string: `None` and `Some("")` are both falsy.
fn truthy(value: &Option<String>) -> Option<&str> {
    match value {
        Some(s) if !s.is_empty() => Some(s.as_str()),
        _ => None,
    }
}

/// `GOi` (226610241) — unescape a raw JSON string body.
///
/// `if(!e.includes("\\"))return e; try{return JSON.parse(`"${e}"`)}catch{return e}`
fn unescape_json_string(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    let mut quoted = String::with_capacity(raw.len() + 2);
    quoted.push('"');
    quoted.push_str(raw);
    quoted.push('"');
    serde_json::from_str::<String>(&quoted).unwrap_or_else(|_| raw.to_string())
}

/// `TC(e,t)` (226610554) — substring-extract the value of a quoted scalar field
/// from ONE line, preferring the occurrence at the LARGEST index.
///
/// Deliberately not a JSON parse: it scans for `"<key>":"` and `"<key>": "`,
/// walks to the first unescaped closing quote (skipping two bytes after each
/// backslash), and keeps whichever match started latest in the line. Returns
/// `None` when no match has a terminating quote.
#[must_use]
pub fn extract_quoted_field(line: &str, key: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let prefixes = [format!("\"{key}\":\""), format!("\"{key}\": \"")];
    let mut best: Option<String> = None;
    let mut best_at: Option<usize> = None;

    for prefix in &prefixes {
        let pat = prefix.as_bytes();
        let mut search = 0usize;
        loop {
            let Some(rel) = find_sub(&bytes[search.min(bytes.len())..], pat) else {
                break;
            };
            let at = search + rel;
            let start = at + pat.len();
            let mut cursor = start;
            while cursor < bytes.len() {
                if bytes[cursor] == b'\\' {
                    cursor += 2;
                    continue;
                }
                if bytes[cursor] == b'"' {
                    if best_at.is_none_or(|prev| at > prev) {
                        best = Some(unescape_json_string(&String::from_utf8_lossy(
                            &bytes[start..cursor.min(bytes.len())],
                        )));
                        best_at = Some(at);
                    }
                    break;
                }
                cursor += 1;
            }
            // `s=c+1` — resume past wherever the inner walk stopped, which is
            // how the oracle avoids rescanning the value body.
            search = cursor.saturating_add(1);
            if search >= bytes.len() {
                break;
            }
        }
    }
    best
}

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// `sRt(e,t,r)` (226610818) — scan `tail` backwards line by line for the most
/// recent line that both mentions `"type":"<record_type>"` and `"<field>":`,
/// `JSON.parse` it, and return `field` when it is a string.
///
/// The FIRST (partial) line of a tail window is examined too, exactly as the
/// oracle does — a parse failure there is swallowed and the scan continues.
#[must_use]
pub fn find_last_typed_field(tail: &str, record_type: &str, field: &str) -> Option<String> {
    let type_needle = format!("\"type\":\"{record_type}\"");
    let field_needle = format!("\"{field}\":");
    let bytes = tail.as_bytes();
    let mut end = bytes.len();
    while end > 0 {
        // `s = e.lastIndexOf("\n", i-1)`; the line is `e.slice(s+1, i)` and the
        // scan then continues with `i = s`, stopping once `s <= 0` — so a tail
        // that OPENS with a newline never yields an (empty) leading line, while
        // a tail with no newline at all still yields its single partial line.
        let newline = bytes[..end].iter().rposition(|&b| b == b'\n');
        let start = newline.map_or(0, |p| p + 1);
        let line = String::from_utf8_lossy(&bytes[start..end]);
        end = newline.unwrap_or(0);
        if line.contains(&type_needle) && line.contains(&field_needle) {
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&line) {
                if map.get("type").and_then(Value::as_str) == Some(record_type) {
                    if let Some(found) = map.get(field).and_then(Value::as_str) {
                        return Some(found.to_string());
                    }
                }
            }
        }
        if newline.is_none() {
            break;
        }
    }
    None
}

/// `normalizeLastPrompt` (237853077) — collapse newlines to spaces, trim, and
/// truncate to 200 UTF-16 code units with a trailing ellipsis.
///
/// `ma` (225924719) slices by UTF-16 code unit and drops a dangling high
/// surrogate so the truncation never splits an astral character.
#[must_use]
pub fn normalize_last_prompt(raw: &str) -> String {
    let collapsed = raw.replace('\n', " ");
    let trimmed = collapsed.trim();
    let units: Vec<u16> = trimmed.encode_utf16().collect();
    if units.len() <= LAST_PROMPT_MAX_UTF16 {
        return trimmed.to_string();
    }
    let mut head = &units[..LAST_PROMPT_MAX_UTF16];
    if let Some(&last) = head.last() {
        if (0xD800..=0xDBFF).contains(&last) {
            head = &head[..head.len() - 1];
        }
    }
    let sliced = String::from_utf16_lossy(head);
    format!("{}\u{2026}", sliced.trim())
}

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut map = Map::new();
    for (key, value) in pairs {
        map.insert(key.to_string(), value);
    }
    Value::Object(map)
}

/// `planReAppendSessionMetadata(e,t,r)` (237853135).
///
/// Three phases, in order:
/// 1. **Adopt back** from `tail` into `state` — titles only when
///    `skip_title_adopt` is `false`; `tag`, `relocated` and (when a
///    `leaf_uuid` is set) `last-prompt` always.
/// 2. **Rebuild** the 14-record metadata set from `state`, in the oracle's
///    fixed order.
/// 3. **Dedup** (unless `skip_dedup`) against the most recent on-disk record of
///    each planned type, found by a backwards scan bounded by BOTH "all types
///    found" and a 32 768-byte budget. The comparator is
///    `JSON.stringify(record_without_timestamp)`, so it is KEY-ORDER SENSITIVE.
///
/// Returns `None` when `session_id` is empty (the oracle's `if(!n)return null`
/// after `let n=kt()`).
// The 14-record push sequence is a verbatim transposition of one oracle
// function; splitting it would hide the record ORDER, which is load-bearing.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn plan_re_append(
    tail: &str,
    state: &mut SessionMetadataState,
    session_id: &str,
    skip_title_adopt: bool,
    skip_dedup: bool,
) -> Option<ReAppendPlan> {
    if session_id.is_empty() {
        return None;
    }
    let lines: Vec<&str> = tail.split('\n').collect();

    // ── 1. adopt back ───────────────────────────────────────────────────────
    if !skip_title_adopt {
        if let Some(line) = lines
            .iter()
            .rev()
            .find(|l| l.contains("\"type\":\"custom-title\"") && l.contains("\"customTitle\":\""))
        {
            if let Some(found) = extract_quoted_field(line, "customTitle") {
                // `this.currentSessionTitle = g || undefined` — an adopted
                // empty string CLEARS the in-memory title.
                state.title = if found.is_empty() { None } else { Some(found) };
            }
        }
        if let Some(line) = lines
            .iter()
            .rev()
            .find(|l| l.contains("\"type\":\"ai-title\"") && l.contains("\"aiTitle\":\""))
        {
            if let Some(found) = extract_quoted_field(line, "aiTitle") {
                state.ai_title = if found.is_empty() { None } else { Some(found) };
            }
        }
    }
    if let Some(line) = lines
        .iter()
        .rev()
        .find(|l| l.contains("\"type\":\"tag\"") && l.contains("\"tag\":\""))
    {
        if let Some(found) = extract_quoted_field(line, "tag") {
            state.tag = if found.is_empty() { None } else { Some(found) };
        }
    }
    if let Some(found) = find_last_typed_field(tail, "relocated", "relocatedCwd") {
        // `if(a)` — falsy empty string never adopts.
        if !found.is_empty() && state.relocated_cwd.is_none() {
            state.relocated_cwd = Some(found);
        }
    }
    if state.leaf_uuid.is_some() {
        if let Some(found) = find_last_typed_field(tail, "last-prompt", "lastPrompt") {
            if !found.is_empty() && state.last_prompt.is_none() {
                state.last_prompt = Some(normalize_last_prompt(&found));
            }
        }
    }
    if state.atis.is_none() {
        if let Some(found) = find_last_typed_field(tail, "atis-latch", "atis") {
            if is_valid_atis_latch(&found) {
                state.atis = Some(found);
            }
        }
    }

    // ── 2. rebuild ──────────────────────────────────────────────────────────
    let sid = || Value::String(session_id.to_string());
    let mut entries: Vec<Value> = Vec::new();

    if state.last_prompt.is_some() || state.leaf_uuid.is_some() {
        let mut pairs: Vec<(&str, Value)> = vec![("type", Value::String("last-prompt".into()))];
        if let Some(prompt) = truthy(&state.last_prompt) {
            pairs.push(("lastPrompt", Value::String(prompt.to_string())));
        }
        if let Some(leaf) = truthy(&state.leaf_uuid) {
            pairs.push(("leafUuid", Value::String(leaf.to_string())));
        }
        pairs.push(("sessionId", sid()));
        entries.push(obj(pairs));
    }
    if let Some(title) = truthy(&state.title) {
        entries.push(obj(vec![
            ("type", Value::String("custom-title".into())),
            ("customTitle", Value::String(title.to_string())),
            ("sessionId", sid()),
        ]));
    }
    if let Some(ai_title) = truthy(&state.ai_title) {
        entries.push(obj(vec![
            ("type", Value::String("ai-title".into())),
            ("aiTitle", Value::String(ai_title.to_string())),
            ("sessionId", sid()),
        ]));
    }
    if let Some(tag) = truthy(&state.tag) {
        entries.push(obj(vec![
            ("type", Value::String("tag".into())),
            ("tag", Value::String(tag.to_string())),
            ("sessionId", sid()),
        ]));
    }
    if let Some(relocated) = truthy(&state.relocated_cwd) {
        // `&&e!==""` — never emit `relocated` against an empty/absent tail.
        if !tail.is_empty() {
            entries.push(obj(vec![
                ("type", Value::String("relocated".into())),
                ("relocatedCwd", Value::String(relocated.to_string())),
                ("sessionId", sid()),
            ]));
        }
    }
    if let Some(agent_name) = truthy(&state.agent_name) {
        entries.push(obj(vec![
            ("type", Value::String("agent-name".into())),
            ("agentName", Value::String(agent_name.to_string())),
            ("sessionId", sid()),
        ]));
    }
    if let Some(agent_color) = truthy(&state.agent_color) {
        entries.push(obj(vec![
            ("type", Value::String("agent-color".into())),
            ("agentColor", Value::String(agent_color.to_string())),
            ("sessionId", sid()),
        ]));
    }
    if let Some(agent_setting) = truthy(&state.agent_setting) {
        entries.push(obj(vec![
            ("type", Value::String("agent-setting".into())),
            ("agentSetting", Value::String(agent_setting.to_string())),
            ("sessionId", sid()),
        ]));
    }
    if let Some(mode) = truthy(&state.mode) {
        entries.push(obj(vec![
            ("type", Value::String("mode".into())),
            ("mode", Value::String(mode.to_string())),
            ("sessionId", sid()),
        ]));
    }
    if let Some(permission_mode) = truthy(&state.permission_mode) {
        entries.push(obj(vec![
            ("type", Value::String("permission-mode".into())),
            ("permissionMode", Value::String(permission_mode.to_string())),
            ("sessionId", sid()),
        ]));
    }
    if let Some(session_mode) = truthy(&state.session_mode) {
        entries.push(obj(vec![
            ("type", Value::String("session-mode".into())),
            ("sessionMode", Value::String(session_mode.to_string())),
            ("sessionId", sid()),
        ]));
    }
    if let Some(latch) = truthy(&state.isolation_latch) {
        entries.push(obj(vec![
            ("type", Value::String("isolation-latch".into())),
            ("side", Value::String(latch.to_string())),
            ("sessionId", sid()),
        ]));
    }
    if let Some(atis) = &state.atis {
        entries.push(obj(vec![
            ("type", Value::String("atis-latch".into())),
            ("atis", Value::String(atis.clone())),
            ("sessionId", sid()),
        ]));
    }
    if let Some(worktree) = &state.worktree {
        // `!== undefined`, so an explicit null (ExitWorktree) still emits.
        entries.push(obj(vec![
            ("type", Value::String("worktree-state".into())),
            ("worktreeSession", worktree.clone()),
            ("sessionId", sid()),
        ]));
    }
    if let (Some(number), Some(url), Some(repo)) = (
        state.pr_number,
        truthy(&state.pr_url),
        truthy(&state.pr_repository),
    ) {
        entries.push(obj(vec![
            ("type", Value::String("pr-link".into())),
            ("sessionId", sid()),
            ("prNumber", Value::from(number)),
            ("prUrl", Value::String(url.to_string())),
            ("prRepository", Value::String(repo.to_string())),
            ("timestamp", Value::String(iso_now())),
        ]));
    }
    if let Some(bridge_id) = truthy(&state.bridge_id) {
        let mut pairs: Vec<(&str, Value)> = vec![
            ("type", Value::String("bridge-session".into())),
            ("sessionId", sid()),
            ("bridgeSessionId", Value::String(bridge_id.to_string())),
            (
                "lastSequenceNum",
                Value::from(state.bridge_seq.unwrap_or(0)),
            ),
        ];
        if !state.bridge_dialog_kinds.is_empty() {
            pairs.push((
                "declaredDialogKinds",
                Value::Array(
                    state
                        .bridge_dialog_kinds
                        .iter()
                        .map(|k| Value::String(k.clone()))
                        .collect(),
                ),
            ));
        }
        if let Some(grouping) = truthy(&state.bridge_grouping_id) {
            pairs.push(("sessionGroupingId", Value::String(grouping.to_string())));
        }
        entries.push(obj(pairs));
    }

    // ── 3. dedup ────────────────────────────────────────────────────────────
    if skip_dedup || entries.is_empty() {
        return Some(ReAppendPlan { entries });
    }

    // `new Set(l.map(f=>f.type))` — insertion-ordered, and each type is pushed
    // at most once, so this is just the plan order.
    let wanted: Vec<String> = entries
        .iter()
        .filter_map(|e| e.get("type").and_then(Value::as_str).map(str::to_string))
        .collect();
    let mut latest: Vec<(String, Value)> = Vec::new();
    let mut budget = 0usize;
    for line in lines.iter().rev() {
        if latest.len() == wanted.len() {
            break;
        }
        budget += line.len() + 1;
        if budget > METADATA_REAPPEND_BACKSTOP_BYTES {
            break;
        }
        if line.is_empty() {
            continue;
        }
        let Some(hit) = wanted.iter().find(|ty| {
            !latest.iter().any(|(seen, _)| seen == *ty)
                && line.contains(&format!("\"type\":\"{ty}\""))
        }) else {
            continue;
        };
        if let Ok(parsed) = serde_json::from_str::<Value>(line) {
            if parsed.get("type").and_then(Value::as_str) == Some(hit.as_str()) {
                latest.push((hit.clone(), parsed));
            }
        }
    }

    let kept = entries
        .into_iter()
        .filter(|entry| {
            let Some(ty) = entry.get("type").and_then(Value::as_str) else {
                return true;
            };
            match latest.iter().find(|(seen, _)| seen == ty) {
                None => true,
                Some((_, on_disk)) => dedup_key(entry) != dedup_key(on_disk),
            }
        })
        .collect();
    Some(ReAppendPlan { entries: kept })
}

/// The dedup comparator (cc-238.js @296784560):
///
/// ```js
/// let f=(m)=>{ if(m.type==="history-suppression") return Ie({type:m.type,sessionId:m.sessionId});
///              let{timestamp:h, ts:g, ...y}=m; return Ie(y)};
/// ```
///
/// KEY ORDER SENSITIVE by construction — this is a string compare of the
/// serialization, not a structural compare. A writer that emits the same fields
/// in a different order defeats dedup and re-appends forever.
///
/// Two changes from the 2.1.220 twin (`let p=(f)=>{let{timestamp:m,...g}=f;return Ie(g)}`),
/// both landed here:
///
/// * `ts` is stripped alongside `timestamp`. Latent in the port today — nothing
///   it writes carries a `ts` — but a record that gained one would otherwise
///   re-append on every turn forever, which is exactly the failure the strip
///   exists to prevent.
/// * `history-suppression` compares on `{type, sessionId}` ONLY, so a repeat
///   suppression record for the same session dedups no matter what else it
///   carries. The record type is new in 2.1.238 (0 hits in 2.1.220) and the
///   special case reproduces the oracle's key order verbatim: `type` first.
fn dedup_key(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut stripped = Map::new();
            if map.get("type").and_then(Value::as_str) == Some("history-suppression") {
                // `Ie({type:m.type,sessionId:m.sessionId})` — a fresh object,
                // so both keys are present (as `null` when absent) in the
                // oracle's order regardless of the source record's shape.
                stripped.insert(
                    "type".to_string(),
                    map.get("type").cloned().unwrap_or(Value::Null),
                );
                stripped.insert(
                    "sessionId".to_string(),
                    map.get("sessionId").cloned().unwrap_or(Value::Null),
                );
            } else {
                for (key, val) in map {
                    if key != "timestamp" && key != "ts" {
                        stripped.insert(key.clone(), val.clone());
                    }
                }
            }
            serde_json::to_string(&Value::Object(stripped)).unwrap_or_default()
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// `new Date().toISOString()` — millisecond precision, `Z` suffix.
pub(crate) fn iso_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format_iso_millis(now.as_millis())
}

/// `Date.prototype.toISOString()` — millisecond precision, `Z` suffix.
///
/// Public because the `--resume <title>` disambiguation listing renders
/// `(modified ${p.modified.toISOString()})` and must produce the same shape;
/// pinned by `iso_timestamp_matches_javascript_to_iso_string`.
#[must_use]
pub fn format_iso_millis(millis: u128) -> String {
    let total_secs = i64::try_from(millis / 1000).unwrap_or(i64::MAX);
    let ms = u32::try_from(millis % 1000).unwrap_or(0);
    let days = total_secs.div_euclid(86_400);
    let secs_of_day = total_secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    )
}

/// Howard Hinnant's `civil_from_days` — days since the Unix epoch to y/m/d.
///
/// `day_of_era` / `day_of_year` are the algorithm's canonical `doe`/`doy`;
/// keeping the short names would trip `clippy::similar_names`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * month_prime + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    })
    .unwrap_or(1);
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_title(title: &str) -> SessionMetadataState {
        SessionMetadataState {
            title: Some(title.to_string()),
            ..Default::default()
        }
    }

    // ── record shape / order ────────────────────────────────────────────────

    /// The rebuild emits every record type in the exact push order of
    /// `planReAppendSessionMetadata`, with the exact key order per record.
    /// Byte-locked: this is what the dedup comparator hashes on, so any drift
    /// here silently turns every re-append into an unconditional append.
    #[test]
    fn rebuild_emits_all_record_types_in_oracle_order_and_key_order() {
        let mut state = SessionMetadataState {
            title: Some("My Title".into()),
            ai_title: Some("AI Title".into()),
            tag: Some("blue".into()),
            relocated_cwd: Some("/new/cwd".into()),
            last_prompt: Some("hello".into()),
            leaf_uuid: Some("leaf-1".into()),
            agent_name: Some("reviewer".into()),
            agent_color: Some("red".into()),
            agent_setting: Some("reviewer".into()),
            mode: Some("default".into()),
            permission_mode: Some("acceptEdits".into()),
            session_mode: Some("code".into()),
            isolation_latch: Some("left".into()),
            atis: Some("atis-token".into()),
            worktree: Some(serde_json::json!({"worktreePath": "/wt"})),
            pr_number: Some(42),
            pr_url: Some("https://example.test/pull/42".into()),
            pr_repository: Some("acme/widgets".into()),
            bridge_id: Some("bridge-9".into()),
            bridge_seq: Some(7),
            bridge_dialog_kinds: vec!["ask".into()],
            bridge_grouping_id: Some("grp-1".into()),
        };
        // Non-empty tail (so `relocated` is eligible) that matches nothing.
        let plan = plan_re_append("{\"type\":\"user\"}\n", &mut state, "S1", true, true)
            .expect("session id present");

        let types: Vec<&str> = plan
            .entries
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            vec![
                "last-prompt",
                "custom-title",
                "ai-title",
                "tag",
                "relocated",
                "agent-name",
                "agent-color",
                "agent-setting",
                "mode",
                "permission-mode",
                "session-mode",
                "isolation-latch",
                "atis-latch",
                "worktree-state",
                "pr-link",
                "bridge-session",
            ]
        );

        let lines: Vec<String> = plan
            .entries
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect();
        assert_eq!(
            lines[0],
            r#"{"type":"last-prompt","lastPrompt":"hello","leafUuid":"leaf-1","sessionId":"S1"}"#
        );
        assert_eq!(
            lines[1],
            r#"{"type":"custom-title","customTitle":"My Title","sessionId":"S1"}"#
        );
        assert_eq!(
            lines[2],
            r#"{"type":"ai-title","aiTitle":"AI Title","sessionId":"S1"}"#
        );
        assert_eq!(lines[3], r#"{"type":"tag","tag":"blue","sessionId":"S1"}"#);
        assert_eq!(
            lines[4],
            r#"{"type":"relocated","relocatedCwd":"/new/cwd","sessionId":"S1"}"#
        );
        assert_eq!(
            lines[5],
            r#"{"type":"agent-name","agentName":"reviewer","sessionId":"S1"}"#
        );
        assert_eq!(
            lines[6],
            r#"{"type":"agent-color","agentColor":"red","sessionId":"S1"}"#
        );
        assert_eq!(
            lines[7],
            r#"{"type":"agent-setting","agentSetting":"reviewer","sessionId":"S1"}"#
        );
        assert_eq!(
            lines[8],
            r#"{"type":"mode","mode":"default","sessionId":"S1"}"#
        );
        assert_eq!(
            lines[9],
            r#"{"type":"permission-mode","permissionMode":"acceptEdits","sessionId":"S1"}"#
        );
        assert_eq!(
            lines[10],
            r#"{"type":"session-mode","sessionMode":"code","sessionId":"S1"}"#
        );
        // Wire key is `side`, NOT `isolationLatch`.
        assert_eq!(
            lines[11],
            r#"{"type":"isolation-latch","side":"left","sessionId":"S1"}"#
        );
        assert_eq!(
            lines[12],
            r#"{"type":"atis-latch","atis":"atis-token","sessionId":"S1"}"#
        );
        assert_eq!(
            lines[13],
            r#"{"type":"worktree-state","worktreeSession":{"worktreePath":"/wt"},"sessionId":"S1"}"#
        );
        // pr-link and bridge-session put `sessionId` SECOND, unlike every other record.
        assert!(lines[14].starts_with(
            r#"{"type":"pr-link","sessionId":"S1","prNumber":42,"prUrl":"https://example.test/pull/42","prRepository":"acme/widgets","timestamp":""#
        ));
        assert_eq!(
            lines[15],
            r#"{"type":"bridge-session","sessionId":"S1","bridgeSessionId":"bridge-9","lastSequenceNum":7,"declaredDialogKinds":["ask"],"sessionGroupingId":"grp-1"}"#
        );
    }

    /// Empty in-memory state produces no records at all.
    #[test]
    fn empty_state_plans_nothing() {
        let mut state = SessionMetadataState::default();
        let plan = plan_re_append("", &mut state, "S1", true, true).expect("plan");
        assert!(plan.is_empty());
        assert_eq!(plan.to_jsonl(), "");
    }

    /// `if(!n)return null` — no session id, no plan.
    #[test]
    fn missing_session_id_returns_none() {
        let mut state = state_with_title("x");
        assert!(plan_re_append("", &mut state, "", true, true).is_none());
    }

    /// `worktree-state` is gated on `!== undefined`, so an explicit null (the
    /// `ExitWorktree` clear record) still emits, while `None` emits nothing.
    #[test]
    fn worktree_state_emits_explicit_null_but_not_absent() {
        let mut absent = SessionMetadataState::default();
        assert!(plan_re_append("", &mut absent, "S1", true, true)
            .unwrap()
            .is_empty());

        let mut cleared = SessionMetadataState {
            worktree: Some(Value::Null),
            ..Default::default()
        };
        let plan = plan_re_append("", &mut cleared, "S1", true, true).unwrap();
        assert_eq!(
            plan.to_jsonl(),
            "{\"type\":\"worktree-state\",\"worktreeSession\":null,\"sessionId\":\"S1\"}\n"
        );
    }

    /// `relocated` additionally requires a NON-EMPTY tail (`&&e!==""`).
    #[test]
    fn relocated_is_suppressed_against_an_empty_tail() {
        let mut state = SessionMetadataState {
            relocated_cwd: Some("/moved".into()),
            ..Default::default()
        };
        assert!(plan_re_append("", &mut state, "S1", true, true)
            .unwrap()
            .is_empty());

        let mut state2 = SessionMetadataState {
            relocated_cwd: Some("/moved".into()),
            ..Default::default()
        };
        assert_eq!(
            plan_re_append("x\n", &mut state2, "S1", true, true)
                .unwrap()
                .entries
                .len(),
            1
        );
    }

    /// JS truthiness: an EMPTY string is falsy and suppresses its record.
    #[test]
    fn empty_string_fields_are_falsy() {
        let mut state = SessionMetadataState {
            title: Some(String::new()),
            mode: Some(String::new()),
            ..Default::default()
        };
        assert!(plan_re_append("x\n", &mut state, "S1", true, true)
            .unwrap()
            .is_empty());
    }

    /// `last-prompt` is eligible on EITHER half, and each half is omitted from
    /// the record when falsy.
    #[test]
    fn last_prompt_record_omits_falsy_halves() {
        let mut leaf_only = SessionMetadataState {
            leaf_uuid: Some("leaf-1".into()),
            ..Default::default()
        };
        assert_eq!(
            plan_re_append("", &mut leaf_only, "S1", true, true)
                .unwrap()
                .to_jsonl(),
            "{\"type\":\"last-prompt\",\"leafUuid\":\"leaf-1\",\"sessionId\":\"S1\"}\n"
        );

        let mut prompt_only = SessionMetadataState {
            last_prompt: Some("hi".into()),
            ..Default::default()
        };
        assert_eq!(
            plan_re_append("", &mut prompt_only, "S1", true, true)
                .unwrap()
                .to_jsonl(),
            "{\"type\":\"last-prompt\",\"lastPrompt\":\"hi\",\"sessionId\":\"S1\"}\n"
        );
    }

    /// `lastSequenceNum` defaults to 0 (`?? 0`) and the two optional bridge
    /// keys are omitted when empty.
    #[test]
    fn bridge_session_defaults_sequence_and_omits_empty_optionals() {
        let mut state = SessionMetadataState {
            bridge_id: Some("b1".into()),
            ..Default::default()
        };
        assert_eq!(
            plan_re_append("", &mut state, "S1", true, true)
                .unwrap()
                .to_jsonl(),
            "{\"type\":\"bridge-session\",\"sessionId\":\"S1\",\"bridgeSessionId\":\"b1\",\"lastSequenceNum\":0}\n"
        );
    }

    // ── adopt-back ──────────────────────────────────────────────────────────

    /// With `skip_title_adopt = false` the titles are re-read OUT of the tail
    /// and overwrite in-memory state — this is a read-modify-write, not a pure
    /// append.
    #[test]
    fn adopt_back_overwrites_titles_from_the_tail() {
        let tail = concat!(
            r#"{"type":"custom-title","customTitle":"from disk","sessionId":"S1"}"#,
            "\n",
            r#"{"type":"ai-title","aiTitle":"ai from disk","sessionId":"S1"}"#,
            "\n",
            // Trailing null records must be SKIPPED by `findLast`, not selected
            // (the matcher requires the `"<key>":"` prefix too).
            r#"{"type":"ai-title","aiTitle":null,"sessionId":"S1"}"#,
            "\n",
        );
        let mut state = state_with_title("in memory");
        plan_re_append(tail, &mut state, "S1", false, true).unwrap();
        assert_eq!(state.title.as_deref(), Some("from disk"));
        assert_eq!(state.ai_title.as_deref(), Some("ai from disk"));
    }

    /// With `skip_title_adopt = true` (the RESUME polarity) the in-memory
    /// titles survive untouched. Getting this flag backwards clobbers a
    /// resumed session's title with a stale tail value.
    #[test]
    fn skip_title_adopt_leaves_in_memory_titles_alone() {
        let tail = concat!(
            r#"{"type":"custom-title","customTitle":"from disk","sessionId":"S1"}"#,
            "\n",
        );
        let mut state = state_with_title("in memory");
        plan_re_append(tail, &mut state, "S1", true, true).unwrap();
        assert_eq!(state.title.as_deref(), Some("in memory"));
    }

    /// `tag` adopt-back is NOT gated by `skip_title_adopt` — it always runs.
    #[test]
    fn tag_adopt_back_runs_even_when_titles_are_skipped() {
        let tail = concat!(r#"{"type":"tag","tag":"green","sessionId":"S1"}"#, "\n");
        let mut state = SessionMetadataState::default();
        plan_re_append(tail, &mut state, "S1", true, true).unwrap();
        assert_eq!(state.tag.as_deref(), Some("green"));
    }

    /// `relocated` / `last-prompt` adopt with `??=`: they fill an ABSENT value
    /// only, never overwrite one already in memory.
    #[test]
    fn relocated_and_last_prompt_adopt_only_when_absent() {
        let tail = concat!(
            r#"{"type":"relocated","relocatedCwd":"/from/disk","sessionId":"S1"}"#,
            "\n",
            r#"{"type":"last-prompt","lastPrompt":"disk prompt","leafUuid":"L","sessionId":"S1"}"#,
            "\n",
        );

        let mut absent = SessionMetadataState {
            leaf_uuid: Some("L".into()),
            ..Default::default()
        };
        plan_re_append(tail, &mut absent, "S1", true, true).unwrap();
        assert_eq!(absent.relocated_cwd.as_deref(), Some("/from/disk"));
        assert_eq!(absent.last_prompt.as_deref(), Some("disk prompt"));

        let mut present = SessionMetadataState {
            leaf_uuid: Some("L".into()),
            relocated_cwd: Some("/in/memory".into()),
            last_prompt: Some("memory prompt".into()),
            ..Default::default()
        };
        plan_re_append(tail, &mut present, "S1", true, true).unwrap();
        assert_eq!(present.relocated_cwd.as_deref(), Some("/in/memory"));
        assert_eq!(present.last_prompt.as_deref(), Some("memory prompt"));
    }

    /// The `last-prompt` adopt is gated on a leaf uuid being set at all
    /// (`if(this.currentSessionLeafUuid!==void 0)`).
    #[test]
    fn last_prompt_adopt_requires_a_leaf_uuid() {
        let tail = concat!(
            r#"{"type":"last-prompt","lastPrompt":"disk prompt","sessionId":"S1"}"#,
            "\n",
        );
        let mut state = SessionMetadataState::default();
        plan_re_append(tail, &mut state, "S1", true, true).unwrap();
        assert_eq!(state.last_prompt, None);
    }

    /// An adopted EMPTY title clears in-memory state (`g || undefined`).
    #[test]
    fn adopted_empty_title_clears_state() {
        let tail = concat!(
            r#"{"type":"custom-title","customTitle":"","sessionId":"S1"}"#,
            "\n",
        );
        let mut state = state_with_title("in memory");
        plan_re_append(tail, &mut state, "S1", false, true).unwrap();
        assert_eq!(state.title, None);
    }

    /// The adopt-back matcher requires BOTH `"type":"custom-title"` AND
    /// `"customTitle":"` on the line. A `customTitle: null` record therefore
    /// does not merely fail to yield a value — it is SKIPPED, so `findLast`
    /// keeps walking back to the newest record that really carries a string.
    #[test]
    fn adopt_back_skips_a_non_string_title_record_and_keeps_walking_back() {
        let tail = concat!(
            r#"{"type":"custom-title","customTitle":"real title","sessionId":"S1"}"#,
            "\n",
            r#"{"type":"custom-title","customTitle":null,"sessionId":"S1"}"#,
            "\n",
        );
        let mut state = state_with_title("in memory");
        plan_re_append(tail, &mut state, "S1", false, true).unwrap();
        assert_eq!(
            state.title.as_deref(),
            Some("real title"),
            "the null record must be skipped, not merely un-extractable"
        );

        // With ONLY a null record the in-memory title is left alone.
        let only_null = concat!(
            r#"{"type":"custom-title","customTitle":null,"sessionId":"S1"}"#,
            "\n",
        );
        let mut state2 = state_with_title("in memory");
        plan_re_append(only_null, &mut state2, "S1", false, true).unwrap();
        assert_eq!(state2.title.as_deref(), Some("in memory"));
    }

    // ── dedup ───────────────────────────────────────────────────────────────

    /// A record byte-identical to the newest on-disk one of its type is
    /// dropped; a changed one survives.
    #[test]
    fn dedup_drops_only_unchanged_records() {
        let tail = concat!(
            r#"{"type":"custom-title","customTitle":"same","sessionId":"S1"}"#,
            "\n",
            r#"{"type":"mode","mode":"old","sessionId":"S1"}"#,
            "\n",
        );
        let mut state = SessionMetadataState {
            title: Some("same".into()),
            mode: Some("new".into()),
            ..Default::default()
        };
        let plan = plan_re_append(tail, &mut state, "S1", true, false).unwrap();
        assert_eq!(
            plan.to_jsonl(),
            "{\"type\":\"mode\",\"mode\":\"new\",\"sessionId\":\"S1\"}\n"
        );
    }

    /// `skip_dedup = true` forces the write even when the tail already has it.
    #[test]
    fn skip_dedup_forces_the_write() {
        let tail = concat!(
            r#"{"type":"custom-title","customTitle":"same","sessionId":"S1"}"#,
            "\n",
        );
        let mut state = state_with_title("same");
        assert_eq!(
            plan_re_append(tail, &mut state, "S1", true, false)
                .unwrap()
                .entries
                .len(),
            0
        );
        let mut state2 = state_with_title("same");
        assert_eq!(
            plan_re_append(tail, &mut state2, "S1", true, true)
                .unwrap()
                .entries
                .len(),
            1
        );
    }

    /// Only the NEWEST on-disk record of each type is compared — an older
    /// matching record further up must not mask a newer differing one.
    #[test]
    fn dedup_compares_against_the_newest_record_only() {
        let tail = concat!(
            r#"{"type":"mode","mode":"target","sessionId":"S1"}"#,
            "\n",
            r#"{"type":"mode","mode":"newer","sessionId":"S1"}"#,
            "\n",
        );
        let mut state = SessionMetadataState {
            mode: Some("target".into()),
            ..Default::default()
        };
        let plan = plan_re_append(tail, &mut state, "S1", true, false).unwrap();
        assert_eq!(plan.entries.len(), 1, "newest on-disk record is `newer`");
    }

    /// The comparator strips `timestamp` from both sides, so a `pr-link` whose
    /// only difference is its timestamp still dedups.
    #[test]
    fn dedup_ignores_the_timestamp_field() {
        let tail = concat!(
            r#"{"type":"pr-link","sessionId":"S1","prNumber":42,"prUrl":"u","prRepository":"r","timestamp":"1999-01-01T00:00:00.000Z"}"#,
            "\n",
        );
        let mut state = SessionMetadataState {
            pr_number: Some(42),
            pr_url: Some("u".into()),
            pr_repository: Some("r".into()),
            ..Default::default()
        };
        let plan = plan_re_append(tail, &mut state, "S1", true, false).unwrap();
        assert!(
            plan.is_empty(),
            "timestamp-only differences must not defeat dedup"
        );
    }

    /// SC-12. 2.1.238 destructures `{timestamp, ts, ...rest}`, not just
    /// `{timestamp, ...rest}` — a record whose only difference is a `ts` field
    /// dedups too. Asserted directly on the comparator because nothing the port
    /// writes carries a `ts` yet, which is precisely why the strip would
    /// otherwise rot unnoticed until the first writer that adds one bloats the
    /// transcript on every turn.
    #[test]
    fn dedup_key_strips_ts_alongside_timestamp() {
        let with_ts = serde_json::json!({
            "type": "mode", "mode": "default", "sessionId": "S1",
            "ts": 1_700_000_000_u64, "timestamp": "1999-01-01T00:00:00.000Z",
        });
        let bare = serde_json::json!({
            "type": "mode", "mode": "default", "sessionId": "S1",
        });
        assert_eq!(dedup_key(&with_ts), dedup_key(&bare));
        assert_eq!(
            dedup_key(&bare),
            r#"{"type":"mode","mode":"default","sessionId":"S1"}"#
        );

        // A real field difference still defeats dedup.
        let changed = serde_json::json!({
            "type": "mode", "mode": "plan", "sessionId": "S1", "ts": 1_700_000_000_u64,
        });
        assert_ne!(dedup_key(&with_ts), dedup_key(&changed));
    }

    /// SC-12. `history-suppression` is compared on `{type, sessionId}` ONLY, in
    /// that key order — every other field is ignored, so a repeat suppression
    /// record for the same session always dedups.
    #[test]
    fn dedup_key_special_cases_history_suppression() {
        let planned = serde_json::json!({
            "type": "history-suppression", "sessionId": "S1",
        });
        let on_disk = serde_json::json!({
            "sessionId": "S1", "suppressedUuids": ["a", "b"],
            "type": "history-suppression", "timestamp": "1999-01-01T00:00:00.000Z",
        });
        assert_eq!(
            dedup_key(&planned),
            dedup_key(&on_disk),
            "payload and key order are both ignored for this type"
        );
        assert_eq!(
            dedup_key(&planned),
            r#"{"type":"history-suppression","sessionId":"S1"}"#
        );

        // A different session is a different record.
        let other_session = serde_json::json!({
            "type": "history-suppression", "sessionId": "S2",
        });
        assert_ne!(dedup_key(&planned), dedup_key(&other_session));

        // The special case is keyed on the type — any other type still uses the
        // full-record comparator.
        let mode = serde_json::json!({"type": "mode", "sessionId": "S1", "mode": "plan"});
        let mode2 = serde_json::json!({"type": "mode", "sessionId": "S1", "mode": "default"});
        assert_ne!(dedup_key(&mode), dedup_key(&mode2));
    }

    #[test]
    fn re_append_adopts_and_writes_valid_atis_latch_in_oracle_order() {
        let mut state = SessionMetadataState {
            isolation_latch: Some("left".into()),
            worktree: Some(serde_json::json!({"name": "wt"})),
            ..Default::default()
        };
        let plan = plan_re_append(
            concat!(
                r#"{"type":"atis-latch","sessionId":"S1","atis":"valid-token"}"#,
                "\n",
            ),
            &mut state,
            "S1",
            true,
            true,
        )
        .expect("plan");

        assert_eq!(state.atis.as_deref(), Some("valid-token"));
        assert_eq!(
            plan.entries
                .iter()
                .map(|entry| entry["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["isolation-latch", "atis-latch", "worktree-state"],
        );
        assert_eq!(plan.entries[1]["atis"], "valid-token");

        let mut invalid = SessionMetadataState::default();
        let invalid_plan = plan_re_append(
            "{\"type\":\"atis-latch\",\"sessionId\":\"S1\",\"atis\":\"bad\\u0020token\"}\n",
            &mut invalid,
            "S1",
            true,
            true,
        )
        .expect("plan");
        assert!(invalid.atis.is_none());
        assert!(invalid_plan.entries.is_empty());
    }

    /// The comparator is a STRING compare of the serialization, so a record
    /// carrying the same fields in a DIFFERENT key order does not dedup.
    /// This is the silent-transcript-bloat trap: any writer whose key order
    /// drifts from the plan re-appends forever.
    #[test]
    fn dedup_is_key_order_sensitive() {
        let tail = concat!(
            r#"{"customTitle":"same","type":"custom-title","sessionId":"S1"}"#,
            "\n",
        );
        let mut state = state_with_title("same");
        let plan = plan_re_append(tail, &mut state, "S1", true, false).unwrap();
        assert_eq!(plan.entries.len(), 1);
    }

    /// The backwards dedup scan is bounded by 32 768 bytes: a matching record
    /// buried under more than that much newer data is NOT found, so the plan
    /// entry survives.
    #[test]
    fn dedup_scan_stops_after_32768_bytes() {
        let target = r#"{"type":"custom-title","customTitle":"same","sessionId":"S1"}"#;
        let filler_line = "x".repeat(1023); // 1024 bytes with the newline
        let mut tail = String::from(target);
        tail.push('\n');
        for _ in 0..40 {
            tail.push_str(&filler_line);
            tail.push('\n');
        }
        assert!(tail.len() > METADATA_REAPPEND_BACKSTOP_BYTES);

        let mut state = state_with_title("same");
        let plan = plan_re_append(&tail, &mut state, "S1", true, false).unwrap();
        assert_eq!(
            plan.entries.len(),
            1,
            "record beyond the 32 KiB budget must not be seen"
        );

        // Same record just inside the budget IS seen and dedups.
        let mut near = String::new();
        for _ in 0..20 {
            near.push_str(&filler_line);
            near.push('\n');
        }
        let mut tail2 = String::from(target);
        tail2.push('\n');
        tail2.push_str(&near);
        let mut state2 = state_with_title("same");
        let plan2 = plan_re_append(&tail2, &mut state2, "S1", true, false).unwrap();
        assert!(plan2.is_empty(), "record inside the budget must dedup");
    }

    /// A malformed line that merely MENTIONS a wanted type consumes that type's
    /// slot for the rest of the scan only if it parses — a parse failure is
    /// swallowed and the scan continues past it.
    #[test]
    fn dedup_scan_survives_a_malformed_line() {
        let tail = concat!(
            r#"{"type":"custom-title","customTitle":"same","sessionId":"S1"}"#,
            "\n",
            r#"{"type":"custom-title","customTitle":"trunc"#,
            "\n",
        );
        let mut state = state_with_title("same");
        let plan = plan_re_append(tail, &mut state, "S1", true, false).unwrap();
        assert!(plan.is_empty(), "malformed line must not stop the scan");
    }

    // ── helpers ─────────────────────────────────────────────────────────────

    /// `TC` prefers the LAST occurrence in the line and accepts the
    /// space-after-colon spelling.
    #[test]
    fn extract_quoted_field_prefers_the_last_occurrence() {
        assert_eq!(
            extract_quoted_field(r#"{"tag":"first","tag":"second"}"#, "tag").as_deref(),
            Some("second")
        );
        assert_eq!(
            extract_quoted_field(r#"{"tag": "spaced"}"#, "tag").as_deref(),
            Some("spaced")
        );
        assert_eq!(extract_quoted_field(r#"{"other":"x"}"#, "tag"), None);
    }

    /// `TC` walks past escaped quotes and unescapes the captured body.
    #[test]
    fn extract_quoted_field_handles_escapes() {
        assert_eq!(
            extract_quoted_field(r#"{"customTitle":"a \"quoted\" \\ title"}"#, "customTitle")
                .as_deref(),
            Some(r#"a "quoted" \ title"#)
        );
        assert_eq!(
            extract_quoted_field(r#"{"customTitle":"line\nbreak"}"#, "customTitle").as_deref(),
            Some("line\nbreak")
        );
    }

    /// `sRt` scans backwards, requires the type to match after parsing, and
    /// tolerates the leading partial line of a tail window.
    #[test]
    fn find_last_typed_field_scans_backwards_over_a_partial_first_line() {
        let tail = concat!(
            r#"ncated":"relocated","relocatedCwd":"/junk"}"#,
            "\n",
            r#"{"type":"relocated","relocatedCwd":"/first","sessionId":"S1"}"#,
            "\n",
            r#"{"type":"relocated","relocatedCwd":"/second","sessionId":"S1"}"#,
            "\n",
        );
        assert_eq!(
            find_last_typed_field(tail, "relocated", "relocatedCwd").as_deref(),
            Some("/second")
        );
        assert_eq!(find_last_typed_field("", "relocated", "relocatedCwd"), None);

        // The LEADING partial line is examined too — a tail window whose only
        // matching record is the (here still parseable) first line must hit.
        let only_first_line = concat!(
            r#"{"type":"relocated","relocatedCwd":"/only","sessionId":"S1"}"#,
            "\n",
            r#"{"type":"user"}"#,
            "\n",
        );
        assert_eq!(
            find_last_typed_field(only_first_line, "relocated", "relocatedCwd").as_deref(),
            Some("/only")
        );
        // …and with no trailing newline at all (a single unterminated line).
        assert_eq!(
            find_last_typed_field(
                r#"{"type":"relocated","relocatedCwd":"/bare","sessionId":"S1"}"#,
                "relocated",
                "relocatedCwd"
            )
            .as_deref(),
            Some("/bare")
        );
        // Non-string values are rejected.
        assert_eq!(
            find_last_typed_field(
                r#"{"type":"relocated","relocatedCwd":7}"#,
                "relocated",
                "relocatedCwd"
            ),
            None
        );
    }

    /// `normalizeLastPrompt` collapses newlines, trims, and truncates at 200
    /// UTF-16 code units with a trailing ellipsis.
    #[test]
    fn normalize_last_prompt_collapses_and_truncates() {
        assert_eq!(normalize_last_prompt("  a\nb  "), "a b");
        let long = "x".repeat(250);
        let out = normalize_last_prompt(&long);
        assert_eq!(out.chars().count(), 201);
        assert!(out.ends_with('\u{2026}'));
        assert_eq!(out.trim_end_matches('\u{2026}').len(), 200);
    }

    /// Truncation must not split an astral character: 200 code units landing
    /// mid-surrogate-pair drops the dangling high surrogate.
    #[test]
    fn normalize_last_prompt_does_not_split_a_surrogate_pair() {
        // 199 ASCII + an astral char = 201 UTF-16 units; the cut at 200 would
        // land on the high surrogate.
        let mut raw = "a".repeat(199);
        raw.push('\u{1F600}');
        let out = normalize_last_prompt(&raw);
        assert_eq!(out, format!("{}\u{2026}", "a".repeat(199)));
        assert!(!out.contains('\u{FFFD}'), "no replacement char may appear");
    }

    // ── tail reader ─────────────────────────────────────────────────────────

    /// `read_tail` returns the LAST 64 KiB and `""` for a missing file.
    #[test]
    fn read_tail_returns_the_last_window_and_empty_on_error() {
        let dir = std::env::temp_dir().join(format!(
            "lingxi-reappend-tail-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");

        assert_eq!(read_tail(&path), "", "missing file yields empty");

        let body = format!("{}TAIL", "y".repeat(LITE_READ_BUF_SIZE + 100));
        std::fs::write(&path, &body).unwrap();
        let tail = read_tail(&path);
        assert_eq!(tail.len(), LITE_READ_BUF_SIZE);
        assert!(tail.ends_with("TAIL"));

        std::fs::write(&path, "short\n").unwrap();
        assert_eq!(read_tail(&path), "short\n");

        std::fs::write(&path, "").unwrap();
        assert_eq!(read_tail(&path), "");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The 64 KiB window is a raw BYTE window, so a multi-byte character split
    /// at its head becomes U+FFFD — matching Node's `Buffer#toString('utf8')`.
    #[test]
    fn read_tail_replaces_a_split_leading_code_point() {
        let dir = std::env::temp_dir().join(format!(
            "lingxi-reappend-split-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");

        // Lay out a 3-byte char so the 64 KiB window opens on its SECOND byte:
        // 3 leading filler bytes, the char at 3..6, then enough filler that
        // `size - 65536 == 4`.
        let mut body = vec![b'z'; 3];
        body.extend_from_slice("中".as_bytes());
        body.extend(std::iter::repeat(b'z').take(LITE_READ_BUF_SIZE - 2));
        assert_eq!(body.len(), LITE_READ_BUF_SIZE + 4);
        std::fs::write(&path, &body).unwrap();
        let tail = read_tail(&path);
        // The two orphaned continuation bytes each decode to one U+FFFD —
        // WHATWG "maximal subpart" behaviour, identical in Node and Rust.
        assert!(
            tail.starts_with("\u{FFFD}\u{FFFD}z"),
            "split code point must become U+FFFD, got {:?}",
            tail.chars().take(4).collect::<String>()
        );
        assert!(
            tail.ends_with("zzzz") && !tail[6..].contains('\u{FFFD}'),
            "only the split head is replaced"
        );

        // A window that opens exactly ON a char boundary keeps it intact.
        let mut aligned = vec![b'z'; 2];
        aligned.extend_from_slice("中".as_bytes());
        aligned.extend(std::iter::repeat(b'z').take(LITE_READ_BUF_SIZE - 3));
        assert_eq!(aligned.len(), LITE_READ_BUF_SIZE + 2);
        std::fs::write(&path, &aligned).unwrap();
        let tail = read_tail(&path);
        assert!(tail.starts_with("中"), "aligned char survives");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── constants / timestamp ───────────────────────────────────────────────

    #[test]
    fn backstop_is_half_the_tail_window() {
        assert_eq!(METADATA_REAPPEND_BACKSTOP_BYTES, 32_768);
        assert_eq!(LITE_READ_BUF_SIZE, 65_536);
    }

    /// The `pr-link` timestamp is `new Date().toISOString()` — millisecond
    /// precision with a `Z` suffix.
    #[test]
    fn iso_timestamp_matches_javascript_to_iso_string() {
        assert_eq!(format_iso_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            format_iso_millis(1_753_920_000_123),
            "2025-07-31T00:00:00.123Z"
        );
        // Leap day.
        assert_eq!(
            format_iso_millis(1_709_164_800_000),
            "2024-02-29T00:00:00.000Z"
        );
        let now = iso_now();
        assert_eq!(now.len(), 24, "{now}");
        assert!(now.ends_with('Z'));
    }
}
