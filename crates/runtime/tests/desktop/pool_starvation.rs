//! M10 (T14) — separate-teammate-pool starvation regression.
//!
//! In a coordinator session `build()` gives the `InProcessTeammate` handler its
//! OWN `StateMachinePool` (`harness_runtime::desktop::TEAMMATE_POOL_CAP`), distinct from
//! the `AgentTool` `subagent_pool` (`PoolSubagentSpawner`, cap 4). Teammates are
//! PERSISTENT: each parks on `wait_for_message` and NEVER frees its slot until
//! killed. If the two shared one pool, `TEAMMATE_POOL_CAP` parked teammates
//! would saturate it and every one-shot `AgentTool` subagent spawn would be
//! rejected with `TooManyAgents` — a deadlock for the parent agent.
//!
//! This regression drives the REAL `agent::StateMachinePool` +
//! `agent::PoolSubagentSpawner` with the tokio-backed `MockRuntimeSpawner`:
//!
//! * `pool_starvation_parked_teammates_do_not_starve_agent_tool` — fills a
//!   teammate pool to its cap with parked (never-deallocated) slots, then proves
//!   an `AgentTool` subagent still spawns to completion through the SEPARATE
//!   subagent pool.
//! * `pool_starvation_shared_pool_would_starve_agent_tool` — the inverted
//!   control: routing the subagent spawn through the SAME pool the parked
//!   teammates saturated yields a pool-full failure, proving the separate-pool
//!   decision is load-bearing (this is the assertion that fails against a
//!   shared-pool implementation).

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::Mutex;

use agent::api::SubagentApiClient;
use agent::context::SubagentContext;
use agent::definition::{
    AgentDefinition, AgentModel, AgentPermissionMode, AgentSource, AgentToolPolicy,
};
use agent::display::{AgentColor, AgentDisplay};
use agent::pool::StateMachinePool;
use agent::PoolSubagentSpawner;
use async_trait::async_trait;
use harness_runtime::desktop::TEAMMATE_POOL_CAP;
use platform_api::budget::{BudgetEnforcerHandle, BudgetError};
use platform_api::subagent_spawn::{
    SubagentInheritance, SubagentResult, SubagentSpawnRequest, SubagentSpawner,
};
use platform_api::tool_invoker::{SubagentInvocationContext, ToolInvoker, ToolInvokerError};
use protocol::AgentId;
use test_harness::mocks::MockRuntimeSpawner;

/// Scripted `SubagentApiClient`: one non-streaming round-trip per call, returning
/// a single `end_turn` text turn so the non-persistent subagent loop terminates
/// cleanly in one turn-set and the spawn surfaces `Completed`.
struct ScriptedApiClient {
    calls: Mutex<usize>,
}

impl ScriptedApiClient {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(0),
        })
    }
}

#[async_trait]
impl SubagentApiClient for ScriptedApiClient {
    async fn messages_create(
        &self,
        _model: &str,
        _system: Option<&str>,
        _messages: Vec<protocol::ConversationMessage>,
        _tools: Vec<serde_json::Value>,
    ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
        *self.calls.lock().unwrap() += 1;
        Ok(llm_runtime::HistoryResponse {
            id: "scripted".into(),
            model: "scripted".into(),
            content: vec![llm_runtime::ContentBlock::Text {
                text: "done".into(),
                cache_control: None,
            }],
            stop_reason: Some("end_turn".into()),
            stop_details: None,
            usage: llm_runtime::ExecutionUsage::default(),
            cost: None,
            provider_metadata: serde_json::Value::Null,
        })
    }
}

/// Inert `ToolInvoker` — the scripted single-turn `end_turn` response dispatches
/// no tools, so this is never invoked; it only satisfies the inheritance bundle.
struct InertInvoker;

