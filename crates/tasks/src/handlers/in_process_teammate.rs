//! In-process-teammate task handler — M2 implementation.
//!
//! A teammate is a **persistent, message-driven** subagent. Unlike a one-shot
//! `local_agent` run, it does not terminate at the end of a turn-set: after the
//! model stops it parks awaiting the next inbound user message, runs the next
//! turn-set, and so on, until cooperatively shut down. This mirrors the
//! claude-code `InProcessTeammateTask` lifecycle, whose state machine alternates
//! between *processing turns* and *idle-awaiting-input*, accepting injected
//! messages whenever the task is not terminal (`injectUserMessageToTeammate`).
//!
//! ## How persistence is wired
//!
//! The persistence lives entirely in the [`agent`] crate: [`spawn`](Task::spawn)
//! builds a [`agent::SubagentContext`] with `persistent = true` and hands it to
//! [`agent::StateMachinePool::allocate`]. The pool's runner parks on its inbound
//! `event_rx` between turn-sets. This handler:
//!
//! * routes typed text into the running agent via
//!   [`agent::StateMachinePool::send_event`] with an
//!   [`lingxi_core::Event::UserMessage`] (the Rust analogue of
//!   `injectUserMessageToTeammate`), and
//! * pumps the agent's outbound [`agent::SubagentEvent`] stream into the task's
//!   spool file (one line per event), reporting terminal status to a
//!   [`TaskStatusSink`] on `Completed` / `Failed` / `Killed`.
//!
//! ## Shutdown ordering (cooperative, then hard)
//!
//! [`kill`](Task::kill) sends [`lingxi_core::Event::UserExit`] first (giving the
//! runner a chance to emit a clean `Killed` the streaming worker spools), then
//! hard-cancels the slot via [`agent::StateMachinePool::deallocate`]. This
//! matches the TS `requestTeammateShutdown` (cooperative) → `kill` (hard)
//! ordering.
//!
//! ## Swarm auto-claim (oracle 2.1.241 `zvb`/`Vvb`/`rIp`)
//!
//! A teammate auto-claims work from the shared task list at two moments,
//! mirroring the oracle's in-process runner:
//!
//! 1. **Startup** (`if(!standalone) await rIp(...)` before the loop): the
//!    claim's side effect only — the returned prompt is discarded because the
//!    Agent prompt already seeded the first message.
//! 2. **While parked**: one 500ms tick first consumes a queued mailbox message,
//!    then (only when none exists) runs [`check_and_claim_next_task`]. A claimed
//!    task's [`claimed_task_prompt`] is self-injected wearing the
//!    `<teammate-message teammate_id="task-list">` envelope. This preserves the
//!    oracle's strict mailbox-before-task-list ordering and prevents a message
//!    arriving during a model request from cancelling that request.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::handlers::local_bash::{NoopStatusSink, TaskStatusSink};
use crate::id::{generate_task_id, TaskType};
use crate::output_manager::TaskOutputManager;
use crate::state::TaskStatus;
use crate::task_trait::{Task, TaskContext, TaskError, TaskHandle, TaskSpawnInput};

use agent::context::SubagentContext;
use agent::definition::{
    AgentDefinition, AgentModel, AgentPermissionMode, AgentSource, AgentToolPolicy,
};
use agent::display::{AgentColor, AgentDisplay};
use agent::pool::StateMachinePool;
// `PermissionMode` is re-exported from the `agent` crate (which depends on
// `permission`) so `tasks` can reference it without a new `permission` dep.
use agent::model_resolution::{
    ModelResolutionContext, ModelResolutionContextProvider, ResolvedModelSelection,
};
use agent::runner::SubagentEvent;
use agent::PermissionMode;
use agent::SubagentApiClient;

enum TeammateInput {
    Message(String),
    PlanApproval(lingxi_core::host::teammate_plan::PlanApprovalResponse),
}

/// Handler name reported by [`Task::name`] and used as the runtime task-name
/// prefix.
const HANDLER_NAME: &str = "in_process_teammate";

/// The idle-poll cadence of the oracle's in-process runner (2.1.241 `Kvb`
/// polls its mailbox + task list every 500ms while the teammate is parked).
const IDLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Keep at most one already-drained mailbox batch behind the outer bounded
/// mailbox. This prevents the two queues from multiplying their capacities.
const PENDING_MESSAGE_CAPACITY: usize = 1;

/// Canonical sender for the lead's initial assignment and direct messages.
pub const TEAM_LEAD_NAME: &str = "team-lead";

/// Exact Claude Code 2.1.241 teammate-only system-prompt suffix. The leading
/// and trailing line feeds are load-bearing: the default system prompt already
/// ends in one LF, so direct concatenation produces one blank-line boundary.
pub const TEAMMATE_SYSTEM_PROMPT_ADDENDUM: &str = "\n# Agent Teammate Communication\n\
IMPORTANT: You are running as an agent in a team. To communicate with anyone on your team, use the SendMessage tool with `to: \"<name>\"` to send messages to specific teammates.\n\
Just writing a response in text is not visible to others on your team - you MUST use the SendMessage tool.\n\
The user interacts primarily with the team lead. Your work is coordinated through the task system and teammate messaging.\n";

/// Best-effort write of a source-owned team-member activity fact. Failure to
/// update the coordinator roster must not change query/task execution.
async fn publish_team_member_activity(
    status_sink: &dyn TaskStatusSink,
    task_id: &str,
    agent_id: &lingxi_core::types::AgentId,
    active: bool,
) {
    if let Err(error) = status_sink.set_team_member_active(*agent_id, active).await {
        tracing::warn!(
            target: "lingxi_tasks::in_process_teammate",
            task_id,
            agent_id = %agent_id,
            active,
            error = %error,
            "team member activity update failed"
        );
    }
}

// ── Swarm auto-claim (oracle 2.1.241 `zvb` / `Vvb` / `rIp`, in-process runner) ──

/// Pick the next auto-claimable task: the FIRST (the list is id-ascending)
/// task that is `pending`, unowned, and whose every blocker is completed or
/// absent. 1:1 port of oracle `zvb` (2.1.241) — note the owner test
/// is JS-falsy (`if(r.owner)return!1`), so an empty-string owner counts as
/// unowned.
pub(crate) fn pick_next_task(tasks: &[task_store::TodoTask]) -> Option<&task_store::TodoTask> {
    let open: std::collections::HashSet<&str> = tasks
        .iter()
        .filter(|t| t.status != lingxi_core::TodoState::Completed)
        .map(|t| t.id.as_str())
        .collect();
    tasks.iter().find(|t| {
        if t.status != lingxi_core::TodoState::Pending {
            return false;
        }
        if t.owner.as_deref().is_some_and(|o| !o.is_empty()) {
            return false;
        }
        t.blocked_by.iter().all(|b| !open.contains(b.as_str()))
    })
}

/// Build the injected prompt for an auto-claimed task. 1:1 port of oracle
/// `Vvb` (2.1.241). Byte-exact quirks locked by the segment table:
/// a trailing SPACE after the colon at end-of-line, and a leading space
/// before the subject (`` `…task #${id}: \n\n ${subject}` ``); the
/// description (when non-empty) follows after a blank line.
pub(crate) fn claimed_task_prompt(task: &task_store::TodoTask) -> String {
    let mut t = format!(
        "Complete all open tasks. Start with task #{}: \n\n {}",
        task.id, task.subject
    );
    if !task.description.is_empty() {
        t.push_str(&format!("\n\n{}", task.description));
    }
    t
}

