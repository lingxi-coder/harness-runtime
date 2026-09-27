//! Forked-skill execution — the `context: fork` path.
//!
//! A skill declaring `context: fork` does not expand inline into the caller's
//! conversation. It runs as a SUBAGENT under the skill's own permission
//! scoping, normally in the background, and the tool returns immediately with a
//! handle instead of the skill body.
//!
//! Three decisions live here, in the order the binary makes them:
//!
//! 1. [`should_background_fork`] (claude `KCo`) — background, or fork
//!    synchronously? Only the background path is resumable, so only it freezes
//!    command denies and persists scoping.
//! 2. [`decide_fork_launch`] (the guard half of claude `YCo`) — the caps and
//!    the live-duplicate check, each of which falls back to INLINE rather than
//!    failing the call, except the one that raises a hard error.
//! 3. [`fork_result`] / [`fork_tool_result_text`] — the result union and the
//!    model-facing `tool_result` text, which differ between the background and
//!    synchronous fork.
//!
//! Every fall-back-to-inline branch is silent by design (claude returns `null`
//! from `YCo` and drops into the synchronous path): a skill that cannot fork
//! still runs, just in this context.

use platform_api::task_registry::TaskRecord;
use session::forked_skill::ForkedSkillScoping;

/// Whether a forking skill should run in the BACKGROUND (claude `KCo`).
///
/// `if (t || DT() || _n()) return false; return e.background ?? true` — the
/// default is background, and three things force the synchronous path:
/// - the caller's own opt-out (`t`, threaded here as `forced_sync`),
/// - background tasks being disabled for the session,
/// - already running as a subagent — a background fork from inside a background
///   agent would detach work from the agent that is about to be judged complete.
#[must_use]
pub fn should_background_fork(
    declared: Option<bool>,
    forced_sync: bool,
    background_tasks_disabled: bool,
    is_subagent: bool,
) -> bool {
    if forced_sync || background_tasks_disabled || is_subagent {
        return false;
    }
    declared.unwrap_or(true)
}

/// Why a fork fell back to running inline. Each maps to one of claude's
/// `Ne("subagent_launch", …)` counters, which is the only place these surface —
/// the model just sees the skill run inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineFallback {
    /// This skill is ALREADY running as a live (non-terminal) fork. One live
    /// fork per skill: a second would race the first over the same scoping
    /// sidecar and the same `SendMessage` name.
    LiveDuplicate,
    /// Past the subagent nesting cap, with spawn budget still available.
    DepthCap,
    /// The session has spawned its maximum number of subagents.
    SpawnCap,
    /// The scoping record would not satisfy the on-disk schema, so it could
    /// never be read back — forking would produce an unresumable agent.
    ScopingUnpersistable,
    /// Writing the scoping sidecars failed.
    ScopingWriteFailed,
}

impl InlineFallback {
    /// The telemetry reason string (claude's `Ne("subagent_launch", <this>)`).
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::LiveDuplicate => "forked_skill_live_duplicate",
            Self::DepthCap => "forked_skill_depth_cap",
            Self::SpawnCap => "forked_skill_spawn_cap",
            Self::ScopingUnpersistable => "forked_skill_scoping_unpersistable",
            Self::ScopingWriteFailed => "forked_skill_scoping_write_failed",
        }
    }
}

/// What [`decide_fork_launch`] concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkDecision {
    /// Fork. Carries the scoping record to persist before launching.
    Launch(Box<ForkedSkillScoping>),
    /// Run inline instead, for this reason.
    Inline(InlineFallback),
    /// HARD failure — the one branch that does not degrade to inline. Past the
    /// nesting cap AND out of spawn budget, claude throws rather than silently
    /// running the skill here, because the caller is deep in a chain that has
    /// already exhausted the session's agents.
    DepthChainCap {
        /// Agents already spawned this session.
        spawned: u64,
        /// The session cap.
        cap: u64,
    },
}

/// The hard-error message for [`ForkDecision::DepthChainCap`] (claude's
/// `Or(…, "forked_skill_depth_chain_cap")`).
#[must_use]
pub fn depth_chain_cap_message(spawned: u64, cap: u64) -> String {
    format!(
        "Subagent spawn limit reached ({spawned} of {cap}) past the nesting depth cap. \
Do the skill's work directly in this context instead of invoking further skills."
    )
}

