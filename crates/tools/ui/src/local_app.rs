//! `LocalApp` — publish, list and read the apps of this device's library.
//!
//! An app is ONE self-contained HTML document. Publishing checks it and stores
//! it as the app's next revision in the library under the config home; the
//! host shows a card under the tool call that opens the app full screen.
//! Nothing leaves the device. The document arrives inline (`html`) or from a
//! file the agent wrote (`file_path`), read like `Read` reads it: guest paths
//! are translated, the path must sit in a trusted directory, and `Read`
//! permission rules decide.
//!
//! The structured result never carries the HTML: clients draw the card from
//! it, live and after a resume, so it stays a few fields long.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use local_app::checks::check_document;
use local_app::store::{
    normalize_name, normalize_summary, LocalAppStore, StoreError, MAX_DOCUMENT_BYTES,
    MAX_NAME_CHARS, MAX_SUMMARY_CHARS,
};
use local_app::{LocalAppId, TOOL_NAME};
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};
use tool_api::util::path_validation::{
    canonicalize_and_validate, emit_blocked_event, resolve_against_cwd, translate_model_path,
};
use tool_api::BuiltinToolContext;

/// Largest document `read` returns; bigger apps are changed from the project
/// they were built from.
pub const MAX_READ_BYTES: usize = 256 * 1024;

const DESCRIPTION: &str = "Publish a small app as one self-contained HTML document to this device's app library, where the user opens it full screen; also list the library or read an app's current document. Nothing is uploaded.";

const PROMPT: &str = r#"Build and change the apps in this device's app library. An app is ONE self-contained HTML document that runs offline at its own origin and keeps its own localStorage and IndexedDB data. The user opens it from the card under this tool call or from the Apps page. Nothing is uploaded.

Load the `local-app` skill first unless you already have it in context: it covers the workflow, the offline and phone-first rules, stored data, and what a page cannot do.

- `action: "publish"` (the default) stores a new app, or a new revision of the app named by `id`. Pass the complete document in `html`, or `file_path` for an `.html` file you wrote; give exactly one. A new app needs a `name` (at most 60 characters); add a one-line `summary`. Omit both on a revision to keep the current ones.
- `action: "list"` shows every app in the library with its `id`.
- `action: "read"` with an `id` returns that app's current document, so you can change an app made in an earlier conversation.

After publishing, tell the user what the app does and where it opens. Never paste the HTML into your reply."#;

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": {
                "type": "string",
                "enum": ["publish", "list", "read"],
                "description": "publish (default): store a new app or a new revision. list: every app with its id. read: an app's current document."
            },
            "name": {
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_NAME_CHARS,
                "description": "The app's name, shown on its card and in the Apps page. Required for a new app; omit on a revision to keep it."
            },
            "summary": {
                "type": "string",
                "maxLength": MAX_SUMMARY_CHARS,
                "description": "One line on what the app does."
            },
            "html": {
                "type": "string",
                "description": "The complete HTML document. Omit when passing file_path."
            },
            "file_path": {
                "type": "string",
                "description": "Absolute path of an .html file holding the document. Omit when passing html."
            },
            "id": {
                "type": "string",
                "pattern": "^[a-z0-9]{1,32}$",
                "description": "An app's id: publish its next revision, or read it."
            }
        }
    })
});

static OUTPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "action": { "type": "string" },
            "id": { "type": "string" },
            "revision": { "type": "integer" },
            "name": { "type": "string" },
            "bytes": { "type": "integer" },
            "warnings": { "type": "array", "items": { "type": "string" } },
            "apps": { "type": "array", "items": { "type": "object" } }
        },
        "required": ["action"]
    })
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Publish,
    List,
    Read,
}

impl Action {
    fn of(input: &Value) -> Result<Self, String> {
        match input.get("action").and_then(Value::as_str) {
            None | Some("publish") => Ok(Self::Publish),
            Some("list") => Ok(Self::List),
            Some("read") => Ok(Self::Read),
            Some(other) => Err(format!(
                "\"{other}\" is not an action; use publish, list or read."
            )),
        }
    }
}

/// Publish, list and read the device's local apps.
pub struct LocalAppTool {
    ctx: BuiltinToolContext,
    store: Arc<LocalAppStore>,
}

