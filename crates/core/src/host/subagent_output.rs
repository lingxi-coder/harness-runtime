//! Shared harness notes for foreground and background agent reports.

/// claude `pRe` (src_160528463.js @870) — the prefix of the harness NOTE that
/// fronts a turn-limited agent's result.
pub const MAX_TURNS_NOTE_PREFIX: &str = "NOTE: this agent stopped at its ";

/// Build the harness NOTE `bft` prepends when the run ended on its turn budget
/// (src_162329786.js @3532630):
///
/// ```js
/// let en=PKt.has(C)?"":` Send the agent a message (${Yr}) to let it continue from where it stopped.`,
///     Dt=ye.length>0?"The text below is PARTIAL output; treat it as incomplete."
///                   :"It was still calling tools and had produced no report.";
/// Le.push({type:"text",text:`${pRe}${Fe}-turn limit before finishing. ${Dt}${en}\n`})
/// ```
///
/// `Fe` is the exhausted budget (`N2n`, the `max_turns_reached` attachment),
/// `ye` the agent's own final text blocks BEFORE the output guard runs, and
/// `PKt` the one-shot built-ins — `Explore` / `Plan` cannot be continued, so
/// they get no "send it a message" tail. The note is a harness block, not agent
/// output, so it is NOT passed through the subagent output guard.
pub fn max_turns_harness_note(
    max_turns: u64,
    agent_type: &str,
    has_partial_output: bool,
) -> String {
    max_turns_harness_note_with_continuation(max_turns, agent_type, has_partial_output, true)
}

/// Native 2.1.319 also suppresses continuation guidance when the caller has
/// no messaging tool. The background caller without that fact keeps the
/// existing default through [`max_turns_harness_note`].
pub fn max_turns_harness_note_with_continuation(
    max_turns: u64,
    agent_type: &str,
    has_partial_output: bool,
    can_continue_agent: bool,
) -> String {
    let continuation = if !can_continue_agent || matches!(agent_type, "Explore" | "Plan") {
        ""
    } else {
        " Send the agent a message (SendMessage) to let it continue from where it stopped."
    };
    let body = if has_partial_output {
        "The text below is PARTIAL output; treat it as incomplete."
    } else {
        "It was still calling tools and had produced no report."
    };
    format!(
        "{MAX_TURNS_NOTE_PREFIX}{max_turns}-turn limit before finishing. {body}{continuation}\n"
    )
}
