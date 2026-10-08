//! `goal_checkin` — the goal check-in interstitial, NEW in Claude Code 2.1.238
//! (`CC_VER=2.1.220 oracle.sh count 'Goal check-in'` → **0**).
//!
//! When a goal (`/goal`, the session Stop hook that keeps looping until a
//! condition is met) is active and BACKGROUND WORK is still running, the oracle
//! does not evaluate the goal at all — it removes the goal's Stop hook for that
//! turn and defers. Deferral is not free: once the goal has been deferred for
//! longer than the check-in interval, the model is told, at turn end, that the
//! goal is still there and what is holding it up.
//!
//! Oracle anatomy (offsets into `~/.local/share/claude/versions/2.1.238`):
//!
//! * interval `Zil()` @**292038365**:
//!   ```js
//!   function Zil(){if(!it("tengu_saffron_wren",!0))return 0;
//!     return(V.CLAUDE_CODE_GOAL_CHECKIN_MINUTES??cUv)*60000}
//!   ```
//!   with `cUv = 30` @292041909. The GrowthBook default is **TRUE**, so unlike
//!   most of this family the feature is LIVE in a stock install.
//! * text builder `Szf(condition, deferredMs, tasks)` @**292038578** — see
//!   [`build_checkin_body`].
//! * deferral state machine `Tzf` @**292039748** + `wzf` @**292040130** — see
//!   [`GoalDeferralState::advance`].
//! * the deferring-task filter `_qf` @**292181184**:
//!   ```js
//!   function _qf(e){return Object.values(e).filter((t)=>!rR(t)&&
//!     !(t.type==="local_agent"&&t.agentType==="main-session")&&(xXo(t)||IXo(t)))}
//!   ```
//!   `xXo` = a non-terminal agent-ish task (`j2b = {local_agent, remote_agent,
//!   in_process_teammate, local_workflow}`), `IXo` = a non-terminal `local_bash`,
//!   `rR` = an observer agent.
//! * turn-end call site @**292174788**:
//!   ```js
//!   if(L&&M){let U=_qf(i.taskRegistry.all());
//!     if(U.length>0){ y=…the goal's Stop hook…;
//!       if(y){ i.sessionHooksRegistry.remove(zt(),"Stop",y);   // ⇐ NO evaluation this turn
//!         let W=wzf(L,U,Date.now()); _=W.nextGoal; Z=W.checkinText;
//!         if(Z!==void 0){let W=kn({content:Z,isMeta:!0});S.push(W),h.push(W),yield W}}}
//!     else if(L.deferredSince!==void 0){…clear the deferral fields…}}
//!   ```
//!
//! The check-in body is NOT `<system-reminder>`-wrapped in the oracle — it is a
//! plain `isMeta` user message (`kn({content:checkinText,isMeta:!0})`), unlike
//! the attachment family, which goes through `Zy`/`NT`. The port keeps that.
//!
//! # Divergence (reason)
//!
//! * Check-in text sanitation follows 2.1.270 `Z7n = Vue(SU(text))`:
//!   strip invisible/private/unassigned characters, collapse whitespace, map
//!   bracket/slash lookalikes, then HTML-escape. See `sanitize_checkin_text`.
//! * `Tzf`'s new-run detection keys on the minimum task startTime being
//!   strictly after lastDeferralPassAt. The internal snapshot carries this
//!   metadata separately from serialized Stop-hook payloads.

