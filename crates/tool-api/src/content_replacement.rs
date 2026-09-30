//! Content replacement state — tracks per-turn cleared tool results.
//!
//! Claude Code 2.1.245 threads an extra `storageV5` argument through
//! `insertContentReplacement` / content-replacement apply / subagent-exit
//! precompute. That store is Anthropic's remote session backend; this module
//! keeps the in-memory map only. The extra persist arg is a no-op without that
//! backend (same scope cut as `performCompactTranscriptV5`).
//!
//! ⚠️ SES-3, verified 2026-09-14 — and stated more carefully than in
//! `46743c4d3`, whose note said "nothing in this port ever replaces a tool
//! result". That is WRONG about the behaviour and right only about this struct.
//!
//! `compaction::microcompact` DOES replace old tool results: it keeps the last
//! `keep_recent` compactable ids and swaps every older `ToolResult` for
//! `[Old tool result content cleared]`, above a 20,000-token floor, on the
//! time-gap trigger. What is missing is the BOOKKEEPING — this state is `None`
//! at every `ToolUseContext` construction site in the workspace, so the
//! replacements happen and are never recorded, which is why there is nothing
//! for a `content-replacement` row to carry.
//!
//! Two halves are genuinely absent, and they are one feature with TL-6 and
//! CMP-2 rather than three backlog items:
//!
//! * upstream's clear takes an optional `persist(content, tool_use_id)` that
//!   writes the cleared content to disk and leaves a reference in its place —
//!   that hook is TL-6's `persistedToolResultFiles` surface;
//! * the replacement is recorded here and persisted as a `content-replacement`
//!   row, which is SES-3 proper.
//!
//! ⇒ Populating this struct from the microcompact clear is the small end; the
//! persist hook is the larger one. Neither is "add a row writer".

use lingxi_core::types::ToolUseId;
use std::collections::HashMap;

/// Tracks replaced tool result content (large outputs cleared in older turns).
#[derive(Debug, Clone, Default)]
pub struct ContentReplacementState {
    /// Per-tool-use replacement records.
    pub replacements: HashMap<ToolUseId, ReplacementRecord>,
    /// Total character budget across the session.
    pub total_budget_chars: usize,
    /// Characters consumed by current results.
    pub used_chars: usize,
}

/// Record of one replaced tool result.
#[derive(Debug, Clone)]
pub struct ReplacementRecord {
    /// Original result size (chars).
    pub original_size: usize,
    /// Turn number when the result was replaced.
    pub replaced_at_turn: u32,
    /// Placeholder text shown in place of the original (e.g., `"[Old tool result content cleared]"`).
    pub placeholder: String,
}