/// Whether `tasks` already contains a LIVE fork of `skill_name` (claude's
/// `L.type==="local_agent" && !CT(L.status) && L.forkedSkillName===r.name`).
///
/// "Live" is not-terminal, which includes `pending` — a fork that has been
/// admitted but not yet started still owns the skill's slot.
#[must_use]
pub fn has_live_fork(tasks: &[TaskRecord], skill_name: &str) -> bool {
    tasks.iter().any(|t| {
        t.task_type == "local_agent"
            && !matches!(t.status.as_str(), "completed" | "failed" | "killed")
            && t.forked_skill_name.as_deref() == Some(skill_name)
    })
}

/// Inputs to the launch decision, gathered by the caller.
#[derive(Debug, Clone)]
pub struct ForkLaunchInputs<'a> {
    /// The skill's name — the fork identity.
    pub skill_name: &'a str,
    /// The display name the fork is attributed to (claude `spawnedBySkill`).
    pub attribution_name: &'a str,
    /// The skill's declared effort, when it declared one.
    pub effort: Option<session::forked_skill::Effort>,
    /// Command-deny rules frozen at fork time. An EMPTY list writes no key —
    /// claude gates the spread on `f !== undefined && f.length > 0`.
    pub frozen_command_denies: Vec<String>,
    /// This spawn's nesting depth (parent depth + 1).
    pub depth: usize,
    /// The nesting cap.
    pub depth_limit: usize,
    /// Agents already spawned this session.
    pub total_spawns: u64,
    /// The per-session spawn cap.
    pub spawn_cap: u64,
    /// Live task records, for the duplicate check.
    pub tasks: &'a [TaskRecord],
}

/// The guard half of claude `YCo`, in the binary's order.
///
/// Ordering is behavioural, not stylistic: the duplicate check runs FIRST, so a
/// second invocation of an already-forked skill falls back to inline without
/// consuming spawn budget. The depth check runs before the spawn cap, so being
/// too deep reports `depth_cap` rather than masquerading as budget exhaustion.
///
/// The caller must re-check [`has_live_fork`] and the spawn cap AFTER the
/// scoping write and before actually spawning — the write is an `await`, and
/// claude re-runs both checks across it. [`recheck_after_persist`] is that
/// second pass.
#[must_use]
pub fn decide_fork_launch(inputs: &ForkLaunchInputs<'_>) -> ForkDecision {
    if has_live_fork(inputs.tasks, inputs.skill_name) {
        return ForkDecision::Inline(InlineFallback::LiveDuplicate);
    }
    if inputs.depth > inputs.depth_limit {
        // Past the nesting cap. If the session ALSO has no spawn budget left,
        // this is a hard error rather than a quiet inline run: the caller is in
        // a chain that has already consumed the session's agents, and silently
        // running here would hide that from the model.
        if inputs.total_spawns >= inputs.spawn_cap {
            return ForkDecision::DepthChainCap {
                spawned: inputs.total_spawns,
                cap: inputs.spawn_cap,
            };
        }
        return ForkDecision::Inline(InlineFallback::DepthCap);
    }
    if inputs.total_spawns >= inputs.spawn_cap {
        return ForkDecision::Inline(InlineFallback::SpawnCap);
    }

    let scoping = ForkedSkillScoping {
        skill_name: inputs.skill_name.to_string(),
        attribution_name: inputs.attribution_name.to_string(),
        effort: inputs.effort.clone(),
        frozen_command_denies: (!inputs.frozen_command_denies.is_empty())
            .then(|| inputs.frozen_command_denies.clone()),
    };
    // Validate BEFORE writing: a record that fails the schema would be read
    // back as `Malformed`, and `Malformed` is a resume REFUSAL. Forking on an
    // unpersistable record would produce an agent that can never be resumed.
    if !scoping.is_valid() {
        return ForkDecision::Inline(InlineFallback::ScopingUnpersistable);
    }
    ForkDecision::Launch(Box::new(scoping))
}

