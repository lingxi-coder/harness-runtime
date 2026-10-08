//! The Local App product's rows for the permission crate's per-tool table.
//!
//! The table (`permission::tool_default`) knows the oracle's tools and `Workflow`. The `LocalApp*` family are
//! this product's first-party host operations, with no oracle counterpart; their defaults, which of them may
//! run while the user is only planning, and what a rule like `LocalAppBuild(app-a)` is matched against are
//! installed here, once, by whichever composition root is starting.

use permission::{PromptDefault, ToolDefaultExtension};
use std::collections::HashMap;

/// Install the Local App rows. Idempotent: the first call wins.
pub(crate) fn install() {
    let _ = permission::install_tool_default_extension(extension());
}

fn extension() -> ToolDefaultExtension {
    ToolDefaultExtension {
        rows: rows(),
        plan_safe: PLAN_SAFE_TOOLS,
        rule_content,
    }
}

/// The READ-ONLY host operations. Inspecting an app is exactly what planning does; the mutating siblings
/// (`LocalAppBuild`, `LocalAppRuntime`, …) are deliberately absent, so the Plan backstop still stops them.
///
/// The plan-driven create has to NAME a template while planning, and `LocalAppTemplateCatalog` and
/// `LocalAppRuntimeProfiles` are how a planner sees the catalog and the runtime profiles without guessing.
/// Both only read: `template_catalog` returns the redacted Host-verified view and `runtime_profiles` the
/// published catalog. The operation that ACTS on a choice (`LocalAppPrepare`) stays absent on purpose, so Plan
/// mode can still only look — approving the plan is what authorizes the landing.
const PLAN_SAFE_TOOLS: &[&str] = &[
    "LocalAppList",
    "LocalAppGet",
    "LocalAppLogs",
    "LocalAppCheckpointList",
    "LocalAppBackgroundList",
    "LocalAppBackgroundStatus",
    "LocalAppTemplateCatalog",
    "LocalAppRuntimeProfiles",
];

/// The local-app host operations key on their `app_id`, so `LocalAppBuild(app-a)` grants ONE app instead of
/// every app on the device. Without this the only expressible grant is the tool-wide one — the exact
/// limitation that moving these off `mcp__local_apps__*` was meant to remove.
///
/// Absent `app_id` yields `None`, i.e. a CONTENT rule never matches a call that names no app. A tool-wide rule
/// still matches either way.
fn rule_content(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    tool_name
        .starts_with("LocalApp")
        .then(|| input.get("app_id")?.as_str().map(str::to_string))
        .flatten()
}

