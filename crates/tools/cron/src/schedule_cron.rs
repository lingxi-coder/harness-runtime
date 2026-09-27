//! `CronCreateTool` — schedule a prompt on a 5-field cron schedule and persist
//! the job into the project's single `<root>/.claude/scheduled_tasks.json` file.
//!
//! 1:1 parity rewrite of claude-code `CronCreateTool.ts`. The model supplies a
//! 5-field cron expression plus the prompt to enqueue at each fire time, with
//! optional `recurring` (default true) and `durable` (default false) flags.
//!
//! Wire identifiers:
//! - 5-field cron expressions only (6-field with seconds is rejected by the
//!   shared parser and surfaces as the generic "Expected 5 fields" message).
//! - `id = [0-9a-f]{8}` (`randomUUID().slice(0, 8)`).
//!
//! Persistence (1:1 with claude-code `cronTasks.ts`): a DURABLE job is appended
//! to the single project-relative file `<projectRoot>/.claude/scheduled_tasks.json`
//! shaped `{ "tasks": [ CronTask, … ] }` (camelCase fields, `createdAt` in epoch
//! **milliseconds**, NO `durable`/next-fire on disk — those are runtime-only).
//! A SESSION-ONLY job (`durable:false`) is NOT written to disk at all (matching
//! claude-code's separate in-memory session store and the tool's own
//! "Session-only (not written to disk …)" promise).
//!
//! The desktop tool registers the job with the live
//! [`cron::scheduler::CronScheduler`] immediately. Durable state remains
//! authoritative on disk; session-only state is owned by that scheduler.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use telemetry::pii::{PiiTagged, Verified};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{
    SCHEDULE_CRON_COMPLETED, SCHEDULE_CRON_FAILED, SCHEDULE_CRON_STARTED,
};
use telemetry::AnalyticsBus;

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext, ValidationError,
};

// -- Inlined 5-field cron parser (mirrors cron::schedule) -------------
//
// We mirror the M1.17 cron::schedule::{parse_cron, CronExpression,
// CronField, decompose} surface inline because the natural dependency
// `lingxi-tools → lingxi-cron` would form a workspace cycle through
// `lingxi-cron → lingxi-tasks → lingxi-agent → lingxi-memory →
//  lingxi-sidequery → lingxi-tools`. The parser and matcher are byte-for-byte
// equivalent; downstream parity is verified by the (out-of-cycle) M4-09
// fixture driver against the canonical lingxi_cron table.

/// A parsed cron field: the expanded, sorted, de-duplicated set of integer
/// values it matches. Mirrors the `number[]` that `expandField` returns in
/// claude-code `utils/cron.ts`.
#[derive(Debug, Clone)]
struct CronField(Vec<u32>);

/// Inclusive `[min, max]` value range for a cron field. Mirrors the
/// `FieldRange` rows of the `FIELD_RANGES` table in `utils/cron.ts`.
#[derive(Debug, Clone, Copy)]
struct FieldRange {
    min: u32,
    max: u32,
}

/// Per-field value ranges, indexed minute, hour, day-of-month, month,
/// day-of-week — 1:1 with `cron.ts` `FIELD_RANGES`. Day-of-week is `0..=6`
/// (Sunday = 0) with `7` accepted as a Sunday alias and normalized to `0`.
const FIELD_RANGES: [FieldRange; 5] = [
    FieldRange { min: 0, max: 59 }, // minute
    FieldRange { min: 0, max: 23 }, // hour
    FieldRange { min: 1, max: 31 }, // day-of-month
    FieldRange { min: 1, max: 12 }, // month
    FieldRange { min: 0, max: 6 },  // day-of-week (0=Sunday; 7 = Sunday alias)
];

#[derive(Debug, Clone)]
struct CronExpression {
    raw: String,
    minute: CronField,
    hour: CronField,
    dom: CronField,
    month: CronField,
    dow: CronField,
}

/// Expand one cron field into its sorted, de-duplicated set of matching values.
///
/// 1:1 port of `expandField` in claude-code `utils/cron.ts`. Each comma part is
/// independently one of: a wildcard / step (`*`, `*/N`), a range or stepped
/// range (`N-M`, `N-M/S`), or a single value (`N`). Mixed forms compose freely
/// in one field (e.g. `1-5,0`, `0-30/10,45`, `*/15,7`). Day-of-week `7` is
/// normalized to `0` (Sunday) — both as a bare value and at the high end of a
/// range. Any value outside the field's `[min, max]` range (with `7` allowed
/// for day-of-week) makes the whole field invalid. Returns `Err` — which maps
/// to TS `null` — on any unsupported or out-of-range form, so the caller emits
/// the generic "Invalid cron expression" message.
fn expand_field(field: &str, range: FieldRange) -> Result<CronField, String> {
    let FieldRange { min, max } = range;
    // Day-of-week is the only `0..=6` field; it accepts `7` as a Sunday alias.
    let is_dow = min == 0 && max == 6;
    let mut out: BTreeSet<u32> = BTreeSet::new();

    for part in field.split(',') {
        // wildcard or `*/N`  (regex `^\*(?:\/(\d+))?$`)
        if let Some(step) = match_star(part) {
            if step < 1 {
                return Err(format!("bad field: {field}"));
            }
            let mut i = min;
            while i <= max {
                out.insert(i);
                i = i.saturating_add(step);
            }
            continue;
        }

        // `N-M` or `N-M/S`  (regex `^(\d+)-(\d+)(?:\/(\d+))?$`)
        if let Some((lo, hi, step)) = match_range(part) {
            let eff_max = if is_dow { 7 } else { max };
            if lo > hi || step < 1 || lo < min || hi > eff_max {
                return Err(format!("bad field: {field}"));
            }
            let mut i = lo;
            while i <= hi {
                out.insert(if is_dow && i == 7 { 0 } else { i });
                i = i.saturating_add(step);
            }
            continue;
        }

        // plain `N`  (regex `^\d+$`)
        if let Some(mut n) = match_single(part) {
            if is_dow && n == 7 {
                n = 0;
            }
            if n < min || n > max {
                return Err(format!("bad field: {field}"));
            }
            out.insert(n);
            continue;
        }

        return Err(format!("bad field: {field}"));
    }

    if out.is_empty() {
        return Err(format!("bad field: {field}"));
    }
    Ok(CronField(out.into_iter().collect()))
}

/// Match `*` or `*/N`, returning the step (`1` for a bare `*`). Mirrors the
/// `^\*(?:\/(\d+))?$` branch of `expandField`.
fn match_star(part: &str) -> Option<u32> {
    if part == "*" {
        return Some(1);
    }
    let rest = part.strip_prefix("*/")?;
    if !all_digits(rest) {
        return None;
    }
    Some(rest.parse::<u32>().unwrap_or(u32::MAX))
}

/// Match `N-M` or `N-M/S`, returning `(lo, hi, step)` with `step` defaulting to
/// `1`. Mirrors the `^(\d+)-(\d+)(?:\/(\d+))?$` branch of `expandField`.
fn match_range(part: &str) -> Option<(u32, u32, u32)> {
    let (range_part, step) = match part.split_once('/') {
        Some((r, s)) => {
            if !all_digits(s) {
                return None;
            }
            (r, s.parse::<u32>().unwrap_or(u32::MAX))
        }
        None => (part, 1),
    };
    let (lo, hi) = range_part.split_once('-')?;
    if !all_digits(lo) || !all_digits(hi) {
        return None;
    }
    Some((lo.parse::<u32>().ok()?, hi.parse::<u32>().ok()?, step))
}

