//! Per-type task handler stubs.
//!
//! Full implementations land in M2. The M1 stubs exist so dependents can
//! compile against the [`crate::Task`] trait surface.

/// Serializes every test in this crate that mutates the process-global
/// `LINGXI_CONFIG_DIR`.
///
/// `local_workflow_test` and `in_process_teammate_test` compile into the SAME
/// test binary and both point that variable at a throwaway dir. They used to
/// hold two SEPARATE mutexes (`ENV_LOCK` and `CLAIM_ENV_LOCK`), which is no
/// mutual exclusion at all: a teammate-claim test could swap the variable out
/// from under `workflow_runs_a_nested_name_from_user_workflows_dir` mid-run,
/// and that test then resolved its nested workflow against the wrong config
/// dir and failed with "could not resolve the nested workflow". Intermittent,
/// and it moved between tests as scheduling changed.
///
/// One lock, one variable. Poisoning is recovered rather than propagated so a
/// single failing test cannot cascade into every other holder.
#[cfg(test)]
pub(crate) static CONFIG_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub mod dream;
pub mod in_process_teammate;
pub mod local_agent;
pub mod local_bash;
pub mod local_fusion;
pub mod local_workflow;
pub mod monitor;
pub mod monitor_mcp;
pub mod remote_agent;

// The M2 handler implementations are re-exported at the module root so
// callers (and the registration helper) can name them without the per-type
// submodule path.
pub use dream::DreamHandler;
pub use in_process_teammate::{
    DefaultTeammateDefinition, InProcessTeammateHandler, TeammateDefinitionResolver,
    TeammateSystemPromptRenderer,
};
pub use local_agent::LocalAgentHandler;
pub use local_bash::{LocalBashHandler, NoopStatusSink, TaskStatusSink};
pub use local_fusion::{escape_xml, fusion_result_xml, LocalFusionHandler};
pub use local_workflow::LocalWorkflowHandler;
pub use monitor::MonitorHandler;
pub use monitor_mcp::MonitorMcpHandler;

mod teammate_plan;
