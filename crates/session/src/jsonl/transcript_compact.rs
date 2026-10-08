//! Transcript-file compaction — 1:1 port of `Isp.performCompactTranscript`
//! (cc-238.js @296788068) and its plan builder / applier `N6m` (@296769617),
//! `F6m` (@296771626), `$6m` (@296771932).
//!
//! # Why this exists (SC-08)
//!
//! [`crate::jsonl::re_append`] is the GROWTH side of the transcript: every time
//! the 32 KiB backstop fires it appends a fresh copy of the whole metadata set,
//! so a long session accumulates superseded `custom-title` / `mode` /
//! `worktree-state` / … sidecar records without bound. Upstream pairs that
//! backstop with this module — the RECLAMATION side — which rewrites
//! `<sid>.jsonl` in place, dropping every record the persistence tables mark as
//! superseded, then re-appends the metadata set with `skip_dedup = true`.
//!
//! Shipping the growth half without the reclamation half is what made this
//! finding matter more than its P2 label: the port manufactured the garbage and
//! had no way to remove it.
//!
//! # Oracle provenance
//!
//! | Symbol | Offset | Role |
//! |---|---|---|
//! | `performCompactTranscript(e,t,r,n)` | 296788068 | the rewrite driver + safety envelope |
//! | `N6m(e)` | 296769617 | plan builder (one streaming pass) |
//! | `F6m(e)` | 296771626 | plan applier (`(idx, line) -> lines[]`) |
//! | `$6m(e)` | 296771932 | leading-NUL stripper applied to every written line |
//! | `D6m(e)` | 296757560 | NUL-strip + `JSON.parse`, `undefined` on throw |
//! | `lqT(e)` | 296757443 | `aqT[type] ?? "accumulate"` — the compaction policy table |
//! | `aqT` | 296899279 | the policy table itself |
//! | `eI(e)` | 296710850 | `type==="system" && subtype==="compact_boundary"` |
//! | drain trigger | 296777239 | `bytesSinceCompact >= backstopThresholdBytes` |
//! | boundary arm | 296794903 | `backstopThresholdBytes = Uyr; requestCompact(...)` |
//! | constants | 296896686 | `I6m` / `Uyr` / `P6m` / `O6m` |
//!
//! # Gate: inert in a default install, on purpose
//!
//! Every oracle trigger is guarded by `this.store.localGcEnabled`
//! (@296774876, @296777200), whose only setter `MIl` (@296757412) is called
//! once, from the `--resume` + `sdkUrl` hydration path (@307367489), with
//! `X0y() = CLAUDE_CODE_TRANSCRIPT_LOCAL_GC ?? gate("tengu_transcript_local_gc", false)`.
//! So upstream ships this OFF by default too. [`local_gc_enabled`] reproduces
//! the env half (rebranded `LINGXI_TRANSCRIPT_LOCAL_GC`); the statsig gate has
//! no port analogue and reads as `false`, exactly like a default install.
//!
//! # Deliberately not ported
//!
//! * `performCompactTranscriptV5` — the storage-backend twin. The `storageV5`
//!   record store is Anthropic-backend surface and out of scope.
//! * The `artifact-autoreact-ledger` torn-tail repair inside `N6m`'s
//!   unparseable-line branch (`p(h,g)` / `q9m` / `W9m`). Artifact is an
//!   excluded subsystem; the port keeps the surrounding behaviour — an
//!   unparseable non-empty line is always KEPT — which is the conservative
//!   half of that branch.
//! * `tengu_transcript_compact{,_failed}` telemetry: the `session` crate has no
//!   `telemetry` dependency (checked `session/Cargo.toml`). The reasons are
//!   modelled as [`CompactFailure`] and logged through the oracle's own warn
//!   copy, so wiring an event emitter later is a one-line change.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// `I6m = 5242880` — a transcript smaller than 5 MiB is never rewritten.
pub const MIN_COMPACT_FILE_BYTES: u64 = 5_242_880;

/// `Uyr = 20971520` — the 20 MiB byte backstop. `bytesSinceCompact` reaching it
/// triggers a rewrite; a compact boundary also RESETS the threshold to this
/// value (@296794903) after a poorly-reclaiming rewrite doubled it.
pub const COMPACT_BACKSTOP_BYTES: u64 = 20_971_520;

/// `P6m = 8*Uyr` — 160 MiB, the ceiling the doubling in [`next_backstop`] stops at.
pub const MAX_COMPACT_BACKSTOP_BYTES: u64 = 8 * COMPACT_BACKSTOP_BYTES;

/// `O6m = 0.1` — a rewrite that reclaimed less than 10 % of the file doubles
/// the backstop instead of leaving it at [`COMPACT_BACKSTOP_BYTES`].
pub const MIN_RECLAIM_FRACTION: f64 = 0.1;

/// Width of each of the three sample windows the driver hashes before and after
/// the rewrite to prove the source file did not change underneath it
/// (`let c=4096` inside `performCompactTranscript`).
const SAMPLE_WINDOW_BYTES: u64 = 4096;

/// Env override for `localGcEnabled` — `CLAUDE_CODE_TRANSCRIPT_LOCAL_GC`
/// upstream (`X0y`, @307044097), rebranded like every other `LINGXI_` knob.
pub const TRANSCRIPT_LOCAL_GC_ENV: &str = "LINGXI_TRANSCRIPT_LOCAL_GC";

/// `X0y()` — `CLAUDE_CODE_TRANSCRIPT_LOCAL_GC ?? Vs("tengu_transcript_local_gc", false)`.
///
/// The env half only; the statsig half has no port analogue and is `false`,
/// which is also its upstream default. JS truthiness: any non-empty value other
/// than the literal `"0"`/`"false"` enables it, matching the port's
/// `env_truthy` convention elsewhere.
#[must_use]
pub fn local_gc_enabled() -> bool {
    match std::env::var(TRANSCRIPT_LOCAL_GC_ENV) {
        Ok(raw) => {
            let v = raw.trim();
            !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
        }
        Err(_) => false,
    }
}

/// `this.backstopThresholdBytes = i-f < i*O6m ? Math.min(this.backstopThresholdBytes*2, P6m) : Uyr`
/// — after a rewrite, decide the next byte backstop.
///
/// Reclaiming less than 10 % means the file is mostly live transcript, so
/// rewriting again at the same cadence is wasted I/O; the threshold doubles,
/// capped at [`MAX_COMPACT_BACKSTOP_BYTES`]. A good rewrite resets it.
#[must_use]
pub fn next_backstop(current: u64, bytes_before: u64, bytes_after: u64) -> u64 {
    let reclaimed = bytes_before.saturating_sub(bytes_after) as f64;
    if reclaimed < bytes_before as f64 * MIN_RECLAIM_FRACTION {
        current.saturating_mul(2).min(MAX_COMPACT_BACKSTOP_BYTES)
    } else {
        COMPACT_BACKSTOP_BYTES
    }
}

