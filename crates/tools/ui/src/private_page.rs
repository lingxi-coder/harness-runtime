//! `Page` — publish an HTML fragment as a private page on this computer.
//!
//! LingXi opens the page. Nothing is uploaded and there is no public URL.
//! This tool is unrelated to inline `Visualization` and to the disabled
//! `Artifact` tool.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use private_page::checks::{check_fragment, MAX_FRAGMENT_BYTES};
use private_page::store::{normalize_title, PageStore, StoreError, MAX_TITLE_CHARS};
use private_page::{PageId, TOOL_NAME};
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

const DESCRIPTION: &str = "Publish a private HTML page on this computer. LingXi opens it. Nothing is uploaded and there is no public URL. Returns a reference line to place in your reply.";

const PROMPT: &str = r#"Publish a self-contained HTML fragment as a private page on this computer. LingXi opens the page in the app. Nothing is uploaded and there is no public URL.

Load the `artifact-design` skill first unless you already have it in context. A plan or RFC page also follows `plan-artifact`. Do not call `Visualization` to publish or stand in for this page.

To list private pages already on this computer, call this tool with `action` set to `list`. You may also pass `limit`. Pass no other field. Pass one of those ids to publish its next revision.

- Pass the fragment in `html`, or pass `file_path` for a fragment you wrote to an `.html` file. Give exactly one.
- The fragment has no `<!doctype>`, `<html>`, `<head>` or `<body>`. Keep CSS and JavaScript inline. Do not load a network asset. The fragment is at most 2 MiB.
- `title` names the page (at most 80 characters).
- To revise a page, pass its `id`. Each publish creates a new immutable revision.

After a successful publish, write the returned reference line on its own line in your reply, exactly as given and outside any code block. Without it the page shows only behind this tool call. Do not invent a URL or a share link."#;

const MAX_LIST: usize = 100;

static INPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    Utf16JsonProjection::from_serde_value(&json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": {
                "type": "string",
                "enum": ["publish", "list"],
                "description": "Omit or pass publish to store a fragment. Pass list to enumerate private pages on this computer."
            },
            "title": {
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_TITLE_CHARS,
                "description": "Short title for the page. Required to publish."
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
                "description": "Id of a private page already on this computer; publishes its next revision."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_LIST,
                "description": "How many pages list returns, newest first. List only."
            }
        },
        "required": []
    }))
    .expect("Page input schema is valid JSON")
});

static OUTPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    Utf16JsonProjection::from_serde_value(&json!({
        "anyOf": [
            {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "revision": { "type": "integer" },
                    "title": { "type": "string" },
                    "reference": { "type": "string" },
                    "warnings": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["id", "revision", "title", "reference"]
            },
            {
                "type": "object",
                "properties": {
                    "pages": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "revision": { "type": "integer" },
                                "title": { "type": "string" }
                            },
                            "required": ["id", "revision", "title"]
                        }
                    }
                },
                "required": ["pages"]
            }
        ]
    }))
    .expect("Page output schema is valid JSON")
});

/// Publish private pages into the computer's store.
pub struct PageTool {
    ctx: BuiltinToolContext,
    store: Arc<PageStore>,
}

impl PageTool {
    /// The tool over `store`; registered only by hosts that can open the page.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext, store: Arc<PageStore>) -> Self {
        Self { ctx, store }
    }

    fn file_path(input: &Utf16JsonProjection) -> Option<&str> {
        input
            .string_at("/file_path")
            .filter(|path| !path.is_empty())
    }

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

    fn limit(input: &Utf16JsonProjection) -> Result<usize, ValidationError> {
        match input.number_at("/limit") {
            None => Ok(50),
            Some(limit) if limit.fract() == 0.0 && (1.0..=MAX_LIST as f64).contains(&limit) => {
                Ok(limit as usize)
            }
            Some(_) => Err(ValidationError(
                format!("limit must be an integer from 1 to {MAX_LIST}.").into(),
            )),
        }
    }

    async fn list_published(
        &self,
        input: &Utf16JsonProjection,
    ) -> Result<ToolCallResult, ToolError> {
        let limit = Self::limit(input).map_err(|error| ToolError::InvalidInput(error.0))?;
        let mut items = self.store.list().await.map_err(store_error)?;
        items.sort_by(|left, right| {
            let left_at = left.revisions.last().map_or(0, |rev| rev.created_at_ms);
            let right_at = right.revisions.last().map_or(0, |rev| rev.created_at_ms);
            right_at.cmp(&left_at).then_with(|| left.id.cmp(&right.id))
        });
        let truncated = items.len() > limit;
        items.truncate(limit);
        let rows = items
            .iter()
            .filter_map(|item| {
                let latest = item.revisions.last()?;
                Some(json!({
                    "id": item.id.as_str(),
                    "revision": latest.revision,
                    "title": latest.title,
                }))
            })
            .collect::<Vec<_>>();
        let mut model_content = if rows.is_empty() {
            "This computer has no private pages.".to_string()
        } else {
            let mut text = String::from("Private pages on this computer, latest revision of each:");
            for row in &rows {
                text.push_str(&format!(
                    "\n- {} revision {}: {}",
                    row["id"].as_str().unwrap_or(""),
                    row["revision"],
                    row["title"].as_str().unwrap_or("")
                ));
            }
            text
        };
        if truncated {
            model_content.push_str("\nThe list was truncated. Pass a larger limit to see more.");
        }
        let data = Utf16JsonProjection::from_serde_value(&json!({ "pages": rows }))
            .expect("Page list is valid JSON");
        Ok(ToolCallResult::from_data(data).with_model_text(&model_content))
    }
}