#[async_trait]
impl ToolInvoker for InertInvoker {
    async fn invoke(
        &self,
        _name: &str,
        _input: serde_json::Value,
        _ctx: SubagentInvocationContext,
    ) -> Result<serde_json::Value, ToolInvokerError> {
        Ok(serde_json::Value::Null)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Permissive budget so the per-turn gate never trips.
struct OpenBudget;

#[async_trait]
impl BudgetEnforcerHandle for OpenBudget {
    async fn check_and_charge(&self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    async fn snapshot_total_nano_usd(&self) -> u64 {
        0
    }
}

/// A minimal persistent-teammate `SubagentContext` with `api_client = None`, so
/// the slot runs the legacy stub runner. With no inbound `lingxi_core::Event` ever
/// delivered, the stub parks forever on `event_rx.recv()` — exactly a persistent
/// teammate idling between turn-sets. The slot is never deallocated, so it holds
/// its pool slot for the life of the test.
fn parked_teammate_ctx() -> SubagentContext {
    SubagentContext {
        task_registry: None,
        agent_id: AgentId::new(),
        parent_agent_id: None,
        agent_name: None,
        team_name: None,
        agent_definition: AgentDefinition {
            cache_ttl: None,
            agent_type: "teammate".into(),
            when_to_use: String::new(),
            tools: AgentToolPolicy::All {
                use_exact_tools: true,
            },
            max_turns: 1,
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
        },
        prompt_messages: vec![],
        fork_context_messages: None,
        allowed_tools: vec![],
        worktree_handle: None,
        cwd: None,
        is_async: false,
        // Marked persistent for fidelity; the stub runner parks regardless.
        persistent: true,
        can_show_permission_prompts: false,
        session_interactive: None,
        origin_session_id: None,
        mcp_clients: vec![],
        transcript_subdir: "/tmp".into(),
        transcript_fs: None,
        resumed_history: None,
        rendered_system_prompt: None,
        mobile_runtime_environment_reminder: None,
        mobile_runtime_workspace_reminder: None,
        content_replacement_state: None,
        agent_memory: None,
        display: AgentDisplay {
            color: AgentColor::Cyan,
            icon: None,
        },
        model_profile: None,
        api_client: None,
        tool_invoker: None,
        new_diagnostics_source: None,
        tool_schemas: vec![],
        schema: None,
        structured_output_mode: Default::default(),
        structured_output_parse_retries: 0,
        budget: None,
        hook_executor: None,
        strict_plugin_only_hooks: false,
        skill_loader: None,
        hook_session_id: protocol::SessionId::nil(),
        hook_cwd: std::path::PathBuf::new(),
        depth: 0,
        observer: None,
        permission_mode_override: None,
        frozen_command_denies: Vec::new(),
        max_output_tokens_per_turn: None,
        max_input_bytes_per_turn: None,
        query_source_label: None,
        correlation_id: None,
        model_attempt: None,
        refusal_fallback_chain: Vec::new(),
    }
}

/// Saturate `pool` with `TEAMMATE_POOL_CAP` parked teammate slots. Returns once
/// every slot is occupied; the slots are never deallocated, mirroring teammates
/// parked between turn-sets.
async fn fill_with_parked_teammates(pool: &StateMachinePool) {
    for _ in 0..TEAMMATE_POOL_CAP {
        pool.allocate(parked_teammate_ctx())
            .await
            .expect("parked teammate slot fits under TEAMMATE_POOL_CAP");
    }
    assert_eq!(
        pool.slot_count().await,
        TEAMMATE_POOL_CAP,
        "every teammate slot is occupied and parked"
    );
}

fn agent_tool_request() -> SubagentSpawnRequest {
    SubagentSpawnRequest {
        teammate_color: None,
        subagent_type: "general-purpose".into(),
        prompt: "do one thing".into(),
        observer: None,
        context_paths: vec![],
        // AgentTool spawn-surface parity params (additive optional).
        description: None,
        model: None,
        model_profile: None,
        name: None,
        team_name: None,
        creator_teammate_name: None,
        creator_team_name: None,
        creator_agent_id: None,
        mode: None,
        isolation: None,
        cwd: None,
        worktree: None,
        fork_context_messages: None,
        fork_parent_system_prompt: None,
        schema: None,
        structured_output_mode: Default::default(),
        structured_output_parse_retries: 0,
        effort: None,
        run_in_background: false,
        tool_use_id: None,
        system_prompt_override: None,
        system_prompt_addendum: None,
        additional_disallowed_tools: Vec::new(),
        depth: 0,
        origin_session_id: None,
        parent_model_override: None,
        forked_skill_name: None,
        forked_skill_attribution: None,
        forked_skill_effort: None,
        frozen_command_denies: Vec::new(),
        resumed_history: None,
        max_turns_override: None,
        max_output_tokens_per_turn: None,
        max_input_bytes_per_turn: None,
        query_source_label: None,
        correlation_id: None,
        model_attempt: None,
    }
}

fn inheritance() -> SubagentInheritance {
    SubagentInheritance {
        tool_invoker: Arc::new(InertInvoker),
        budget: Arc::new(OpenBudget),
    }
}

/// PASS path: separate pools. `TEAMMATE_POOL_CAP` parked teammates saturate the
/// teammate pool, yet an `AgentTool` subagent still spawns to completion through
/// the SEPARATE subagent pool.
#[tokio::test]
async fn pool_starvation_parked_teammates_do_not_starve_agent_tool() {
    let runtime = Arc::new(MockRuntimeSpawner::default());

    // The teammate handler's OWN pool, saturated by parked persistent teammates.
    let teammate_pool = StateMachinePool::new(runtime.clone(), TEAMMATE_POOL_CAP);
    fill_with_parked_teammates(&teammate_pool).await;

    // The SEPARATE `AgentTool` subagent pool (mirrors `build()`'s `subagent_pool`,
    // cap 4). It is empty — the parked teammates live on a different pool.
    let subagent_pool = Arc::new(StateMachinePool::new(runtime, 4));
    let api = ScriptedApiClient::new();
    let spawner = PoolSubagentSpawner::new(subagent_pool.clone()).with_api_client(api.clone());

    let result = spawner
        .spawn(agent_tool_request(), inheritance())
        .await
        .expect("AgentTool subagent spawns through the separate pool");

    assert!(
        matches!(result, SubagentResult::Completed { .. }),
        "the subagent ran to completion despite a full teammate pool; got {result:?}"
    );
    assert!(
        *api.calls.lock().unwrap() >= 1,
        "the subagent runner actually made a model round-trip (real run, not hollow)"
    );
    // The teammate pool is still fully occupied — its parked slots were never freed.
    assert_eq!(
        teammate_pool.slot_count().await,
        TEAMMATE_POOL_CAP,
        "parked teammates kept their slots across the subagent spawn"
    );
    // The subagent freed its own slot on completion.
    assert_eq!(
        subagent_pool.slot_count().await,
        0,
        "the completed subagent deallocated its slot"
    );
}

/// INVERTED CONTROL: one SHARED pool. Routing the `AgentTool` subagent spawn
/// through the very pool the parked teammates saturated yields a pool-full
/// failure — proving the separate-pool decision in `build()` is load-bearing.
/// This assertion is what would fail under a shared-pool implementation.
#[tokio::test]
async fn pool_starvation_shared_pool_would_starve_agent_tool() {
    let runtime = Arc::new(MockRuntimeSpawner::default());

    // ONE pool, sized like the teammate pool, fully occupied by parked teammates.
    let shared_pool = Arc::new(StateMachinePool::new(runtime, TEAMMATE_POOL_CAP));
    fill_with_parked_teammates(&shared_pool).await;

    // The AgentTool spawner backed by that SAME saturated pool.
    let api = ScriptedApiClient::new();
    let spawner = PoolSubagentSpawner::new(shared_pool.clone()).with_api_client(api.clone());

    let err = spawner
        .spawn(agent_tool_request(), inheritance())
        .await
        .expect_err("a shared, teammate-saturated pool rejects the subagent spawn");

    // `PoolSubagentSpawner` preserves the pool-capacity condition as PoolFull.
    assert!(
        matches!(
            err,
            platform_api::subagent_spawn::SubagentSpawnError::PoolFull
        ),
        "shared-pool spawn fails pool-full; got {err:?}"
    );
    // The spawn never reached the runner, so no model round-trip occurred.
    assert_eq!(
        *api.calls.lock().unwrap(),
        0,
        "the rejected spawn never invoked the model"
    );
}

/// P0-2 regression: a queued Fusion panel group must never refuse an ordinary
/// `AgentTool` spawn that the pool has room for.
///
/// Before the Fusion sub-pool, both admission paths shared one `CapacityCore`,
/// and ordinary admission reserved headroom for whatever group sat at the queue
/// head. A user's Agent call was then rejected with the concurrency-cap error
/// while free slots existed — for up to the group's whole admission timeout.
#[tokio::test]
async fn fusion_group_never_refuses_an_agent_tool_spawn() {
    let runtime = Arc::new(MockRuntimeSpawner::default());
    // Three free slots, and a Fusion group that wants four: unsatisfiable, so
    // it waits. Nothing about that may reach ordinary admission.
    let pool = Arc::new(StateMachinePool::new(
        runtime.clone(),
        TEAMMATE_POOL_CAP + 3,
    ));
    fill_with_parked_teammates(&pool).await;

    let api = ScriptedApiClient::new();
    let spawner = Arc::new(PoolSubagentSpawner::new(pool.clone()).with_api_client(api.clone()));

    let waiting = {
        let spawner = spawner.clone();
        tokio::spawn(async move {
            spawner
                .reserve_fusion_panel_group(
                    4,
                    tokio::time::Instant::now() + std::time::Duration::from_secs(30),
                    platform_api::panel_pool::PanelAdmissionCancellation::new(),
                )
                .await
        })
    };
    // Let the group reach the point where it is queued and blocked.
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let result = spawner.spawn(agent_tool_request(), inheritance()).await;
    assert!(
        result.is_ok(),
        "an ordinary spawn was refused while {} slots were free: {result:?}",
        pool_free_slots(&pool).await
    );
    assert_eq!(
        *api.calls.lock().unwrap(),
        1,
        "the admitted spawn really reached the model"
    );
    waiting.abort();
}

/// Free ordinary slots, derived from the pool's own occupancy so the message
/// above names a real number rather than restating the fixture.
async fn pool_free_slots(pool: &StateMachinePool) -> usize {
    (TEAMMATE_POOL_CAP + 3).saturating_sub(pool.slot_count().await)
}