/// The compaction policy for one transcript record type — `aqT` (@296899279),
/// read through `lqT(e) = aqT[e] ?? "accumulate"` (@296757443).
///
/// This is a DIFFERENT table from the re-append persistence table `s9m`
/// (`always` / `dedup-transcript` / `route-by-agent`): `s9m` decides which
/// records survive a *fork*, `aqT` decides which survive a *rewrite*. Several
/// types sit in both with different answers (`last-prompt` is `always` in `s9m`
/// and `boundary-cleared` here), so they must not be merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactPersistence {
    /// `"transcript"` — a conversation message. Indexed by uuid for the
    /// preserved-segment walk and the parent rescue; kept when it is at or
    /// after the last compact boundary, or reachable from it.
    Transcript,
    /// `"boundary-cleared"` — sidecar state that the compact boundary
    /// invalidates. Dropped outright when it precedes the boundary.
    BoundaryCleared,
    /// `"accumulate"` — every occurrence is meaningful; always kept. Also the
    /// DEFAULT for an unknown type, which is what makes the rewrite safe
    /// against records this build has never heard of.
    Accumulate,
    /// `"last-wins"` — only the newest per `(type, sessionId|leafUuid)` key
    /// matters; older ones are dropped and the survivor is re-emitted at the
    /// boundary so a tail-scanning reader still finds it.
    LastWins,
}

/// `lqT(e)` — `aqT[e] ?? "accumulate"`.
///
/// The table is transcribed in the oracle's own key order. `artifact-*` entries
/// are retained even though the Artifact subsystem is excluded: their POLICY is
/// what keeps a foreign transcript's artifact records from being mangled by a
/// LingXi rewrite, and getting it wrong would be silent data loss.
#[must_use]
pub fn compact_persistence(record_type: &str) -> CompactPersistence {
    match record_type {
        "user" | "assistant" | "system" | "attachment" => CompactPersistence::Transcript,
        "progress"
        | "file-history-snapshot"
        | "file-history-delta"
        | "last-prompt"
        | "continued-in"
        | "marble-origami-commit"
        | "marble-origami-snapshot"
        | "marble-origami-reset" => CompactPersistence::BoundaryCleared,
        "content-replacement" | "fork-context-ref" | "frame-link" | "artifact-comment-monitor" => {
            CompactPersistence::Accumulate
        }
        "summary"
        | "custom-title"
        | "ended-by-model"
        | "ai-title"
        | "tag"
        | "relocated"
        | "agent-name"
        | "agent-color"
        | "agent-setting"
        | "pr-link"
        | "artifact-autoreact-ledger"
        | "bridge-session"
        | "history-suppression"
        | "attribution-snapshot"
        | "mode"
        | "permission-mode"
        | "isolation-latch"
        | "atis-latch"
        | "worktree-state"
        | "cost-state"
        | "queue-operation"
        | "observer-ref" => CompactPersistence::LastWins,
        _ => CompactPersistence::Accumulate,
    }
}

/// `$6m(e)` — drop leading NUL code units.
///
/// ```js
/// function $6m(e){let t=0;while(e.charCodeAt(t)===0)t++;return t>0?e.slice(t):e}
/// ```
///
/// A crash mid-append can leave a run of NULs at the head of a line (the
/// filesystem zero-fills the hole). The rewrite is the only place that can heal
/// them, so the applier strips them on the way out — which is why a compacted
/// transcript is not always a byte subset of its input.
#[must_use]
pub fn strip_leading_nuls(line: &str) -> &str {
    line.trim_start_matches('\0')
}

/// `D6m(e)` — NUL-strip then `JSON.parse`, `undefined` on any throw.
#[must_use]
fn parse_record(line: &str) -> Option<Value> {
    if line.is_empty() {
        return None;
    }
    crate::jsonl::exact_json::parse_exact_json(strip_leading_nuls(line))
        .ok()
        .map(|exact| exact.value)
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `eI(e)` — `e?.type==="system" && e.subtype==="compact_boundary"` (@296710850).
fn is_compact_boundary(value: &Value) -> bool {
    str_field(value, "type") == Some("system")
        && str_field(value, "subtype") == Some("compact_boundary")
}

/// One `last-wins` survivor: the line to re-emit at the boundary, plus the
/// index and type the applier's two guards read.
#[derive(Debug, Clone)]
struct LastWinsEntry {
    idx: usize,
    line: String,
    record_type: String,
}

/// The `{lastBoundaryIdx, lastNonPreservedBoundaryIdx, keepIdx,
/// supersededLastWins, lastMeta}` object `N6m` returns.
#[derive(Debug, Clone)]
pub struct CompactPlan {
    /// `r` — index of the LAST `compact_boundary` line. Everything from here on
    /// is copied verbatim; the rewrite only ever touches the prefix.
    last_boundary_idx: usize,
    /// `o` — index of the last boundary that carried NEITHER `preservedSegment`
    /// NOR `preservedMessages`, or `None` when there was none. Used only to
    /// decide whether an `attribution-snapshot` may be re-emitted: a boundary
    /// that preserved nothing invalidates attributions written before it.
    last_non_preserved_boundary_idx: Option<usize>,
    /// `l` — pre-boundary indices that must survive anyway.
    keep_idx: HashSet<usize>,
    /// `u` — indices superseded by a later `last-wins` record of a type whose
    /// duplicates are dropped even AFTER the boundary.
    superseded_last_wins: HashSet<usize>,
    /// `c.values()` in JS `Map` insertion order — the surviving `last-wins`
    /// record per key.
    last_meta: Vec<LastWinsEntry>,
}

/// Why `N6m` refused to produce a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAbort {
    /// A uuid named by `compactMetadata.preservedMessages.uuids` is not in the
    /// file. Rewriting would drop a message the boundary promised to keep.
    PreservedUuidMissing,
    /// The `preservedSegment` parent walk from `tailUuid` did not reach
    /// `headUuid` — the chain is broken, so the segment cannot be identified.
    PreservedWalkBroken,
}

