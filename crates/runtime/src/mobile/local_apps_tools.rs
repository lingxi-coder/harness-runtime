//! First-party local-app host operations, registered as ORDINARY BUILTIN tools.
//!
//! These were previously reachable only as `mcp__local_apps__<op>`. That name
//! shape carried third-party-MCP permission semantics they were never meant to
//! have:
//!
//! - `permission::tool_default` is a flat table keyed by literal tool name with
//!   `.unwrap_or(DenyByDefault)`. No `mcp__…` name is in it, so EVERY call —
//!   `build`, `read_logs`, `manage_runtime` — raised a permission prompt. In
//!   the create flow that is a prompt per step.
//! - MCP rule matching is tool-wide: content matching is gated on Deny/Ask, so
//!   no allow rule could ever scope one of these to a single app. A grant
//!   written into one app's workspace settings meant "any app on this device,
//!   forever".
//!
//! They are first-party host operations that take `app_id` as a parameter —
//! the shape of a builtin tool, not of a user-configured MCP server. As
//! builtins they get ordinary permission semantics: a real entry in the
//! defaults table, and content matching, so `LocalAppBuild(<app_id>)` becomes
//! expressible.
//!
//! ## What deliberately stays on the MCP transport
//!
//! The DYNAMIC per-app tools (`mcp__local_apps__<app_id>__data_query`, …) are
//! NOT moved, for two independent reasons:
//!
//! 1. Their namespace is load-bearing: the provider binds `app_id` host-side
//!    and REJECTS a model-supplied one ("app_id is host-bound by the app MCP
//!    namespace"). That is the correct scoping mechanism and it already works.
//! 2. They must be registered at RUNTIME, when an app is created mid-session.
//!    `ToolRegistry::register_builtin` takes `&mut self` (construction only);
//!    only `register_mcp_tools(&self, …)` can add tools to a shared registry.

use std::sync::Arc;

use permission::result::{PermissionMetadata, PermissionPrompt};
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::Value;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext,
};

use crate::mobile::local_apps_mcp::LocalAppsMcpTransport;

/// `(builtin name, provider operation, read-only)` for every host operation.
///
/// `read_only` drives [`Tool::is_read_only`]/[`Tool::is_concurrency_safe`]; the
/// PROMPT default for each name lives in `permission::defaults_per_tool` — one
/// table for every tool in the product, rather than a second policy here.
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

/// One host operation exposed as a builtin tool.
///
/// The catalog entry (description + input schema) and the dispatch both come
/// from [`LocalAppsMcpTransport`], so a builtin and the provider can never
/// disagree about a schema or an argument.
pub struct LocalAppTool {
    /// Model-facing name, e.g. `LocalAppBuild`.
    name: &'static str,
    /// Provider operation this dispatches to, e.g. `build`.
    operation: &'static str,
    description: String,
    input_schema: Value,
    read_only: bool,
    /// The local app this SESSION is rooted in, resolved once at construction
    /// from the engine cwd.
    ///
    /// NOT from `ToolUseContext::cwd`: the main turn loop hard-codes that to
    /// `None` (`turn_loop.rs:2866` — "only an isolated subagent sets this"), so
    /// a call-time binding is inert in exactly the session that needs it. The
    /// engine cwd IS the app workspace for a `.localApp` conversation.
    session_app_id: Option<String>,
    /// Catalog `always_load == Some(false)` → this tool is DEFERRED, i.e. the
    /// model finds it through ToolSearch instead of carrying its description
    /// and schema in every request.
    defer: bool,
    search_hint: Option<String>,
    transport: Arc<LocalAppsMcpTransport>,
}

impl LocalAppTool {
    #[must_use]
    pub fn new(
        name: &'static str,
        operation: &'static str,
        description: String,
        input_schema: Value,
        read_only: bool,
        session_app_id: Option<String>,
        defer: bool,
        search_hint: Option<String>,
        transport: Arc<LocalAppsMcpTransport>,
    ) -> Self {
        Self {
            name,
            operation,
            description,
            input_schema,
            read_only,
            session_app_id,
            defer,
            search_hint,
            transport,
        }
    }
}

/// Build every host operation as a builtin tool, taking each description and
/// input schema from the provider's own catalog.
#[must_use]
pub fn local_app_builtin_tools(
    transport: &Arc<LocalAppsMcpTransport>,
    session_app_id: Option<String>,
) -> Vec<Arc<dyn Tool>> {
    // The composition root passes the exact app id it resolved from the
    // canonical, data-root-bounded workspace. Do not re-derive it from a raw
    // cwd here: a symlink alias can otherwise make the Host grant the full
    // inventory while these tools bind no app (or bind the alias's wrong id).
    // Ordinary Code conversations outside a Local App workspace only need
    // the three global management operations.  The full host surface is
    // registered for an app workspace, where its id comes from the
    // host-owned LINGXI.md binding and the tool permission refinement can
    // scope calls to that app. Chat's existing session allowlist remains the
    // policy boundary; this function is used by the Code/mobile host path.
    const GLOBAL_LOCAL_APP_TOOLS: &[&str] = &[
        "LocalAppCreate",
        "LocalAppList",
        "LocalAppGet",
        "LocalAppPrepare",
    ];
    let global_session = session_app_id.is_none();
    let catalog = LocalAppsMcpTransport::host_tool_catalog();
    LOCAL_APP_TOOLS
        .iter()
        .filter_map(|&(name, operation, read_only)| {
            if global_session && !GLOBAL_LOCAL_APP_TOOLS.contains(&name) {
                return None;
            }
            let entry = catalog.iter().find(|tool| tool.tool_name == operation)?;
            Some(Arc::new(LocalAppTool::new(
                name,
                operation,
                entry.description.clone(),
                entry.input_schema.clone(),
                read_only,
                session_app_id.clone(),
                // Carry the catalog's OWN loading decision. Dropping it made
                // every one of these ride eagerly in every mobile conversation
                // — including ones with no local app — and silently inverted
                // `LocalAppList`'s explicit "keep catalog discovery out of the
                // eager tool set".
                entry.always_load == Some(false),
                entry.search_hint.clone(),
                Arc::clone(transport),
            )) as Arc<dyn Tool>)
        })
        .collect()
}

impl LocalAppTool {
    /// The local app this session is rooted in, if any.
    fn bound_app_id(&self, _ctx: &ToolUseContext) -> Option<String> {
        // A call-time cwd is not an authority source. Isolated agents inherit
        // the session's Host-resolved binding; a crafted A→B path or symlink
        // must never retarget an already-registered builtin.
        self.session_app_id.clone()
    }

