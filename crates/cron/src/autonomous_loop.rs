//! Autonomous-loop subsystem — 1:1 port of the 2.1.191 binary's `T3e` module
//! (`mt(T3e,{resolveLoopFileFire,resolveLoopDefaultFire,resolveAutonomousLoopFire,
//! resetAutonomousLoopDelivered,readLoopFile,…})`, cc_all.txt:504950).
//!
//! The `/loop` dynamic / autonomous modes pass a *sentinel* string back through
//! `ScheduleWakeup`'s `prompt` arg (or a CronCreate prompt). At fire time the
//! runtime resolves the sentinel → the actual tick prompt:
//!   - first delivery: the full autonomous-loop PREAMBLE + the tick instructions
//!   - subsequent deliveries: a short reminder tick only ("the long instructions
//!     stay in the cached message-prefix").
//!
//! Sentinels (binary literals):
//!   - `<<autonomous-loop>>`         (`Wst`) — CronCreate-based autonomous loop
//!   - `<<autonomous-loop-dynamic>>` (`Jke`) — ScheduleWakeup self-paced loop
//!   - `<<loop.md>>`                 (`rKi`) — CronCreate loop.md-tasks loop
//!   - `<<loop.md-dynamic>>`         (`lFt`) — ScheduleWakeup loop.md-tasks loop
//!
//! In 2.1.270, delivery state belongs to the session (`e` in
//! `src_186131770.js`'s `se(e,t)`). [`LoopRuntime`] owns that state so each
//! transcript gets its own initial instructions. Process-global compatibility
//! wrappers remain for isolated callers.
//!
//! FEATURE FLAGS: every gate reads the binary's sync flag reader `nt(key,default)`
//! — ported as [`telemetry::flag_bool`] (an empty cached snapshot by default →
//! returns `default`, i.e. the shipped binary's GrowthBook-absent behavior) —
//! with the binary's EXACT per-gate env-vs-flag split:
//!   - resolution gate `isLoopDefaultPromptEnabled` — REMOVED in 2.1.263 (the
//!     flag `tengu_kairos_loop_prompt` no longer exists in the binary); the
//!     sentinel resolvers are unconditional
//!   - dynamic gate `isLoopDynamic` — REMOVED in 2.1.263 (`tengu_kairos_loop_dynamic`
//!     no longer exists); the dynamic `/loop` mode is unconditional
//!   - preamble variant `isLoopPersistentPreambleEnabled` = env `CLAUDE_CODE_LOOP_PERSISTENT` || `nt("tengu_kairos_loop_persistent",false)`
//!   - keepalive gate `isLoopKeepaliveEnabled` = env `CLAUDE_CODE_LOOP_KEEPALIVE` || `nt("tengu_kairos_loop_keepalive",false)`
//!   - `PushNotification` addendum `Yke()` = `nt("tengu_kairos_push_notifications",false)` && `agentPushNotifEnabled` setting
//! With no live GrowthBook fetcher wired (the prod default) every flag is at its
//! shipped `false`, so the whole subsystem is inert and byte-identical to the
//! shipped binary. Tests flip a flag via [`telemetry::test_set_flag`] (binary's
//! `ROt`/`Uvi` override layer) rather than env vars. NOTE: the earlier
//! `LINGXI_LOOP_PROMPT`/`CLAUDE_CODE_LOOP_DYNAMIC` env stand-ins were REMOVED
//! — the binary's `fJr`/`q_e` are flag-only (no env layer), so those envs were a
//! false-positive divergence. Only PERSISTENT/KEEPALIVE keep an env layer (the
//! binary's `YIn`/`iKi` genuinely have one).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

// ── Sentinels (binary string-table) ──────────────────────────────────────────

/// `Wst` (cc_all.txt:504931) — CronCreate-based autonomous loop sentinel.
pub const AUTONOMOUS_LOOP_SENTINEL: &str = "<<autonomous-loop>>";
/// `Jke` (cc_all.txt:504931) — ScheduleWakeup self-paced autonomous sentinel.
pub const AUTONOMOUS_LOOP_DYNAMIC_SENTINEL: &str = "<<autonomous-loop-dynamic>>";
/// `rKi` (cc_all.txt:504966) — CronCreate loop.md-tasks sentinel.
pub const LOOP_FILE_SENTINEL: &str = "<<loop.md>>";
/// `lFt` (cc_all.txt:504966) — ScheduleWakeup loop.md-tasks sentinel.
pub const LOOP_FILE_DYNAMIC_SENTINEL: &str = "<<loop.md-dynamic>>";

/// `ZVi` (cc_all.txt:504966) — the placeholder stored in `Gst` once the preamble
/// has been delivered through the loop.md-default path (so a later loop.md-absent
/// fire knows the preamble already shipped).
const PREAMBLE_SENTINEL: &str = "__autonomous_preamble__";

/// `zIn` (cc_all.txt:504966) — loop.md truncation budget in bytes.
const LOOP_FILE_MAX_BYTES: usize = 25_000;

/// `Kh` (cc_all.txt:504932) — the `ScheduleWakeup` tool name, interpolated into
/// every tick prompt.
const SCHEDULE_WAKEUP: &str = "ScheduleWakeup";
/// `IA` (cc_all.txt:504932) — the `Monitor` tool name.
const MONITOR: &str = "Monitor";
/// `AI` (cc_all.txt:504945) — the `TaskList` tool name.
const TASK_LIST: &str = "TaskList";
/// `eP` (cc_all.txt:504945) — the `TaskStop` tool name.
const TASK_STOP: &str = "TaskStop";
/// `Z8` (cc_all.txt:504927) — the `PushNotification` tool name.
const PUSH_NOTIFICATION: &str = "PushNotification";

// ── Feature-flag gates (binary `nt` flag reads + per-gate env overrides) ──────
//
// The binary's sync flag reader `nt(key,default)` is ported as
// `telemetry::flag_bool` (an empty cached snapshot by default → returns the
// passed default = the shipped binary's GrowthBook-absent behavior). Each gate
// below matches the binary's EXACT env-vs-flag split: PERSISTENT/KEEPALIVE are
// `env || flag` (the binary's `YIn`/`iKi` have a `CLAUDE_CODE_LOOP_*` env layer);
// PROMPT/DYNAMIC are FLAG-ONLY (`fJr`/`q_e` have NO env layer in the binary).

/// `YIn` / `isLoopPersistentPreambleEnabled` (cc_all.txt:504950):
/// 2.1.270 `g`: a nonempty raw env string wins, otherwise read the flag.
#[must_use]
pub fn is_loop_persistent_preamble_enabled() -> bool {
    std::env::var("CLAUDE_CODE_LOOP_PERSISTENT").is_ok_and(|value| !value.is_empty())
        || telemetry::flag_bool("tengu_kairos_loop_persistent", false)
}

/// `fJr` / `isLoopDefaultPromptEnabled` — REMOVED in 2.1.263.
///
/// Through 2.1.2xx this was `nt("tengu_kairos_loop_prompt",false)` and gated
/// whether the autonomous/loop.md sentinels resolved at all. In 2.1.263 the flag
/// does not appear in the binary and `resolveAutonomousLoopFire` /
/// `resolveLoopFileFire` have no gate return, so this is a constant `true`. It
/// is kept as a function (rather than deleted) so the call sites still read like
/// the binary's dispatch; there is nothing for a test to flip.
#[must_use]
pub fn is_loop_default_prompt_enabled() -> bool {
    // PARITY 2.1.263: `tengu_kairos_loop_prompt` no longer exists in the binary
    // (0 hits); `resolveAutonomousLoopFire` / `resolveLoopFileFire` resolve the
    // sentinels unconditionally. Kept as a function so callers read naturally.
    true
}

/// `q_e` / `isLoopDynamic` — REMOVED in 2.1.263.
///
/// Through 2.1.2xx this was `nt("tengu_kairos_loop_dynamic",false)` and selected
/// the DYNAMIC-pacing builders (`hZm` usage / `gZm` prompt-builder, and
/// `a(loopFile,true)` for the no-prompt autonomous default) over the cron
/// variants. In 2.1.263 the flag does not appear in the binary and those
/// builders are the only ones reachable, so this is a constant `true`.
#[must_use]
pub fn is_loop_dynamic_enabled() -> bool {
    // PARITY 2.1.263: `tengu_kairos_loop_dynamic` no longer exists in the binary
    // (0 hits). `/loop <prompt>` always builds the dynamic (ScheduleWakeup)
    // prompt and `ScheduleWakeup.call` has no gate branch.
    true
}

/// `iKi` / `isLoopKeepaliveEnabled` (cc_all.txt:504966):
/// `rt(process.env.CLAUDE_CODE_LOOP_KEEPALIVE) || nt("tengu_kairos_loop_keepalive",false)`.
/// Gates the keepalive fallback heartbeat (the `lKi`/`cKi` re-arm when a dynamic
/// loop tick completes without the model rescheduling).
// PARITY: the keepalive *gate* is ported here; the keepalive *scheduling*
// machinery (`lKi`/`cKi`, the in-flight-tick tagging, the consecutive-keepalive
// budget, and the loading→idle trigger) is a separate larger subsystem still
// pending — so this gate currently has no production consumer beyond the gate
// being available for that follow-on work.
#[must_use]
pub fn is_loop_keepalive_enabled() -> bool {
    // PARITY 2.1.263 `YXn`: `let e=a.CLAUDE_CODE_LOOP_KEEPALIVE; if(e!==void 0)
    // return e; return H("tengu_kairos_loop_keepalive",!0)` — a DEFINED env var
    // is returned raw (any non-empty string is truthy in JS), and the flag
    // default is TRUE.
    match std::env::var("CLAUDE_CODE_LOOP_KEEPALIVE") {
        Ok(value) => !value.is_empty(),
        Err(_) => telemetry::flag_bool("tengu_kairos_loop_keepalive", true),
    }
}

