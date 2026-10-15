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
//! A visualization of the conversation becomes an app the same way
//! (`visualization_id`): its fragment is rendered into a standalone document
//! and the app starts with the visualization's confirmed state. Such a
//! fragment app is changed through its fragment (`fragment`).
//!
//! The structured result never carries the HTML: clients draw the card from
//! it, live and after a resume, so it stays a few fields long. Every publish
//! returns the same shape, so a saved visualization gets the same card.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use local_app::checks::{check_document, check_fragment, DocumentReport};
use local_app::store::{
    normalize_name, normalize_summary, LocalAppStore, NewRevision, Published, SourceFormat,
    StoreError, VisualizationOrigin, MAX_DOCUMENT_BYTES, MAX_NAME_CHARS, MAX_SUMMARY_CHARS,
};
use local_app::{LocalAppId, BOOT_PLACEHOLDER, TOOL_NAME};
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
use visualization::document::render_standalone_document;
use visualization::store::VisualizationStore;
use visualization::{VisualizationId, VisualizationRef};

use crate::inline_visualization::root_session;

/// Largest document `read` returns; bigger apps are changed from the project
/// they were built from.
pub const MAX_READ_BYTES: usize = 256 * 1024;

const DESCRIPTION: &str = "Publish a small app as one self-contained HTML document to this device's app library, where the user opens it full screen; also save a visualization from this conversation as an app, list the library, or read an app's current document. Nothing is uploaded.";

const PROMPT: &str = r#"Build and change the apps in this device's app library. An app is ONE self-contained HTML document that runs offline at its own origin and keeps its own localStorage and IndexedDB data. The user opens it from the card under this tool call or from the Apps page. Nothing is uploaded.

Load the `local-app` skill first unless you already have it in context: it covers the workflow, the offline and phone-first rules, stored data, and what a page cannot do.

- `action: "publish"` (the default) stores a new app, or a new revision of the app named by `id`. Pass the complete document in `html`, or `file_path` for an `.html` file you wrote. A new app needs a `name` (at most 60 characters); add a one-line `summary`. Omit both on a revision to keep the current ones.
- To keep a visualization from this conversation as an app, publish with its `visualization_id` instead of a document (add `visualization_revision` for an earlier revision). The app gets the visualization's code and starts with the state the user saved in it; `name` defaults to the visualization's title.
- An app saved from a visualization is a fragment app: `read` returns its fragment, and you publish a change with the app's `id` and the changed fragment in `fragment`.
- Give exactly one of `html`, `file_path`, `visualization_id` and `fragment`.
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
            },
            "visualization_id": {
                "type": "string",
                "pattern": "^[A-Za-z0-9_-]{1,64}$",
                "description": "Save this conversation's visualization with this id as an app, with the state the user saved in it. Omit html, file_path and fragment."
            },
            "visualization_revision": {
                "type": "integer",
                "minimum": 1,
                "description": "The visualization revision to save; the latest when omitted."
            },
            "fragment": {
                "type": "string",
                "description": "The changed fragment of an app saved from a visualization, published with its id instead of html."
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
    visualizations: Option<Arc<VisualizationStore>>,
}