/// Claude Code 2.1.270 `Z7n` (`Vue(SU(text))`), before HTML escaping.
/// `SU` removes Unicode Cf/Co/Cn/default-ignorable characters, then `Nr`
/// strips controls and collapses ECMAScript whitespace; `Vue` maps only
/// bracket/slash lookalikes. It does not normalize or fold ordinary letters.
fn sanitize_checkin_text(text: &str) -> String {
    static INVISIBLE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let invisible = INVISIBLE.get_or_init(|| {
        regex::Regex::new(
            r"[\p{Cf}\p{Co}\p{Cn}\p{Default_Ignorable_Code_Point}\x00-\x08\x0E-\x1F\x7F-\x9F]",
        )
        .expect("valid oracle Unicode sanitation pattern")
    });
    let stripped = invisible.replace_all(text, "");
    let mut out = String::new();
    let mut pending_space = false;
    for ch in stripped.chars() {
        if (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}' {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(match ch {
            '\u{ff1c}' | '\u{fe64}' | '\u{2329}' | '\u{27e8}' | '\u{3008}' | '\u{2039}'
            | '\u{02c2}' | '\u{1438}' | '\u{276c}' | '\u{276e}' | '\u{2770}' | '\u{29fc}'
            | '\u{226e}' | '\u{227a}' | '\u{22d6}' => '<',
            '\u{ff1e}' | '\u{fe65}' | '\u{232a}' | '\u{27e9}' | '\u{3009}' | '\u{203a}'
            | '\u{02c3}' | '\u{1433}' | '\u{276d}' | '\u{276f}' | '\u{2771}' | '\u{29fd}'
            | '\u{226f}' | '\u{227b}' | '\u{22d7}' => '>',
            '\u{ff0f}' | '\u{2215}' | '\u{2044}' => '/',
            other => other,
        });
    }
    out
}

/// `cUv = 30` @292041909 — the default check-in interval, in minutes.
pub const DEFAULT_CHECKIN_MINUTES: i64 = 30;

/// `dUv = 120` @292041971 — the per-task line character cap (`w5(line, dUv)`).
pub const TASK_LINE_CHAR_CAP: usize = 120;

/// The `background_tasks[].type` labels that DEFER a goal evaluation — the port
/// side of `_qf`'s `xXo(t) || IXo(t)`.
///
/// These are the labels `crate::stop_hook_snapshot::build_background_tasks`
/// emits (claude-code `O1o`), i.e. already mapped from the wire task types:
/// `local_agent`→`subagent`, `local_workflow`→`workflow`, `local_bash`→`shell`,
/// `in_process_teammate`→`teammate`, `remote_agent`→`cloud session`.
/// The snapshot's `monitor` label means `monitor_mcp` / `monitor_ws`, which
/// 2.1.263 `n3t` / `r3t` excludes. A `local_bash` monitor is projected as
/// `shell` and already participates. `MCP task` and `dream` are excluded too.
pub const DEFERRING_TASK_LABELS: &[&str] =
    &["subagent", "workflow", "shell", "teammate", "cloud session"];

/// The `A9t` half of [`DEFERRING_TASK_LABELS`] — the agent-ish task types, in
/// this port's already-mapped label vocabulary. Upstream's predicate tests the
/// wire type against `w = new Set(["local_agent","remote_agent",
/// "in_process_teammate","local_workflow"])`; its complement within the
/// deferring set is the single `v9t` type `local_bash` (`"shell"` here).
///
/// Used by `tengu_goal_evaluated`'s deferred branch, which reports the two
/// counts separately rather than as a total and a remainder.
pub const GOAL_EVAL_AGENT_TASK_LABELS: &[&str] =
    &["subagent", "workflow", "teammate", "cloud session"];

/// `w5(e, t)` @281369905 — truncate to `t` UTF-16 code units and append the
/// dropped-character count:
///
/// ```js
/// function w5(e,t){if(e.length<=t)return e;let r=wo(e,t);
///   return `${r}… [+${e.length-r.length} chars]`}
/// ```
///
/// `wo` slices at a code-unit boundary (never inside a surrogate pair); the Rust
/// analogue walks `char_indices` and stops before the first char that would
/// cross the cap.
#[must_use]
pub fn truncate_with_char_count(s: &str, cap: usize) -> String {
    let total = s.encode_utf16().count();
    if total <= cap {
        return s.to_string();
    }
    let mut end = 0usize;
    let mut units = 0usize;
    for (idx, ch) in s.char_indices() {
        if units + ch.len_utf16() > cap {
            end = idx;
            break;
        }
        units += ch.len_utf16();
        end = idx + ch.len_utf8();
    }
    format!("{}\u{2026} [+{} chars]", &s[..end], total - units)
}

/// One background task that is currently deferring the goal — the port's stand-in
/// for `_qf`'s output elements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferringTask {
    /// `s.id`.
    pub id: String,
    /// Task start time in Unix epoch milliseconds, separate from hook JSON.
    pub start_time_ms: Option<u64>,
    /// `efr[s.type]` — the already-mapped type label.
    pub label: String,
    /// `s.type==="local_bash"&&!isMonitor ? s.command : s.description`.
    pub detail: String,
}