/// `Yke` (cc_all.txt:504927): `Rle() && mc("agentPushNotifEnabled",false).value`,
/// where `Rle()` = `nt("tengu_kairos_push_notifications",false)`. Gates the
/// `PushNotification` addendum appended to tick prompts.
// PARITY: the flag `tengu_kairos_push_notifications` is read via `flag_bool`
// (default false, no live GrowthBook) and the `agentPushNotifEnabled` setting is
// not a supported port setting (binary `mc(...,false)` default → false), so
// `Yke()` resolves to `false` and `aFt()`/`Kpc()` render "" — byte-identical to
// the shipped binary's default config. The gate is now STRUCTURALLY 1:1 (it reads
// the real flag) rather than a hardcoded `false`, so it flips correctly if the
// flag/setting are ever enabled.
#[must_use]
pub fn is_push_notif_enabled() -> bool {
    // ONE implementation, hosted in `telemetry` so the orchestrator's
    // monitor-event renderer can gate on the same predicate without taking a
    // dependency on `cron`.
    telemetry::push_notifications_enabled()
}

// ── Preambles (binary `aJr` / `VVi`) ─────────────────────────────────────────

/// `loopAutonomousPreamble-07qcyhv4.md` (2.1.263) — the default (non-
/// persistent) autonomous-loop preamble, shipped as a bundled markdown file in
/// the binary (ASCII hyphens, blank lines between paragraphs, trailing newline).
/// Byte-exact copy of that file.
const PREAMBLE_DEFAULT: &str = include_str!("bundled/loopAutonomousPreamble.md");

/// `loopAutonomousPreamblePersistent-3zqtkrvg.md` (2.1.263) — the persistent-
/// preamble variant (`getAutonomousLoopPreamble` returns this when
/// `isLoopPersistentPreambleEnabled`). Byte-exact copy of the bundled file.
const PREAMBLE_PERSISTENT: &str = include_str!("bundled/loopAutonomousPreamblePersistent.md");

/// `AUTONOMOUS_LOOP_PREAMBLE` (`j4d`, cc_all.txt:504950) = the default preamble.
pub const AUTONOMOUS_LOOP_PREAMBLE: &str = PREAMBLE_DEFAULT;

/// `dJr` / `getAutonomousLoopPreamble` (cc_all.txt:504950):
/// `isLoopPersistentPreambleEnabled() ? VVi : aJr`.
#[must_use]
pub fn get_autonomous_loop_preamble() -> &'static str {
    if is_loop_persistent_preamble_enabled() {
        PREAMBLE_PERSISTENT
    } else {
        PREAMBLE_DEFAULT
    }
}

/// `pJr` / `logAutonomousLoopActivation` (cc_all.txt:504950):
/// `W("tengu_kairos_loop_persistent_activated",{variant:YIn()})`. Emitted when
/// the autonomous-loop default is activated — by the `/loop` command's no-prompt
/// builder (binary `a(c,u)` when loop.md is absent) and by the fire-time
/// resolvers `nKi` (every autonomous fire) / `sKi` (loop.md-absent fire).
// PARITY: binary pJr (cc_all.txt:504950).
pub fn log_autonomous_loop_activation() {
    telemetry::emit_loop_persistent_activated(is_loop_persistent_preamble_enabled());
}

// ── PushNotification addendum (binary `aFt`) ─────────────────────────────────

/// `aFt(e)` (cc_all.txt:504950) — the `PushNotification` block appended to tick
/// prompts. Returns "" when `Yke()` is off (the port's default). `cron` selects
/// the "you're ending the loop" phrasing (cron variant) vs the "third straight
/// tick…" phrasing (dynamic variant, when persistent preamble is on).
// PARITY: binary aFt (cc_all.txt:504950-504951).
fn push_notif_addendum(cron: bool) -> String {
    if !is_push_notif_enabled() {
        return String::new();
    }
    // PARITY: `let n=!e&&YIn()?…:…` — `e`=cron flag, `YIn()`=persistent.
    let n = if !cron && is_loop_persistent_preamble_enabled() {
        "newly blocked on a decision you won't make alone, you're ending the loop"
    } else {
        "newly blocked on a decision you won't make alone, third straight tick with nothing to do, you're ending the loop"
    };
    format!(
        "\n\nUse {PUSH_NOTIFICATION} when the loop can't move further without the user, or when something landed that they'd want to act on now: {n}, or a major update arrived (CI went red, a review changes the plan). Progress you made yourself isn't a trigger — the transcript covers that. One ping per state, not per tick."
    )
}

/// `mJr` (cc_all.txt:504966) — the Monitor/keepalive fallback addendum appended
/// to every DYNAMIC tick prompt. Interpolates Monitor/TaskList/TaskStop names.
// PARITY: binary mJr (cc_all.txt:504966).
fn monitor_addendum() -> String {
    format!(
        "\n\nIf a {MONITOR} is armed (check {TASK_LIST}), keep `delaySeconds` at 1200–1800s — the {MONITOR} is the wake signal and this is only the fallback heartbeat. If you were woken by a `<task-notification>`, handle the event before deciding whether to re-arm. To stop the loop, call {SCHEDULE_WAKEUP} with `stop: true` and {TASK_STOP} the monitor (use {TASK_LIST} to find its task ID if no longer in context)."
    )
}

// ── Tick-prompt builders (binary `tKi`/`W4d`/`G4d`/`V4d`/`K4d`) ───────────────

/// `tKi` (cc_all.txt:504951) — autonomous loop tick (cron mode).
fn tick_autonomous_cron() -> String {
    format!(
        "# Autonomous loop tick\n\nRun the autonomous check using the loop instructions established earlier in this conversation. If you cannot find them, treat this as a no-op tick. The recurring cron will fire the next tick automatically — do not call {SCHEDULE_WAKEUP} from this tick.{addendum}",
        addendum = push_notif_addendum(false),
    )
}

/// `W4d` (cc_all.txt:504952) — autonomous loop tick (dynamic pacing).
fn tick_autonomous_dynamic() -> String {
    format!(
        "# Autonomous loop tick (dynamic pacing)\n\nRun the autonomous check using the loop instructions established earlier in this conversation. If you cannot find them, treat this as a no-op tick.\n\nYou scheduled this tick via the {SCHEDULE_WAKEUP} tool (not a recurring cron). To keep the loop alive, call {SCHEDULE_WAKEUP} again at the end of this turn with `prompt` set to the literal sentinel `{AUTONOMOUS_LOOP_DYNAMIC_SENTINEL}` and `noop` set to `true` if this tick changed nothing (or `false` if it did) — otherwise the loop ends after this tick.{monitor}{push}",
        monitor = monitor_addendum(),
        push = push_notif_addendum(false),
    )
}

/// `G4d` (cc_all.txt:504952) — loop.md tasks tick (cron mode).
fn tick_loopfile_cron() -> String {
    format!(
        "# /loop tick — loop.md tasks\n\nWork the tasks from the loop.md contents established earlier in this conversation. If you cannot find them, treat this as a no-op tick. The recurring cron will fire the next tick automatically — do not call {SCHEDULE_WAKEUP} from this tick.{addendum}",
        addendum = push_notif_addendum(true),
    )
}

/// `V4d` (cc_all.txt:504952) — loop.md tasks tick (dynamic pacing).
fn tick_loopfile_dynamic() -> String {
    format!(
        "# /loop tick — loop.md tasks (dynamic pacing)\n\nWork the tasks from the loop.md contents established earlier in this conversation. If you cannot find them, treat this as a no-op tick.\n\nYou scheduled this tick via the {SCHEDULE_WAKEUP} tool (not a recurring cron). To keep the loop alive, call {SCHEDULE_WAKEUP} again at the end of this turn with `prompt` set to the literal sentinel `{LOOP_FILE_DYNAMIC_SENTINEL}` and `noop` set to `true` if this tick changed nothing (or `false` if it did) — otherwise the loop ends after this tick.{monitor}{push}",
        monitor = monitor_addendum(),
        push = push_notif_addendum(true),
    )
}

/// `K4d` (cc_all.txt:504952) — loop.md ABSENT tick (dynamic pacing).
fn tick_loopfile_absent_dynamic() -> String {
    format!(
        "# /loop tick — loop.md absent (dynamic pacing)\n\nloop.md is not currently present. Run the autonomous check using the loop instructions established earlier in this conversation.\n\nYou scheduled this tick via the {SCHEDULE_WAKEUP} tool (not a recurring cron). To keep the loop alive — and to pick up loop.md if it is recreated — call {SCHEDULE_WAKEUP} again at the end of this turn with `prompt` set to the literal sentinel `{LOOP_FILE_DYNAMIC_SENTINEL}` and `noop` set to `true` if this tick changed nothing (or `false` if it did) — otherwise the loop ends after this tick.{monitor}{push}",
        monitor = monitor_addendum(),
        push = push_notif_addendum(false),
    )
}

// ── loop.md reader (binary `oKi` + truncation `z4d`) ──────────────────────────

/// JavaScript trim/regexp whitespace (ECMAScript WhiteSpace + LineTerminator).
/// U+FEFF is included; U+0085, unlike Rust's `char::is_whitespace`, is not.
pub fn is_loop_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

// The upstream warning says bytes, but 2.1.270 `P` uses JS UTF-16 length/slice.
fn truncate_loop_file(content: &str) -> String {
    let units: Vec<u16> = content.encode_utf16().collect();
    if units.len() <= LOOP_FILE_MAX_BYTES {
        return content.to_string();
    }
    let cut = units[..=LOOP_FILE_MAX_BYTES]
        .iter()
        .rposition(|u| *u == 10)
        .filter(|i| *i > 0)
        .unwrap_or(LOOP_FILE_MAX_BYTES);
    // A JS slice can leave a lone surrogate; Node UTF-8 serialization replaces
    // it with U+FFFD, matching from_utf16_lossy at the Rust string boundary.
    let head = String::from_utf16_lossy(&units[..cut]);
    format!("{head}\n\n> WARNING: loop.md was truncated to {LOOP_FILE_MAX_BYTES} bytes. Keep the task list concise.")
}

