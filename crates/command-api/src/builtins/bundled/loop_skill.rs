//! Bundled `/loop` prompt builders, audited against Claude Code 2.1.270.
//!
//! Empty input starts a self-paced autonomous loop; interval-only input starts
//! its cron variant; other input gets the combined cron/self-paced prompt.
//! Oracle fixtures execute the extracted official JavaScript with deterministic
//! feature gates. See tests/fixtures/loop_2_1_270/generate.mjs and provenance.json.
//! Cloud offers are unavailable in this runtime and use the upstream disabled
//! branch. Monitor and push-notification prompt variants follow their tool gates.

use crate::BundledPromptFn;

// Feature-flag overrides are process-global. Keep every unit test in this crate
// that exercises `LoopPromptFn` on one lock so flag-on cases cannot transiently
// change the result observed by a sibling test module.
#[cfg(test)]
pub(crate) static LOOP_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Binary `VSt` (cc_all.txt:521947) — default interval when none is parsed.
const DEFAULT_INTERVAL: &str = "10m";

/// Binary `dZm` (cc_all.txt:521947) — returned verbatim when the trimmed args
/// are empty (or interval-only). `${VSt}` → `10m`. No trailing newline (the
/// binary template literal ends at the last example line).
// PARITY: binary dZm (cc_all.txt:521947); matches BLOCK D (binary-loop-reference).
const USAGE_MESSAGE: &str = "Usage: /loop [interval] <prompt>
Run a prompt or slash command on a recurring interval.
Intervals: Ns, Nm, Nh, Nd (e.g. 5m, 30m, 2h, 1d). Minimum granularity is 1 minute.
If no interval is specified, defaults to 10m.
Examples:
  /loop 5m /babysit-prs
  /loop 30m check the deploy
  /loop 1h /standup 1
  /loop check the deploy          (defaults to 10m)
  /loop check the deploy every 20m";

/// Binary `hZm` (cc_all.txt:521879) — the USAGE message for the DYNAMIC variant,
/// dispatch branch 2's empty-input arm. Unreachable in 2.1.263 (branch 1 claims
/// every empty input); kept because the constant is still the binary's text.
// PARITY: binary hZm (cc_all.txt:521879-521889).
const USAGE_MESSAGE_DYNAMIC: &str = "Usage: /loop [interval] <prompt>
Run a prompt or slash command on a recurring interval — or with no interval, let the model self-pace based on the task.
Intervals: Ns, Nm, Nh, Nd (e.g. 5m, 30m, 2h, 1d). Minimum granularity is 1 minute.
If no interval is specified, the model picks a delay between iterations based on what it's doing.
Examples:
  /loop 5m /babysit-prs
  /loop 30m check the deploy
  /loop 1h /standup 1
  /loop check the deploy          (dynamic — model picks delays)
  /loop check the deploy every 20m";

/// Binary `fZm(e)` head (cc_all.txt:521800-521846) — everything before the final
/// `## Input\n${e}`. BYTE-EXACT to the shipped binary with the cloud-offer
/// splices DISABLED. The 2.1.191 binary collapsed the older leaked `loop.ts`
/// blank-line spacing to SINGLE newlines everywhere EXCEPT the `${zpc()}` splice
/// point: the binary emits `…→ show usage\n${zpc()}\n## Interval → cron`, so when
/// `zpc()` returns `""` (the default) the result is `…→ show usage\n\n## Interval`
/// — a preserved blank line, which this const reproduces. Interpolations:
/// `${xw}` = `CronCreate`, `${VSt}` = `10m`, `${lte}` = `7`, `${t9}` = `CronDelete`.
///
// PARITY: binary fZm (cc_all.txt:521800-521846).
// PARITY-TODO: fZm splices `${zpc()}` (own line, after the parsing examples) and
// `${Ypc()}` (inline, after the confirm step `${t9} (include the job ID).${Ypc()}`)
// — cloud-offer / "Runs until you close this session" lines gated on
// `tengu_surreal_dali` + `allow_remote_sessions` (cc_all.txt:521830/521844). Both
// default OFF and BOTH guards end with `return""` (verified), and the port has no
// remote-session subsystem, so both render "" — this const matches the default-
// disabled binary path: `zpc()` leaves a blank line before `## Interval` (its own
// line collapses to `\n\n`); `Ypc()` is inline so it leaves a single `\n` before
// step 3.
fn cron_prompt_head() -> String {
    format!(
        "# /loop — schedule a recurring prompt
Parse the input below into `[interval] <prompt…>` and schedule it with CronCreate.
## Parsing (in priority order)
1. **Leading token**: if the first whitespace-delimited token matches `^\\d+[smhd]$` (e.g. `5m`, `2h`), that's the interval; the rest is the prompt.
2. **Trailing \"every\" clause**: otherwise, if the input ends with `every <N><unit>` or `every <N> <unit-word>` (e.g. `every 20m`, `every 5 minutes`, `every 2 hours`), extract that as the interval and strip it from the prompt. Only match when what follows \"every\" is a time expression — `check every PR` has no interval.
3. **Default**: otherwise, interval is `{default}` and the entire input is the prompt.
If the resulting prompt is empty, show usage `/loop [interval] <prompt>` and stop — do not call CronCreate.
Examples:
- `5m /babysit-prs` → interval `5m`, prompt `/babysit-prs` (rule 1)
- `check the deploy every 20m` → interval `20m`, prompt `check the deploy` (rule 2)
- `run tests every 5 minutes` → interval `5m`, prompt `run tests` (rule 2)
- `check the deploy` → interval `{default}`, prompt `check the deploy` (rule 3)
- `check every PR` → interval `{default}`, prompt `check every PR` (rule 3 — \"every\" not followed by time)
- `5m` → empty prompt → show usage

## Interval → cron
Supported suffixes: `s` (seconds, rounded up to nearest minute, min 1), `m` (minutes), `h` (hours), `d` (days). Convert:
| Interval pattern      | Cron expression     | Notes                                    |
|-----------------------|---------------------|------------------------------------------|
| `Nm` where N ≤ 59   | `*/N * * * *`     | every N minutes                          |
| `Nm` where N ≥ 60   | `0 */H * * *`     | round to hours (H = N/60, must divide 24)|
| `Nh` where N ≤ 23   | `0 */N * * *`     | every N hours                            |
| `Nd`                | `0 0 */N * *`     | every N days at midnight local           |
| `Ns`                | treat as `ceil(N/60)m` | cron minimum granularity is 1 minute  |
**If the interval doesn't cleanly divide its unit** (e.g. `7m` → `*/7 * * * *` gives uneven gaps at :56→:00; `90m` → 1.5h which cron can't express), pick the nearest clean interval and tell the user what you rounded to before scheduling.
## Action
1. Call CronCreate with:
   - `cron`: the expression from the table above
   - `prompt`: the parsed prompt from above, verbatim (slash commands are passed through unchanged)
   - `recurring`: `true`
2. Briefly confirm: what's scheduled, the cron expression, the human-readable cadence, that recurring tasks auto-expire after 7 days, and that they can cancel sooner with CronDelete (include the job ID).
3. **Then immediately execute the parsed prompt now** — don't wait for the first cron fire. If it's a slash command, invoke it via the Skill tool; otherwise act on it directly.",
        default = DEFAULT_INTERVAL,
    )
}