impl PlanAbort {
    /// The `reason` property of `tengu_transcript_compact_failed`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreservedUuidMissing => "preserved_uuid_missing",
            Self::PreservedWalkBroken => "preserved_walk_broken",
        }
    }
}

/// `N6m`'s three-way return.
#[derive(Debug, Clone)]
pub enum PlanOutcome {
    /// `{kind:"skip"}` — no compact boundary in the file, so there is nothing
    /// the rewrite is allowed to drop.
    Skip,
    /// `{kind:"abort",reason}` — the plan would be unsafe.
    Abort(PlanAbort),
    /// `{kind:"plan",plan}`.
    Plan(CompactPlan),
}

/// `N6m(e)` — one streaming pass that decides what a rewrite may drop.
///
/// `lines` yields the transcript's lines in file order, newline already
/// stripped. Returns [`PlanOutcome::Skip`] when the file has no compact
/// boundary — the common case for a young session, and the reason a huge
/// never-compacted transcript is left alone rather than rewritten pointlessly.
#[must_use]
pub fn build_compact_plan<'a, I: IntoIterator<Item = &'a str>>(lines: I) -> PlanOutcome {
    // `i` — uuid -> (parentUuid, idx), transcript records only.
    let mut by_uuid: HashMap<String, (Option<String>, usize)> = HashMap::new();
    // `s` / `a` — file-history sidecars keyed by the message they belong to.
    let mut history_snapshot: HashMap<String, usize> = HashMap::new();
    let mut history_delta: HashMap<String, Vec<usize>> = HashMap::new();
    // `l` — indices to keep regardless of position.
    let mut keep_idx: HashSet<usize> = HashSet::new();
    // `c` — last-wins survivors, in JS `Map` insertion order.
    let mut last_meta: Vec<(String, LastWinsEntry)> = Vec::new();
    // `u` — indices a later last-wins record superseded.
    let mut superseded: HashSet<usize> = HashSet::new();

    let mut last_boundary_idx: Option<usize> = None;
    let mut last_boundary_metadata: Option<Value> = None;
    let mut last_non_preserved_boundary_idx: Option<usize> = None;

    for (idx, raw) in lines.into_iter().enumerate() {
        let Some(record) = parse_record(raw) else {
            // `else if(h) l.add(t)` — a non-empty line this build cannot parse
            // is ALWAYS kept. (The oracle additionally repairs a torn
            // `artifact-autoreact-ledger` tail here; see the module docs.)
            if !raw.is_empty() {
                keep_idx.insert(idx);
            }
            continue;
        };
        let record_type = str_field(&record, "type").unwrap_or_default().to_string();

        // The two file-history side maps are populated BEFORE the policy
        // switch, so they exist even though both types are `boundary-cleared`:
        // a preserved message drags its own history records back into `keepIdx`.
        if record_type == "file-history-snapshot" {
            if let Some(mid) = str_field(&record, "messageId") {
                history_snapshot.insert(mid.to_string(), idx);
            }
        } else if record_type == "file-history-delta" {
            if let Some(mid) = str_field(&record, "messageId") {
                history_delta.entry(mid.to_string()).or_default().push(idx);
            }
        }

        match compact_persistence(&record_type) {
            CompactPersistence::Transcript => {
                if let Some(uuid) = str_field(&record, "uuid") {
                    let parent = record
                        .get("parentUuid")
                        .and_then(Value::as_str)
                        .map(ToString::to_string);
                    by_uuid.insert(uuid.to_string(), (parent, idx));
                }
                if is_compact_boundary(&record) {
                    last_boundary_idx = Some(idx);
                    let metadata = record.get("compactMetadata").cloned();
                    let preserves = metadata.as_ref().is_some_and(|m| {
                        !m.get("preservedSegment").unwrap_or(&Value::Null).is_null()
                            || !m.get("preservedMessages").unwrap_or(&Value::Null).is_null()
                    });
                    last_boundary_metadata = metadata;
                    if !preserves {
                        last_non_preserved_boundary_idx = Some(idx);
                    }
                }
            }
            CompactPersistence::Accumulate => {
                keep_idx.insert(idx);
            }
            CompactPersistence::LastWins => {
                // `d(h)` — `${type}:${type==="summary"?leafUuid:sessionId ?? ""}`.
                let discriminator = if record_type == "summary" {
                    str_field(&record, "leafUuid")
                } else {
                    str_field(&record, "sessionId")
                }
                .unwrap_or_default();
                let key = format!("{record_type}:{discriminator}");
                let entry = LastWinsEntry {
                    idx,
                    line: raw.to_string(),
                    record_type: record_type.clone(),
                };
                match last_meta.iter().position(|(k, _)| *k == key) {
                    Some(pos) => {
                        // Only these two types have their SUPERSEDED copy
                        // actively deleted; for every other last-wins type the
                        // older copy simply falls out of `keepIdx` (and a
                        // post-boundary duplicate is left alone).
                        if record_type == "attribution-snapshot"
                            || record_type == "artifact-autoreact-ledger"
                        {
                            superseded.insert(last_meta[pos].1.idx);
                        }
                        // JS `Map.set` on an existing key keeps its insertion
                        // position — replace in place, do NOT push.
                        last_meta[pos].1 = entry;
                    }
                    None => last_meta.push((key, entry)),
                }
            }
            CompactPersistence::BoundaryCleared => {}
        }
    }

    let Some(boundary) = last_boundary_idx else {
        return PlanOutcome::Skip;
    };

    // Pull one message (and its file-history sidecars) back into `keepIdx`.
    let keep_message = |uuid: &str, keep: &mut HashSet<usize>, idx: usize| {
        keep.insert(idx);
        if let Some(&snap) = history_snapshot.get(uuid) {
            keep.insert(snap);
        }
        if let Some(deltas) = history_delta.get(uuid) {
            for &d in deltas {
                keep.insert(d);
            }
        }
    };

    let preserved_messages = last_boundary_metadata
        .as_ref()
        .and_then(|m| m.get("preservedMessages"))
        .filter(|v| !v.is_null());
    let preserved_segment = last_boundary_metadata
        .as_ref()
        .and_then(|m| m.get("preservedSegment"))
        .filter(|v| !v.is_null());

    if let Some(pm) = preserved_messages {
        let uuids = pm.get("uuids").and_then(Value::as_array);
        for uuid in uuids.into_iter().flatten() {
            let Some(uuid) = uuid.as_str() else { continue };
            let Some(&(_, idx)) = by_uuid.get(uuid) else {
                return PlanOutcome::Abort(PlanAbort::PreservedUuidMissing);
            };
            keep_message(uuid, &mut keep_idx, idx);
        }
    } else if let Some(seg) = preserved_segment {
        let head_uuid = str_field(seg, "headUuid").map(ToString::to_string);
        let mut cursor = str_field(seg, "tailUuid").map(ToString::to_string);
        let mut seen: HashSet<String> = HashSet::new();
        while let Some(current) = cursor.clone() {
            if seen.contains(&current) {
                break;
            }
            seen.insert(current.clone());
            match by_uuid.get(&current) {
                Some((parent, idx)) => {
                    let (parent, idx) = (parent.clone(), *idx);
                    keep_message(&current, &mut keep_idx, idx);
                    if Some(&current) == head_uuid.as_ref() {
                        // `break` INSIDE the `if(_)` arm: `cursor` keeps
                        // pointing at headUuid, which is what the post-loop
                        // equality check reads.
                        break;
                    }
                    cursor = parent;
                }
                // `g=_?.parent` with `_` undefined ends the walk with
                // `cursor = None`, which fails the equality check below.
                None => cursor = None,
            }
        }
        if cursor != head_uuid {
            return PlanOutcome::Abort(PlanAbort::PreservedWalkBroken);
        }
    }

    // Parent rescue: a message at/after the boundary whose parent lives BEFORE
    // it keeps that parent alive, one level only (the oracle does not recurse —
    // a grandparent is not rescued, and `m` is collected before any of it is
    // merged into `l` so a rescued parent cannot itself rescue).
    let mut rescued: HashSet<usize> = HashSet::new();
    for (parent, idx) in by_uuid.values() {
        let Some(parent) = parent else { continue };
        if *idx < boundary {
            continue;
        }
        if let Some(&(_, parent_idx)) = by_uuid.get(parent) {
            if parent_idx < boundary && !keep_idx.contains(&parent_idx) {
                rescued.insert(parent_idx);
            }
        }
    }
    keep_idx.extend(rescued);

    PlanOutcome::Plan(CompactPlan {
        last_boundary_idx: boundary,
        last_non_preserved_boundary_idx,
        keep_idx,
        superseded_last_wins: superseded,
        last_meta: last_meta.into_iter().map(|(_, v)| v).collect(),
    })
}

