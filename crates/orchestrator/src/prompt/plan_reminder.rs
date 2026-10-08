//! Byte-locked "plan-mode reminder" assembler — the BODY only.
//!
//! Port of the Claude Code 2.1.206 per-turn plan-mode system-reminder text.
//! The 206 dispatch is:
//! ```js
//! if (e.isSubAgent) return NU_(e);
//! if (e.reminderType === "sparse") return MU_(e);
//! return LU_(e);
//! ```
//! and this module ports the three renderers plus their helpers, byte-for-byte
//! against the 2.1.206 binary
//! (`~/.local/share/claude/versions/2.1.206`):
//!
//! * `NU_`  — subagent reminder            → [`render_subagent`]
//! * `MU_`  — sparse per-turn reminder      → [`render_sparse`]
//! * `LU_`  — full (default) reminder       → [`render_full`]
//! * `OU_`  — "### Phase 4: Final Plan" block (const [`OU_PHASE4`])
//! * `lIp()`— "### Call ExitPlanMode" tail   (const [`LIP_EXIT_TAIL`])
//! * `aIp`  — the full-variant plan-mode banner (const [`AIP_BANNER`])
//! * `Ykp` / `Kkp` / `QVt` — the count / availability helpers.
//!
//! 206 interpolation map → resolved port literals (all inlined below):
//! `gj.name`=`ExitPlanMode`, `Lm`=`AskUserQuestion`, `YI.name`=`Edit`,
//! `pk.name`=`Write`, `iPe.agentType`=`Explore`, `eTo.agentType`=`Plan`.
//! Em-dashes (`\u{2014}`) in the 206 source appear as `—` unicode escapes
//! inside template literals; they render as U+2014 and are emitted as such here.
//!
//! These renderers return the BARE body. The oracle's three plan-mode renderers
//! all return through the batch wrapper `Zy` (2.1.238 @296675470), which maps
//! `NT` (@296673554) = `` `<system-reminder>\n${e}\n</system-reminder>` `` over
//! every message and marks it `isMeta:!0`; that envelope is applied by the
//! single caller, `ConversationOrchestrator::plan_mode_turn_messages`, so the
//! byte-exact body assertions below stay readable.
//!
//! `render_full` keeps the 206 single-letter locals `r`/`n`/`o` (plan count /
//! explore count / subagents-available) for a 1:1 read against `LU_`.
#![allow(clippy::doc_markdown, clippy::many_single_char_names)]

/// Inputs to the plan-mode reminder renderers.
///
/// Mirrors the 206 `e` object read by `NU_`/`MU_`/`LU_`.
#[derive(Debug, Clone, Copy)]
pub struct PlanReminderParams<'a> {
    /// `e.planFilePath` — absolute path of the plan file.
    pub plan_file_path: &'a str,
    /// `e.planExists` — whether the plan file is already on disk.
    pub plan_exists: bool,
    /// `e.customInstructions` — `--plan-mode-instructions`, when set.
    pub custom_instructions: Option<&'a str>,
    /// `e.isSubAgent` — reminder is being rendered for a subagent turn.
    pub is_subagent: bool,
    /// `e.reminderType === "sparse"` — the terse per-turn variant.
    pub reminder_type_sparse: bool,
    /// `zx() === "default"` — whether the session is on the default output
    /// style. 2.1.266 `fTs` gates the Explore/Plan-subagent Phase 1/2 bodies on
    /// `d8() && zx()==="default"`, so a custom output style falls back to the
    /// no-subagent phases.
    pub output_style_is_default: bool,
}

// ─── shared fragments ────────────────────────────────────────────────────────

/// 206 `aIp` — the full-variant plan-mode banner (note the source misspelling
/// "supercedes", preserved verbatim).
const AIP_BANNER: &str = "Plan mode is active. The user indicated that they do not want you to execute yet -- you MUST NOT make any edits (with the exception of the plan file mentioned below), run any non-readonly tools (including changing configs or making commits), or otherwise make any changes to the system. This supercedes any other instructions you have received.";

/// Line shared by `NU_` and both `LU_` bodies, immediately after the Plan File
/// Info line.
const INCREMENTAL_LINE: &str = "You should build your plan incrementally by writing to or editing this file. NOTE that this is the only file you are allowed to edit - other than this you are only allowed to take READ-ONLY actions.";