impl LocalAppTool {
    /// The tool over `store`; registered only by hosts that can open apps.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext, store: Arc<LocalAppStore>) -> Self {
        Self {
            ctx,
            store,
            visualizations: None,
        }
    }

    /// Also save the conversation's visualizations from `store` as apps;
    /// for hosts that render visualizations too.
    #[must_use]
    pub fn with_visualizations(mut self, store: Arc<VisualizationStore>) -> Self {
        self.visualizations = Some(store);
        self
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

    async fn publish(
        &self,
        input: &Value,
        ctx: &ToolUseContext,
    ) -> Result<ToolCallResult, ToolError> {
        if let Some(id) = input.get("visualization_id").and_then(Value::as_str) {
            return self.save_visualization(input, id, ctx).await;
        }
        if let Some(fragment) = input.get("fragment").and_then(Value::as_str) {
            return self.publish_fragment(input, fragment).await;
        }
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
        reject_unpublishable(&report)?;
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
            .map_err(publish_error)?;
        let model_content = format!(
            "Published \"{}\" as local app {}, revision {}. It opens from the card under this tool call and from the Apps page. Tell the user; do not paste the HTML.",
            published.name, published.id, published.revision
        );
        Ok(published_result(
            &published,
            &report.warnings,
            model_content,
        ))
    }

    /// A changed fragment of a fragment app, rendered like the original.
    async fn publish_fragment(
        &self,
        input: &Value,
        fragment: &str,
    ) -> Result<ToolCallResult, ToolError> {
        let Some(id) = Self::id(input) else {
            return Err(ToolError::InvalidInput(
                "Pass the app's id with fragment.".into(),
            ));
        };
        let app = self.store.get(&id).await.map_err(store_error)?;
        if app.format != SourceFormat::Fragment {
            return Err(ToolError::InvalidInput(format!(
                "Local app {id} is a complete document, not a fragment: publish its changed document in html."
            )));
        }
        let report = check_fragment(fragment);
        reject_unpublishable(&report)?;
        let name = Self::name(input)?;
        let title = name.clone().unwrap_or(app.name);
        let document = render_standalone_document(&title, fragment, BOOT_PLACEHOLDER);
        let published = self
            .store
            .publish_revision(
                Some(&id),
                name.as_deref(),
                input.get("summary").and_then(Value::as_str),
                NewRevision {
                    document: &document,
                    fragment: Some(fragment),
                    saved_from: None,
                    initial_state: None,
                },
                self.now_ms(),
            )
            .await
            .map_err(publish_error)?;
        let model_content = format!(
            "Published the changed fragment of local app {} \"{}\" as revision {}. It opens from the card under this tool call and from the Apps page, and keeps its stored data. Tell the user; do not paste the HTML.",
            published.id, published.name, published.revision
        );
        Ok(published_result(
            &published,
            &report.warnings,
            model_content,
        ))
    }

    /// Save a visualization of this conversation as a new app, or as the
    /// next revision of the fragment app `id`.
    async fn save_visualization(
        &self,
        input: &Value,
        visualization_id: &str,
        ctx: &ToolUseContext,
    ) -> Result<ToolCallResult, ToolError> {
        let Some(visualizations) = &self.visualizations else {
            return Err(ToolError::InvalidInput(
                "This device has no visualizations to save.".into(),
            ));
        };
        let Some(visualization_id) = VisualizationId::parse(visualization_id) else {
            return Err(ToolError::InvalidInput(format!(
                "\"{visualization_id}\" is not a visualization id."
            )));
        };
        let Some(session) = root_session(&self.ctx, ctx).await else {
            return Err(ToolError::Internal(
                "this session has no id to find its visualizations under".into(),
            ));
        };
        let missing = || {
            ToolError::InvalidInput(format!(
                "This conversation has no visualization \"{visualization_id}\"; use the id in its reference line."
            ))
        };
        let revision = match input.get("visualization_revision").and_then(Value::as_u64) {
            Some(revision) => u32::try_from(revision).map_err(|_| missing())?,
            None => visualizations
                .list(session)
                .await
                .map_err(visualization_error)?
                .into_iter()
                .find(|item| item.id == visualization_id)
                .and_then(|item| item.revisions.last().map(|meta| meta.revision))
                .ok_or_else(missing)?,
        };
        let reference = VisualizationRef {
            id: visualization_id,
            revision,
        };
        let stored = visualizations
            .read_revision(session, &reference)
            .await
            .map_err(|_| {
                ToolError::InvalidInput(format!(
                    "This conversation has no revision {revision} of visualization \"{}\".",
                    reference.id
                ))
            })?;
        let state = visualizations
            .read_state(session, &reference)
            .await
            .map_err(visualization_error)?;
        let origin = VisualizationOrigin {
            root_session: session.to_string(),
            id: reference.id.to_string(),
            revision,
            state_version: state.version,
        };
        let target = Self::id(input);
        let current_name = match &target {
            Some(id) => {
                let app = self.store.get(id).await.map_err(store_error)?;
                if app.format != SourceFormat::Fragment {
                    return Err(ToolError::InvalidInput(format!(
                        "Local app {id} is a complete document; a visualization only replaces the code of an app saved from one. Omit id to save it as a new app."
                    )));
                }
                Some(app.name)
            }
            None => {
                if let Some(app) = self.store.find_saved(&origin).await.map_err(store_error)? {
                    let model_content = format!(
                        "Revision {revision} of visualization \"{}\" is already saved, unchanged, as local app {} \"{}\" (revision {}); nothing new was published. It opens from the card under this tool call and from the Apps page.",
                        stored.title, app.id, app.name, app.revision
                    );
                    let published = Published {
                        id: app.id,
                        revision: app.revision,
                        name: app.name,
                    };
                    return Ok(published_result(
                        &published,
                        &["This visualization revision was already saved; returning the existing app.".into()],
                        model_content,
                    ));
                }
                None
            }
        };
        let report = check_fragment(&stored.fragment);
        reject_unpublishable(&report)?;
        let name = match Self::name(input)? {
            Some(name) => Some(name),
            None if target.is_none() => Some(normalize_name(&stored.title).map_err(|_| {
                ToolError::InvalidInput(format!(
                    "The visualization's title is longer than {MAX_NAME_CHARS} characters; pass a name."
                ))
            })?),
            None => None,
        };
        let title = name
            .clone()
            .or(current_name)
            .unwrap_or_else(|| stored.title.clone());
        let document = render_standalone_document(&title, &stored.fragment, BOOT_PLACEHOLDER);
        let initial_state = match target {
            Some(_) => None,
            None => Some(
                serde_json::to_string(&state)
                    .map_err(|error| ToolError::Internal(error.to_string()))?,
            ),
        };
        let published = self
            .store
            .publish_revision(
                target.as_ref(),
                name.as_deref(),
                input.get("summary").and_then(Value::as_str),
                NewRevision {
                    document: &document,
                    fragment: Some(&stored.fragment),
                    saved_from: Some(origin),
                    initial_state: initial_state.as_deref(),
                },
                self.now_ms(),
            )
            .await
            .map_err(publish_error)?;
        let model_content = if target.is_some() {
            format!(
                "Replaced the code of local app {} \"{}\" with revision {revision} of visualization \"{}\", as app revision {}. The app keeps its stored data. It opens from the card under this tool call and from the Apps page. Tell the user; do not paste the HTML.",
                published.id, published.name, stored.title, published.revision
            )
        } else {
            format!(
                "Saved revision {revision} of visualization \"{}\" as local app {} \"{}\", starting with the state the user saved in it. It opens from the card under this tool call and from the Apps page, works after this conversation is gone, and keeps its own data from now on. Tell the user; do not paste the HTML.",
                stored.title, published.id, published.name
            )
        };
        Ok(published_result(
            &published,
            &report.warnings,
            model_content,
        ))
    }

    /// The normalized `name`, when given.
    fn name(input: &Value) -> Result<Option<String>, ToolError> {
        input
            .get("name")
            .and_then(Value::as_str)
            .map(normalize_name)
            .transpose()
            .map_err(|error| ToolError::InvalidInput(error.to_string()))
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
        if let Some((summary, fragment)) = self
            .store
            .read_current_fragment(&id)
            .await
            .map_err(store_error)?
        {
            if fragment.len() > MAX_READ_BYTES {
                return Err(ToolError::InvalidInput(format!(
                    "The fragment of local app {id} is {} bytes, more than the {MAX_READ_BYTES} bytes read returns.",
                    fragment.len()
                )));
            }
            let model_content = format!(
                "Local app {id} \"{}\", revision {}, was saved from a visualization. This is its fragment ({} bytes); the app wraps it in its document. Change it and publish with id \"{id}\" and the changed fragment in `fragment`.\n<local-app-fragment id=\"{id}\" revision=\"{}\">\n{}\n</local-app-fragment>",
                summary.name,
                summary.revision,
                fragment.len(),
                summary.revision,
                fragment
            );
            let mut result = ToolCallResult::from_data(json!({
                "action": "read",
                "id": id.as_str(),
                "name": summary.name,
                "revision": summary.revision,
                "bytes": fragment.len(),
                "format": "fragment",
            }));
            result.model_content = Some(model_content);
            return Ok(result);
        }
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

fn publish_error(error: StoreError) -> ToolError {
    match error {
        StoreError::NotFound(_) => {
            ToolError::InvalidInput(format!("{error}; omit id to publish a new app."))
        }
        other => store_error(other),
    }
}

fn visualization_error(error: visualization::StoreError) -> ToolError {
    match error {
        visualization::StoreError::Io(message) => ToolError::Io(message),
        other => ToolError::InvalidInput(other.to_string()),
    }
}

/// The tool error for a document or fragment the checks reject.
fn reject_unpublishable(report: &DocumentReport) -> Result<(), ToolError> {
    if report.is_publishable() {
        return Ok(());
    }
    let mut message = String::from("The app was not published:\n");
    for error in &report.errors {
        message.push_str("- ");
        message.push_str(error);
        message.push('\n');
    }
    Err(ToolError::InvalidInput(message.trim_end().to_string()))
}

/// The result of every publish: hosts draw the app's card from it.
fn published_result(
    published: &Published,
    warnings: &[String],
    mut model_content: String,
) -> ToolCallResult {
    if !warnings.is_empty() {
        model_content.push_str("\n\nWarnings:");
        for warning in warnings {
            model_content.push_str("\n- ");
            model_content.push_str(warning);
        }
    }
    let mut result = ToolCallResult::from_data(json!({
        "action": "publish",
        "id": published.id.as_str(),
        "revision": published.revision,
        "name": published.name,
        "warnings": warnings,
    }));
    result.model_content = Some(model_content);
    result
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
                let html = input.get("html").and_then(Value::as_str);
                let file_path = Self::file_path(input);
                let visualization_id = input.get("visualization_id").and_then(Value::as_str);
                let fragment = input.get("fragment").and_then(Value::as_str);
                let sources = [
                    html.is_some(),
                    file_path.is_some(),
                    visualization_id.is_some(),
                    fragment.is_some(),
                ];
                match sources.into_iter().filter(|given| *given).count() {
                    0 => {
                        return Err(ValidationError(
                            "Pass the document in html or its file in file_path, a visualization_id to save a visualization, or a fragment app's changed fragment."
                                .into(),
                        ));
                    }
                    1 => {}
                    _ => {
                        return Err(ValidationError(
                            "Pass exactly one of html, file_path, visualization_id and fragment."
                                .into(),
                        ));
                    }
                }
                if let Some(path) = file_path {
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
                if let Some(id) = visualization_id {
                    if VisualizationId::parse(id).is_none() {
                        return Err(ValidationError(format!(
                            "\"{id}\" is not a visualization id."
                        )));
                    }
                    if self.visualizations.is_none() {
                        return Err(ValidationError(
                            "This device has no visualizations to save.".into(),
                        ));
                    }
                }
                if let Some(revision) = input.get("visualization_revision") {
                    if visualization_id.is_none() {
                        return Err(ValidationError(
                            "visualization_revision goes with visualization_id.".into(),
                        ));
                    }
                    if !revision
                        .as_u64()
                        .is_some_and(|revision| (1..=u64::from(u32::MAX)).contains(&revision))
                    {
                        return Err(ValidationError(
                            "visualization_revision is a revision number, from 1.".into(),
                        ));
                    }
                }
                if fragment.is_some() && input.get("id").is_none() {
                    return Err(ValidationError(
                        "fragment changes an app saved from a visualization: pass that app's id. To save a visualization as a new app, pass its visualization_id."
                            .into(),
                    ));
                }
                match input.get("name").and_then(Value::as_str) {
                    Some(name) => {
                        normalize_name(name).map_err(|error| ValidationError(error.to_string()))?;
                    }
                    None if input.get("id").is_none() && visualization_id.is_none() => {
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
                _ if input.get("visualization_id").is_some() => "Saving visualization as an app",
                _ => "Publishing local app",
            }
            .to_string(),
        )
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        match Action::of(&input).map_err(ToolError::InvalidInput)? {
            Action::Publish => self.publish(&input, &ctx).await,
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
            json!({"visualization_id": "v1"}),
            json!({"name": "t", "html": DOC, "visualization_id": "v1"}),
            json!({"fragment": "<p>x</p>"}),
            json!({"name": "t", "html": DOC, "visualization_revision": 1}),
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

    #[tokio::test]
    async fn saving_visualizations_validates_its_inputs() {
        let home = tempfile::tempdir().unwrap();
        let (tool, _) = tool_with_visualizations(home.path());
        let ctx = fresh_ctx();
        for input in [
            json!({"visualization_id": "v1"}),
            json!({"visualization_id": "v1", "visualization_revision": 2, "name": "t"}),
            json!({"id": "a1", "visualization_id": "v1"}),
            json!({"id": "a1", "fragment": "<p>x</p>"}),
        ] {
            assert!(tool.validate_input(&input, &ctx).await.is_ok(), "{input}");
        }
        for input in [
            json!({"visualization_id": "not an id"}),
            json!({"visualization_id": "v1", "visualization_revision": 0}),
            json!({"visualization_id": "v1", "fragment": "<p>x</p>", "id": "a1"}),
        ] {
            assert!(tool.validate_input(&input, &ctx).await.is_err(), "{input}");
        }
    }

    const FRAGMENT: &str = "<div id=\"count\"></div><script>document.getElementById('count').textContent = lingxi.state.privateContent;</script>";

    fn tool_with_visualizations(home: &std::path::Path) -> (LocalAppTool, Arc<VisualizationStore>) {
        let ctx = quiet_ctx();
        let apps = Arc::new(LocalAppStore::new(ctx.fs.clone(), home.to_path_buf()));
        let visualizations = Arc::new(VisualizationStore::new(ctx.fs.clone(), home.to_path_buf()));
        (
            LocalAppTool::new(ctx, apps).with_visualizations(visualizations.clone()),
            visualizations,
        )
    }

    fn session_ctx(session: lingxi_core::types::SessionId) -> ToolUseContext {
        let mut ctx = fresh_ctx();
        ctx.origin_session_id = Some(session);
        ctx
    }

    async fn publish_visualization(
        visualizations: &VisualizationStore,
        session: lingxi_core::types::SessionId,
        id: Option<&VisualizationId>,
        title: &str,
        fragment: &str,
    ) -> VisualizationRef {
        let publisher = visualization::Publisher {
            root_session: session.as_uuid(),
            agent_id: None,
        };
        visualizations
            .publish(&publisher, id, title, fragment, 0)
            .await
            .unwrap()
            .reference
    }

    #[tokio::test]
    async fn saves_a_visualization_as_an_app_with_its_confirmed_state() {
        let home = tempfile::tempdir().unwrap();
        let (tool, visualizations) = tool_with_visualizations(home.path());
        let session = lingxi_core::types::SessionId::new();
        let reference =
            publish_visualization(&visualizations, session, None, "Counter", FRAGMENT).await;
        visualizations
            .write_state(
                session.as_uuid(),
                &reference,
                0,
                "\"shared\"",
                "\"t1-private-sentinel\"",
            )
            .await
            .unwrap();
        let saved = tool
            .call(
                json!({"visualization_id": reference.id.as_str()}),
                session_ctx(session),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            saved.data["action"], "publish",
            "the same card as any publish"
        );
        assert_eq!(saved.data["name"], "Counter");
        assert_eq!(saved.data["revision"], 1);
        assert!(!saved.data.to_string().contains("<script"));
        let id = LocalAppId::parse(saved.data["id"].as_str().unwrap()).unwrap();

        let stored = tool.store.read_current(&id).await.unwrap();
        assert_eq!(stored.summary.format, SourceFormat::Fragment);
        assert!(stored.html.contains(BOOT_PLACEHOLDER));
        assert!(stored.html.contains(FRAGMENT));
        assert!(
            !stored.html.contains("shared"),
            "the state stays out of the document"
        );
        let (_, source) = tool
            .store
            .read_current_fragment(&id)
            .await
            .unwrap()
            .unwrap();
        for text in [&stored.html, &source] {
            assert!(!text.contains("t1-private-sentinel"));
        }

        // The app outlives the conversation and starts with the saved state.
        visualizations
            .delete_session(session.as_uuid())
            .await
            .unwrap();
        let served = local_app::LocalAppService::new(tool.store.clone())
            .serve(id.as_str(), "/")
            .await;
        let served = String::from_utf8(served.body).unwrap();
        let boot = served
            .split("window.__lingxiAppBoot=")
            .nth(1)
            .and_then(|rest| rest.split(";</script>").next())
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(boot).unwrap(),
            json!({"version": 1, "modelContent": "shared", "privateContent": "t1-private-sentinel"})
        );

        let read = call(&tool, json!({"action": "read", "id": id.as_str()}))
            .await
            .unwrap();
        assert_eq!(read.data["format"], "fragment");
        assert!(read.model_content.unwrap().contains(FRAGMENT));
    }

    #[tokio::test]
    async fn saving_the_same_revision_again_returns_the_saved_app() {
        let home = tempfile::tempdir().unwrap();
        let (tool, visualizations) = tool_with_visualizations(home.path());
        let session = lingxi_core::types::SessionId::new();
        let first =
            publish_visualization(&visualizations, session, None, "Counter", FRAGMENT).await;
        let save = |input: Value| tool.call(input, session_ctx(session), fresh_tx());
        let one = save(json!({"visualization_id": first.id.as_str()}))
            .await
            .unwrap();
        let again = save(json!({"visualization_id": first.id.as_str()}))
            .await
            .unwrap();
        assert_eq!(again.data["id"], one.data["id"]);
        assert_eq!(again.data["revision"], 1);
        assert_eq!(again.data["warnings"].as_array().unwrap().len(), 1);
        assert!(again.model_content.unwrap().contains("already saved"));

        let second = publish_visualization(
            &visualizations,
            session,
            Some(&first.id),
            "Counter v2",
            "<p>v2</p>",
        )
        .await;
        assert_eq!(second.revision, 2);
        let newer = save(json!({"visualization_id": first.id.as_str()}))
            .await
            .unwrap();
        assert_ne!(
            newer.data["id"], one.data["id"],
            "the latest revision is a new app"
        );
        assert_eq!(newer.data["name"], "Counter v2");
        let older =
            save(json!({"visualization_id": first.id.as_str(), "visualization_revision": 1}))
                .await
                .unwrap();
        assert_eq!(older.data["id"], one.data["id"]);
        assert_eq!(tool.store.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn fragment_apps_change_through_their_fragment() {
        let home = tempfile::tempdir().unwrap();
        let (tool, visualizations) = tool_with_visualizations(home.path());
        let session = lingxi_core::types::SessionId::new();
        let reference =
            publish_visualization(&visualizations, session, None, "Counter", FRAGMENT).await;
        let save = |input: Value| tool.call(input, session_ctx(session), fresh_tx());
        let id = save(json!({"visualization_id": reference.id.as_str()}))
            .await
            .unwrap()
            .data["id"]
            .as_str()
            .unwrap()
            .to_string();

        let changed = call(&tool, json!({"id": id, "fragment": "<p>v2</p>"}))
            .await
            .unwrap();
        assert_eq!(
            (
                changed.data["revision"].as_u64(),
                changed.data["name"].as_str()
            ),
            (Some(2), Some("Counter"))
        );
        let read = call(&tool, json!({"action": "read", "id": id}))
            .await
            .unwrap();
        assert!(read.model_content.unwrap().contains("<p>v2</p>"));
        let wrapped = call(&tool, json!({"id": id, "fragment": DOC}))
            .await
            .unwrap_err();
        assert!(
            wrapped.model_facing_message().contains("<!doctype>"),
            "{}",
            wrapped.model_facing_message()
        );

        // A changed app is no longer "already saved": saving again replaces
        // its code only when asked to, by id.
        let replaced = save(json!({"id": id, "visualization_id": reference.id.as_str()}))
            .await
            .unwrap();
        assert_eq!(
            (
                replaced.data["id"].as_str(),
                replaced.data["revision"].as_u64()
            ),
            (Some(id.as_str()), Some(3))
        );
        assert!(replaced
            .model_content
            .unwrap()
            .contains("keeps its stored data"));

        let document = call(&tool, json!({"name": "Plain", "html": DOC}))
            .await
            .unwrap()
            .data["id"]
            .as_str()
            .unwrap()
            .to_string();
        for input in [
            json!({"id": document, "fragment": "<p>x</p>"}),
            json!({"id": document, "visualization_id": reference.id.as_str()}),
        ] {
            let error = save(input.clone()).await.unwrap_err();
            assert!(
                error.model_facing_message().contains("complete document"),
                "{input}"
            );
        }
    }

    #[tokio::test]
    async fn saving_from_a_subagent_uses_the_origin_conversation() {
        let home = tempfile::tempdir().unwrap();
        let (mut tool, visualizations) = tool_with_visualizations(home.path());
        let root = lingxi_core::types::SessionId::new();
        tool.ctx.session_id = Some(lingxi_core::types::SessionId::new());
        let reference =
            publish_visualization(&visualizations, root, None, "Root chart", FRAGMENT).await;
        let saved = tool
            .call(
                json!({"visualization_id": reference.id.as_str()}),
                session_ctx(root),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(saved.data["name"], "Root chart");
        assert_eq!(tool.store.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unknown_visualizations_and_long_titles_are_explained() {
        let home = tempfile::tempdir().unwrap();
        let (tool, visualizations) = tool_with_visualizations(home.path());
        let session = lingxi_core::types::SessionId::new();
        let save = |input: Value| tool.call(input, session_ctx(session), fresh_tx());
        let missing = save(json!({"visualization_id": "nope"})).await.unwrap_err();
        assert!(missing.model_facing_message().contains("no visualization"));
        let title = "T".repeat(MAX_NAME_CHARS + 1);
        let reference =
            publish_visualization(&visualizations, session, None, &title, FRAGMENT).await;
        let missing_revision =
            save(json!({"visualization_id": reference.id.as_str(), "visualization_revision": 9}))
                .await
                .unwrap_err();
        assert!(missing_revision
            .model_facing_message()
            .contains("no revision 9"));
        let long = save(json!({"visualization_id": reference.id.as_str()}))
            .await
            .unwrap_err();
        assert!(long.model_facing_message().contains("pass a name"));
        let named = save(json!({"visualization_id": reference.id.as_str(), "name": "Short"}))
            .await
            .unwrap();
        assert_eq!(named.data["name"], "Short");
        assert!(tool.store.list().await.unwrap().len() == 1);
    }

    #[test]
    fn name_and_description() {
        assert_eq!(TOOL_NAME, "LocalApp");
        assert_ne!(TOOL_NAME, visualization::TOOL_NAME);
        assert!(DESCRIPTION.contains("Nothing is uploaded"));
        assert!(PROMPT.contains("`local-app` skill"));
    }
}