pub use tasks::handlers::monitor::{
    bounded_monitor_timeout_ms, bounded_monitors_enabled, monitor_duration,
};

/// A located loop.md file: its path and (truncated) contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopFile {
    /// The file the contents were read from.
    pub path: PathBuf,
    /// Trimmed + truncated contents.
    pub content: String,
}

/// Legacy single-directory reader. Session callers use [`read_loop_file_at`]
/// so project root, current directory, and filesystem errors remain explicit.
#[must_use]
pub fn read_loop_file(cwd: &Path) -> Option<LoopFile> {
    read_loop_file_at(cwd, cwd).ok().flatten()
}

// `Dt` in src_164606951.js accepts this exact errno set; `c` adds EISDIR.
fn skippable_loop_file_error(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    if let Some(code) = error.raw_os_error() {
        return [
            libc::ENOENT,
            libc::EACCES,
            libc::EPERM,
            libc::ENOTDIR,
            libc::ELOOP,
            libc::ENAMETOOLONG,
            libc::EROFS,
            libc::EISDIR,
        ]
        .contains(&code);
    }
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
    ) || cfg!(windows) && matches!(error.raw_os_error(), Some(19 | 206 | 267 | 1142 | 1921))
}

/// 2.1.270 `_( )`: project-root `.claude/loop.md`, then current-directory
/// `loop.md`. Node's UTF-8 replacement decoding precedes ECMAScript trimming.
/// Expected path/access failures (`Dt` plus EISDIR) are skipped; unexpected
/// errors such as EIO propagate.
pub fn read_loop_file_at(project_root: &Path, cwd: &Path) -> std::io::Result<Option<LoopFile>> {
    let candidates = [
        project_root.join(".claude").join("loop.md"),
        cwd.join("loop.md"),
    ];
    for path in candidates {
        let raw = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if skippable_loop_file_error(&error) => continue,
            Err(error) => return Err(error),
        };
        let text = String::from_utf8_lossy(&raw);
        let trimmed = text.trim_matches(is_loop_js_whitespace);
        if !trimmed.is_empty() {
            return Ok(Some(LoopFile {
                path,
                content: truncate_loop_file(trimmed),
            }));
        }
    }
    Ok(None)
}

// ── Sentinel predicates (binary `hJr`/`gJr`/`Y4d`) ───────────────────────────

/// `hJr` / `isAutonomousLoopSentinel` (cc_all.txt:504952).
#[must_use]
pub fn is_autonomous_loop_sentinel(s: &str) -> bool {
    s == AUTONOMOUS_LOOP_SENTINEL || s == AUTONOMOUS_LOOP_DYNAMIC_SENTINEL
}

/// `gJr` / `isLoopFileSentinel` (cc_all.txt:504966).
#[must_use]
pub fn is_loop_file_sentinel(s: &str) -> bool {
    s == LOOP_FILE_SENTINEL || s == LOOP_FILE_DYNAMIC_SENTINEL
}

/// `Y4d` / `isLoopDefaultSentinel` (cc_all.txt:504966): any of the four.
#[must_use]
pub fn is_loop_default_sentinel(s: &str) -> bool {
    is_autonomous_loop_sentinel(s) || is_loop_file_sentinel(s)
}

// ── First-vs-subsequent delivery state (binary `iFt` / `Gst`) ─────────────────

/// 2.1.270 `e.autonomousPreambleDelivered` / `e.lastLoopFileDelivered`.
/// Each session owns its own state; the global wrapper serves isolated callers.
#[derive(Default)]
struct DeliveryState {
    /// `iFt` — true once the full preamble has been delivered on a prior fire.
    preamble_delivered: bool,
    /// `Gst` — last delivered loop.md content (or the preamble placeholder).
    last_content: Option<String>,
}

static DELIVERY: Mutex<DeliveryState> = Mutex::new(DeliveryState {
    preamble_delivered: false,
    last_content: None,
});

/// `X4d` / `resetAutonomousLoopDelivered` (cc_all.txt:504966): clears the
/// first-delivery state so the next fire re-emits the full preamble.
// PARITY: in the binary `X4d` is invoked from the post-compact cleanup `Zne`
// (`if(o)resetAutonomousLoopDelivered()`, main-thread compact). The port wires it
// at the same site — `compaction::run_post_compact_cleanup` inside its
// main-thread-compact gate. Live as of 2.1.263: the resolver gate is gone, so
// the sentinels always resolve and `DELIVERY` really is mutated.
pub fn reset_autonomous_loop_delivered() {
    let mut st = DELIVERY.lock().unwrap();
    st.preamble_delivered = false;
    st.last_content = None;
}

// ── Loop runtime state (binary `Nt.loopTickInFlightPrompt` / `…Keepalives`) ───
//
// The keepalive fallback tracks, across a /loop tick turn: the prompt of the tick
// currently in flight (`tAt`/`I7e`) and how many consecutive keepalives have been
// armed without the model rescheduling (`PZt`/`nAt`, budget `tqd`=1). In the
// binary these live on the session state object `Nt`; the port mirrors them with
// a process-global [`Mutex`] (single live /loop per process, like `DELIVERY`).

/// Per-prompt dynamic-loop bookkeeping (2.1.263 `PLn(prompt)` / `dYt(prompt, …)`):
/// when this loop started, when its last wakeup was due, and whether it has
/// already been aged out. A loop whose last wakeup is more than an hour in the
/// past is treated as a NEW loop (the binary's `S` restart check).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DynamicLoopRecord {
    /// Epoch ms when the loop's first wakeup was scheduled.
    pub started_at_ms: i64,
    /// Epoch ms the most recent wakeup was scheduled FOR.
    pub last_scheduled_for_ms: i64,
    /// True once the loop reached `recurringMaxAgeMs` and was ended.
    pub aged_out: bool,
}

/// Why a `/loop` tick was NOT quiet — the veto arms of the oracle's `v()`,
/// with its exact reason literals.
///
/// The oracle finds these by walking the transcript span between the last
/// `scheduled_task_fire{cronKind:"loop"}` anchor and the end of the transcript.
/// LingXi has no live transcript array to walk: a wakeup is enqueued as ONE
/// command, so the span IS the turn it runs. The components that observe a
/// disturbance therefore mark it on [`LoopRuntime`] as it happens, and the
/// turn-completion edge reads the mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopFoldVeto {
    /// A blocking system message (a compaction boundary or another scheduled
    /// fire) landed in the span.
    BlockingSystemInSpan,
    /// A tool call was interrupted or cancelled.
    ToolAbort,
    /// A tool call was denied.
    ToolDenial,
    /// Real input arrived from a human during the tick.
    ForeignUserInput,
    /// A queued command was waiting to run.
    QueuedCommand,
    /// The model did not end the tick with `ScheduleWakeup({noop: true})`.
    ModelReportedWork,
}

impl LoopFoldVeto {
    /// The oracle's literal, as passed to `g("loop_noop_fold", reason)`.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::BlockingSystemInSpan => "blocking_system_in_span",
            Self::ToolAbort => "tool_abort",
            Self::ToolDenial => "tool_denial",
            Self::ForeignUserInput => "foreign_user_input",
            Self::QueuedCommand => "queued_command",
            Self::ModelReportedWork => "model_reported_work",
        }
    }
}

/// What the turn that just ended contributed to the fold span.
///
/// PARITY the two fields the oracle counts by walking the transcript span:
/// `span_len` (messages) and `tool_uses` (`tool_use` blocks). LingXi's span is
/// one turn, so the orchestrator tallies them as they happen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoopSpanCounts {
    /// `tool_use` blocks the assistant emitted (the oracle's `c.size`).
    pub tool_uses: u32,
    /// Messages the span holds (the oracle's `b.length`).
    pub span_len: u32,
}

/// What the turn-completion edge decided about the tick that just ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopFoldOutcome {
    /// The tick was quiet. `streak` counts it (so it is never 0) and `since` is
    /// when the run of quiet ticks began — the oracle's `noOpStreak` /
    /// `streakStartedAt`.
    Folded {
        /// Consecutive quiet ticks including this one.
        streak: u32,
        /// Start of the quiet run.
        since: SystemTime,
        /// Wall time from the fire that started this tick to now, in seconds
        /// (the oracle's `span_duration_s`).
        duration_secs: u64,
        /// The span's `tool_use` count and message count.
        span: LoopSpanCounts,
    },
    /// The tick did something; the streak resets.
    Vetoed {
        /// The oracle's veto literal.
        reason: LoopFoldVeto,
    },
}

#[derive(Default)]
struct LoopRuntimeState {
    /// Preamble and loop.md instructions delivered to this session transcript.
    delivery: DeliveryState,
    /// `Nt.loopTickInFlightPrompt` — prompt of the loop tick being processed.
    tick_in_flight_prompt: Option<String>,
    /// `Nt.loopConsecutiveKeepalives` — consecutive keepalive count.
    consecutive_keepalives: u32,
    /// The port's stand-in for `Xke()` (is-a-loop-cron-armed): set when the model
    /// calls `ScheduleWakeup` successfully this turn. The port has no loop-cron
    /// registry to query, and the wakeup enqueues in the FUTURE, so this
    /// synchronous per-turn flag is the only reliable "model rescheduled" signal.
    rescheduled_this_turn: bool,
    /// 2.1.263 dynamic-loop records keyed by the wakeup prompt (`PLn`/`dYt`).
    dynamic_loops: std::collections::HashMap<String, DynamicLoopRecord>,
    /// 2.1.263 `OLn()` / `gHt(bool)` — the loop already emitted its terminal
    /// `tengu_loop_ended`; a later `stop: true` is cleanup only.
    loop_ended: bool,
    /// When the in-flight tick started (the oracle's anchor timestamp), used for
    /// `span_duration_s`.
    tick_started_at: Option<SystemTime>,
    /// `noop` of the LAST `ScheduleWakeup` call in the current tick — the
    /// oracle's `p` in `v()`. `None` when the model armed no wakeup at all,
    /// which vetoes exactly as `p !== true` does.
    tick_noop_reported: Option<bool>,
    /// The FIRST disturbance seen during the current tick. The oracle returns on
    /// the first veto it meets walking the span forward, so first-wins.
    tick_veto: Option<LoopFoldVeto>,
    /// `noOpStreak` — consecutive quiet ticks folded so far.
    noop_streak: u32,
    /// `streakStartedAt` — when the current quiet run began.
    streak_started_at: Option<SystemTime>,
}

