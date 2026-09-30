use super::*;
use lingxi_core::host::{
    BudgetError, WorkflowOutputAccount, WorkflowOutputEventId, WorkflowOutputScope,
};
use std::sync::atomic::Ordering;
use tokio::sync::Semaphore;

struct BatchProbe {
    session: lingxi_core::types::SessionId,
    generation: lingxi_core::types::MessageId,
    listings: AtomicU64,
    spawned: std::sync::Mutex<Vec<String>>,
    b_listing_entered: Semaphore,
    release_b: Semaphore,
    accounting_failed: Semaphore,
}

#[async_trait]
impl SubagentSpawner for BatchProbe {
    async fn agent_listing(&self) -> Vec<lingxi_core::host::subagent_spawn::SubagentListingEntry> {
        if self.listings.fetch_add(1, Ordering::SeqCst) == 1 {
            self.b_listing_entered.add_permits(1);
            self.release_b.acquire().await.unwrap().forget();
        }
        vec![lingxi_core::host::subagent_spawn::SubagentListingEntry {
            agent_type: "probe".into(),
            when_to_use: String::new(),
            when_to_use_lean: None,
            tools_description: String::new(),
        }]
    }

    async fn spawn(
        &self,
        request: SubagentSpawnRequest,
        _: SubagentInheritance,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.spawned.lock().unwrap().push(request.prompt.clone());
        if request.prompt == "A" {
            // A may complete only after B passed the entry latch and reached
            // an awaited catalog lookup. This isolates the FINAL dispatch check.
            self.b_listing_entered.acquire().await.unwrap().forget();
        }
        Ok(SubagentResult::Completed {
            agent_id: lingxi_core::types::AgentId::new(),
            content: Value::String("answer".into()),
            usage: lingxi_core::host::SubagentUsage {
                output_tokens: 1,
                ..Default::default()
            },
            total_tool_use_count: 0,
            total_duration_ms: 0,
            total_tokens: 1,
            assistant_message_count: 0,
            response_char_count: 0,
            last_request_id: None,
            cumulative_usage: lingxi_core::host::SubagentUsage::default(),
            usage_complete: true,
        })
    }
}

impl WorkflowOutputAccount for BatchProbe {
    fn session_id(&self) -> lingxi_core::types::SessionId {
        self.session
    }
    fn generation_id(&self) -> lingxi_core::types::MessageId {
        self.generation
    }
    fn spent(&self) -> u64 {
        u64::MAX
    }
    fn record_legacy(&self, _: WorkflowOutputEventId, output: u64) -> Result<(), BudgetError> {
        assert!(self.spent().checked_add(output).is_none());
        self.accounting_failed.add_permits(1);
        Err(BudgetError::Internal("output accounting overflow".into()))
    }
}

#[async_trait]
impl ToolInvoker for BatchProbe {
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
#[async_trait]
impl BudgetEnforcerHandle for BatchProbe {
    async fn check_and_charge(&self, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
    async fn snapshot_total_nano_usd(&self) -> u64 {
        0
    }
}

#[tokio::test(flavor = "current_thread")]
async fn output_accounting_failure_blocks_peer_after_awaited_listing() {
    assert!(concurrency_cap() >= 2);
    let probe = Arc::new(BatchProbe {
        session: lingxi_core::types::SessionId::new(),
        generation: lingxi_core::types::MessageId::new(),
        listings: AtomicU64::new(0),
        spawned: std::sync::Mutex::new(Vec::new()),
        b_listing_entered: Semaphore::new(0),
        release_b: Semaphore::new(0),
        accounting_failed: Semaphore::new(0),
    });
    let scope = WorkflowOutputScope::new(probe.clone());
    let run = run_workflow_script_with_live_updates_recorded(
        "return await parallel([() => agent('A', {agentType:'probe'}), () => agent('B', {agentType:'probe'})]);",
        DEFAULT_WORKFLOW_SUBAGENT, "workflow", probe.clone(), probe.clone(), probe.clone(),
        None, None, None, None, None, None, 0, NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)), Arc::new(AnalyticsBus::new()),
        None, None, None, Some(scope),
    );
    let release = async {
        probe.accounting_failed.acquire().await.unwrap().forget();
        // On this current-thread runtime, the producer's synchronous Err path
        // stores its Release latch before this waiter can be polled again.
        probe.release_b.add_permits(1);
    };
    let (_outcome, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(run, release)
    })
    .await
    .expect("both batch members must reach the deterministic barriers");
    assert_eq!(probe.listings.load(Ordering::SeqCst), 2);
    assert_eq!(*probe.spawned.lock().unwrap(), vec!["A".to_string()]);
}