    fn requires_bound_session_for_auto_allow(&self) -> bool {
        matches!(
            self.name,
            "LocalAppGet"
                | "LocalAppLogs"
                | "LocalAppCheckpointList"
                | "LocalAppBackgroundList"
                | "LocalAppBackgroundStatus"
                | "LocalAppBuild"
                | "LocalAppRuntime"
                // Allow-by-default so the shell conversation's ONE way out
                // needs no prompt — but only inside the shell's own workspace.
                // A builtin registers for EVERY session, so a global chat can
                // see `LocalAppScaffold` and name any app id; without this row
                // it could commit a name, a brief and an IMMUTABLE surface onto
                // a stranger's app with no prompt at all.
                | "LocalAppScaffold"
                // The plan-driven create/modify step, and the same hazard as
                // `LocalAppScaffold`: its default row is `AllowByDefault`
                // because the approval of the PLAN is the create confirmation,
                // and `defaults_per_tool`'s comment promises this refinement
                // ("In a session NOT bound to an app workspace
                // `LocalAppTool::check_permissions` still refines this to Ask,
                // which is the create case — a global chat naming an app id").
                // Without this row a global chat can name ANY app id and land
                // the approved plan's template on it with no prompt at all.
                | "LocalAppPrepare"
        )
    }
}

#[async_trait::async_trait]
impl Tool for LocalAppTool {
    fn name(&self) -> &str {
        self.name
    }

    fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }

    /// NOT `is_mcp`. These are builtins now; reporting MCP here would put them
    /// back on the tool-wide-only matching path this move exists to leave.
    fn is_mcp(&self) -> bool {
        false
    }

    fn should_defer(&self) -> bool {
        self.defer
    }

    fn search_hint(&self) -> Option<&str> {
        self.search_hint.as_deref()
    }

    fn max_result_size_chars(&self) -> usize {
        30_000
    }

    /// Same value the MCP adapter these replaced used — a large result is
    /// persisted rather than inlined.
    fn persistence_threshold(&self) -> Option<usize> {
        Some(100_000)
    }

    /// A long `LocalAppBuild` (30-minute budget) or `LocalAppInstallDeps` must
    /// be interruptible. The trait default is `Block`, under which the turn
    /// loop does NOT race the cancel token — ESC would leave the session
    /// parked until the budget expired. The MCP adapter these replaced
    /// returned `Cancel`; keep that.
    ///
    /// `create` and `scaffold` are the exception, and it is not a preference.
    ///
    /// On `Cancel` the turn loop races the token and DROPS the tool future.
    /// For `create`, dropping it does NOT undo it: `AppService::create_app*`
    /// runs its mint/persist/commit on a DETACHED `tokio::spawn` precisely so
    /// a dropped caller cannot leave a half-created app, so the record still
    /// lands while the model is told the call was aborted. The model's only
    /// recovery from "aborted" is to call create again — and create is not
    /// idempotent, so one interrupted intent becomes two apps in the library.
    ///
    /// `scaffold` (`LocalAppScaffold`) is worse under `Cancel`: by the time it
    /// is running, the workspace has already been wiped and reseeded and the
    /// FORMAL create contract has been written to disk — dropping its future
    /// mid-transaction abandons that seeded workspace behind a
    /// `scaffolded == false` record AND skips its own Err-path guards
    /// (`recovery.rollback()` and the dependency-record restore), neither of
    /// which runs when the future is dropped instead of awaited to
    /// completion. (The receipt claim is the one piece that IS still released
    /// on a drop, by `ReceiptClaim`'s `Drop` in `local_apps_host.rs` — the
    /// seeded workspace and the dependency record have no such guard.)
    /// `Block` routes the call through the
    /// `_ => tool_call.await` arm instead, so the transaction always reaches
    /// its own commit-or-rollback.
    ///
    /// Blocking costs a bounded wait for both — `create` is a few file writes
    /// plus a scaffold, and `scaffold` is a wipe plus a template copy plus a
    /// dependency install whose own ceiling is `DEPENDENCY_INSTALL_TIMEOUT`
    /// (ten minutes), not an open-ended build — and buys the guarantee
    /// that the model always learns the outcome of the mutation it just
    /// caused, instead of leaving durable state an engine restart must repair.
    fn interrupt_behavior(&self, _input: &Value) -> InterruptBehavior {
        if self.operation == "create" || self.operation == "scaffold" {
            InterruptBehavior::Block
        } else {
            InterruptBehavior::Cancel
        }
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        // TRUE for all of them, matching the MCP adapter these replaced.
        //
        // This answers "may this overlap other tools?", NOT "is this a
        // mutation". A single app's build/runtime mutations are already
        // serialized inside the service, so reporting the mutating ones unsafe
        // buys no safety and turns each into a full execution BARRIER: a turn
        // emitting [LocalAppBuild, Read, LocalAppLogs] would stall both cheap
        // reads behind the build's 30-minute budget.
        true
    }

    /// Input-aware, like `ConfigTool`/`ArtifactTool`: a per-tool flag cannot
    /// express `read_app_events`, which only leaves the cursor alone when the
    /// caller passes `peek=true`.
    fn is_read_only(&self, input: &Value) -> bool {
        if self.operation == "read_app_events" {
            return input.get("peek").and_then(Value::as_bool).unwrap_or(false);
        }
        self.read_only
    }

    /// The tool has NO opinion — the policy layer decides from the defaults
    /// table plus any matching rule. Same "allow-all-gate" convention the
    /// native file tools use; the real gate is `PolicyPermissionGate`.
    async fn check_permissions(&self, input: &Value, ctx: &ToolUseContext) -> PermissionResult {
        if self.bound_app_id(ctx).is_none() && self.requires_bound_session_for_auto_allow() {
            let target = input
                .get("app_id")
                .and_then(Value::as_str)
                .map(|app_id| format!(" app `{app_id}`"))
                .unwrap_or_else(|| " local apps outside an app-bound workspace".to_string());
            return PermissionResult::Ask {
                reason: PermissionDecisionReason::Other {
                    reason: "global local-app sessions must confirm host operations".into(),
                },
                prompt: PermissionPrompt {
                    title: format!("Allow {} here?", self.name),
                    message: format!(
                        "This conversation is not bound to a single local app. \
                         Confirm before {} accesses{}.",
                        self.name, target
                    ),
                    options: vec!["Deny".into(), "Allow once".into(), "Always allow".into()],
                },
                pending_classifier_check: None,
                metadata: PermissionMetadata::default(),
            };
        }
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "allow-all-gate (M4-01 default)".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _input: &Value, _opts: &DescriptionOptions) -> String {
        self.description.clone()
    }

    async fn prompt(&self, _opts: &PromptOptions) -> String {
        self.description.clone()
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        // A `LocalAppBuild` runs on a 30-minute budget. Without a `started`
        // frame the row renders frozen for the whole of it — the MCP adapter
        // these replaced emitted one, so dropping it was a visible regression.
        // Best-effort `try_send`, like that adapter.
        if let Some(tool_use_id) = _ctx.tool_use_id.clone() {
            let _ = progress_tx.try_send(tool_api::progress::ToolProgress {
                tool_use_id,
                data: serde_json::json!({
                    "type": "local_app_progress",
                    "status": "started",
                    "tool": self.name,
                    "operation": self.operation,
                }),
            });
        }
        // HOST-BIND `app_id` to the session's own app.
        //
        // These tools are AUTO-ALLOWED by the defaults table, and the static
        // dispatch takes `app_id` from model input verbatim. Without this, a
        // session working on app A could read app B's records, logs, events or
        // live WebView with no prompt at all — strictly worse than the
        // `mcp__local_apps__*` spelling, which at least asked. The dynamic MCP
        // tools solve this by binding the id to their namespace; a builtin has
        // no namespace, so it binds from the session cwd instead.
        //
        // Only binds when the session IS a local-app workspace. A global
        // conversation legitimately addresses apps by id (that is how one gets
        // created), and there the ordinary permission gate is the control.
        let input = match self.bound_app_id(&_ctx) {
            None => input,
            Some(session_app) => match input.get("app_id") {
                // Absent → fill it in, so a workspace session never has to name
                // its own app.
                None => {
                    let mut bound = input;
                    if let Some(object) = bound.as_object_mut() {
                        object.insert("app_id".into(), Value::String(session_app));
                    }
                    bound
                }
                Some(Value::String(requested)) if *requested == session_app => input,
                // A mismatch OR a non-string (`null`, a number, an array) fails
                // CLOSED. Letting a non-string through would slip past both the
                // comparison and the fill-in and reach the provider unbound.
                other => {
                    return Err(ToolError::InvalidInput(format!(
                        "{}: app_id {other:?} does not belong to this workspace \
                         (bound to {session_app:?})",
                        self.name
                    )));
                }
            },
        };
        let result = self
            .transport
            .call_host_operation(self.operation, input)
            .await
            .map_err(|error| ToolError::Internal(error.to_string()))?;
        Ok(envelope_to_tool_result(result))
    }
}