/// `Szf(e, t, r).body` @292038578 — the check-in text.
///
/// ```js
/// function Szf(e,t,r){let n=Math.max(1,Math.round(t/60000)),o=Ma(bzf(e));
///  if(r.length===0)return{…,body:`Goal check-in: \xAB${o}\xBB is still active. Its evaluation was deferred for ${n} min while background work ran, and that work is no longer running (it finished or was stopped without reporting back). Continue toward the goal.`};
///  let i=r.map((s)=>{…return Ma(w5(`- ${s.id} \xB7 ${l} \xB7 ${bzf(c)}`,dUv))});
///  return{…,body:`Goal check-in: \xAB${o}\xBB is still active, and evaluation has been deferred for ${n} min because background work is still running:\n${i.join("\n")}\nCheck on their progress (e.g. read their output). If they are progressing, say so briefly and keep waiting; if they are stuck or no longer needed, fix or stop them and continue toward the goal.`}}
/// ```
///
/// `«` / `»` are U+00AB / U+00BB; the minute count is
/// `max(1, round(deferred_ms / 60000))`, so a sub-minute deferral still reads
/// "1 min".
#[must_use]
pub fn build_checkin_body(condition: &str, deferred_ms: i64, tasks: &[DeferringTask]) -> String {
    // `Math.max(1, Math.round(t/60000))`. Integer round-half-up: for a
    // non-negative `t`, `(t + 30_000) / 60_000` is exactly `Math.round(t/60000)`
    // (JS rounds `.5` toward +Infinity), with no float cast.
    let minutes = deferred_ms.max(0).saturating_add(30_000) / 60_000;
    let minutes = minutes.max(1);
    let goal = super::sanitize::escape_reminder_html(&sanitize_checkin_text(condition));
    if tasks.is_empty() {
        return format!(
            "Goal check-in: \u{ab}{goal}\u{bb} is still active. Its evaluation was deferred for \
{minutes} min while background work ran, and that work is no longer running (it finished or was \
stopped without reporting back). Continue toward the goal."
        );
    }
    let lines = tasks
        .iter()
        .map(|task| {
            super::sanitize::escape_reminder_html(&truncate_with_char_count(
                &format!(
                    "- {} \u{b7} {} \u{b7} {}",
                    task.id,
                    task.label,
                    sanitize_checkin_text(&task.detail)
                ),
                TASK_LINE_CHAR_CAP,
            ))
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Goal check-in: \u{ab}{goal}\u{bb} is still active, and evaluation has been deferred for \
{minutes} min because background work is still running:\n{lines}\nCheck on their progress (e.g. \
read their output). If they are progressing, say so briefly and keep waiting; if they are stuck \
or no longer needed, fix or stop them and continue toward the goal."
    )
}

/// `Zil()` @292038365 — the check-in interval in milliseconds, or `0` (disabled).
///
/// `CLAUDE_CODE_GOAL_CHECKIN_MINUTES` overrides [`DEFAULT_CHECKIN_MINUTES`];
/// setting it to `0` disables the feature, exactly like the oracle's
/// `if(n===0)return{nextGoal:e,checkinText:void 0}`. There is no GrowthBook in
/// the port and `tengu_saffron_wren` defaults to TRUE upstream, so the feature
/// is ON by default here too.
#[must_use]
pub fn checkin_interval_ms() -> i64 {
    let minutes = std::env::var("CLAUDE_CODE_GOAL_CHECKIN_MINUTES")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|m| *m >= 0)
        .unwrap_or(DEFAULT_CHECKIN_MINUTES);
    minutes.saturating_mul(60_000)
}

/// Claude Code 2.1.241 backoff: first check-in after `base`, then `2x base`,
/// then every `4x base` thereafter.
#[must_use]
pub fn next_checkin_interval_ms(base_interval_ms: i64, checkin_count: u32) -> i64 {
    let multiplier = match checkin_count {
        0 => 1,
        1 => 2,
        _ => 4,
    };
    base_interval_ms.saturating_mul(multiplier)
}

/// `xns = 3` (2.1.266 `src_162329786.js` @4166900) — after this many IDLE
/// check-ins the timer keeps re-arming but stops delivering, until the user
/// speaks again.
pub const IDLE_CHECKIN_CAP: u32 = 3;