/// Session-scoped dynamic-loop state.
///
/// Claude Code stores these fields on the live session object.  Keeping them
/// behind an owned handle prevents one bridge connection's normal prompt from
/// clearing another connection's in-flight loop tick.
#[derive(Clone, Default)]
pub struct LoopRuntime {
    state: std::sync::Arc<Mutex<LoopRuntimeState>>,
}

impl LoopRuntime {
    /// Resolve a scheduled prompt using only this session's delivered instructions.
    /// Mirrors 2.1.270 `se(e, t)`; ordinary prompts pass through byte-for-byte.
    #[must_use]
    pub fn resolve_loop_default_fire(&self, sentinel: &str, cwd: &Path) -> String {
        let mut st = self.state.lock().unwrap();
        resolve_loop_default_fire_with_state(&mut st.delivery, sentinel, cwd)
    }

    /// Resolve with explicit session roots and propagate loop.md read errors.
    pub fn try_resolve_loop_default_fire(
        &self,
        sentinel: &str,
        project_root: &Path,
        cwd: &Path,
    ) -> std::io::Result<String> {
        // Read before taking/mutating delivery state; a failed read must not
        // consume the first-delivery marker.
        let file = if is_loop_file_sentinel(sentinel) {
            read_loop_file_at(project_root, cwd)?
        } else {
            None
        };
        let mut st = self.state.lock().unwrap();
        Ok(
            resolve_autonomous_loop_fire_with_state(&mut st.delivery, sentinel)
                .or_else(|| resolve_loop_file_fire_with_file(&mut st.delivery, sentinel, file))
                .unwrap_or_else(|| sentinel.to_string()),
        )
    }

    /// After this session compacts away its instructions, re-deliver them on
    /// its next tick without changing other loop or session state.
    pub fn reset_autonomous_loop_delivered(&self) {
        self.state.lock().unwrap().delivery = DeliveryState::default();
    }

    /// Mark the start of a loop tick and clear the prior reschedule marker.
    pub fn begin_tick(&self, prompt: String) {
        self.begin_tick_at(prompt, SystemTime::now());
    }

    /// [`Self::begin_tick`] with an explicit start instant (tests, and hosts
    /// with their own clock). Also clears the per-tick fold marks, so a veto or
    /// a `noop` from the previous tick can never settle this one.
    pub fn begin_tick_at(&self, prompt: String, at: SystemTime) {
        let mut st = self.state.lock().unwrap();
        st.tick_in_flight_prompt = Some(prompt);
        st.rescheduled_this_turn = false;
        st.tick_started_at = Some(at);
        st.tick_noop_reported = None;
        st.tick_veto = None;
    }

    /// Record the `noop` argument of a `ScheduleWakeup` call made during this
    /// tick. The LAST call of the tick wins, matching the oracle's `p`, which is
    /// overwritten by each `ScheduleWakeup` tool_use it walks past.
    pub fn mark_noop_reported(&self, noop: bool) {
        self.state.lock().unwrap().tick_noop_reported = Some(noop);
    }

    /// Mark this tick as not-quiet. The FIRST veto recorded wins.
    pub fn veto_tick(&self, veto: LoopFoldVeto) {
        let mut st = self.state.lock().unwrap();
        if st.tick_veto.is_none() {
            st.tick_veto = Some(veto);
        }
    }

    /// PARITY `v()` + the streak arithmetic in `D()`: settle the tick that just
    /// ended into the no-op streak.
    ///
    /// Returns `None` when no tick was in flight (the turn was not a loop
    /// wakeup). A veto resets the streak; a fold extends it and returns the
    /// count INCLUDING this tick, which is what the next wakeup renders.
    ///
    /// Reads the in-flight marker WITHOUT taking it — the keepalive and
    /// user-abort edges that run after this one consume it.
    pub fn settle_tick(&self, now: SystemTime, span: LoopSpanCounts) -> Option<LoopFoldOutcome> {
        let mut st = self.state.lock().unwrap();
        st.tick_in_flight_prompt.as_ref()?;
        let veto = st.tick_veto.or_else(|| {
            (st.tick_noop_reported != Some(true)).then_some(LoopFoldVeto::ModelReportedWork)
        });
        st.tick_veto = None;
        st.tick_noop_reported = None;
        let started_at = st.tick_started_at.take();
        if let Some(reason) = veto {
            st.noop_streak = 0;
            st.streak_started_at = None;
            return Some(LoopFoldOutcome::Vetoed { reason });
        }
        let since = st.streak_started_at.or(started_at).unwrap_or(now);
        st.noop_streak = st.noop_streak.saturating_add(1);
        st.streak_started_at = Some(since);
        let duration_secs = started_at
            .and_then(|start| now.duration_since(start).ok())
            .map_or(0, |d| d.as_secs());
        Some(LoopFoldOutcome::Folded {
            streak: st.noop_streak,
            since,
            duration_secs,
            span,
        })
    }

    /// Break a settled quiet streak when new user work appears between fires.
    /// Upstream inspects the entire fire-to-fire transcript, not only the tick.
    pub fn invalidate_noop_streak(&self) {
        let mut state = self.state.lock().unwrap();
        state.noop_streak = 0;
        state.streak_started_at = None;
    }

    /// The current no-op streak and when it started — what a firing wakeup
    /// renders as `\u{b7} N no-op tick(s) since \u{2026}`. `None` when the last tick
    /// was not quiet.
    #[must_use]
    pub fn noop_streak(&self) -> Option<(u32, SystemTime)> {
        let st = self.state.lock().unwrap();
        match (st.noop_streak, st.streak_started_at) {
            (0, _) | (_, None) => None,
            (streak, Some(since)) => Some((streak, since)),
        }
    }

    /// Peek at the current in-flight loop prompt.
    #[must_use]
    pub fn in_flight_prompt(&self) -> Option<String> {
        self.state.lock().unwrap().tick_in_flight_prompt.clone()
    }

    /// Take and clear the current in-flight loop prompt.
    pub fn take_in_flight_prompt(&self) -> Option<String> {
        self.state.lock().unwrap().tick_in_flight_prompt.take()
    }

    /// Record that the model scheduled its own next wakeup this turn.
    pub fn mark_rescheduled(&self) {
        self.state.lock().unwrap().rescheduled_this_turn = true;
    }

    /// Take and clear the per-turn reschedule marker.
    pub fn take_rescheduled(&self) -> bool {
        let mut st = self.state.lock().unwrap();
        std::mem::take(&mut st.rescheduled_this_turn)
    }

    /// Return the consecutive keepalive count.
    #[must_use]
    pub fn consecutive_keepalives(&self) -> u32 {
        self.state.lock().unwrap().consecutive_keepalives
    }

    /// Set the consecutive keepalive count.
    pub fn set_consecutive_keepalives(&self, count: u32) {
        self.state.lock().unwrap().consecutive_keepalives = count;
    }

    /// 2.1.263 `PLn(prompt)` — the dynamic-loop record for `prompt`, if any.
    #[must_use]
    pub fn dynamic_loop_record(&self, prompt: &str) -> Option<DynamicLoopRecord> {
        self.state
            .lock()
            .unwrap()
            .dynamic_loops
            .get(prompt)
            .copied()
    }

    /// 2.1.263 `dYt(prompt, record)` — store the dynamic-loop record for `prompt`.
    pub fn set_dynamic_loop_record(&self, prompt: &str, record: DynamicLoopRecord) {
        self.state
            .lock()
            .unwrap()
            .dynamic_loops
            .insert(prompt.to_string(), record);
    }

    /// 2.1.263 `Ort(prompt)` / `sessionCron.forgetChainStart` — drop the
    /// dynamic-loop record for `prompt`. `ZXn` (`stop: true`) and `t3t` (user
    /// abort) forget every cancelled wakeup's prompt plus the in-flight tick's,
    /// so a later `/loop` on the same prompt starts a fresh 7-day window instead
    /// of inheriting the stopped loop's `startedAt`.
    pub fn forget_dynamic_loop(&self, prompt: &str) {
        self.state.lock().unwrap().dynamic_loops.remove(prompt);
    }

    /// 2.1.263 `OLn()` — whether the loop already ended (terminal event emitted).
    #[must_use]
    pub fn loop_ended(&self) -> bool {
        self.state.lock().unwrap().loop_ended
    }

    /// 2.1.263 `gHt(ended)` — record / clear the loop-ended marker. `/loop`
    /// invocation clears it (`K_n`); every terminal path sets it.
    pub fn set_loop_ended(&self, ended: bool) {
        self.state.lock().unwrap().loop_ended = ended;
    }

    /// Clear all state for a fresh loop/session.
    pub fn reset(&self) {
        *self.state.lock().unwrap() = LoopRuntimeState::default();
    }
}

static LOOP_RUNTIME: std::sync::LazyLock<Mutex<LoopRuntimeState>> =
    std::sync::LazyLock::new(|| Mutex::new(LoopRuntimeState::default()));

