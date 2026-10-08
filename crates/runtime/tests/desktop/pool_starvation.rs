//! M10 (T14) — separate-teammate-pool starvation regression.
//!
//! In a coordinator session `build()` gives the `InProcessTeammate` handler its
//! OWN `StateMachinePool` (`harness_runtime::desktop::TEAMMATE_POOL_CAP`), distinct from
//! the `AgentTool` `subagent_pool` (`PoolSubagentSpawner`, cap 4). Teammates are
//! PERSISTENT: an occupied teammate retains its slot until it is killed. This
//! fixture keeps actual model calls pending. If the two shared one pool,
//! `TEAMMATE_POOL_CAP` occupied teammates
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

use futures::StreamExt;
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
use lingxi_core::host::budget::{BudgetEnforcerHandle, BudgetError};
use lingxi_core::host::subagent_spawn::{
    SubagentInheritance, SubagentResult, SubagentSpawnRequest, SubagentSpawner,
};
use lingxi_core::host::tool_invoker::{SubagentInvocationContext, ToolInvoker, ToolInvokerError};
use lingxi_core::types::AgentId;
use test_harness::mocks::MockRuntimeSpawner;

/// Scripted `SubagentApiClient`: one scripted stream per call, returning
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
    async fn stream(
        &self,
        _request: agent::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let response: Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> = {
            *self.calls.lock().unwrap() += 1;
            Ok(llm_runtime::HistoryResponse {
                id: "scripted".into(),
                model: "scripted".into(),
                content: vec![llm_runtime::ContentBlock::Text {
                    text: "done".into(),
                    cache_control: None, citations: None,
                }],
                stop_reason: Some("end_turn".into()),
                stop_details: None,
                usage: llm_runtime::ExecutionUsage::default(),
                cost: None,
                provider_metadata: serde_json::Value::Null,
            })
        };
        let events = llm_runtime::stream_accumulator::response_to_stream_events(response?);
        Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
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

/// A model whose opened-call future stays pending until the real runner is
/// cancelled. Entry and drop receipts prove each occupied slot has a live
/// model-driven state machine rather than an already-failed allocation.
struct PendingModel {
    entered: tokio::sync::Semaphore,
    cancelled: Arc<tokio::sync::Semaphore>,
}

impl PendingModel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: tokio::sync::Semaphore::new(0),
            cancelled: Arc::new(tokio::sync::Semaphore::new(0)),
        })
    }
}

struct PendingCall(Arc<tokio::sync::Semaphore>);

impl Drop for PendingCall {
    fn drop(&mut self) {
        self.0.add_permits(1);
    }
}

#[async_trait]
impl SubagentApiClient for PendingModel {
    async fn stream(
        &self,
        _request: agent::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let _call = PendingCall(self.cancelled.clone());
        self.entered.add_permits(1);
        std::future::pending().await
    }
}

