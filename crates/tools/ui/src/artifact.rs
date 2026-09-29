//! `ArtifactTool` — the `Artifact` tool (claude-code 2.1.207 `eIs`, name `dw`).
//!
//! 1:1 registration skeleton for claude-code's Artifact tool: it renders an
//! HTML or Markdown file to a default-private claude.ai web page the user can
//! later share. This port lands the **register-but-disable** surface that is the
//! load-bearing parity (H-BIN-03):
//!
//!   - the byte-faithful tool metadata (name/searchHint/userFacingName, the
//!     input schema, the output schema union, `maxResultSizeChars`,
//!     `isConcurrencySafe`/`isReadOnly` = `action==="list"`),
//!   - the model-facing `description` + `prompt` strings, and
//!   - `is_enabled()` = [`tool_api::artifact_gate::is_enabled`] (CC `dY()`),
//!     which reads its code-default `false` with no Statsig backend, so the tool
//!     registers DISABLED and is invisible to the model — byte-identical to the
//!     shipped binary on a host without the `tengu_cobalt_plinth` gate.
//!
//! The full publish/list pipeline against the claude.ai backend (share-status
//! probe, capabilities read-back, favicon/title/URL sanitizers, the artifacts
//! API client, and the `artifact-design` / `artifact-capabilities` bundled
//! skills that ride the same gate) is Stage-2 follow-up: `call()` returns a
//! truthful "not wired" error, and `validate_input` / `check_permissions` port
//! the pure, byte-exact branches (the file-existence / favicon-entity / URL
//! probes that need the backend are deferred). Because the tool is gated off,
//! none of these paths are reachable by the model in this build.

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::{PermissionMetadata, PermissionPrompt};
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use std::path::PathBuf;

use tool_api::artifact_gate;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::BuiltinToolContext;

/// Binary `dw` — the tool name.
pub use tool_api::artifact_gate::ARTIFACT_TOOL_NAME;

/// Binary `eIs.searchHint`.
const SEARCH_HINT: &str = "render an HTML or Markdown file to a claude.ai web page";

/// Binary `eIs.description()` — the short model-facing description (distinct from
/// the long `prompt()` = `zHd`).
const DESCRIPTION: &str = "Render an HTML or Markdown file to an Artifact \u{2014} a default-private claude.ai web page the user can share with teammates.";

// ── Input-schema `describe(...)` strings (binary `XHd`), byte-exact ──────────
const DESC_ACTION: &str = r#"Omit (or 'publish') to publish file_path. 'list' enumerates the user's published artifacts; only `limit` may accompany it."#;
const DESC_FILE_PATH: &str = "Path to an .html or .md file to render. Required to publish (the default action). Use a short, distinctive basename \u{2014} it is the fallback title if the HTML has no <title>.";
const DESC_FAVICON: &str = r#"Browser-tab icon: one or two emoji (e.g. "📊"). No markup. Required to publish. Keep stable across redeploys; change only on a hard topic pivot."#;
const DESC_LIMIT: &str = "list only: maximum artifacts to return (default 25).";
const DESC_DESCRIPTION: &str =
    "One-sentence subtitle shown on the gallery card. Say what the page is or does.";
const DESC_LABEL: &str = r#"Short human-readable name for this version, max 60 chars (e.g. "fixed-background"). Shown in the version picker. Not a description — keep it to a few words."#;
const DESC_URL: &str = r#"Existing artifact URL to update in place. Pass whenever the user wants to update an artifact this conversation did not publish — "update my artifact", "keep the same link", a pasted artifact URL — and find the URL with action: "list" if you don't have it; without this, a conversation that didn't publish the artifact always mints a new URL. Omit for new artifacts and same-conversation redeploys. Must be an artifact the user owns."#;
const DESC_FORCE: &str = "Overwrite without a conflict check. Use only after a 409 when you have reconciled with the other session's version and intend to replace it. Omit (or false) to send baseVersion so a concurrent write 409s instead of being silently clobbered.";