/// Match a bare `N` (regex `^\d+$`).
fn match_single(part: &str) -> Option<u32> {
    if all_digits(part) {
        part.parse::<u32>().ok()
    } else {
        None
    }
}

fn parse_cron(s: &str) -> Result<CronExpression, String> {
    let parts: Vec<&str> = s
        .split(cron::schedule::is_cron_whitespace)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.len() != 5 {
        return Err(format!("expected 5 fields, got {}", parts.len()));
    }
    Ok(CronExpression {
        raw: s.to_owned(),
        minute: expand_field(parts[0], FIELD_RANGES[0])?,
        hour: expand_field(parts[1], FIELD_RANGES[1])?,
        dom: expand_field(parts[2], FIELD_RANGES[2])?,
        month: expand_field(parts[3], FIELD_RANGES[3])?,
        dow: expand_field(parts[4], FIELD_RANGES[4])?,
    })
}

fn field_match(field: &CronField, value: u32) -> bool {
    field.0.contains(&value)
}

/// Decompose unix seconds into (year, month 1-12, day 1-31, hour, minute,
/// dow 0-6 Sun=0), UTC, on the real proleptic Gregorian calendar. Mirrors
/// `cron::schedule::decompose` — Howard Hinnant's integer-only `civil_from_days`,
/// so real month lengths (incl. leap-year Feb 29 and 31-day months) are honored.
/// This fixes the old fixed-30-day approximation under which day-of-month 31
/// could never match. Pure UTC core — [`CronExpression::matches`] shifts the
/// instant by [`local_offset_seconds`] first, so the `next_fire_unix_secs`
/// preview is evaluated in LOCAL time, consistent with the live `cron`
/// scheduler's local firing (claude-code `cron.ts` is local).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
fn decompose(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let minute = (secs / 60 % 60) as u32;
    let hour = (secs / 3600 % 24) as u32;
    let days = (secs / 86_400) as i64; // days since 1970-01-01 (a Thursday)
    let dow = ((days % 7 + 4) % 7) as u32; // 1970-01-01 = Thursday (4)

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = (y + i64::from(month <= 2)) as u32;

    (year, month, day, hour, minute, dow)
}

impl CronExpression {
    fn matches(&self, time: SystemTime) -> bool {
        let secs = time
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // LOCAL-time evaluation (claude-code parity + consistency with the live
        // `cron` scheduler): shift by the system UTC offset, then decompose.
        #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
        let local = (secs as i64)
            .saturating_add(cron::schedule::local_offset_seconds(secs as i64))
            .max(0) as u64;
        let (_, month, day, hour, minute, dow) = decompose(local);
        // Standard cron day rule (claude-code `cron.ts`): when BOTH day-of-month
        // and day-of-week are restricted, the day matches if EITHER matches; when
        // one is `*` (its expanded set covers the whole domain — len 31 / 7), only
        // the other constrains.
        let dom_wild = self.dom.0.len() == 31;
        let dow_wild = self.dow.0.len() == 7;
        let day_matches = match (dom_wild, dow_wild) {
            (true, true) => true,
            (false, true) => field_match(&self.dom, day),
            (true, false) => field_match(&self.dow, dow),
            (false, false) => field_match(&self.dom, day) || field_match(&self.dow, dow),
        };
        field_match(&self.minute, minute)
            && field_match(&self.hour, hour)
            && field_match(&self.month, month)
            && day_matches
    }
}

/// Tool name byte-lock.
pub const CRON_CREATE_TOOL_NAME: &str = "CronCreate";
/// Legacy per-job subdirectory under `~/.lingxi/` (retained as a wire-identifier
/// lock; the durable path is now the single project file
/// [`cron::tasks_file::SCHEDULED_TASKS_FILE`]).
pub const CRON_SUBDIR: &str = "cron";
/// Legacy per-job file extension (retained as a wire-identifier lock).
pub const CRON_FILE_SUFFIX: &str = ".json";
/// 6-field rejection message (retained for the wire-identifier parity lock).
pub const SIX_FIELD_REJECTION: &str =
    "ScheduleCron: 6-field cron (with seconds) is not supported; use 5-field cron (minute hour day month weekday)";
/// Legacy iteration-count constant; upstream counts calendar jumps, not minutes.
pub const NEXT_FIRE_HORIZON_MINUTES: u64 = 60 * 24 * 366;
/// Maximum number of scheduled jobs allowed at once (CronCreateTool.ts:25).
const MAX_JOBS: usize = 50;

/// Absolute path to the single project tasks file
/// (`<project_root>/.claude/scheduled_tasks.json`). 1:1 with claude-code, which
/// keys cron persistence off the project root (the session cwd), NOT the user
/// config-home. Thin re-export of [`cron::tasks_file::session_scheduled_tasks_path`] so
/// every cron tool resolves the path identically.
#[must_use]
pub(crate) fn cron_file_path(project_root: &Path) -> PathBuf {
    cron::tasks_file::session_scheduled_tasks_path(project_root)
}

/// Generate a fresh eight-character lowercase hexadecimal cron id, matching
/// Claude Code 2.1.217's `randomUUID().slice(0, 8)`. Exposed so the mobile FFI
/// `cron_create` path mints ids in the
/// SAME format as the `CronCreate` tool (no on-disk drift between a UI-created
/// and a model-created job).
#[must_use]
pub fn generate_cron_task_id() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"0123456789abcdef";
    let mut rng = rand::rng();
    let mut s = String::with_capacity(8);
    for _ in 0..8 {
        let idx = rng.random_range(0..ALPHABET.len());
        s.push(ALPHABET[idx] as char);
    }
    s
}

/// Compute the next matching local calendar time, using the shared upstream
/// calendar-advance iteration limit.
fn next_fire_after(expr: &CronExpression, from: SystemTime) -> Option<SystemTime> {
    // Share the calendar-jumping search with the scheduler: the upstream
    // iteration limit is not a one-year time horizon (e.g. leap days).
    let next = cron::schedule::parse_cron(&expr.raw)
        .ok()?
        .next_match_after(from)?;
    debug_assert!(expr.matches(next));
    Some(next)
}

/// Reject 6-field expressions (Quartz-style with seconds). Retained from the
/// previous `ScheduleCron` seam for the wire-identifier parity lock; the live
/// `validate_input`/`call` paths now rely on the shared 5-field parser, which
/// surfaces 6-field input as the generic "Expected 5 fields" message (matching
/// `parseCronExpression` returning null in CronCreateTool.ts).
#[allow(dead_code)]
fn reject_six_field(expr: &str) -> Result<(), ToolError> {
    let parts: usize = expr.split_whitespace().count();
    if parts == 6 {
        return Err(ToolError::InvalidInput(SIX_FIELD_REJECTION.into()));
    }
    Ok(())
}

// -- cronToHuman ------------------------------------------------------------
// Port of `cronToHuman` from claude-code utils/cron.ts. Narrow by design:
// covers the common patterns and falls through to the raw cron string for
// anything else (matches the TS `return cron`).

/// `true` if `s` is one or more ASCII digits (mirrors the TS `/^\d+$/`).
fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Render a 5-field cron expression as a human-readable schedule string
/// (claude-code `K_`). Shared with the scheduler's missed-task prompt.
#[must_use]
pub fn cron_to_human(cron: &str) -> String {
    cron::human_schedule(cron)
}

/// Match the upstream boolean preprocessor: only exact "true"/"false" strings
/// become booleans. Schema validation rejects all other supplied non-booleans.
fn semantic_bool(v: Option<&Value>, default: bool) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => match s.as_str() {
            "true" => true,
            "false" => false,
            _ => default,
        },
        _ => default,
    }
}