/// 206 `OU_` — "### Phase 4: Final Plan" block.
const OU_PHASE4: &str = "### Phase 4: Final Plan\nGoal: Write your final plan to the plan file (the only file you can edit).\n- Begin with a **Context** section: explain why this change is being made \u{2014} the problem or need it addresses, what prompted it, and the intended outcome\n- Include only your recommended approach, not all alternatives\n- Ensure that the plan file is concise enough to scan quickly, but detailed enough to execute effectively\n- Name the critical files to be modified. For changes that repeat a pattern across many files, describe the pattern once and list a few representative paths \u{2014} do not enumerate every file or line number\n- Reference existing functions and utilities you found that should be reused, with their file paths\n- Include a verification section describing how to test the changes end-to-end (run the code, use MCP tools, run tests)";

/// 206 `lIp()` — the "### Call ExitPlanMode" tail body.
const LIP_EXIT_TAIL: &str = "At the very end of your turn, once you have asked the user questions and are happy with your final plan file - you should always call ExitPlanMode to indicate to the user that you are done planning.\nThis is critical - your turn should only end with either using the AskUserQuestion tool OR calling ExitPlanMode. Do not stop unless it's for these 2 reasons\n\n**Important:** Use AskUserQuestion ONLY to clarify requirements or choose between approaches. Use ExitPlanMode to request plan approval. Do NOT ask about plan approval in any other way - no text questions, no AskUserQuestion. Phrases like \"Is this plan okay?\", \"Should I proceed?\", \"How does this plan look?\", \"Any changes before we start?\", or similar MUST use ExitPlanMode.";

/// 206 `LU_` "### Phase 3: Review" block (fixed).
const PHASE3_REVIEW: &str = "### Phase 3: Review\nGoal: Review the plan(s) from Phase 2 and ensure alignment with the user's intentions.\n1. Read the critical files you identified during exploration to deepen your understanding\n2. Ensure that the plans align with the user's original request\n3. Use AskUserQuestion to clarify any remaining questions with the user";

/// 206 `LU_` trailing NOTE paragraph (fixed).
const FULL_NOTE_TAIL: &str = "NOTE: At any point in time through this workflow you should feel free to ask the user questions or clarifications using the AskUserQuestion tool. Don't make large assumptions about user intent. The goal is to present a well researched plan to the user, and tie any loose ends before implementation begins.";

/// 206 `LU_` Phase 1 (no-subagent variant), fixed.
const PHASE1_NO_SUBAGENT: &str = "### Phase 1: Initial Understanding\nGoal: Gain a comprehensive understanding of the user's request by reading through code and asking them questions.\n\n1. Focus on understanding the user's request and the code associated with their request. Actively search for existing functions, utilities, and patterns that can be reused \u{2014} avoid proposing new code when suitable implementations already exist.\n\n2. Read and explore the relevant files directly to efficiently understand the codebase.";

/// 206 `LU_` Phase 2 (no-subagent variant), fixed.
const PHASE2_NO_SUBAGENT: &str = "### Phase 2: Design\nGoal: Design an implementation approach based on the user's intent and your exploration results from Phase 1.\n\n- Provide comprehensive background context from Phase 1 exploration including filenames and code path traces\n- Describe requirements and constraints\n- Produce a detailed implementation plan";

// ─── count / availability helpers ────────────────────────────────────────────

/// 206 `Ykp()` — Explore-agent count `n`. Env override
/// `branding::PLAN_EXPLORE_AGENT_COUNT_ENV` selects a count from 1 through 10,
/// with a default of 3.
fn explore_agent_count() -> u32 {
    if let Ok(v) = std::env::var(branding::PLAN_EXPLORE_AGENT_COUNT_ENV) {
        // 206 uses parseInt(v, 10); a bare `parse` is close enough for the
        // env-override path (which is the byte-faithful part). Parsing as u32
        // means non-positive values fall through to the default, matching the
        // 206 `e>0` guard.
        if let Ok(n) = v.trim().parse::<u32>() {
            if n > 0 && n <= 10 {
                return n;
            }
        }
    }
    3
}