/// Binary `zHd` — the long `prompt()` string. Byte-exact runtime form (the
/// binary stores it as a template literal with `\uXXXX` escapes, escaped
/// backticks, and a `${IOe}` = `artifact-design` interpolation; paragraphs are
/// joined by real `\n\n`). 4742 bytes.
const PROMPT: &str = r#"Render an HTML or Markdown file to an Artifact — a default-private web page hosted on claude.ai that the user can later choose to share with their teammates. Use this when communicating visually would be clearer than terminal text. Publishing proactively is fine for your own work-product — artifacts start private. The exception is content that could mislead or cause harm if shared onward: anything imitating a real organization, person, or record, or content the user framed as sensitive. Build those as files, and let the user decide whether they get a URL.

**Before writing the page, you MUST load the `artifact-design` skill** to calibrate how much design investment this particular request warrants. Then write the content to a file (via Write/Edit) and call Artifact with its path. The file is wrapped in a `<!doctype html>…<head>…</head><body>` skeleton at publish time, so write the page content directly — no `<!DOCTYPE>`, `<html>`, `<head>`, or `<body>` tags of your own. The file includes a minimal CSS reset. Unless the user names a location, put the file in your scratchpad directory if one is listed in your system prompt.

**Title**: Set a concise `<title>` in the HTML — it names the artifact in the browser tab and gallery. Keep it stable across redeploys. Pass a one-sentence `description` parameter — it becomes the gallery card's subtitle.

**To update**: Edit the file, then call Artifact again with the same file path — it redeploys to the same URL. A different file path claims a new URL so only use a different path if you intend to create a separate new Artifact.

**To update an artifact from an earlier conversation** — whenever the user wants an existing artifact updated or its link kept, not only when they paste a URL: pass the artifact's URL as `url` (find it with `action: "list"` if you don't have it). Without `url`, a conversation that didn't publish the artifact always mints a new URL — there is no other way to target an existing one.

**To read an existing artifact's content**: call WebFetch with its URL.

**To find artifacts from earlier sessions**: pass `action: "list"` (with no other parameter except optionally `limit`) to enumerate the user's published artifacts — title, URL, and last-updated, newest first. Use it when the user refers to a published artifact whose URL you don't have, then follow the update flow above with the URL you found. Artifacts published earlier in THIS session need neither `action: "list"` nor `url` — calling again with the same file path redeploys them.

**Files you did not write**: Read the complete file before publishing it, even when asked not to ("it's personal", "no need to open it") — publishing distributes the content, and you must never distribute what you haven't seen. A request for privacy is a reason to read before publishing, not an exemption. If you cannot read it, do not publish it.

**Self-contained only**: A strict CSP blocks requests to any external host — CDN scripts, external stylesheets, fonts, remote images, fetch/XHR/WebSockets. Inline all CSS/JS and embed assets as data: URIs.

**Responsive**: Use relative units, flexbox/grid, `max-width:100%` on images. Wide content (tables, diagrams, code blocks) must scroll inside its own `overflow-x: auto` container — the page body must never scroll horizontally.

**Theme-aware**: Pages render in the viewer's light or dark theme. Unless the design deliberately commits to a single look, style both: use `@media (prefers-color-scheme: dark)` as the default signal, plus `:root[data-theme="dark"]` / `:root[data-theme="light"]` overrides — the viewer's theme toggle stamps `data-theme` on the root element, and it must win in both directions.

**Favicon** (required): Pass one or two emoji as `favicon` (e.g. `"📊"`, `"🐛"`, `"⚡🔥"`). It becomes the browser-tab icon. Emoji only — no SVG, no markup. Keep it the **same** across redeploys of an artifact — users find their tab by its icon, and a changed favicon reads as a different page. Only pick a new emoji on a hard pivot in what the artifact is about (new investigation, new deliverable), not for incremental updates.