/// Binary `fZm(e)` (cc_all.txt:521800) — `${head}\n## Input\n${e}` with `${e}`
/// the trimmed input. SINGLE newline before `## Input` (binary template).
// PARITY: binary fZm tail `## Input\n${e}` (cc_all.txt:521846).
fn build_prompt(args: &str) -> String {
    format!(
        "{head}\n## Input\n{args}",
        head = cron_prompt_head(),
        args = args,
    )
}

// ── Flag-on (autonomous-default / dynamic-pacing) builders ───────────────────
//
// These implement the binary `getPromptForCommand` branches that were gated on
// `isLoopDefaultPromptEnabled()` and `q_e()` before 2.1.263. Both flags are gone
// from the binary, so these builders are the shipped dispatch.

use std::sync::OnceLock;

use cron::{
    get_autonomous_loop_preamble, is_loop_default_prompt_enabled, is_loop_dynamic_enabled,
    log_autonomous_loop_activation, note_loop_invoked, LoopFile, AUTONOMOUS_LOOP_DYNAMIC_SENTINEL,
    AUTONOMOUS_LOOP_SENTINEL, LOOP_FILE_DYNAMIC_SENTINEL, LOOP_FILE_SENTINEL,
};
use regex::Regex;

// Tool-name interpolations (binary `Kh`/`IA`/`AI`/`eP`/`xw`/`t9`).
const SCHEDULE_WAKEUP: &str = "ScheduleWakeup"; // Kh
const MONITOR: &str = "Monitor"; // IA
const TASK_LIST: &str = "TaskList"; // AI
const TASK_STOP: &str = "TaskStop"; // eP
const CRON_CREATE: &str = "CronCreate"; // xw
const CRON_DELETE: &str = "CronDelete"; // t9

/// Binary `lZm` (cc_all.txt:521947) — interval-only matcher `^\d+[smhd]$`.
fn interval_only_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[0-9]+[smhd]$").unwrap())
}

/// Binary `cZm` (cc_all.txt:521947) — trailing "every <N> <unit>" matcher.
fn every_clause_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i-u:^every)[\t\n\x0B\x0C\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]+([0-9]+)[\t\n\x0B\x0C\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]*(?i-u:(s|sec|secs|second|seconds|m|min|mins|minute|minutes|h|hr|hrs|hour|hours|d|day|days))[\t\n\x0B\x0C\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]*$",
        )
        .unwrap()
    })
}

/// Binary `uZm(e)` (cc_all.txt:521920) — normalize a parsed `every` clause to a
/// canonical interval token: `s`→`Ns`, `h`→`Nh`, `d`→`Nd`, else `Nm`.
fn normalize_every(num: &str, unit: &str) -> String {
    let u = unit.to_ascii_lowercase();
    if u.starts_with('s') {
        format!("{num}s")
    } else if u.starts_with('h') {
        format!("{num}h")
    } else if u.starts_with('d') {
        format!("{num}d")
    } else {
        format!("{num}m")
    }
}

/// Binary `zpc()` (cc_all.txt:521830) — cloud-offer splice. Gated on
/// `tengu_surreal_dali` && `allow_remote_sessions` (both default off) and the
/// port has no remote-session subsystem → always "".
fn cloud_offer_block() -> &'static str {
    ""
}

/// Binary `Ypc()` (cc_all.txt:521844) — inline "Runs until you close this
/// session" confirmation line. Same gates as [`cloud_offer_block`] → "".
fn remote_confirm_line() -> &'static str {
    ""
}

/// Binary `Kpc()` (cc_all.txt:521844) — the `PushNotification` "send a one-line
/// outcome before you stop" splice: `return Yke()?" Before you stop, …":""`.
/// Gated on the shared `Yke()` (`cron::is_push_notif_enabled`), so it is
/// structurally 1:1 — it renders "" only because the push-notif flag/setting
/// default off (no live GrowthBook), matching the shipped binary. Note the
/// leading space (the binary splices it inline after the step-6 sentence).
fn push_outcome_line() -> &'static str {
    if cron::is_push_notif_enabled() {
        " Before you stop, send a one-line outcome via PushNotification — the user may be away and waiting to hear it's done. Skip this if you're stopping because the user just told you to; they're already here."
    } else {
        ""
    }
}