/// 206 `Kkp()` — Plan-agent count `r`. Env override
/// `branding::PLAN_AGENT_COUNT_ENV` selects a count from 1 through 10,
/// with a default of 1 independent of provider or subscription tier.
fn plan_agent_count() -> u32 {
    if let Ok(v) = std::env::var(branding::PLAN_AGENT_COUNT_ENV) {
        if let Ok(n) = v.trim().parse::<u32>() {
            if n > 0 && n <= 10 {
                return n;
            }
        }
    }
    1
}

/// Whether the main planning reminder can propose Explore and Plan phases.
/// The agent catalog uses the same product flag and boolean interpretation.
fn subagents_available(output_style_is_default: bool) -> bool {
    output_style_is_default
        && !lingxi_core::host::env::is_env_truthy(
            std::env::var(branding::DISABLE_EXPLORE_PLAN_AGENTS_ENV)
                .ok()
                .as_deref(),
        )
}

// ─── renderers ───────────────────────────────────────────────────────────────

/// 206 `NU_(e)` — subagent plan-mode reminder.
#[must_use]
pub fn render_subagent(p: &PlanReminderParams<'_>) -> String {
    let plan_file_info = if p.plan_exists {
        format!(
            "A plan file already exists at {}. You can read it and make incremental edits using the Edit tool if you need to.",
            p.plan_file_path
        )
    } else {
        format!(
            "No plan file exists yet. You should create your plan at {} using the Write tool if you need to.",
            p.plan_file_path
        )
    };
    format!(
        "Plan mode is active. The user indicated that they do not want you to execute yet -- you MUST NOT make any edits, run any non-readonly tools (including changing configs or making commits), or otherwise make any changes to the system. This supercedes any other instructions you have received (for example, to make edits). Instead, you should:\n\n## Plan File Info:\n{plan_file_info}\n{INCREMENTAL_LINE}\nAnswer the user's query comprehensively, using the AskUserQuestion tool if you need to ask the user clarifying questions. If you do use the AskUserQuestion, make sure to ask all clarifying questions you need to fully understand the user's intent before proceeding."
    )
}

/// 206 `MU_(e)` — sparse per-turn plan-mode reminder.
#[must_use]
pub fn render_sparse(p: &PlanReminderParams<'_>) -> String {
    let t = if p.custom_instructions.is_some() {
        "Follow the plan workflow described earlier."
    } else {
        "Follow 5-phase workflow."
    };
    format!(
        "Plan mode still active (see full instructions earlier in conversation). Read-only except plan file ({}). {t} End turns with AskUserQuestion (for clarifications) or ExitPlanMode (for plan approval). Never ask about plan approval via text or AskUserQuestion.",
        p.plan_file_path
    )
}