/// Process-global `PLn(prompt)` (hosts without a session-scoped [`LoopRuntime`]).
#[must_use]
pub fn dynamic_loop_record(prompt: &str) -> Option<DynamicLoopRecord> {
    LOOP_RUNTIME
        .lock()
        .unwrap()
        .dynamic_loops
        .get(prompt)
        .copied()
}

/// Process-global `dYt(prompt, record)`.
pub fn set_dynamic_loop_record(prompt: &str, record: DynamicLoopRecord) {
    LOOP_RUNTIME
        .lock()
        .unwrap()
        .dynamic_loops
        .insert(prompt.to_string(), record);
}

/// Process-global `Ort(prompt)` — see [`LoopRuntime::forget_dynamic_loop`].
pub fn forget_dynamic_loop(prompt: &str) {
    LOOP_RUNTIME.lock().unwrap().dynamic_loops.remove(prompt);
}

/// Process-global `OLn()`.
#[must_use]
pub fn loop_ended() -> bool {
    LOOP_RUNTIME.lock().unwrap().loop_ended
}

/// Process-global `gHt(ended)`.
pub fn set_loop_ended(ended: bool) {
    LOOP_RUNTIME.lock().unwrap().loop_ended = ended;
}

/// 2.1.263 `K_n()` = `gHt(!1), Eje()` — `/loop` was invoked with no arguments,
/// so a previously-ended loop is live again.
///
/// The binary also calls `resetWakeFires()` here; the port has no `wakeFires`
/// counter (it exists only to render `fires: N` on the pending-wakeup status
/// line and `loopWakeFires` in checkpoint state, neither of which the port
/// surfaces), so there is nothing to reset. The binary guards this call with
/// `!isSkillPreload && !modelScheduledOrigin`; the port's `BundledPromptFn::build`
/// seam carries neither flag, and `build` is only reached from an actual user
/// dispatch, so the guard has no port-side equivalent to honour.
pub fn note_loop_invoked() {
    set_loop_ended(false);
}

/// Mark the START of a loop-tick turn (binary `onFireTask` `I7e(d.prompt)`):
/// record the in-flight tick prompt and clear the per-turn reschedule flag. Called
/// by the bridge drain when it pops a `QueueSource::Cron` command.
pub fn begin_loop_tick(prompt: String) {
    let mut st = LOOP_RUNTIME.lock().unwrap();
    st.tick_in_flight_prompt = Some(prompt);
    st.rescheduled_this_turn = false;
}

/// `tAt` (cc_all.txt) — peek the in-flight loop-tick prompt (or None).
#[must_use]
pub fn loop_tick_in_flight_prompt() -> Option<String> {
    LOOP_RUNTIME.lock().unwrap().tick_in_flight_prompt.clone()
}

/// `I7e(null)` (cc_all.txt) at turn end — take (read+clear) the in-flight prompt.
/// `Some` iff the just-completed turn was a loop tick.
pub fn take_loop_tick_in_flight_prompt() -> Option<String> {
    LOOP_RUNTIME.lock().unwrap().tick_in_flight_prompt.take()
}

/// The port's `Xke()=true` side: record that the model rescheduled this turn
/// (`ScheduleWakeup` success). Called from `ScheduleWakeupTool::call`.
pub fn mark_loop_rescheduled() {
    LOOP_RUNTIME.lock().unwrap().rescheduled_this_turn = true;
}

/// Take (read+clear) the per-turn reschedule flag — the port's `!Xke()` check.
pub fn take_loop_rescheduled() -> bool {
    let mut st = LOOP_RUNTIME.lock().unwrap();
    std::mem::take(&mut st.rescheduled_this_turn)
}

/// `PZt` (cc_all.txt) — consecutive-keepalive count.
#[must_use]
pub fn loop_consecutive_keepalives() -> u32 {
    LOOP_RUNTIME.lock().unwrap().consecutive_keepalives
}

/// `nAt` (cc_all.txt) — set the consecutive-keepalive count. Reset to 0 on any
/// non-keepalive schedule (binary `cKi`: `if(!r)nAt(0)`).
pub fn set_loop_consecutive_keepalives(n: u32) {
    LOOP_RUNTIME.lock().unwrap().consecutive_keepalives = n;
}

/// Clear all loop runtime state. Used by tests and a fresh-loop start.
pub fn reset_loop_runtime_state() {
    *LOOP_RUNTIME.lock().unwrap() = LoopRuntimeState::default();
}

/// Process-wide serialization lock for tests that mutate the shared `DELIVERY`
/// state and/or the `CLAUDE_CODE_LOOP_*` env vars. Shared across this crate's
/// test modules (e.g. `wakeup::tests::sentinel_resolution`) so resolution tests
/// don't race each other over the globals.
///
/// Exposed `pub` (and NOT `#[cfg(test)]`-gated) because after this module was
/// relocated from `tool-cron` into `cron` to satisfy §8.1 layering, the
/// `tool-cron` `wakeup::tests` modules reach it cross-crate as
/// `cron::autonomous_loop::TEST_SERIAL`; a dependency crate is never built with
/// `cfg(test)`, so the lock must exist in the normal build. It is a zero-cost
/// `Mutex<()>` static.
pub static TEST_SERIAL: Mutex<()> = Mutex::new(());

// ── Fire resolvers (binary `nKi` / `sKi` / `J4d`) ────────────────────────────

/// `nKi` / `resolveAutonomousLoopFire` (cc_all.txt:504952): resolves an
/// autonomous sentinel → the tick prompt, prefixing the full preamble on first
/// delivery only. Returns `None` for a non-autonomous sentinel or when the gate
/// is off (passthrough handled by [`resolve_loop_default_fire`]).
#[must_use]
pub fn resolve_autonomous_loop_fire(sentinel: &str) -> Option<String> {
    resolve_autonomous_loop_fire_with_state(&mut DELIVERY.lock().unwrap(), sentinel)
}

fn resolve_autonomous_loop_fire_with_state(
    st: &mut DeliveryState,
    sentinel: &str,
) -> Option<String> {
    if !is_autonomous_loop_sentinel(sentinel) {
        return None;
    }
    // PARITY: `nKi` calls `pJr()` on EVERY autonomous fire, right after the two
    // gate returns and before computing the tick (cc_all.txt:504952).
    log_autonomous_loop_activation();
    let tick = if sentinel == AUTONOMOUS_LOOP_DYNAMIC_SENTINEL {
        tick_autonomous_dynamic()
    } else {
        tick_autonomous_cron()
    };
    // PARITY: `if(iFt||Gst!==null)return t;return iFt=!0,`${dJr()}\n${t}``.
    if st.preamble_delivered || st.last_content.is_some() {
        return Some(tick);
    }
    st.preamble_delivered = true;
    // PARITY 2.1.263: `${v()}\n\n---\n\n${o}`.
    Some(format!(
        "{}\n\n---\n\n{}",
        get_autonomous_loop_preamble(),
        tick
    ))
}

/// `sKi` / `resolveLoopFileFire` (cc_all.txt:504966): resolves a loop.md sentinel
/// → the tick prompt. On first delivery (or whenever loop.md content changed
/// since last fire) it inlines the full loop.md contents; on unchanged fires it
/// returns the short reminder tick only. When loop.md is absent it falls back to
/// the autonomous preamble + absent-tick (first delivery) or the autonomous cron
/// tick (subsequent). `cwd` locates loop.md (binary uses process cwd).
#[must_use]
pub fn resolve_loop_file_fire(sentinel: &str, cwd: &Path) -> Option<String> {
    resolve_loop_file_fire_with_state(&mut DELIVERY.lock().unwrap(), sentinel, cwd)
}

fn resolve_loop_file_fire_with_state(
    st: &mut DeliveryState,
    sentinel: &str,
    cwd: &Path,
) -> Option<String> {
    if !is_loop_file_sentinel(sentinel) {
        return None;
    }
    resolve_loop_file_fire_with_file(st, sentinel, read_loop_file(cwd))
}

fn resolve_loop_file_fire_with_file(
    st: &mut DeliveryState,
    sentinel: &str,
    file: Option<LoopFile>,
) -> Option<String> {
    if !is_loop_file_sentinel(sentinel) {
        return None;
    }
    let dynamic = sentinel == LOOP_FILE_DYNAMIC_SENTINEL;
    if let Some(file) = file {
        // PARITY: `let o=t?V4d():G4d();if(Gst===n.content)return o;Gst=n.content,…`
        let tick = if dynamic {
            tick_loopfile_dynamic()
        } else {
            tick_loopfile_cron()
        };
        if st.last_content.as_deref() == Some(file.content.as_str()) {
            return Some(tick);
        }
        st.last_content = Some(file.content.clone());
        // PARITY 2.1.263: `# /loop tick — tasks from ${path}\n\n…\n\n---\n\n${content}\n\n---\n\n${tick}`.
        return Some(format!(
            "# /loop tick — tasks from {path}\n\nThe user configured a loop-tasks file. Work through the tasks defined below; these are the instructions for this tick and every subsequent tick (the reminder on later fires refers back to this message).\n\n---\n\n{content}\n\n---\n\n{tick}",
            path = file.path.display(),
            content = file.content,
        ));
    }
    // PARITY: loop.md ABSENT — `sKi` calls `pJr()` here (only on the absent path,
    // after the `if(n){…}` present-block), then `let r=t?K4d():tKi();
    // if(Gst===ZVi||iFt)return r;return Gst=ZVi,iFt=!0,`${dJr()}\n${r}``
    // (cc_all.txt:504966).
    log_autonomous_loop_activation();
    let tick = if dynamic {
        tick_loopfile_absent_dynamic()
    } else {
        tick_autonomous_cron()
    };
    if st.last_content.as_deref() == Some(PREAMBLE_SENTINEL) || st.preamble_delivered {
        return Some(tick);
    }
    st.last_content = Some(PREAMBLE_SENTINEL.to_string());
    st.preamble_delivered = true;
    Some(format!(
        "{}\n\n---\n\n{}",
        get_autonomous_loop_preamble(),
        tick
    ))
}