/// Remove fields that could carry image bytes before metadata is copied into a
/// model-facing summary.  The actual primary image remains in `file.base64`
/// for the live media projection; this helper only protects the duplicate
/// metadata/summary path.  Local App QA metadata is Host-shaped and small, but
/// treating the envelope as untrusted keeps a malformed evidence response from
/// smuggling a second pixel payload into session text.
fn safe_ephemeral_metadata(value: &Value) -> Value {
    const MAX_STRING_CHARS: usize = 4096;
    const MAX_ARRAY_ITEMS: usize = 256;
    const MAX_ID_LIST_ITEMS: usize = 4096;
    const MAX_OBJECT_FIELDS: usize = 128;

    fn is_pixel_field(key: &str) -> bool {
        matches!(
            key.to_ascii_lowercase().as_str(),
            "data" | "base64" | "bytes" | "pixels" | "image_data" | "additional_images"
        )
    }

    fn is_identity_list_field(key: &str) -> bool {
        matches!(
            key,
            "qa_evidence_ids"
                | "evidence_ids"
                | "resolved_by_evidence_ids"
                | "declared_target_ids"
                | "in_scope_target_ids"
                | "unverified_target_ids"
                | "unverified_scenario_ids"
        )
    }

    fn sanitize(value: &Value, array_limit: usize) -> Value {
        match value {
            Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
            Value::String(text) => {
                if text.chars().count() <= MAX_STRING_CHARS {
                    value.clone()
                } else {
                    Value::String(text.chars().take(MAX_STRING_CHARS).collect())
                }
            }
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .take(array_limit)
                    .map(|item| sanitize(item, MAX_ARRAY_ITEMS))
                    .collect(),
            ),
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .filter(|(key, _)| !is_pixel_field(key))
                    .take(MAX_OBJECT_FIELDS)
                    .map(|(key, value)| {
                        let limit = if is_identity_list_field(key) {
                            MAX_ID_LIST_ITEMS
                        } else {
                            MAX_ARRAY_ITEMS
                        };
                        (key.clone(), sanitize(value, limit))
                    })
                    .collect(),
            ),
        }
    }

    sanitize(value, MAX_ARRAY_ITEMS)
}

fn ephemeral_image_summary(metadata: &Value, image_count: usize) -> String {
    let details = serde_json::json!({
        "image_count": image_count,
        "metadata": metadata,
    });
    format!(
        "Temporary local-app view capture attached; pixels are not persisted. QA metadata: {}",
        serde_json::to_string(&details).unwrap_or_else(|_| "{}".into())
    )
}