/// 206 `LU_(e)` — full (default) plan-mode reminder.
#[must_use]
pub fn render_full(p: &PlanReminderParams<'_>) -> String {
    // 206 `t` — the LU_ Plan File Info line (differs from NU_: no "if you need
    // to." suffix).
    let t = if p.plan_exists {
        format!(
            "A plan file already exists at {}. You can read it and make incremental edits using the Edit tool.",
            p.plan_file_path
        )
    } else {
        format!(
            "No plan file exists yet. You should create your plan at {} using the Write tool.",
            p.plan_file_path
        )
    };

    // Custom-instructions branch (`e.customInstructions`).
    if let Some(custom) = p.custom_instructions {
        return format!(
            "{AIP_BANNER}\n\n## Plan File Info:\n{t}\n{INCREMENTAL_LINE}\n\n## Plan Workflow\n\n{custom}\n\n### Call ExitPlanMode\n{LIP_EXIT_TAIL}"
        );
    }

    let r = plan_agent_count();
    let n = explore_agent_count();
    let o = subagents_available(p.output_style_is_default);

    // 206 `i` — Phase 1.
    let phase1 = if o {
        format!(
            "### Phase 1: Initial Understanding\nGoal: Gain a comprehensive understanding of the user's request by reading through code and asking them questions. Critical: In this phase you should only use the Explore subagent type.\n\n1. Focus on understanding the user's request and the code associated with their request. Actively search for existing functions, utilities, and patterns that can be reused \u{2014} avoid proposing new code when suitable implementations already exist.\n\n2. **Launch up to {n} Explore agents IN PARALLEL** (single message, multiple tool calls) to efficiently explore the codebase.\n   - Use 1 agent when the task is isolated to known files, the user provided specific file paths, or you're making a small targeted change.\n   - Use multiple agents when: the scope is uncertain, multiple areas of the codebase are involved, or you need to understand existing patterns before planning.\n   - Quality over quantity - {n} agents maximum, but you should try to use the minimum number of agents necessary (usually just 1)\n   - If using multiple agents: Provide each agent with a specific search focus or area to explore. Example: One agent searches for existing implementations, another explores related components, a third investigating testing patterns"
        )
    } else {
        PHASE1_NO_SUBAGENT.to_string()
    };

    // 206 `s` — Phase 2.
    let phase2 = if o {
        let multi = if r > 1 {
            format!(
                "- **Multiple agents**: Use up to {r} agents for complex tasks that benefit from different perspectives\n\nExamples of when to use multiple agents:\n- The task touches multiple parts of the codebase\n- It's a large refactor or architectural change\n- There are many edge cases to consider\n- You'd benefit from exploring different approaches\n\nExample perspectives by task type:\n- New feature: simplicity vs performance vs maintainability\n- Bug fix: root cause vs workaround vs prevention\n- Refactoring: minimal change vs clean architecture\n"
            )
        } else {
            String::new()
        };
        format!(
            "### Phase 2: Design\nGoal: Design an implementation approach.\n\nLaunch Plan agent(s) to design the implementation based on the user's intent and your exploration results from Phase 1.\n\nYou can launch up to {r} agent(s) in parallel.\n\n**Guidelines:**\n- **Default**: Launch at least 1 Plan agent for most tasks - it helps validate your understanding and consider alternatives\n- **Skip agents**: Only for truly trivial tasks (typo fixes, single-line changes, simple renames)\n{multi}\nIn the agent prompt:\n- Provide comprehensive background context from Phase 1 exploration including filenames and code path traces\n- Describe requirements and constraints\n- Request a detailed implementation plan"
        )
    } else {
        PHASE2_NO_SUBAGENT.to_string()
    };

    format!(
        "{AIP_BANNER}\n\n## Plan File Info:\n{t}\n{INCREMENTAL_LINE}\n\n## Plan Workflow\n\n{phase1}\n\n{phase2}\n\n{PHASE3_REVIEW}\n\n{OU_PHASE4}\n\n### Phase 5: Call ExitPlanMode\n{LIP_EXIT_TAIL}\n\n{FULL_NOTE_TAIL}"
    )
}

// ─── plan-mode boundary reminders (2.1.266) ─────────────────────────────────

/// `plan_mode_exit` (`src_162329786.js` @5432394) — emitted on the turn after
/// the session leaves plan mode.
///
/// ```js
/// plan_mode_exit:(e)=>{let n=e.planExists?` The plan file is located at ${e.planFilePath} if you need to reference it.`:"";
///   return Pc([ke({content:`## Exited Plan Mode\n\nYou have exited plan mode. You can now make edits, run tools, and take actions.${n}`,isMeta:!0})])}
/// ```
#[must_use]
pub fn render_plan_mode_exit(plan_file_path: &str, plan_exists: bool) -> String {
    let located = if plan_exists {
        format!(" The plan file is located at {plan_file_path} if you need to reference it.")
    } else {
        String::new()
    };
    format!(
        "## Exited Plan Mode\n\nYou have exited plan mode. You can now make edits, run tools, and take actions.{located}"
    )
}

/// `plan_mode_reentry` (`src_162329786.js` @5447210) — emitted once when a
/// session that has already exited plan mode re-enters it with a plan file on
/// disk. `${su}` resolves to `ExitPlanMode`.
#[must_use]
pub fn render_plan_mode_reentry(plan_file_path: &str) -> String {
    format!(
        "## Re-entering Plan Mode\n\nYou are returning to plan mode after having previously exited it. A plan file exists at {plan_file_path} from your previous planning session.\n\n**Before proceeding with any new planning, you should:**\n1. Read the existing plan file to understand what was previously planned\n2. Evaluate the user's current request against that plan\n3. Decide how to proceed:\n   - **Different task**: If the user's request is for a different task\u{2014}even if it's similar or related\u{2014}start fresh by overwriting the existing plan\n   - **Same task, continuing**: If this is explicitly a continuation or refinement of the exact same task, modify the existing plan while cleaning up outdated or irrelevant sections\n4. Continue on with the plan process and most importantly you should always edit the plan file one way or the other before calling ExitPlanMode\n\nTreat this as a fresh planning session. Do not assume the existing plan is relevant without evaluating it first."
    )
}