/// Build the model-facing result text (2.1.263 `mapToolResultToToolResultBlockParam`).
/// Every host that registers this tool fires jobs: desktop through the live
/// `CronScheduler`, iOS/Android through their OS wake + `run_cron_task_if_due`.
fn build_result_content(id: &str, human: &str, recurring: bool, durable: bool) -> String {
    let where_ = if durable {
        "Persisted to .claude/scheduled_tasks.json"
    } else {
        "Session-only (not written to disk, dies when Claude exits)"
    };
    if recurring {
        // Claude Code 2.1.270 model-facing result, including its fixed seven-day limit.
        format!("Scheduled recurring job {id} ({human}). {where_}. Auto-expires after 7 days. Use CronDelete to cancel sooner.")
    } else {
        format!(
            "Scheduled one-shot task {id} ({human}). {where_}. It will fire once then auto-delete."
        )
    }
}

fn schedulable_job_count(doc: &cron::ScheduledTasks) -> usize {
    doc.tasks
        .iter()
        .filter(|task| {
            task.automation
                .as_ref()
                .is_none_or(|config| config.status != cron::AutomationStatus::Completed)
        })
        .count()
}

/// Count persisted jobs in the project's single `scheduled_tasks.json`
/// excluding completed tasks. A missing/garbage file counts as zero. Backs the
/// `MAX_JOBS = 50` limit (CronCreateTool.ts:25).
async fn count_existing_jobs(ctx: &tool_api::BuiltinToolContext) -> usize {
    let durable =
        match cron::tasks_file::read_tasks_body(ctx.fs.as_ref(), &ctx.session_cwd.project_root())
            .await
        {
            Ok(body) => schedulable_job_count(&cron::tasks_file::parse_tasks(&body)),
            Err(_) => 0,
        };
    let session = match &ctx.task_registry {
        Some(registry) => cron::session_jobs(registry)
            .await
            .map_or(0, |jobs| jobs.len()),
        None => 0,
    };
    durable + session
}

fn teammate_owner(ctx: &ToolUseContext) -> Option<String> {
    ctx.agent_name.as_ref().map(|name| {
        ctx.agent_id
            .map_or_else(|| name.clone(), |agent_id| agent_id.to_string())
    })
}

/// `CronCreateTool` — schedule a prompt on a 5-field cron + persist the job.
pub struct CronCreateTool {
    pub(crate) ctx: tool_api::BuiltinToolContext,
}

impl CronCreateTool {
    /// Construct.
    #[must_use]
    pub fn new(ctx: tool_api::BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

static SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "cron": {
                "type": "string",
                "description": "Standard 5-field cron expression in local time: \"M H DoM Mon DoW\" (e.g. \"*/5 * * * *\" = every 5 minutes, \"30 14 28 2 *\" = Feb 28 at 2:30pm local once)."
            },
            "prompt": {
                "type": "string",
                "description": "The prompt to enqueue at each fire time."
            },
            "recurring": {
                "type": "boolean",
                "description": "true (default) = fire on every cron match until deleted or auto-expired after 7 days. false = fire once at the next match, then auto-delete. Use false for \"remind me at X\" one-shot requests with pinned minute/hour/dom/month."
            },
            "durable": {
                "type": "boolean",
                "description": "true = persist to .claude/scheduled_tasks.json and survive restarts. false (default) = in-memory only, dies when this Claude session ends. Use true only when the user asks the task to survive across sessions."
            }
        },
        "required": ["cron", "prompt"]
    })
});

pub(crate) fn durable_enabled() -> bool {
    telemetry::flag_bool("tengu_kairos_cron_durable", true)
}

static SESSION_ONLY_SCHEMA: Lazy<Value> = Lazy::new(|| {
    let mut schema = SCHEMA.clone();
    schema["properties"]["durable"]["description"] = json!("Has no effect — durable persistence is not available. All jobs are session-only (in-memory, gone when this Claude session ends).");
    schema
});

fn pii_str(s: &str) -> AnalyticsValue {
    AnalyticsValue::String(PiiTagged::assert_pii_tagged_column(s.to_string()).into_inner())
}

fn verified_str(s: &str) -> AnalyticsValue {
    AnalyticsValue::String(Verified::assert_safe(s.to_string()).into_inner())
}

async fn emit_failed(bus: &Arc<AnalyticsBus>, kind: &str, duration_ms: u64) {
    let mut md: LogEventMetadata = HashMap::new();
    md.insert("error_kind".into(), verified_str(kind));
    md.insert(
        "duration_ms".into(),
        AnalyticsValue::Int(duration_ms as i64),
    );
    bus.log_event(SCHEDULE_CRON_FAILED, md).await;
}

/// PARITY 2.1.263 `QI()` = `H("tengu_amber_sentinel", false)` — the gate the
/// Monitor tool is behind, and the same gate that decides whether the
/// CronCreate prompt carries its "use Monitor instead" section.
///
/// The flag name is duplicated from `tools/task/src/monitor.rs` (`Monitor`'s
/// own `isEnabled`): `tool-cron` cannot depend on `tool-task`, and the port
/// keeps flag literals crate-local. The literal is pinned by a test below.
const AMBER_SENTINEL_FLAG: &str = "tengu_amber_sentinel";

fn amber_sentinel_enabled() -> bool {
    telemetry::flag_bool(AMBER_SENTINEL_FLAG, false)
}