/// `qmt = 60000` — the floor on the idle timer's re-arm delay.
pub const IDLE_REARM_FLOOR_MS: i64 = 60_000;

/// `$S = 2147483647` (`src_158588265.js`) — upstream's `setTimeout` ceiling.
/// Retained so the delay arithmetic matches; `tokio::time::sleep` has no such
/// limit of its own.
pub const IDLE_MAX_DELAY_MS: i64 = 2_147_483_647;

/// `Rns` — appended to the check-in SUMMARY once the idle cap is reached.
///
/// ## Divergence (reason)
///
/// Upstream delivers `{summary, body}` and the summary drives a UI notification
/// row. LingXi's idle path enqueues only the body as an `isMeta` user message,
/// so this constant has no delivery channel yet; it is defined here so the pair
/// stays together and so a future summary channel does not re-derive it.
pub const IDLE_PAUSED_SUMMARY_SUFFIX: &str =
    " \u{b7} idle check-ins paused until your next message";

/// `Pns` — appended to the check-in BODY once the idle cap is reached
/// (`Mns(e)` = `{summary: e.summary+Rns, body: e.body+Pns}`).
pub const IDLE_PAUSED_BODY_SUFFIX: &str = " The agent won't wake this session for another check-in until the user sends a message, so say clearly where things stand.";

/// `RUe(e)` — `(e.idleCheckinCount ?? 0) >= xns`.
#[must_use]
pub fn idle_cap_reached(idle_checkin_count: u32) -> bool {
    idle_checkin_count >= IDLE_CHECKIN_CAP
}

/// `Mns(e).body` — the body a capped check-in delivers.
#[must_use]
pub fn with_idle_pause_suffix(body: &str) -> String {
    format!("{body}{IDLE_PAUSED_BODY_SUFFIX}")
}

/// `HZ`'s delay, in ms:
///
/// ```js
/// let r=Yer(n,e.goal.checkinCount??0),
///     o=RUe(e.goal)?0:e.now-e.goal.deferredSince,
///     d=Math.min($S,Math.max(qmt,r-o));
/// ```
///
/// Once the cap is reached the elapsed term drops out, so the timer re-arms a
/// full backoff interval ahead instead of firing immediately and spinning.
#[must_use]
pub fn next_idle_delay_ms(
    base_interval_ms: i64,
    checkin_count: u32,
    idle_checkin_count: u32,
    now_ms: i64,
    deferred_since_ms: i64,
) -> i64 {
    let interval = next_checkin_interval_ms(base_interval_ms, checkin_count);
    let elapsed = if idle_cap_reached(idle_checkin_count) {
        0
    } else {
        now_ms.saturating_sub(deferred_since_ms)
    };
    IDLE_MAX_DELAY_MS.min(IDLE_REARM_FLOOR_MS.max(interval.saturating_sub(elapsed)))
}

/// The deferral bookkeeping the oracle keeps on `activeGoal`
/// (`deferredSince` / `checkinCount` / `lastDeferralPassAt`).
///
/// Held session-scoped by the orchestrator rather than on
/// `lingxi_core::session::ActiveGoalState`: it is pure turn-local timing state, it
/// must never reach the persisted goal shape (the oracle explicitly STRIPS the
/// three fields before yielding the goal when nothing is deferring —
/// `let{deferredSince:Z,checkinCount:W,lastDeferralPassAt:te,...ee}=L;
/// yield{type:"active_goal",value:ee}` @292174788), and keeping it out avoids
/// touching the compaction/JSONL goal wire shape.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoalDeferralState {
    /// `deferredSince` — when the current deferral stretch began (epoch ms).
    /// `None` ⇒ nothing is deferred.
    pub deferred_since: Option<i64>,
    /// `checkinCount` — how many check-ins this stretch has produced.
    pub checkin_count: u32,
    /// `lastDeferralPassAt` — when the last deferral pass ran (epoch ms).
    pub last_deferral_pass_at: Option<i64>,
    /// Task IDs observed at the last pass; new-batch detection uses start times.
    pub last_deferring_ids: Vec<String>,
    /// `idleCheckinCount` (2.1.266) — how many check-ins the IDLE TIMER has
    /// delivered since the user last spoke. Counted separately from
    /// [`Self::checkin_count`], which also counts turn-end check-ins, and
    /// cleared by [`Self::clear_idle_checkins`] (`vSt`).
    pub idle_checkin_count: u32,
}