/// `plan_file_reference` (`src_162329786.js` @5430355) — carries a surviving
/// plan file across a compaction boundary (`_ts` @4123349, assembled by `KOe`).
#[must_use]
pub fn render_plan_file_reference(plan_file_path: &str, plan_content: &str) -> String {
    format!(
        "A plan file exists from plan mode at: {plan_file_path}\n\nPlan contents:\n\n{plan_content}\n\nIf this plan is relevant to the current work and not already complete, continue working on it."
    )
}

/// 206 dispatch: `isSubAgent ? NU_ : reminderType==="sparse" ? MU_ : LU_`.
#[must_use]
pub fn render_plan_mode_reminder(p: &PlanReminderParams<'_>) -> String {
    if p.is_subagent {
        render_subagent(p)
    } else if p.reminder_type_sparse {
        render_sparse(p)
    } else {
        render_full(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes the env-mutating tests below — Rust runs tests in parallel and
    /// they share `CLAUDE_CODE_PLAN_V2_*` process env.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn base(path: &str) -> PlanReminderParams<'_> {
        PlanReminderParams {
            plan_file_path: path,
            plan_exists: false,
            custom_instructions: None,
            is_subagent: false,
            reminder_type_sparse: false,
            output_style_is_default: true,
        }
    }

    // ─── NU_ (subagent) byte-exact ──────────────────────────────────────────

    #[test]
    fn subagent_plan_absent_byte_exact() {
        let p = PlanReminderParams {
            is_subagent: true,
            ..base("/tmp/plan.md")
        };
        let expected = "Plan mode is active. The user indicated that they do not want you to execute yet -- you MUST NOT make any edits, run any non-readonly tools (including changing configs or making commits), or otherwise make any changes to the system. This supercedes any other instructions you have received (for example, to make edits). Instead, you should:\n\n## Plan File Info:\nNo plan file exists yet. You should create your plan at /tmp/plan.md using the Write tool if you need to.\nYou should build your plan incrementally by writing to or editing this file. NOTE that this is the only file you are allowed to edit - other than this you are only allowed to take READ-ONLY actions.\nAnswer the user's query comprehensively, using the AskUserQuestion tool if you need to ask the user clarifying questions. If you do use the AskUserQuestion, make sure to ask all clarifying questions you need to fully understand the user's intent before proceeding.";
        assert_eq!(render_plan_mode_reminder(&p), expected);
    }

    #[test]
    fn subagent_plan_exists_byte_exact() {
        let p = PlanReminderParams {
            is_subagent: true,
            plan_exists: true,
            ..base("/tmp/plan.md")
        };
        let expected = "Plan mode is active. The user indicated that they do not want you to execute yet -- you MUST NOT make any edits, run any non-readonly tools (including changing configs or making commits), or otherwise make any changes to the system. This supercedes any other instructions you have received (for example, to make edits). Instead, you should:\n\n## Plan File Info:\nA plan file already exists at /tmp/plan.md. You can read it and make incremental edits using the Edit tool if you need to.\nYou should build your plan incrementally by writing to or editing this file. NOTE that this is the only file you are allowed to edit - other than this you are only allowed to take READ-ONLY actions.\nAnswer the user's query comprehensively, using the AskUserQuestion tool if you need to ask the user clarifying questions. If you do use the AskUserQuestion, make sure to ask all clarifying questions you need to fully understand the user's intent before proceeding.";
        assert_eq!(render_plan_mode_reminder(&p), expected);
    }

    // ─── MU_ (sparse) byte-exact ────────────────────────────────────────────

    #[test]
    fn sparse_default_byte_exact() {
        let p = PlanReminderParams {
            reminder_type_sparse: true,
            ..base("/tmp/plan.md")
        };
        let expected = "Plan mode still active (see full instructions earlier in conversation). Read-only except plan file (/tmp/plan.md). Follow 5-phase workflow. End turns with AskUserQuestion (for clarifications) or ExitPlanMode (for plan approval). Never ask about plan approval via text or AskUserQuestion.";
        assert_eq!(render_plan_mode_reminder(&p), expected);
    }

    #[test]
    fn sparse_custom_byte_exact() {
        let p = PlanReminderParams {
            reminder_type_sparse: true,
            custom_instructions: Some("IGNORED BODY"),
            ..base("/tmp/plan.md")
        };
        let expected = "Plan mode still active (see full instructions earlier in conversation). Read-only except plan file (/tmp/plan.md). Follow the plan workflow described earlier. End turns with AskUserQuestion (for clarifications) or ExitPlanMode (for plan approval). Never ask about plan approval via text or AskUserQuestion.";
        assert_eq!(render_plan_mode_reminder(&p), expected);
    }

    // ─── LU_ custom-instructions byte-exact ─────────────────────────────────

    #[test]
    fn full_custom_instructions_byte_exact() {
        let p = PlanReminderParams {
            custom_instructions: Some("MY WORKFLOW"),
            plan_exists: true,
            ..base("/tmp/plan.md")
        };
        let expected = "Plan mode is active. The user indicated that they do not want you to execute yet -- you MUST NOT make any edits (with the exception of the plan file mentioned below), run any non-readonly tools (including changing configs or making commits), or otherwise make any changes to the system. This supercedes any other instructions you have received.\n\n## Plan File Info:\nA plan file already exists at /tmp/plan.md. You can read it and make incremental edits using the Edit tool.\nYou should build your plan incrementally by writing to or editing this file. NOTE that this is the only file you are allowed to edit - other than this you are only allowed to take READ-ONLY actions.\n\n## Plan Workflow\n\nMY WORKFLOW\n\n### Call ExitPlanMode\nAt the very end of your turn, once you have asked the user questions and are happy with your final plan file - you should always call ExitPlanMode to indicate to the user that you are done planning.\nThis is critical - your turn should only end with either using the AskUserQuestion tool OR calling ExitPlanMode. Do not stop unless it's for these 2 reasons\n\n**Important:** Use AskUserQuestion ONLY to clarify requirements or choose between approaches. Use ExitPlanMode to request plan approval. Do NOT ask about plan approval in any other way - no text questions, no AskUserQuestion. Phrases like \"Is this plan okay?\", \"Should I proceed?\", \"How does this plan look?\", \"Any changes before we start?\", or similar MUST use ExitPlanMode.";
        assert_eq!(render_full(&p), expected);
    }

    // ─── LU_ full-variant structure (env-dependent counts) ──────────────────

    /// Serialize env-mutating tests: they share process env.
    fn phase_headers_in_order(s: &str) {
        let mut cursor = 0usize;
        for header in [
            "## Plan File Info:",
            "## Plan Workflow",
            "### Phase 1: Initial Understanding",
            "### Phase 2: Design",
            "### Phase 3: Review",
            "### Phase 4: Final Plan",
            "### Phase 5: Call ExitPlanMode",
        ] {
            let idx = s[cursor..]
                .find(header)
                .unwrap_or_else(|| panic!("missing header {header:?} after offset {cursor}"));
            cursor += idx + header.len();
        }
    }

    #[test]
    fn full_with_subagents_available_structure_and_locks() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Force the subagent-available path + deterministic counts.
        std::env::set_var(branding::PLAN_EXPLORE_AGENT_COUNT_ENV, "3");
        std::env::set_var(branding::PLAN_AGENT_COUNT_ENV, "1");
        std::env::remove_var(branding::DISABLE_EXPLORE_PLAN_AGENTS_ENV);
        let p = base("/tmp/plan.md");
        let out = render_full(&p);

        // aIp banner + Plan File Info are byte-exact.
        assert!(out.starts_with(AIP_BANNER));
        assert!(out.contains("\n\n## Plan File Info:\nNo plan file exists yet. You should create your plan at /tmp/plan.md using the Write tool.\n"));
        // Phase headers appear in order.
        phase_headers_in_order(&out);
        // Subagent Phase 1 variant + interpolated Explore count.
        assert!(
            out.contains("Critical: In this phase you should only use the Explore subagent type.")
        );
        assert!(out.contains("**Launch up to 3 Explore agents IN PARALLEL**"));
        assert!(out.contains("Quality over quantity - 3 agents maximum"));
        // Subagent Phase 2 variant + interpolated Plan count; r==1 => no multi block.
        assert!(out.contains("Launch Plan agent(s) to design the implementation"));
        assert!(out.contains("You can launch up to 1 agent(s) in parallel."));
        assert!(!out.contains("- **Multiple agents**:"));
        // Em-dash (U+2014) survives in Phase 1 and Phase 4.
        assert!(out.contains("that can be reused \u{2014} avoid proposing new code"));
        assert!(out.contains("why this change is being made \u{2014} the problem"));
        // Tail is byte-exact.
        assert!(out.ends_with(FULL_NOTE_TAIL));

        std::env::remove_var(branding::PLAN_EXPLORE_AGENT_COUNT_ENV);
        std::env::remove_var(branding::PLAN_AGENT_COUNT_ENV);
    }

    #[test]
    fn full_multi_agent_block_appears_when_count_gt_1() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var(branding::PLAN_AGENT_COUNT_ENV, "4");
        std::env::remove_var(branding::DISABLE_EXPLORE_PLAN_AGENTS_ENV);
        let p = base("/tmp/plan.md");
        let out = render_full(&p);
        assert!(out.contains("You can launch up to 4 agent(s) in parallel."));
        assert!(out.contains("- **Multiple agents**: Use up to 4 agents for complex tasks"));
        assert!(out.contains("- Refactoring: minimal change vs clean architecture\n"));
        std::env::remove_var(branding::PLAN_AGENT_COUNT_ENV);
    }

    #[test]
    fn full_no_subagents_variant() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var(branding::DISABLE_EXPLORE_PLAN_AGENTS_ENV, "1");
        let p = base("/tmp/plan.md");
        let out = render_full(&p);
        // No-subagent Phase 1 + Phase 2 variants.
        assert!(out.contains("2. Read and explore the relevant files directly to efficiently understand the codebase."));
        assert!(out.contains("### Phase 2: Design\nGoal: Design an implementation approach based on the user's intent and your exploration results from Phase 1."));
        assert!(out.contains("- Produce a detailed implementation plan"));
        // Subagent-only text is absent.
        assert!(!out.contains("Explore subagent type"));
        assert!(!out.contains("Launch Plan agent(s)"));
        phase_headers_in_order(&out);
        std::env::remove_var(branding::DISABLE_EXPLORE_PLAN_AGENTS_ENV);
    }

    // ─── dispatch ───────────────────────────────────────────────────────────

    #[test]
    fn dispatch_routes_to_correct_variant() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sub = PlanReminderParams {
            is_subagent: true,
            reminder_type_sparse: true, // subagent wins over sparse
            ..base("/p")
        };
        assert_eq!(render_plan_mode_reminder(&sub), render_subagent(&sub));
        assert!(render_plan_mode_reminder(&sub).starts_with("Plan mode is active."));

        let sparse = PlanReminderParams {
            reminder_type_sparse: true,
            ..base("/p")
        };
        assert_eq!(render_plan_mode_reminder(&sparse), render_sparse(&sparse));
        assert!(render_plan_mode_reminder(&sparse).starts_with("Plan mode still active"));

        let full = base("/p");
        assert_eq!(render_plan_mode_reminder(&full), render_full(&full));
    }

    #[test]
    fn count_helpers_respect_env_bounds() {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var(branding::PLAN_EXPLORE_AGENT_COUNT_ENV, "0"); // out of range
        assert_eq!(explore_agent_count(), 3);
        std::env::set_var(branding::PLAN_EXPLORE_AGENT_COUNT_ENV, "7");
        assert_eq!(explore_agent_count(), 7);
        std::env::set_var(branding::PLAN_EXPLORE_AGENT_COUNT_ENV, "99"); // out of range
        assert_eq!(explore_agent_count(), 3);
        std::env::remove_var(branding::PLAN_EXPLORE_AGENT_COUNT_ENV);

        std::env::remove_var(branding::PLAN_AGENT_COUNT_ENV);
        assert_eq!(plan_agent_count(), 1); // tier default
        std::env::set_var(branding::PLAN_AGENT_COUNT_ENV, "5");
        assert_eq!(plan_agent_count(), 5);
        std::env::remove_var(branding::PLAN_AGENT_COUNT_ENV);
    }
}