/// A persistent teammate configured with the required model seam.
fn pending_teammate_ctx(model: Arc<PendingModel>) -> SubagentContext {
    SubagentContext {
        server_fallback_model_enforcement: None,
        handback: None,
        handback_restore_start: None,
        task_registry: None,
        agent_spawn_provenance: Default::default(),
        agent_id: AgentId::new(),
        parent_agent_id: None,
        agent_name: None,
        team_name: None,
        agent_definition: AgentDefinition {
            omit_instructions: false,
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
            offer_provider: None,
        },
        prompt_messages: vec![lingxi_core::types::ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            "hold this teammate's model request open".into(),
        )],
        fork_context_messages: None,
        allowed_tools: vec![],
        worktree_handle: None,
        cwd: None,
        is_async: false,
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
        instruction_context: Default::default(),
        instruction_context_is_override: false,
        instruction_provider: None,
        mobile_runtime_workspace_reminder: None,
        content_replacement_state: None,
        agent_memory: None,
        display: AgentDisplay {
            color: AgentColor::Cyan,
            icon: None,
        },
        model_profile: None,
        model_resolution_context_provider: None,
        api_client: Some(model),
        tool_invoker: None,
        new_diagnostics_source: None,
        tool_schemas: vec![],
        schema: None,
        structured_output_mode: Default::default(),
        structured_output_parse_retries: 0,
        budget: None,
        hook_executor: None,
        stop_hook_scope: Default::default(),
        subagent_stop_firer: None,
        strict_plugin_only_hooks: false,
        skill_loader: None,
        hook_session_id: lingxi_core::types::SessionId::nil(),
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

struct FilledTeammates {
    model: Arc<PendingModel>,
    slots: Vec<(
        AgentId,
        tokio::sync::mpsc::Receiver<agent::runner::SubagentEvent>,
    )>,
}

impl FilledTeammates {
    async fn assert_running(&mut self, pool: &StateMachinePool) {
        assert_eq!(self.model.cancelled.available_permits(), 0);
        for (id, events) in &mut self.slots {
            assert!(
                !pool.agent_runner_finished(id).await,
                "occupied teammate {id} must still be running its pending model call"
            );
            loop {
                match events.try_recv() {
                    Ok(event) => assert!(
                        !matches!(
                            &event,
                            agent::runner::SubagentEvent::Completed { .. }
                                | agent::runner::SubagentEvent::Failed { .. }
                                | agent::runner::SubagentEvent::Killed { .. }
                        ),
                        "occupied teammate {id} emitted a terminal event: {event:?}"
                    ),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        panic!("occupied teammate {id} closed its output channel")
                    }
                }
            }
        }
    }

    async fn cancel(self, pool: &StateMachinePool) {
        for (id, _) in &self.slots {
            pool.deallocate(id)
                .await
                .expect("cancel the actual pending teammate runner");
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            self.model
                .cancelled
                .acquire_many(u32::try_from(TEAMMATE_POOL_CAP).unwrap())
                .await
                .unwrap()
                .forget();
        })
        .await
        .expect("every cancelled runner must drop its pending model call");
        assert_eq!(pool.slot_count().await, 0);
    }
}

/// Saturate the pool and wait until every real runner has reached its model
/// seam. Hold output receivers until cancellation so terminal-state assertions
/// observe the actual runners throughout the admission checks.
async fn fill_with_pending_teammates(pool: &StateMachinePool) -> FilledTeammates {
    let model = PendingModel::new();
    let mut slots = Vec::new();
    for _ in 0..TEAMMATE_POOL_CAP {
        slots.push(
            pool.allocate(pending_teammate_ctx(model.clone()))
                .await
                .expect("pending teammate slot fits under TEAMMATE_POOL_CAP"),
        );
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        model
            .entered
            .acquire_many(u32::try_from(TEAMMATE_POOL_CAP).unwrap())
            .await
            .unwrap()
            .forget();
    })
    .await
    .expect("every occupied slot must actually enter the model seam");
    assert_eq!(
        pool.slot_count().await,
        TEAMMATE_POOL_CAP,
        "every teammate slot is occupied by a live model-driven runner"
    );
    let mut filled = FilledTeammates { model, slots };
    filled.assert_running(pool).await;
    filled
}