impl CompactPlan {
    /// `F6m(e)` — the lines the rewrite emits for input line `idx`.
    ///
    /// ```js
    /// return(t,r)=>{let n=[];
    ///  if((t>=e.lastBoundaryIdx||e.keepIdx.has(t))&&!e.supersededLastWins.has(t))n.push(r);
    ///  if(t===e.lastBoundaryIdx)
    ///   for(let{idx:o,line:i,type:s}of e.lastMeta.values())
    ///    if(o<e.lastBoundaryIdx&&(s!=="attribution-snapshot"||o>e.lastNonPreservedBoundaryIdx))n.push(i);
    ///  return n}
    /// ```
    ///
    /// Zero, one or many lines: the boundary line itself is followed by every
    /// surviving `last-wins` record that used to live before it, hoisted so a
    /// tail-scanning reader still sees the session's title / mode / worktree.
    #[must_use]
    pub fn apply<'a>(&'a self, idx: usize, line: &'a str) -> Vec<&'a str> {
        let mut out: Vec<&str> = Vec::new();
        if (idx >= self.last_boundary_idx || self.keep_idx.contains(&idx))
            && !self.superseded_last_wins.contains(&idx)
        {
            out.push(line);
        }
        if idx == self.last_boundary_idx {
            for entry in &self.last_meta {
                if entry.idx >= self.last_boundary_idx {
                    continue;
                }
                // `o > e.lastNonPreservedBoundaryIdx` with the JS sentinel
                // `-1` — an absent non-preserving boundary lets EVERY
                // attribution-snapshot through, since any index is `> -1`.
                if entry.record_type == "attribution-snapshot"
                    && self
                        .last_non_preserved_boundary_idx
                        .is_some_and(|b| entry.idx <= b)
                {
                    continue;
                }
                out.push(&entry.line);
            }
        }
        out
    }

    /// Index of the last compact boundary — the point after which the rewrite
    /// copies verbatim.
    #[must_use]
    pub fn last_boundary_idx(&self) -> usize {
        self.last_boundary_idx
    }
}

/// Why `performCompactTranscript` gave up — the `reason` property of
/// `tengu_transcript_compact_failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactFailure {
    /// `"snapshot_mid_line"` — the file does not end on a newline, so a
    /// concurrent append is in flight and the tail cannot be parsed safely.
    SnapshotMidLine,
    /// `"source_changed"` — the inode, the size or one of the three sample
    /// windows moved while the rewrite was running.
    SourceChanged,
    /// `"rename_fallback"` — the publish rename failed with a code the oracle
    /// classifies as a cross-device / permission fallback.
    RenameFallback(String),
    /// `"io"` — anything else.
    Io(String),
    /// The plan builder refused; reason spelled by [`PlanAbort::as_str`].
    Plan(PlanAbort),
}

impl CompactFailure {
    /// The telemetry `reason` spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::SnapshotMidLine => "snapshot_mid_line",
            Self::SourceChanged => "source_changed",
            Self::RenameFallback(_) => "rename_fallback",
            Self::Io(_) => "io",
            Self::Plan(p) => p.as_str(),
        }
    }
}

/// `tengu_transcript_compact{bytesBefore,bytesAfter}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactStats {
    /// Size of the transcript before the rewrite.
    pub bytes_before: u64,
    /// Size written, including any bytes that arrived during the rewrite.
    pub bytes_after: u64,
}

/// What one [`perform_compact_transcript`] call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactOutcome {
    /// Below [`MIN_COMPACT_FILE_BYTES`], or no compact boundary yet — the two
    /// `return` (not `throw`) exits of the oracle.
    Skipped,
    /// Rewritten and published.
    Compacted(CompactStats),
    /// Abandoned; the transcript is untouched and the temp file is gone.
    Failed(CompactFailure),
}

/// The three 4 KiB sample offsets `performCompactTranscript` re-reads to prove
/// the source did not change: head, middle, tail.
fn sample_offsets(size: u64) -> [(u64, u64); 3] {
    let len = SAMPLE_WINDOW_BYTES.min(size);
    [
        (0, len),
        ((size / 2).saturating_sub(SAMPLE_WINDOW_BYTES / 2), len),
        (size.saturating_sub(SAMPLE_WINDOW_BYTES), len),
    ]
}