**Never publish**: pages that impersonate a real person or organization (their name, branding, byline, or domain); fabricated records, receipts, or reviews presented as genuine; forms or flows that collect credentials or payment details under false pretenses; or content targeting a private individual. This applies whether you authored the page or the user supplied it, and regardless of claimed purpose ("it's a prop", "for testing") when the page would function as the real thing. If publishing is refused, do not suggest other ways to host or distribute the page."#;

/// Binary `XHd` — the input schema (`strictObject`, every field optional).
static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": { "type": "string", "enum": ["publish", "list"], "description": DESC_ACTION },
            "file_path": { "type": "string", "description": DESC_FILE_PATH },
            "favicon": { "type": "string", "minLength": 1, "maxLength": 32, "description": DESC_FAVICON },
            "limit": { "type": "integer", "minimum": 1, "maximum": 50, "description": DESC_LIMIT },
            "description": { "type": "string", "maxLength": 1000, "description": DESC_DESCRIPTION },
            "label": { "type": "string", "maxLength": 60, "description": DESC_LABEL },
            "url": { "type": "string", "description": DESC_URL },
            "force": { "type": "boolean", "description": DESC_FORCE }
        }
    })
});

/// Binary `aOy` = `E.union([iOy, sOy])` — publish-result | list-result.
static OUTPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "anyOf": [
            {
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "path": { "type": "string" },
                    "title": { "type": "string" },
                    "version": { "type": "string" },
                    "capabilities": {},
                    "stored": {
                        "type": "object",
                        "properties": {
                            "contract": { "type": "string" },
                            "capabilities": { "type": "object", "additionalProperties": {} }
                        },
                        "required": ["contract"]
                    },
                    "warnings": { "type": "array", "items": { "type": "string" } },
                    "contract": { "type": "string" },
                    "updated": { "type": "boolean" }
                },
                "required": ["url", "path"]
            },
            {
                "type": "object",
                "properties": {
                    "artifacts": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": { "type": "string" },
                                "url": { "type": "string" },
                                "updatedAt": { "type": "string" }
                            },
                            "required": ["title", "url"]
                        }
                    },
                    "truncated": { "type": "boolean" }
                },
                "required": ["artifacts"]
            }
        ]
    })
});

/// `Artifact` — render an HTML/Markdown file to a claude.ai web page (binary `eIs`).
pub struct ArtifactTool {
    _ctx: BuiltinToolContext,
}

impl ArtifactTool {
    /// Construct the tool over the builtin context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { _ctx: ctx }
    }
}

/// `e?.action==="list"` — the only concurrency-safe / read-only action.
fn is_list_action(input: &Value) -> bool {
    input.get("action").and_then(Value::as_str) == Some("list")
}

#[async_trait]
impl Tool for ArtifactTool {
    fn name(&self) -> &str {
        ARTIFACT_TOOL_NAME
    }

    fn search_hint(&self) -> Option<&str> {
        Some(SEARCH_HINT)
    }