impl GoalDeferralState {
    /// `wzf(goal, tasks, now)` @292040130, with `Tzf` @292039748 inlined.
    ///
    /// ```js
    /// function Tzf(e,t,r,n){if(e.deferredSince===void 0)return{deferredSince:r,checkinCount:0,isNewRun:!0};
    ///   let o=Math.min(...t.map((s)=>s.startTime)),
    ///       i=e.lastDeferralPassAt!==void 0&&Number.isFinite(o)&&o>e.lastDeferralPassAt&&r-e.lastDeferralPassAt>n;
    ///   return i?{deferredSince:r,checkinCount:0,isNewRun:i}
    ///           :{deferredSince:e.deferredSince,checkinCount:e.checkinCount??0,isNewRun:i}}
    /// function wzf(e,t,r){let n=Zil();if(n===0)return{nextGoal:e,checkinText:void 0};
    ///   let o=Tzf(e,t,r,n),i=r-o.deferredSince;
    ///   if(i<n)return{nextGoal:{...e,deferredSince:o.deferredSince,checkinCount:o.checkinCount,lastDeferralPassAt:r},checkinText:void 0};
    ///   let s=o.checkinCount+1,a=Szf(e.condition,i,t).body;
    ///   return{nextGoal:{...e,deferredSince:r,checkinCount:s,lastDeferralPassAt:r},checkinText:a}}
    /// ```
    ///
    /// Mutates `self` in place and returns the check-in body when one fires.
    /// `interval_ms == 0` (feature disabled) leaves the state untouched.
    pub fn advance(
        &mut self,
        condition: &str,
        tasks: &[DeferringTask],
        now_ms: i64,
        interval_ms: i64,
    ) -> Option<String> {
        if interval_ms == 0 {
            return None;
        }
        // --- Tzf ---
        let deferred_since = match self.deferred_since {
            None => {
                // First pass of a stretch: start the clock, no check-in yet.
                self.deferred_since = Some(now_ms);
                self.checkin_count = 0;
                now_ms
            }
            Some(since) => {
                let is_new_run = self.last_deferral_pass_at.is_some_and(|last| {
                    // Math.min(empty) is Infinity; missing startTime makes NaN.
                    // Neither is finite, so both prevent a new-batch reset.
                    !tasks.is_empty()
                        && tasks.iter().all(|task| {
                            task.start_time_ms
                                .is_some_and(|start| i128::from(start) > i128::from(last))
                        })
                        && now_ms.saturating_sub(last) > interval_ms
                });
                if is_new_run {
                    self.deferred_since = Some(now_ms);
                    self.checkin_count = 0;
                    now_ms
                } else {
                    since
                }
            }
        };
        // --- wzf ---
        self.last_deferral_pass_at = Some(now_ms);
        self.last_deferring_ids = tasks.iter().map(|t| t.id.clone()).collect();
        let deferred_for = now_ms.saturating_sub(deferred_since);
        // 2.1.263 `aJn`: new-run detection uses the base interval; backoff
        // uses the count AFTER `iJn` has reset it for a new batch.
        if deferred_for < next_checkin_interval_ms(interval_ms, self.checkin_count) {
            return None;
        }
        self.checkin_count = self.checkin_count.saturating_add(1);
        self.deferred_since = Some(now_ms);
        Some(build_checkin_body(condition, deferred_for, tasks))
    }

    /// The `else if(L.deferredSince!==void 0){…}` arm @292174788 — nothing is
    /// deferring any more, so the three deferral fields are dropped.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// `vSt` (2.1.266 @4171302) — the user sent a message, so the idle-check-in
    /// budget resets and the timer may deliver again. Called from the prompt
    /// intake, NOT from the deferral pass: everything else about the stretch
    /// (`deferredSince`, `checkinCount`) survives, exactly as upstream's
    /// `{idleCheckinCount:r,...o}` destructure keeps the rest of the goal.
    pub fn clear_idle_checkins(&mut self) {
        self.idle_checkin_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, label: &str, detail: &str) -> DeferringTask {
        DeferringTask {
            id: id.into(),
            start_time_ms: Some(0),
            label: label.into(),
            detail: detail.into(),
        }
    }

