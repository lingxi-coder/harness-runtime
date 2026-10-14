//! End-to-end acceptance tests for the local Insights report producer.
//!
//! These tests deliberately use the real embedded Bun worker and temporary
//! project transcripts. Only the LLM side-query boundary is deterministic;
//! this module does not inspect or claim an SDK `ProviderRequest` (that is
//! covered by `insights_query_tests.rs`). The test must fail when Bun 1.3.11+
//! cannot be resolved; it must never skip the source executor path.

use super::{
    InsightsModelSelector, InsightsNativeCommandNameFactsProvider,
    InsightsNativeSideQueryContextProvider, InsightsSelectedModel, InsightsSourceExecutor,
    LocalInsightsReportProvider, RuntimeInsightsRecommendationProvider,
};
use async_trait::async_trait;
use command_api::builtins::insights::{
    InsightsCommandAvailability, InsightsHandler, InsightsReport, InsightsReportProvider,
};
use command_api::model::{BuiltinCommandHandler, CommandResult};
use lingxi_core::host::permission_gate::{PermissionDecision, PermissionGate};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::SessionId;
use llm_runtime::test_support::FixtureTransport;
use llm_runtime::{
    ApiService, BoxFuture, ClientConfig, LlmError, ModelRuntime, ProviderRequest, ProviderResponse,
    StreamingResponse,
};
use serde_json::json;
use sidequery::{SideQueryClient, SideQueryError, SideQueryRequest, SideQueryResponse};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as AsyncMutex;

const SESSION_ID: &str = "ed32091e-d448-41c9-b7bd-e99bbd50a833";

struct FixedModelSelector;

#[async_trait]
impl InsightsModelSelector for FixedModelSelector {
    async fn selected_model(&self) -> Result<InsightsSelectedModel, Utf16JsonProjection> {
        Ok(InsightsSelectedModel {
            model: "claude-haiku-4-5".into(),
            model_source: None,
            profile: Some("insights-report-test".into()),
        })
    }
}

struct NoNativePromptContext;

#[async_trait]
impl InsightsNativeSideQueryContextProvider for NoNativePromptContext {
    async fn context_for_request(
        &self,
        _model: &str,
        _profile: Option<&str>,
        _first_user_text: &[u16],
    ) -> Result<Option<sidequery::NativeSideQueryPromptContext>, Utf16JsonProjection> {
        Ok(None)
    }
}

struct FixedCommandNameFacts;

#[async_trait]
impl InsightsNativeCommandNameFactsProvider for FixedCommandNameFacts {
    async fn q4_open_results(&self) -> Result<Utf16JsonProjection, Utf16JsonProjection> {
        Utf16JsonProjection::parse(
            r#"{"fleetFork":false,"fleetBackground":false,"daemon":false,"skillDoctor":false,"logout":false}"#,
        )
        .map_err(|error| super::error_source(format!("q4 test facts invalid: {error}")))
    }
}

/// Records report-level scheduling and always returns deliberately malformed
/// JSON. The real Bun parser must omit those report sections and still render
/// a complete local report.
#[derive(Default)]
struct MalformedResponses {
    requests: Mutex<Vec<SideQueryRequest>>,
}

impl MalformedResponses {
    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait]
