//! `Visualization` — publish an HTML fragment as an inline chat widget.
//!
//! LingXi-only; unrelated to the claude.ai `Artifact` tool next to it, which
//! uploads pages to the web. This tool never leaves the device: it checks the
//! fragment, stores it as an immutable revision of the conversation's root
//! session, and returns the reference line the assistant writes into its
//! reply. Hosts render that line inline in a sandboxed WebView.
//!
//! The fragment arrives inline (`html`) or from a file the agent wrote and
//! perhaps checked with a shell (`file_path`). A file is read like `Read`
//! reads it: guest paths are translated, the path must sit in a trusted
//! directory, and `Read` permission rules decide.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::json;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolInputRepairMetadata,
    ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext, ValidationError,
};
use tool_api::util::path_validation::{
    canonicalize_and_validate, emit_blocked_event, resolve_against_cwd, translate_model_path,
};
use tool_api::BuiltinToolContext;
use visualization::checks::{check_fragment, MAX_FRAGMENT_BYTES};
use visualization::store::{Publisher, StoreError, VisualizationStore, MAX_TITLE_CHARS};
use visualization::{VisualizationId, TOOL_NAME};

const DESCRIPTION: &str = "Publish an interactive HTML widget (chart, simulation, explainer, UI mockup) that renders inline in this chat, offline and sandboxed. Returns a reference line to place in your reply. Nothing is uploaded.";

const PROMPT: &str = r#"Publish a self-contained HTML fragment as an inline visualization in this conversation. The widget renders offline in a sandboxed frame inside the chat; nothing is uploaded anywhere.

Load the `visualize` skill first unless you already have it in context: it covers when a widget is worth it, the sandbox rules, the theme variables and controls, and the `lingxi` global state API.

- Pass the fragment in `html`, or pass `file_path` for a fragment you wrote to an `.html` file (for example to check it with a shell first). Give exactly one.
- The fragment has no `<!doctype>`, `<html>`, `<head>` or `<body>`, loads nothing from the network, and is at most 2 MiB. `d3`, `lucide` and Floating UI tooltips are already loaded.
- `title` names the widget (at most 80 characters).
- To revise a widget you published, pass its `id`; each publish creates a new immutable revision.

After a successful publish, write the returned reference line on its own line in your reply, exactly as given and outside any code block. Without it the widget shows only behind this tool call."#;

static INPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    Utf16JsonProjection::from_serde_value(&json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "title": {
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_TITLE_CHARS,
                "description": "Short title for the widget, shown above it and to assistive technology."
            },
            "html": {
                "type": "string",
                "description": "The HTML fragment: markup with inline <style> and <script>. Omit when passing file_path."
            },
            "file_path": {
                "type": "string",
                "description": "Absolute path of an .html file holding the fragment. Omit when passing html."
            },
            "id": {
                "type": "string",
                "pattern": "^[A-Za-z0-9_-]{1,64}$",
                "description": "Id of a visualization you already published in this conversation; publishes its next revision."
            }
        },
        "required": ["title"]
    }))
    .expect("Visualization input schema is valid JSON")
});

static OUTPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    Utf16JsonProjection::from_serde_value(&json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "revision": { "type": "integer" },
            "title": { "type": "string" },
            "reference": { "type": "string" },
            "warnings": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["id", "revision", "title", "reference"]
    }))
    .expect("Visualization output schema is valid JSON")
});

/// Publish inline visualizations into the conversation's store.
pub struct VisualizationTool {
    ctx: BuiltinToolContext,
    store: Arc<VisualizationStore>,
}