/// `J4d` / `resolveLoopDefaultFire` (cc_all.txt:504966):
/// `nKi(e) ?? sKi(e) ?? e` — try autonomous, then loop.md, else passthrough.
#[must_use]
pub fn resolve_loop_default_fire(sentinel: &str, cwd: &Path) -> String {
    resolve_loop_default_fire_with_state(&mut DELIVERY.lock().unwrap(), sentinel, cwd)
}

fn resolve_loop_default_fire_with_state(
    st: &mut DeliveryState,
    sentinel: &str,
    cwd: &Path,
) -> String {
    resolve_autonomous_loop_fire_with_state(st, sentinel)
        .or_else(|| resolve_loop_file_fire_with_state(st, sentinel, cwd))
        .unwrap_or_else(|| sentinel.to_string())
}

#[cfg(test)]
mod fold_tests {
    use super::{LoopFoldOutcome, LoopFoldVeto, LoopRuntime, LoopSpanCounts};
    use std::time::{Duration, SystemTime};

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// A quiet tick still costs one assistant message plus the `ScheduleWakeup`
    /// call and its result.
    fn quiet_span() -> LoopSpanCounts {
        LoopSpanCounts {
            tool_uses: 1,
            span_len: 2,
        }
    }

    /// A tick the model closed with `noop: true` folds, and consecutive quiet
    /// ticks keep the streak's ORIGINAL start (the oracle's
    /// `since: s.streakStartedAt ?? s.timestamp`).
    #[test]
    fn consecutive_quiet_ticks_extend_one_streak() {
        let rt = LoopRuntime::default();
        rt.begin_tick_at("p".into(), at(1_000));
        rt.mark_noop_reported(true);
        assert_eq!(
            rt.settle_tick(at(1_030), quiet_span()),
            Some(LoopFoldOutcome::Folded {
                streak: 1,
                since: at(1_000),
                duration_secs: 30,
                span: quiet_span(),
            })
        );
        rt.begin_tick_at("p".into(), at(2_000));
        rt.mark_noop_reported(true);
        assert_eq!(
            rt.settle_tick(at(2_010), quiet_span()),
            Some(LoopFoldOutcome::Folded {
                streak: 2,
                since: at(1_000),
                duration_secs: 10,
                span: quiet_span(),
            })
        );
        assert_eq!(rt.noop_streak(), Some((2, at(1_000))));
    }

    /// PARITY `if(p!==!0) return {kind:"veto",reason:"model_reported_work"}` —
    /// a tick that did work, and one that armed no wakeup at all, both veto.
    #[test]
    fn work_or_silence_vetoes_and_resets_the_streak() {
        for reported in [Some(false), None] {
            let rt = LoopRuntime::default();
            rt.begin_tick_at("p".into(), at(1_000));
            rt.mark_noop_reported(true);
            rt.settle_tick(at(1_010), quiet_span());
            rt.begin_tick_at("p".into(), at(2_000));
            if let Some(noop) = reported {
                rt.mark_noop_reported(noop);
            }
            assert_eq!(
                rt.settle_tick(at(2_010), quiet_span()),
                Some(LoopFoldOutcome::Vetoed {
                    reason: LoopFoldVeto::ModelReportedWork,
                })
            );
            assert_eq!(rt.noop_streak(), None, "a veto resets the streak");
        }
    }

    /// A veto marked by the driver wins over the model's own `noop: true`, and
    /// the FIRST veto of the tick is the one reported.
    #[test]
    fn a_marked_veto_beats_a_noop_claim_and_first_wins() {
        let rt = LoopRuntime::default();
        rt.begin_tick_at("p".into(), at(1_000));
        rt.mark_noop_reported(true);
        rt.veto_tick(LoopFoldVeto::ToolAbort);
        rt.veto_tick(LoopFoldVeto::QueuedCommand);
        assert_eq!(
            rt.settle_tick(at(1_010), quiet_span()),
            Some(LoopFoldOutcome::Vetoed {
                reason: LoopFoldVeto::ToolAbort,
            })
        );
    }

    /// A turn that was not a loop tick settles nothing — the edge runs after
    /// EVERY turn, so this is what keeps a normal turn out of the streak.
    #[test]
    fn a_turn_that_was_not_a_tick_settles_nothing() {
        let rt = LoopRuntime::default();
        rt.mark_noop_reported(true);
        assert_eq!(rt.settle_tick(at(1_000), quiet_span()), None);
        assert_eq!(rt.noop_streak(), None);
    }

    /// `begin_tick` clears the previous tick's marks, so a stale veto or a
    /// stale `noop` can never decide the next one.
    #[test]
    fn beginning_a_tick_clears_the_previous_ticks_marks() {
        let rt = LoopRuntime::default();
        rt.begin_tick_at("p".into(), at(1_000));
        rt.veto_tick(LoopFoldVeto::ToolDenial);
        rt.begin_tick_at("p".into(), at(2_000));
        rt.mark_noop_reported(true);
        assert!(matches!(
            rt.settle_tick(at(2_005), quiet_span()),
            Some(LoopFoldOutcome::Folded { streak: 1, .. })
        ));
    }

    /// PARITY the counter's `span_len` / `tool_uses`: whatever the turn tallied
    /// is what the fold reports, not a placeholder.
    #[test]
    fn the_span_counts_reach_the_fold_outcome() {
        let rt = LoopRuntime::default();
        rt.begin_tick_at("p".into(), at(1_000));
        rt.mark_noop_reported(true);
        let span = LoopSpanCounts {
            tool_uses: 3,
            span_len: 7,
        };
        assert_eq!(
            rt.settle_tick(at(1_012), span),
            Some(LoopFoldOutcome::Folded {
                streak: 1,
                since: at(1_000),
                duration_secs: 12,
                span,
            })
        );
    }