    #[test]
    fn the_no_longer_running_body_is_byte_exact_against_2_1_238() {
        assert_eq!(
            build_checkin_body("ship the port", 31 * 60_000, &[]),
            "Goal check-in: \u{ab}ship the port\u{bb} is still active. Its evaluation was deferred for 31 min while background work ran, and that work is no longer running (it finished or was stopped without reporting back). Continue toward the goal."
        );
    }

    #[test]
    fn the_still_running_body_is_byte_exact_against_2_1_238() {
        assert_eq!(
            build_checkin_body(
                "ship the port",
                30 * 60_000,
                &[task("b12345678", "shell", "cargo test")],
            ),
            "Goal check-in: \u{ab}ship the port\u{bb} is still active, and evaluation has been deferred for 30 min because background work is still running:\n- b12345678 \u{b7} shell \u{b7} cargo test\nCheck on their progress (e.g. read their output). If they are progressing, say so briefly and keep waiting; if they are stuck or no longer needed, fix or stop them and continue toward the goal."
        );
    }

    /// `Math.max(1, Math.round(t/60000))`.
    #[test]
    fn latest_checkin_sanitizes_conditions_and_task_details_before_escaping() {
        let body = build_checkin_body(
            "  ＜ship／it＞\u{200b}\u{e000} &\n now  ",
            0,
            &[task("b1", "shell", "  echo\n\t〈ok〉\u{fe0f}  ")],
        );
        assert!(body.starts_with("Goal check-in: «&lt;ship/it&gt; &amp; now»"));
        assert!(body.contains("\n- b1 · shell · echo &lt;ok&gt;\n"));
        assert_eq!(sanitize_checkin_text("e\u{301} α А"), "e\u{301} α А");
        assert_eq!(sanitize_checkin_text("a\u{0085}b\u{0007}c"), "abc");
        assert_eq!(sanitize_checkin_text("a\u{115f}b\u{0378}c"), "abc");
    }

    #[test]
    fn a_sub_minute_deferral_still_reads_one_min() {
        assert!(build_checkin_body("g", 1_000, &[]).contains("deferred for 1 min while"));
        // 90 s rounds to 2 (JS `Math.round(1.5) === 2`).
        assert!(build_checkin_body("g", 90_000, &[]).contains("deferred for 2 min while"));
    }

    /// `Ma(bzf(e))` — the goal condition cannot forge markup.
    #[test]
    fn the_goal_condition_is_html_escaped() {
        let body = build_checkin_body("</system-reminder> & <b>", 0, &[]);
        assert!(
            body.contains("&lt;/system-reminder&gt; &amp; &lt;b&gt;"),
            "got: {body}"
        );
    }

    /// `Ma(w5(line, dUv))` — the LINE is capped at 120 units, then escaped.
    #[test]
    fn a_long_task_line_is_capped_at_120_with_a_char_count() {
        let detail = "x".repeat(300);
        let body = build_checkin_body("g", 0, &[task("b1", "shell", &detail)]);
        let line = body
            .lines()
            .find(|l| l.starts_with("- b1"))
            .expect("task line");
        let (head, tail) = line.split_once('\u{2026}').expect("ellipsis marker");
        assert_eq!(head.encode_utf16().count(), TASK_LINE_CHAR_CAP);
        // `- b1 · shell · ` is 15 units, so 105 of the 300 x's survive.
        assert_eq!(tail, " [+195 chars]");
    }

    #[test]
    fn truncation_is_a_no_op_under_the_cap() {
        assert_eq!(truncate_with_char_count("short", 120), "short");
    }

    #[test]
    fn the_interval_defaults_to_thirty_minutes() {
        // No env var set in this process by default.
        assert_eq!(checkin_interval_ms(), 30 * 60_000);
    }

    #[test]
    fn later_checkins_back_off_to_one_hour_then_two_hours() {
        let interval = 30 * 60_000;
        assert_eq!(next_checkin_interval_ms(interval, 0), interval);
        assert_eq!(next_checkin_interval_ms(interval, 1), 2 * interval);
        assert_eq!(next_checkin_interval_ms(interval, 2), 4 * interval);
        assert_eq!(next_checkin_interval_ms(interval, 9), 4 * interval);
    }