    fn user_facing_name(&self) -> Option<&str> {
        Some(ARTIFACT_TOOL_NAME)
    }

    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }

    fn output_schema(&self) -> Option<&Value> {
        Some(&OUTPUT_SCHEMA)
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        // PARITY: binary `isEnabled(){return dY()}`. With no Statsig backend the
        // dominant `tengu_cobalt_plinth` gate reads its code-default `false`, so
        // the tool is registered-but-disabled (invisible to the model),
        // byte-identical to the shipped binary. See `tool_api::artifact_gate`.
        artifact_gate::is_enabled()
    }

    fn max_result_size_chars(&self) -> usize {
        // Binary `maxResultSizeChars:16000`.
        16_000
    }

    fn is_concurrency_safe(&self, input: &Value) -> bool {
        is_list_action(input)
    }

    fn is_read_only(&self, input: &Value) -> bool {
        is_list_action(input)
    }

    fn get_path(&self, input: &Value) -> Option<PathBuf> {
        // Binary `getPath({file_path:e}){return e?$i(e):Ct()}` — the target file
        // (resolved) or the cwd. We return the raw file_path when present; the
        // cwd fallback is left to the permission engine's default.
        input
            .get("file_path")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    }

    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        // PARITY: binary `validateInput` (errorCodes 1-8). The pure, structural
        // branches are ported byte-exact here; the favicon-entity (errorCode 6),
        // artifact-URL (errorCodes 4/5), and file-existence/size (errorCodes 2/3)
        // checks need the Stage-2 helper ports + backend and are deferred. The
        // Rust `ValidationError` carries the model-facing message (the numeric
        // errorCode is CC-internal telemetry).
        let action = input.get("action").and_then(Value::as_str);
        let file_path = input.get("file_path").and_then(Value::as_str);
        let favicon = input.get("favicon").and_then(Value::as_str);

        if action == Some("list") {
            // errorCode 8: `list` takes only `limit`.
            let extras: Vec<&str> = [
                "file_path",
                "favicon",
                "description",
                "label",
                "url",
                "force",
            ]
            .into_iter()
            .filter(|k| input.get(*k).is_some_and(|v| !v.is_null()))
            .collect();
            if !extras.is_empty() {
                return Err(ValidationError(format!(
                    "action \"list\" takes only `limit` \u{2014} remove {}. To publish or update an artifact, omit `action`.",
                    extras.join(", ")
                )));
            }
            return Ok(());
        }

        // errorCode 7: publish requires file_path + favicon.
        if file_path.is_none() || favicon.is_none() {
            let missing: Vec<&str> = [("file_path", file_path), ("favicon", favicon)]
                .into_iter()
                .filter(|(_, v)| v.is_none())
                .map(|(k, _)| k)
                .collect();
            return Err(ValidationError(format!(
                "{} required to publish",
                missing.join(" and ")
            )));
        }

        // errorCode 8: `limit` is a list-only field.
        if input.get("limit").is_some_and(|v| !v.is_null()) {
            return Err(ValidationError(
                "`limit` applies only to action \"list\"".to_string(),
            ));
        }

        // errorCode 1: only .html / .htm / .md render.
        let name = file_path.unwrap_or("");
        let ext = std::path::Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e.to_ascii_lowercase()))
            .unwrap_or_default();
        if ext != ".html" && ext != ".htm" && ext != ".md" {
            let shown = if ext.is_empty() {
                "(none)".to_string()
            } else {
                ext
            };
            return Err(ValidationError(format!(
                "unsupported file type: {shown} \u{2014} use .html or .md"
            )));
        }

        // NOTE (Stage-2): favicon-entity (errorCode 6), artifact-URL env match
        // (errorCodes 4/5), and file-existence/size (errorCodes 2/3) checks are
        // deferred with the publish pipeline.
        Ok(())
    }

    async fn check_permissions(&self, input: &Value, _ctx: &ToolUseContext) -> PermissionResult {
        // PARITY: binary `checkPermissions`. The share-status probe + capabilities
        // read-back + list-already-approved app-state read are Stage-2; the
        // byte-exact ASK/DENY wording for the reachable branches is ported here.
        if is_list_action(input) {
            // First artifact listing this session requires confirmation (the
            // `artifactListApproved` app-state fast-path is Stage-2).
            return PermissionResult::Ask {
                reason: PermissionDecisionReason::Other {
                    reason: "First artifact listing this session requires confirmation".into(),
                },
                prompt: PermissionPrompt {
                    title: ARTIFACT_TOOL_NAME.into(),
                    message:
                        "Claude wants to list your published artifacts (titles and links from your earlier sessions)"
                            .into(),
                    options: vec![],
                },
                pending_classifier_check: None,
                metadata: PermissionMetadata::default(),
            };
        }

        let file_path = input.get("file_path").and_then(Value::as_str);
        let favicon = input.get("favicon").and_then(Value::as_str);
        if file_path.is_none() || favicon.is_none() {
            return PermissionResult::Deny {
                reason: PermissionDecisionReason::Other {
                    reason: "Publish input missing required fields".into(),
                },
                explanation: Some("file_path and favicon are required to publish".into()),
                metadata: PermissionMetadata::default(),
            };
        }

        // Base publish ASK (owner/private, no title probe): byte-exact with the
        // binary's `Claude wants to publish ${file_path} to a private page on
        // claude.ai` fallback branch.
        PermissionResult::Ask {
            reason: PermissionDecisionReason::Other {
                reason: "Publishing a file to the web requires confirmation".into(),
            },
            prompt: PermissionPrompt {
                title: ARTIFACT_TOOL_NAME.into(),
                message: format!(
                    "Claude wants to publish {} to a private page on claude.ai",
                    file_path.unwrap_or("")
                ),
                options: vec![],
            },
            pending_classifier_check: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _input: &Value, _opts: &DescriptionOptions) -> String {
        DESCRIPTION.to_string()
    }

    async fn prompt(&self, _opts: &PromptOptions) -> String {
        PROMPT.to_string()
    }

    async fn call(
        &self,
        _input: Value,
        _ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        // Stage-2: the claude.ai publish/list pipeline (artifacts API client,
        // share-status probe, capabilities contracts, live-update subscription)
        // is not wired in this build. The tool is gated off (see `is_enabled`),
        // so this path is unreachable by the model; the error is truthful rather
        // than a stub success.
        Err(ToolError::Internal(
            "Artifact publishing is not available in this build".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::process::ProcessOutput;
    use tool_api::test_support::{fresh_ctx, shell_test_ctx};

    fn dummy_out() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("CLAUDE_CODE_DISABLE_ARTIFACT");
        std::env::remove_var("CLAUDE_CODE_ARTIFACT");
        std::env::remove_var("CLAUDE_CODE_ENTRYPOINT");
        telemetry::test_clear_flag("tengu_cobalt_plinth");
        telemetry::test_clear_flag("allow_cobalt_plinth");
        g
    }

    fn tool() -> ArtifactTool {
        ArtifactTool::new(shell_test_ctx(dummy_out()))
    }

    #[test]
    fn name_and_static_metadata() {
        let _g = guard();
        let t = tool();
        assert_eq!(t.name(), "Artifact");
        assert_eq!(t.user_facing_name(), Some("Artifact"));
        assert_eq!(
            t.search_hint(),
            Some("render an HTML or Markdown file to a claude.ai web page")
        );
        assert_eq!(t.max_result_size_chars(), 16_000);
        assert!(!t.should_defer());
        // isConcurrencySafe / isReadOnly == (action === "list").
        assert!(t.is_concurrency_safe(&json!({"action": "list"})));
        assert!(t.is_read_only(&json!({"action": "list"})));
        assert!(!t.is_concurrency_safe(&json!({"file_path": "x.html"})));
        assert!(!t.is_read_only(&json!({"file_path": "x.html"})));
    }

    #[test]
    fn disabled_by_default_enabled_by_flags() {
        let _g = guard();
        let t = tool();
        let ctx = ToolStaticContext::default();
        assert!(!t.is_enabled(&ctx));
        telemetry::test_set_flag("tengu_cobalt_plinth", true);
        telemetry::test_set_flag("allow_cobalt_plinth", true);
        assert!(t.is_enabled(&ctx));
    }

    #[test]
    fn prompt_and_description_byte_exact() {
        let _g = guard();
        let t = tool();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let p = rt.block_on(t.prompt(&PromptOptions::default()));
        assert_eq!(p.len(), 4742, "prompt byte length");
        assert!(p.starts_with("Render an HTML or Markdown file to an Artifact \u{2014} a default-private web page hosted on claude.ai"));
        assert!(
            p.contains("**Before writing the page, you MUST load the `artifact-design` skill**")
        );
        assert!(p.ends_with("do not suggest other ways to host or distribute the page."));
        // Paragraphs are joined by `\n\n`.
        assert_eq!(p.split("\n\n").count(), 13);
        let d = rt.block_on(t.description(
            &Value::Null,
            &DescriptionOptions {
                is_non_interactive_session: false,
            },
        ));
        assert_eq!(
            d,
            "Render an HTML or Markdown file to an Artifact \u{2014} a default-private claude.ai web page the user can share with teammates."
        );
    }

    #[test]
    fn input_schema_shape() {
        let _g = guard();
        let t = tool();
        let s = t.input_schema();
        assert_eq!(s["additionalProperties"], json!(false));
        let props = &s["properties"];
        assert_eq!(props["action"]["enum"], json!(["publish", "list"]));
        assert_eq!(props["favicon"]["minLength"], json!(1));
        assert_eq!(props["favicon"]["maxLength"], json!(32));
        assert_eq!(props["limit"]["maximum"], json!(50));
        assert_eq!(props["label"]["maxLength"], json!(60));
        assert_eq!(props["description"]["maxLength"], json!(1000));
        // No required fields (every field optional).
        assert!(s.get("required").is_none());
    }

    #[tokio::test]
    async fn validate_input_error_messages() {
        let _g = guard();
        let t = tool();
        let ctx = fresh_ctx();

        // list with extra keys → byte-exact removal message.
        let e = t
            .validate_input(&json!({"action": "list", "file_path": "x.html"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(
            e.0,
            "action \"list\" takes only `limit` \u{2014} remove file_path. To publish or update an artifact, omit `action`."
        );

        // missing both required.
        let e = t.validate_input(&json!({}), &ctx).await.unwrap_err();
        assert_eq!(e.0, "file_path and favicon required to publish");

        // missing favicon only.
        let e = t
            .validate_input(&json!({"file_path": "x.html"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(e.0, "favicon required to publish");

        // limit on a publish call.
        let e = t
            .validate_input(
                &json!({"file_path": "x.html", "favicon": "📊", "limit": 5}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert_eq!(e.0, "`limit` applies only to action \"list\"");

        // unsupported extension.
        let e = t
            .validate_input(&json!({"file_path": "notes.txt", "favicon": "📊"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(e.0, "unsupported file type: .txt \u{2014} use .html or .md");

        // no extension → "(none)".
        let e = t
            .validate_input(&json!({"file_path": "README", "favicon": "📊"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(
            e.0,
            "unsupported file type: (none) \u{2014} use .html or .md"
        );

        // a valid .html publish passes the ported structural checks.
        assert!(t
            .validate_input(&json!({"file_path": "page.html", "favicon": "📊"}), &ctx)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn check_permissions_wording() {
        let _g = guard();
        let t = tool();
        let ctx = fresh_ctx();

        match t.check_permissions(&json!({"action": "list"}), &ctx).await {
            PermissionResult::Ask { prompt, .. } => assert_eq!(
                prompt.message,
                "Claude wants to list your published artifacts (titles and links from your earlier sessions)"
            ),
            other => panic!("expected Ask, got {other:?}"),
        }

        match t
            .check_permissions(&json!({"file_path": "x.html"}), &ctx)
            .await
        {
            PermissionResult::Deny { explanation, .. } => {
                assert_eq!(
                    explanation.as_deref(),
                    Some("file_path and favicon are required to publish")
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }

        match t
            .check_permissions(&json!({"file_path": "page.html", "favicon": "📊"}), &ctx)
            .await
        {
            PermissionResult::Ask { prompt, .. } => assert_eq!(
                prompt.message,
                "Claude wants to publish page.html to a private page on claude.ai"
            ),
            other => panic!("expected Ask, got {other:?}"),
        }
    }
}