/// PARITY 2.1.263 `pbn(true)` — the CronCreate tool prompt with the durable
/// gate on. Storage-path literals match the upstream tool store. The Monitor
/// section rides the same `QI()` gate the binary puts it behind, so it appears
/// exactly when the Monitor tool itself is available. The 7-day paragraph is
/// emitted only while the scheduler's recurring max age is set (see
/// `cron::default_recurring_max_age`), so the model is never promised an expiry
/// the scheduler does not enforce.
fn build_prompt_text() -> String {
    let durability = "## Durability\n\nBy default (durable: false) the job lives only in this Claude session — nothing is written to disk, and the job is gone when Claude exits. Pass durable: true to write to .claude/scheduled_tasks.json so the job survives restarts. Only use durable: true when the user explicitly asks for the task to persist (\"keep doing this every day\", \"set this up permanently\"). Most \"remind me in 5 minutes\" / \"check back in an hour\" requests should stay session-only.";
    let durability = if durable_enabled() {
        durability
    } else {
        "## Session-only\n\nJobs live only in this Claude session — nothing is written to disk, and the job is gone when Claude exits."
    };
    let durable_runtime = "Durable jobs persist to .claude/scheduled_tasks.json and survive session restarts — on next launch they resume automatically. One-shot durable tasks that were missed while the REPL was closed are surfaced for catch-up. Session-only jobs die with the process. ";
    let durable_runtime = if durable_enabled() {
        durable_runtime
    } else {
        ""
    };
    // `${o}\n${QI()?`\n## Not for live watching\n\n…\n`:""}\n## Runtime behavior`
    // — the empty arm collapses to the single blank line the gate-off prompt has.
    let monitor = if amber_sentinel_enabled() {
        "\n## Not for live watching\n\nCronCreate re-runs a prompt at fixed wall-clock intervals. To watch a log file, process, or command output and be notified the moment something changes, use the Monitor tool instead — Monitor streams events as they happen; cron polls on a schedule.\n"
    } else {
        ""
    };
    let expiry = cron::default_recurring_max_age().map_or_else(String::new, |age| {
        let days = age.as_secs() / 86_400;
        format!("Recurring tasks auto-expire after {days} days — they fire one final time, then are deleted. This bounds session lifetime. Tell the user about the {days}-day limit when scheduling recurring jobs.\n\n")
    });
    format!(
        "Schedule a prompt to be enqueued at a future time. Use for both recurring schedules and one-shot reminders.\n\nUses standard 5-field cron in the user's local timezone: minute hour day-of-month month day-of-week. \"0 9 * * *\" means 9am local — no timezone conversion needed.\n\n## One-shot tasks (recurring: false)\n\nFor \"remind me at X\" or \"at <time>, do Y\" requests — fire once then auto-delete.\nPin minute/hour/day-of-month/month to specific values:\n  \"remind me at 2:30pm today to check the deploy\" → cron: \"30 14 <today_dom> <today_month> *\", recurring: false\n  \"tomorrow morning, run the smoke test\" → cron: \"57 8 <tomorrow_dom> <tomorrow_month> *\", recurring: false\n\n## Recurring jobs (recurring: true, the default)\n\nFor \"every N minutes\" / \"every hour\" / \"weekdays at 9am\" requests:\n  \"*/5 * * * *\" (every 5 min), \"0 * * * *\" (hourly), \"0 9 * * 1-5\" (weekdays at 9am local)\n\n## Avoid the :00 and :30 minute marks when the task allows it\n\nEvery user who asks for \"9am\" gets `0 9`, and every user who asks for \"hourly\" gets `0 *` — which means requests from across the planet land on the API at the same instant. When the user's request is approximate, pick a minute that is NOT 0 or 30:\n  \"every morning around 9\" → \"57 8 * * *\" or \"3 9 * * *\" (not \"0 9 * * *\")\n  \"hourly\" → \"7 * * * *\" (not \"0 * * * *\")\n  \"in an hour or so, remind me to...\" → pick whatever minute you land on, don't round\n\nOnly use minute 0 or 30 when the user names that exact time and clearly means it (\"at 9:00 sharp\", \"at half past\", coordinating with a meeting). When in doubt, nudge a few minutes early or late — the user will not notice, and the fleet will.\n\n{durability}\n{monitor}\n## Runtime behavior\n\nJobs only fire while the REPL is idle (not mid-query). {durable_runtime}The scheduler adds a small deterministic jitter on top of whatever you pick: recurring tasks fire up to 10% of their period late (max 15 min); one-shot tasks landing on :00 or :30 fire up to 90 s early. Picking an off-minute is still the bigger lever.\n\n{expiry}Returns a job ID you can pass to CronDelete."
    )
}