/// The post-write re-check (claude re-runs the duplicate and spawn-cap tests
/// after `await qdd(...)`). Returns the fallback that now applies, if any.
///
/// The window is real: the scoping write is I/O, and a concurrent tool call can
/// launch the same skill or exhaust the spawn budget while it is in flight.
#[must_use]
pub fn recheck_after_persist(
    tasks: &[TaskRecord],
    skill_name: &str,
    total_spawns: u64,
    spawn_cap: u64,
) -> Option<InlineFallback> {
    if has_live_fork(tasks, skill_name) {
        return Some(InlineFallback::LiveDuplicate);
    }
    if total_spawns >= spawn_cap {
        return Some(InlineFallback::SpawnCap);
    }
    None
}

/// The `data` payload for a forked skill (claude's two `status:"forked"`
/// returns).
///
/// Background: `{success, commandName, status:"forked", background:true,
/// agentId, result:"Running in the background as @<name>"}`.
/// Synchronous: the same without `background`, and `result` is the agent's
/// output.
#[must_use]
pub fn fork_result(
    command_name: &str,
    agent_id: &str,
    background: bool,
    result: &str,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("success".into(), serde_json::Value::Bool(true));
    obj.insert(
        "commandName".into(),
        serde_json::Value::String(command_name.to_string()),
    );
    obj.insert(
        "status".into(),
        serde_json::Value::String("forked".to_string()),
    );
    if background {
        obj.insert("background".into(), serde_json::Value::Bool(true));
    }
    obj.insert(
        "agentId".into(),
        serde_json::Value::String(agent_id.to_string()),
    );
    obj.insert(
        "result".into(),
        serde_json::Value::String(result.to_string()),
    );
    serde_json::Value::Object(obj)
}