impl VisualizationTool {
    /// The tool over `store`; registered only by hosts that render visualizations.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext, store: Arc<VisualizationStore>) -> Self {
        Self { ctx, store }
    }

    fn file_path(input: &Utf16JsonProjection) -> Option<&str> {
        input
            .string_at("/file_path")
            .filter(|path| !path.is_empty())
    }

    /// The host path a `file_path` names, translated and contained like `Read`.
    async fn resolve_file(&self, file_path: &str) -> Result<PathBuf, ToolError> {
        let (cwd, trusted) = self.ctx.cwd_and_trusted();
        let requested = resolve_against_cwd(PathBuf::from(file_path), &cwd);
        let path = translate_model_path(&self.ctx.fs, requested, false)
            .map_err(|message| ToolError::InvalidInput(message.into()))?;
        match canonicalize_and_validate(&path, &trusted) {
            Ok(canonical) => Ok(canonical),
            Err(_) => {
                emit_blocked_event(&self.ctx.bus, TOOL_NAME, &path).await;
                Err(ToolError::PathBlocked { path })
            }
        }
    }

    async fn read_fragment(&self, file_path: &str) -> Result<String, ToolError> {
        // Like the file tools, read the validated host path directly; the
        // FileSystem seam only translated and contained it above.
        let path = self.resolve_file(file_path).await?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|error| ToolError::Io(format!("Cannot read {file_path}: {error}").into()))?;
        if !metadata.is_file() {
            return Err(ToolError::InvalidInput(
                format!("{file_path} is not a regular file").into(),
            ));
        }
        if metadata.len() > MAX_FRAGMENT_BYTES as u64 {
            return Err(ToolError::FileTooLarge {
                size: metadata.len(),
                limit: MAX_FRAGMENT_BYTES as u64,
            });
        }
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|error| ToolError::Io(format!("Cannot read {file_path}: {error}").into()))?;
        String::from_utf8(bytes)
            .map_err(|_| ToolError::InvalidInput(format!("{file_path} is not UTF-8 text").into()))
    }

    /// The root session that owns what this call publishes.
    async fn owner(&self, ctx: &ToolUseContext) -> Option<uuid::Uuid> {
        root_session(&self.ctx, ctx).await
    }
}

/// The root session that owns a call's visualizations: a subagent's call
/// resolves to the conversation it works for.
pub(crate) async fn root_session(
    tool: &BuiltinToolContext,
    ctx: &ToolUseContext,
) -> Option<uuid::Uuid> {
    if let Some(origin) = ctx.origin_session_id {
        return Some(origin.as_uuid());
    }
    if let Some(session) = ctx.session.as_ref() {
        return Some(session.lock().await.session_id.as_uuid());
    }
    tool.session_id.map(|session| session.as_uuid())
}

