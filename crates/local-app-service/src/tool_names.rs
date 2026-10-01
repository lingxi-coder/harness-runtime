//! The model-facing names of the Local App host operations.
//!
//! The service owns the operations ([`crate::mcp_server`] answers them); a host
//! that exposes them as tools must call each one by a name the model can use.
//! This table is that naming, kept beside the operations so the text the service
//! writes for the model (error copy, workspace contracts, next-step guidance)
//! and the tools a host registers cannot drift apart: the service's tests check
//! every tool name its prose mentions against this list.

/// `(tool name, host operation, read-only)` for every host operation.
///
/// `read_only` is the operation's own claim that it observes and changes
/// nothing; whether a call may run without asking the person is the host's
/// policy, not this table's.
pub const LOCAL_APP_TOOLS: &[(&str, &str, bool)] = &[
    // Read-only unless noted; the state-journaling entries below are `false`.
    // `LocalAppValidateMcpProposal`, `LocalAppApproveMcpProposal`,
    // `LocalAppQaMcpCandidate` and `LocalAppPromoteMcpCandidate` journal Host
    // state in the MCP-authoring pipeline; `LocalAppEvents` drains a
    // queue (its own note below). The header groups by PIPELINE STAGE, not by
    // the `is_read_only` flag each row carries; read the third column.
    ("LocalAppList", "list", true),
    ("LocalAppGet", "get", true),
    ("LocalAppRuntimeProfiles", "runtime_profiles", true),
    ("LocalAppTemplateCatalog", "template_catalog", true),
    // The plan-driven create/modify step. It is the operation that LANDS a
    // template (create) or stages a new authoring contract (modify), so it is
    // emphatically not read-only — but the bytes it lands are the ones the user
    // approved in the plan, not the ones this call names: `name`, `brief`,
    // `spec` and `template_id` are read from the Host's own approval record.
    ("LocalAppPrepare", "prepare", false),
    ("LocalAppContract", "contract", false),
    (
        "LocalAppValidateMcpProposal",
        "validate_mcp_proposal",
        false,
    ),
    ("LocalAppApproveMcpProposal", "approve_mcp_proposal", false),
    ("LocalAppQaMcpCandidate", "qa_mcp_candidate", false),
    (
        "LocalAppPromoteMcpCandidate",
        "promote_mcp_candidate",
        false,
    ),
    ("LocalAppLogs", "read_logs", true),
    ("LocalAppQueryData", "query_data", true),
    ("LocalAppCheckpointList", "list_checkpoints", true),
    // NOT read-only: `read_app_events` DRAINS the unread queue and advances a
    // persisted cursor unless `peek=true`. `is_read_only` reads the input to
    // honour `peek`; this flag is the DEFAULT for a call that omits it.
    ("LocalAppEvents", "read_app_events", false),
    ("LocalAppBackgroundList", "background_list", true),
    ("LocalAppBackgroundStatus", "background_status", true),
    ("LocalAppInspectUi", "inspect_ui", true),
    // Read-only in the same sense as `inspect_ui`: it observes the view and
    // changes nothing. It is NOT equally cheap for privacy — a pixel capture
    // shows what `inspect_ui` redacts — but that is a PROMPT question, and the
    // prompt default lives in `permission::defaults_per_tool`, not here.
    ("LocalAppCaptureUi", "capture_ui", true),
    // Mutating.
    ("LocalAppBuild", "build", false),
    ("LocalAppQaBegin", "qa_begin", false),
    ("LocalAppQaReadEvidence", "qa_read_evidence", true),
    ("LocalAppQaFinalize", "qa_finalize", false),
    ("LocalAppInstallDeps", "install_dependencies", false),
    // r2-never-wired-02: these two had a full provider catalog entry,
    // description and handler (host's native dependency-review confirmation
    // flow) but no row here, so `AppDependencyChangeConfirmationRequested`
    // had no reachable producer and both clients' dependency-review sheet
    // was dead code.
    (
        "LocalAppConfirmDependencyChange",
        "confirm_dependency_change",
        false,
    ),
    ("LocalAppUpdateDependencies", "update_dependencies", false),
    ("LocalAppRuntime", "manage_runtime", false),
    ("LocalAppCreate", "create", false),
    // Lands the scaffold into an app the "+" button created as an empty SHELL
    // (`AppRecord::scaffolded == false`): it stamps the manifest surface,
    // writes the whole workspace source tree and overwrites the bootstrap
    // `LINGXI.md`. Emphatically NOT read-only — `is_read_only` answers "did
    // this observe without changing anything", and every byte of an app's
    // initial source is written here.
    ("LocalAppScaffold", "scaffold", false),
    ("LocalAppManifest", "update_manifest", false),
    ("LocalAppMutateData", "mutate_data", false),
    ("LocalAppActOnUi", "act_on_ui", false),
    ("LocalAppCheckpointCreate", "create_checkpoint", false),
    ("LocalAppCheckpointRestore", "restore_checkpoint", false),
    ("LocalAppBackgroundSchedule", "background_schedule", false),
    ("LocalAppBackgroundCancel", "background_cancel", false),
    ("LocalAppBackgroundRetry", "background_retry", false),
];