fn store_error(error: StoreError) -> ToolError {
    match error {
        StoreError::Io(message) => ToolError::Io(message.into()),
        other => ToolError::InvalidInput(other.to_string().into()),
    }
}

#[async_trait]
impl Tool for PageTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("publish a private page on this computer that LingXi opens".into())
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
        true
    }

    fn is_read_only(&self, _input: &Utf16JsonProjection) -> bool {
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
        let action = input.string_at("/action").unwrap_or("publish");
        if action == "list" {
            if input.string_at("/html").is_some()
                || Self::file_path(input).is_some()
                || input.string_at("/id").is_some()
                || input.string_at("/title").is_some()
            {
                return Err(ValidationError(
                    "list takes no other fields except limit.".into(),
                ));
            }
            Self::limit(input)?;
            return Ok(());
        }
        if action != "publish" {
            return Err(ValidationError("action must be publish or list.".into()));
        }
        if input.number_at("/limit").is_some() {
            return Err(ValidationError("limit is only valid with list.".into()));
        }
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
        normalize_title(title).map_err(|error| ValidationError(error.to_string().into()))?;
        if let Some(id) = input.string_at("/id") {
            if PageId::parse(id).is_none() {
                return Err(ValidationError(
                    format!("\"{id}\" is not a page id.").into(),
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
                reason: "publishes to the local private-page store".into(),
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

    fn get_activity_description(&self, input: &Utf16JsonProjection) -> Option<String> {
        if input.string_at("/action") == Some("list") {
            Some("Listing private pages".to_string())
        } else {
            Some("Publishing private page".to_string())
        }
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        if input.string_at("/action") == Some("list") {
            return self.list_published(&input).await;
        }
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
            let mut message = String::from("The private page was not published:\n");
            for error in &report.errors {
                message.push_str("- ");
                message.push_str(error);
                message.push('\n');
            }
            return Err(ToolError::InvalidInput(
                message.trim_end().to_string().into(),
            ));
        }
        let id = input.string_at("/id").and_then(PageId::parse);
        let now_ms = self
            .ctx
            .clock
            .now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
        let published = self
            .store
            .publish(id.as_ref(), title, &fragment, now_ms)
            .await
            .map_err(store_error)?;
        let reference = published.reference.reference_line();
        let mut model_content = format!(
            "Published \"{}\" as private page {} revision {} on this computer. LingXi opens it. There is no public URL. Write this line on its own line in your reply, exactly as shown:\n{reference}",
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
        .expect("Page tool result is valid JSON");
        Ok(ToolCallResult::from_data(data).with_model_text(&model_content))
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

    fn tool(home: &std::path::Path) -> PageTool {
        let ctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        let store = Arc::new(PageStore::new(ctx.fs.clone(), home.to_path_buf()));
        PageTool::new(ctx, store)
    }

    #[tokio::test]
    async fn publishes_a_private_page_without_a_url() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let result = tool
            .call(
                source_input(json!({"title": "Notes", "html": "<p>Hello</p>"})),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .unwrap();
        let reference = result.data.string_at("/reference").unwrap();
        assert!(reference.starts_with("::lingxi-page{id=\""));
        assert!(reference.contains("rev=\"1\""));
        let text = result
            .model_content
            .as_ref()
            .and_then(|content| content.string_at(""))
            .unwrap();
        assert!(text.contains("There is no public URL"));
        assert!(!text.contains("http"));
        assert!(tool
            .call(
                source_input(json!({"title": "Shell", "html": "<!DOCTYPE html><body>x</body>"})),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .is_err());
    }
}