#[async_trait]
impl Tool for VisualizationTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("show an interactive chart, simulation or widget inline in the chat".into())
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        &INPUT_SCHEMA
    }

    fn output_schema(&self) -> Option<&Utf16JsonProjection> {
        Some(&OUTPUT_SCHEMA)
    }

    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        16_000
    }

    fn is_concurrency_safe(&self, _input: &Utf16JsonProjection) -> bool {
        // Revisions of one id are serialized by the store; concurrent
        // publishes of different widgets are independent.
        true
    }

    fn is_read_only(&self, _input: &Utf16JsonProjection) -> bool {
        // Writes only the app-private visualization store, never user files.
        true
    }

    fn get_path(&self, input: &Utf16JsonProjection) -> Option<PathBuf> {
        Self::file_path(input).map(PathBuf::from)
    }

    async fn validate_input(
        &self,
        input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let html = input.string_at("/html");
        let file_path = Self::file_path(input);
        match (html, file_path) {
            (Some(_), Some(_)) => {
                return Err(ValidationError(
                    "Pass either html or file_path, not both.".into(),
                ));
            }
            (None, None) => {
                return Err(ValidationError(
                    "Pass the fragment in html, or its file in file_path.".into(),
                ));
            }
            (None, Some(path)) => {
                let is_html = std::path::Path::new(path)
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("html"));
                if !is_html {
                    return Err(ValidationError(
                        format!("{path} is not an .html file.").into(),
                    ));
                }
            }
            (Some(_), None) => {}
        }
        let title = input.string_at("/title").unwrap_or_default();
        visualization::store::normalize_title(title)
            .map_err(|error| ValidationError(error.to_string().into()))?;
        if let Some(id) = input.string_at("/id") {
            if VisualizationId::parse(id).is_none() {
                return Err(ValidationError(
                    format!("\"{id}\" is not a visualization id.").into(),
                ));
            }
        }
        Ok(())
    }

    async fn check_permissions(
        &self,
        input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> PermissionResult {
        // A file is read under exactly the rules `Read` would get; an inline
        // fragment only reaches the app's own store.
        if Self::file_path(input).is_some() {
            let path_source = input
                .subprojection("/file_path")
                .expect("file_path is a source member of the tool input");
            let read_input = Utf16JsonProjection::object(vec![("file_path".into(), path_source)])
                .expect("Read permission input is a valid source object");
            return self.ctx.permission_policy.authorize("Read", &read_input);
        }
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "publishes to the local visualization store".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(
        &self,
        _input: &Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        DESCRIPTION.to_string()
    }

    async fn prompt(&self, _opts: &PromptOptions) -> Utf16JsonProjection {
        PROMPT.into()
    }

    async fn parse_output(
        &self,
        output: &Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ToolError> {
        let invalid = || ToolError::InvalidInput("Visualization returned invalid output".into());
        let mut fields = Vec::with_capacity(5);
        for key in ["id", "revision", "title", "reference"] {
            let pointer = format!("/{key}");
            let Some(value) = output.subprojection(&pointer).ok() else {
                return Err(invalid());
            };
            match key {
                "id" | "title" | "reference" if output.string_at(&pointer).is_none() => {
                    return Err(invalid());
                }
                "revision" => {
                    let Some(revision) = output.number_at(&pointer) else {
                        return Err(invalid());
                    };
                    if !revision.is_finite() || revision.fract() != 0.0 {
                        return Err(invalid());
                    }
                }
                _ => {}
            }
            fields.push((key.to_owned(), value));
        }
        if let Some(warnings) = output.get_property("warnings") {
            let Some(count) = warnings.array_len() else {
                return Err(invalid());
            };
            for index in 0..count {
                if output.string_at(&format!("/warnings/{index}")).is_none() {
                    return Err(invalid());
                }
            }
            fields.push((
                "warnings".to_owned(),
                output.subprojection("/warnings").map_err(|_| invalid())?,
            ));
        }
        Utf16JsonProjection::object(fields).map_err(|_| invalid())
    }

    fn get_activity_description(&self, _input: &Utf16JsonProjection) -> Option<String> {
        Some("Publishing visualization".to_string())
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let title = input.string_at("/title").unwrap_or_default();
        let fragment = match (input.string_at("/html"), Self::file_path(&input)) {
            (Some(html), None) => html.to_string(),
            (None, Some(path)) => self.read_fragment(path).await?,
            _ => {
                return Err(ToolError::InvalidInput(
                    "Pass either html or file_path.".into(),
                ))
            }
        };
        let report = check_fragment(&fragment);
        if !report.is_publishable() {
            let mut message = String::from("The visualization was not published:\n");
            for error in &report.errors {
                message.push_str("- ");
                message.push_str(error);
                message.push('\n');
            }
            return Err(ToolError::InvalidInput(
                message.trim_end().to_string().into(),
            ));
        }
        let Some(root_session) = self.owner(&ctx).await else {
            return Err(ToolError::Internal(
                "this session has no id to store visualizations under".into(),
            ));
        };
        let id = input.string_at("/id").and_then(VisualizationId::parse);
        let now_ms = self
            .ctx
            .clock
            .now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
        let publisher = Publisher {
            root_session,
            agent_id: ctx.agent_id.map(|agent| agent.to_string()),
        };
        let published = self
            .store
            .publish(&publisher, id.as_ref(), title, &fragment, now_ms)
            .await
            .map_err(|error| match error {
                StoreError::Io(message) => ToolError::Io(message.into()),
                other => ToolError::InvalidInput(other.to_string().into()),
            })?;
        let reference = published.reference.reference_line();
        let mut model_content = format!(
            "Published \"{}\" as visualization {} revision {}. Write this line on its own line in your reply, exactly as shown:\n{reference}",
            published.title, published.reference.id, published.reference.revision
        );
        if !report.warnings.is_empty() {
            model_content.push_str("\n\nWarnings:");
            for warning in &report.warnings {
                model_content.push_str("\n- ");
                model_content.push_str(warning);
            }
        }
        let data = Utf16JsonProjection::from_serde_value(&json!({
            "id": published.reference.id.as_str(),
            "revision": published.reference.revision,
            "title": published.title,
            "reference": reference,
            "warnings": report.warnings,
        }))
        .expect("Visualization tool result is valid JSON");
        let result = ToolCallResult::from_data(data).with_model_text(&model_content);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobile_linux_api::ProcessOutput;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};

    fn source_input(value: serde_json::Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(&value).expect("test input is valid JSON")
    }

    fn tool(home: &std::path::Path) -> VisualizationTool {
        let ctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        let store = Arc::new(VisualizationStore::new(ctx.fs.clone(), home.to_path_buf()));
        VisualizationTool::new(ctx, store)
    }

    fn call_ctx(session: lingxi_core::types::SessionId) -> ToolUseContext {
        let mut ctx = fresh_ctx();
        ctx.origin_session_id = Some(session);
        ctx
    }

    #[tokio::test]
    async fn publishes_inline_fragments_and_revisions() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let session = lingxi_core::types::SessionId::new();
        let first = tool
            .call(
                source_input(json!({"title": "Growth", "html": "<div id=\"widget\">x</div>"})),
                call_ctx(session),
                fresh_tx(),
            )
            .await
            .unwrap();
        let id = first.data.string_at("/id").unwrap().to_string();
        assert_eq!(first.data.number_at("/revision"), Some(1.0));
        let reference = first.data.string_at("/reference").unwrap();
        assert_eq!(
            reference,
            format!("::lingxi-visualization{{id=\"{id}\" rev=\"1\"}}")
        );
        assert!(first
            .model_content
            .as_ref()
            .and_then(|content| content.string_at(""))
            .unwrap()
            .ends_with(reference));
        let second = tool
            .call(
                source_input(
                    json!({"title": "Growth v2", "html": "<div id=\"widget\">y</div>", "id": id}),
                ),
                call_ctx(session),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(second.data.number_at("/revision"), Some(2.0));
        assert_eq!(
            tool.store.list(session.as_uuid()).await.unwrap()[0]
                .revisions
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn rejected_fragments_explain_why_and_store_nothing() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let session = lingxi_core::types::SessionId::new();
        let error = tool
            .call(
                source_input(json!({"title": "t", "html": "<html><img src=\"https://x.test/a.png\"></html>"})),
                call_ctx(session),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        let message = error
            .model_facing_message()
            .string_at("")
            .unwrap()
            .to_string();
        assert!(message.contains("<html>"), "{message}");
        assert!(message.contains("external URLs"), "{message}");
        assert!(tool.store.list(session.as_uuid()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn warnings_ride_along_with_a_successful_publish() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let result = tool
            .call(
                source_input(json!({"title": "t", "html": "<script>localStorage.x = 1</script>"})),
                call_ctx(lingxi_core::types::SessionId::new()),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(result.data.array_len("/warnings"), Some(1));
        assert!(result
            .model_content
            .as_ref()
            .and_then(|content| content.string_at(""))
            .unwrap()
            .contains("Warnings:"));
    }

    #[tokio::test]
    async fn input_validation_requires_exactly_one_source() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let ctx = fresh_ctx();
        for value in [
            json!({"title": "t"}),
            json!({"title": "t", "html": "<p>x</p>", "file_path": "/a.html"}),
            json!({"title": "t", "file_path": "/a.svg"}),
            json!({"title": " ", "html": "<p>x</p>"}),
            json!({"title": "t", "html": "<p>x</p>", "id": "../x"}),
        ] {
            let input = source_input(value);
            assert!(
                tool.validate_input(&input, &ctx).await.is_err(),
                "{}",
                input.to_json_string().unwrap()
            );
        }
        let valid = source_input(json!({"title": "t", "file_path": "/tmp/a.HTML"}));
        assert!(tool.validate_input(&valid, &ctx).await.is_ok());
    }

    #[tokio::test]
    async fn unknown_ids_are_rejected_instead_of_minting_a_new_one() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let error = tool
            .call(
                source_input(json!({"title": "t", "html": "<p>x</p>", "id": "nope"})),
                call_ctx(lingxi_core::types::SessionId::new()),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(error
            .model_facing_message()
            .string_at("")
            .unwrap()
            .contains("omit id"));
    }

    #[tokio::test]
    async fn file_fragments_are_contained_and_size_checked() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut ctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        ctx.session_cwd = tool_api::SessionCwd::new(
            workspace.path().to_path_buf(),
            vec![workspace.path().to_path_buf()],
        );
        let store = Arc::new(VisualizationStore::new(
            ctx.fs.clone(),
            home.path().to_path_buf(),
        ));
        let tool = VisualizationTool::new(ctx, store);
        let session = lingxi_core::types::SessionId::new();
        std::fs::write(
            workspace.path().join("chart.html"),
            "<div id=\"widget\">file</div>",
        )
        .unwrap();
        let published = tool
            .call(
                source_input(json!({"title": "From file", "file_path": "chart.html"})),
                call_ctx(session),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(published.data.number_at("/revision"), Some(1.0));
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("x.html"), "<p>x</p>").unwrap();
        let outside_file = outside.path().join("x.html").to_string_lossy().into_owned();
        let blocked = tool
            .call(
                source_input(json!({"title": "t", "file_path": outside_file})),
                call_ctx(session),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(blocked, ToolError::PathBlocked { .. }),
            "{blocked:?}"
        );
        let big = workspace.path().join("big.html");
        std::fs::write(&big, "a".repeat(MAX_FRAGMENT_BYTES + 1)).unwrap();
        let too_large = tool
            .call(
                source_input(json!({"title": "t", "file_path": big.to_string_lossy()})),
                call_ctx(session),
                fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(too_large, ToolError::FileTooLarge { .. }));
    }

    #[test]
    fn name_and_description_stay_distinct_from_artifact() {
        assert_eq!(TOOL_NAME, "Visualization");
        assert_ne!(TOOL_NAME, crate::artifact::ARTIFACT_TOOL_NAME);
        assert!(DESCRIPTION.contains("Nothing is uploaded"));
        assert!(!PROMPT.contains("claude.ai"));
    }
}