fn agent_tool_request() -> SubagentSpawnRequest {
    SubagentSpawnRequest {
        stop_hook_scope: Default::default(),
        agent_spawn_provenance: Default::default(),
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
        instruction_context: None,
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
        parent_model_profile_override: None,
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
        handback_opt_in: false,
        parent_permission_mode: None,
        handback_enabled: None,
        handback_ends_turn_enabled: None,
        restored_handback_state: None,
        restored_handback_history: Vec::new(),
        restore_handback_start: None,
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
    let mut teammates = fill_with_pending_teammates(&teammate_pool).await;

    // The SEPARATE `AgentTool` subagent pool (mirrors `build()`'s `subagent_pool`,
    // cap 4). It is empty — the parked teammates live on a different pool.
    let subagent_pool = Arc::new(StateMachinePool::new(runtime, 4));
    let api = ScriptedApiClient::new();
    let spawner = PoolSubagentSpawner::new(subagent_pool.clone()).with_api_client(api.clone());

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        spawner.spawn(agent_tool_request(), inheritance()),
    )
    .await
    .expect("occupied teammates must not block the separate ordinary pool")
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
    teammates.assert_running(&teammate_pool).await;
    teammates.cancel(&teammate_pool).await;
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
    let mut teammates = fill_with_pending_teammates(&shared_pool).await;

    // The AgentTool spawner backed by that SAME saturated pool.
    let api = ScriptedApiClient::new();
    let spawner = PoolSubagentSpawner::new(shared_pool.clone()).with_api_client(api.clone());

    let err = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        spawner.spawn(agent_tool_request(), inheritance()),
    )
    .await
    .expect("a full shared pool must reject the ordinary spawn promptly")
    .expect_err("a shared, teammate-saturated pool rejects the subagent spawn");

    // `PoolSubagentSpawner` preserves the pool-capacity condition as PoolFull.
    assert!(
        matches!(
            err,
            lingxi_core::host::subagent_spawn::SubagentSpawnError::PoolFull
        ),
        "shared-pool spawn fails pool-full; got {err:?}"
    );
    // The spawn never reached the runner, so no model round-trip occurred.
    assert_eq!(
        *api.calls.lock().unwrap(),
        0,
        "the rejected spawn never invoked the model"
    );
    teammates.assert_running(&shared_pool).await;
    teammates.cancel(&shared_pool).await;
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
    // Three ordinary slots remain free. Occupy the separate panel pool so a
    // further Fusion group truly queues, independent of ordinary admission.
    let pool = Arc::new(StateMachinePool::new(
        runtime.clone(),
        TEAMMATE_POOL_CAP + 3,
    ));
    let mut teammates = fill_with_pending_teammates(&pool).await;

    let api = ScriptedApiClient::new();
    let spawner = Arc::new(PoolSubagentSpawner::new(pool.clone()).with_api_client(api.clone()));

    let panel_occupancy = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        spawner.reserve_fusion_panel_group(
            lingxi_core::host::FUSION_PANEL_POOL_CAP,
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            lingxi_core::host::panel_pool::PanelAdmissionCancellation::new(),
        ),
    )
    .await
    .expect("the empty panel pool must admit its full capacity")
    .expect("hold every panel slot before queuing the second group");
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let waiting = {
        let spawner = spawner.clone();
        tokio::spawn(async move {
            let reservation = spawner.reserve_fusion_panel_group(
                4,
                tokio::time::Instant::now() + std::time::Duration::from_secs(30),
                lingxi_core::host::panel_pool::PanelAdmissionCancellation::new(),
            );
            tokio::pin!(reservation);
            let mut entered_tx = Some(entered_tx);
            std::future::poll_fn(|cx| {
                let polled = std::future::Future::poll(reservation.as_mut(), cx);
                if polled.is_pending() {
                    if let Some(entered_tx) = entered_tx.take() {
                        let _ = entered_tx.send(());
                    }
                }
                polled
            })
            .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
        .await
        .expect("the group must reach the blocked admission future")
        .expect("the saturated panel admission must first return Pending");
    assert!(!waiting.is_finished(), "the panel group is actually queued");

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        spawner.spawn(agent_tool_request(), inheritance()),
    )
    .await
    .expect("the blocked panel group must not hold the ordinary spawn");
    assert!(
        matches!(result, Ok(SubagentResult::Completed { .. })),
        "an ordinary spawn was refused while {} slots were free: {result:?}",
        pool_free_slots(&pool).await
    );
    assert_eq!(
        pool.slot_count().await,
        TEAMMATE_POOL_CAP,
        "the completed ordinary runner must release its slot"
    );
    assert_eq!(
        *api.calls.lock().unwrap(),
        1,
        "the admitted spawn really reached the model"
    );
    teammates.assert_running(&pool).await;
    assert!(
        !waiting.is_finished(),
        "ordinary spawn did not release the panel queue"
    );
    waiting.abort();
    let cancelled = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
        .await
        .expect("cancel the queued panel admission future")
        .expect_err("the blocked panel task must acknowledge cancellation");
    assert!(cancelled.is_cancelled());
    drop(panel_occupancy);
    teammates.cancel(&pool).await;
}

/// Free ordinary slots, derived from the pool's own occupancy so the message
/// above names a real number rather than restating the fixture.
async fn pool_free_slots(pool: &StateMachinePool) -> usize {
    (TEAMMATE_POOL_CAP + 3).saturating_sub(pool.slot_count().await)
}
