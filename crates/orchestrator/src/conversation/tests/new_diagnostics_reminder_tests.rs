use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

struct MockDiag(Option<String>);
#[async_trait::async_trait]
impl platform_api::NewDiagnosticsSource for MockDiag {
    async fn take_new_diagnostics_block(&self) -> Option<String> {
        self.0.clone()
    }
}

fn orch_with_diag(
    source: Option<Arc<dyn platform_api::NewDiagnosticsSource>>,
) -> ConversationOrchestrator {
    let o = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        std::path::PathBuf::from("/work"),
    );
    match source {
        Some(s) => o.with_new_diagnostics_source(s),
        None => o,
    }
}

#[tokio::test]
async fn injects_block_when_source_has_new_diagnostics() {
    let block = "<new-diagnostics>The following new diagnostic issues were detected:\n\nx.rs:\n  \u{2718} [Line 1:1] boom</new-diagnostics>";
    let orch = orch_with_diag(Some(Arc::new(MockDiag(Some(block.to_string())))));
    let msg = orch
        .new_diagnostics_reminder_message()
        .await
        .expect("a block is injected");
    // The diagnostics attachment renders through the SAME batch wrapper as
    // every other reminder: oracle 2.1.238 @296693609 is
    //   case"diagnostics": … return Zy([kn({content: formatDiagnosticsBlock(n), isMeta:!0})])
    // and `Zy` maps `NT` = `<system-reminder>\n${e}\n</system-reminder>`.
    // `formatDiagnosticsBlock` itself returns the BARE block (@289741641),
    // which is why this test previously asserted the bare form — it was
    // reading the formatter and not the renderer that consumes it.
    assert_eq!(
        msg.text_content(),
        format!("<system-reminder>\n{block}\n</system-reminder>")
    );
}

#[tokio::test]
async fn no_reminder_without_source_or_when_empty() {
    // No source wired (the common no-LSP case).
    assert!(orch_with_diag(None)
        .new_diagnostics_reminder_message()
        .await
        .is_none());
    // Source wired but nothing new.
    assert!(orch_with_diag(Some(Arc::new(MockDiag(None))))
        .new_diagnostics_reminder_message()
        .await
        .is_none());
}