fn rows() -> HashMap<&'static str, PromptDefault> {
    use PromptDefault::{AllowByDefault, DenyByDefault};
    let mut m: HashMap<&'static str, PromptDefault> = HashMap::with_capacity(36);
    // ---- MOBILE DIVERGENCE: first-party local-app host operations ----------
    // No oracle counterpart — claude-code has no host-owned local-app surface.
    // These are BUILTIN tools (see `harness_runtime::mobile::local_apps_tools`), not a
    // user-configured MCP server; while they were spelled `mcp__local_apps__*`
    // they matched nothing here and fell through to `DenyByDefault`, so the
    // create flow prompted on every step.
    //
    // Split by REVERSIBILITY, not read/write. The allow-by-default operations
    // still refine to Ask in `LocalAppTool::check_permissions` when a session
    // is not bound to an app workspace.
    m.insert("LocalAppList", AllowByDefault);
    m.insert("LocalAppGet", AllowByDefault);
    // Read-only: the tool table (`local_apps_tools::LOCAL_APP_TOOLS`) marks
    // its `read_only` column `true`.
    m.insert("LocalAppRuntimeProfiles", AllowByDefault);
    m.insert("LocalAppTemplateCatalog", AllowByDefault);
    m.insert("LocalAppLogs", AllowByDefault);
    m.insert("LocalAppCheckpointList", AllowByDefault);
    // Contract reads/staging are Host-bound to the current app workspace and
    // are part of the authoring loop; the tool still fails closed when the
    // session has no bound app.
    m.insert("LocalAppContract", AllowByDefault);
    // NOT auto-allowed: `read_app_events` DRAINS the unread queue and advances
    // a persisted cursor by default, so a speculative call permanently
    // consumes what the user's running app posted. `peek=true` is the
    // non-destructive form, but the defaults table is per-NAME, not per-input.
    m.insert("LocalAppEvents", DenyByDefault);
    m.insert("LocalAppBackgroundList", AllowByDefault);
    m.insert("LocalAppBackgroundStatus", AllowByDefault);
    // Writes, but app-local and trivially reversible: the build runs with the
    // network DISABLED into the app's own `dist/`, and the runtime is a local
    // preview server. These two are the hot loop of the create flow.
    m.insert("LocalAppBuild", AllowByDefault);
    m.insert("LocalAppRuntime", AllowByDefault);
    // The ONE way out of an app the "+" button created as an empty shell:
    // until it succeeds, every build/dependency/runtime/UI operation on that
    // app refuses. Allowed by default because the user has just confirmed the
    // name, brief and shape IN THE CONVERSATION — a permission sheet on top of
    // that confirmation asks the same question twice. The scope check is not
    // waived, only the policy prompt: `LocalAppTool::check_permissions` still
    // refines this to Ask in a session that is not bound to an app workspace,
    // which is the case where the target id comes from the model rather than
    // from the user's own workspace.
    m.insert("LocalAppScaffold", AllowByDefault);
    // The library's create sheet does NOT come through this gate: it sends
    // `ClientCommand::CreateApp` and creates the app outright, before any
    // conversation exists (see `client::protocol::version`). So the ONLY caller
    // this row governs is an agent reaching `LocalAppCreate` from a global or
    // project chat — which is exactly the case the deny exists for, and the
    // user is right there in that chat to answer.
    //
    // This was briefly `AllowByDefault` while the create sheet ran an intake
    // conversation and needed the agent to commit the create without a prompt.
    // That flow is gone; the exemption went with it.
    m.insert("LocalAppCreate", DenyByDefault);
    // The plan-driven create/modify step. It asks NOTHING here because it asks
    // everything of the plan: the user approved the exact name, brief, spec and
    // template that this call lands, and the Host re-reads that approval record
    // itself rather than trusting anything the caller sends (see
    // `plan_approval`). A policy prompt on top would be the duplicate create
    // confirmation the plan flow exists to remove. In a session NOT bound to an
    // app workspace `LocalAppTool::check_permissions` still refines this to Ask,
    // which is the create case — a global chat naming an app id.
    m.insert("LocalAppPrepare", AllowByDefault);
    // MCP proposal lifecycle — the ONLY path by which network-reaching MCP
    // server configuration gets authored onto an app and promoted into its
    // live catalog, so every step of it asks. `approve_mcp_proposal` asks even
    // in its `create_without_mcp=true` branch, which authors no tools: that
    // branch still seals and persists a signed candidate + journal for the
    // app and drives the same approval surface, and the row is per-NAME, not
    // per-input, so it cannot be split by that flag.
    m.insert("LocalAppValidateMcpProposal", DenyByDefault);
    m.insert("LocalAppApproveMcpProposal", DenyByDefault);
    m.insert("LocalAppQaMcpCandidate", DenyByDefault);
    m.insert("LocalAppPromoteMcpCandidate", DenyByDefault);
    // r2-never-wired-02: newly wired into `local_apps_tools::LOCAL_APP_TOOLS`.
    // Same posture as the MCP proposal lifecycle above: a native confirmation
    // sheet already gates the actual dependency change/apply inside the
    // handler, but the row here is the POLICY prompt in front of that sheet,
    // and network-reaching dependency resolution is not trivially undone.
    m.insert("LocalAppConfirmDependencyChange", DenyByDefault);
    m.insert("LocalAppUpdateDependencies", DenyByDefault);
    // Effects the user cannot trivially undo, or that reach the network.
    // These two expose an app's CONTENT — user records and the live WebView
    // DOM. Binding scopes them inside an app workspace, but a GLOBAL
    // conversation has no binding and can name any app, so they ask.
    m.insert("LocalAppQueryData", DenyByDefault);
    m.insert("LocalAppInspectUi", DenyByDefault);
    // Strictly more revealing than `LocalAppInspectUi`, which is already
    // DenyByDefault: the DOM snapshot nulls out `password`/`hidden` input
    // values and a pixel capture cannot redact anything it renders.
    m.insert("LocalAppCaptureUi", DenyByDefault);
    // QA evidence can contain user data and screenshots. Beginning or
    // finalizing a run mutates Host QA state; reading evidence exposes it.
    m.insert("LocalAppQaBegin", DenyByDefault);
    m.insert("LocalAppQaReadEvidence", DenyByDefault);
    m.insert("LocalAppQaFinalize", DenyByDefault);
    m.insert("LocalAppManifest", DenyByDefault);
    m.insert("LocalAppMutateData", DenyByDefault);
    m.insert("LocalAppActOnUi", DenyByDefault);
    m.insert("LocalAppCheckpointCreate", DenyByDefault);
    m.insert("LocalAppCheckpointRestore", DenyByDefault);
    m.insert("LocalAppInstallDeps", DenyByDefault);
    m.insert("LocalAppBackgroundSchedule", DenyByDefault);
    m.insert("LocalAppBackgroundCancel", DenyByDefault);
    m.insert("LocalAppBackgroundRetry", DenyByDefault);
    debug_assert_eq!(m.len(), 36, "the Local App rows must list all 36 tools");
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use permission::{is_plan_safe_tool, tool_default};

    fn installed() {
        install();
    }

    /// The local-app host operations are FIRST-PARTY builtin tools, not a
    /// user-configured MCP server. The split stays based on reversibility; the
    /// tool-level permission refinement adds the missing session-scope check
    /// for every allow-by-default operation that targets an app.
    #[test]
    fn local_app_tools_split_by_reversibility() {
        installed();
        for name in [
            "LocalAppList",
            "LocalAppGet",
            "LocalAppTemplateCatalog",
            "LocalAppPrepare",
            "LocalAppLogs",
            "LocalAppCheckpointList",
            "LocalAppBackgroundList",
            "LocalAppBackgroundStatus",
            "LocalAppBuild",
            "LocalAppRuntime",
            // The shell's only way out; the user confirmed it in the chat.
            "LocalAppScaffold",
        ] {
            assert_eq!(
                tool_default(name),
                PromptDefault::AllowByDefault,
                "{name} should reach its tool-level scope refinement without a policy prompt"
            );
        }

        for name in [
            // The library's create sheet bypasses tools entirely; the only
            // caller here is an agent creating an app from a global or project
            // chat, with the user present to answer.
            "LocalAppCreate",
            // Expose app CONTENT; a global chat can name any app.
            "LocalAppQueryData",
            "LocalAppInspectUi",
            // Renders what inspect_ui redacts.
            "LocalAppCaptureUi",
            // Drains the unread queue and advances a persisted cursor.
            "LocalAppEvents",
            // Mutates the user's own records.
            "LocalAppMutateData",
            // Drives the app UI on the user's behalf.
            "LocalAppActOnUi",
            // Can discard uncommitted work.
            "LocalAppCheckpointRestore",
            "LocalAppCheckpointCreate",
            "LocalAppManifest",
            // Opens the network.
            "LocalAppInstallDeps",
            "LocalAppBackgroundSchedule",
            "LocalAppBackgroundCancel",
            "LocalAppBackgroundRetry",
        ] {
            assert_eq!(
                tool_default(name),
                PromptDefault::DenyByDefault,
                "{name} must still ask"
            );
        }
    }

    /// Thirty-six rows: 13 allow-by-default (read-only, plus the network-disabled build, the restartable local
    /// preview runtime, the shell-scaffolding commit, guarded Create coordination) and 23 deny-by-default (user
    /// data, UI actuation, view capture, checkpoint restore, network, and the MCP proposal / dependency-review
    /// lifecycle). With `Workflow` that is the 14 / 23 split the permission table used to state itself.
    #[test]
    fn the_rows_are_the_rows_the_table_used_to_carry() {
        let m = rows();
        let count = |want: PromptDefault| m.values().filter(|v| **v == want).count();
        assert_eq!(m.len(), 36);
        assert_eq!(count(PromptDefault::AllowByDefault), 13, "allow rows");
        assert_eq!(count(PromptDefault::DenyByDefault), 23, "deny rows");
        assert!(m.keys().all(|name| name.starts_with("LocalApp")));
    }

    /// Plan mode: reading is frictionless, and anything that acts on an app is not plan-safe, even where its
    /// default is allow (a 30-minute build, an app put on screen).
    #[test]
    fn plan_safe_rows_are_read_only_and_every_one_has_a_row() {
        installed();
        for name in PLAN_SAFE_TOOLS {
            assert!(is_plan_safe_tool(name), "{name}");
            assert_eq!(
                tool_default(name),
                PromptDefault::AllowByDefault,
                "{name} reads, so it is allowed"
            );
        }
        for name in [
            "LocalAppBuild",
            "LocalAppRuntime",
            "LocalAppPrepare",
            "LocalAppScaffold",
        ] {
            assert_eq!(tool_default(name), PromptDefault::AllowByDefault, "{name}");
            assert!(
                !is_plan_safe_tool(name),
                "{name} acts, so Plan mode must still gate it"
            );
        }
        assert!(permission::is_divergence_tool("LocalAppBuild"));
    }

    #[test]
    fn a_rule_is_keyed_on_the_apps_id() {
        let call = |tool: &str, input: serde_json::Value| rule_content(tool, &input);
        assert_eq!(
            call("LocalAppMutateData", serde_json::json!({"app_id": "app-a"})).as_deref(),
            Some("app-a")
        );
        assert_eq!(
            call("LocalAppMutateData", serde_json::json!({})),
            None,
            "a call naming no app matches no content rule"
        );
        assert_eq!(
            call("LocalAppMutateData", serde_json::json!({"app_id": 7})),
            None
        );
        assert_eq!(
            call("Read", serde_json::json!({"app_id": "app-a"})),
            None,
            "only this product's tools"
        );
    }
}