#[async_trait]
impl Tool for CronCreateTool {
    fn name(&self) -> &str {
        CRON_CREATE_TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` — the def that validates a 5-field
    /// cron expression (byte-verified against the binary).
    fn search_hint(&self) -> Option<&str> {
        Some("schedule a recurring or one-shot prompt")
    }
    fn native_input_validation(&self) -> bool {
        true
    }
    fn input_schema(&self) -> &Value {
        if durable_enabled() {
            &SCHEMA
        } else {
            &SESSION_ONLY_SCHEMA
        }
    }
    fn coerce_input(&self, input: &Value) -> Option<tool_api::tool_trait::CoercedInput> {
        let mut updated = input.clone();
        let mut keys = Vec::new();
        for key in ["recurring", "durable"] {
            let value = match input.get(key).and_then(Value::as_str) {
                Some("true") => true,
                Some("false") => false,
                _ => continue,
            };
            updated[key] = json!(value);
            keys.push(key);
        }
        (!keys.is_empty()).then(|| tool_api::tool_trait::CoercedInput {
            input: updated,
            shape_class: keys.join(","),
        })
    }

    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        // PARITY 2.1.263 `EC()`: `!CLAUDE_CODE_DISABLE_CRON && gate(tengu_kairos_cron, true)`
        // — the env kill switch hides the tool from the model.
        crate::cron_tools_enabled()
    }
    fn max_result_size_chars(&self) -> usize {
        100_000
    }
    fn should_defer(&self) -> bool {
        true
    }
    fn get_path(&self, _: &Value) -> Option<std::path::PathBuf> {
        Some(cron::tasks_file::session_scheduled_tasks_path(
            &self.ctx.session_cwd.project_root(),
        ))
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn is_destructive(&self, _: &Value) -> bool {
        false
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "CronCreate persists a cron job to .claude/scheduled_tasks.json".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        if !durable_enabled() {
            return "Schedule a prompt to run at a future time within this Claude session — either recurring on a cron schedule, or once at a specific time.".into();
        }
        // PARITY 2.1.270 durable gate on.
        "Schedule a prompt to run at a future time — either recurring on a cron schedule, or once at a specific time. Pass durable: true to persist to .claude/scheduled_tasks.json; otherwise session-only.".into()
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        build_prompt_text()
    }

    async fn validate_input(
        &self,
        input: &Value,
        call_ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let cron = input
            .get("cron")
            .and_then(Value::as_str)
            .ok_or_else(|| ValidationError("CronCreate: missing or non-string cron".into()))?;

        // Invalid cron (not 5 fields / parse fail).
        let parsed = match parse_cron(cron) {
            Ok(p) => p,
            Err(_) => {
                return Err(ValidationError(format!(
                    "Invalid cron expression '{cron}'. Expected 5 fields: M H DoM Mon DoW."
                )));
            }
        };

        // No calendar date in the next year satisfies the expression.
        let now = self.ctx.clock.now();
        if next_fire_after(&parsed, now).is_none() {
            return Err(ValidationError(format!(
                "Cron expression '{cron}' does not match any calendar date in the next year."
            )));
        }

        // Too many scheduled jobs already (durable + this session's in-memory
        // jobs, exactly the set CronList exposes).
        if count_existing_jobs(&self.ctx).await >= MAX_JOBS {
            return Err(ValidationError(format!(
                "Too many scheduled jobs (max {MAX_JOBS}). Cancel one first."
            )));
        }

        // Only refuse a durability the tool would actually honour: with the gate
        // off the schema tells the caller `durable` "has no effect", so refusing
        // a teammate for sending it denies a job that would have been created.
        if semantic_bool(input.get("durable"), false)
            && durable_enabled()
            && teammate_owner(call_ctx).is_some()
        {
            return Err(ValidationError(
                "durable crons are not supported for teammates (teammates do not persist across sessions)".into(),
            ));
        }
        Ok(())
    }

    async fn call(
        &self,
        input: Value,
        call_ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let bus = self.ctx.bus.clone();

        let cron = match input.get("cron").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                emit_failed(&bus, "missing_cron", started.elapsed().as_millis() as u64).await;
                return Err(ToolError::InvalidInput(
                    "CronCreate: missing or non-string cron".into(),
                ));
            }
        };
        let prompt = match input.get("prompt").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                emit_failed(&bus, "missing_prompt", started.elapsed().as_millis() as u64).await;
                return Err(ToolError::InvalidInput(
                    "CronCreate: missing or non-string prompt".into(),
                ));
            }
        };

        let parsed = match parse_cron(&cron) {
            Ok(c) => c,
            Err(_) => {
                emit_failed(&bus, "parse_error", started.elapsed().as_millis() as u64).await;
                return Err(ToolError::InvalidInput(format!(
                    "Invalid cron expression '{cron}'. Expected 5 fields: M H DoM Mon DoW."
                )));
            }
        };

        let recurring = semantic_bool(input.get("recurring"), true);
        let durable = semantic_bool(input.get("durable"), false);
        let owner = teammate_owner(&call_ctx);
        if durable && durable_enabled() && owner.is_some() {
            return Err(ToolError::InvalidInput(
                "durable crons are not supported for teammates (teammates do not persist across sessions)".into(),
            ));
        }

        let durable = durable && durable_enabled();
        let mut md: LogEventMetadata = HashMap::new();
        md.insert("_PROTO_cron".into(), pii_str(&cron));
        md.insert("_PROTO_prompt".into(), pii_str(&prompt));
        bus.log_event(SCHEDULE_CRON_STARTED, md).await;

        let now = self.ctx.clock.now();
        // Next-fire is COMPUTED at runtime (cron + createdAt/lastFiredAt) and is
        // never persisted (claude-code parity); we still derive it here purely
        // for the analytics `next_fire_unix_secs` signal.
        let next = next_fire_after(&parsed, now);
        let next_unix = next
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        // `createdAt` is epoch MILLISECONDS on disk (claude-code `Date.now()`).
        let created_ms = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let id = generate_cron_task_id();

        // Persist ONLY when durable. Session-only jobs are inserted into the
        // live scheduler's session-owned store below.
        let mut bytes_written: usize = 0;
        if durable {
            let path = cron_file_path(&self.ctx.session_cwd.project_root());
            // Serialize in-process contenders before taking the blocking OS
            // lock, then hold both locks across the entire read-modify-write.
            let _process_guard = cron::lock_cron_file().await;
            let _file_guard = match cron::tasks_file::lock_scheduled_tasks(
                self.ctx.fs.as_ref(),
                &self.ctx.session_cwd.project_root(),
            )
            .await
            {
                Ok(guard) => guard,
                Err(e) => {
                    emit_failed(&bus, "io_lock", started.elapsed().as_millis() as u64).await;
                    return Err(ToolError::Io(format!(
                        "CronCreate: io error at {}: {e}",
                        path.display()
                    )));
                }
            };
            // Read-modify-write the single `{ "tasks": [...] }` document: load the
            // existing tasks (empty if absent/garbage), append the new CronTask,
            // and write the whole file back.
            let mut doc = match cron::tasks_file::read_tasks_body(
                self.ctx.fs.as_ref(),
                &self.ctx.session_cwd.project_root(),
            )
            .await
            {
                Ok(body) => cron::tasks_file::parse_tasks(&body),
                // ONLY a genuinely absent file starts a fresh document. Any other
                // read error (EIO, EACCES, the rooted-fs symlink rejection) is not
                // evidence that there are no tasks, and starting from `default()`
                // would write the new task over every existing one.
                Err(platform_api::FsError::NotFound(_)) => {
                    cron::tasks_file::ScheduledTasks::default()
                }
                Err(e) => {
                    emit_failed(&bus, "io_read", started.elapsed().as_millis() as u64).await;
                    return Err(ToolError::Io(format!(
                        "Failed to read scheduled tasks: {e}"
                    )));
                }
            };
            // Re-check under the lock; validate_input's early check is only a
            // UX fast path and cannot enforce the cap against concurrent writers.
            if schedulable_job_count(&doc) >= MAX_JOBS {
                return Err(ToolError::InvalidInput(format!(
                    "Too many scheduled jobs (max {MAX_JOBS}). Cancel one first."
                )));
            }
            doc.tasks.push(cron::tasks_file::CronTask {
                creator: cron::tasks_file::CronTaskCreator {
                    created_by_session_id: call_ctx
                        .origin_session_id
                        .as_ref()
                        .or(self.ctx.session_id.as_ref())
                        // The scheduler compares this against the BARE uuid every
                        // host feeds `set_session_id`; `SessionId`'s Display adds a
                        // `sess:` prefix, which would never match.
                        .map(|id| id.as_uuid().to_string()),
                    created_by_pid: Some(std::process::id()),
                    created_by_proc_start: platform_api::live_sessions::process_start_identity(
                        std::process::id(),
                    ),
                },
                automation: None,
                id: id.clone(),
                cron: cron.clone(),
                prompt: prompt.clone(),
                created_at: created_ms,
                last_fired_at: None,
                // PARITY 2.1.263 `nCe`: `...r&&{recurring:!0}` — the key is
                // written only when true.
                recurring: recurring.then_some(true),
                permanent: None,
                expires_at: None,
                session_id: None,
            });
            let body = cron::tasks_file::serialize_tasks(&doc);
            bytes_written = body.len();
            if let Err(e) = cron::tasks_file::write_tasks_body(
                self.ctx.fs.as_ref(),
                &self.ctx.session_cwd.project_root(),
                &body,
            )
            .await
            {
                emit_failed(&bus, "io_write", started.elapsed().as_millis() as u64).await;
                return Err(ToolError::Io(format!(
                    "CronCreate: io error at {}: {e}",
                    path.display()
                )));
            }
        }

        let live_task = cron::SessionCronTask {
            id: id.clone(),
            cron: cron.clone(),
            prompt: prompt.clone(),
            created_at: now,
            last_fired_at: None,
            recurring,
            owner,
        };
        if let Some(registry) = &self.ctx.task_registry {
            match cron::register_live_job(registry, live_task, durable).await {
                Ok(()) => {}
                Err(error) if durable => {
                    // The durable file is authoritative; a host without a live
                    // scheduler (mobile) fires it from its own OS wake.
                    tracing::warn!(%error, "durable cron saved; live scheduler unavailable");
                }
                Err(error) => {
                    emit_failed(&bus, "no_scheduler", started.elapsed().as_millis() as u64).await;
                    return Err(ToolError::Internal(format!(
                        "CronCreate: session-only job requires an active scheduler: {error}"
                    )));
                }
            }
        } else if !durable {
            emit_failed(&bus, "no_scheduler", started.elapsed().as_millis() as u64).await;
            return Err(ToolError::Internal(
                "CronCreate: session-only job requires an active scheduler".into(),
            ));
        };

        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "duration_ms".into(),
            AnalyticsValue::Int(started.elapsed().as_millis() as i64),
        );
        md.insert(
            "next_fire_unix_secs".into(),
            match next_unix {
                Some(n) => AnalyticsValue::Int(n as i64),
                None => AnalyticsValue::None,
            },
        );
        md.insert(
            "bytes_written".into(),
            AnalyticsValue::Int(bytes_written as i64),
        );
        bus.log_event(SCHEDULE_CRON_COMPLETED, md).await;

        let human = cron_to_human(&cron);
        let content = build_result_content(&id, &human, recurring, durable);

        Ok(ToolCallResult {
            data: json!({
                "id": id,
                "humanSchedule": human,
                "recurring": recurring,
                "durable": durable,
                "content": content,
            }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Latest 2.1.270 `aIn(true)` with Monitor disabled; no path normalization.
    #[test]
    fn prompt_matches_2_1_263_pbn() {
        let _serial = cron::autonomous_loop::TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        telemetry::test_clear_flag(AMBER_SENTINEL_FLAG);
        telemetry::test_clear_flag("tengu_kairos_cron_durable");
        let expected = include_str!("../tests/fixtures/cron_create_prompt_2_1_263.txt");
        let actual = build_prompt_text();
        assert_eq!(actual, expected);
    }

    /// PARITY `QI()` = `H("tengu_amber_sentinel", false)`: the Monitor section
    /// appears exactly when the Monitor tool does, and the gate-off prompt is
    /// byte-identical to the fixture (the `:""` arm leaves one blank line).
    ///
    /// Pins the flag literal too — `tool-cron` cannot import `tool-task`'s copy,
    /// so a rename there would otherwise silently decouple the two.
    #[test]
    fn the_monitor_section_rides_the_amber_sentinel_gate() {
        const SECTION: &str = "\n## Not for live watching\n\nCronCreate re-runs a prompt at fixed wall-clock intervals. To watch a log file, process, or command output and be notified the moment something changes, use the Monitor tool instead — Monitor streams events as they happen; cron polls on a schedule.\n";
        assert_eq!(AMBER_SENTINEL_FLAG, "tengu_amber_sentinel");

        let _serial = cron::autonomous_loop::TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        telemetry::test_clear_flag(AMBER_SENTINEL_FLAG);
        let off = build_prompt_text();
        assert!(!off.contains("## Not for live watching"));

        telemetry::test_set_flag(AMBER_SENTINEL_FLAG, true);
        let on = build_prompt_text();
        telemetry::test_clear_flag(AMBER_SENTINEL_FLAG);
        assert!(on.contains(SECTION), "{on}");
        assert_eq!(
            on.replace(SECTION, ""),
            off,
            "the section is the only difference the gate makes"
        );
        assert!(on.contains(&format!("{SECTION}\n## Runtime behavior")));
    }

    use platform_api::process::ProcessOutput;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx_in};

    fn dummy_out() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    /// Read the parsed `{ "tasks": [...] }` document at
    /// `<root>/.claude/scheduled_tasks.json` (empty if absent).
    async fn read_doc(root: &std::path::Path) -> cron::tasks_file::ScheduledTasks {
        let path = cron::tasks_file::session_scheduled_tasks_path(root);
        match tokio::fs::read_to_string(&path).await {
            Ok(body) => cron::tasks_file::parse_tasks(&body),
            Err(_) => cron::tasks_file::ScheduledTasks::default(),
        }
    }

    #[test]
    fn constants_locked() {
        assert_eq!(CRON_CREATE_TOOL_NAME, "CronCreate");
        assert_eq!(CRON_SUBDIR, "cron");
        assert_eq!(MAX_JOBS, 50);
    }

    #[test]
    fn task_id_format() {
        let id = generate_cron_task_id();
        assert_eq!(id.len(), 8);
        assert!(id
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()));
    }

    #[test]
    fn rejects_six_field_with_seconds() {
        let r = reject_six_field("0 */5 9-17 * * 1-5");
        let err = r.unwrap_err();
        assert!(format!("{err}").contains("6-field cron"));
    }

    #[test]
    fn latest_boolean_preprocess_accepts_only_exact_lowercase_strings() {
        let _serial = cron::autonomous_loop::TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        use platform_api::process::ProcessOutput;
        use tool_api::test_support::shell_test_ctx_in;
        let tool = CronCreateTool::new(shell_test_ctx_in(
            ProcessOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            },
            std::path::PathBuf::from("/tmp"),
        ));
        let coerced = tool
            .coerce_input(&json!({"recurring":"false", "durable":"true"}))
            .unwrap();
        assert_eq!(coerced.input, json!({"recurring":false, "durable":true}));
        for invalid in ["FALSE", "yes", "1", " true "] {
            assert!(tool.coerce_input(&json!({"recurring":invalid})).is_none());
        }
    }

    #[test]
    fn semantic_bool_parsing() {
        assert!(semantic_bool(None, true));
        assert!(!semantic_bool(None, false));
        assert!(semantic_bool(Some(&json!(true)), false));
        assert!(!semantic_bool(Some(&json!(false)), true));
        assert!(semantic_bool(Some(&json!("true")), false));
        assert!(!semantic_bool(Some(&json!("false")), true));
        for invalid in [
            json!("TRUE"),
            json!("yes"),
            json!("1"),
            json!(" true "),
            json!(5),
        ] {
            assert!(!semantic_bool(Some(&invalid), false));
        }
    }

    #[test]
    fn cron_to_human_cases() {
        assert_eq!(cron_to_human("*/5 * * * *"), "Every 5 minutes");
        assert_eq!(cron_to_human("*/1 * * * *"), "Every minute");
        assert_eq!(cron_to_human("0 * * * *"), "Every hour");
        assert_eq!(cron_to_human("30 * * * *"), "Every hour at :30");
        assert_eq!(cron_to_human("0 */2 * * *"), "Every 2 hours");
        assert_eq!(cron_to_human("0 9 * * *"), "Every day at 9:00 AM");
        assert_eq!(cron_to_human("0 12 * * *"), "Every day at 12:00 PM");
        assert_eq!(cron_to_human("30 14 * * 1"), "Every Monday at 2:30 PM");
        assert_eq!(cron_to_human("0 0 * * 0"), "Every Sunday at 12:00 AM");
        assert_eq!(cron_to_human("0 9 * * 1-5"), "Weekdays at 9:00 AM");
        assert_eq!(cron_to_human("30 14 28 2 *"), "30 14 28 2 *");
        assert_eq!(cron_to_human("0 9 15 * *"), "0 9 15 * *");
        // Unrecognized -> raw cron string.
        assert_eq!(cron_to_human("garbage"), "garbage");
        assert_eq!(cron_to_human("* * * * *"), "Every minute");
    }

    #[test]
    fn result_content_variants() {
        assert_eq!(
            build_result_content("d12345678", "Every day at 9:00 AM", true, false),
            "Scheduled recurring job d12345678 (Every day at 9:00 AM). Session-only (not written to disk, dies when Claude exits). Auto-expires after 7 days. Use CronDelete to cancel sooner."
        );
        assert_eq!(
            build_result_content("d87654321", "February 28 at 2:30pm", false, true),
            "Scheduled one-shot task d87654321 (February 28 at 2:30pm). Persisted to .claude/scheduled_tasks.json. It will fire once then auto-delete."
        );
    }

    #[tokio::test]
    async fn session_only_job_without_live_scheduler_fails_honestly() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = CronCreateTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let error = tool
            .call(
                json!({"cron": "*/5 9-17 * * 1-5", "prompt": "echo hi"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("must not claim an inert session cron was scheduled");
        assert!(format!("{error}").contains("requires an active scheduler"));

        // No file written at all.
        let path = cron::tasks_file::session_scheduled_tasks_path(tmp.path());
        assert!(!tokio::fs::try_exists(&path).await.unwrap());
        assert!(read_doc(tmp.path()).await.tasks.is_empty());
    }

    #[tokio::test]
    async fn durable_job_persists_to_single_project_file() {
        let _serial = cron::autonomous_loop::TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let ctx = shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf());
        let nested = tmp.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        ctx.session_cwd.swap(nested.clone(), vec![]);
        let tool = CronCreateTool::new(ctx);
        let mut call_ctx = fresh_ctx();
        let owner = protocol::SessionId::new();
        call_ctx.origin_session_id = Some(owner);
        let out = tool
            .call(
                json!({"cron": "*/5 9-17 * * 1-5", "prompt": "echo hi", "durable": true}),
                call_ctx,
                fresh_tx(),
            )
            .await
            .expect("ok");
        let id = out.data["id"].as_str().unwrap().to_string();

        // The single `{ "tasks": [...] }` file carries one CronTask with the
        // camelCase, epoch-MILLISECONDS shape — and NO durable / next-fire keys.
        let path = cron::tasks_file::session_scheduled_tasks_path(tmp.path());
        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(written.contains("\"tasks\""));
        assert!(written.contains("\"cron\": \"*/5 9-17 * * 1-5\""));
        assert!(written.contains("\"prompt\": \"echo hi\""));
        assert!(written.contains("\"createdAt\""));
        assert!(written.contains("\"recurring\": true"));
        assert!(!written.contains("durable"));
        assert!(!written.contains("nextFire"));
        assert!(!written.contains("next_fire"));
        assert!(!written.contains("created_at_unix"));
        // Trailing newline (claude-code `+ '\n'`).
        assert!(written.ends_with("}\n"));

        let doc = read_doc(tmp.path()).await;
        assert_eq!(doc.tasks.len(), 1);
        assert_eq!(doc.tasks[0].id, id);
        assert_eq!(doc.tasks[0].recurring, Some(true));
        assert_eq!(doc.tasks[0].last_fired_at, None);
        assert_eq!(doc.tasks[0].session_id, None);
        // The BARE uuid: the scheduler compares this against
        // `set_session_id(current.as_uuid().to_string())`, so a `sess:` prefix
        // would leave the job owned by nobody.
        assert_eq!(
            doc.tasks[0].creator.created_by_session_id.as_deref(),
            Some(owner.as_uuid().to_string().as_str())
        );
        assert!(!written.contains("\"sessionId\""));
        assert!(!cron::tasks_file::session_scheduled_tasks_path(&nested).exists());
        assert_eq!(doc.tasks[0].expires_at, None);
        // `createdAt` is derived from the clock in epoch MILLISECONDS. The test
        // StubClock is anchored at the Unix epoch, so this is 0 here — the ms
        // conversion (`as_millis`) is exercised by the scheduler round-trip tests
        // with a non-epoch clock.
        assert_eq!(doc.tasks[0].created_at, 0);
        assert!(written.contains("\"createdAt\": 0"));
    }

    #[tokio::test]
    async fn completed_history_does_not_consume_create_quota() {
        let _serial = cron::autonomous_loop::TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let ctx = shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf());
        let tasks: Vec<_> = (0..MAX_JOBS).map(|index| json!({
            "id": format!("done-{index}"), "cron": "* * * * *", "prompt": "done", "createdAt": 1,
            "automation": {"version": 2, "status": "completed", "model": "provider/model"}
        })).collect();
        let history_path = cron::tasks_file::scheduled_tasks_path(tmp.path());
        tokio::fs::create_dir_all(history_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&history_path, json!({"tasks":tasks}).to_string())
            .await
            .unwrap();
        assert_eq!(count_existing_jobs(&ctx).await, 0);
        let tool = CronCreateTool::new(ctx);
        tool.call(
            json!({"cron":"* * * * *", "prompt":"new", "durable":true}),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
        let doc = read_doc(tmp.path()).await;
        assert_eq!(doc.tasks.len(), 1);
        assert_eq!(schedulable_job_count(&doc), 1);
        let history = tokio::fs::read_to_string(cron::tasks_file::scheduled_tasks_path(tmp.path()))
            .await
            .unwrap();
        assert_eq!(
            cron::tasks_file::parse_tasks(&history).tasks.len(),
            MAX_JOBS,
            "task-center history stays in its own store"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn durable_job_refuses_symlinked_project_state_directory() {
        let _serial = cron::autonomous_loop::TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(victim.path(), tmp.path().join(".claude")).unwrap();
        let tool = CronCreateTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));

        let err = tool
            .call(
                json!({"cron": "*/5 * * * *", "prompt": "escape", "durable": true}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("symlinked state directory must fail closed");
        assert!(matches!(err, ToolError::Io(_)));
        assert!(
            !victim.path().join("scheduled_tasks.json").exists(),
            "the symlink target must remain untouched"
        );
    }

    #[tokio::test]
    async fn durable_creates_append_into_one_file() {
        let _serial = cron::autonomous_loop::TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Two durable creates accumulate in the SAME file (read-modify-write).
        let tmp = tempfile::tempdir().unwrap();
        let tool = CronCreateTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        for cron in ["*/5 * * * *", "0 9 * * *"] {
            tool.call(
                json!({"cron": cron, "prompt": "p", "durable": true}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        }
        let doc = read_doc(tmp.path()).await;
        assert_eq!(doc.tasks.len(), 2);
    }

    #[tokio::test]
    async fn concurrent_durable_creates_do_not_lose_updates() {
        let _serial = cron::autonomous_loop::TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let first = CronCreateTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let second = CronCreateTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));

        let first_call = first.call(
            json!({"cron": "*/5 * * * *", "prompt": "first", "durable": true}),
            fresh_ctx(),
            fresh_tx(),
        );
        let second_call = second.call(
            json!({"cron": "0 9 * * *", "prompt": "second", "durable": true}),
            fresh_ctx(),
            fresh_tx(),
        );
        let (first_result, second_result) = tokio::join!(first_call, second_call);
        first_result.expect("first concurrent create");
        second_result.expect("second concurrent create");

        let doc = read_doc(tmp.path()).await;
        assert_eq!(doc.tasks.len(), 2);
        let mut prompts = doc
            .tasks
            .iter()
            .map(|task| task.prompt.as_str())
            .collect::<Vec<_>>();
        prompts.sort_unstable();
        assert_eq!(prompts, ["first", "second"]);
    }

    #[tokio::test]
    async fn one_shot_durable_result() {
        let _serial = cron::autonomous_loop::TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let tool = CronCreateTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let out = tool
            .call(
                json!({"cron": "30 14 28 2 *", "prompt": "remind me", "recurring": false, "durable": true}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(out.data["recurring"], json!(false));
        assert_eq!(out.data["durable"], json!(true));
        assert_eq!(out.data["humanSchedule"], json!("30 14 28 2 *"));
        let content = out.data["content"].as_str().unwrap();
        assert!(content.contains("Scheduled one-shot task"));
        // A durable job is authoritative on disk whether or not this host runs a
        // live scheduler (mobile fires it from its own OS wake), so the result
        // is the oracle text.
        assert!(content.ends_with(
            "Persisted to .claude/scheduled_tasks.json. It will fire once then auto-delete."
        ));
        // PARITY 2.1.263 `nCe`: recurring:false ⇒ the `recurring` key is omitted
        // on disk (and the reader normalises a literal `false` to absent too).
        let doc = read_doc(tmp.path()).await;
        assert_eq!(doc.tasks.len(), 1);
        assert_eq!(doc.tasks[0].recurring, None);
        let raw =
            std::fs::read_to_string(cron::tasks_file::session_scheduled_tasks_path(tmp.path()))
                .unwrap();
        assert!(!raw.contains("\"recurring\""), "{raw}");
    }

    #[tokio::test]
    async fn semantic_string_flags_via_call() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = CronCreateTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let out = tool
            .call(
                json!({"cron": "0 9 * * *", "prompt": "x", "recurring": "false", "durable": "true"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(out.data["recurring"], json!(false));
        assert_eq!(out.data["durable"], json!(true));
    }

    #[tokio::test]
    async fn teammate_cannot_create_durable_job() {
        let _serial = cron::autonomous_loop::TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let tool = CronCreateTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let mut call_ctx = fresh_ctx();
        call_ctx.agent_name = Some("researcher".into());
        let input = json!({"cron": "0 9 * * *", "prompt": "x", "durable": true});

        let validation = tool.validate_input(&input, &call_ctx).await.unwrap_err();
        assert_eq!(
            validation.0,
            "durable crons are not supported for teammates (teammates do not persist across sessions)"
        );
        let error = tool
            .call(input, call_ctx, fresh_tx())
            .await
            .expect_err("call must enforce the boundary without pre-validation too");
        assert!(format!("{error}").contains("durable crons are not supported for teammates"));
        assert!(!cron::tasks_file::session_scheduled_tasks_path(tmp.path()).exists());
    }

    #[tokio::test]
    async fn call_rejects_garbage_cron() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = CronCreateTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let err = tool
            .call(
                json!({"cron": "not a cron", "prompt": "x"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("garbage");
        assert!(
            format!("{err}").contains("Invalid cron expression 'not a cron'. Expected 5 fields")
        );
    }

    #[tokio::test]
    async fn call_rejects_six_field_as_invalid() {
        // PARITY: 6-field is no longer a special message; the shared 5-field
        // parser surfaces it as the generic "Expected 5 fields" error (TS
        // `parseCronExpression` returns null for non-5-field input).
        let tmp = tempfile::tempdir().unwrap();
        let tool = CronCreateTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let err = tool
            .call(
                json!({"cron": "0 */5 9-17 * * 1-5", "prompt": "x"}),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("must reject");
        assert!(format!("{err}").contains("Invalid cron expression"));
        assert!(format!("{err}").contains("Expected 5 fields"));
    }

    #[tokio::test]
    async fn validate_rejects_garbage_cron() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = CronCreateTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let err = tool
            .validate_input(&json!({"cron": "not a cron", "prompt": "x"}), &fresh_ctx())
            .await
            .expect_err("garbage");
        assert!(err
            .0
            .contains("Invalid cron expression 'not a cron'. Expected 5 fields: M H DoM Mon DoW."));
    }

    // -- CRON.3: day-of-week 7 (Sunday alias) normalizes to 0 -----------------
    #[test]
    fn dow_seven_normalizes_to_sunday() {
        // Bare 7 -> 0 (both mean Sunday).
        let e = parse_cron("0 0 * * 7").expect("dow 7 accepted");
        assert_eq!(e.dow.0, vec![0]);
        // 7 at the top of a range expands with 7 folded to 0 (5-7 = Fri,Sat,Sun).
        let e = parse_cron("0 0 * * 5-7").expect("range ending at 7");
        assert_eq!(e.dow.0, vec![0, 5, 6]);
        // 0 and 7 collapse to a single Sunday entry.
        let e = parse_cron("0 0 * * 0,7").expect("0 and 7 dedupe");
        assert_eq!(e.dow.0, vec![0]);
        // 7 is a Sunday alias ONLY for day-of-week; elsewhere it is a plain
        // value and is NOT rewritten to 0.
        assert_eq!(parse_cron("7 0 * * *").unwrap().minute.0, vec![7]);
        assert_eq!(parse_cron("0 0 * 7 *").unwrap().month.0, vec![7]);
    }

    // -- CRON.4: mixed list / range / step forms in one field -----------------
    #[test]
    fn mixed_list_range_step_forms() {
        // List + range + single composed in the day-of-week field.
        let e = parse_cron("0 9 * * 1-5,0").expect("mixed dow");
        assert_eq!(e.dow.0, vec![0, 1, 2, 3, 4, 5]);
        // Stepped range plus an explicit single value.
        let e = parse_cron("0-30/10,45 * * * *").expect("stepped range + single");
        assert_eq!(e.minute.0, vec![0, 10, 20, 30, 45]);
        // Wildcard step combined with a single value (deduped + sorted).
        let e = parse_cron("*/15,7 * * * *").expect("wildcard step + single");
        assert_eq!(e.minute.0, vec![0, 7, 15, 30, 45]);
        // The previous parser rejected "1-5,0" (range branch swallowed the
        // comma); confirm the whole expression now parses end-to-end.
        assert!(parse_cron("30 14 * * 1-5,0").is_ok());
    }

    // -- CRON.2: per-field range validation -----------------------------------
    #[test]
    fn out_of_range_values_rejected() {
        // One past the top of each field's range.
        assert!(parse_cron("60 * * * *").is_err()); // minute max 59
        assert!(parse_cron("* 24 * * *").is_err()); // hour max 23
        assert!(parse_cron("* * 0 * *").is_err()); // dom min 1
        assert!(parse_cron("* * 32 * *").is_err()); // dom max 31
        assert!(parse_cron("* * * 0 *").is_err()); // month min 1
        assert!(parse_cron("* * * 13 *").is_err()); // month max 12
        assert!(parse_cron("* * * * 8").is_err()); // dow max 7 (alias), 8 invalid
                                                   // Out-of-range hidden inside a list / range is rejected too.
        assert!(parse_cron("0,60 * * * *").is_err());
        assert!(parse_cron("* * * * 5-8").is_err());
        // Boundary values (including the dow 7 alias) are accepted.
        assert!(parse_cron("59 23 31 12 7").is_ok());
    }

    #[tokio::test]
    async fn validate_rejects_out_of_range_with_invalid_message() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = CronCreateTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        // Minute 99 is out of range -> the parser fails -> the generic
        // byte-exact "Invalid cron expression" message (errorCode 1 in TS),
        // NOT the "does not match any calendar date" message (errorCode 2).
        let err = tool
            .validate_input(&json!({"cron": "99 * * * *", "prompt": "x"}), &fresh_ctx())
            .await
            .expect_err("out of range");
        assert_eq!(
            err.0,
            "Invalid cron expression '99 * * * *'. Expected 5 fields: M H DoM Mon DoW."
        );
    }

    #[tokio::test]
    async fn validate_rejects_unsatisfiable_cron() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = CronCreateTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        // February 30 is a real impossible date (Feb has at most 29 days), so no
        // calendar date in the horizon matches. (DOM 31 — which the old crude
        // 30-day decompose wrongly rejected — is now correctly satisfiable; see
        // `day_of_month_31_is_satisfiable`.)
        let err = tool
            .validate_input(&json!({"cron": "0 0 30 2 *", "prompt": "x"}), &fresh_ctx())
            .await
            .expect_err("unsatisfiable");
        assert!(err
            .0
            .contains("does not match any calendar date in the next year"));
        // NOTE: DOM 31 (which the old crude 30-day decompose wrongly rejected) is
        // now correctly satisfiable on the real calendar — proven directly against
        // the matcher in `cron::schedule::tests::day_of_month_31_can_match`. It
        // can't be asserted *here* because the test `StubClock` is anchored at the
        // Unix epoch and the matcher's `year > 1970` sentinel keeps the 366-day
        // horizon (all in 1970) from matching any narrow date.
    }
    #[tokio::test]
    async fn all_local_tool_prompt_branches_match_latest_oracle_without_normalization() {
        let _serial = cron::autonomous_loop::TEST_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cases: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/cron_contracts_2_1_270.json"
        ))
        .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let create = CronCreateTool::new(shell_test_ctx_in(dummy_out(), tmp.path().to_path_buf()));
        let delete = crate::cron_delete::CronDeleteTool::new(shell_test_ctx_in(
            dummy_out(),
            tmp.path().to_path_buf(),
        ));
        let list = crate::cron_list::CronListTool::new(shell_test_ctx_in(
            dummy_out(),
            tmp.path().to_path_buf(),
        ));
        for case in cases.as_array().unwrap() {
            telemetry::test_set_flag(
                "tengu_kairos_cron_durable",
                case["durable"].as_bool().unwrap(),
            );
            telemetry::test_set_flag(AMBER_SENTINEL_FLAG, case["monitor"].as_bool().unwrap());
            assert_eq!(build_prompt_text(), case["create"].as_str().unwrap());
            assert_eq!(
                create
                    .description(
                        &json!({}),
                        &DescriptionOptions {
                            is_non_interactive_session: false
                        }
                    )
                    .await,
                case["description"].as_str().unwrap()
            );
            assert_eq!(
                create.input_schema()["properties"]["durable"]["description"],
                case["durableDescription"]
            );
            assert_eq!(
                delete.prompt(&PromptOptions::default()).await,
                case["delete"].as_str().unwrap()
            );
            assert_eq!(
                list.prompt(&PromptOptions::default()).await,
                case["list"].as_str().unwrap()
            );
        }
        telemetry::test_clear_flag(AMBER_SENTINEL_FLAG);
        telemetry::test_clear_flag("tengu_kairos_cron_durable");
    }
}