/// Turn the provider's MCP envelope into a builtin tool result.
///
/// Extracted so it can be tested WITHOUT a live transport and host. It was
/// inline and therefore untestable, which is how it came to silently discard
/// image content: the unit test that existed covered the MCP transport (the
/// layer that BUILDS the image block) and not this one (the layer the agent
/// actually goes through, which threw it away).
fn envelope_to_tool_result(result: lingxi_core::host::McpToolResultDto) -> ToolCallResult {
    // The provider builds an MCP ENVELOPE: `content` is
    // `[{"type":"text","text":"<json>"}]` and `structured_content` carries
    // the real payload. Handing the envelope to `from_data` would make the
    // dispatch JSON-dump the array, so the model would receive
    // `[{"type":"text","text":"{\"records\":…}"}]` — a doubly-escaped
    // string — instead of `{"records":…}`. Every prompt that says "the
    // returned `records[].document` must contain the value" would then be
    // asking the model to read through two layers of escaping.
    //
    // Unwrap it: the structured payload is the data, and the envelope's
    // text is the model-facing rendering.
    let text = result
        .content
        .as_array()
        .and_then(|blocks| {
            let joined: Vec<&str> = blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect();
            (!joined.is_empty()).then(|| joined.join("\n"))
        })
        .or_else(|| result.content.as_str().map(str::to_owned));
    let data = result
        .structured_content
        .clone()
        .or_else(|| text.clone().map(Value::String))
        .unwrap_or(Value::Null);
    // A capture carries an IMAGE content block, and the unwrap above keeps
    // only `text` blocks — so handing `from_data` the structured metadata
    // alone gives the model the viewport and SILENTLY DROPS THE FRAME. That
    // is the exact failure `capture_ui` exists to prevent, and it hides
    // well: the metadata still reads as a successful result. Observed on
    // device before this branch existed — the agent got
    // `{"viewport":{...},"ok":true,"action":"capture_view"}` and no picture,
    // three times, and reported the capture as having worked.
    //
    // Shape follows `android_use`'s screenshot result rather than inventing
    // one. `_lingxi_ephemeral` is load-bearing: `conversation.rs` uses it to
    // keep the pixels out of session persistence, and a QA loop that
    // captures every round would otherwise grow the transcript by a JPEG a
    // turn, forever.
    //
    // Keyed on a NON-EMPTY `data` string, not merely on `type == "image"`: an
    // empty payload would still satisfy `image_tool_result_blocks` and go out
    // as `source.data: ""`, which the provider rejects with a 400 for the WHOLE
    // request rather than for this one tool call. The producer already decided
    // that case is an error (`local_apps_mcp.rs` answers `tool_error` for an
    // empty frame); falling through to the text path here agrees with it
    // instead of manufacturing a success.
    let image_blocks: Vec<(String, String)> = result
        .content
        .as_array()
        .into_iter()
        .flat_map(|blocks| blocks.iter())
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("image"))
        .filter_map(|block| {
            let data = block.get("data").and_then(Value::as_str)?;
            (!data.is_empty()).then(|| {
                (
                    data.to_string(),
                    block
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("image/jpeg")
                        .to_string(),
                )
            })
        })
        .collect();
    if !image_blocks.is_empty() {
        // Keep the established `{type:image,file:{base64,type}}` shape so the
        // shared turn egress emits a real image content block. The generic
        // media projection intentionally emits one primary frame; retain only
        // a safe count for any additional frames rather than duplicating their
        // base64 in metadata or model text.
        let (base64, media_type) = &image_blocks[0];
        let metadata =
            safe_ephemeral_metadata(&result.structured_content.clone().unwrap_or(Value::Null));
        let summary = ephemeral_image_summary(&metadata, image_blocks.len());
        let mut out = ToolCallResult::from_data(serde_json::json!({
            "type": "image",
            "file": { "base64": base64, "type": media_type },
            "image_count": image_blocks.len(),
            "metadata": metadata,
            "_lingxi_ephemeral": true,
            "summary": summary,
        }));
        // NOT `text`: for a capture envelope `content` is image-only, so `text`
        // is None and `tool_result_to_model_text` would fall through to a JSON
        // dump of `data` — putting the ~230 KB of base64 into the tool_result
        // STRING that lives in session history and rides the client event sink
        // for the rest of the session. The pixels already travel as a real
        // image block (`image_tool_result_blocks`), so the string only needs
        // the marker the session sanitizer reads plus the replacement copy.
        // `android_use::screenshot_result` sets exactly this, for exactly this
        // reason; keeping it JSON is load-bearing, because
        // `redact_ephemeral_tool_result_images` finds `_lingxi_ephemeral` by
        // PARSING this string.
        out.model_content = Some(
            serde_json::json!({
                "_lingxi_ephemeral": true,
                "summary": out.data["summary"].clone(),
            })
            .to_string(),
        );
        out.is_error = result.is_error;
        return out;
    }
    let mut out = ToolCallResult::from_data(data);
    out.model_content = text;
    // Mirror the MCP contract: a logical failure is a flagged RESULT, not a
    // transport error, so the model sees it 1:1 with the former spelling.
    out.is_error = result.is_error;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A captured frame must survive the envelope unwrap.
    ///
    /// REGRESSION: it did not. The unwrap kept only `text` blocks, so the model
    /// received `{"viewport":{...},"ok":true,"action":"capture_view"}` and no
    /// picture — and reported the capture as successful, because the metadata
    /// alone still looks like one. Observed three times on device before this
    /// was found.
    ///
    /// The MCP transport's own test covered the layer that BUILDS the image
    /// block and passed throughout; this is the layer the agent actually goes
    /// through, and it had no test at all.
    #[test]
    fn a_captured_frame_survives_the_envelope_unwrap() {
        const DATA: &str = "/9j/4AAQSkZJRgABAQAAAQ==";
        let out = envelope_to_tool_result(lingxi_core::host::McpToolResultDto {
            result_projection: None,

            content: serde_json::json!([
                { "type": "image", "data": DATA, "mimeType": "image/jpeg" }
            ]),
            is_error: false,
            structured_content: Some(serde_json::json!({
                "ok": true,
                "action": "capture_view",
                "viewport": { "width": 414, "height": 804 },
            })),
            ..Default::default()
        });

        assert_eq!(out.data["type"], "image", "the frame must ride as an image");
        assert_eq!(out.data["file"]["base64"], DATA);
        assert_eq!(out.data["file"]["type"], "image/jpeg");
        assert_eq!(
            out.data["_lingxi_ephemeral"], true,
            "pixels must stay out of session persistence, or a QA loop that \
             captures every round grows the transcript by a JPEG a turn"
        );
        // The viewport is what makes the frame readable as evidence — the same
        // app is a different layout on a tablet, and pixels do not say which.
        assert_eq!(out.data["metadata"]["viewport"]["width"], 414);
        // The model-facing STRING must not be the base64. It is what lands in
        // session history and in the client's tool-result event, and the
        // sanitizer reads `_lingxi_ephemeral` out of it by parsing it as JSON —
        // so it has to stay JSON AND stay small.
        let model_content = out
            .model_content
            .as_deref()
            .expect("an image result must carry a model-facing marker string");
        assert!(
            !model_content.contains(DATA),
            "the base64 must not ride in the tool_result text: {model_content}"
        );
        let parsed: Value =
            serde_json::from_str(model_content).expect("the sanitizer parses this as JSON");
        assert_eq!(parsed["_lingxi_ephemeral"], true);
    }

    /// QA metadata must survive the two projections that agents actually read:
    /// the shared media projection used by the subagent runner and the
    /// `model_content` string used by the main turn.  Checking only
    /// `ToolCallResult.data["metadata"]` misses both regressions because the
    /// runner deliberately replaces that object with the ephemeral summary.
    #[test]
    fn qa_image_projection_keeps_identity_dimensions_and_all_frame_count_without_pixels() {
        const FRAME_A: &str = "/9j/4AAQSkZJRgABAQAAAQ==";
        const FRAME_B: &str = "iVBORw0KGgoAAAANSUhEUg==";
        let out = envelope_to_tool_result(lingxi_core::host::McpToolResultDto {
            result_projection: None,

            content: serde_json::json!([
                { "type": "image", "data": FRAME_A, "mimeType": "image/jpeg" },
                { "type": "image", "data": FRAME_B, "mimeType": "image/png" }
            ]),
            is_error: false,
            structured_content: Some(serde_json::json!({
                "ok": true,
                "qa_handle": "qa_0123456789abcdef0123456789abcdef",
                "qa_evidence_ids": ["qa-evidence-1", "qa-evidence-2"],
                "evidence": {
                    "evidence_id": "qa-evidence-2",
                    "scenario_id": "primary-action",
                    "target_id": "iphone",
                    "kind": "capture",
                    "source": "host",
                    "artifact": { "artifact_id": "artifact-2", "format": "png", "byte_len": 42 }
                },
                "image_width": 834,
                "image_height": 1194,
                // A producer bug must not be able to smuggle a second copy of
                // pixels through the metadata side channel.
                "data": FRAME_A,
            })),
            ..Default::default()
        });

        let projected = tool_api::tool_result_media::media_content_blocks(&out.data)
            .expect("the actual subagent media projection must see the image");
        assert_eq!(
            projected.len(),
            1,
            "the established projection emits the primary frame once"
        );
        assert_eq!(projected[0]["source"]["data"], FRAME_A);
        let summary = tool_api::tool_result_media::ephemeral_summary(&out.data)
            .expect("the subagent runner must use the compact summary");
        assert!(summary.contains("qa_0123456789abcdef0123456789abcdef"));
        assert!(summary.contains("qa-evidence-1"));
        assert!(summary.contains("qa-evidence-2"));
        assert!(summary.contains("primary-action"));
        assert!(summary.contains("image_width"));
        assert!(summary.contains("image_height"));
        assert!(summary.contains("image_count"));
        assert!(!summary.contains(FRAME_A));
        assert!(!summary.contains(FRAME_B));

        // The main model receives the model_content string, not `data`.
        let model_content = out.model_content.as_deref().expect("model-facing marker");
        assert!(model_content.contains("qa-evidence-2"));
        assert!(model_content.contains("primary-action"));
        assert!(!model_content.contains(FRAME_A));
        assert!(!model_content.contains(FRAME_B));
        let marker: Value = serde_json::from_str(model_content).expect("sanitizer marker JSON");
        assert_eq!(marker["_lingxi_ephemeral"], true);
        assert!(marker["summary"]
            .as_str()
            .is_some_and(|text| text.contains("artifact-2")));

        // The durable sanitizer uses the same marker and keeps only summary
        // text.  This mirrors its behavior without importing the orchestrator
        // crate into the mobile tool unit tests.
        let persisted = marker["summary"].as_str().expect("summary text");
        assert!(!persisted.contains(FRAME_A));
        assert!(!persisted.contains(FRAME_B));
        assert!(persisted.contains("qa-evidence-2"));
    }

    #[test]
    fn qa_read_evidence_image_projection_retains_evidence_identity() {
        const DATA: &str = "RkFLRS1GUkFNRQ==";
        let out = envelope_to_tool_result(lingxi_core::host::McpToolResultDto {
            result_projection: None,

            content: serde_json::json!([
                { "type": "image", "data": DATA, "mimeType": "image/webp" }
            ]),
            is_error: false,
            structured_content: Some(serde_json::json!({
                "evidence": {
                    "evidence_id": "ev-capture-9",
                    "scenario_id": "checkout",
                    "target_id": "ipad",
                    "kind": "capture",
                    "source": "host",
                    "event_id": "event-9",
                    "artifact": { "artifact_id": "artifact-9", "format": "webp" }
                },
                "qa_handle": "qa_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "qa_evidence_ids": ["ev-capture-9"]
            })),
            ..Default::default()
        });
        let projected = tool_api::tool_result_media::media_content_blocks(&out.data)
            .expect("QaReadEvidence capture must remain an actual image");
        assert_eq!(projected[0]["source"]["media_type"], "image/webp");
        let summary = tool_api::tool_result_media::ephemeral_summary(&out.data).unwrap();
        for identity in ["ev-capture-9", "checkout", "ipad", "event-9", "artifact-9"] {
            assert!(
                summary.contains(identity),
                "metadata lost {identity}: {summary}"
            );
        }
        assert!(!summary.contains(DATA));
    }

    /// An image block with no payload must NOT be dressed up as a success.
    ///
    /// `image_tool_result_blocks` only checks that `base64` is a string, so an
    /// empty one goes out as `source.data: ""` and the provider rejects the
    /// WHOLE request with a 400 — not just this tool call. The producer already
    /// answers `tool_error` for an empty frame; this layer has to agree.
    #[test]
    fn an_empty_frame_falls_through_to_the_text_path() {
        let out = envelope_to_tool_result(lingxi_core::host::McpToolResultDto {
            result_projection: None,

            content: serde_json::json!([{ "type": "image", "data": "", "mimeType": "image/jpeg" }]),
            is_error: true,
            structured_content: Some(serde_json::json!({ "ok": false })),
            ..Default::default()
        });
        assert!(
            out.data.get("file").is_none(),
            "an empty payload must not become an image block: {:?}",
            out.data
        );
        assert!(out.is_error, "the producer's error flag must survive");
    }

    /// The image branch must not disturb an ordinary result.
    ///
    /// Every other local-app tool returns a text envelope, and they are the
    /// overwhelming majority of calls; a capture-shaped change that altered
    /// them would be a far larger regression than the one it fixes.
    #[test]
    fn a_text_envelope_is_unwrapped_unchanged() {
        let out = envelope_to_tool_result(lingxi_core::host::McpToolResultDto {
            result_projection: None,

            content: serde_json::json!([{ "type": "text", "text": "{\"records\":[]}" }]),
            is_error: false,
            structured_content: Some(serde_json::json!({ "records": [] })),
            ..Default::default()
        });
        assert!(out.data.get("type").is_none(), "not an image result");
        assert_eq!(out.data["records"], serde_json::json!([]));
        assert_eq!(out.model_content.as_deref(), Some("{\"records\":[]}"));
    }

    /// `LocalAppScaffold` must `Block` on interrupt, exactly like `create`.
    ///
    /// REGRESSION: it used to fall into the `Cancel` default, so a Stop mid-
    /// scaffold dropped the transaction's future AFTER the workspace was
    /// wiped and reseeded and the formal create contract was written to
    /// disk — abandoning that seeded workspace behind a `scaffolded == false`
    /// record, with its Err-path guards (rollback, dependency restore)
    /// skipped because the future was dropped instead of run to completion.
    /// (The receipt claim is separately covered on a drop by `ReceiptClaim`'s
    /// `Drop`; these two are not.) Only an engine restart repaired it.
    #[test]
    fn scaffold_blocks_on_interrupt_like_create() {
        let transport = Arc::new(LocalAppsMcpTransport::new(std::path::PathBuf::from("/tmp")));
        let scaffold = LocalAppTool::new(
            "LocalAppScaffold",
            "scaffold",
            String::new(),
            serde_json::json!({}),
            false,
            None,
            false,
            None,
            Arc::clone(&transport),
        );
        assert!(
            matches!(
                scaffold.interrupt_behavior(&serde_json::json!({})),
                InterruptBehavior::Block
            ),
            "LocalAppScaffold must report InterruptBehavior::Block, not the \
             Cancel default, or a mid-scaffold Stop drops the tool future \
             after the workspace and formal contract are already on disk"
        );
        // The sibling that must NOT have flipped: an ordinary read has no
        // transaction to protect and should stay cheaply cancellable.
        let list = LocalAppTool::new(
            "LocalAppList",
            "list",
            String::new(),
            serde_json::json!({}),
            true,
            None,
            false,
            None,
            transport,
        );
        assert!(matches!(
            list.interrupt_behavior(&serde_json::json!({})),
            InterruptBehavior::Cancel
        ));
    }

    /// Every entry in the rename table must resolve to a real provider
    /// operation. A typo here would silently drop a tool: `local_app_builtin_tools`
    /// uses `filter_map`, so an unmatched operation yields FEWER tools rather
    /// than an error.
    #[test]
    fn every_declared_tool_maps_to_a_real_provider_operation() {
        let catalog = LocalAppsMcpTransport::host_tool_catalog();
        let ops: std::collections::BTreeSet<&str> = catalog.iter().map(|t| t.tool_name()).collect();
        for &(name, operation, _) in LOCAL_APP_TOOLS {
            assert!(
                ops.contains(operation),
                "{name} maps to unknown provider operation {operation:?}; \
                 known: {ops:?}"
            );
        }
    }

    /// r2-never-wired-02: `confirm_dependency_change`/`update_dependencies`
    /// had a full provider catalog entry, description and handler but no row
    /// in `LOCAL_APP_TOOLS`, so `AppDependencyChangeConfirmationRequested`
    /// had no reachable producer and both clients' dependency-review sheet
    /// was dead code.
    #[test]
    fn dependency_review_operations_are_wired_as_builtin_tools() {
        let names: std::collections::BTreeSet<&str> =
            LOCAL_APP_TOOLS.iter().map(|&(name, _, _)| name).collect();
        assert!(
            names.contains("LocalAppConfirmDependencyChange"),
            "confirm_dependency_change has a full provider catalog entry and \
             handler but no builtin tool row, so \
             AppDependencyChangeConfirmationRequested can never be emitted by \
             a model-driven call"
        );
        assert!(
            names.contains("LocalAppUpdateDependencies"),
            "update_dependencies has a full provider catalog entry and \
             handler but no builtin tool row, so a confirmed dependency \
             change receipt can never be applied"
        );
    }

    /// The builder must produce one tool per declared entry. `filter_map`
    /// turns a bad mapping into a MISSING tool, which no other assertion here
    /// would notice.
    #[test]
    fn the_builder_produces_every_declared_tool() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let transport = Arc::new(LocalAppsMcpTransport::new(root.path().to_path_buf()));
        let built = local_app_builtin_tools(&transport, Some("app-a".into()));
        assert_eq!(
            built.len(),
            LOCAL_APP_TOOLS.len(),
            "a declared tool was dropped by the catalog lookup"
        );
        let names: std::collections::BTreeSet<&str> = built.iter().map(|t| t.name()).collect();
        for &(name, _, _) in LOCAL_APP_TOOLS {
            assert!(names.contains(name), "{name} was not built");
        }
    }

    #[test]
    fn forged_project_suffix_stays_on_global_inventory() {
        let root = tempfile::tempdir().expect("tempdir");
        let forged = root.path().join("ordinary-project/apps/app-a/workspace");
        std::fs::create_dir_all(&forged).expect("forged workspace");
        let transport = Arc::new(LocalAppsMcpTransport::new(root.path().to_path_buf()));
        // The composition root did not authenticate this as a Local App
        // workspace. A path suffix alone must not grant app-scoped tools.
        let built = local_app_builtin_tools(&transport, None);
        let names: std::collections::BTreeSet<&str> = built.iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            [
                "LocalAppCreate",
                "LocalAppGet",
                "LocalAppList",
                "LocalAppPrepare"
            ]
            .into_iter()
            .collect()
        );
    }

    /// The catalog's own loading decision must survive the move. Dropping it
    /// made all 21 descriptions plus the very large `update_manifest` /
    /// `mutate_data` schemas ride in EVERY mobile conversation, and inverted
    /// `LocalAppList`'s explicit "keep catalog discovery out of the eager
    /// tool set".
    #[test]
    fn the_catalogs_loading_decision_survives_the_move() {
        let transport = Arc::new(LocalAppsMcpTransport::new(std::path::PathBuf::from("/tmp")));
        let built = local_app_builtin_tools(&transport, None);
        let list = built
            .iter()
            .find(|t| t.name() == "LocalAppList")
            .expect("LocalAppList");
        assert!(list.should_defer(), "LocalAppList must stay deferred");
        assert_eq!(list.search_hint(), Some("discover existing local apps"));
    }

    /// The name table here and the rows in `permission::defaults_per_tool` are
    /// separate hand-written lists in different crates. Nothing else compares
    /// them, so renaming one side alone leaves every other guard green while
    /// `tool_default` silently falls back to `DenyByDefault` and the
    /// create-flow prompt storm returns.
    #[test]
    fn every_tool_has_a_permission_default_row() {
        for &(name, _, _) in LOCAL_APP_TOOLS {
            // `permission::tool_default` collapses "no row" and "a row that
            // says Deny" to the same DenyByDefault value, so asserting
            // against IT would let a dropped row pass silently (it would just
            // look like a deliberate deny). `tool_default_row` returns
            // `None` for a missing row, so assert ROW EXISTENCE first — by
            // name, so the failure names the specific missing tool — and
            // only then check the value.
            let row = permission::tool_default_row(name).unwrap_or_else(|| {
                panic!(
                    "{name} has no row in permission::defaults_per_tool; it \
                     would silently fall back to DenyByDefault"
                )
            });
            let expected_allow = matches!(
                name,
                "LocalAppList"
                    | "LocalAppGet"
                    | "LocalAppRuntimeProfiles"
                    | "LocalAppTemplateCatalog"
                    | "LocalAppPrepare"
                    | "LocalAppContract"
                    | "LocalAppLogs"
                    | "LocalAppCheckpointList"
                    | "LocalAppBackgroundList"
                    | "LocalAppBackgroundStatus"
                    | "LocalAppBuild"
                    | "LocalAppRuntime"
                    | "LocalAppScaffold"
            );
            let expected = if expected_allow {
                permission::PromptDefault::AllowByDefault
            } else {
                permission::PromptDefault::DenyByDefault
            };
            assert_eq!(row, expected, "{name} has the wrong permission default");
        }
    }

    /// r2-tests-honesty-012: the REVERSE of the guard above, which was
    /// forward-only. `every_tool_has_a_permission_default_row` catches a tool
    /// that lost its row; nothing caught a ROW that outlived its tool. An
    /// orphaned `LocalApp*` row is not cosmetic — `is_divergence_tool` keys
    /// the Plan-mode auto-allow carve-out off the `LocalApp` prefix, so a
    /// stale `AllowByDefault` row keeps granting a name nobody audits.
    ///
    /// THREE guards stand on `defaults_per_tool`'s table, not one:
    /// `init_defaults`'s `debug_assert_eq!(m.len(), 79)`, the test
    /// `table_splits_into_the_parity_set_and_the_mobile_divergence`'s
    /// `oracle == 44` / `divergence == 35`, and the test
    /// `the_counts_in_this_module_doc_are_the_counts_in_the_table`'s four
    /// hand-bumped `assert_eq!`s over `is_divergence_tool` x `PromptDefault`
    /// -- 23 oracle-deny, 21 oracle-allow, 14 divergence-allow, 21
    /// divergence-deny. None of the three catches this. All seven numbers
    /// count `TOOL_DEFAULTS` alone, so a tool DELETED from `LOCAL_APP_TOOLS`
    /// with its row left behind moves not one of them; they fail by naming a
    /// NUMBER rather than the offending row; a removal paired with an addition
    /// inside one bucket cancels out; and they live in a crate that cannot see
    /// `LOCAL_APP_TOOLS` at all. `permission::tool_default_names` was added
    /// for this test.
    #[test]
    fn every_local_app_permission_row_names_a_real_tool() {
        let declared: std::collections::BTreeSet<&str> =
            LOCAL_APP_TOOLS.iter().map(|&(name, _, _)| name).collect();
        let rows: Vec<&str> = permission::tool_default_names()
            .into_iter()
            .filter(|name| name.starts_with("LocalApp"))
            .collect();
        // Vacuity guard FIRST: prove the needle can hit. A
        // `tool_default_names` that returned nothing would make the orphan
        // check below pass for the wrong reason.
        assert!(
            rows.len() >= LOCAL_APP_TOOLS.len(),
            "vacuity: permission::tool_default_names returned only {} LocalApp row(s) for {} \
             declared tools, so the orphan check below would be comparing against nothing; \
             rows={rows:?}",
            rows.len(),
            LOCAL_APP_TOOLS.len()
        );
        let orphans: Vec<&str> = rows
            .iter()
            .copied()
            .filter(|name| !declared.contains(name))
            .collect();
        assert!(
            orphans.is_empty(),
            "permission::defaults_per_tool has LocalApp row(s) {orphans:?} naming no tool in \
             LOCAL_APP_TOOLS -- a renamed or deleted tool left its permission row behind, and \
             `is_divergence_tool` still hands that name the Plan-mode carve-out"
        );
        assert_eq!(
            rows.len(),
            LOCAL_APP_TOOLS.len(),
            "with no orphans, the two tables must be the same size; rows={rows:?}, \
             declared={declared:?}"
        );
    }

    /// This does not retype the lease allow-list: it drives the PRODUCTION
    /// entry point (`permission::WorkspacePermissionLeaseRegistry::
    /// allows_for_token`, which wraps the private `allows_for_lease` `matches!`
    /// arm) with a real lease over every declared `LocalApp*` tool name and
    /// asserts the resulting true/false split against the known five-name set.
    /// A prior version of this test only asserted the five names are members
    /// of `LOCAL_APP_TOOLS` — that compared a test constant to a test
    /// constant and would stay green even if the production arm were
    /// narrowed to zero names. This would fail on that mutation: every
    /// `allows_for_token` call would return `false` and the loop below would
    /// panic on the first expected-true name.
    #[test]
    fn the_lease_allowlist_matches_the_production_arm() {
        const LEASE_AUTHORIZED: &[&str] = &[
            "LocalAppBuild",
            "LocalAppLogs",
            "LocalAppRuntime",
            "LocalAppManifest",
            "LocalAppQueryData",
        ];
        let known: std::collections::BTreeSet<&str> =
            LOCAL_APP_TOOLS.iter().map(|&(name, _, _)| name).collect();
        for name in LEASE_AUTHORIZED {
            assert!(
                known.contains(name),
                "lease allow-list names unknown tool {name}"
            );
        }

        let root = tempfile::tempdir().expect("tempdir");
        let app_id = "app-lease-probe";
        let workspace = root.path().join("apps").join(app_id).join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let registry = permission::WorkspacePermissionLeaseRegistry::new();
        let lease = registry
            .begin_local_app(app_id, &workspace)
            .expect("canonical local-app layout must grant a lease");
        let token = Some(lease.token());
        // The `LocalApp*` branch of `allows_for_lease` returns before
        // consulting `roots`, so a dummy value proves nothing was skipped.
        let roots = permission::FsRoots {
            cwd: root.path().to_path_buf(),
            home: None,
            lingxi_home: root.path().to_path_buf(),
        };
        let input = serde_json::json!({"app_id": app_id});

        for &(name, _, _) in LOCAL_APP_TOOLS {
            let expected = LEASE_AUTHORIZED.contains(&name);
            let actual = registry.allows_for_token(token, name, &input, &roots);
            assert_eq!(
                actual, expected,
                "{name}: production lease allow-list disagrees with the known set"
            );
        }
        // Vacuity guard: the mismatched app_id path of the same production
        // call must still refuse, proving `input` above was actually load-bearing.
        let wrong_app_input = serde_json::json!({"app_id": "someone-elses-app"});
        assert!(
            !registry.allows_for_token(token, "LocalAppBuild", &wrong_app_input, &roots),
            "a mismatched app_id must never be authorized by this lease"
        );
    }

    /// These are builtins, not MCP. Reporting `is_mcp` would put them back on
    /// the tool-wide-only matching path the move exists to leave, and would
    /// make `LocalAppBuild(<app_id>)` inexpressible again.
    #[test]
    fn the_tools_do_not_report_themselves_as_mcp() {
        let transport = Arc::new(LocalAppsMcpTransport::new(std::path::PathBuf::from("/tmp")));
        for tool in local_app_builtin_tools(&transport, None) {
            assert!(!tool.is_mcp(), "{} must not report as MCP", tool.name());
        }
    }

    /// A workspace session must not reach a SIBLING app.
    ///
    /// These tools are auto-allowed by the defaults table and the static
    /// dispatch takes `app_id` from model input verbatim, so without host
    /// binding a session working on app-a could read app-b's records, logs,
    /// events or live WebView with no prompt — strictly worse than the
    /// `mcp__local_apps__*` spelling this replaced, which at least asked.
    #[tokio::test]
    async fn a_workspace_session_cannot_address_a_sibling_app() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let transport = Arc::new(LocalAppsMcpTransport::new(root.path().to_path_buf()));
        // Bind the way production does: from the exact canonical app id the
        // Host resolved at construction.
        let tools = local_app_builtin_tools(&transport, Some("app-a".into()));
        let query = tools
            .iter()
            .find(|t| t.name() == "LocalAppQueryData")
            .expect("LocalAppQueryData");

        // Deliberately left as the main loop leaves it.
        let ctx = tool_api::test_support::fresh_ctx();
        assert!(ctx.cwd.is_none(), "the main turn loop supplies no cwd");
        let refused = query
            .call(
                serde_json::json!({"app_id": "app-b", "collection": "journal"}),
                ctx.clone(),
                tool_api::test_support::fresh_tx(),
            )
            .await;
        match refused {
            Err(ToolError::InvalidInput(message)) => {
                assert!(
                    message.contains("app-b") && message.contains("app-a"),
                    "the refusal must name both ids: {message}"
                );
            }
            other => panic!("a sibling app must be refused, got {other:?}"),
        }

        // An isolated-agent cwd is metadata, not a new authority source. A
        // raw path that names app-b must not retarget tools Host-bound to
        // app-a when the session was composed.
        let mut hostile_ctx = tool_api::test_support::fresh_ctx();
        hostile_ctx.cwd = Some(root.path().join("apps/app-b/workspace"));
        let refused = query
            .call(
                serde_json::json!({"app_id": "app-b", "collection": "journal"}),
                hostile_ctx,
                tool_api::test_support::fresh_tx(),
            )
            .await;
        assert!(
            matches!(&refused, Err(ToolError::InvalidInput(message)) if message.contains("app-b") && message.contains("app-a")),
            "call-time cwd must not replace the Host-resolved binding: {refused:?}"
        );

        // Its OWN app is not refused by the binding (it fails later, on the
        // absent service — which proves the guard let it through).
        let own = query
            .call(
                serde_json::json!({"app_id": "app-a", "collection": "journal"}),
                ctx,
                tool_api::test_support::fresh_tx(),
            )
            .await;
        assert!(
            !matches!(&own, Err(ToolError::InvalidInput(m)) if m.contains("does not belong")),
            "the session's own app must pass the binding, got {own:?}"
        );
    }

    #[tokio::test]
    async fn a_global_session_must_confirm_app_targeted_auto_allowed_tools() {
        let transport = Arc::new(LocalAppsMcpTransport::new(std::path::PathBuf::from("/tmp")));
        let tools = local_app_builtin_tools(&transport, None);
        assert!(tools.iter().all(|tool| tool.name() != "LocalAppBuild"));
    }

    #[tokio::test]
    async fn a_global_session_must_confirm_background_list_and_status() {
        let transport = Arc::new(LocalAppsMcpTransport::new(std::path::PathBuf::from("/tmp")));
        let tools = local_app_builtin_tools(&transport, None);
        for name in ["LocalAppBackgroundList", "LocalAppBackgroundStatus"] {
            assert!(tools.iter().all(|tool| tool.name() != name), "{name}");
        }
    }

    #[tokio::test]
    async fn a_workspace_session_keeps_auto_allow_for_its_own_app() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let transport = Arc::new(LocalAppsMcpTransport::new(root.path().to_path_buf()));
        let tools = local_app_builtin_tools(&transport, Some("app-a".into()));
        for name in [
            "LocalAppGet",
            "LocalAppLogs",
            "LocalAppCheckpointList",
            "LocalAppBackgroundList",
            "LocalAppBackgroundStatus",
            "LocalAppBuild",
            "LocalAppRuntime",
        ] {
            let tool = tools.iter().find(|tool| tool.name() == name).expect("tool");
            let decision = tool
                .check_permissions(
                    &serde_json::json!({"app_id": "app-a"}),
                    &tool_api::test_support::fresh_ctx(),
                )
                .await;
            assert!(matches!(decision, PermissionResult::Allow { .. }), "{name}");
        }
    }

    /// Read-only classification drives `is_read_only`/`is_concurrency_safe`,
    /// which the dispatcher uses to decide what may overlap.
    #[test]
    fn read_only_classification_reaches_the_tool() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let transport = Arc::new(LocalAppsMcpTransport::new(root.path().to_path_buf()));
        let built = local_app_builtin_tools(&transport, Some("app-a".into()));
        let by_name: std::collections::BTreeMap<&str, &Arc<dyn Tool>> =
            built.iter().map(|t| (t.name(), t)).collect();
        let empty = serde_json::json!({});
        assert!(by_name["LocalAppLogs"].is_read_only(&empty));
        assert!(by_name["LocalAppQueryData"].is_read_only(&empty));
        assert!(!by_name["LocalAppBuild"].is_read_only(&empty));
        assert!(!by_name["LocalAppMutateData"].is_read_only(&empty));
    }

    /// The builtin must exist AND resolve to the `scaffold` provider
    /// operation. `local_app_builtin_tools` uses `filter_map` against the
    /// provider catalog, so a name registered without a matching catalog entry
    /// yields FEWER tools rather than an error — the tool would simply not
    /// exist, and the shell conversation would have no way out.
    #[test]
    fn scaffold_is_registered_and_maps_to_the_scaffold_operation() {
        assert!(
            LOCAL_APP_TOOLS
                .iter()
                .any(|&(name, operation, _)| name == "LocalAppScaffold" && operation == "scaffold"),
            "the builtin must map to the `scaffold` provider operation"
        );
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let transport = Arc::new(LocalAppsMcpTransport::new(root.path().to_path_buf()));
        let built = local_app_builtin_tools(&transport, Some("app-a".into()));
        let scaffold = built
            .iter()
            .find(|tool| tool.name() == "LocalAppScaffold")
            .expect("LocalAppScaffold must be BUILT, not merely named in the table");
        // The way out of an empty shell has to ride eagerly: the shell agent
        // is told to call it by name, and a deferred tool would have to be
        // found through ToolSearch first.
        assert!(
            !scaffold.should_defer(),
            "the shell's only way out must be in the eager tool set"
        );
        assert!(
            !scaffold.is_read_only(&serde_json::json!({})),
            "scaffolding writes an app's entire initial source tree"
        );
    }

    /// A global (non-app) conversation sees this builtin like every other, and
    /// it takes `app_id` from model input. The surface it commits is
    /// IMMUTABLE, so a silent auto-allow there would let an agent decide a
    /// stranger app's shape with no prompt.
    #[tokio::test]
    async fn a_global_session_must_confirm_scaffold() {
        let transport = Arc::new(LocalAppsMcpTransport::new(std::path::PathBuf::from("/tmp")));
        let tools = local_app_builtin_tools(&transport, None);
        assert!(tools.iter().all(|tool| tool.name() != "LocalAppScaffold"));
    }

    /// …and inside the shell's own workspace it must NOT prompt: the whole
    /// point of the conversational create flow is that the user confirms the
    /// name/brief/surface in the chat, not a second time in a permission
    /// sheet.
    #[tokio::test]
    async fn a_workspace_session_keeps_auto_allow_for_scaffold() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let transport = Arc::new(LocalAppsMcpTransport::new(root.path().to_path_buf()));
        let tools = local_app_builtin_tools(&transport, Some("app-a".into()));
        let scaffold = tools
            .iter()
            .find(|tool| tool.name() == "LocalAppScaffold")
            .expect("LocalAppScaffold");
        let decision = scaffold
            .check_permissions(
                &serde_json::json!({"app_id": "app-a", "name": "N", "brief": "b", "surface": "dom"}),
                &tool_api::test_support::fresh_ctx(),
            )
            .await;
        assert!(
            matches!(decision, PermissionResult::Allow { .. }),
            "the shell's own workspace must not prompt, got {decision:?}"
        );
    }

    /// NAMING BAN. `local_apps_mcp` asserts the whole catalog's schemas carry
    /// no `template`; this pins the same ban on the tool the model actually
    /// sees, description included, so a description-only regression (which the
    /// schema-only assertion cannot see) still fails.
    #[tokio::test]
    async fn the_scaffold_tool_avoids_the_banned_vocabulary() {
        let root = tempfile::tempdir().expect("tempdir");
        let workspace = root.path().join("apps/app-a/workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let transport = Arc::new(LocalAppsMcpTransport::new(root.path().to_path_buf()));
        let built = local_app_builtin_tools(&transport, Some("app-a".into()));
        let scaffold = built
            .iter()
            .find(|tool| tool.name() == "LocalAppScaffold")
            .expect("LocalAppScaffold");
        let schema = scaffold.input_schema().to_string().to_lowercase();
        // The five banned symbols, spelled out rather than counted.
        for banned in [
            "template",
            "dashboard",
            "crud_tracker",
            "content_showcase",
            "form_utility",
        ] {
            assert!(!schema.contains(banned), "schema names `{banned}`");
        }
        // `Tool::description` is async and input-aware; the constant text is
        // what `LocalAppTool` stores, so read it through the public accessor.
        let description = scaffold
            .description(
                &serde_json::json!({}),
                &DescriptionOptions {
                    is_non_interactive_session: false,
                },
            )
            .await
            .to_lowercase();
        for banned in [
            "template",
            "dashboard",
            "crud_tracker",
            "content_showcase",
            "form_utility",
        ] {
            assert!(
                !description.contains(banned),
                "description names `{banned}`: {description}"
            );
        }
        // Runtime identity is no longer model-selectable at scaffold time.
        // The native confirmation receipt is the only authority that reaches
        // this tool, and direct surface/profile fields remain absent.
        assert!(scaffold.input_schema()["properties"]["surface"].is_null());
        assert!(scaffold.input_schema()["properties"]["runtime_profile"].is_null());
        assert_eq!(
            scaffold.input_schema()["required"],
            serde_json::json!(["app_id", "name", "brief", "workflow_run_id", "receipt_id"])
        );
    }
}