/// Binary `pZm` (cc_all.txt:521920) — the interval→cron conversion table, used by
/// the dynamic-flag `gZm` builder. Byte-exact incl. the trailing blank line +
/// rounding note (`≤`/`≥` are U+2264/U+2265; `→` is U+2192).
const CRON_TABLE: &str = "| Interval pattern      | Cron expression     | Notes                                    |
|-----------------------|---------------------|------------------------------------------|
| `Nm` where N ≤ 59   | `*/N * * * *`     | every N minutes                          |
| `Nm` where N ≥ 60   | `0 */H * * *`     | round to hours (H = N/60, must divide 24)|
| `Nh` where N ≤ 23   | `0 */N * * *`     | every N hours                            |
| `Nd`                | `0 0 */N * *`     | every N days at midnight local           |
| `Ns`                | treat as `ceil(N/60)m` | cron minimum granularity is 1 minute  |

**If the interval doesn't cleanly divide its unit** (e.g. `7m` → `*/7 * * * *` gives uneven gaps at :56→:00; `90m` → 1.5h which cron can't express), pick the nearest clean interval and tell the user what you rounded to before scheduling.";

/// 2.1.270 `g` / `w`: bounded monitors must be re-armed after expiry.
fn monitor_arm(timeout: Option<u64>) -> String {
    match timeout {
        Some(ms) => format!("arm one now with `timeout_ms: {ms}`"),
        None => "arm one now with `persistent: true`".to_string(),
    }
}

fn monitor_rearm(timeout: Option<u64>, unit: &str) -> String {
    match timeout {
        Some(ms) => format!("A monitor expires after at most {} minutes and tells you; on later {unit} call TaskList first and re-arm only if no monitor for it is still running.", ms / 60000),
        None if unit == "iterations" => "Arm once; on later iterations call TaskList first and skip this step if a monitor is already running.".to_string(),
        None => "Arm once; on later ticks call TaskList first and skip if a monitor is already running.".to_string(),
    }
}

fn monitor_timeout() -> Option<u64> {
    cron::bounded_monitors_enabled().then(cron::bounded_monitor_timeout_ms)
}

/// 2.1.263 `A(e)` — the `/loop <input>` prompt builder: parsing rules, the
/// fixed-interval (cron) mode and the dynamic (ScheduleWakeup) mode. `${T()}`
/// (cloud offer) and `${I()}` (session-only line) are claude.ai-only and
/// render "" here; `${y()}` is the push-notification outcome line.
/// Seven-day expiry wording is preserved verbatim.
fn build_dynamic_prompt(args: &str) -> String {
    build_dynamic_prompt_with_monitor(args, monitor_timeout())
}