    /// The in-flight marker is READ, not taken: the keepalive and user-abort
    /// edges run after the fold and still need it.
    #[test]
    fn settling_leaves_the_in_flight_prompt_for_the_later_edges() {
        let rt = LoopRuntime::default();
        rt.begin_tick_at("tick".into(), at(1_000));
        rt.mark_noop_reported(true);
        rt.settle_tick(at(1_001), quiet_span());
        assert_eq!(rt.take_in_flight_prompt().as_deref(), Some("tick"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests touch the process-global DELIVERY state + env var, so serialize them
    // via the crate-shared lock (shared with `wakeup::tests` resolution tests).
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        let g = super::TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        reset_autonomous_loop_delivered();
        std::env::remove_var("CLAUDE_CODE_LOOP_PERSISTENT");
        telemetry::test_clear_flag("tengu_kairos_loop_persistent");
        platform_api::session_flags::set_agent_push_notif_enabled(false);
        std::env::remove_var("CLAUDE_CODE_LOOP_KEEPALIVE");
        telemetry::test_clear_flag("tengu_kairos_loop_keepalive");
        g
    }

    #[test]
    fn sentinel_constants_locked() {
        let _g = guard();
        assert_eq!(AUTONOMOUS_LOOP_SENTINEL, "<<autonomous-loop>>");
        assert_eq!(
            AUTONOMOUS_LOOP_DYNAMIC_SENTINEL,
            "<<autonomous-loop-dynamic>>"
        );
        assert_eq!(LOOP_FILE_SENTINEL, "<<loop.md>>");
        assert_eq!(LOOP_FILE_DYNAMIC_SENTINEL, "<<loop.md-dynamic>>");
    }

    #[test]
    fn sentinel_predicates() {
        let _g = guard();
        assert!(is_autonomous_loop_sentinel("<<autonomous-loop>>"));
        assert!(is_autonomous_loop_sentinel("<<autonomous-loop-dynamic>>"));
        assert!(!is_autonomous_loop_sentinel("<<loop.md>>"));
        assert!(is_loop_file_sentinel("<<loop.md>>"));
        assert!(is_loop_file_sentinel("<<loop.md-dynamic>>"));
        assert!(is_loop_default_sentinel("<<autonomous-loop-dynamic>>"));
        assert!(is_loop_default_sentinel("<<loop.md>>"));
        assert!(!is_loop_default_sentinel("5m /foo"));
    }

    // PARITY 2.1.263: the preambles are the bundled markdown files
    // `loopAutonomousPreamble-07qcyhv4.md` (4972 bytes) and
    // `loopAutonomousPreamblePersistent-3zqtkrvg.md` (5380 bytes) — ASCII
    // hyphens, a blank line after the heading and between paragraphs.
    #[test]
    fn preamble_default_is_the_bundled_file_verbatim() {
        let _g = guard();
        assert_eq!(AUTONOMOUS_LOOP_PREAMBLE, PREAMBLE_DEFAULT);
        assert_eq!(PREAMBLE_DEFAULT.len(), 4972);
        assert_eq!(PREAMBLE_PERSISTENT.len(), 5380);
        assert!(PREAMBLE_DEFAULT.starts_with("# Autonomous loop check\n\nYou're being invoked"));
        assert!(PREAMBLE_DEFAULT.contains("without the user driving every step - finishing things"));
        assert!(
            !PREAMBLE_DEFAULT.contains('\u{2014}'),
            "the bundled file has no em-dashes"
        );
        assert!(!PREAMBLE_PERSISTENT.contains('\u{2014}'));
        assert!(PREAMBLE_DEFAULT.contains("You're a steward, not an initiator."));
        assert!(PREAMBLE_DEFAULT.contains("\n\n## What to act on\n\n"));
        assert!(PREAMBLE_DEFAULT.ends_with("building together.\n"));
        assert!(!PREAMBLE_DEFAULT.contains("the *spirit* of the task"));
    }

    #[test]
    fn preamble_persistent_selected_by_env() {
        let _g = guard();
        assert_eq!(get_autonomous_loop_preamble(), PREAMBLE_DEFAULT);
        std::env::set_var("CLAUDE_CODE_LOOP_PERSISTENT", "1");
        assert!(is_loop_persistent_preamble_enabled());
        assert_eq!(get_autonomous_loop_preamble(), PREAMBLE_PERSISTENT);
        assert!(PREAMBLE_PERSISTENT.contains("the *spirit* of the task"));
        assert!(PREAMBLE_PERSISTENT.contains("Persistence is the point of autonomous mode."));
        std::env::remove_var("CLAUDE_CODE_LOOP_PERSISTENT");
    }

    #[test]
    fn persistent_preamble_env_uses_javascript_string_truthiness() {
        let _g = guard();
        for value in ["0", "false", "off", " ", "1"] {
            std::env::set_var("CLAUDE_CODE_LOOP_PERSISTENT", value);
            assert!(
                is_loop_persistent_preamble_enabled(),
                "{value:?} is nonempty"
            );
        }
        std::env::set_var("CLAUDE_CODE_LOOP_PERSISTENT", "");
        assert!(!is_loop_persistent_preamble_enabled());
        telemetry::test_set_flag("tengu_kairos_loop_persistent", true);
        assert!(
            is_loop_persistent_preamble_enabled(),
            "empty env falls through to flag"
        );
        std::env::remove_var("CLAUDE_CODE_LOOP_PERSISTENT");
        telemetry::test_clear_flag("tengu_kairos_loop_persistent");
    }

    #[test]
    fn gates_match_2_1_263() {
        let _g = guard();
        telemetry::test_clear_flag("tengu_kairos_loop_persistent");
        telemetry::test_clear_flag("tengu_kairos_loop_keepalive");
        telemetry::test_clear_flag("tengu_kairos_push_notifications");
        std::env::remove_var("CLAUDE_CODE_LOOP_PERSISTENT");
        std::env::remove_var("CLAUDE_CODE_LOOP_KEEPALIVE");
        // PARITY 2.1.263: `tengu_kairos_loop_prompt` / `tengu_kairos_loop_dynamic`
        // no longer exist — the sentinel resolvers and the dynamic /loop mode are
        // unconditional.
        assert!(is_loop_default_prompt_enabled());
        assert!(is_loop_dynamic_enabled());
        assert!(!is_loop_persistent_preamble_enabled());
        // Keepalive `YXn`: flag default TRUE …
        assert!(is_loop_keepalive_enabled());
        telemetry::test_set_flag("tengu_kairos_loop_keepalive", false);
        assert!(!is_loop_keepalive_enabled(), "flag off disables");
        // … and a DEFINED env var wins raw: any non-empty string is truthy, an
        // empty string is falsy (binary `if(e!==void 0)return e`).
        std::env::set_var("CLAUDE_CODE_LOOP_KEEPALIVE", "1");
        assert!(is_loop_keepalive_enabled(), "env arm");
        std::env::set_var("CLAUDE_CODE_LOOP_KEEPALIVE", "");
        assert!(!is_loop_keepalive_enabled(), "empty env is falsy");
        std::env::remove_var("CLAUDE_CODE_LOOP_KEEPALIVE");
        telemetry::test_clear_flag("tengu_kairos_loop_keepalive");

        // PERSISTENT: env || flag.
        telemetry::test_set_flag("tengu_kairos_loop_persistent", true);
        assert!(is_loop_persistent_preamble_enabled(), "flag arm");
        telemetry::test_clear_flag("tengu_kairos_loop_persistent");
        std::env::set_var("CLAUDE_CODE_LOOP_PERSISTENT", "1");
        assert!(is_loop_persistent_preamble_enabled(), "env arm");
        std::env::remove_var("CLAUDE_CODE_LOOP_PERSISTENT");

        // Push: flag alone is not enough.
        telemetry::test_set_flag("tengu_kairos_push_notifications", true);
        assert!(!is_push_notif_enabled());
        platform_api::session_flags::set_agent_push_notif_enabled(true);
        assert!(is_push_notif_enabled());
        platform_api::session_flags::set_agent_push_notif_enabled(false);
        telemetry::test_clear_flag("tengu_kairos_push_notifications");
    }

    #[test]
    fn autonomous_dynamic_first_then_subsequent() {
        let _g = guard();
        // First fire: full preamble + dynamic tick.
        let first = resolve_autonomous_loop_fire(AUTONOMOUS_LOOP_DYNAMIC_SENTINEL).unwrap();
        assert!(first.starts_with("# Autonomous loop check\n"));
        assert!(first.contains("# Autonomous loop tick (dynamic pacing)"));
        assert!(first.contains("set to the literal sentinel `<<autonomous-loop-dynamic>>`"));
        // Subsequent fire: short tick only, no preamble.
        let second = resolve_autonomous_loop_fire(AUTONOMOUS_LOOP_DYNAMIC_SENTINEL).unwrap();
        assert!(!second.starts_with("# Autonomous loop check"));
        assert!(second.starts_with("# Autonomous loop tick (dynamic pacing)"));
    }

    #[test]
    fn autonomous_cron_tick_says_do_not_call_schedulewakeup() {
        let _g = guard();
        let out = resolve_autonomous_loop_fire(AUTONOMOUS_LOOP_SENTINEL).unwrap();
        assert!(out.contains("# Autonomous loop tick\n"));
        assert!(out.contains("do not call ScheduleWakeup from this tick."));
        // Reset state then a subsequent cron fire drops the preamble.
        let second = resolve_autonomous_loop_fire(AUTONOMOUS_LOOP_SENTINEL).unwrap();
        assert!(second.starts_with("# Autonomous loop tick\n"));
    }

    #[test]
    fn non_sentinel_passes_through() {
        let _g = guard();
        let cwd = std::env::temp_dir();
        assert_eq!(
            resolve_loop_default_fire("5m /babysit-prs", &cwd),
            "5m /babysit-prs"
        );
        assert!(resolve_autonomous_loop_fire("5m /x").is_none());
        assert!(resolve_loop_file_fire("5m /x", &cwd).is_none());
    }

    #[test]
    fn sentinels_always_resolve_and_preamble_joins_with_rule() {
        // PARITY 2.1.263: no resolver gate — every sentinel resolves, and the
        // first delivery is `${preamble}\n\n---\n\n${tick}`.
        let _g = guard();
        let cwd = std::env::temp_dir();
        let first = resolve_loop_default_fire(AUTONOMOUS_LOOP_DYNAMIC_SENTINEL, &cwd);
        assert_ne!(first, AUTONOMOUS_LOOP_DYNAMIC_SENTINEL);
        let expected_head = format!(
            "{}\n\n---\n\n# Autonomous loop tick (dynamic pacing)\n\nRun the autonomous check",
            PREAMBLE_DEFAULT
        );
        assert!(first.starts_with(&expected_head), "{first}");
        assert!(first.contains("and `noop` set to `true` if this tick changed nothing (or `false` if it did) — otherwise the loop ends after this tick."));
        assert!(first.contains("\n\nIf a Monitor is armed (check TaskList)"));
        assert!(first.contains(
            "To stop the loop, call ScheduleWakeup with `stop: true` and TaskStop the monitor"
        ));
        assert_ne!(
            resolve_loop_default_fire(LOOP_FILE_DYNAMIC_SENTINEL, &cwd),
            LOOP_FILE_DYNAMIC_SENTINEL
        );
        // Cron-mode tick: blank line after the heading, no ScheduleWakeup call.
        let cron = tick_autonomous_cron();
        assert!(cron.starts_with("# Autonomous loop tick\n\nRun the autonomous check"));
        assert!(cron.ends_with("do not call ScheduleWakeup from this tick."));
    }

    #[test]
    fn loop_file_first_inlines_then_reminds() {
        let _g = guard();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("loop.md"), "- task A\n- task B\n").unwrap();
        // First fire: inlines the full loop.md contents (NO autonomous preamble —
        // the binary's sKi prefixes the preamble only on the loop.md-ABSENT path).
        let first = resolve_loop_file_fire(LOOP_FILE_DYNAMIC_SENTINEL, tmp.path()).unwrap();
        assert!(!first.contains("# Autonomous loop check"));
        assert!(first.starts_with("# /loop tick — tasks from "));
        assert!(first.contains("(the reminder on later fires refers back to this message).\n\n---\n\n- task A\n- task B\n\n---\n\n# /loop tick — loop.md tasks (dynamic pacing)\n\n"));
        assert!(first.contains("# /loop tick — loop.md tasks (dynamic pacing)"));
        // Second fire, unchanged content: short reminder tick only.
        let second = resolve_loop_file_fire(LOOP_FILE_DYNAMIC_SENTINEL, tmp.path()).unwrap();
        assert!(!second.contains("# Autonomous loop check"));
        // No "tasks from <path>" inline-block header on the unchanged reminder.
        assert!(!second.contains("# /loop tick — tasks from "));
        assert!(!second.contains("- task A"));
        assert!(second.starts_with("# /loop tick — loop.md tasks (dynamic pacing)"));
        // Edited content re-inlines.
        std::fs::write(tmp.path().join("loop.md"), "- task C\n").unwrap();
        let third = resolve_loop_file_fire(LOOP_FILE_DYNAMIC_SENTINEL, tmp.path()).unwrap();
        assert!(third.contains("# /loop tick — tasks from "));
        assert!(third.contains("- task C"));
    }

    #[test]
    fn loop_file_absent_falls_back_to_autonomous() {
        let _g = guard();
        let tmp = tempfile::tempdir().unwrap();
        // No loop.md present: first dynamic fire → preamble + absent-tick.
        let first = resolve_loop_file_fire(LOOP_FILE_DYNAMIC_SENTINEL, tmp.path()).unwrap();
        assert!(first.starts_with("# Autonomous loop check\n"));
        assert!(first.contains("# /loop tick — loop.md absent (dynamic pacing)"));
        // Subsequent absent fire: short absent-tick, no preamble.
        let second = resolve_loop_file_fire(LOOP_FILE_DYNAMIC_SENTINEL, tmp.path()).unwrap();
        assert!(!second.contains("# Autonomous loop check"));
        assert!(second.starts_with("# /loop tick — loop.md absent (dynamic pacing)"));
    }

    #[test]
    fn loop_file_truncation() {
        let _g = guard();
        let big = "x".repeat(LOOP_FILE_MAX_BYTES + 5000);
        let out = truncate_loop_file(&big);
        assert!(out.len() <= LOOP_FILE_MAX_BYTES + 200);
        assert!(out.ends_with(
            "\n\n> WARNING: loop.md was truncated to 25000 bytes. Keep the task list concise."
        ));
        // Short content is untouched.
        assert_eq!(truncate_loop_file("- task\n"), "- task\n");
    }

    #[test]
    fn loop_file_truncation_matches_javascript_utf16_slice() {
        let _g = guard();
        let short = "€".repeat(8334);
        assert_eq!(truncate_loop_file(&short), short);
        let footer =
            "\n\n> WARNING: loop.md was truncated to 25000 bytes. Keep the task list concise.";
        assert_eq!(
            truncate_loop_file(&"€".repeat(25001)),
            format!("{}{footer}", "€".repeat(25000))
        );
        assert_eq!(
            truncate_loop_file(&format!("{}😀x", "a".repeat(24999))),
            format!("{}�{footer}", "a".repeat(24999))
        );
        // lastIndexOf includes a newline exactly at the 25000-unit boundary.
        assert_eq!(
            truncate_loop_file(&format!("{}\nx", "a".repeat(25000))),
            format!("{}{footer}", "a".repeat(25000))
        );
    }

    #[test]
    fn latest_2_1_270_tick_oracle_all_variants() {
        let _g = guard();
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("bundled/loop_ticks_2_1_270.json")).unwrap();
        for case in cases.as_array().unwrap() {
            let persistent = case["persistent"].as_bool().unwrap();
            let push = case["push"].as_bool().unwrap();
            telemetry::test_set_flag("tengu_kairos_loop_persistent", persistent);
            telemetry::test_set_flag("tengu_kairos_push_notifications", push);
            platform_api::session_flags::set_agent_push_notif_enabled(push);
            let name = case["name"].as_str().unwrap();
            let actual = match name {
                "auto_cron" => tick_autonomous_cron(),
                "auto_dynamic" => tick_autonomous_dynamic(),
                "file_cron" => tick_loopfile_cron(),
                "file_dynamic" => tick_loopfile_dynamic(),
                "absent_dynamic" => tick_loopfile_absent_dynamic(),
                _ => panic!("unknown oracle case"),
            };
            assert_eq!(
                actual.as_bytes(),
                case["text"].as_str().unwrap().as_bytes(),
                "{name}, persistent={persistent}, push={push}"
            );
        }
        telemetry::test_clear_flag("tengu_kairos_loop_persistent");
        telemetry::test_clear_flag("tengu_kairos_push_notifications");
        platform_api::session_flags::set_agent_push_notif_enabled(false);
    }

