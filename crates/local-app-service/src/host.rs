//! What the service needs from whoever hosts it.
//!
//! Each trait here is a seam: the service decides *when* something is needed
//! and the host decides *how* it is provided. A host implements the traits it
//! can and hands them to the service when it is built; the service never
//! imports the host.

use async_trait::async_trait;
use local_app_contracts::approvals::{
    AgentProfileProposal, CapabilityRequest, DependencyChangeConfirmationRequest,
    McpProposalApprovalRequest, UiRequest, VerificationSummary,
};
use local_app_contracts::bridge::{BridgeResponse, BridgeStreamFrame};
use local_app_contracts::diagnostics::{DiagnosticsSettleStatus, FileDiagnostics};
use local_app_contracts::events::{ManagedMcpServer, PluginErrorCode, PublicationState};
use local_app_contracts::execution::{CommandOutcome, IsolatedCommand};
use local_apps::{AppErrorCode, AppRecord};
use std::path::Path;
use std::time::Duration;

/// One thing the service reports to its host.
///
/// Every variant is something the host must show, forward or answer; none of
/// them changes service state by being delivered. A host that has no surface
/// for a variant may drop it, except the requests that wait for an answer
/// ([`Self::CapabilityRequested`], [`Self::UiRequest`],
/// [`Self::DependencyChangeConfirmationRequested`],
/// [`Self::McpProposalApprovalRequested`] and [`Self::ProfileProposal`]): the
/// service holds the operation until the person answers or a deadline passes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// The answer to one page bridge request.
    BridgeResponse(BridgeResponse),
    /// One ordered frame of a streamed bridge answer.
    BridgeStreamFrame(BridgeStreamFrame),
    /// An app needs a capability the person has not yet decided on.
    CapabilityRequested(CapabilityRequest),
    /// A permission-gated UI automation action waits for the app's own view.
    UiRequest(UiRequest),
    /// A dependency change needs a reviewed approval before anything is
    /// resolved or installed.
    DependencyChangeConfirmationRequested(DependencyChangeConfirmationRequest),
    /// An MCP proposal needs a reviewed approval before it takes effect.
    McpProposalApprovalRequested(McpProposalApprovalRequest),
    /// An app proposed a new agent profile; it stays inert until approved.
    ProfileProposal(AgentProfileProposal),
    /// An app-initiated model call started (`active`) or finished. Always
    /// emitted in pairs.
    LlmActivityChanged { app_id: String, active: bool },
    /// An app posted one event to its conversation mailbox. The body is not
    /// carried; hosts show a badge and the body is read through the MCP tool.
    AgentEventPosted {
        app_id: String,
        seq: u64,
        topic: String,
        created_at_ms: u64,
    },
    /// A background task changed state. The result is bounded and optional.
    BackgroundTaskChanged {
        app_id: String,
        task_id: String,
        status: String,
        result_json: Option<String>,
        error: Option<String>,
        retryable: bool,
    },
    /// The managed MCP inventory changed.
    ManagedMcpInventoryChanged { servers: Vec<ManagedMcpServer> },
    /// Publication and verification summary of one app changed.
    VerificationSummaryChanged {
        app_id: String,
        publication_state: PublicationState,
        mcp_verification: VerificationSummary,
        ui_verification: VerificationSummary,
    },
    /// A Local App operation failed in a way the host should show: a problem
    /// with the plugin, a catalog or a proposal rather than with the app.
    PluginOperationFailed {
        app_id: Option<String>,
        code: PluginErrorCode,
        message: String,
        request_id: Option<String>,
    },
    /// An operation on the app library failed. `app_id` is `None` when the
    /// failure is that no app came into being; `request_id` echoes the
    /// correlation key of the client request behind it, if any.
    AppOperationFailed {
        app_id: Option<String>,
        code: AppErrorCode,
        message: String,
        request_id: Option<String>,
    },
}

/// Where the service sends what its host must show, forward or answer.
///
/// Implementations should be cheap and must not block: the service calls
/// [`emit`](Self::emit) from the middle of an operation, often while it holds
/// the operation's own locks, so a sink that waits on a person or on a slow
/// transport stalls the service. Enqueue and return.
#[async_trait]
pub trait HostEventSink: Send + Sync {
    /// Deliver one event. Events are delivered in the order they are emitted.
    async fn emit(&self, event: HostEvent);
}

/// Source diagnostics for an app's workspace, from whatever language tooling
/// the host runs.
///
/// A host without any simply does not provide one, and the service builds
/// without the check; a provider that cannot answer says so with `None` or an
/// empty list. Neither blocks a build, so an unavailable tool never stops one.
#[async_trait]
pub trait DiagnosticsProvider: Send + Sync {
    /// Wait up to `timeout` for the diagnostics under `workspace` to catch up
    /// with the files they describe. `None` when the provider tracks nothing
    /// for that workspace.
    async fn settle(&self, workspace: &Path, timeout: Duration) -> Option<DiagnosticsSettleStatus>;

    /// The diagnostics currently held for the files under `workspace`.
    async fn latest(&self, workspace: &Path) -> Vec<FileDiagnostics>;
}

/// The conversation an app's creation happens in, as the host keeps it.
#[async_trait]
pub trait ConversationHost: Send + Sync {
    /// A scaffold committed: the app is formed and its record carries its name,
    /// so the conversation opened for it should show that name instead of a
    /// placeholder. `Ok(true)` when a rename was made, `Ok(false)` when nothing
    /// needed to change.
    ///
    /// A failure is cosmetic and the service does not undo the scaffold for it:
    /// the host repairs a title it could not write on its own schedule.
    async fn app_scaffolded(&self, record: &AppRecord) -> Result<bool, String>;
}

/// Where the service runs the commands a build needs: the dependency install
/// and the app's own build.
///
/// The service says what must be isolated and how; the host provides the
/// isolation and reports what it actually enforced, which the service checks
/// ([`local_app_contracts::execution::Enforcement::ensure_for`]) before it
/// trusts the result. A host without an executor cannot build, and the service
/// says so rather than running anything unisolated.
#[async_trait]
pub trait BuildExecutor: Send + Sync {
    /// Run one command to completion. `Err` is a failure to run it at all,
    /// worded for the person; a command that ran and failed is an `Ok` outcome
    /// with its exit code.
    async fn run(&self, command: IsolatedCommand) -> Result<CommandOutcome, String>;
}