impl SideQueryClient for MalformedResponses {
    async fn query(&self, request: SideQueryRequest) -> Result<SideQueryResponse, SideQueryError> {
        self.requests.lock().unwrap().push(request);
        Ok(SideQueryResponse {
            text: Some(Utf16JsonProjection::plain(json!("not valid JSON"))),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

struct UnusedApiTransport;

impl FixtureTransport for UnusedApiTransport {
    fn execute<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        Box::pin(async {
            panic!("report recommendation with absent policy must not call ApiService")
        })
    }

    fn open_stream<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        Box::pin(async { panic!("Insights report tests do not stream through ApiService") })
    }
}
llm_runtime::impl_fixture_transport!(UnusedApiTransport);

struct NoPolicySnapshot;

#[async_trait]
impl PermissionGate for NoPolicySnapshot {
    async fn check(&self, _name: &str, _input: &Utf16JsonProjection) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

struct CapturingProvider {
    inner: Arc<LocalInsightsReportProvider>,
    report: Mutex<Option<InsightsReport>>,
}

#[async_trait]
impl InsightsReportProvider for CapturingProvider {
    fn command_availability(&self) -> InsightsCommandAvailability {
        self.inner.command_availability()
    }

    async fn generate_report(&self) -> Result<InsightsReport, Utf16JsonProjection> {
        let report = self.inner.generate_report().await?;
        *self.report.lock().unwrap() = Some(report.clone());
        Ok(report)
    }
}

fn api_service() -> Arc<ApiService> {
    let config: ClientConfig = serde_json::from_value(json!({"providers":[{
        "provider_id":"anthropic_first_party",
        "profile_name":"insights-report-test",
        "protocol":"anthropic_messages",
        "base_url":"https://api.anthropic.com",
        "auth":"none",
        "credential":{"type":"none"},
        "models":[{
            "display_model":"claude-haiku-4-5",
            "request_model":"claude-haiku-4-5",
            "billing_model":"claude-haiku-4-5",
            "capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":true,"structured_output":false}
        }]
    }]}))
    .expect("report test service config is valid");
    let runtime = Arc::new(ModelRuntime::from_config(config).expect("model runtime is valid"));
    Arc::new(ApiService::new_with_routing(
        runtime,
        Arc::new(UnusedApiTransport),
        Default::default(),
        Default::default(),
        "insights-report-test",
        None,
        None,
        None,
        Default::default(),
        Some(0),
        Some(0),
    ))
}

fn recommendation_provider() -> Arc<RuntimeInsightsRecommendationProvider> {
    let tool_context = tool_api::test_support::shell_test_ctx(mobile_linux_api::ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    Arc::new(RuntimeInsightsRecommendationProvider {
        permission_gate: Arc::new(NoPolicySnapshot),
        tool_context: Arc::new(tool_context),
        api: api_service(),
        session: Arc::new(AsyncMutex::new(lingxi_core::session::SessionState::empty(
            SessionId::new(),
            "claude-haiku-4-5".into(),
        ))),
    })
}

fn make_provider(
    config_root: PathBuf,
    availability: InsightsCommandAvailability,
    queries: Arc<MalformedResponses>,
    source: Arc<dyn InsightsSourceExecutor>,
) -> Arc<LocalInsightsReportProvider> {
    Arc::new(LocalInsightsReportProvider::new(
        config_root,
        availability,
        queries,
        Arc::new(FixedModelSelector),
        Arc::new(NoNativePromptContext),
        recommendation_provider(),
        source,
        Arc::new(FixedCommandNameFacts),
    ))
}

fn slash_args() -> command_api::parser::ParsedSlashCommand {
    command_api::parser::parse_slash_command(&super::string_source("/insights"))
        .expect("/insights parses")
}

async fn add_temp_project_corpus(config_root: &Path) {
    let project = config_root.join("projects").join("temporary-project");
    tokio::fs::create_dir_all(&project)
        .await
        .expect("temporary project directory is created");
    tokio::fs::write(
        project.join(format!("{SESSION_ID}.jsonl")),
        include_bytes!("fixtures/insights-small-session.jsonl"),
    )
    .await
    .expect("temporary transcript is written");
}

fn report_path(report: &InsightsReport) -> PathBuf {
    PathBuf::from(
        report
            .html_path
            .string_at("")
            .expect("report html path is a string"),
    )
}

#[tokio::test]
async fn local_provider_gates_queries_and_writes_source_worker_report() {
    // No PATH fallback/skip: this integration draft requires the actual Bun
    // executable selected by the host and its real embedded source bundle.
    let bun = super::BunInsightsSourceExecutor::new_from_host_environment()
        .expect("Bun must be installed for the Insights report integration test");
    bun.preflight()
        .await
        .expect("actual Bun 1.3.11+ source worker must pass preflight");
    let source: Arc<dyn InsightsSourceExecutor> = Arc::new(bun);
    let queries = Arc::new(MalformedResponses::default());

    let unavailable = [
        (None, Some(true)),
        (Some(false), Some(true)),
        (Some(true), None),
        (Some(true), Some(false)),
        (None, None),
    ];
    for (workspace_available, allow_insights) in unavailable {
        let root = tempfile::tempdir().expect("unavailable provider temp root");
        let provider = make_provider(
            root.path().to_path_buf(),
            InsightsCommandAvailability {
                workspace_available,
                allow_insights,
            },
            queries.clone(),
            source.clone(),
        );
        assert!(!provider.command_availability().is_available());
        let result = InsightsHandler::new(provider).handle(&slash_args()).await;
        assert!(
            matches!(result, CommandResult::Done { .. }),
            "unknown or false host facts must stop before report generation"
        );
        assert_eq!(
            queries.request_count(),
            0,
            "unavailable report sent a query"
        );
    }

    let success_root = tempfile::tempdir().expect("successful report temp root");
    add_temp_project_corpus(success_root.path()).await;
    let success_provider = make_provider(
        success_root.path().to_path_buf(),
        InsightsCommandAvailability {
            workspace_available: Some(true),
            allow_insights: Some(true),
        },
        queries.clone(),
        source.clone(),
    );
    let captured = Arc::new(CapturingProvider {
        inner: success_provider,
        report: Mutex::new(None),
    });
    let result = InsightsHandler::new(captured.clone())
        .handle(&slash_args())
        .await;
    let content = match result {
        CommandResult::InjectMessage { content } => content
            .string_at("")
            .expect("successful command frame is text")
            .to_owned(),
        other => panic!("both positive facts must run the local report: {other:?}"),
    };
    assert!(
        queries.request_count() >= 8,
        "report queries were not dispatched"
    );

    let report = captured
        .report
        .lock()
        .unwrap()
        .clone()
        .expect("handler must receive the locally generated report");
    assert_eq!(report.auto_mode_recommendation, None);
    assert_eq!(
        report.insights_json.string_at(""),
        Some("{}"),
        "malformed section and at-a-glance responses must be omitted by the real Bun parser"
    );

    let timestamped = report_path(&report);
    assert!(timestamped.starts_with(success_root.path().join("usage-data")));
    assert!(timestamped
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("report-") && name.ends_with(".html")));
    let expected_url = format!("file://{}", timestamped.to_string_lossy());
    assert_eq!(
        report.report_url.string_at(""),
        Some(expected_url.as_str()),
        "the report URL is a literal file:// URL for the timestamped report"
    );
    assert!(content.contains(&expected_url));

    let stable = success_root.path().join("usage-data/report.html");
    let timestamped_bytes = tokio::fs::read(&timestamped)
        .await
        .expect("timestamped report exists");
    let stable_bytes = tokio::fs::read(&stable)
        .await
        .expect("stable report exists");
    assert_eq!(timestamped_bytes, stable_bytes);
    assert!(!timestamped_bytes.is_empty());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            tokio::fs::metadata(&timestamped)
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            tokio::fs::metadata(&stable)
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    // Native writes the timestamped report first and stable report.html
    // second. A failure on the second write is returned without rolling back
    // the first file; this deliberately proves the pair is non-atomic.
    let failure_root = tempfile::tempdir().expect("failed-write temp root");
    add_temp_project_corpus(failure_root.path()).await;
    let usage_root = failure_root.path().join("usage-data");
    tokio::fs::create_dir_all(usage_root.join("report.html"))
        .await
        .expect("stable target is a directory to force the second write to fail");
    let before_failure = queries.request_count();
    let failing_provider = make_provider(
        failure_root.path().to_path_buf(),
        InsightsCommandAvailability {
            workspace_available: Some(true),
            allow_insights: Some(true),
        },
        queries.clone(),
        source,
    );
    let error = failing_provider
        .generate_report()
        .await
        .expect_err("stable report write failure is returned to the caller");
    assert!(error
        .string_at("")
        .is_some_and(|message| message.contains("insights report write failed")));
    assert!(queries.request_count() > before_failure);
    let mut entries = tokio::fs::read_dir(&usage_root)
        .await
        .expect("report directory remains available");
    let mut timestamped_file_exists = false;
    while let Some(entry) = entries.next_entry().await.unwrap() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("report-")
            && entry.file_type().await.unwrap().is_file()
        {
            timestamped_file_exists = true;
            assert!(tokio::fs::metadata(entry.path()).await.unwrap().len() > 0);
        }
    }
    assert!(
        timestamped_file_exists,
        "first report write must remain after second-write failure"
    );
    assert!(usage_root.join("report.html").is_dir());
}