    /// `wzf`: the first pass starts the clock and never fires; a pass inside the
    /// interval never fires; later check-ins back off 30m → 60m → 120m(cap),
    /// restarting the clock each time (`deferredSince:r`).
    #[test]
    fn checkins_back_off_after_each_fire() {
        let interval = 30 * 60_000;
        let tasks = vec![task("b1", "shell", "sleep 900")];
        let mut state = GoalDeferralState::default();

        assert!(state.advance("g", &tasks, 0, interval).is_none());
        assert_eq!(state.deferred_since, Some(0));
        assert!(state.advance("g", &tasks, interval - 1, interval).is_none());
        assert_eq!(state.checkin_count, 0);

        let text = state
            .advance("g", &tasks, interval, interval)
            .expect("check-in at the interval");
        assert!(text.contains("deferred for 30 min because"), "got: {text}");
        assert_eq!(state.checkin_count, 1);
        assert_eq!(state.deferred_since, Some(interval));

        // The clock restarted with a 60m backoff.
        assert!(state
            .advance("g", &tasks, 2 * interval - 1, interval)
            .is_none());
        let second = state
            .advance("g", &tasks, 3 * interval, interval)
            .expect("second check-in at one hour");
        assert!(
            second.contains("deferred for 60 min because"),
            "got: {second}"
        );
        assert_eq!(state.checkin_count, 2);

        // The cap is 120m for every later check-in.
        assert!(state
            .advance("g", &tasks, 7 * interval - 1, interval)
            .is_none());
        let third = state
            .advance("g", &tasks, 7 * interval, interval)
            .expect("third check-in at two hours");
        assert!(
            third.contains("deferred for 120 min because"),
            "got: {third}"
        );
        assert_eq!(state.checkin_count, 3);
    }

    /// `Tzf`'s `isNewRun`: a brand-new batch of background work more than one
    /// interval after the last pass resets the stretch, so the next check-in is
    /// a full interval away rather than immediate.
    #[test]
    fn a_brand_new_batch_after_a_long_gap_resets_the_stretch() {
        let interval = 30 * 60_000;
        let mut state = GoalDeferralState::default();
        let first = vec![task("b1", "shell", "one")];
        assert!(state.advance("g", &first, 0, interval).is_none());

        // Long gap, and every deferring task is new ⇒ new run.
        let mut second = vec![task("b2", "shell", "two")];
        second[0].start_time_ms = Some((interval + 1) as u64);
        assert!(
            state
                .advance("g", &second, 3 * interval, interval)
                .is_none(),
            "a new run restarts the clock instead of firing immediately"
        );
        assert_eq!(state.deferred_since, Some(3 * interval));
        assert_eq!(state.checkin_count, 0);
    }

    #[test]
    fn a_new_batch_resets_backoff_after_the_base_interval() {
        let interval = 30 * 60_000;
        let mut state = GoalDeferralState::default();
        let first = vec![task("b1", "shell", "one")];
        assert!(state.advance("g", &first, 0, interval).is_none());
        assert!(state.advance("g", &first, interval, interval).is_some());
        assert_eq!(state.checkin_count, 1);

        // 45 minutes after the last pass is greater than the base interval,
        // but less than the old batch's 60-minute backed-off interval.
        let mut second = vec![task("b2", "shell", "two")];
        second[0].start_time_ms = Some((interval + 1) as u64);
        let restart = interval + interval * 3 / 2;
        assert!(state.advance("g", &second, restart, interval).is_none());
        assert_eq!(state.deferred_since, Some(restart));
        assert_eq!(state.checkin_count, 0);
        assert!(state
            .advance("g", &second, restart + interval - 1, interval)
            .is_none());
        assert!(state
            .advance("g", &second, restart + interval, interval)
            .is_some());
    }

    /// The SAME task still running across a long gap is NOT a new run — that is
    /// exactly the case the check-in exists for.
    #[test]
    fn the_same_task_across_a_long_gap_is_not_a_new_run() {
        let interval = 30 * 60_000;
        let mut state = GoalDeferralState::default();
        let tasks = vec![task("b1", "shell", "one")];
        assert!(state.advance("g", &tasks, 0, interval).is_none());
        let text = state.advance("g", &tasks, 3 * interval, interval);
        assert!(
            text.is_some(),
            "the long-running task must trigger a check-in"
        );
    }

