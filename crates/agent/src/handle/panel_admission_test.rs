use super::*;
use async_trait::async_trait;
use lingxi_core::host::budget::{BudgetEnforcerHandle, BudgetError};
use lingxi_core::host::panel_pool::PanelAdmissionCancellation as CancellationToken;
use lingxi_core::host::tool_invoker::{SubagentInvocationContext, ToolInvoker, ToolInvokerError};
use serde_json::Value;
use test_harness::mocks::MockRuntimeSpawner;
use tokio::sync::{Barrier, Notify};

struct Invoker;
#[async_trait]
impl ToolInvoker for Invoker {
    async fn invoke(
        &self,
        _: &str,
        _: Value,
        _: SubagentInvocationContext,
    ) -> Result<Value, ToolInvokerError> {
        Ok(Value::Null)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct Budget;
#[async_trait]
impl BudgetEnforcerHandle for Budget {
    async fn check_and_charge(&self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    async fn snapshot_total_nano_usd(&self) -> u64 {
        0
    }
}

fn inherit() -> SubagentInheritance {
    SubagentInheritance {
        tool_invoker: Arc::new(Invoker),
        budget: Arc::new(Budget),
    }
}

fn request() -> SubagentSpawnRequest {
    SubagentSpawnRequest {
        subagent_type: "general-purpose".into(),
        prompt: "answer".into(),
        ..Default::default()
    }
}

fn pool(count: usize) -> Arc<StateMachinePool> {
    Arc::new(StateMachinePool::new(
        Arc::new(MockRuntimeSpawner::default()),
        count,
    ))
}

async fn reserve(spawner: &PoolSubagentSpawner, count: usize) -> lingxi_core::host::PanelPoolLease {
    spawner
        .reserve_fusion_panel_group(
            count,
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            CancellationToken::new(),
        )
        .await
        .unwrap()
}

struct Api {
    both: Option<Arc<Barrier>>,
    entered: Arc<Notify>,
    park: bool,
}
#[async_trait]
impl crate::api::SubagentApiClient for Api {
    async fn stream(
        &self,
        _request: crate::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let response: Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> = async {
            self.entered.notify_one();
            if let Some(both) = &self.both {
                both.wait().await;
            }
            if self.park {
                std::future::pending::<()>().await;
            }
            Ok(llm_runtime::HistoryResponse {
                id: "admitted".into(),
                model: "mock".into(),
                content: vec![llm_runtime::ContentBlock::Text {
                    text: "done".into(),
                    cache_control: None,
                    citations: None,
                }],
                stop_reason: Some("end_turn".into()),
                stop_details: None,
                usage: llm_runtime::ExecutionUsage::default(),
                cost: None,
                provider_metadata: Value::Null,
            })
        }
        .await;
        let events = llm_runtime::stream_accumulator::response_to_stream_events(response?);
        Ok(futures::StreamExt::boxed(futures::stream::iter(
            events.into_iter().map(Ok),
        )))
    }
}

#[tokio::test]
async fn panel_admission_whole_group_runs_two_children_at_zero_unreserved_capacity() {
    let pool = pool(2);
    let spawner = PoolSubagentSpawner::new(pool.clone()).with_api_client(Arc::new(Api {
        both: Some(Arc::new(Barrier::new(2))),
        entered: Arc::new(Notify::new()),
        park: false,
    }));
    let mut permits = reserve(&spawner, 2).await.into_permits();
    assert_eq!(pool.slot_count().await, 0, "reservation is not allocation");
    let first = spawner.spawn_workflow_with_observer_admitted(
        request(),
        inherit(),
        None,
        None,
        Default::default(),
        permits.pop().unwrap(),
    );
    let second = spawner.spawn_workflow_with_observer_admitted(
        request(),
        inherit(),
        None,
        None,
        Default::default(),
        permits.pop().unwrap(),
    );
    // Both API futures must enter before either can finish. Reacquisition fails
    // this test because the entire pool is already reserved by these permits.
    let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(first, second)
    })
    .await
    .expect("both admitted children must reach their shared API barrier");
    assert!(matches!(first.unwrap(), SubagentResult::Completed { .. }));
    assert!(matches!(second.unwrap(), SubagentResult::Completed { .. }));
    drop(reserve(&spawner, 2).await);
    assert_eq!(pool.slot_count().await, 0);
}

#[tokio::test]
async fn panel_admission_wrong_pool_and_unpolled_future_return_original_permit() {
    let source = PoolSubagentSpawner::new(pool(1));
    let other = PoolSubagentSpawner::new(pool(1));
    let permit = reserve(&source, 1).await.into_permits().pop().unwrap();
    assert!(
        other
            .spawn_workflow_with_observer_admitted(
                request(),
                inherit(),
                None,
                None,
                Default::default(),
                permit
            )
            .await
            .is_err()
    );
    drop(reserve(&other, 1).await);
    let permit = reserve(&source, 1).await.into_permits().pop().unwrap();
    let unpolled = source.spawn_workflow_with_observer_admitted(
        request(),
        inherit(),
        None,
        None,
        Default::default(),
        permit,
    );
    drop(unpolled);
    drop(reserve(&source, 1).await);
}

#[tokio::test]
async fn panel_admission_context_rejection_and_running_cancellation_return_capacity() {
    let pool = pool(1);
    let entered = Arc::new(Notify::new());
    let spawner = PoolSubagentSpawner::new(pool.clone()).with_api_client(Arc::new(Api {
        both: None,
        entered: entered.clone(),
        park: true,
    }));
    let unavailable = PoolSubagentSpawner::new(pool.clone())
        .with_default_model_selection_provider(Arc::new(|| Ok(None)));
    let permit = reserve(&unavailable, 1).await.into_permits().pop().unwrap();
    assert!(
        unavailable
            .spawn_workflow_with_observer_admitted(
                request(),
                inherit(),
                None,
                None,
                Default::default(),
                permit
            )
            .await
            .is_err()
    );
    let permit = reserve(&spawner, 1).await.into_permits().pop().unwrap();
    let mut running = Box::pin(spawner.spawn_workflow_with_observer_admitted(
        request(),
        inherit(),
        None,
        None,
        Default::default(),
        permit,
    ));
    tokio::select! {
        result = &mut running => panic!("parked API returned: {result:?}"),
        () = entered.notified() => {}
    }
    assert_eq!(
        spawner.panel_pool().slot_count().await,
        1,
        "a panel occupies the Fusion sub-pool"
    );
    assert_eq!(
        pool.slot_count().await,
        0,
        "and never the ordinary subagent pool"
    );
    drop(running);
    drop(reserve(&spawner, lingxi_core::host::FUSION_PANEL_POOL_CAP).await);
    assert_eq!(spawner.panel_pool().slot_count().await, 0);
}
