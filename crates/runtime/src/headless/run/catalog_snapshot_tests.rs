use lingxi_core::host::{ModelListing, OrchestratorHandle};
use orchestrator::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Debug)]
struct CountingCatalogSource(Arc<AtomicUsize>);

impl llm_runtime::client::ModelRuntimeSource for CountingCatalogSource {
    fn current(&self, template: &llm_runtime::ModelRuntime) -> Arc<llm_runtime::ModelRuntime> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Arc::new(template.clone())
    }
}

struct CatalogApi(Arc<llm_runtime::ModelRuntime>);

#[async_trait::async_trait]
impl orchestrator::OrchestratorApiClient for CatalogApi {
    async fn messages_create(
        &self,
        _request: orchestrator::OrchestratorApiRequest,
    ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
        Err(llm_runtime::LlmError::ModelUnavailable)
    }

    fn available_models(&self) -> Vec<String> {
        let _snapshot = self.0.current();
        vec![
            "provider-a/shared-model".into(),
            "provider-b/shared-model".into(),
        ]
    }

    fn list_model_listings(&self) -> Vec<ModelListing> {
        let _snapshot = self.0.current();
        ["provider-a", "provider-b"]
            .into_iter()
            .map(|provider| ModelListing {
                provider_id: provider.into(),
                request_model: "shared-model".into(),
                display_model: "shared-model".into(),
                ..Default::default()
            })
            .collect()
    }

    fn resolve_media_route(
        &self,
        _model: &str,
        _profile: Option<&str>,
    ) -> Result<llm_runtime::MediaRoute, llm_runtime::LlmError> {
        let _snapshot = self.0.current();
        // An unknown generic route must not acquire Native catalog restrictions.
        Err(llm_runtime::LlmError::ModelUnavailable)
    }
}

#[tokio::test]
async fn catalog_snapshot_is_shared_per_listing_and_refreshed_between_operations() {
    let builds = Arc::new(AtomicUsize::new(0));
    let client = llm_runtime::ModelRuntime::from_config(Default::default())
        .unwrap()
        .with_catalog_source(Arc::new(CountingCatalogSource(builds.clone())));
    let root = tempfile::tempdir().unwrap();
    let orchestrator = orchestrator::ConversationOrchestrator::new(
        Default::default(),
        Arc::new(CatalogApi(Arc::new(client))),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        root.path().to_path_buf(),
    );

    let first = orchestrator.list_model_listings().await;
    assert_eq!(
        first.len(),
        2,
        "both providers retain the shared model name"
    );
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    let next = orchestrator.list_model_listings().await;
    assert_eq!(next, first);
    assert_eq!(
        builds.load(Ordering::SeqCst),
        2,
        "a new read refreshes accounts"
    );
    assert_eq!(
        orchestrator.list_available_models().await,
        ["provider-a/shared-model", "provider-b/shared-model"]
    );
    assert_eq!(builds.load(Ordering::SeqCst), 3);
}