fn read_samples(path: &Path, windows: &[(u64, u64); 3]) -> std::io::Result<Vec<Vec<u8>>> {
    let mut file = File::open(path)?;
    let mut out = Vec::with_capacity(3);
    for &(offset, len) in windows {
        let len = usize::try_from(len).unwrap_or(0);
        let mut buf = vec![0u8; len];
        file.seek(SeekFrom::Start(offset))?;
        let mut filled = 0usize;
        while filled < len {
            match file.read(&mut buf[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        buf.truncate(filled);
        out.push(buf);
    }
    Ok(out)
}

/// Stable identity of the open file. `ino` on unix — the oracle's `C.ino!==l.ino`
/// check, which is what catches "someone replaced the transcript by rename".
#[cfg(unix)]
fn file_identity(meta: &std::fs::Metadata) -> u128 {
    use std::os::unix::fs::MetadataExt;
    (u128::from(meta.dev()) << 64) | u128::from(meta.ino())
}

/// Windows has no inode; fall back to the creation timestamp, which changes on
/// the same replace-by-rename this guard exists to catch.
#[cfg(not(unix))]
fn file_identity(meta: &std::fs::Metadata) -> u128 {
    meta.created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos())
}

/// Read the file's lines up to `limit` bytes, newline (and a trailing `\r`)
/// stripped — the oracle's `createInterface({input:createReadStream(e,
/// {encoding:"utf8",end:l.size-1}),crlfDelay:Infinity})`.
///
/// Lossy UTF-8 exactly like `encoding:"utf8"`; splitting on `\n` first means a
/// multi-byte sequence is never cut, so the only replacement characters are the
/// ones the oracle would also produce.
fn read_lines(path: &Path, limit: u64) -> std::io::Result<Vec<String>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file.take(limit));
    let mut lines = Vec::new();
    loop {
        let mut buf = Vec::new();
        if std::io::BufRead::read_until(&mut reader, b'\n', &mut buf)? == 0 {
            break;
        }
        if buf.last() == Some(&b'\n') {
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
        }
        lines.push(String::from_utf8_lossy(&buf).into_owned());
    }
    Ok(lines)
}

/// A temp path sibling of `path`: `<path>.compact.tmp.<8 hex>`
/// (`randomBytes(4).toString("hex")`).
fn temp_path(path: &Path) -> PathBuf {
    let random = uuid::Uuid::new_v4().simple().to_string();
    let suffix = &random[..8];
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".compact.tmp.{suffix}"));
    PathBuf::from(name)
}

/// `performCompactTranscript(e)` — rewrite `path` in place, dropping every
/// record the plan supersedes.
///
/// # The safety envelope IS the feature
///
/// Every step below exists because the transcript is being appended to by this
/// same process while the rewrite runs:
///
/// 1. skip under [`MIN_COMPACT_FILE_BYTES`];
/// 2. snapshot `(inode, size)` and three 4 KiB sample windows;
/// 3. refuse when the last window does not end on `\n` (`snapshot_mid_line`) —
///    a torn tail means the last line is half-written;
/// 4. plan and rewrite ONLY the first `size` bytes, into a sibling temp file;
/// 5. re-verify inode, size-did-not-shrink, and all three windows
///    (`source_changed`);
/// 6. copy the bytes that arrived during the rewrite, trimmed back to the last
///    `\n` so a torn tail is never published;
/// 7. `fsync`, re-verify inode + windows once more, then rename;
/// 8. unlink the temp file on every failure path.
///
/// Dropping any one of these silently truncates a user's transcript, which is
/// why the earlier waves deferred the whole thing rather than ship half of it.
///
/// This is synchronous by design — it is called from
/// [`crate::jsonl::writer::JsonlWriter`] inside `spawn_blocking`.
pub fn perform_compact_transcript(path: &Path) -> CompactOutcome {
    compact_with_min(path, MIN_COMPACT_FILE_BYTES)
}

/// [`perform_compact_transcript`] with the 5 MiB floor as a parameter.
///
/// Only the unit tests pass anything other than [`MIN_COMPACT_FILE_BYTES`] —
/// exercising the rewrite for real needs a file that crosses the floor, and a
/// 5 MiB fixture per case would make the suite pay 100 MB of I/O to assert
/// bookkeeping the floor has nothing to do with.
fn compact_with_min(path: &Path, min_bytes: u64) -> CompactOutcome {
    match compact_inner(path, min_bytes) {
        Ok(outcome) => outcome,
        Err(e) => {
            // `T(`Transcript compact failed (${bt(l)}): ${ce(l)}`,{level:"warn"})`
            tracing::warn!("Transcript compact failed (io): {e}");
            CompactOutcome::Failed(CompactFailure::Io(e.to_string()))
        }
    }
}