fn build_dynamic_prompt_with_monitor(args: &str, timeout: Option<u64>) -> String {
    let dynamic = format!(
        "The user wants you to self-pace. Decide what makes the next iteration worth running — a passage of time, or an observable event.

1. **Run the parsed prompt now.** If it's a slash command, invoke it via the Skill tool; otherwise act on it directly.
2. **If the next run is gated on an event** (CI finishing, a log line matching, a file changing, a PR comment) and no Monitor is already running for it: {arm}. Its events arrive as `<task-notification>` messages and wake this loop immediately — you do not wait for the ScheduleWakeup deadline. {rearm}
3. **Briefly confirm**: that you're self-pacing, whether a Monitor is the primary wake signal, that you ran the task now, and what fallback delay you're about to pick. Write this as text *before* calling ScheduleWakeup — the turn ends as soon as that tool returns.
4. **Then, as the last action of this turn, decide whether the loop continues.** If the task needs another iteration, call ScheduleWakeup with:
   - `delaySeconds`: with a Monitor armed this is the **fallback heartbeat** — how long to wait if no event fires (lean 1200–1800s; idle ticks more frequent than the task needs are pure overhead). Without a Monitor this is the cadence — pick based on what you observed. Read the tool's own description for cache-aware delay guidance.
   - `reason`: one short sentence on why you picked that delay.
   - `prompt`: the full original /loop input verbatim, prefixed with `/loop ` so the next firing re-enters this skill and continues the loop. For example, if the user typed `/loop check the deploy`, pass `/loop check the deploy` as the prompt.
   - `noop`: `true` if this tick changed nothing (\"still waiting\", \"quiet hold\"); `false` if it did something worth keeping. Consecutive `noop: true` ticks collapse in the terminal.
   If it doesn't need another iteration, stop instead (step 6) — re-arming is a per-turn choice, not a default.
5. **If you were woken by a `<task-notification>`** rather than this prompt: handle the event in the context of the loop task, then make the same decision. If the loop should continue, call ScheduleWakeup again with the same `prompt` and the same 1200–1800s `delaySeconds` from step 4 (the Monitor remains the wake signal; the new wakeup is only the fallback heartbeat). If the event means the work is finished, stop (step 6).
6. **To stop the loop** — the task is complete, further iterations can't make progress, or the user asked you to stop — call ScheduleWakeup with `stop: true` (no other fields) and TaskStop any Monitor you armed (use TaskList to find the task ID if it is no longer in context). Stopping is the loop's normal ending — the user can restart it anytime with /loop.{kpc}",
        kpc = push_outcome_line(),
        arm = monitor_arm(timeout),
        rearm = monitor_rearm(timeout, "iterations"),
    );
    format!(
        "# /loop — schedule a recurring or self-paced prompt

Parse the input below into `[interval] <prompt…>` and schedule it.

## Parsing (in priority order)

1. **Leading token**: if the first whitespace-delimited token matches `^\\d+[smhd]$` (e.g. `5m`, `2h`), that's the interval; the rest is the prompt.
2. **Trailing \"every\" clause**: otherwise, if the input ends with `every <N><unit>` or `every <N> <unit-word>` (e.g. `every 20m`, `every 5 minutes`, `every 2 hours`), extract that as the interval and strip it from the prompt. Only match when what follows \"every\" is a time expression — `check every PR` has no interval.
3. **No interval**: otherwise, the entire input is the prompt and you'll self-pace dynamically (see \"Dynamic mode\" below).

If the resulting prompt is empty, show usage `/loop [interval] <prompt>` and stop.

Examples:
- `5m /babysit-prs` → interval `5m`, prompt `/babysit-prs` (rule 1)
- `check the deploy every 20m` → interval `20m`, prompt `check the deploy` (rule 2)
- `run tests every 5 minutes` → interval `5m`, prompt `run tests` (rule 2)
- `check the deploy` → no interval → dynamic mode, prompt `check the deploy` (rule 3)
- `check every PR` → no interval → dynamic mode, prompt `check every PR` (rule 3 — \"every\" not followed by time)
- `5m` → empty prompt → show usage
{zpc}
## Fixed-interval mode (rules 1 and 2)

Convert the interval to a cron expression:

{table}

Then:
1. Call {CRON_CREATE} with: `cron` (the expression above), `prompt` (the parsed prompt verbatim), `recurring: true`.
2. Briefly confirm: what's scheduled, the cron expression, the human-readable cadence, that recurring tasks auto-expire after 7 days, and that the user can cancel sooner with {CRON_DELETE} (include the job ID).{ypc}
3. **Then immediately execute the parsed prompt now** — don't wait for the first cron fire. If it's a slash command, invoke it via the Skill tool; otherwise act on it directly.

## Dynamic mode (rule 3 — no interval)

{dynamic}

## Input

{args}",
        zpc = cloud_offer_block(),
        ypc = String::new(),
        table = CRON_TABLE,
        dynamic = dynamic,
        args = args,
    )
}

/// 2.1.263 `f(loopFile, dynamic, interval)` — the NO-PROMPT autonomous-default
/// builder. With a loop.md the file contents are inlined; otherwise the
/// activation is logged and the autonomous preamble is inlined.
fn build_autonomous(loop_file: Option<&LoopFile>, dynamic: bool, interval: &str) -> String {
    build_autonomous_with_monitor(loop_file, dynamic, interval, monitor_timeout())
}

fn build_autonomous_with_monitor(
    loop_file: Option<&LoopFile>,
    dynamic: bool,
    interval: &str,
    timeout: Option<u64>,
) -> String {
    let path = loop_file
        .map(|f| f.path.display().to_string())
        .unwrap_or_default();
    // `s` — the inlined-instructions section header.
    let header = match loop_file {
        Some(_) => format!("## Loop tasks (from {path})"),
        None => "## Autonomous-loop instructions (for the immediate execution and every fire)"
            .to_string(),
    };
    // `n` — the inlined instruction body.
    let body = match loop_file {
        Some(f) => f.content.clone(),
        None => {
            log_autonomous_loop_activation();
            get_autonomous_loop_preamble().to_string()
        }
    };
    // `h` — the human label for "run X now".
    let what = if loop_file.is_some() {
        "the loop.md tasks"
    } else {
        "the autonomous check"
    };

    if dynamic {
        let sentinel = if loop_file.is_some() {
            LOOP_FILE_DYNAMIC_SENTINEL
        } else {
            AUTONOMOUS_LOOP_DYNAMIC_SENTINEL
        };
        let heading = if loop_file.is_some() {
            format!("# /loop — loop.md tasks with dynamic pacing

The user invoked `/loop` with no prompt and no interval and has a loop-tasks file at `{path}`. Run those tasks now, then self-pace the next iteration via {SCHEDULE_WAKEUP} — no cron.")
        } else {
            format!("# /loop — autonomous default with dynamic pacing

The user invoked `/loop` with no prompt and no interval. Run the autonomous check now, then self-pace the next iteration via {SCHEDULE_WAKEUP} — no cron.")
        };
        let confirm = if loop_file.is_some() {
            format!("that you're running tasks from `{path}` in dynamic-pacing mode, that you ran the first tick now")
        } else {
            "that this is the autonomous default in dynamic-pacing mode, that you ran the check now"
                .to_string()
        };
        let action = format!(
            "1. **Run {what} now**, following the instructions inlined below.
2. **If the next tick is gated on an event** (CI finishing, a PR comment, a log line) and no {MONITOR} is already running for it: {arm}. Its events wake this loop immediately — you do not wait for the {SCHEDULE_WAKEUP} deadline. {rearm}
3. **Briefly confirm**: {confirm}, whether a {MONITOR} is the primary wake signal, and what fallback delay you're about to pick. Write this as text *before* calling {SCHEDULE_WAKEUP} — the turn ends as soon as that tool returns.
4. **Then, as the last action of this turn, decide whether the loop continues.** If the next check is worth running, call {SCHEDULE_WAKEUP} with:
   - `delaySeconds`: with a {MONITOR} armed this is the fallback heartbeat (lean 1200–1800s). Without one, pick based on what you observed this turn — quiet branch? wait longer. Lots in flight? wait shorter. Read the tool's own description for cache-aware delay guidance.
   - `reason`: one short sentence on why you picked that delay.
   - `prompt`: the literal string `{sentinel}` — the dynamic-mode sentinel expands at fire time to the full instructions (first fire / first fire post-compact / loop.md edited) or a dynamic-pacing-specific short reminder (subsequent fires). Do not pass the full instructions; that is handled automatically.
   - `noop`: `true` if this tick changed nothing (\"still waiting\", \"quiet hold\"); `false` if it did something worth keeping. Consecutive `noop: true` ticks collapse in the terminal.
   If it isn't, stop instead (step 6) — re-arming is a per-turn choice, not a default.
5. **If woken by a `<task-notification>`** rather than this prompt: handle the event, then make the same decision. If the loop should continue, call {SCHEDULE_WAKEUP} again with `{sentinel}` and the same 1200–1800s `delaySeconds` (the {MONITOR} remains the wake signal; the new wakeup is only the fallback heartbeat). If the event means the work is finished, stop (step 6).
6. **To stop the loop** — the task is complete, further iterations can't make progress, or the user asked you to stop — call {SCHEDULE_WAKEUP} with `stop: true` (no other fields) and {TASK_STOP} any {MONITOR} you armed (use {TASK_LIST} to find the task ID if it is no longer in context). Stopping is the loop's normal ending — the user can restart it anytime with /loop.{kpc}",
            kpc = push_outcome_line(),
            arm = monitor_arm(timeout),
            rearm = monitor_rearm(timeout, "ticks"),
        );
        return format!(
            "{heading}

## Action

{action}

{header}

{body}"
        );
    }

    let sentinel = if loop_file.is_some() {
        LOOP_FILE_SENTINEL
    } else {
        AUTONOMOUS_LOOP_SENTINEL
    };
    let heading = if loop_file.is_some() {
        format!("# /loop — schedule loop.md tasks

The user invoked `/loop` with no prompt (input was empty or just the interval `{interval}`) and has a loop-tasks file at `{path}`. Schedule a recurring cron that runs those tasks each tick, then run the first tick immediately.")
    } else {
        format!("# /loop — schedule the autonomous default

The user invoked `/loop` with no prompt (input was empty or just the interval `{interval}`). Schedule the autonomous-loop default and then run the first autonomous check immediately.")
    };
    let expands = if loop_file.is_some() {
        "it expands at fire time to the full loop.md contents on first delivery (and whenever loop.md has been edited since last fire), and to a short reminder on subsequent unchanged fires. The long instructions stay in the cached message-prefix."
    } else {
        "it expands at fire time to the full autonomous-loop instructions on first delivery, and to a short reminder on subsequent fires (the long instructions stay in the cached message-prefix)."
    };
    let confirm = if loop_file.is_some() {
        format!("what's scheduled, the cron expression, the human-readable cadence, that it's running tasks from `{path}`, that recurring tasks auto-expire after 7 days, and that the user can cancel sooner with {CRON_DELETE} (include the job ID).")
    } else {
        format!("what's scheduled, the cron expression, the human-readable cadence, that recurring tasks auto-expire after 7 days, and that they can cancel sooner with {CRON_DELETE} (include the job ID). Mention this is the autonomous default and that the autonomous-loop instructions are baked in.")
    };
    format!("{heading}

## Action

1. Convert `{interval}` to a 5-field cron expression. Supported suffixes: `s` → ceil to nearest minute, `m` (minutes), `h` (hours), `d` (days). Examples: `5m` → `*/5 * * * *`, `1h` → `0 * * * *`, `1d` → `0 0 * * *`. If the interval doesn't cleanly divide its unit, round to the nearest clean interval and tell the user what you rounded to.
2. Call {CRON_CREATE} with:
   - `cron`: the expression from step 1
   - `prompt`: the literal string `{sentinel}` — {expands}
   - `recurring`: `true`
3. Briefly confirm: {confirm}
4. **Then immediately run {what} now**, following the instructions inlined below. Don't wait for the first cron fire.

{header}

{body}")
}

/// The `/loop` bundled-skill prompt builder (port of `getPromptForCommand`,
/// `_Zm`, cc_all.txt:521920). Two-flag dispatch matrix:
///
/// 1. **No-prompt** (empty or interval-only) **&& `isLoopDefaultPromptEnabled()`**
///    → the autonomous-default builder `a(loopFile, dynamic)`. `dynamic` is true
///    only when the input is fully empty AND `q_e()` is on; else cron.
/// 2. → `gZm(n)`, the combined fixed-interval + dynamic prompt.
/// 3. the pre-2.1.263 cron-only variants (`dZm` / `fZm(n)`), now unreachable.
///
/// In 2.1.263 neither gate exists, so branches 1 and 2 are the shipped dispatch
/// and branch 3 is retained only for provenance.
pub struct LoopPromptFn;

impl BundledPromptFn for LoopPromptFn {
    fn build(&self, args: &str) -> String {
        self.build_input(args, true)
    }

    fn build_for_preload(&self, args: &str) -> String {
        self.build_input(args, false)
    }

    fn try_build_at(
        &self,
        args: &str,
        project_root: &std::path::Path,
        cwd: &std::path::Path,
        is_preload: bool,
    ) -> std::io::Result<String> {
        self.build_input_at(args, !is_preload, project_root, cwd)
    }
}

impl LoopPromptFn {
    fn build_input(&self, args: &str, is_invocation: bool) -> String {
        let cwd = std::env::current_dir().unwrap_or_default();
        self.build_input_at(args, is_invocation, &cwd, &cwd)
            .unwrap_or_else(|error| format!("Could not read loop.md: {error}"))
    }

    fn build_input_at(
        &self,
        args: &str,
        is_invocation: bool,
        project_root: &std::path::Path,
        cwd: &std::path::Path,
    ) -> std::io::Result<String> {
        // Binary `let n=e.trim()` (cc_all.txt:521921).
        let n = args.trim_matches(cron::is_loop_js_whitespace);
        let every = every_clause_re().captures(n);
        let empty = n.is_empty();
        // `s` = lZm.test(n) || r!==null — input is just an interval.
        let interval_only = interval_only_re().is_match(n) || every.is_some();

        // Branch 1: no-prompt autonomous default. `is_loop_default_prompt_enabled`
        // is a constant `true` in 2.1.263; the call is kept so the shape still
        // reads like the binary's dispatch.
        if (empty || interval_only) && is_loop_default_prompt_enabled() {
            // `i = r ? uZm(r) : n || VSt`.
            let interval = if let Some(caps) = &every {
                normalize_every(&caps[1], &caps[2])
            } else if empty {
                DEFAULT_INTERVAL.to_string()
            } else {
                n.to_string()
            };
            let loop_file = cron::autonomous_loop::read_loop_file_at(project_root, cwd)?;
            // `if(o&&q_e())return a(l,!0);return a(l,!1)` — dynamic only when empty.
            let dynamic = empty && is_loop_dynamic_enabled();
            if dynamic && is_invocation {
                // PARITY `K_n()`: the binary clears the loop-ended marker on the
                // empty-input arm, so a `/loop` after a `stop: true` is a live
                // loop again. Agent skill preloading must not clear it.
                note_loop_invoked();
            }
            return Ok(build_autonomous(loop_file.as_ref(), dynamic, &interval));
        }

        // Branch 2: the combined fixed-interval + dynamic prompt. `q_e()` is a
        // constant `true` in 2.1.263, so this branch always returns and branch 3
        // below is unreachable.
        if is_loop_dynamic_enabled() {
            if empty {
                // PARITY: `hZm()`.
                return Ok(USAGE_MESSAGE_DYNAMIC.to_string());
            }
            // 2.1.270 `IHn()` also clears the stop marker on explicit prompts.
            if is_invocation {
                note_loop_invoked();
            }
            return Ok(build_dynamic_prompt(n));
        }

        // Branch 3: shipped default — `!n ? dZm : fZm(n)`.
        Ok(if empty {
            USAGE_MESSAGE.to_string()
        } else {
            build_prompt(n)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_message_byte_exact() {
        // PARITY: binary dZm — the cron-mode usage text. In 2.1.263 it is dead
        // code (empty args route to the autonomous default), so only the
        // constant is pinned.
        let expected = "Usage: /loop [interval] <prompt>\nRun a prompt or slash command on a recurring interval.\nIntervals: Ns, Nm, Nh, Nd (e.g. 5m, 30m, 2h, 1d). Minimum granularity is 1 minute.\nIf no interval is specified, defaults to 10m.\nExamples:\n  /loop 5m /babysit-prs\n  /loop 30m check the deploy\n  /loop 1h /standup 1\n  /loop check the deploy          (defaults to 10m)\n  /loop check the deploy every 20m";
        assert_eq!(USAGE_MESSAGE, expected);
    }

    #[test]
    fn dynamic_usage_byte_exact() {
        // PARITY: binary hZm (cc_all.txt:521879-521889).
        let expected = "Usage: /loop [interval] <prompt>\nRun a prompt or slash command on a recurring interval — or with no interval, let the model self-pace based on the task.\nIntervals: Ns, Nm, Nh, Nd (e.g. 5m, 30m, 2h, 1d). Minimum granularity is 1 minute.\nIf no interval is specified, the model picks a delay between iterations based on what it's doing.\nExamples:\n  /loop 5m /babysit-prs\n  /loop 30m check the deploy\n  /loop 1h /standup 1\n  /loop check the deploy          (dynamic — model picks delays)\n  /loop check the deploy every 20m";
        assert_eq!(USAGE_MESSAGE_DYNAMIC, expected);
    }

    #[test]
    fn cron_head_byte_exact() {
        // PARITY: binary fZm head (cc_all.txt:521800-521846) — single newlines
        // except the blank line before `## Interval` from the empty `${zpc()}` on
        // its own line; ${default}=10m, tool names baked in, zpc()/Ypc() empty.
        let expected = "# /loop — schedule a recurring prompt\nParse the input below into `[interval] <prompt…>` and schedule it with CronCreate.\n## Parsing (in priority order)\n1. **Leading token**: if the first whitespace-delimited token matches `^\\d+[smhd]$` (e.g. `5m`, `2h`), that's the interval; the rest is the prompt.\n2. **Trailing \"every\" clause**: otherwise, if the input ends with `every <N><unit>` or `every <N> <unit-word>` (e.g. `every 20m`, `every 5 minutes`, `every 2 hours`), extract that as the interval and strip it from the prompt. Only match when what follows \"every\" is a time expression — `check every PR` has no interval.\n3. **Default**: otherwise, interval is `10m` and the entire input is the prompt.\nIf the resulting prompt is empty, show usage `/loop [interval] <prompt>` and stop — do not call CronCreate.\nExamples:\n- `5m /babysit-prs` → interval `5m`, prompt `/babysit-prs` (rule 1)\n- `check the deploy every 20m` → interval `20m`, prompt `check the deploy` (rule 2)\n- `run tests every 5 minutes` → interval `5m`, prompt `run tests` (rule 2)\n- `check the deploy` → interval `10m`, prompt `check the deploy` (rule 3)\n- `check every PR` → interval `10m`, prompt `check every PR` (rule 3 — \"every\" not followed by time)\n- `5m` → empty prompt → show usage\n\n## Interval → cron\nSupported suffixes: `s` (seconds, rounded up to nearest minute, min 1), `m` (minutes), `h` (hours), `d` (days). Convert:\n| Interval pattern      | Cron expression     | Notes                                    |\n|-----------------------|---------------------|------------------------------------------|\n| `Nm` where N ≤ 59   | `*/N * * * *`     | every N minutes                          |\n| `Nm` where N ≥ 60   | `0 */H * * *`     | round to hours (H = N/60, must divide 24)|\n| `Nh` where N ≤ 23   | `0 */N * * *`     | every N hours                            |\n| `Nd`                | `0 0 */N * *`     | every N days at midnight local           |\n| `Ns`                | treat as `ceil(N/60)m` | cron minimum granularity is 1 minute  |\n**If the interval doesn't cleanly divide its unit** (e.g. `7m` → `*/7 * * * *` gives uneven gaps at :56→:00; `90m` → 1.5h which cron can't express), pick the nearest clean interval and tell the user what you rounded to before scheduling.\n## Action\n1. Call CronCreate with:\n   - `cron`: the expression from the table above\n   - `prompt`: the parsed prompt from above, verbatim (slash commands are passed through unchanged)\n   - `recurring`: `true`\n2. Briefly confirm: what's scheduled, the cron expression, the human-readable cadence, that recurring tasks auto-expire after 7 days, and that they can cancel sooner with CronDelete (include the job ID).\n3. **Then immediately execute the parsed prompt now** — don't wait for the first cron fire. If it's a slash command, invoke it via the Skill tool; otherwise act on it directly.";
        assert_eq!(cron_prompt_head(), expected);
    }

    #[test]
    fn build_prompt_structure() {
        let _g = no_persistent_guard();
        // 2.1.263: `/loop <input>` always builds the dynamic prompt = head +
        // `\n## Input\n${e}`.
        let out = LoopPromptFn.build("5m /babysit-prs");
        assert!(out.starts_with(&build_dynamic_prompt("5m /babysit-prs")));
        assert!(out.ends_with("\n## Input\n\n5m /babysit-prs"));
        // No fabricated dynamic section is spliced in.
        assert!(!out.contains("## Self-pace"));
    }

    #[test]
    fn no_fabricated_dynamic_section() {
        let _g = no_persistent_guard();
        // The previous SYNTHESIZED `## Self-pace (dynamic) mode` addendum (which
        // existed NOWHERE in the binary) is gone.
        let out = LoopPromptFn.build("keep working on the migration");
        assert!(!out.contains("## Self-pace"));
        assert!(!out.contains("## Self-pace (dynamic) mode"));
    }

    #[test]
    fn build_prompt_trims_and_interpolates_args() {
        let _g = no_persistent_guard();
        // Binary `n=e.trim()` then `fZm(n)` — trimmed text appears verbatim under
        // `## Input`.
        let out = LoopPromptFn.build("  check the deploy  ");
        assert!(out.ends_with("\n## Input\n\ncheck the deploy"));
        assert!(out.starts_with("# /loop — schedule a recurring or self-paced prompt"));
        assert!(out.contains("ScheduleWakeup"));
    }

    // ── Flag-on (autonomous-default / dynamic-pacing) builders ───────────────
    //
    // These assert byte-exactness against fixtures reconstructed INDEPENDENTLY in
    // Python directly from the 2.1.191 binary's template literals (see
    // `tests/fixtures/loop_autonomous/`). The fixtures are a second transcription
    // of the binary; agreement with the Rust impl == byte-parity. The `None`
    // (autonomous, no loop.md) cases inline the default preamble `aJr` — assert it
    // ends with the binary preamble (kept apart so a preamble edit doesn't have to
    // re-touch every fixture).

    fn no_persistent_guard() -> std::sync::MutexGuard<'static, ()> {
        let g = LOOP_TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Establish the shipped-binary default: persistent off (preamble = aJr),
        // and the prompt/dynamic flags cleared. Gates now read the flag override
        // layer (binary `nt`), so reset it rather than env vars.
        std::env::remove_var("CLAUDE_CODE_LOOP_PERSISTENT");
        telemetry::test_clear_flag("tengu_kairos_loop_persistent");
        telemetry::test_clear_flag("tengu_breezy_crescent");
        telemetry::test_clear_flag("tengu_kairos_push_notifications");
        lingxi_core::host::session_flags::set_agent_push_notif_enabled(false);
        g
    }

    fn fixture(name: &str) -> String {
        match name {
            "a_auto_cron" => {
                include_str!("../../../tests/fixtures/loop_autonomous/a_auto_cron.txt").to_string()
            }
            "a_auto_dynamic" => {
                include_str!("../../../tests/fixtures/loop_autonomous/a_auto_dynamic.txt")
                    .to_string()
            }
            "a_loopfile_cron" => {
                include_str!("../../../tests/fixtures/loop_autonomous/a_loopfile_cron.txt")
                    .strip_suffix('\n')
                    .unwrap_or(include_str!(
                        "../../../tests/fixtures/loop_autonomous/a_loopfile_cron.txt"
                    ))
                    .to_string()
            }
            "a_loopfile_dynamic" => {
                include_str!("../../../tests/fixtures/loop_autonomous/a_loopfile_dynamic.txt")
                    .to_string()
            }
            "gZm" => include_str!("../../../tests/fixtures/loop_autonomous/gZm.txt")
                .strip_suffix('\n')
                .unwrap_or(include_str!(
                    "../../../tests/fixtures/loop_autonomous/gZm.txt"
                ))
                .to_string(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn autonomous_cron_builder_byte_exact() {
        let _g = no_persistent_guard();
        // a(null, u=false, i="10m") — the autonomous-default cron prompt.
        assert_eq!(build_autonomous(None, false, "10m"), fixture("a_auto_cron"));
        // Sanity: the inlined body IS the binary default preamble (aJr).
        assert!(build_autonomous(None, false, "10m").ends_with(cron::AUTONOMOUS_LOOP_PREAMBLE));
    }

    #[test]
    fn autonomous_dynamic_builder_byte_exact() {
        let _g = no_persistent_guard();
        // a(null, u=true, i unused) — the autonomous-default dynamic-pacing prompt.
        assert_eq!(
            build_autonomous(None, true, "10m"),
            fixture("a_auto_dynamic")
        );
        assert!(build_autonomous(None, true, "10m").ends_with(cron::AUTONOMOUS_LOOP_PREAMBLE));
    }

    #[test]
    fn loopfile_cron_builder_byte_exact() {
        let _g = no_persistent_guard();
        let lf = LoopFile {
            path: std::path::PathBuf::from("/tmp/proj/loop.md"),
            content: "- task A\n- task B".to_string(),
        };
        assert_eq!(
            build_autonomous(Some(&lf), false, "5m"),
            fixture("a_loopfile_cron")
        );
    }

    #[test]
    fn loopfile_dynamic_builder_byte_exact() {
        let _g = no_persistent_guard();
        let lf = LoopFile {
            path: std::path::PathBuf::from("/tmp/proj/loop.md"),
            content: "- task A\n- task B".to_string(),
        };
        assert_eq!(
            build_autonomous(Some(&lf), true, "10m"),
            fixture("a_loopfile_dynamic")
        );
    }

    #[test]
    fn dynamic_prompt_builder_byte_exact() {
        let _g = no_persistent_guard();
        // gZm("check the deploy") — the dynamic-flag prompt builder.
        assert_eq!(build_dynamic_prompt("check the deploy"), fixture("gZm"));
    }

    #[test]
    fn normalize_every_matches_uzm() {
        // Binary uZm: s→Ns, h→Nh, d→Nd, else Nm.
        assert_eq!(normalize_every("5", "minutes"), "5m");
        assert_eq!(normalize_every("2", "hours"), "2h");
        assert_eq!(normalize_every("30", "secs"), "30s");
        assert_eq!(normalize_every("1", "day"), "1d");
        assert_eq!(normalize_every("20", "m"), "20m");
    }

    #[test]
    fn dispatch_matches_2_1_263() {
        let _g = no_persistent_guard();
        // PARITY 2.1.263 `getPromptForCommand`: no feature gates. Empty input →
        // the autonomous default with DYNAMIC pacing (cwd has no loop.md in the
        // test sandbox → the `None` builder); interval-only → the autonomous
        // default on a cron; any other input → the dynamic prompt builder.
        let empty = LoopPromptFn.build("");
        assert!(
            empty.starts_with("# /loop — autonomous default with dynamic pacing"),
            "{empty}"
        );
        assert!(LoopPromptFn
            .build("   ")
            .starts_with("# /loop — autonomous default with dynamic pacing"));
        let interval_only = LoopPromptFn.build("5m");
        assert!(interval_only.starts_with("# /loop — schedule the autonomous default"));
        let with_prompt = LoopPromptFn.build("check the deploy");
        assert!(with_prompt.starts_with("# /loop — schedule a recurring or self-paced prompt"));
        assert!(with_prompt.ends_with("\n## Input\n\ncheck the deploy"));
        assert!(LoopPromptFn
            .build("5m /foo")
            .starts_with("# /loop — schedule a recurring or self-paced prompt"));
        // The cron-mode usage / prompt builders are unreachable from dispatch.
        assert_ne!(LoopPromptFn.build(""), USAGE_MESSAGE);
        assert_ne!(LoopPromptFn.build(""), USAGE_MESSAGE_DYNAMIC);
    }
    #[test]
    fn latest_2_1_270_command_oracle_all_variants() {
        let _g = no_persistent_guard();
        let cases: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/loop_2_1_270/commands.json"
        ))
        .unwrap();
        let file = LoopFile {
            path: "/tmp/proj/loop.md".into(),
            content: "- task A\n- task B".into(),
        };
        for case in cases.as_array().unwrap() {
            let persistent = case["persistent"].as_bool().unwrap();
            let push = case["push"].as_bool().unwrap();
            telemetry::test_set_flag("tengu_kairos_loop_persistent", persistent);
            telemetry::test_set_flag("tengu_kairos_push_notifications", push);
            lingxi_core::host::session_flags::set_agent_push_notif_enabled(push);
            let timeout = case["timeout"].as_u64().filter(|n| *n != 0);
            let name = case["name"].as_str().unwrap();
            let actual = match name {
                "prompt" => build_dynamic_prompt_with_monitor("check the deploy", timeout),
                "auto_dynamic" => build_autonomous_with_monitor(None, true, "10m", timeout),
                "auto_cron" => build_autonomous_with_monitor(None, false, "10m", timeout),
                "file_dynamic" => build_autonomous_with_monitor(Some(&file), true, "5m", timeout),
                "file_cron" => build_autonomous_with_monitor(Some(&file), false, "5m", timeout),
                _ => panic!("unknown oracle case"),
            };
            assert_eq!(
                actual.as_bytes(),
                case["text"].as_str().unwrap().as_bytes(),
                "{name}, persistent={persistent}, push={push}, timeout={timeout:?}"
            );
        }
        telemetry::test_clear_flag("tengu_kairos_loop_persistent");
        telemetry::test_clear_flag("tengu_kairos_push_notifications");
        lingxi_core::host::session_flags::set_agent_push_notif_enabled(false);
    }

    #[test]
    fn interval_parser_uses_ecmascript_characters() {
        let _g = no_persistent_guard();
        assert!(!interval_only_re().is_match("５m"));
        assert!(!every_clause_re().is_match("every ５ minutes"));
        assert!(every_clause_re().is_match("every\u{feff}5 minutes"));
        assert!(!every_clause_re().is_match("every\u{85}5 minutes"));
        assert_eq!(
            LoopPromptFn.build("\u{feff}check the deploy\u{feff}"),
            build_dynamic_prompt("check the deploy")
        );
        assert!(LoopPromptFn.build("\u{85}check").ends_with("\u{85}check"));
    }
    #[test]
    fn preloading_loop_does_not_restart_a_stopped_loop() {
        let _g = no_persistent_guard();
        for args in ["", "check the deploy", "5m"] {
            cron::autonomous_loop::set_loop_ended(true);
            let preloaded = LoopPromptFn.build_for_preload(args);
            assert!(
                cron::autonomous_loop::loop_ended(),
                "preload {args:?} restarted loop"
            );
            let invoked = LoopPromptFn.build(args);
            assert_eq!(preloaded, invoked, "preload preserves prompt bytes");
            assert_eq!(cron::autonomous_loop::loop_ended(), args == "5m");
        }
        cron::autonomous_loop::set_loop_ended(false);
    }
    #[test]
    fn checked_builder_reads_session_project_root_without_preload_activation() {
        let _g = no_persistent_guard();
        let root = std::env::temp_dir().join(format!(
            "lingxi-loop-reader-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cwd = root.as_path().join("nested");
        std::fs::create_dir_all(root.as_path().join(".claude")).unwrap();
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(root.as_path().join(".claude/loop.md"), "project tasks").unwrap();
        cron::autonomous_loop::set_loop_ended(true);
        let text = LoopPromptFn
            .try_build_at("", root.as_path(), &cwd, true)
            .unwrap();
        assert!(text.contains("project tasks"));
        assert!(text.starts_with("# /loop — loop.md tasks with dynamic pacing"));
        assert!(cron::autonomous_loop::loop_ended());
        cron::autonomous_loop::set_loop_ended(false);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