impl LocalAppTool {
    /// The tool over `store`; registered only by hosts that can open apps.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext, store: Arc<LocalAppStore>) -> Self {
        Self { ctx, store }
    }

    fn file_path(input: &Value) -> Option<&str> {
        input
            .get("file_path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
    }

    fn id(input: &Value) -> Option<LocalAppId> {
        input
            .get("id")
            .and_then(Value::as_str)
            .and_then(LocalAppId::parse)
    }

    /// The host path a `file_path` names, translated and contained like `Read`.
    async fn resolve_file(&self, file_path: &str) -> Result<PathBuf, ToolError> {
        let (cwd, trusted) = self.ctx.cwd_and_trusted();
        let requested = resolve_against_cwd(PathBuf::from(file_path), &cwd);
        let path = translate_model_path(&self.ctx.fs, requested, false)
            .map_err(ToolError::InvalidInput)?;
        match canonicalize_and_validate(&path, &trusted) {
            Ok(canonical) => Ok(canonical),
            Err(_) => {
                emit_blocked_event(&self.ctx.bus, TOOL_NAME, &path).await;
                Err(ToolError::PathBlocked { path })
            }
        }
    }

    async fn read_document(&self, file_path: &str) -> Result<String, ToolError> {
        // Like the file tools, read the validated host path directly; the
        // FileSystem seam only translated and contained it above.
        let path = self.resolve_file(file_path).await?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|error| ToolError::Io(format!("Cannot read {file_path}: {error}")))?;
        if !metadata.is_file() {
            return Err(ToolError::InvalidInput(format!(
                "{file_path} is not a regular file"
            )));
        }
        if metadata.len() > MAX_DOCUMENT_BYTES as u64 {
            return Err(ToolError::FileTooLarge {
                size: metadata.len(),
                limit: MAX_DOCUMENT_BYTES as u64,
            });
        }
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|error| ToolError::Io(format!("Cannot read {file_path}: {error}")))?;
        String::from_utf8(bytes)
            .map_err(|_| ToolError::InvalidInput(format!("{file_path} is not UTF-8 text")))
    }

    fn now_ms(&self) -> u64 {
        self.ctx
            .clock
            .now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            })
    }

    async fn publish(&self, input: &Value) -> Result<ToolCallResult, ToolError> {
        let html = match (
            input.get("html").and_then(Value::as_str),
            Self::file_path(input),
        ) {
            (Some(html), None) => html.to_string(),
            (None, Some(path)) => self.read_document(path).await?,
            _ => {
                return Err(ToolError::InvalidInput(
                    "Pass either html or file_path.".into(),
                ))
            }
        };
        let report = check_document(&html);
        if !report.is_publishable() {
            let mut message = String::from("The app was not published:\n");
            for error in &report.errors {
                message.push_str("- ");
                message.push_str(error);
                message.push('\n');
            }
            return Err(ToolError::InvalidInput(message.trim_end().to_string()));
        }
        let published = self
            .store
            .publish(
                Self::id(input).as_ref(),
                input.get("name").and_then(Value::as_str),
                input.get("summary").and_then(Value::as_str),
                &html,
                self.now_ms(),
            )
            .await
            .map_err(|error| match error {
                StoreError::NotFound(_) => {
                    ToolError::InvalidInput(format!("{error}; omit id to publish a new app."))
                }
                other => store_error(other),
            })?;
        let mut model_content = format!(
            "Published \"{}\" as local app {}, revision {}. It opens from the card under this tool call and from the Apps page. Tell the user; do not paste the HTML.",
            published.name, published.id, published.revision
        );
        if !report.warnings.is_empty() {
            model_content.push_str("\n\nWarnings:");
            for warning in &report.warnings {
                model_content.push_str("\n- ");
                model_content.push_str(warning);
            }
        }
        let mut result = ToolCallResult::from_data(json!({
            "action": "publish",
            "id": published.id.as_str(),
            "revision": published.revision,
            "name": published.name,
            "warnings": report.warnings,
        }));
        result.model_content = Some(model_content);
        Ok(result)
    }

    async fn list(&self) -> Result<ToolCallResult, ToolError> {
        let apps = self.store.list().await.map_err(store_error)?;
        let model_content = if apps.is_empty() {
            "This device's app library is empty.".to_string()
        } else {
            let mut text = format!("{} apps, most recently updated first:", apps.len());
            for app in &apps {
                text.push_str(&format!(
                    "\n- {}: \"{}\", revision {}, {} bytes",
                    app.id, app.name, app.revision, app.bytes
                ));
                if !app.summary.is_empty() {
                    text.push_str(&format!(" ({})", app.summary));
                }
            }
            text
        };
        let mut result = ToolCallResult::from_data(json!({
            "action": "list",
            "apps": apps
                .iter()
                .map(|app| json!({
                    "id": app.id.as_str(),
                    "name": app.name,
                    "summary": app.summary,
                    "revision": app.revision,
                    "updatedAtMs": app.updated_at_ms,
                }))
                .collect::<Vec<_>>(),
        }));
        result.model_content = Some(model_content);
        Ok(result)
    }

    async fn read(&self, input: &Value) -> Result<ToolCallResult, ToolError> {
        let Some(id) = Self::id(input) else {
            return Err(ToolError::InvalidInput(
                "read needs the app's id; call with action \"list\" to find it.".into(),
            ));
        };
        let app = self.store.read_current(&id).await.map_err(store_error)?;
        let summary = app.summary;
        if app.html.len() > MAX_READ_BYTES {
            return Err(ToolError::InvalidInput(format!(
                "Local app {id} is {} bytes, more than the {MAX_READ_BYTES} bytes read returns. Change it in the project it was built from and publish the new build with id \"{id}\".",
                app.html.len()
            )));
        }
        let model_content = format!(
            "Local app {id} \"{}\", revision {} ({} bytes). Change it and publish with id \"{id}\".\n<local-app-document id=\"{id}\" revision=\"{}\">\n{}\n</local-app-document>",
            summary.name, summary.revision, summary.bytes, summary.revision, app.html
        );
        let mut result = ToolCallResult::from_data(json!({
            "action": "read",
            "id": id.as_str(),
            "name": summary.name,
            "revision": summary.revision,
            "bytes": summary.bytes,
        }));
        result.model_content = Some(model_content);
        Ok(result)
    }
}