fn compact_inner(path: &Path, min_bytes: u64) -> std::io::Result<CompactOutcome> {
    let before_meta = std::fs::metadata(path)?;
    let size = before_meta.len();
    if size < min_bytes {
        return Ok(CompactOutcome::Skipped);
    }
    let identity = file_identity(&before_meta);
    let windows = sample_offsets(size);
    let before_samples = read_samples(path, &windows)?;

    // `let w=p[2]; if(!w||w.length===0||w.at(-1)!==10)` — a torn tail.
    match before_samples.get(2) {
        Some(tail) if tail.last() == Some(&b'\n') => {}
        _ => {
            tracing::warn!("Transcript compact skipped (snapshot_mid_line): torn tail");
            return Ok(CompactOutcome::Failed(CompactFailure::SnapshotMidLine));
        }
    }

    let lines = read_lines(path, size)?;
    let plan = match build_compact_plan(lines.iter().map(String::as_str)) {
        PlanOutcome::Skip => return Ok(CompactOutcome::Skipped),
        PlanOutcome::Abort(reason) => {
            tracing::warn!(
                "Transcript compact skipped ({}): plan aborted",
                reason.as_str()
            );
            return Ok(CompactOutcome::Failed(CompactFailure::Plan(reason)));
        }
        PlanOutcome::Plan(plan) => plan,
    };

    let tmp = temp_path(path);
    let result = write_and_publish(
        path,
        &tmp,
        &lines,
        &plan,
        &Snapshot {
            size,
            identity,
            windows,
            samples: before_samples,
        },
    );
    if !matches!(result, Ok(CompactOutcome::Compacted(_))) {
        // `finally{if(!s)await ap.unlink(i).catch(()=>{})}`
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Everything the driver captured about the source file BEFORE the rewrite —
/// the whole basis of the `source_changed` verdict.
struct Snapshot {
    /// `l.size` — the byte bound the rewrite is allowed to read and rewrite.
    size: u64,
    /// `l.ino`.
    identity: u128,
    /// The three sample windows' offsets and lengths.
    windows: [(u64, u64); 3],
    /// Their contents at snapshot time (`p`).
    samples: Vec<Vec<u8>>,
}

fn write_and_publish(
    path: &Path,
    tmp: &Path,
    lines: &[String],
    plan: &CompactPlan,
    snapshot: &Snapshot,
) -> std::io::Result<CompactOutcome> {
    let Snapshot {
        size,
        identity,
        windows,
        samples,
    } = snapshot;
    let (size, identity) = (*size, *identity);
    let mut bytes_after: u64 = 0;
    {
        let mut out = open_tmp(tmp)?;
        let mut buffered = String::new();
        for (idx, line) in lines.iter().enumerate() {
            for emitted in plan.apply(idx, line) {
                buffered.push_str(strip_leading_nuls(emitted));
                buffered.push('\n');
            }
            // `if(_.length>=IP)` — flush at the 64 KiB tail-window size.
            if buffered.len() >= crate::jsonl::LITE_READ_BUF_SIZE {
                bytes_after += buffered.len() as u64;
                out.write_all(buffered.as_bytes())?;
                buffered.clear();
            }
        }
        bytes_after += buffered.len() as u64;
        out.write_all(buffered.as_bytes())?;

        // First re-verification: inode, no shrink, and the three windows.
        let now = std::fs::metadata(path)?;
        if file_identity(&now) != identity
            || now.len() < size
            || read_samples(path, windows)? != *samples
        {
            tracing::warn!("Transcript compact failed (source_changed): source moved");
            return Ok(CompactOutcome::Failed(CompactFailure::SourceChanged));
        }

        // Bytes appended while the rewrite ran, trimmed back to the last `\n`.
        if now.len() > size {
            let mut src = File::open(path)?;
            src.seek(SeekFrom::Start(size))?;
            let extra_len = usize::try_from(now.len() - size).unwrap_or(0);
            let mut extra = vec![0u8; extra_len];
            let mut filled = 0usize;
            while filled < extra_len {
                match src.read(&mut extra[filled..])? {
                    0 => break,
                    n => filled += n,
                }
            }
            while filled > 0 && extra[filled - 1] != b'\n' {
                filled -= 1;
            }
            bytes_after += filled as u64;
            out.write_all(&extra[..filled])?;
        }

        out.sync_all()?;

        // Second re-verification, after the fsync — the oracle checks again
        // because the append window is still open.
        let now = std::fs::metadata(path)?;
        if file_identity(&now) != identity || read_samples(path, windows)? != *samples {
            tracing::warn!("Transcript compact failed (source_changed): inode moved");
            return Ok(CompactOutcome::Failed(CompactFailure::SourceChanged));
        }
    }

    if let Err(e) = std::fs::rename(tmp, path) {
        tracing::warn!("Transcript compact failed (rename_fallback): {e}");
        return Ok(CompactOutcome::Failed(CompactFailure::RenameFallback(
            e.to_string(),
        )));
    }
    Ok(CompactOutcome::Compacted(CompactStats {
        bytes_before: size,
        bytes_after,
    }))
}

#[cfg(unix)]
fn open_tmp(tmp: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        // `ap.open(i,"w",384)` — 384 == 0o600.
        .mode(0o600)
        .open(tmp)
}

#[cfg(not(unix))]
fn open_tmp(tmp: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_preserved_segment_recognizes_native_exact_peer_rows_and_copies_bytes() {
        let fixture = include_str!("../../tests/fixtures/handback_exact_utf16_2_1_286.jsonl");
        let mut lines: Vec<_> = fixture.lines().map(str::to_string).collect();
        lines.push(r#"{"type":"system","subtype":"compact_boundary","uuid":"boundary","parentUuid":null,"compactMetadata":{"preservedSegment":{"headUuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaa1","anchorUuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaa1","tailUuid":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaa4"}}}"#.to_string());
        let plan = plan_of(&lines);
        let copied: Vec<_> = lines
            .iter()
            .enumerate()
            .flat_map(|(index, line)| plan.apply(index, line))
            .collect();
        assert_eq!(copied, lines.iter().map(String::as_str).collect::<Vec<_>>());
    }

    /// One transcript message line.
    fn msg(uuid: &str, parent: Option<&str>) -> String {
        let parent = parent.map_or("null".to_string(), |p| format!("\"{p}\""));
        format!(r#"{{"type":"user","uuid":"{uuid}","parentUuid":{parent}}}"#)
    }

    /// A `compact_boundary` system line, optionally carrying `preservedSegment`.
    fn boundary(uuid: &str, metadata: &str) -> String {
        format!(
            r#"{{"type":"system","subtype":"compact_boundary","uuid":"{uuid}","parentUuid":null,"compactMetadata":{metadata}}}"#
        )
    }

    fn title(session: &str, text: &str) -> String {
        format!(r#"{{"type":"custom-title","customTitle":"{text}","sessionId":"{session}"}}"#)
    }

    fn plan_of(lines: &[String]) -> CompactPlan {
        match build_compact_plan(lines.iter().map(String::as_str)) {
            PlanOutcome::Plan(p) => p,
            other => panic!("expected a plan, got {other:?}"),
        }
    }

    fn rewrite(lines: &[String]) -> Vec<String> {
        let plan = plan_of(lines);
        let mut out = Vec::new();
        for (idx, line) in lines.iter().enumerate() {
            for emitted in plan.apply(idx, line) {
                out.push(strip_leading_nuls(emitted).to_string());
            }
        }
        out
    }

    #[test]
    fn latest_policy_keeps_all_monitors_and_last_cost_but_clears_continuation() {
        // Claude Code 2.1.261 wys @166365512: monitors accumulate;
        // continued-in is boundary-cleared; cost-state is last-wins.
        let monitor1 =
            r#"{"type":"artifact-comment-monitor","sessionId":"s","cursor":1}"#.to_string();
        let monitor2 =
            r#"{"type":"artifact-comment-monitor","sessionId":"s","cursor":2}"#.to_string();
        let old_cost = r#"{"type":"cost-state","sessionId":"s","total":1}"#.to_string();
        let cost = r#"{"type":"cost-state","sessionId":"s","total":2}"#.to_string();
        let continuation =
            r#"{"type":"continued-in","sessionId":"s","nextSessionId":"next"}"#.to_string();
        let marker = boundary("b", "{}");
        assert_eq!(
            rewrite(&[
                monitor1.clone(),
                old_cost,
                continuation,
                monitor2.clone(),
                cost.clone(),
                marker.clone()
            ]),
            vec![monitor1, monitor2, marker, cost]
        );
    }

    /// `aqT` transcription check, including the DEFAULT arm — the one that
    /// keeps a record type this build has never seen (`?? "accumulate"`).
    #[test]
    fn sc08_persistence_table_matches_the_oracle() {
        use CompactPersistence::{Accumulate, BoundaryCleared, LastWins, Transcript};
        for t in ["user", "assistant", "system", "attachment"] {
            assert_eq!(compact_persistence(t), Transcript, "{t}");
        }
        for t in [
            "progress",
            "file-history-snapshot",
            "file-history-delta",
            "last-prompt",
            "continued-in",
            "marble-origami-commit",
            "marble-origami-snapshot",
            "marble-origami-reset",
        ] {
            assert_eq!(compact_persistence(t), BoundaryCleared, "{t}");
        }
        for t in [
            "content-replacement",
            "fork-context-ref",
            "frame-link",
            "artifact-comment-monitor",
        ] {
            assert_eq!(compact_persistence(t), Accumulate, "{t}");
        }
        for t in [
            "summary",
            "custom-title",
            "ended-by-model",
            "ai-title",
            "tag",
            "relocated",
            "agent-name",
            "agent-color",
            "agent-setting",
            "pr-link",
            "artifact-autoreact-ledger",
            "bridge-session",
            "history-suppression",
            "attribution-snapshot",
            "mode",
            "permission-mode",
            "isolation-latch",
            "atis-latch",
            "worktree-state",
            "cost-state",
            "queue-operation",
            "observer-ref",
        ] {
            assert_eq!(compact_persistence(t), LastWins, "{t}");
        }
        // The default arm — a type from a NEWER build must never be dropped.
        assert_eq!(compact_persistence("something-from-the-future"), Accumulate);
        assert_eq!(compact_persistence(""), Accumulate);
    }

    /// No boundary anywhere ⇒ `{kind:"skip"}`: a young session's transcript is
    /// entirely live and nothing may be dropped from it.
    #[test]
    fn sc08_no_boundary_is_a_skip() {
        let lines = [msg("a", None), msg("b", Some("a")), title("s", "hi")];
        assert!(matches!(
            build_compact_plan(lines.iter().map(String::as_str)),
            PlanOutcome::Skip
        ));
    }

    /// The core reclamation: pre-boundary messages and superseded metadata go
    /// away, the surviving `last-wins` record is hoisted to the boundary, and
    /// everything at/after the boundary is copied verbatim.
    #[test]
    fn sc08_rewrite_drops_pre_boundary_records_and_hoists_last_wins() {
        let lines = vec![
            msg("a", None),        // 0 dropped
            title("s", "old"),     // 1 superseded by idx 3
            msg("b", Some("a")),   // 2 dropped
            title("s", "new"),     // 3 hoisted to the boundary
            boundary("bnd", "{}"), // 4 kept
            msg("c", Some("bnd")), // 5 kept
        ];
        assert_eq!(
            rewrite(&lines),
            vec![
                boundary("bnd", "{}"),
                title("s", "new"),
                msg("c", Some("bnd")),
            ]
        );
    }

    /// A `last-wins` key is `(type, sessionId)`; two sessions in one file keep
    /// one survivor each. (A `summary` keys on `leafUuid` instead.)
    #[test]
    fn sc08_last_wins_keys_on_type_and_session() {
        let lines = vec![
            title("s1", "one"),
            title("s2", "two"),
            title("s1", "one-b"),
            boundary("bnd", "{}"),
        ];
        assert_eq!(
            rewrite(&lines),
            vec![
                boundary("bnd", "{}"),
                title("s1", "one-b"),
                title("s2", "two"),
            ],
            "insertion order of the surviving keys is the JS Map's, not sorted"
        );
    }

    /// `preservedSegment` drags its whole head→tail chain back over the
    /// boundary, together with each preserved message's file-history sidecars —
    /// the records `boundary-cleared` would otherwise have deleted.
    #[test]
    fn sc08_preserved_segment_walk_keeps_the_chain_and_its_history() {
        let meta = r#"{"preservedSegment":{"headUuid":"a","anchorUuid":"a","tailUuid":"c"}}"#;
        let lines = vec![
            msg("a", None),                                                    // 0 head
            r#"{"type":"file-history-snapshot","messageId":"a"}"#.to_string(), // 1
            msg("b", Some("a")),                                               // 2
            r#"{"type":"file-history-delta","messageId":"b"}"#.to_string(),    // 3
            msg("c", Some("b")),                                               // 4 tail
            msg("z", None),        // 5 unrelated, dropped
            boundary("bnd", meta), // 6
        ];
        let got = rewrite(&lines);
        assert!(got.contains(&msg("a", None)));
        assert!(got.contains(&msg("b", Some("a"))));
        assert!(got.contains(&msg("c", Some("b"))));
        assert!(got.contains(&r#"{"type":"file-history-snapshot","messageId":"a"}"#.to_string()));
        assert!(got.contains(&r#"{"type":"file-history-delta","messageId":"b"}"#.to_string()));
        assert!(
            !got.contains(&msg("z", None)),
            "unrelated message is reclaimed"
        );
    }

    /// A broken parent chain must ABORT, not silently publish a transcript that
    /// lost a message the boundary promised to keep.
    #[test]
    fn sc08_broken_preserved_walk_aborts() {
        let meta = r#"{"preservedSegment":{"headUuid":"a","anchorUuid":"a","tailUuid":"c"}}"#;
        let lines = [
            msg("a", None),
            // "b" is missing, so the walk from "c" dead-ends before "a".
            msg("c", Some("b")),
            boundary("bnd", meta),
        ];
        assert!(matches!(
            build_compact_plan(lines.iter().map(String::as_str)),
            PlanOutcome::Abort(PlanAbort::PreservedWalkBroken)
        ));

        let pm = r#"{"preservedMessages":{"anchorUuid":"a","uuids":["a","gone"]}}"#;
        let lines = [msg("a", None), boundary("bnd", pm)];
        assert!(matches!(
            build_compact_plan(lines.iter().map(String::as_str)),
            PlanOutcome::Abort(PlanAbort::PreservedUuidMissing)
        ));
    }

    /// A post-boundary message whose parent lives before the boundary rescues
    /// that parent — one level only, so the grandparent still goes.
    #[test]
    fn sc08_parent_rescue_is_one_level_only() {
        let lines = vec![
            msg("gp", None),       // 0 grandparent — reclaimed
            msg("p", Some("gp")),  // 1 parent — rescued
            boundary("bnd", "{}"), // 2
            msg("kid", Some("p")), // 3
        ];
        let got = rewrite(&lines);
        assert!(got.contains(&msg("p", Some("gp"))), "parent is rescued");
        assert!(!got.contains(&msg("gp", None)), "grandparent is not");
    }

    /// An unparseable non-empty line is ALWAYS kept; a NUL-prefixed line is
    /// healed on the way out (`$6m`), which is the one case where a compacted
    /// transcript is not a byte subset of its input.
    #[test]
    fn sc08_unparseable_lines_survive_and_nul_prefixes_are_healed() {
        assert_eq!(strip_leading_nuls("\0\0{\"a\":1}"), "{\"a\":1}");
        assert_eq!(strip_leading_nuls("{\"a\":1}"), "{\"a\":1}");
        let torn = format!("\0\0{}", title("s", "healed"));
        let lines = vec!["not json at all".to_string(), torn, boundary("bnd", "{}")];
        let got = rewrite(&lines);
        assert!(got.contains(&"not json at all".to_string()));
        // The NUL-prefixed line parses after the strip, so it is a live
        // `custom-title` and gets hoisted — WITHOUT its NULs.
        assert!(got.contains(&title("s", "healed")));
    }

    /// `attribution-snapshot` is the one hoist with an extra guard: a boundary
    /// that preserved NOTHING invalidates attributions written before it.
    #[test]
    fn sc08_attribution_snapshot_hoist_respects_the_non_preserving_boundary() {
        let attribution = r#"{"type":"attribution-snapshot","sessionId":"s"}"#.to_string();
        let seg = r#"{"preservedSegment":{"headUuid":"a","anchorUuid":"a","tailUuid":"a"}}"#;
        // attribution BEFORE a non-preserving boundary ⇒ not hoisted.
        let lines = vec![
            attribution.clone(),  // 0
            msg("a", None),       // 1
            boundary("b1", "{}"), // 2 non-preserving
            boundary("b2", seg),  // 3 last boundary
        ];
        assert!(!rewrite(&lines).contains(&attribution));
        // attribution AFTER it ⇒ hoisted.
        let lines = vec![
            msg("a", None),       // 0
            boundary("b1", "{}"), // 1 non-preserving
            attribution.clone(),  // 2
            boundary("b2", seg),  // 3
        ];
        assert!(rewrite(&lines).contains(&attribution));
    }

    /// `next_backstop`: a poor rewrite doubles the threshold (capped), a good
    /// one resets it.
    #[test]
    fn sc08_backstop_doubles_on_a_poor_reclaim() {
        // 5 % reclaimed → double.
        assert_eq!(
            next_backstop(COMPACT_BACKSTOP_BYTES, 1000, 950),
            COMPACT_BACKSTOP_BYTES * 2
        );
        // 50 % reclaimed → reset.
        assert_eq!(
            next_backstop(COMPACT_BACKSTOP_BYTES * 4, 1000, 500),
            COMPACT_BACKSTOP_BYTES
        );
        // The ceiling holds.
        assert_eq!(
            next_backstop(MAX_COMPACT_BACKSTOP_BYTES, 1000, 1000),
            MAX_COMPACT_BACKSTOP_BYTES
        );
    }

    fn write_transcript(dir: &Path, lines: &[String], trailing_newline: bool) -> PathBuf {
        let path = dir.join("session.jsonl");
        let mut f = std::fs::File::create(&path).expect("create fixture");
        for (i, line) in lines.iter().enumerate() {
            f.write_all(line.as_bytes()).expect("write");
            if trailing_newline || i + 1 < lines.len() {
                f.write_all(b"\n").expect("write nl");
            }
        }
        f.sync_all().expect("sync");
        path
    }

    /// End-to-end: the file on disk shrinks, is republished at the same path,
    /// and the temp file is gone.
    #[test]
    fn sc08_end_to_end_rewrite_publishes_and_cleans_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut lines: Vec<String> = Vec::new();
        for i in 0..200 {
            lines.push(msg(&format!("m{i}"), None));
            lines.push(title("s", &format!("t{i}")));
        }
        lines.push(boundary("bnd", "{}"));
        lines.push(msg("live", Some("bnd")));
        let path = write_transcript(dir.path(), &lines, true);
        let before = std::fs::metadata(&path).expect("stat").len();

        let outcome = compact_with_min(&path, 0);
        let CompactOutcome::Compacted(stats) = outcome else {
            panic!("expected a rewrite, got {outcome:?}");
        };
        assert_eq!(stats.bytes_before, before);
        assert!(stats.bytes_after < stats.bytes_before);

        let after = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(
            after.lines().count(),
            3,
            "boundary + hoisted title + live message"
        );
        assert!(after.contains("\"t199\""), "the surviving title is hoisted");
        assert!(!after.contains("\"t0\""), "superseded titles are gone");
        assert_eq!(stats.bytes_after as usize, after.len());

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".compact.tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }

    /// A file whose tail is a half-written line is refused — this is the guard
    /// that stops the rewrite from parsing (and then dropping) a torn record.
    #[test]
    fn sc08_torn_tail_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lines = vec![
            msg("a", None),
            boundary("bnd", "{}"),
            "{\"type\":\"us".to_string(),
        ];
        let path = write_transcript(dir.path(), &lines, false);
        let before = std::fs::read(&path).expect("read");
        assert_eq!(
            compact_with_min(&path, 0),
            CompactOutcome::Failed(CompactFailure::SnapshotMidLine)
        );
        assert_eq!(std::fs::read(&path).expect("read"), before, "untouched");
    }

    /// Under the 5 MiB floor the real entry point never touches the file.
    #[test]
    fn sc08_small_files_are_skipped_by_the_real_entry_point() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lines = vec![msg("a", None), boundary("bnd", "{}")];
        let path = write_transcript(dir.path(), &lines, true);
        let before = std::fs::read(&path).expect("read");
        assert_eq!(perform_compact_transcript(&path), CompactOutcome::Skipped);
        assert_eq!(std::fs::read(&path).expect("read"), before);
    }
}