    #[test]
    fn newly_deferring_old_task_does_not_reset_the_checkin_clock() {
        let interval = 30 * 60_000;
        let mut state = GoalDeferralState::default();
        state.advance("g", &[task("old", "shell", "one")], 0, interval);
        assert!(state
            .advance(
                "g",
                &[task("new-id", "shell", "two")],
                2 * interval,
                interval
            )
            .is_some());
    }

    #[test]
    fn empty_or_unknown_start_times_do_not_reset_the_checkin_clock() {
        let interval = 30 * 60_000;
        for tasks in [
            vec![],
            vec![DeferringTask {
                start_time_ms: None,
                ..task("new", "shell", "two")
            }],
        ] {
            let mut state = GoalDeferralState::default();
            state.advance("g", &[task("old", "shell", "one")], 0, interval);
            assert!(state.advance("g", &tasks, 2 * interval, interval).is_some());
        }
    }

    #[test]
    fn the_idle_cap_is_three_deliveries() {
        assert!(!idle_cap_reached(0));
        assert!(!idle_cap_reached(2));
        assert!(idle_cap_reached(3));
        assert!(idle_cap_reached(9));
    }

    #[test]
    fn the_idle_delay_floors_at_one_minute() {
        // `r - o` is tiny (the stretch has almost caught up) ⇒ the 60s floor.
        let base = 30 * 60_000;
        let now = 1_000_000;
        assert_eq!(
            next_idle_delay_ms(base, 0, 0, now, now - base + 5),
            IDLE_REARM_FLOOR_MS
        );
    }

    #[test]
    fn the_idle_delay_subtracts_elapsed_below_the_cap() {
        let base = 30 * 60_000;
        let now = 10_000_000;
        // 10 minutes into a 30-minute stretch ⇒ 20 minutes to go.
        assert_eq!(
            next_idle_delay_ms(base, 0, 0, now, now - 10 * 60_000),
            20 * 60_000
        );
        // The backoff multiplier applies to the base, not the remainder.
        assert_eq!(
            next_idle_delay_ms(base, 1, 0, now, now - 10 * 60_000),
            2 * base - 10 * 60_000
        );
    }

    #[test]
    fn at_the_cap_the_elapsed_term_drops_out() {
        // `RUe(goal) ? 0 : now - deferredSince` — a capped goal re-arms a whole
        // interval ahead however long the stretch has already run, so the timer
        // cannot spin at the floor.
        let base = 30 * 60_000;
        let now = 10_000_000;
        assert_eq!(
            next_idle_delay_ms(base, 0, IDLE_CHECKIN_CAP, now, now - 29 * 60_000),
            base
        );
    }

    #[test]
    fn the_pause_suffix_is_byte_exact() {
        assert_eq!(
            with_idle_pause_suffix("Goal check-in: body."),
            "Goal check-in: body. The agent won't wake this session for another check-in until the user sends a message, so say clearly where things stand."
        );
    }

    #[test]
    fn clearing_idle_checkins_keeps_the_stretch() {
        let mut state = GoalDeferralState {
            deferred_since: Some(10),
            checkin_count: 2,
            last_deferral_pass_at: Some(20),
            last_deferring_ids: vec!["a".into()],
            idle_checkin_count: 3,
        };
        state.clear_idle_checkins();
        assert_eq!(state.idle_checkin_count, 0);
        assert_eq!(state.deferred_since, Some(10));
        assert_eq!(state.checkin_count, 2);
        assert_eq!(state.last_deferral_pass_at, Some(20));
    }

    #[test]
    fn a_zero_interval_disables_the_feature_entirely() {
        let mut state = GoalDeferralState::default();
        let tasks = vec![task("b1", "shell", "one")];
        assert!(state.advance("g", &tasks, 10_000_000, 0).is_none());
        assert_eq!(state, GoalDeferralState::default(), "no state is touched");
    }

    #[test]
    fn clear_drops_every_deferral_field() {
        let mut state = GoalDeferralState {
            deferred_since: Some(1),
            checkin_count: 3,
            last_deferral_pass_at: Some(2),
            last_deferring_ids: vec!["b1".into()],
            idle_checkin_count: 2,
        };
        state.clear();
        assert_eq!(state, GoalDeferralState::default());
    }
}