/// The subagent's final TEXT (claude `Jc(content, "\n")` — text blocks joined
/// with `\n`; non-text blocks dropped). A non-array content has no text blocks.
#[must_use]
pub fn final_text(content: &serde_json::Value) -> String {
    let Some(blocks) = content.as_array() else {
        return String::new();
    };
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(serde_json::Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The background launch's `result` line (claude
/// `` `Running in the background as @${G.name}` ``).
#[must_use]
pub fn running_in_background_line(agent_name: &str) -> String {
    format!("Running in the background as @{agent_name}")
}

/// The model-facing `tool_result` text for a forked skill (claude
/// `mapToolResultToToolResultBlockParam`'s `status === "forked"` branch).
///
/// The model never sees the raw result object — the inline path's equivalent is
/// the one-line `Launching skill: …`, and these are the forked path's two.
#[must_use]
pub fn fork_tool_result_text(command_name: &str, background: bool, result: &str) -> String {
    if background {
        format!(
            "Skill \"{command_name}\" launched (forked execution, running in the background).\n\n{result}"
        )
    } else {
        format!("Skill \"{command_name}\" completed (forked execution).\n\nResult:\n{result}")
    }
}

/// The synchronous fork's `result` when the subagent produced no final text
/// (claude `bpr(U, "Skill execution completed")`'s default).
pub const SYNC_FORK_EMPTY_RESULT: &str = "Skill execution completed";

/// Whether a skill's frontmatter asks for forked execution (`context: fork`).
#[must_use]
pub fn declares_fork(context: Option<&str>) -> bool {
    context == Some("fork")
}

/// Whether `name` can be used as a fork identity — the same bound the on-disk
/// schema enforces, checked before the launch so an over-long or newline-
/// bearing skill name never reaches the sidecar.
#[must_use]
pub fn is_forkable_skill_name(name: &str) -> bool {
    session::forked_skill::is_valid_skill_name(name)
}

#[cfg(test)]
mod tests {
    use session::forked_skill::SKILL_NAME_MAX_LEN;

    use super::*;

    fn task(status: &str, forked: Option<&str>) -> TaskRecord {
        TaskRecord {
            task_id: "a00000001".into(),
            task_type: "local_agent".into(),
            status: status.into(),
            forked_skill_name: forked.map(str::to_string),
            ..Default::default()
        }
    }

    fn inputs<'a>(skill: &'a str, tasks: &'a [TaskRecord]) -> ForkLaunchInputs<'a> {
        ForkLaunchInputs {
            skill_name: skill,
            attribution_name: skill,
            effort: None,
            frozen_command_denies: Vec::new(),
            depth: 1,
            depth_limit: 1,
            total_spawns: 0,
            spawn_cap: 200,
            tasks,
        }
    }

    #[test]
    fn background_is_the_default_and_each_veto_forces_sync() {
        assert!(should_background_fork(None, false, false, false));
        assert!(should_background_fork(Some(true), false, false, false));
        assert!(!should_background_fork(Some(false), false, false, false));
        assert!(!should_background_fork(None, true, false, false));
        assert!(!should_background_fork(None, false, true, false));
        // A background fork from inside a subagent would detach work from the
        // agent about to be judged complete.
        assert!(!should_background_fork(None, false, false, true));
    }

    #[test]
    fn a_live_fork_of_the_same_skill_forces_inline() {
        let tasks = vec![task("running", Some("review"))];
        assert_eq!(
            decide_fork_launch(&inputs("review", &tasks)),
            ForkDecision::Inline(InlineFallback::LiveDuplicate)
        );
        // A DIFFERENT skill is unaffected.
        assert!(matches!(
            decide_fork_launch(&inputs("other", &tasks)),
            ForkDecision::Launch(_)
        ));
    }

    /// "Live" is not-terminal, so a `pending` fork already owns the slot, while
    /// every terminal status releases it.
    #[test]
    fn liveness_is_non_terminal_not_merely_running() {
        assert!(has_live_fork(&[task("pending", Some("s"))], "s"));
        assert!(has_live_fork(&[task("running", Some("s"))], "s"));
        for done in ["completed", "failed", "killed"] {
            assert!(!has_live_fork(&[task(done, Some("s"))], "s"), "{done}");
        }
    }

    /// A non-agent task carrying the same name must not block the fork — the
    /// guard is scoped to `local_agent`.
    #[test]
    fn only_local_agent_tasks_count_as_a_live_fork() {
        let mut t = task("running", Some("s"));
        t.task_type = "local_workflow".into();
        assert!(!has_live_fork(&[t], "s"));
    }

    #[test]
    fn past_the_depth_cap_falls_back_to_inline_while_budget_remains() {
        let tasks = vec![];
        let mut i = inputs("s", &tasks);
        i.depth = 2;
        i.depth_limit = 1;
        assert_eq!(
            decide_fork_launch(&i),
            ForkDecision::Inline(InlineFallback::DepthCap)
        );
    }

    /// Past the depth cap AND out of spawn budget is the one HARD failure —
    /// running inline there would hide an exhausted chain from the model.
    #[test]
    fn past_the_depth_cap_with_no_budget_is_a_hard_error() {
        let tasks = vec![];
        let mut i = inputs("s", &tasks);
        i.depth = 2;
        i.depth_limit = 1;
        i.total_spawns = 200;
        i.spawn_cap = 200;
        assert_eq!(
            decide_fork_launch(&i),
            ForkDecision::DepthChainCap {
                spawned: 200,
                cap: 200
            }
        );
        assert_eq!(
            depth_chain_cap_message(200, 200),
            "Subagent spawn limit reached (200 of 200) past the nesting depth cap. \
Do the skill's work directly in this context instead of invoking further skills."
        );
    }

    #[test]
    fn the_spawn_cap_falls_back_to_inline() {
        let tasks = vec![];
        let mut i = inputs("s", &tasks);
        i.total_spawns = 200;
        i.spawn_cap = 200;
        assert_eq!(
            decide_fork_launch(&i),
            ForkDecision::Inline(InlineFallback::SpawnCap)
        );
    }

    /// The duplicate check precedes the caps, so re-invoking a live fork does
    /// not consume budget on its way to running inline.
    #[test]
    fn the_duplicate_check_precedes_the_caps() {
        let tasks = vec![task("running", Some("s"))];
        let mut i = inputs("s", &tasks);
        i.total_spawns = 200;
        i.spawn_cap = 200;
        assert_eq!(
            decide_fork_launch(&i),
            ForkDecision::Inline(InlineFallback::LiveDuplicate)
        );
    }

    /// A record that would fail the on-disk schema must not be written: it
    /// would read back as `Malformed`, which is a resume REFUSAL, leaving an
    /// agent that can never be resumed.
    #[test]
    fn an_unpersistable_scoping_record_forces_inline() {
        let tasks = vec![];
        let mut i = inputs("s", &tasks);
        i.frozen_command_denies = vec!["x".repeat(2000)];
        assert_eq!(
            decide_fork_launch(&i),
            ForkDecision::Inline(InlineFallback::ScopingUnpersistable)
        );
    }

    /// An EMPTY deny list writes no key at all (claude's spread is gated on
    /// `f.length > 0`), so it must not become `"frozenCommandDenies": []`.
    #[test]
    fn an_empty_deny_list_writes_no_key() {
        let tasks = vec![];
        let ForkDecision::Launch(scoping) = decide_fork_launch(&inputs("s", &tasks)) else {
            panic!("expected Launch");
        };
        assert!(scoping.frozen_command_denies.is_none());

        let mut i = inputs("s", &tasks);
        i.frozen_command_denies = vec!["Bash(rm:*)".into()];
        let ForkDecision::Launch(scoping) = decide_fork_launch(&i) else {
            panic!("expected Launch");
        };
        assert_eq!(
            scoping.frozen_command_denies.as_deref(),
            Some(["Bash(rm:*)".to_string()].as_slice())
        );
    }

    #[test]
    fn the_recheck_catches_a_race_over_the_persist_window() {
        let tasks = vec![task("running", Some("s"))];
        assert_eq!(
            recheck_after_persist(&tasks, "s", 0, 200),
            Some(InlineFallback::LiveDuplicate)
        );
        assert_eq!(
            recheck_after_persist(&[], "s", 200, 200),
            Some(InlineFallback::SpawnCap)
        );
        assert_eq!(recheck_after_persist(&[], "s", 0, 200), None);
    }

    #[test]
    fn background_result_carries_the_background_flag_and_handle_line() {
        let v = fork_result(
            "review",
            "agent-7",
            true,
            &running_in_background_line("rev"),
        );
        assert_eq!(v["status"], "forked");
        assert_eq!(v["background"], true);
        assert_eq!(v["agentId"], "agent-7");
        assert_eq!(v["result"], "Running in the background as @rev");
    }

    /// The synchronous fork omits `background` entirely — claude only spreads
    /// the key on the background return.
    #[test]
    fn synchronous_fork_result_omits_the_background_key() {
        let v = fork_result("review", "agent-7", false, "the findings");
        assert_eq!(v["status"], "forked");
        assert!(v.get("background").is_none());
        assert_eq!(v["result"], "the findings");
    }

    #[test]
    fn tool_result_text_differs_between_the_two_fork_paths() {
        assert_eq!(
            fork_tool_result_text("review", true, "Running in the background as @rev"),
            "Skill \"review\" launched (forked execution, running in the background).\n\n\
             Running in the background as @rev"
        );
        assert_eq!(
            fork_tool_result_text("review", false, "the findings"),
            "Skill \"review\" completed (forked execution).\n\nResult:\nthe findings"
        );
    }

    #[test]
    fn the_sync_fork_default_result_is_locked() {
        assert_eq!(SYNC_FORK_EMPTY_RESULT, "Skill execution completed");
    }

    #[test]
    fn only_context_fork_declares_a_fork() {
        assert!(declares_fork(Some("fork")));
        assert!(!declares_fork(Some("inline")));
        assert!(!declares_fork(None));
        // No case folding — claude compares the literal string.
        assert!(!declares_fork(Some("Fork")));
    }

    #[test]
    fn fork_identity_rejects_names_the_sidecar_could_not_hold() {
        assert!(is_forkable_skill_name("review"));
        assert!(!is_forkable_skill_name(&"x".repeat(SKILL_NAME_MAX_LEN + 1)));
        assert!(!is_forkable_skill_name("a\nb"));
    }
}