fn store_error(error: StoreError) -> ToolError {
    match error {
        StoreError::Io(message) => ToolError::Io(message),
        other => ToolError::InvalidInput(other.to_string()),
    }
}

#[async_trait]
impl Tool for LocalAppTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn search_hint(&self) -> Option<&str> {
        Some("build a small app the user keeps and opens on this device")
    }

    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }

    fn output_schema(&self) -> Option<&Value> {
        Some(&OUTPUT_SCHEMA)
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        // `read` returns a whole document of up to MAX_READ_BYTES.
        300_000
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        // The store serializes its writers.
        true
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        // Writes only the app-private library, never user files.
        true
    }

    fn get_path(&self, input: &Value) -> Option<PathBuf> {
        Self::file_path(input).map(PathBuf::from)
    }

    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let action = Action::of(input).map_err(ValidationError)?;
        if let Some(id) = input.get("id").and_then(Value::as_str) {
            if LocalAppId::parse(id).is_none() {
                return Err(ValidationError(format!("\"{id}\" is not a local app id.")));
            }
        }
        match action {
            Action::List => Ok(()),
            Action::Read => {
                if input.get("id").is_none() {
                    return Err(ValidationError(
                        "read needs the app's id; call with action \"list\" to find it.".into(),
                    ));
                }
                Ok(())
            }
            Action::Publish => {
                match (
                    input.get("html").and_then(Value::as_str),
                    Self::file_path(input),
                ) {
                    (Some(_), Some(_)) => {
                        return Err(ValidationError(
                            "Pass either html or file_path, not both.".into(),
                        ));
                    }
                    (None, None) => {
                        return Err(ValidationError(
                            "Pass the document in html, or its file in file_path.".into(),
                        ));
                    }
                    (None, Some(path)) => {
                        let is_html = std::path::Path::new(path)
                            .extension()
                            .and_then(|extension| extension.to_str())
                            .is_some_and(|extension| {
                                extension.eq_ignore_ascii_case("html")
                                    || extension.eq_ignore_ascii_case("htm")
                            });
                        if !is_html {
                            return Err(ValidationError(format!("{path} is not an .html file.")));
                        }
                    }
                    (Some(_), None) => {}
                }
                match input.get("name").and_then(Value::as_str) {
                    Some(name) => {
                        normalize_name(name).map_err(|error| ValidationError(error.to_string()))?;
                    }
                    None if input.get("id").is_none() => {
                        return Err(ValidationError(
                            "A new app needs a name; pass id instead to publish a revision of an existing app."
                                .into(),
                        ));
                    }
                    None => {}
                }
                if let Some(summary) = input.get("summary").and_then(Value::as_str) {
                    normalize_summary(summary)
                        .map_err(|error| ValidationError(error.to_string()))?;
                }
                Ok(())
            }
        }
    }

    async fn check_permissions(&self, input: &Value, _ctx: &ToolUseContext) -> PermissionResult {
        // A file is read under exactly the rules `Read` would get; everything
        // else only reaches the app's own library.
        if Action::of(input) == Ok(Action::Publish) {
            if let Some(path) = Self::file_path(input) {
                return self
                    .ctx
                    .permission_policy
                    .authorize("Read", &json!({ "file_path": path }));
            }
        }
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "uses the local app library".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _input: &Value, _opts: &DescriptionOptions) -> String {
        DESCRIPTION.to_string()
    }

    async fn prompt(&self, _opts: &PromptOptions) -> String {
        PROMPT.to_string()
    }

    fn get_activity_description(&self, input: &Value) -> Option<String> {
        Some(
            match Action::of(input) {
                Ok(Action::List) => "Listing local apps",
                Ok(Action::Read) => "Reading local app",
                _ => "Publishing local app",
            }
            .to_string(),
        )
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        match Action::of(&input).map_err(ToolError::InvalidInput)? {
            Action::Publish => self.publish(&input).await,
            Action::List => self.list().await,
            Action::Read => self.read(&input).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobile_linux_api::ProcessOutput;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};

    const DOC: &str = "<!doctype html><html><head><title>t</title></head><body>x</body></html>";

    fn quiet_ctx() -> BuiltinToolContext {
        shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        })
    }

    fn tool(home: &std::path::Path) -> LocalAppTool {
        let ctx = quiet_ctx();
        let store = Arc::new(LocalAppStore::new(ctx.fs.clone(), home.to_path_buf()));
        LocalAppTool::new(ctx, store)
    }

    async fn call(tool: &LocalAppTool, input: Value) -> Result<ToolCallResult, ToolError> {
        tool.call(input, fresh_ctx(), fresh_tx()).await
    }

    #[tokio::test]
    async fn publishes_new_apps_and_revisions_with_a_small_result() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let first = call(
            &tool,
            json!({"name": "Water", "summary": "Log glasses", "html": DOC}),
        )
        .await
        .unwrap();
        assert_eq!(first.data["action"], "publish");
        assert_eq!(first.data["revision"], 1);
        assert_eq!(first.data["name"], "Water");
        let id = first.data["id"].as_str().unwrap().to_string();
        assert!(
            !first.data.to_string().contains("<!doctype"),
            "the client-state result never carries the document"
        );
        assert!(first.model_content.as_deref().unwrap().contains(&id));
        let second = call(&tool, json!({"html": DOC, "id": id})).await.unwrap();
        assert_eq!(
            (
                second.data["revision"].as_u64(),
                second.data["name"].as_str()
            ),
            (Some(2), Some("Water"))
        );
    }

    #[tokio::test]
    async fn list_and_read_return_the_library() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let empty = call(&tool, json!({"action": "list"})).await.unwrap();
        assert_eq!(empty.data["apps"].as_array().unwrap().len(), 0);
        let id = call(&tool, json!({"name": "Timer", "html": DOC}))
            .await
            .unwrap()
            .data["id"]
            .as_str()
            .unwrap()
            .to_string();
        let listed = call(&tool, json!({"action": "list"})).await.unwrap();
        assert_eq!(listed.data["apps"][0]["id"], id.as_str());
        assert!(listed.model_content.unwrap().contains("\"Timer\""));
        let read = call(&tool, json!({"action": "read", "id": id}))
            .await
            .unwrap();
        assert_eq!(read.data["action"], "read");
        assert_eq!(read.data["bytes"], DOC.len());
        assert!(read.model_content.unwrap().contains(DOC));
        assert!(!read.data.to_string().contains("<!doctype"));
    }

    #[tokio::test]
    async fn rejected_documents_explain_why_and_store_nothing() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let error = call(&tool, json!({"name": "t", "html": "<div>fragment</div>"}))
            .await
            .unwrap_err();
        assert!(
            error
                .model_facing_message()
                .contains("complete HTML document"),
            "{}",
            error.model_facing_message()
        );
        assert!(tool.store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn warnings_ride_along_with_a_successful_publish() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let result = call(
            &tool,
            json!({"name": "t", "html": "<!doctype html><script src=\"https://cdn.test/x.js\"></script>"}),
        )
        .await
        .unwrap();
        assert_eq!(result.data["warnings"].as_array().unwrap().len(), 1);
        assert!(result.model_content.unwrap().contains("Warnings:"));
    }

    #[tokio::test]
    async fn input_validation() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let ctx = fresh_ctx();
        for input in [
            json!({"name": "t"}),
            json!({"html": DOC}),
            json!({"name": "t", "html": DOC, "file_path": "/a.html"}),
            json!({"name": "t", "file_path": "/a.svg"}),
            json!({"name": " ", "html": DOC}),
            json!({"name": "t", "html": DOC, "id": "../x"}),
            json!({"name": "t", "html": DOC, "summary": "s".repeat(MAX_SUMMARY_CHARS + 1)}),
            json!({"action": "read"}),
            json!({"action": "delete", "id": "a1"}),
        ] {
            assert!(tool.validate_input(&input, &ctx).await.is_err(), "{input}");
        }
        for input in [
            json!({"name": "t", "file_path": "/tmp/a.HTML"}),
            json!({"id": "a1", "html": DOC}),
            json!({"action": "list"}),
            json!({"action": "read", "id": "a1"}),
        ] {
            assert!(tool.validate_input(&input, &ctx).await.is_ok(), "{input}");
        }
    }

    #[tokio::test]
    async fn unknown_ids_are_rejected_instead_of_minting_a_new_app() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let error = call(&tool, json!({"html": DOC, "id": "nope"}))
            .await
            .unwrap_err();
        assert!(error.model_facing_message().contains("omit id"));
        let missing = call(&tool, json!({"action": "read", "id": "nope"}))
            .await
            .unwrap_err();
        assert!(missing
            .model_facing_message()
            .contains("not in this device"));
    }

    #[tokio::test]
    async fn oversized_apps_are_not_returned_by_read() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let big = format!("<!doctype html>{}", "a".repeat(MAX_READ_BYTES));
        let id = tool
            .store
            .publish(None, Some("Big"), None, &big, 0)
            .await
            .unwrap()
            .id;
        let error = call(&tool, json!({"action": "read", "id": id.as_str()}))
            .await
            .unwrap_err();
        assert!(error.model_facing_message().contains("built from"));
    }

    #[tokio::test]
    async fn file_documents_are_contained_and_size_checked() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let mut ctx = quiet_ctx();
        ctx.session_cwd = tool_api::SessionCwd::new(
            workspace.path().to_path_buf(),
            vec![workspace.path().to_path_buf()],
        );
        let store = Arc::new(LocalAppStore::new(
            ctx.fs.clone(),
            home.path().to_path_buf(),
        ));
        let tool = LocalAppTool::new(ctx, store);
        std::fs::write(workspace.path().join("app.html"), DOC).unwrap();
        let published = call(&tool, json!({"name": "From file", "file_path": "app.html"}))
            .await
            .unwrap();
        assert_eq!(published.data["revision"], 1);
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("x.html"), DOC).unwrap();
        let blocked = call(
            &tool,
            json!({"name": "t", "file_path": outside.path().join("x.html")}),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(blocked, ToolError::PathBlocked { .. }),
            "{blocked:?}"
        );
        let big = workspace.path().join("big.html");
        std::fs::write(&big, "a".repeat(MAX_DOCUMENT_BYTES + 1)).unwrap();
        let too_large = call(&tool, json!({"name": "t", "file_path": big}))
            .await
            .unwrap_err();
        assert!(matches!(too_large, ToolError::FileTooLarge { .. }));
    }

    #[test]
    fn name_and_description() {
        assert_eq!(TOOL_NAME, "LocalApp");
        assert_ne!(TOOL_NAME, visualization::TOOL_NAME);
        assert!(DESCRIPTION.contains("Nothing is uploaded"));
        assert!(PROMPT.contains("`local-app` skill"));
    }
}