/// Escape an XML attribute exactly like the oracle's `Hd`: `&`, `<`, `>`,
/// double quote, then apostrophe.
///
/// Single pass. The chained-`replace` spelling is byte-equivalent (escaping `&`
/// first is what keeps the entities it introduces from being re-escaped) but
/// allocates a fresh `String` and re-scans the whole input five times even when
/// nothing matches — and this runs twice per teammate message.
fn escape_xml_attribute(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Neutralize literal teammate-envelope tags inside untrusted message text.
///
/// Same rule as `lingxi_core::host::subagent_output_guard`'s harness-envelope neutralizer
/// (whose tag list already contains `teammate-message`): `<` before an optional
/// `/`, the tag name case-insensitively, then `>` / `/` / whitespace / end-of-
/// input becomes `<\`. The boundary predicate is IMPORTED from there rather than
/// re-spelled — an ASCII-only `is_ascii_whitespace` would let
/// `<teammate-message\u{a0}>` (NBSP) and `</teammate-message\u{2028}>` through
/// here while the sibling guard escapes them.
fn escape_teammate_tags(text: &str) -> String {
    const TAG: &str = "teammate-message";
    let mut out = String::with_capacity(text.len());
    let mut start = 0;
    // `match_indices` yields guaranteed char-boundary byte offsets, and `<` is
    // ASCII so it can never occur inside a multi-byte sequence.
    for (index, _) in text.match_indices('<') {
        let rest = &text[index + 1..];
        let name = rest.strip_prefix('/').unwrap_or(rest);
        let Some(suffix) = name
            .as_bytes()
            .get(..TAG.len())
            .filter(|prefix| prefix.eq_ignore_ascii_case(TAG.as_bytes()))
            // The 16 matched bytes are ASCII, so `TAG.len()` is a char boundary.
            .map(|_| &name[TAG.len()..])
        else {
            continue;
        };
        if suffix.chars().next().is_none_or(|c| {
            c == '>' || c == '/' || lingxi_core::host::subagent_output_guard::is_js_space(c)
        }) {
            out.push_str(&text[start..=index]);
            out.push('\\');
            start = index + 1;
        }
    }
    out.push_str(&text[start..]);
    out
}

/// Wrap an inter-agent message in the exact `<teammate-message>` envelope used
/// by Claude Code 2.1.241. `summary` keeps its first line, trims it, caps it at
/// 200 UTF-16 code units, and is omitted when empty (`dge` / `l5f`).
pub fn teammate_message_envelope_with_summary(
    from: &str,
    text: &str,
    summary: Option<&str>,
) -> String {
    let summary = summary
        .map(|value| value.split('\n').next().unwrap_or_default().trim())
        .filter(|value| !value.is_empty())
        .map(|value| {
            let mut utf16_units = 0;
            value
                .chars()
                .take_while(|character| {
                    let next = utf16_units + character.len_utf16();
                    if next > 200 {
                        return false;
                    }
                    utf16_units = next;
                    true
                })
                .collect::<String>()
        });
    let summary_attr = summary.as_deref().map_or_else(String::new, |value| {
        format!(" summary=\"{}\"", escape_xml_attribute(value))
    });
    format!(
        "<teammate-message teammate_id=\"{}\"{}>\n{}\n</teammate-message>",
        escape_xml_attribute(from),
        summary_attr,
        escape_teammate_tags(text)
    )
}

/// Convenience wrapper for senders that carry no summary (task-list and the
/// initial team-lead assignment).
#[must_use]
pub fn teammate_message_envelope(from: &str, text: &str) -> String {
    teammate_message_envelope_with_summary(from, text, None)
}

/// Narrow async seam used by the task leaf to obtain the host's freshly
/// assembled default system prompt without depending on the orchestrator.
#[async_trait]
pub trait TeammateSystemPromptRenderer: Send + Sync {
    /// Render the default (not main-thread overridden) system prompt.
    async fn render_default_system_prompt(&self) -> String;
}

fn render_teammate_system_prompt(base: &str, custom: Option<&str>) -> Arc<str> {
    let custom = custom.filter(|value| !value.is_empty());
    let mut rendered = String::with_capacity(
        base.len()
            + TEAMMATE_SYSTEM_PROMPT_ADDENDUM.len()
            + custom.map_or(0, |value| value.len() + 31),
    );
    rendered.push_str(base);
    if !base.is_empty() && !base.ends_with('\n') {
        rendered.push('\n');
    }
    rendered.push_str(TEAMMATE_SYSTEM_PROMPT_ADDENDUM);
    if let Some(custom) = custom {
        rendered.push_str("\n\n# Custom Agent Instructions\n");
        rendered.push_str(custom);
    }
    Arc::from(rendered)
}

/// Resolve the task-list id a teammate's auto-claim reads.
///
/// `LINGXI_TASK_LIST_ID` env override, else the teammate's team name — the
/// SAME first two levels as the Task tools' `resolve_task_list_id`, so the
/// lead's TaskCreate and the teammate's auto-claim always see one list.
///
/// // ORACLE QUIRK (2.1.241): the oracle passes
/// `t.parentSessionId` here, but `initializeSessionTeam` has RENAMED the
/// session task dir to the team-name dir by then, so the oracle's auto-claim
/// reads a stale (usually empty) directory whenever teamName ≠ sessionId.
/// The port deliberately keeps reading the live list (behavior over bug);
/// see the `leader_and_teammate_resolve_same_dir` invariant in tool-task.
///
/// An empty team name (a standalone spawn outside any team) returns `None` —
/// the analogue of the oracle's `standalone: g` gate, which skips both rIp
/// call sites.
pub(crate) fn resolve_teammate_list_id(team_name: &str) -> Option<String> {
    if let Ok(id) = std::env::var("LINGXI_TASK_LIST_ID") {
        if !id.trim().is_empty() {
            return Some(id);
        }
    }
    if team_name.is_empty() {
        return None;
    }
    Some(team_name.to_string())
}

/// One successful claim plus the exact prompt that should wake the teammate.
#[derive(Debug, Clone)]
pub(crate) struct ClaimedTask {
    task_id: String,
    prompt: String,
}

/// Check the shared task list and atomically claim the next available task.
/// 1:1 port of oracle `rIp` (2.1.241): list → [`pick_next_task`]
/// → [`task_store::TodoStore::claim_task`] → mark `in_progress` → return the
/// [`claimed_task_prompt`] text. `None` when there is nothing claimable, the
/// claim loses a race, or any store error occurs (all logged with the
/// oracle's `[inProcessRunner]` message bodies).
pub(crate) async fn check_and_claim_next_task(
    config_home: Option<&std::path::Path>,
    list_id: &str,
    agent_name: &str,
) -> Option<ClaimedTask> {
    let store = config_home.map_or_else(
        || task_store::TodoStore::for_list(list_id),
        |home| task_store::TodoStore::for_list_at(home, list_id),
    );
    let tasks = store.list().await;
    let next = pick_next_task(&tasks)?.clone();
    let res = store
        .claim_task(&next.id, agent_name, task_store::ClaimOptions::default())
        .await;
    if let Some(reason) = res.reason() {
        tracing::info!(
            target: "lingxi_tasks::in_process_teammate",
            "[inProcessRunner] Failed to claim task #{}: {reason}", next.id
        );
        return None;
    }
    // Oracle: `await WXe(e, n.id, {status:"in_progress"})` as a separate
    // follow-up write after the claim.
    store
        .update(&next.id, |t| t.status = lingxi_core::TodoState::InProgress)
        .await;
    tracing::info!(
        target: "lingxi_tasks::in_process_teammate",
        "[inProcessRunner] Claimed task #{}: {}", next.id, next.subject
    );
    Some(ClaimedTask {
        task_id: next.id.clone(),
        prompt: claimed_task_prompt(&next),
    })
}

/// Undo a claim only while it is still the exact untouched claim this runner
/// made. The owner/status guards prevent a late failure from overwriting work
/// that was reassigned or completed concurrently.
async fn rollback_claimed_task(
    config_home: Option<&std::path::Path>,
    list_id: &str,
    agent_name: &str,
    task_id: &str,
) {
    let store = config_home.map_or_else(
        || task_store::TodoStore::for_list(list_id),
        |home| task_store::TodoStore::for_list_at(home, list_id),
    );
    let _ = store
        .update(task_id, |task| {
            if task.owner.as_deref() == Some(agent_name)
                && task.status == lingxi_core::TodoState::InProgress
            {
                task.owner = None;
                task.status = lingxi_core::TodoState::Pending;
            }
        })
        .await;
}

/// Resolves the static [`AgentDefinition`] for a teammate spawn.
///
/// The handler does not own the agent catalog (it would be an over-broad
/// dependency), so definition lookup is delegated through this narrow seam —
/// the same pattern as [`TaskStatusSink`]. The wire step plugs in an adapter
/// over the host's loaded agent registry; [`DefaultTeammateDefinition`] makes
/// the handler usable standalone (and in unit tests) by synthesizing a
/// permissive built-in definition.
#[async_trait]
pub trait TeammateDefinitionResolver: Send + Sync {
    /// Resolve the definition for the teammate identified by `agent_id` /
    /// `name`. Returns `None` when no such definition exists.
    async fn resolve(
        &self,
        agent_id: &lingxi_core::types::AgentId,
        name: &str,
    ) -> Option<AgentDefinition>;
}

/// Default resolver that synthesizes a permissive built-in definition. Lets the
/// handler run without a wired agent catalog (tests / standalone use).
pub struct DefaultTeammateDefinition;

#[async_trait]
impl TeammateDefinitionResolver for DefaultTeammateDefinition {
    async fn resolve(
        &self,
        _agent_id: &lingxi_core::types::AgentId,
        name: &str,
    ) -> Option<AgentDefinition> {
        Some(AgentDefinition {
            omit_instructions: false,
            cache_ttl: None,
            agent_type: name.to_string(),
            when_to_use: String::new(),
            tools: AgentToolPolicy::All {
                use_exact_tools: true,
            },
            max_turns: 64,
            model: AgentModel::Inherit,
            permission_mode: AgentPermissionMode::Bubble,
            source: AgentSource::BuiltIn,
            base_dir: "/tmp".into(),
            system_prompt: None,
            mcp_servers: vec![],
            frontmatter_hooks: vec![],
            icon: None,
            allowed_tools: vec![],
            worktree_requirement: None,
            // Synthesized stub: no extended frontmatter — all defaults.
            disallowed_tools: vec![],
            skills: vec![],
            required_mcp_servers: vec![],
            background: false,
            isolation: None,
            memory: None,
            effort: None,
            initial_prompt: None,
            color: None,
            observer: None,
            offer_provider: None,
        })
    }
}

/// Per-task control block held by the handler so [`Task::send_message`] can
/// route input to the live slot and [`Task::kill`] can tear it down.
struct TeammateEntry {
    /// Slot key in the [`StateMachinePool`].
    agent_id: lingxi_core::types::AgentId,
    /// Cooperative stop flag for the streaming worker. The worker also exits
    /// naturally when `out_rx` closes (the slot's runner future drops its
    /// sender on deallocate); the flag is the belt-and-braces fast path.
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// Coordinator-mailbox messages wait here until the teammate is idle. Only
    /// one drained mailbox batch may sit behind the outer 100-message inbox, so
    /// the two buffering layers cannot multiply to 10,000 pending messages.
    pending_messages: tokio::sync::mpsc::Sender<TeammateInput>,
}

/// Handler for [`TaskType::InProcessTeammate`].
///
/// Holds the host slot pool plus the constructor-injected dependencies needed
/// to build a persistent [`SubagentContext`] (`api_client`, optional
/// `tool_invoker`, the definition resolver) and stream its output (the spool
/// `output` manager). `fs` + `runtime` arrive per-call via [`TaskContext`].
pub struct InProcessTeammateHandler {
    /// Host slot pool: `spawn` → `allocate`, `send_message` → `send_event`,
    /// `kill` → `send_event(UserExit)` + `deallocate`.
    pool: Arc<StateMachinePool>,
    /// Spool-file owner (same role as in `LocalBash` / `MonitorMcp`).
    output: Arc<TaskOutputManager>,
    /// Optional host-resolved config-home root for V2 task-list reads. When
    /// absent, teammate auto-claim preserves the legacy process-global
    /// `HOME`/`LINGXI_CONFIG_DIR` resolution.
    config_home: Option<std::path::PathBuf>,
    /// The session's published plan-file identity. When present, a teammate's
    /// plan file is `ay(agentId)` — `<plansDir>/<slug>-agent-<agentId>.md` —
    /// the exact shape the permission policy's one write carve-out matches and
    /// the directory the session's own reminders name, so the teammate can
    /// actually write it. `None` keeps the legacy
    /// `<config-home>/plans/<agentId>.md`, which only the call-local
    /// `own_plan_file_root` allowance covers (and only once that directory
    /// exists).
    plan_files: Option<Arc<lingxi_core::host::plan_files::PlanFileMatcher>>,
    plan_approval_mailbox: Option<Arc<dyn lingxi_core::host::mailbox::MailboxRouterHandle>>,
    plan_approval_gate: Option<Arc<dyn lingxi_core::host::PermissionGate>>,
    transcript: Option<(Arc<dyn lingxi_core::host::FileSystem>, std::path::PathBuf)>,
    /// Model API seam handed to every spawned teammate's runner.
    api_client: Arc<dyn SubagentApiClient>,
    /// Tool dispatch seam inherited by the teammate. `None` means the teammate
    /// cannot dispatch tools (a `tool_use` then surfaces a runner failure).
    tool_invoker: Option<Arc<dyn lingxi_core::host::ToolInvoker>>,
    /// Resolves the [`AgentDefinition`] for a spawn.
    definitions: Arc<dyn TeammateDefinitionResolver>,
    /// Parent / main-loop model used to resolve a teammate definition's
    /// `AgentModel::Inherit` / family aliases to a concrete wire id (mirrors
    /// `PoolSubagentSpawner::default_model`). Set at boot from `cfg.model` via
    /// [`Self::with_default_model`]. `None` (the default / tests) leaves the
    /// definition's model RAW (legacy: the runner emits `Inherit`→`"inherit"`).
    default_model: Option<String>,
    /// Live/boot permission-mode anchor threaded into
    /// [`agent::resolve_agent_model`] (so an `AgentModel::Inherit` teammate gets
    /// the plan-mode runtime resolution `opusplan`→Opus / `haiku`→Sonnet). Default
    /// `PermissionMode::Default` keeps the Inherit branch returning the parent
    /// model unchanged (mirrors `PoolSubagentSpawner::permission_mode`).
    permission_mode: PermissionMode,
    /// RAW user model setting string (mirrors `getUserSpecifiedModelSetting()`,
    /// e.g. `"opusplan"` / `"haiku"`). Used ONLY for the opusplan/haiku plan-mode
    /// runtime resolution; without it (the default) the Inherit branch returns the
    /// parent model unchanged (mirrors `PoolSubagentSpawner::model_setting`).
    model_setting: Option<String>,
    /// Exact host route authority shared with the parent model selection.
    model_resolution_context_provider: Option<Arc<dyn ModelResolutionContextProvider>>,
    /// Profile selected for the main session's current model route.
    default_model_profile: Option<String>,
    /// Same live session route cell used by the ordinary Agent spawner.
    default_model_selection_provider: Arc<OnceLock<agent::DefaultModelSelectionProvider>>,
    /// Owning session mode for prompt/provider gates inside the independently
    /// spawned persistent runner. `None` preserves the legacy fallback.
    session_interactive: Option<bool>,
    instruction_provider:
        Option<Arc<dyn lingxi_core::host::instructions::InstructionContextProvider>>,
    /// Creates one passive-LSP-diagnostics consumer per teammate. A factory is
    /// required here: sharing one source would also share its dedup cursor, so
    /// the first teammate to poll would consume diagnostics for every peer.
    new_diagnostics_source_factory:
        Option<Arc<dyn Fn() -> Arc<dyn lingxi_core::host::NewDiagnosticsSource> + Send + Sync>>,
    /// Live tool registry used to resolve the teammate's advertised tool
    /// SCHEMAS + dispatch allow-list per spawn (claude-code `assembleToolPool`),
    /// mirroring [`agent::PoolSubagentSpawner`]. A SET-ONCE cell (same
    /// construction cycle-break: the registry is built AFTER the handler is
    /// boxed, so the composition root fills it via
    /// [`Self::tool_registry_handle`]). Unfilled (the default / tests) ⇒ no tools
    /// advertised (chat-only — byte-identical to before this seam).
    tool_registry: Arc<OnceLock<Arc<agent::ToolRegistry>>>,
    /// Tool-wide deny-rule names from the boot permission policy, applied in the
    /// per-spawn tool resolution so a blanket-denied tool never leaks into the
    /// teammate's advertised pool (claude-code `filterToolsByDenyRules`).
    /// SET-ONCE; unfilled ⇒ no filtering.
    tool_wide_deny_names: Arc<OnceLock<Vec<String>>>,
    /// Budget enforcer inherited by the teammate so its turns charge the shared
    /// cumulative cost (claude-code teammates share the session budget). `None`
    /// (the default / tests) ⇒ no per-turn budget gate.
    budget_enforcer: Option<Arc<dyn lingxi_core::host::budget::BudgetEnforcerHandle>>,
    /// Hook executor handed to the teammate's runner so it fires `SubagentStart`
    /// (+ frontmatter hooks) like a normal subagent. SET-ONCE cell (same
    /// cycle-break as [`Self::tool_registry`]); unfilled ⇒ the runner skips the
    /// SubagentStart fire (byte-identical legacy).
    hook_executor: Arc<OnceLock<Arc<hooks::HookExecutorImpl>>>,
    /// Managed hook-slot policy shared with normal subagent spawns.
    strict_plugin_only_hooks: Arc<OnceLock<bool>>,
    /// Skill loader handed to the teammate's runner so it preloads the
    /// definition's frontmatter `skills:`. SET-ONCE; unfilled ⇒ no preloading.
    skill_loader: Arc<OnceLock<Arc<dyn lingxi_core::host::skill_loader::SkillLoader>>>,
    /// Session id + cwd stamped on the `HookContext` the runner builds for the
    /// SubagentStart fire (only consulted when [`Self::hook_executor`] is filled).
    hook_session_id: lingxi_core::types::SessionId,
    hook_cwd: std::path::PathBuf,
    /// Terminal-status sink (same seam as `LocalBashHandler`).
    status_sink: Arc<dyn TaskStatusSink>,
    /// Best-effort seam to fire the `TeammateIdle` hook each time the persistent
    /// runner finishes a turn-set and the teammate is about to park awaiting the
    /// next message ("about to go idle"). `None` (the default) => strict no-op;
    /// the orchestrator injects a real firer via
    /// [`with_teammate_idle_firer`](Self::with_teammate_idle_firer). Mirrors the
    /// `TaskStatusSink` decoupling: the `tasks` leaf cannot reach a live hook
    /// executor, so it calls through this narrow trait instead.
    teammate_idle_firer: hooks::OptionalTeammateIdleFirer,
    /// Host-owned renderer for the default main system prompt. The teammate
    /// appends its canonical addendum and optional custom agent prompt.
    system_prompt_renderer: Arc<OnceLock<Arc<dyn TeammateSystemPromptRenderer>>>,
    /// `task_id` → control block, so `send_message` / `kill` can find the slot.
    entries: Arc<Mutex<HashMap<String, TeammateEntry>>>,
}

impl InProcessTeammateHandler {
    /// Construct a handler with the injected execution dependencies.
    ///
    /// Uses [`DefaultTeammateDefinition`] + [`NoopStatusSink`] by default; swap
    /// them via [`Self::with_definitions`] / [`Self::with_status_sink`].
    #[must_use]
    pub fn new(
        pool: Arc<StateMachinePool>,
        output: Arc<TaskOutputManager>,
        api_client: Arc<dyn SubagentApiClient>,
    ) -> Self {
        Self {
            pool,
            output,
            config_home: None,
            plan_files: None,
            plan_approval_mailbox: None,
            plan_approval_gate: None,
            transcript: None,
            api_client,
            tool_invoker: None,
            definitions: Arc::new(DefaultTeammateDefinition),
            default_model: None,
            permission_mode: PermissionMode::Default,
            model_setting: None,
            model_resolution_context_provider: None,
            default_model_profile: None,
            default_model_selection_provider: Arc::new(OnceLock::new()),
            session_interactive: None,
            instruction_provider: None,
            new_diagnostics_source_factory: None,
            tool_registry: Arc::new(OnceLock::new()),
            tool_wide_deny_names: Arc::new(OnceLock::new()),
            budget_enforcer: None,
            hook_executor: Arc::new(OnceLock::new()),
            strict_plugin_only_hooks: Arc::new(OnceLock::new()),
            skill_loader: Arc::new(OnceLock::new()),
            hook_session_id: lingxi_core::types::SessionId::nil(),
            hook_cwd: std::path::PathBuf::new(),
            status_sink: Arc::new(NoopStatusSink),
            teammate_idle_firer: None,
            system_prompt_renderer: Arc::new(OnceLock::new()),
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Return a clone of the set-once tool-registry cell so the composition root
    /// can fill it AFTER the registry is built (same cycle-break as
    /// [`agent::PoolSubagentSpawner::tool_registry_handle`]). Enables per-spawn
    /// tool resolution. First fill wins; unfilled ⇒ chat-only teammate.
    #[must_use]
    pub fn tool_registry_handle(&self) -> Arc<OnceLock<Arc<agent::ToolRegistry>>> {
        self.tool_registry.clone()
    }

    /// Builder: set the tool registry immediately (tests).
    #[must_use]
    pub fn with_tool_registry(self, registry: Arc<agent::ToolRegistry>) -> Self {
        let _ = self.tool_registry.set(registry);
        self
    }

    /// Return a clone of the set-once tool-wide-deny-names cell (filled from the
    /// boot permission policy, like the spawner).
    #[must_use]
    pub fn tool_wide_deny_names_handle(&self) -> Arc<OnceLock<Vec<String>>> {
        self.tool_wide_deny_names.clone()
    }

    /// Builder: inherit a budget enforcer so the teammate's turns charge the
    /// shared cumulative cost.
    #[must_use]
    pub fn with_budget_enforcer(
        mut self,
        enforcer: Arc<dyn lingxi_core::host::budget::BudgetEnforcerHandle>,
    ) -> Self {
        self.budget_enforcer = Some(enforcer);
        self
    }

    /// Return a clone of the set-once hook-executor cell so the composition root
    /// can fill it after the `HookExecutorImpl` exists (enables SubagentStart).
    #[must_use]
    pub fn hook_executor_handle(&self) -> Arc<OnceLock<Arc<hooks::HookExecutorImpl>>> {
        self.hook_executor.clone()
    }

    /// Builder: set the hook executor immediately (the executor already exists
    /// when the teammate handler is constructed at the composition root, unlike
    /// the spawner's deferred cell). Enables the teammate runner's SubagentStart.
    #[must_use]
    pub fn with_hook_executor(self, executor: Arc<hooks::HookExecutorImpl>) -> Self {
        let _ = self.hook_executor.set(executor);
        self
    }

    /// Return the set-once managed hook-policy cell.
    #[must_use]
    pub fn strict_plugin_only_hooks_handle(&self) -> Arc<OnceLock<bool>> {
        self.strict_plugin_only_hooks.clone()
    }

    /// Return a clone of the set-once skill-loader cell.
    #[must_use]
    pub fn skill_loader_handle(
        &self,
    ) -> Arc<OnceLock<Arc<dyn lingxi_core::host::skill_loader::SkillLoader>>> {
        self.skill_loader.clone()
    }

    /// Set the session id + cwd stamped on the teammate runner's SubagentStart
    /// `HookContext` (only consulted when the hook executor is filled).
    #[must_use]
    pub fn with_hook_context(
        mut self,
        session_id: lingxi_core::types::SessionId,
        cwd: std::path::PathBuf,
    ) -> Self {
        self.hook_session_id = session_id;
        self.hook_cwd = cwd;
        self
    }

    /// Pin teammate auto-claim reads to a host-resolved config home.
    #[must_use]
    pub fn with_config_home(mut self, config_home: std::path::PathBuf) -> Self {
        self.config_home = Some(config_home);
        self
    }

    /// Publish the session's plan-file identity, so teammate plan files land in
    /// the session's plans directory (`ay(agentId)`) instead of the config home.
    #[must_use]
    pub fn with_plan_files(
        mut self,
        plan_files: Arc<lingxi_core::host::plan_files::PlanFileMatcher>,
    ) -> Self {
        self.plan_files = Some(plan_files);
        self
    }

    /// Attach the tool dispatch seam inherited by spawned teammates.
    #[must_use]
    pub fn with_tool_invoker(mut self, invoker: Arc<dyn lingxi_core::host::ToolInvoker>) -> Self {
        self.tool_invoker = Some(invoker);
        self
    }

    /// Set the owning session mode for this handler's independent runners.
    #[must_use]
    pub fn with_instruction_provider(
        mut self,
        provider: Arc<dyn lingxi_core::host::instructions::InstructionContextProvider>,
    ) -> Self {
        self.instruction_provider = Some(provider);
        self
    }

    #[must_use]
    pub fn with_session_interactive(mut self, interactive: bool) -> Self {
        self.session_interactive = Some(interactive);
        self
    }

    /// Attach a factory for independent passive LSP diagnostic cursors.
    ///
    /// Teammates share the parent workspace, so the host captures the live
    /// session cwd in the factory rather than passing a per-agent worktree.
    #[must_use]
    pub fn with_new_diagnostics_source_factory(
        mut self,
        factory: Arc<dyn Fn() -> Arc<dyn lingxi_core::host::NewDiagnosticsSource> + Send + Sync>,
    ) -> Self {
        self.new_diagnostics_source_factory = Some(factory);
        self
    }

    /// Set the parent / main-loop model used to resolve a teammate's
    /// `AgentModel::Inherit` / bare family aliases to a concrete wire id (see
    /// [`agent::resolve_agent_model`]). Wire this from `cfg.model` at boot;
    /// without it, the definition's model is passed through raw (legacy).
    #[must_use]
    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Attach the same host-owned route resolver used by the main session and
    /// ordinary Agent spawns. Family aliases cannot be resolved without this
    /// authority.
    #[must_use]
    pub fn with_model_resolution_context_provider(
        mut self,
        provider: Arc<dyn ModelResolutionContextProvider>,
    ) -> Self {
        self.model_resolution_context_provider = Some(provider);
        self
    }

    /// Set the profile that actually serves the main session's default model.
    #[must_use]
    pub fn with_default_model_profile(mut self, profile: Option<String>) -> Self {
        self.default_model_profile = profile;
        self
    }

    /// Share the ordinary Agent spawner's live model/profile selection cell so
    /// teammates spawned after a session `/model` change use the active route.
    #[must_use]
    pub fn with_default_model_selection_provider_handle(
        mut self,
        provider: Arc<OnceLock<agent::DefaultModelSelectionProvider>>,
    ) -> Self {
        self.default_model_selection_provider = provider;
        self
    }

    /// Set the live/boot permission-mode anchor threaded into
    /// [`agent::resolve_agent_model`] (so an `AgentModel::Inherit` teammate gets
    /// the plan-mode runtime resolution `opusplan`→Opus / `haiku`→Sonnet). Without
    /// it the default (`PermissionMode::Default`) keeps the Inherit branch
    /// returning the parent model unchanged.
    #[must_use]
    pub fn with_permission_mode(mut self, mode: PermissionMode) -> Self {
        self.permission_mode = mode;
        self
    }

    /// Set the RAW user model setting string (mirrors
    /// `getUserSpecifiedModelSetting()`, e.g. `"opusplan"` / `"haiku"`). Used ONLY
    /// for the opusplan/haiku plan-mode runtime resolution; without it the Inherit
    /// branch returns the parent model unchanged.
    #[must_use]
    pub fn with_model_setting(mut self, setting: impl Into<String>) -> Self {
        self.model_setting = Some(setting.into());
        self
    }

    /// Attach a custom [`TeammateDefinitionResolver`] (e.g. an adapter over the
    /// host's loaded agent catalog).
    #[must_use]
    /// Share the leader inbox used for nonmutating teammate plan review.
    pub fn with_plan_approval_mailbox(
        mut self,
        mailbox: Arc<dyn lingxi_core::host::mailbox::MailboxRouterHandle>,
    ) -> Self {
        self.plan_approval_mailbox = Some(mailbox);
        self
    }

    /// Live mode availability used when the lead approves a teammate plan.
    pub fn with_plan_approval_gate(
        mut self,
        gate: Arc<dyn lingxi_core::host::PermissionGate>,
    ) -> Self {
        self.plan_approval_gate = Some(gate);
        self
    }

    /// Attach the session transcript filesystem and subagent directory.
    pub fn with_transcript(
        mut self,
        fs: Arc<dyn lingxi_core::host::FileSystem>,
        directory: std::path::PathBuf,
    ) -> Self {
        self.transcript = Some((fs, directory));
        self
    }

    pub fn with_definitions(mut self, definitions: Arc<dyn TeammateDefinitionResolver>) -> Self {
        self.definitions = definitions;
        self
    }

    /// Attach a [`TaskStatusSink`] so terminal transitions are reported.
    #[must_use]
    pub fn with_status_sink(mut self, sink: Arc<dyn TaskStatusSink>) -> Self {
        self.status_sink = sink;
        self
    }

    /// Inject the best-effort `TeammateIdle` hook firer. Default-`None` builder
    /// (the firer-seam pattern): existing constructors and tests stay no-op; the
    /// composition root threads the orchestrator's firer here so each completed
    /// turn-set (the teammate parking awaiting the next message) fires the
    /// `TeammateIdle` hook — claude-code `executeTeammateIdleHooks`
    /// (`stopHooks.ts:403`).
    #[must_use]
    pub fn with_teammate_idle_firer(mut self, firer: Arc<dyn hooks::TeammateIdleFirer>) -> Self {
        self.teammate_idle_firer = Some(firer);
        self
    }

    /// Attach the host's live default-system-prompt renderer. Production uses a
    /// weak orchestrator adapter to avoid a registry/orchestrator reference
    /// cycle; tests can inject a static renderer.
    #[must_use]
    pub fn with_system_prompt_renderer(
        self,
        renderer: Arc<dyn TeammateSystemPromptRenderer>,
    ) -> Self {
        let _ = self.system_prompt_renderer.set(renderer);
        self
    }

    /// Return the set-once renderer cell for composition roots whose
    /// orchestrator is constructed after the task registry.
    #[must_use]
    pub fn system_prompt_renderer_handle(
        &self,
    ) -> Arc<OnceLock<Arc<dyn TeammateSystemPromptRenderer>>> {
        self.system_prompt_renderer.clone()
    }

    /// Build the persistent [`SubagentContext`] for a spawn.
    ///
    /// `name` is the teammate's DISPLAY name and `team_name` the coordinator
    /// team it belongs to (empty when spawned standalone). Both ride on the
    /// context as [`SubagentContext::agent_name`] / [`SubagentContext::team_name`]
    /// so the runner threads them into every dispatched tool's
    /// [`lingxi_core::host::tool_invoker::SubagentInvocationContext`] — the Rust analogue of
    /// claude-code running the teammate inside `runWithTeammateContext` so
    /// `getAgentName()` / `getTeammateContext()?.teamName` resolve inside its
    /// tool calls.
    async fn build_context(
        &self,
        agent_id: lingxi_core::types::AgentId,
        name: &str,
        team_name: &str,
        description: &str,
        definition: AgentDefinition,
    ) -> Result<SubagentContext, TaskError> {
        self.build_context_for_profile(agent_id, name, team_name, description, None, definition)
            .await
    }

    async fn build_context_for_profile(
        &self,
        agent_id: lingxi_core::types::AgentId,
        name: &str,
        team_name: &str,
        description: &str,
        route_profile: Option<&str>,
        definition: AgentDefinition,
    ) -> Result<SubagentContext, TaskError> {
        let environmental_model = std::env::var(branding::SUBAGENT_MODEL_ENV)
            .ok()
            .filter(|model| !model.is_empty());
        self.build_context_for_selection(
            agent_id,
            name,
            team_name,
            description,
            route_profile,
            None,
            None,
            environmental_model.as_deref(),
            definition,
        )
        .await
        .map(|(context, _)| context)
    }

    #[allow(clippy::too_many_arguments)]
    async fn build_context_for_selection(
        &self,
        agent_id: lingxi_core::types::AgentId,
        name: &str,
        team_name: &str,
        description: &str,
        route_profile: Option<&str>,
        parent_model_override: Option<&str>,
        parent_profile_override: Option<&str>,
        environmental_model: Option<&str>,
        mut definition: AgentDefinition,
    ) -> Result<
        (
            SubagentContext,
            Option<lingxi_core::host::team_spawn::TeammateModelSelection>,
        ),
        TaskError,
    > {
        // Resolve the model preference to a concrete wire id, mirroring the
        // `PoolSubagentSpawner` seam (`Inherit`→parent model, family alias→
        // concrete id), so a wired teammate runs against a live provider. Unset
        // `default_model` (tests / no boot wiring) leaves it RAW (legacy).
        let has_live_selection_provider = self.default_model_selection_provider.get().is_some();
        let explicit_parent = parent_model_override.filter(|model| !model.is_empty());
        let live_selection = if explicit_parent.is_some() {
            None
        } else {
            self.default_model_selection_provider
                .get()
                .map(|provider| provider())
                .transpose()
                .map_err(|error| TaskError::Internal(error.to_string()))?
                .flatten()
        };
        if has_live_selection_provider && live_selection.is_none() && explicit_parent.is_none() {
            return Err(TaskError::Internal(
                "model/provider selection is unavailable".into(),
            ));
        }
        let parent_model = explicit_parent
            .or_else(|| {
                live_selection
                    .as_ref()
                    .map(|selection| selection.model.as_str())
            })
            .or(self.default_model.as_deref());
        let parent_profile = parent_profile_override.or_else(|| {
            live_selection
                .as_ref()
                .and_then(|selection| selection.model_profile.as_deref())
                .or(self.default_model_profile.as_deref())
        });
        let environmental_model = environmental_model.filter(|model| !model.is_empty());
        let route_profile = environmental_model
            .is_none()
            .then_some(route_profile)
            .flatten();
        let selected_preference = environmental_model
            .map(|model| AgentModel::Explicit(model.to_owned()))
            .unwrap_or_else(|| definition.model.clone());
        let mut selected_profile = None;
        let mut admitted_route = None;
        let mut child_selection = None;
        if let Some(parent_model) = parent_model {
            let parent_context = if let Some(selection) = live_selection.as_ref() {
                if parent_model == selection.model
                    && parent_profile == selection.model_profile.as_deref()
                {
                    selection.model_resolution_context.clone()
                } else if let Some(provider) = &self.model_resolution_context_provider {
                    provider
                        .context_for_route(parent_model, parent_profile)
                        .map_err(|error| TaskError::Internal(error.to_string()))?
                } else {
                    return Err(TaskError::Internal(
                        "parent model route override has no host route resolver".into(),
                    ));
                }
            } else if let Some(provider) = &self.model_resolution_context_provider {
                provider
                    .context_for_route(parent_model, parent_profile)
                    .map_err(|error| TaskError::Internal(error.to_string()))?
            } else {
                ModelResolutionContext {
                    route: agent::model_resolution::ModelRouteFacts {
                        model: parent_model.to_owned(),
                        profile: parent_profile.map(str::to_owned),
                        ..Default::default()
                    },
                    ..Default::default()
                }
            };
            let uses_parent_policy = environmental_model.is_none()
                && route_profile.is_none()
                && match &selected_preference {
                    AgentModel::Inherit => true,
                    AgentModel::Alias(model) | AgentModel::Explicit(model) => {
                        agent::model_resolution::is_relative_model_alias(model)
                    }
                };
            let preference = if uses_parent_policy {
                agent::resolve_agent_model_with_context(
                    &selected_preference,
                    parent_model,
                    self.permission_mode,
                    self.model_setting.as_deref(),
                    &parent_context,
                )
                .map_err(|error| TaskError::Internal(error.to_string()))?
            } else {
                match &selected_preference {
                    AgentModel::Explicit(model) | AgentModel::Alias(model) => model.clone(),
                    AgentModel::Inherit => parent_model.to_owned(),
                }
            };
            let selection = if let Some(provider) = &self.model_resolution_context_provider {
                let profile = if uses_parent_policy {
                    parent_context.route.profile.as_deref()
                } else {
                    route_profile
                };
                let mut selection = agent::model_resolution::resolve_user_model_selection(
                    &preference,
                    profile,
                    &parent_context,
                    provider.as_ref(),
                )
                .map_err(|error| TaskError::Internal(error.to_string()))?;
                // A same-route Bedrock child inherits its parent's region.
                // An environmental or cross-provider selection owns its route.
                if environmental_model.is_none()
                    && selection.model_profile == parent_context.route.profile
                    && selection.model_resolution_context.route.provider
                        == Some(agent::ModelProviderKind::Bedrock)
                {
                    let prefixed = agent::resolve_agent_model_with_context(
                        &AgentModel::Explicit(selection.model.clone()),
                        parent_model,
                        self.permission_mode,
                        self.model_setting.as_deref(),
                        &selection.model_resolution_context,
                    )
                    .map_err(|error| TaskError::Internal(error.to_string()))?;
                    selection = agent::model_resolution::resolve_user_model_selection(
                        &prefixed,
                        selection.model_profile.as_deref(),
                        &selection.model_resolution_context,
                        provider.as_ref(),
                    )
                    .map_err(|error| TaskError::Internal(error.to_string()))?;
                }
                selection
            } else {
                if route_profile.is_some() && route_profile != parent_profile {
                    return Err(TaskError::Internal(
                        "explicit model profile has no host route resolver".into(),
                    ));
                }
                let resolved = agent::resolve_agent_model_with_context(
                    &selected_preference,
                    parent_model,
                    self.permission_mode,
                    self.model_setting.as_deref(),
                    &parent_context,
                )
                .map_err(|error| TaskError::Internal(error.to_string()))?;
                let mut context = parent_context;
                context.route.model = resolved.clone();
                ResolvedModelSelection {
                    model: resolved,
                    model_profile: context.route.profile.clone(),
                    model_resolution_context: context,
                }
            };
            selected_profile = selection.model_profile.clone();
            admitted_route = Some(lingxi_core::host::team_spawn::TeammateModelSelection {
                model: selection.model.clone(),
                model_profile: selected_profile.clone(),
            });
            definition.model = AgentModel::Explicit(selection.model.clone());
            child_selection = Some(selection);
        }
        // Advertise the teammate's tool pool (claude-code `assembleToolPool`) via
        // the SAME shared resolver `PoolSubagentSpawner` uses, keyed on the
        // resolved model/profile. Unfilled registry ⇒ empty (chat-only). With
        // tools advertised the teammate can actually emit
        // `tool_use`; the dispatch allow-list guards what the inherited invoker runs.
        let (tool_schemas, allowed_tools) = match self.tool_registry.get() {
            Some(registry) => {
                let _has_task_list_tools =
                    agent::augment_teammate_tool_policy(registry, &mut definition);
                let empty: Vec<String> = Vec::new();
                let denied = self.tool_wide_deny_names.get().unwrap_or(&empty);
                agent::resolve_subagent_tools(
                    registry,
                    &definition,
                    denied,
                    child_selection.as_ref(),
                    0,
                    // Every in-process teammate is a coordinator worker, so
                    // shared and inline `role:"comms"` MCP tools stay with
                    // the lead and are not advertised to the worker.
                    true,
                    // §24b agent-scoped MCP servers are a Task-tool-spawn-only
                    // concern (claude `Agr`); an in-process teammate has no
                    // equivalent connect step, so this is always empty
                    // (byte-identical to before this feature).
                    &[],
                )
                .await
                .map_err(|e| TaskError::Internal(e.to_string()))?
            }
            None => (Vec::new(), Vec::new()),
        };
        // The Agent prompt is the lead's initial assignment. Claude
        // Code runs it through the same teammate-message renderer as mailbox
        // input, with `from:"team-lead"`; a raw user message changes both the
        // prompt bytes and the trust boundary.
        let prompt_messages = if description.is_empty() {
            vec![]
        } else {
            vec![lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                teammate_message_envelope(TEAM_LEAD_NAME, description),
            )]
        };
        // Oracle order: freshly assembled default prompt, exact teammate
        // addendum, then an optional `# Custom Agent Instructions` section.
        // A standalone/test handler without a renderer still receives the
        // teammate addendum; production always wires the live renderer.
        let base_system_prompt = match self.system_prompt_renderer.get() {
            Some(renderer) => renderer.render_default_system_prompt().await,
            None => String::new(),
        };
        let rendered_system_prompt =
            render_teammate_system_prompt(&base_system_prompt, definition.system_prompt.as_deref());
        let icon = definition.icon.clone();
        let context = SubagentContext {
            server_fallback_model_enforcement: None,
            handback: None,
            handback_restore_start: None,
            task_registry: self.status_sink.task_registry(),
            agent_id,
            parent_agent_id: None,
            agent_spawn_provenance: Default::default(),
            // Swarm identity (claude-code `TeammateContext.agentName` /
            // `.teamName`): the DISPLAY name is always reachable here (it is the
            // spawn input); `team_name` is threaded from the coordinator team via
            // the spawn input. An empty value (standalone spawn / no team) becomes
            // `None` — the leader / main-thread default. The runner reads these
            // and threads them into every dispatched tool's
            // `SubagentInvocationContext`, so the merged swarm `TaskUpdate`
            // side-effects (auto-owner, owner-change mailbox notification) and
            // `getTaskListId()` actually fire for this teammate.
            agent_name: (!name.is_empty()).then(|| name.to_string()),
            team_name: (!team_name.is_empty()).then(|| team_name.to_string()),
            agent_definition: definition,
            prompt_messages,
            fork_context_messages: None,
            allowed_tools,
            worktree_handle: None,
            // Teammates run in the shared session (no per-agent worktree).
            cwd: None,
            origin_session_id: None,
            is_async: false,
            // The defining trait of a teammate: park between turn-sets and
            // resume on the next injected UserMessage.
            persistent: true,
            can_show_permission_prompts: true,
            session_interactive: self.session_interactive,
            mcp_clients: vec![],
            transcript_subdir: self
                .transcript
                .as_ref()
                .map(|(_, path)| path.clone())
                .unwrap_or_else(|| "/tmp".into()),
            transcript_fs: self.transcript.as_ref().map(|(fs, _)| fs.clone()),
            resumed_history: None,
            rendered_system_prompt: Some(rendered_system_prompt),
            mobile_runtime_environment_reminder: None,
            instruction_context: Default::default(),
            instruction_context_is_override: false,
            instruction_provider: self.instruction_provider.clone(),
            mobile_runtime_workspace_reminder: None,
            content_replacement_state: None,
            agent_memory: None,
            display: AgentDisplay {
                color: AgentColor::Cyan,
                icon,
            },
            model_profile: selected_profile,
            model_resolution_context_provider: self.model_resolution_context_provider.clone(),
            api_client: Some(self.api_client.clone()),
            tool_invoker: self.tool_invoker.clone(),
            // Invoke the factory for every context. Cloning one source here
            // would merge teammate cursors and make diagnostics first-reader
            // wins across concurrent workers.
            new_diagnostics_source: self
                .new_diagnostics_source_factory
                .as_ref()
                .map(|factory| factory()),
            // Advertised tool schemas (claude-code `assembleToolPool`) — resolved
            // above from the live registry per the definition's policy.
            tool_schemas,
            // Teammates have no structured-output schema.
            schema: None,
            structured_output_mode: Default::default(),
            structured_output_parse_retries: 0,
            // Inherit the shared budget enforcer when wired (claude-code teammates
            // charge the session's cumulative cost); `None` ⇒ no per-turn gate.
            budget: self.budget_enforcer.clone(),
            // Wire the SubagentStart-hook + skills-preload seam from the set-once
            // cells (filled at the composition root, same as `PoolSubagentSpawner`).
            // Unfilled ⇒ the runner skips them (byte-identical legacy).
            hook_executor: self.hook_executor.get().cloned(),
            stop_hook_scope: lingxi_core::host::subagent_spawn::SubagentStopScope::AgentScoped,
            subagent_stop_firer: self
                .hook_executor
                .get()
                .and_then(|executor| executor.subagent_stop_firer(self.hook_session_id)),
            strict_plugin_only_hooks: self
                .strict_plugin_only_hooks
                .get()
                .copied()
                .unwrap_or(false),
            skill_loader: self.skill_loader.get().cloned(),
            hook_session_id: self.hook_session_id,
            hook_cwd: self.hook_cwd.clone(),
            depth: 0,
            observer: None,
            permission_mode_override: None,
            frozen_command_denies: Vec::new(),
            max_output_tokens_per_turn: None,
            max_input_bytes_per_turn: None,
            query_source_label: None,
            // Teammates do not inherit forked skill execution, so
            // there is no caller correlation id to thread.
            correlation_id: None,
            model_attempt: None,
            refusal_fallback_chain: Vec::new(),
        };
        Ok((context, admitted_route))
    }
}

/// Render one outbound [`SubagentEvent`] as a spool line (no trailing newline;
/// the appender adds it). `None` for events we do not surface.
fn event_line(ev: &SubagentEvent) -> String {
    match ev {
        SubagentEvent::Progress {
            tool_use_count,
            token_count,
            ..
        } => format!("progress: tool_uses={tool_use_count} tokens={token_count}"),
        // Host-only settled transcript state must not enter teammate output.
        SubagentEvent::TranscriptSnapshot { .. } => String::new(),
        // Spools are append-only output logs, not UUID-addressed transcripts;
        // the tombstone is consumed by transcript-aware observers instead.
        SubagentEvent::ServerFallbackTombstone { .. } => String::new(),
        // The host-created row is the visible assistant refusal for this turn.
        // Project its query message once, in the same form as ordinary Message
        // events; this runner emits the typed row instead of a duplicate Message.
        SubagentEvent::ServerFallbackApiErrorRow { row, .. } => {
            let message = serde_json::to_value(row.query_message())
                .expect("fallback API-error query message serializes");
            format!("message: {message}")
        }
        SubagentEvent::Message { message, .. } => {
            format!("message: {message}")
        }
        SubagentEvent::Completed { result, .. } => format!("completed: {result}"),
        SubagentEvent::Failed { error, .. } => format!("failed: {error}"),
        SubagentEvent::Killed { .. } => "killed".to_string(),
    }
}

fn apply_spawn_context(
    context: &mut SubagentContext,
    request: lingxi_core::host::SubagentSpawnRequest,
) {
    context.agent_spawn_provenance = request.agent_spawn_provenance.clone();
    context.instruction_context_is_override =
        context.agent_definition.is_fork() && request.instruction_context.is_some();
    context.instruction_context = request.instruction_context.unwrap_or_default();
    context.resumed_history = request.resumed_history;
    context.cwd = request.cwd.map(Into::into);
    context.origin_session_id = request.origin_session_id;
    context.depth = request.depth;
    // The context builder has already resolved model and profile together.
    // Applying local execution context must not replace that selected route.
    if request.mode.as_deref() == Some("plan") {
        context.permission_mode_override = Some("plan".into());
    }
    context.parent_agent_id = request.creator_agent_id;
    if let Some(color) = request
        .teammate_color
        .as_deref()
        .and_then(|color| match color {
            "red" => Some(AgentColor::Red),
            "blue" => Some(AgentColor::Blue),
            "green" => Some(AgentColor::Green),
            "yellow" => Some(AgentColor::Yellow),
            "purple" => Some(AgentColor::Purple),
            "orange" => Some(AgentColor::Orange),
            "pink" => Some(AgentColor::Pink),
            "cyan" => Some(AgentColor::Cyan),
            _ => None,
        })
    {
        context.display.color = color;
    }
}

/// Map a [`SubagentEvent`] to the terminal [`TaskStatus`] that ends the
/// teammate. `None` for events that do NOT terminate it.
///
/// Crucially, a persistent teammate emits a `Completed` at the end of *every*
/// turn-set yet keeps running (it then parks awaiting the next message), so
/// `Completed` is NOT terminal here — treating it as terminal would make the
/// streaming worker stop, drop `out_rx`, and strand all subsequent turn-sets on
/// a closed channel. Only `Failed` / `Killed` truly end the teammate.
fn terminal_status(ev: &SubagentEvent) -> Option<TaskStatus> {
    match ev {
        SubagentEvent::Failed { .. } => Some(TaskStatus::Failed),
        SubagentEvent::Killed { .. } => Some(TaskStatus::Killed),
        SubagentEvent::Completed { .. }
        | SubagentEvent::Progress { .. }
        | SubagentEvent::Message { .. }
        | SubagentEvent::TranscriptSnapshot { .. }
        | SubagentEvent::ServerFallbackTombstone { .. }
        | SubagentEvent::ServerFallbackApiErrorRow { .. } => None,
    }
}

/// Whether `ev` marks the teammate "about to go idle" — i.e. a turn-set finished
/// and the persistent runner is about to park awaiting the next message.
///
/// This is the Rust analogue of claude-code's `isTeammate()`-gated
/// `executeTeammateIdleHooks` fire (`stopHooks.ts:403`), which runs after the
/// teammate's query loop stops. A persistent teammate emits exactly one
/// `Completed` at the end of *every* turn-set yet keeps running (it is NOT
/// terminal here — see [`terminal_status`]), so `Completed` is precisely the
/// idle moment. `Failed` / `Killed` are terminal (the teammate ends, it does not
/// idle), and `Progress` / `Message` are mid-turn, so none of them are idle.
fn is_idle_event(ev: &SubagentEvent) -> bool {
    matches!(ev, SubagentEvent::Completed { .. })
}

/// Render the model-visible continuation content from one `TeammateIdle` hook
/// result. Blocking feedback is already the oracle's exact bare meta-message
/// text; `additionalContext` uses the generic hook `<system-reminder>` form.
fn teammate_idle_follow_up(outcome: &hooks::TeammateIdleOutcome) -> Option<hooks::ExactHookText> {
    let mut messages: Vec<hooks::ExactHookText> = outcome
        .blocking_feedback
        .iter()
        .cloned()
        .map(Into::into)
        .collect();
    if !outcome.additional_contexts.is_empty() {
        let body = hooks::ExactHookText::join(&outcome.additional_contexts, "\n");
        messages.push(hooks::ExactHookText::wrapped(
            "<system-reminder>\nTeammateIdle hook additional context: ",
            &body,
            "\n</system-reminder>",
        ));
    }
    (!messages.is_empty()).then(|| hooks::ExactHookText::join(&messages, "\n\n"))
}

fn teammate_idle_user_message_event(content: hooks::ExactHookText) -> lingxi_core::Event {
    let message_id = lingxi_core::types::MessageId::new();
    let request_id = lingxi_core::types::RequestId::new();
    if content
        .display
        .encode_utf16()
        .eq(content.utf16_code_units.iter().copied())
    {
        lingxi_core::Event::UserMessage {
            message_id,
            request_id,
            content: content.display,
        }
    } else {
        lingxi_core::Event::UserMessageJsUtf16 {
            message_id,
            request_id,
            content: content.display,
            utf16_code_units: content.utf16_code_units,
        }
    }
}

#[async_trait]
impl Task for InProcessTeammateHandler {
    fn name(&self) -> &str {
        HANDLER_NAME
    }

    fn task_type(&self) -> TaskType {
        TaskType::InProcessTeammate
    }

    async fn spawn(
        &self,
        input: TaskSpawnInput,
        ctx: TaskContext,
    ) -> Result<TaskHandle, TaskError> {
        // 1. Only the InProcessTeammate variant is accepted.
        let TaskSpawnInput::InProcessTeammate {
            spawn_request,
            inheritance,
            agent_id,
            name,
            team_name,
            description,
        } = input
        else {
            return Err(TaskError::Internal(
                "in_process_teammate handler received a non-InProcessTeammate spawn input".into(),
            ));
        };

        if spawn_request
            .as_ref()
            .is_some_and(|request| request.model_profile.is_some() && request.model.is_none())
        {
            return Err(TaskError::Internal(
                "model_profile requires an explicit child model".into(),
            ));
        }

        // 2. Allocate the task id + spool file.
        let task_id = generate_task_id(TaskType::InProcessTeammate);
        let spool_path = self
            .output
            .allocate(&task_id)
            .await
            .map_err(|e| TaskError::Io(e.to_string()))?;
        let spool = spool_path
            .to_str()
            .ok_or_else(|| TaskError::Internal("spool path is not valid UTF-8".into()))?
            .to_string();

        // 3. Resolve the definition and build a persistent SubagentContext.
        let mut definition = self
            .definitions
            .resolve(&agent_id, &name)
            .await
            .ok_or_else(|| {
                TaskError::Internal(format!("no agent definition for teammate {name}"))
            })?;
        if let Some(request) = &spawn_request {
            if let Some(model) = &request.model {
                definition.model = AgentModel::Explicit(model.clone());
            }
        }
        let route_profile = spawn_request
            .as_ref()
            .and_then(|request| request.model_profile.as_deref());
        let environmental_model = std::env::var(branding::SUBAGENT_MODEL_ENV)
            .ok()
            .filter(|model| !model.is_empty());
        let (mut subagent_ctx, admitted_route) = self
            .build_context_for_selection(
                agent_id,
                &name,
                &team_name,
                &description,
                route_profile,
                spawn_request
                    .as_ref()
                    .and_then(|request| request.parent_model_override.as_deref()),
                spawn_request
                    .as_ref()
                    .and_then(|request| request.parent_model_profile_override.as_deref()),
                environmental_model.as_deref(),
                definition,
            )
            .await?;
        if let Some(request) = spawn_request {
            apply_spawn_context(&mut subagent_ctx, request);
        }
        if let Some(inherit) = inheritance {
            subagent_ctx.tool_invoker = Some(inherit.tool_invoker);
            subagent_ctx.budget = Some(inherit.budget);
        }
        let teammate_model_route =
            admitted_route.map(|selection| (selection.model, selection.model_profile));
        let plan_control = match (&self.plan_approval_mailbox, &subagent_ctx.tool_invoker) {
            (Some(mailbox), Some(inner))
                if subagent_ctx.permission_mode_override.as_deref() == Some("plan") =>
            {
                // `ay(agentId)` when the host published an identity: the
                // teammate's plan file belongs in the same plans directory the
                // session's reminders name and the write carve-out matches.
                // Without an identity, the legacy config-home path is kept —
                // the teammate can still write it through the call-local
                // allowance, provided that directory exists.
                let agent_key = agent_id.as_uuid().to_string();
                let path = self
                    .plan_files
                    .as_ref()
                    .and_then(|plan_files| plan_files.plan_file(Some(agent_key.as_str())))
                    .unwrap_or_else(|| {
                        self.config_home
                            .clone()
                            .unwrap_or_else(|| std::path::PathBuf::from(branding::DOT_DIR))
                            .join("plans")
                            .join(format!("{agent_key}.md"))
                    })
                    .display()
                    .to_string();
                let controller = Arc::new(
                    super::teammate_plan::PlanAwareInvoker::new(
                        inner.clone(),
                        mailbox.clone(),
                        ctx.fs.clone(),
                        name.clone(),
                        team_name.clone(),
                        path,
                    )
                    .with_permission_gate(self.plan_approval_gate.clone())
                    .with_status_sink(self.status_sink.clone(), task_id.clone()),
                );
                let plan_file_info = if ctx
                    .fs
                    .read_file(controller.plan_path(), None, None)
                    .await
                    .is_ok()
                {
                    format!(
                        "A plan file already exists at {}. You can read it and make incremental edits using the Edit tool if you need to.",
                        controller.plan_path()
                    )
                } else {
                    format!(
                        "No plan file exists yet. You should create your plan at {} using the Write tool if you need to.",
                        controller.plan_path()
                    )
                };
                subagent_ctx.prompt_messages.insert(0, lingxi_core::types::ConversationMessage::user(lingxi_core::types::MessageId::new(), format!("<system-reminder>\n## Plan File Info:\n{plan_file_info}\nYou should build your plan incrementally by writing to or editing this file. NOTE that this is the only file you are allowed to edit - other than this you are only allowed to take READ-ONLY actions.\n</system-reminder>")));
                let requester: Arc<dyn lingxi_core::host::teammate_plan::TeammatePlanRequester> =
                    controller.clone();
                lingxi_core::host::teammate_plan::register(agent_id, &requester);
                subagent_ctx.tool_invoker = Some(controller.clone());
                Some(controller)
            }
            _ => None,
        };
        // The wired tool resolver already applies the session model gate.
        // Without a registry/model, the oracle falls back to the task env gate.
        const TASK_LIST_TOOLS: [&str; 4] = ["TaskCreate", "TaskGet", "TaskUpdate", "TaskList"];
        let has_task_list_tools = if self.tool_registry.get().is_none() {
            !lingxi_core::host::env::is_env_defined_falsy(
                std::env::var("LINGXI_ENABLE_TASKS").ok().as_deref(),
            )
        } else {
            TASK_LIST_TOOLS
                .iter()
                .all(|name| subagent_ctx.allowed_tools.iter().any(|tool| tool == name))
        };

        // 4. Resolve startup auto-claim configuration. The actual claim and
        //    pool allocation happen inside the activation-gated worker below:
        //    no task is claimed and no provider request starts before the
        //    registry publishes its row/route/cleanup transaction.
        let claim_list_id = has_task_list_tools
            .then(|| resolve_teammate_list_id(&team_name))
            .flatten();
        let claim_config_home = self.config_home.clone();

        // 5. Spawn the activation-gated streaming worker through the runtime
        //    (never tokio::spawn
        //    — D17). It pumps out_rx -> spool, one line per event, and reports
        //    terminal status. It stops on Failed / Killed or when out_rx closes
        //    (the slot's runner dropped its sender on deallocate); it does NOT
        //    stop on Completed, since a persistent teammate emits one Completed
        //    per turn-set yet keeps running.
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_loop = stop.clone();
        let output_manager = self.output.clone();
        let worker_spool_path = spool_path.clone();
        let status_sink = self.status_sink.clone();
        // Best-effort `TeammateIdle` firer + the teammate name / team name it
        // carries. The team_name is now threaded from the coordinator through the
        // spawn input (claude-code `getTeamName()`); an empty value (standalone
        // spawn / no team) rides as `""`, matching claude-code's
        // `getTeamName() ?? ''` fallback.
        let idle_firer = self.teammate_idle_firer.clone();
        let idle_session_id = subagent_ctx
            .origin_session_id
            .unwrap_or(subagent_ctx.hook_session_id);
        let idle_permission_mode = subagent_ctx
            .permission_mode_override
            .clone()
            .unwrap_or_else(|| self.permission_mode.wire_str().to_string());
        let idle_permission_gate = self.plan_approval_gate.clone();
        let idle_name = name.clone();
        let idle_team_name = team_name.clone();
        let worker_task_id = task_id.clone();
        // Idle auto-claim state (oracle `Kvb` poll loop): the pool handle +
        // slot id let the worker self-inject a claimed task's prompt as the
        // next user message, exactly like the mailbox path.
        let claim_pool = self.pool.clone();
        let claim_agent_id = agent_id;
        let claim_name = name.clone();
        let (pending_messages, mut pending_message_rx) =
            tokio::sync::mpsc::channel(PENDING_MESSAGE_CAPACITY);
        let worker_entries = self.entries.clone();
        let (activation_tx, activation_rx) = if status_sink.requires_explicit_activation() {
            let (tx, rx) = tokio::sync::oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let worker = Box::pin(async move {
            if let Some(activation_rx) = activation_rx {
                if activation_rx.await.is_err() {
                    stop_loop.store(true, std::sync::atomic::Ordering::Release);
                    worker_entries.lock().await.remove(&worker_task_id);
                    return;
                }
            }
            if stop_loop.load(std::sync::atomic::Ordering::Acquire) {
                worker_entries.lock().await.remove(&worker_task_id);
                return;
            }

            // Oracle startup order: claim before the first provider request,
            // but only after the registry commit point. Keep the claim token so
            // a subsequent pool-allocation failure can conditionally undo it.
            let startup_claim = match &claim_list_id {
                Some(list_id) => {
                    check_and_claim_next_task(claim_config_home.as_deref(), list_id, &claim_name)
                        .await
                }
                None => None,
            };
            if stop_loop.load(std::sync::atomic::Ordering::Acquire) {
                if let (Some(list_id), Some(claimed)) = (&claim_list_id, &startup_claim) {
                    rollback_claimed_task(
                        claim_config_home.as_deref(),
                        list_id,
                        &claim_name,
                        &claimed.task_id,
                    )
                    .await;
                }
                worker_entries.lock().await.remove(&worker_task_id);
                return;
            }

            let (_aid, mut out_rx) = match claim_pool.allocate(subagent_ctx).await {
                Ok(slot) => slot,
                Err(error) => {
                    if let (Some(list_id), Some(claimed)) = (&claim_list_id, &startup_claim) {
                        rollback_claimed_task(
                            claim_config_home.as_deref(),
                            list_id,
                            &claim_name,
                            &claimed.task_id,
                        )
                        .await;
                    }
                    publish_team_member_activity(
                        status_sink.as_ref(),
                        &worker_task_id,
                        &claim_agent_id,
                        false,
                    )
                    .await;
                    status_sink
                        .set_failed(&worker_task_id, &error.to_string())
                        .await;
                    stop_loop.store(true, std::sync::atomic::Ordering::Release);
                    worker_entries.lock().await.remove(&worker_task_id);
                    return;
                }
            };
            if stop_loop.load(std::sync::atomic::Ordering::Acquire) {
                if let (Some(list_id), Some(claimed)) = (&claim_list_id, &startup_claim) {
                    rollback_claimed_task(
                        claim_config_home.as_deref(),
                        list_id,
                        &claim_name,
                        &claimed.task_id,
                    )
                    .await;
                }
                let _ = claim_pool.deallocate(&claim_agent_id).await;
                worker_entries.lock().await.remove(&worker_task_id);
                return;
            }
            publish_team_member_activity(
                status_sink.as_ref(),
                &worker_task_id,
                &claim_agent_id,
                true,
            )
            .await;
            status_sink
                .set_status(&worker_task_id, TaskStatus::Running)
                .await;
            // `true` while the teammate is parked between turn-sets — the only
            // window in which the oracle's runner polls the task list.
            let mut idle = false;
            let mut tick = tokio::time::interval(IDLE_POLL_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let ev = tokio::select! {
                    ev = out_rx.recv() => match ev {
                        Some(ev) => ev,
                        None => {
                            if !stop_loop.load(std::sync::atomic::Ordering::Acquire) {
                                publish_team_member_activity(
                                    status_sink.as_ref(),
                                    &worker_task_id,
                                    &claim_agent_id,
                                    false,
                                )
                                .await;
                                status_sink
                                    .set_failed(&worker_task_id, "teammate runner closed unexpectedly")
                                    .await;
                            }
                            break;
                        }
                    },
                    _ = tick.tick(), if idle => {
                        // One oracle-aligned idle poll: pending user messages
                        // STRICTLY before task-list auto-claim. `send_message`
                        // only fills this bounded queue, so a message received
                        // while the model is busy cannot cancel the request.
                        if stop_loop.load(std::sync::atomic::Ordering::SeqCst) {
                            break;
                        }
                        let next_message = match pending_message_rx.try_recv() {
                            Ok(message) => {
                                let mut messages = vec![message];
                                while let Ok(message) = pending_message_rx.try_recv() {
                                    messages.push(message);
                                }
                                let mut rendered=Vec::new();
                                for message in messages {
                                    match message {
                                        TeammateInput::Message(text) => rendered.push(text),
                                        TeammateInput::PlanApproval(response) => {
                                            if let Some(control)=&plan_control {
                                                if let Some(text)=control.apply(response).await { rendered.push(teammate_message_envelope(TEAM_LEAD_NAME,&text)); }
                                            }
                                        }
                                    }
                                }
                                let messages=rendered;
                                (!messages.is_empty()).then(|| (messages.join("\n\n"), None))
                            }
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                                if plan_control.as_ref().is_some_and(|control| control.awaiting()) { continue; }
                                match &claim_list_id {
                                    Some(list_id) => check_and_claim_next_task(
                                        claim_config_home.as_deref(),
                                        list_id,
                                        &claim_name,
                                    )
                                    .await
                                    .map(|claimed| {
                                        let content = teammate_message_envelope(
                                            "task-list",
                                            &claimed.prompt,
                                        );
                                        (content, Some(claimed.task_id))
                                    }),
                                    None => None,
                                }
                            }
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                        };
                        // Claiming and mailbox dequeueing both cross await/
                        // scheduling boundaries. A concurrent explicit kill
                        // owns termination; release any just-won claim and do
                        // not reinterpret the missing pool slot as a failure.
                        if stop_loop.load(std::sync::atomic::Ordering::Acquire) {
                            if let (Some(list_id), Some((_, Some(task_id)))) =
                                (&claim_list_id, next_message.as_ref())
                            {
                                rollback_claimed_task(
                                    claim_config_home.as_deref(),
                                    list_id,
                                    &claim_name,
                                    task_id,
                                )
                                .await;
                            }
                            break;
                        }
                        if let Some((content, claimed_task_id)) = next_message {
                            match claim_pool
                                .send_event(
                                    &claim_agent_id,
                                    lingxi_core::Event::UserMessage {
                                        message_id: lingxi_core::types::MessageId::new(),
                                        request_id: lingxi_core::types::RequestId::new(),
                                        content,
                                    },
                                )
                                .await
                            {
                                Ok(()) => {
                                    publish_team_member_activity(
                                        status_sink.as_ref(),
                                        &worker_task_id,
                                        &claim_agent_id,
                                        true,
                                    )
                                    .await;
                                    idle = false;
                                    status_sink.set_status(&worker_task_id, TaskStatus::Running).await;
                                },
                                Err(e) => {
                                    if let (Some(list_id), Some(task_id)) =
                                        (&claim_list_id, claimed_task_id.as_deref())
                                    {
                                        rollback_claimed_task(
                                            claim_config_home.as_deref(),
                                            list_id,
                                            &claim_name,
                                            task_id,
                                        )
                                        .await;
                                    }
                                    if stop_loop.load(std::sync::atomic::Ordering::Acquire) {
                                        break;
                                    }
                                    tracing::warn!(
                                        target: "lingxi_tasks::in_process_teammate",
                                        error = %e,
                                        "idle message injection failed; terminating teammate worker"
                                    );
                                    publish_team_member_activity(
                                        status_sink.as_ref(),
                                        &worker_task_id,
                                        &claim_agent_id,
                                        false,
                                    )
                                    .await;
                                    status_sink
                                        .set_failed(&worker_task_id, &e.to_string())
                                        .await;
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                };
                if stop_loop.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                if matches!(
                    &ev,
                    SubagentEvent::TranscriptSnapshot { .. }
                        | SubagentEvent::ServerFallbackTombstone { .. }
                ) {
                    // Snapshot and tombstone events are host transcript control,
                    // not teammate activity or append-only spool output.
                    continue;
                }
                // Any runner event means the teammate is (or just was) active;
                // is_idle_event re-opens the poll window below.
                idle = false;
                let line = event_line(&ev);
                // Routed through the output manager's `append` so the per-file
                // 5GB disk cap is enforced (T17) and the write uses O_NOFOLLOW
                // (claude-code `diskOutput.ts`, T18).
                if let Err(e) = output_manager
                    .append(&worker_spool_path, &format!("{line}\n"))
                    .await
                {
                    tracing::warn!(
                        target: "lingxi_tasks::in_process_teammate",
                        spool, error = %e, "spool append failed"
                    );
                }
                // A completed turn-set is the "about to go idle" moment. Hook
                // blocking feedback/additional context re-wakes the teammate;
                // `continue:false` ends it; only a no-op result opens the poll
                // window.
                if is_idle_event(&ev) {
                    let outcome = match &idle_firer {
                        Some(firer) => {
                            firer
                                .fire(hooks::TeammateIdleFire {
                                    session_id: idle_session_id,
                                    permission_mode: plan_control
                                        .as_ref()
                                        .map(|control| control.permission_mode())
                                        .or_else(|| {
                                            idle_permission_gate
                                                .as_ref()
                                                .and_then(|gate| gate.permission_mode())
                                        })
                                        .unwrap_or_else(|| idle_permission_mode.clone()),
                                    teammate_name: idle_name.clone(),
                                    team_name: idle_team_name.clone(),
                                })
                                .await
                        }
                        None => hooks::TeammateIdleOutcome::default(),
                    };
                    // The hook can block while an explicit kill tears down the
                    // runner. Once kill has set the stop flag, none of the hook
                    // outcomes may publish a competing terminal status or try
                    // to re-wake the removed slot.
                    if stop_loop.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    if outcome.prevent_continuation {
                        status_sink
                            .set_status(&worker_task_id, TaskStatus::Completed)
                            .await;
                        stop_loop.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                    if outcome.should_continue_working() {
                        let content = teammate_idle_follow_up(&outcome)
                            .expect("continue-working outcome has model-visible content");
                        match claim_pool
                            .send_event(&claim_agent_id, teammate_idle_user_message_event(content))
                            .await
                        {
                            Ok(()) => {
                                publish_team_member_activity(
                                    status_sink.as_ref(),
                                    &worker_task_id,
                                    &claim_agent_id,
                                    true,
                                )
                                .await;
                                idle = false;
                                status_sink
                                    .set_status(&worker_task_id, TaskStatus::Running)
                                    .await;
                            }
                            Err(e) => {
                                if stop_loop.load(std::sync::atomic::Ordering::Acquire) {
                                    break;
                                }
                                tracing::warn!(
                                    target: "lingxi_tasks::in_process_teammate",
                                    error = %e,
                                    "TeammateIdle feedback injection failed"
                                );
                                publish_team_member_activity(
                                    status_sink.as_ref(),
                                    &worker_task_id,
                                    &claim_agent_id,
                                    false,
                                )
                                .await;
                                status_sink
                                    .set_failed(&worker_task_id, &e.to_string())
                                    .await;
                                break;
                            }
                        }
                    } else {
                        // No hook intervention: open the oracle's 500ms
                        // mailbox/task-list poll window.
                        idle = true;
                        status_sink.set_teammate_idle(&worker_task_id).await;
                        tick.reset();
                    }
                }
                if let Some(status) = terminal_status(&ev) {
                    // Failed / Killed end the teammate; a per-turn-set Completed
                    // does not (terminal_status returns None for it), so the
                    // worker keeps pumping subsequent turn-sets.
                    //
                    // A Failed carries its error through `set_failed` so the
                    // lead-facing sink can surface the REASON, not a sentinel —
                    // claude-code 2.1.198's failed idle notification to the
                    // leader (`{idleReason:"failed", completedStatus:"failed",
                    // failureReason}`, binary @216293689).
                    if let SubagentEvent::Failed { error, .. } = &ev {
                        publish_team_member_activity(
                            status_sink.as_ref(),
                            &worker_task_id,
                            &claim_agent_id,
                            false,
                        )
                        .await;
                        status_sink.set_failed(&worker_task_id, error).await;
                    } else {
                        status_sink.set_status(&worker_task_id, status).await;
                    }
                    break;
                }
            }
            stop_loop.store(true, std::sync::atomic::Ordering::Release);
            let _ = claim_pool.deallocate(&claim_agent_id).await;
            worker_entries.lock().await.remove(&worker_task_id);
        });

        // Publish the control block before the runtime can execute the worker,
        // so an immediate terminal result cannot race a stale entry back in.
        self.entries.lock().await.insert(
            task_id.clone(),
            TeammateEntry {
                agent_id,
                stop: stop.clone(),
                pending_messages,
            },
        );

        if let Err(error) = ctx
            .runtime
            .spawn(&format!("{HANDLER_NAME}:{task_id}"), worker)
            .await
        {
            self.entries.lock().await.remove(&task_id);
            let _ = self.pool.deallocate(&agent_id).await;
            return Err(TaskError::Internal(error.to_string()));
        }

        // 7. Cleanup seam: synchronous, so it cannot await, but it must still
        //    release the pool slot. Remove the live entry, stop the streaming
        //    worker, and best-effort schedule the async deallocate on the current
        //    runtime. `Task::kill` remains the authoritative explicit stop path;
        //    cleanup covers parent/session teardown where the registry only has
        //    the returned `TaskHandle`.
        let cleanup_stop = stop;
        let cleanup_pool = self.pool.clone();
        let cleanup_entries = self.entries.clone();
        let cleanup_task_id = task_id.clone();
        let cleanup: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            cleanup_stop.store(true, std::sync::atomic::Ordering::SeqCst);
            let Some(agent_id) = cleanup_entries.try_lock().ok().and_then(|mut entries| {
                entries.remove(&cleanup_task_id).map(|entry| entry.agent_id)
            }) else {
                return;
            };
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let pool = cleanup_pool.clone();
                handle.spawn(async move {
                    let _ = pool.deallocate(&agent_id).await;
                });
            }
        });

        let handle = TaskHandle::new(task_id, Some(cleanup));
        let handle = match teammate_model_route {
            Some((model, profile)) => handle.with_teammate_model_route(model, profile),
            None => handle,
        };
        Ok(match activation_tx {
            Some(activation_tx) => handle.with_activation(move || {
                let _ = activation_tx.send(());
            }),
            None => handle,
        })
    }

    fn supports_messages(&self) -> bool {
        true
    }

    async fn send_message(
        &self,
        task_id: &str,
        message: String,
        _ctx: TaskContext,
    ) -> Result<(), TaskError> {
        // Look up the live bounded pending-message queue. An unknown id means
        // the teammate was never spawned or already terminated.
        let (pending_messages, stop) = {
            let entries = self.entries.lock().await;
            entries
                .get(task_id)
                .map(|entry| (entry.pending_messages.clone(), entry.stop.clone()))
                .ok_or_else(|| TaskError::NotFound(task_id.to_string()))?
        };
        if stop.load(std::sync::atomic::Ordering::Acquire) {
            return Err(TaskError::TerminatedTask);
        }

        // Upstream only consumes pending user messages after the current query
        // loop stops. Queue here; the handler's single 500ms idle poll drains
        // this before considering task-list auto-claim. Awaiting a full channel
        // backpressures the outer mailbox rather than dropping messages.
        pending_messages
            .send(TeammateInput::Message(message))
            .await
            .map_err(|_| TaskError::TerminatedTask)
    }

    async fn send_peer(
        &self,
        task_id: &str,
        envelope: lingxi_core::host::handback::HandbackEnvelope,
        _ctx: TaskContext,
    ) -> Result<(), TaskError> {
        let (agent_id, stop) = {
            let entries = self.entries.lock().await;
            let entry = entries.get(task_id).ok_or(TaskError::TerminatedTask)?;
            (entry.agent_id, entry.stop.clone())
        };
        if stop.load(std::sync::atomic::Ordering::Acquire)
            || !envelope.validate()
            || !matches!(envelope.receipt.recipient, lingxi_core::host::handback::HandbackRecipient::Agent { agent_id: recipient, .. } if recipient == agent_id)
        {
            return Err(TaskError::TerminatedTask);
        }
        // The runner owns this typed queue and buffers arrivals while a model
        // request is in flight. Acceptance does not wait for a resumed turn.
        self.pool
            .send_event(&agent_id, lingxi_core::Event::PeerMessage { envelope })
            .await
            .map_err(|error| TaskError::Internal(error.to_string()))
    }

    async fn apply_plan_approval(
        &self,
        task_id: &str,
        response: lingxi_core::host::teammate_plan::PlanApprovalResponse,
        _ctx: TaskContext,
    ) -> Result<(), TaskError> {
        let sender = self
            .entries
            .lock()
            .await
            .get(task_id)
            .map(|entry| entry.pending_messages.clone())
            .ok_or_else(|| TaskError::NotFound(task_id.into()))?;
        sender
            .send(TeammateInput::PlanApproval(response))
            .await
            .map_err(|_| TaskError::TerminatedTask)
    }

    async fn kill(&self, task_id: &str, _ctx: TaskContext) -> Result<(), TaskError> {
        // Remove the control block. An absent entry is a graceful no-op
        // (already killed / never spawned), mirroring local_bash.
        let entry = self.entries.lock().await.remove(task_id);
        let Some(entry) = entry else {
            return Ok(());
        };
        // Cooperative stop first: give the runner a chance to emit a clean
        // Killed (which the streaming worker spools) before the hard cancel.
        // A send failure (slot already gone) is non-fatal — proceed to
        // deallocate, which is itself idempotent.
        //
        // ⚠️ The stop flag MUST NOT be raised before this point. The worker's
        // event loop tests `stop_loop` immediately after `out_rx.recv()` and
        // BEFORE `output_manager.append(...)` / `terminal_status(&ev)`, so a
        // flag raised first makes the worker drop the very `Killed` (or
        // `Failed { error }`) event this cooperative stop exists to collect —
        // the terminal line never reaches the spool and a real failure reason is
        // replaced by a generic `Killed`.
        publish_team_member_activity(self.status_sink.as_ref(), task_id, &entry.agent_id, false)
            .await;
        let _ = self
            .pool
            .send_event(&entry.agent_id, lingxi_core::Event::UserExit)
            .await;

        // Stop the streaming worker, then hard-cancel the slot (deallocate
        // cancels the run_subagent task). out_rx closes when the slot drops, so
        // the worker would exit on its own too; the flag is the fast path.
        // Raised BEFORE `deallocate` so the worker reads the closed `out_rx` as
        // an intentional teardown rather than reporting
        // `set_failed("teammate runner closed unexpectedly")`.
        entry.stop.store(true, std::sync::atomic::Ordering::Release);
        self.pool
            .deallocate(&entry.agent_id)
            .await
            .map_err(|e| TaskError::Internal(e.to_string()))?;

        self.status_sink
            .set_status(task_id, TaskStatus::Killed)
            .await;
        Ok(())
    }
}

#[cfg(test)]
#[path = "in_process_teammate_test.rs"]
mod in_process_teammate_test;
