//! `tengu_memory_*` event schemas — 12 events (M3-02 emits).
//!
//! Spec §7 line 757-768. Covers LINGXI.md hierarchy load, case-mismatch
//! detection, oversize gating, secret-scanner redactions, age-based ranking,
//! and team-memory scans. Paths route through [`PiiTagged`] (privileged
//! `BigQuery` proto column); rule identifiers and filenames route through
//! [`Verified`].

use crate::pii::{PiiTagged, Verified};
use serde::{Deserialize, Serialize};

/// `tengu_memory_loaded` — LINGXI.md hierarchy successfully loaded.
pub const LOADED: &str = "tengu_memory_loaded";
/// `tengu_memory_load_failed` — load aborted before any file resident.
pub const LOAD_FAILED: &str = "tengu_memory_load_failed";
/// `tengu_memory_case_mismatch` — a LINGXI.md sibling exists under a different case.
pub const CASE_MISMATCH: &str = "tengu_memory_case_mismatch";
/// `tengu_memory_file_too_large` — file exceeded the 10 MB cap.
pub const FILE_TOO_LARGE: &str = "tengu_memory_file_too_large";
/// `tengu_memory_secret_redacted` — secret scanner replaced a match.
pub const SECRET_REDACTED: &str = "tengu_memory_secret_redacted";
/// `tengu_memory_age_penalty_applied` — relevance reduced by age decay.
pub const AGE_PENALTY_APPLIED: &str = "tengu_memory_age_penalty_applied";
/// `tengu_memory_dropped_for_age` — entry dropped past the age threshold.
pub const DROPPED_FOR_AGE: &str = "tengu_memory_dropped_for_age";
/// `tengu_memory_rank_computed` — final relevance score computed for an entry.
pub const RANK_COMPUTED: &str = "tengu_memory_rank_computed";
/// `tengu_memory_team_scan_started` — team-memory directory scan began.
pub const TEAM_SCAN_STARTED: &str = "tengu_memory_team_scan_started";
/// `tengu_memory_team_scan_completed` — team-memory directory scan finished.
pub const TEAM_SCAN_COMPLETED: &str = "tengu_memory_team_scan_completed";
/// `tengu_memory_team_scan_failed` — team-memory directory scan errored.
pub const TEAM_SCAN_FAILED: &str = "tengu_memory_team_scan_failed";
/// `tengu_memory_claude_md_hierarchy_walked` — full LINGXI.md walk completed.
pub const CLAUDE_MD_HIERARCHY_WALKED: &str = "tengu_memory_claude_md_hierarchy_walked";

pub(crate) const NAMES: &[&str] = &[
    LOADED,
    LOAD_FAILED,
    CASE_MISMATCH,
    FILE_TOO_LARGE,
    SECRET_REDACTED,
    AGE_PENALTY_APPLIED,
    DROPPED_FOR_AGE,
    RANK_COMPUTED,
    TEAM_SCAN_STARTED,
    TEAM_SCAN_COMPLETED,
    TEAM_SCAN_FAILED,
    CLAUDE_MD_HIERARCHY_WALKED,
];

/// Payload for [`LOADED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadedPayload {
    /// Number of LINGXI.md (or local override) files resident after load.
    pub files_loaded: u32,
    /// Total bytes resident after load.
    pub total_bytes: u64,
    /// Wall-clock duration of the load pass in milliseconds.
    pub duration_ms: u64,
}

/// Payload for [`LOAD_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadFailedPayload {
    /// Whitelisted error kind (no raw IO details).
    pub error: Verified,
}

/// Payload for [`CASE_MISMATCH`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseMismatchPayload {
    /// The actual filename observed on disk (e.g. `claude.md`).
    pub actual_name: Verified,
    /// PII-tagged: full filesystem path.
    pub path: PiiTagged,
}

/// Payload for [`FILE_TOO_LARGE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileTooLargePayload {
    /// PII-tagged: full filesystem path of the rejected file.
    pub path: PiiTagged,
    /// Observed size in bytes.
    pub size_bytes: u64,
    /// Cap that was exceeded (in bytes).
    pub limit_bytes: u64,
}

/// Payload for [`SECRET_REDACTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRedactedPayload {
    /// PII-tagged: full filesystem path where the secret was found.
    pub path: PiiTagged,
    /// Number of redactions applied to this file in this pass.
    pub redactions: u32,
}

/// Payload for [`AGE_PENALTY_APPLIED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgePenaltyAppliedPayload {
    /// PII-tagged: full filesystem path of the penalised entry.
    pub path: PiiTagged,
    /// Age in days at the time of ranking.
    pub age_days: u64,
    /// Basis-points penalty applied (e.g. 1000 = 10% relevance reduction).
    pub penalty_bps: u64,
}

/// Payload for [`DROPPED_FOR_AGE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DroppedForAgePayload {
    /// PII-tagged: full filesystem path of the dropped entry.
    pub path: PiiTagged,
    /// Age in days at the time of drop.
    pub age_days: u64,
    /// Drop threshold (in days).
    pub threshold_days: u64,
}

/// Payload for [`RANK_COMPUTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankComputedPayload {
    /// PII-tagged: full filesystem path of the ranked entry.
    pub path: PiiTagged,
    /// Final rank in basis points (0-10000).
    pub rank_bps: u64,
}

/// Payload for [`TEAM_SCAN_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamScanStartedPayload {
    /// PII-tagged: root directory the scan started from.
    pub root: PiiTagged,
}

/// Payload for [`TEAM_SCAN_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamScanCompletedPayload {
    /// Number of files visited during the scan.
    pub files_scanned: u32,
    /// Wall-clock duration of the scan in milliseconds.
    pub duration_ms: u64,
}

/// Payload for [`TEAM_SCAN_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamScanFailedPayload {
    /// Whitelisted error kind.
    pub error: Verified,
}

/// Payload for [`CLAUDE_MD_HIERARCHY_WALKED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LingxiMdHierarchyWalkedPayload {
    /// Maximum walk depth reached.
    pub depth: u32,
    /// Total LINGXI.md (or local override) files visited.
    pub files_visited: u32,
}