    #[test]
    fn push_notif_addendum_off_by_default() {
        let _g = guard();
        assert_eq!(push_notif_addendum(true), "");
        assert_eq!(push_notif_addendum(false), "");
    }

    #[test]
    fn dynamic_tick_includes_monitor_addendum() {
        let _g = guard();
        let out = tick_autonomous_dynamic();
        assert!(out.contains("\n\nIf a Monitor is armed (check TaskList)"));
        assert!(out.contains("handle the event before deciding whether to re-arm. To stop the loop, call ScheduleWakeup with `stop: true` and TaskStop the monitor (use TaskList to find its task ID if no longer in context)."));
    }

    #[test]
    fn reset_clears_delivery_state() {
        let _g = guard();
        let _ = resolve_autonomous_loop_fire(AUTONOMOUS_LOOP_DYNAMIC_SENTINEL);
        // After a fire the preamble is marked delivered.
        reset_autonomous_loop_delivered();
        // Next fire re-emits the preamble.
        let again = resolve_autonomous_loop_fire(AUTONOMOUS_LOOP_DYNAMIC_SENTINEL).unwrap();
        assert!(again.starts_with("# Autonomous loop check\n"));
    }
    #[test]
    fn session_delivery_is_isolated_and_reset_redelivers() {
        let _g = guard();
        let first = LoopRuntime::default();
        let second = LoopRuntime::default();
        let cwd = tempfile::tempdir().unwrap();
        let sentinel = AUTONOMOUS_LOOP_DYNAMIC_SENTINEL;
        let initial = first.resolve_loop_default_fire(sentinel, cwd.path());
        assert!(initial.starts_with(PREAMBLE_DEFAULT));
        assert_eq!(
            second.resolve_loop_default_fire(sentinel, cwd.path()),
            initial
        );
        let tick = tick_autonomous_dynamic();
        assert_eq!(first.resolve_loop_default_fire(sentinel, cwd.path()), tick);
        first.reset_autonomous_loop_delivered();
        assert_eq!(
            first.resolve_loop_default_fire(sentinel, cwd.path()),
            initial
        );
        assert_eq!(second.resolve_loop_default_fire(sentinel, cwd.path()), tick);
        first.reset();
        assert_eq!(
            first.resolve_loop_default_fire(sentinel, cwd.path()),
            initial
        );
        assert_eq!(
            first.resolve_loop_default_fire(" <<autonomous-loop-dynamic>> ", cwd.path()),
            " <<autonomous-loop-dynamic>> "
        );
    }

    #[test]
    fn loop_file_delivery_is_isolated_between_sessions() {
        let _g = guard();
        let first = LoopRuntime::default();
        let second = LoopRuntime::default();
        let cwd = tempfile::tempdir().unwrap();
        let file = cwd.path().join("loop.md");
        std::fs::write(&file, "- isolated task").unwrap();
        let sentinel = LOOP_FILE_DYNAMIC_SENTINEL;
        let initial = first.resolve_loop_default_fire(sentinel, cwd.path());
        assert!(initial.contains("- isolated task"));
        assert_eq!(
            second.resolve_loop_default_fire(sentinel, cwd.path()),
            initial
        );
        assert_eq!(
            first.resolve_loop_default_fire(sentinel, cwd.path()),
            tick_loopfile_dynamic()
        );
        std::fs::write(&file, "- edited task").unwrap();
        let changed = first.resolve_loop_default_fire(sentinel, cwd.path());
        assert!(changed.contains("- edited task"));
        assert_eq!(
            second.resolve_loop_default_fire(sentinel, cwd.path()),
            changed
        );
        first.reset();
        assert_eq!(
            first.resolve_loop_default_fire(sentinel, cwd.path()),
            changed
        );
        assert_eq!(
            second.resolve_loop_default_fire(sentinel, cwd.path()),
            tick_loopfile_dynamic()
        );
    }
    #[test]
    fn loop_file_reader_uses_project_root_then_live_cwd_and_node_utf8() {
        let _g = guard();
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("nested");
        std::fs::create_dir_all(root.path().join(".claude")).unwrap();
        std::fs::create_dir_all(root.path().join(".lingxi")).unwrap();
        std::fs::create_dir_all(cwd.join(".claude")).unwrap();
        std::fs::write(root.path().join(".lingxi/loop.md"), "wrong legacy file").unwrap();
        std::fs::write(cwd.join(".claude/loop.md"), "wrong cwd root").unwrap();
        std::fs::write(cwd.join("loop.md"), "cwd tasks").unwrap();
        let project_file = root.path().join(".claude/loop.md");
        std::fs::write(&project_file, b"\xef\xbb\xbfroot \xff task\n").unwrap();
        let found = read_loop_file_at(root.path(), &cwd).unwrap().unwrap();
        assert_eq!(found.path, project_file);
        assert_eq!(found.content, "root � task");
        std::fs::write(&project_file, "\u{feff} \n").unwrap();
        assert_eq!(
            read_loop_file_at(root.path(), &cwd).unwrap().unwrap().path,
            cwd.join("loop.md")
        );
        std::fs::remove_file(&project_file).unwrap();
        std::fs::remove_file(cwd.join("loop.md")).unwrap();
        assert!(
            read_loop_file_at(root.path(), &cwd).unwrap().is_none(),
            "legacy and cwd .claude are not extra fallbacks"
        );
    }

    #[cfg(unix)]
    #[test]
    fn loop_file_reader_skips_exact_upstream_errno_set() {
        for code in [
            libc::ENOENT,
            libc::EACCES,
            libc::EPERM,
            libc::ENOTDIR,
            libc::ELOOP,
            libc::ENAMETOOLONG,
            libc::EROFS,
            libc::EISDIR,
        ] {
            assert!(skippable_loop_file_error(
                &std::io::Error::from_raw_os_error(code)
            ));
        }
        for code in [libc::EIO, libc::EMFILE, libc::ENFILE] {
            assert!(!skippable_loop_file_error(
                &std::io::Error::from_raw_os_error(code)
            ));
        }
    }

    #[test]
    fn checked_session_resolution_uses_explicit_project_root() {
        let _g = guard();
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("nested");
        std::fs::create_dir_all(root.path().join(".claude")).unwrap();
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(root.path().join(".claude/loop.md"), "root tasks").unwrap();
        let rt = LoopRuntime::default();
        let first = rt
            .try_resolve_loop_default_fire(LOOP_FILE_DYNAMIC_SENTINEL, root.path(), &cwd)
            .unwrap();
        assert!(first.contains("root tasks"));
        assert_eq!(
            rt.try_resolve_loop_default_fire(LOOP_FILE_DYNAMIC_SENTINEL, root.path(), &cwd)
                .unwrap(),
            tick_loopfile_dynamic()
        );
    }
}
