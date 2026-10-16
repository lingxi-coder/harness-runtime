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

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;

use async_trait::async_trait;
use lingxi_core::host::permission_gate::{PermissionCheckContext, PermissionResolution};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
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
    generated_builtin_result, generated_builtin_schema, source_text_projection, DescriptionOptions,
    PromptOptions, Tool, ToolCallResult, ToolError, ToolInputRepairMetadata, ToolInputRepairSchema,
    ToolInputSchemaProvenance, ToolStaticContext, ValidationError,
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

struct ApprovedLocalAppFile {
    requested: PathBuf,
    canonical: PathBuf,
    root: PathBuf,
    relative: PathBuf,
}

type LocalAppApprovalKey = (String, PathBuf);

/// Bound snapshots left behind when a user rejects an Ask or a call is canceled.
const MAX_PENDING_LOCAL_APP_APPROVALS: usize = 256;

#[derive(Default)]
struct PendingLocalAppApprovals {
    by_key: HashMap<LocalAppApprovalKey, ApprovedLocalAppFile>,
    fifo: VecDeque<LocalAppApprovalKey>,
}

impl PendingLocalAppApprovals {
    fn remove(&mut self, key: &LocalAppApprovalKey) -> Option<ApprovedLocalAppFile> {
        self.fifo.retain(|queued| queued != key);
        self.by_key.remove(key)
    }

    fn insert(&mut self, key: LocalAppApprovalKey, approval: ApprovedLocalAppFile) {
        // A repeated permission check for the same physical tool use replaces
        // its older target and refreshes its FIFO position.
        self.remove(&key);
        while self.by_key.len() >= MAX_PENDING_LOCAL_APP_APPROVALS {
            let Some(oldest) = self.fifo.pop_front() else {
                break;
            };
            self.by_key.remove(&oldest);
        }
        self.fifo.push_back(key.clone());
        self.by_key.insert(key, approval);
    }
}

fn rooted_location(approved: &Path, trusted_dirs: &[PathBuf]) -> Option<(PathBuf, PathBuf)> {
    trusted_dirs
        .iter()
        .filter_map(|root| {
            let root = std::fs::canonicalize(root).ok()?;
            if !approved.starts_with(&root) {
                return None;
            }
            let relative = approved.strip_prefix(&root).ok()?.to_path_buf();
            Some((root, relative))
        })
        .max_by_key(|(root, _)| root.components().count())
}

fn is_rooted_path_block(error: &lingxi_core::host::rooted_fs::RootedFsError) -> bool {
    use lingxi_core::host::rooted_fs::{FsError, RootedFsError};

    matches!(
        error,
        RootedFsError::SymlinkResolutionChanged
            | RootedFsError::ParentSymlinkResolutionChanged
            | RootedFsError::LeafSymlink
            | RootedFsError::Fs(FsError::OutsideWorkspace(_))
    )
}

static INPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    generated_builtin_schema(&json!({
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
    }))
});

static OUTPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    generated_builtin_schema(&json!({
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
    }))
});

/// LocalApp's existing business logic consumes serde values. Convert only at
/// that boundary; the Tool API and static schemas stay source-backed.
fn input_value(input: &Utf16JsonProjection) -> Result<Value, String> {
    let source = input.to_json_string().map_err(|error| error.to_string())?;
    serde_json::from_str(&source).map_err(|error| error.to_string())
}

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
    /// One-shot pre-Read target identities, keyed by tool use and lexical path.
    pending_file_approvals: Mutex<PendingLocalAppApprovals>,
    visualizations: Option<Arc<VisualizationStore>>,
}

impl LocalAppTool {
    /// The tool over `store`; registered only by hosts that can open apps.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext, store: Arc<LocalAppStore>) -> Self {
        Self {
            ctx,
            store,
            pending_file_approvals: Mutex::new(PendingLocalAppApprovals::default()),
            visualizations: None,
        }
    }

    fn requested_file_path(&self, file_path: &str) -> Result<(PathBuf, Vec<PathBuf>), ToolError> {
        let (cwd, trusted) = self.ctx.cwd_and_trusted();
        let requested = resolve_against_cwd(PathBuf::from(file_path), &cwd);
        let requested = translate_model_path(&self.ctx.fs, requested, false)
            .map_err(|message| ToolError::InvalidInput(message.into()))?;
        Ok((requested, trusted))
    }

    fn approval_key(ctx: &ToolUseContext, requested: &Path) -> Option<LocalAppApprovalKey> {
        Some((
            ctx.tool_use_id.as_ref()?.as_str().to_owned(),
            requested.to_path_buf(),
        ))
    }

    fn remember_approval(&self, key: LocalAppApprovalKey, approval: ApprovedLocalAppFile) {
        self.pending_file_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, approval);
    }

    fn discard_approval(&self, key: &LocalAppApprovalKey) {
        self.pending_file_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
    }

    fn take_approval(&self, key: &LocalAppApprovalKey) -> Option<ApprovedLocalAppFile> {
        self.pending_file_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key)
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
    async fn resolve_file(&self, file_path: &str) -> Result<ApprovedLocalAppFile, ToolError> {
        let (requested, trusted) = self.requested_file_path(file_path)?;
        match canonicalize_and_validate(&requested, &trusted) {
            Ok(canonical) => {
                let Some((root, relative)) = rooted_location(&canonical, &trusted) else {
                    emit_blocked_event(&self.ctx.bus, TOOL_NAME, &requested).await;
                    return Err(ToolError::PathBlocked { path: requested });
                };
                Ok(ApprovedLocalAppFile {
                    requested,
                    canonical,
                    root,
                    relative,
                })
            }
            Err(_) => {
                emit_blocked_event(&self.ctx.bus, TOOL_NAME, &requested).await;
                Err(ToolError::PathBlocked { path: requested })
            }
        }
    }

    async fn read_document(&self, file_path: &str) -> Result<String, ToolError> {
        let approved = self.resolve_file(file_path).await?;
        self.read_approved_document(file_path, &approved).await
    }

    async fn read_approved_document(
        &self,
        file_path: &str,
        approved: &ApprovedLocalAppFile,
    ) -> Result<String, ToolError> {
        let file = match lingxi_core::host::rooted_fs::open_file_after_permission(
            &approved.root,
            &approved.relative,
            &approved.requested,
            &approved.canonical,
        ) {
            Ok(file) => file,
            Err(error) if is_rooted_path_block(&error) => {
                emit_blocked_event(&self.ctx.bus, TOOL_NAME, &approved.requested).await;
                return Err(ToolError::PathBlocked {
                    path: approved.requested.clone(),
                });
            }
            Err(lingxi_core::host::rooted_fs::RootedFsError::NotRegularFile) => {
                return Err(ToolError::InvalidInput(
                    format!("{file_path} is not a regular file").into(),
                ));
            }
            Err(error) => {
                return Err(ToolError::Io(
                    format!("Cannot read {file_path}: {error}").into(),
                ));
            }
        };
        // Metadata and contents both come from this no-follow handle. Reject
        // known oversized files before reading, then cap the same handle in
        // case the file grows after fstat.
        let file = tokio::fs::File::from_std(file);
        let metadata = file
            .metadata()
            .await
            .map_err(|error| ToolError::Io(format!("Cannot read {file_path}: {error}").into()))?;
        if !metadata.is_file() {
            return Err(ToolError::InvalidInput(
                format!("{file_path} is not a regular file").into(),
            ));
        }
        if metadata.len() > MAX_DOCUMENT_BYTES as u64 {
            return Err(ToolError::FileTooLarge {
                size: metadata.len(),
                limit: MAX_DOCUMENT_BYTES as u64,
            });
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_DOCUMENT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|error| ToolError::Io(format!("Cannot read {file_path}: {error}").into()))?;
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(ToolError::FileTooLarge {
                size: bytes.len() as u64,
                limit: MAX_DOCUMENT_BYTES as u64,
            });
        }
        String::from_utf8(bytes)
            .map_err(|_| ToolError::InvalidInput(format!("{file_path} is not UTF-8 text").into()))
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
            (None, Some(path)) => {
                if let Some(key) = Self::approval_key(ctx, &self.requested_file_path(path)?.0) {
                    let approval = self.take_approval(&key).ok_or_else(|| {
                        ToolError::InvalidInput(
                            "LocalApp file permission snapshot is missing; refusing to read the file."
                                .into(),
                        )
                    })?;
                    self.read_approved_document(path, &approval).await?
                } else if self.ctx.permission_gate.is_some() {
                    return Err(ToolError::InvalidInput(
                        "LocalApp file reads require a tool-use id for permission binding.".into(),
                    ));
                } else {
                    self.read_document(path).await?
                }
            }
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
            ).into()));
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
            return Err(ToolError::InvalidInput(
                format!("\"{visualization_id}\" is not a visualization id.").into(),
            ));
        };
        let Some(session) = root_session(&self.ctx, ctx).await else {
            return Err(ToolError::Internal(
                "this session has no id to find its visualizations under".into(),
            ));
        };
        let missing = || {
            ToolError::InvalidInput(format!(
                "This conversation has no visualization \"{visualization_id}\"; use the id in its reference line."
            ).into())
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
                ToolError::InvalidInput(
                    format!(
                        "This conversation has no revision {revision} of visualization \"{}\".",
                        reference.id
                    )
                    .into(),
                )
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
                    ).into()));
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
                ).into())
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
                    .map_err(|error| ToolError::Internal(error.to_string().into()))?,
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
            .map_err(|error| ToolError::InvalidInput(error.to_string().into()))
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
        let mut result = ToolCallResult::from_data(generated_builtin_result(&json!({
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
        })));
        result.model_content = Some(source_text_projection(&model_content));
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
                ).into()));
            }
            let model_content = format!(
                "Local app {id} \"{}\", revision {}, was saved from a visualization. This is its fragment ({} bytes); the app wraps it in its document. Change it and publish with id \"{id}\" and the changed fragment in `fragment`.\n<local-app-fragment id=\"{id}\" revision=\"{}\">\n{}\n</local-app-fragment>",
                summary.name,
                summary.revision,
                fragment.len(),
                summary.revision,
                fragment
            );
            let mut result = ToolCallResult::from_data(generated_builtin_result(&json!({
                "action": "read",
                "id": id.as_str(),
                "name": summary.name,
                "revision": summary.revision,
                "bytes": fragment.len(),
                "format": "fragment",
            })));
            result.model_content = Some(source_text_projection(&model_content));
            return Ok(result);
        }
        let app = self.store.read_current(&id).await.map_err(store_error)?;
        let summary = app.summary;
        if app.html.len() > MAX_READ_BYTES {
            return Err(ToolError::InvalidInput(format!(
                "Local app {id} is {} bytes, more than the {MAX_READ_BYTES} bytes read returns. Change it in the project it was built from and publish the new build with id \"{id}\".",
                app.html.len()
            )
            .into()));
        }
        let model_content = format!(
            "Local app {id} \"{}\", revision {} ({} bytes). Change it and publish with id \"{id}\".\n<local-app-document id=\"{id}\" revision=\"{}\">\n{}\n</local-app-document>",
            summary.name, summary.revision, summary.bytes, summary.revision, app.html
        );
        let mut result = ToolCallResult::from_data(generated_builtin_result(&json!({
            "action": "read",
            "id": id.as_str(),
            "name": summary.name,
            "revision": summary.revision,
            "bytes": summary.bytes,
        })));
        result.model_content = Some(source_text_projection(&model_content));
        Ok(result)
    }
}

fn store_error(error: StoreError) -> ToolError {
    match error {
        StoreError::Io(message) => ToolError::Io(message.into()),
        other => ToolError::InvalidInput(other.to_string().into()),
    }
}

fn publish_error(error: StoreError) -> ToolError {
    match error {
        StoreError::NotFound(_) => {
            ToolError::InvalidInput(format!("{error}; omit id to publish a new app.").into())
        }
        other => store_error(other),
    }
}

fn visualization_error(error: visualization::StoreError) -> ToolError {
    match error {
        visualization::StoreError::Io(message) => ToolError::Io(message.into()),
        other => ToolError::InvalidInput(other.to_string().into()),
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
    Err(ToolError::InvalidInput(
        message.trim_end().to_string().into(),
    ))
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
    let mut result = ToolCallResult::from_data(generated_builtin_result(&json!({
        "action": "publish",
        "id": published.id.as_str(),
        "revision": published.revision,
        "name": published.name,
        "warnings": warnings,
    })));
    result.model_content = Some(source_text_projection(&model_content));
    result
}

#[async_trait]
impl Tool for LocalAppTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("build a small app the user keeps and opens on this device".into())
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        &INPUT_SCHEMA
    }

    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

    fn output_schema(&self) -> Option<&Utf16JsonProjection> {
        Some(&OUTPUT_SCHEMA)
    }

    async fn parse_output(
        &self,
        output: &Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ToolError> {
        let invalid = || ToolError::InvalidInput("LocalApp returned invalid output".into());
        if !output
            .get_property("action")
            .is_some_and(|value| value.code_units().is_some())
        {
            return Err(invalid());
        }
        for field in ["id", "name"] {
            if output
                .get_property(field)
                .is_some_and(|value| value.code_units().is_none())
            {
                return Err(invalid());
            }
        }
        for field in ["revision", "bytes"] {
            if output.get_property(field).is_some_and(|value| {
                !value
                    .as_number()
                    .is_some_and(|number| number.is_finite() && number.fract() == 0.0)
            }) {
                return Err(invalid());
            }
        }
        if let Some(warnings) = output.get_property("warnings") {
            let Some(count) = warnings.array_len() else {
                return Err(invalid());
            };
            for index in 0..count {
                if warnings
                    .get_index(index)
                    .is_none_or(|value| value.code_units().is_none())
                {
                    return Err(invalid());
                }
            }
        }
        if let Some(apps) = output.get_property("apps") {
            let Some(count) = apps.array_len() else {
                return Err(invalid());
            };
            for index in 0..count {
                if apps
                    .get_index(index)
                    .is_none_or(|value| value.object_entries().is_none())
                {
                    return Err(invalid());
                }
            }
        }
        // The schema intentionally permits additional fields. Retain the exact
        // source instead of rebuilding or stripping the result.
        Ok(output.clone())
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        // `read` returns a whole document of up to MAX_READ_BYTES.
        300_000
    }

    fn is_concurrency_safe(&self, _input: &Utf16JsonProjection) -> bool {
        // The store serializes its writers.
        true
    }

    fn is_read_only(&self, _input: &Utf16JsonProjection) -> bool {
        // Writes only the app-private library, never user files.
        true
    }

    fn get_path(&self, input: &Utf16JsonProjection) -> Option<PathBuf> {
        input_value(input)
            .ok()
            .and_then(|input| Self::file_path(&input).map(PathBuf::from))
    }

    async fn validate_input(
        &self,
        input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let input = input_value(input).map_err(|message| ValidationError(message.into()))?;
        let Some(object) = input.as_object() else {
            return Err(ValidationError("LocalApp input must be an object.".into()));
        };
        for (key, value) in object {
            if !matches!(
                key.as_str(),
                "action"
                    | "name"
                    | "summary"
                    | "html"
                    | "file_path"
                    | "id"
                    | "visualization_id"
                    | "visualization_revision"
                    | "fragment"
            ) {
                return Err(ValidationError(
                    format!("\"{key}\" is not a LocalApp input field.").into(),
                ));
            }
            if key != "visualization_revision" && !value.is_string() {
                return Err(ValidationError(
                    format!("\"{key}\" must be a string.").into(),
                ));
            }
        }
        let action = Action::of(&input).map_err(|message| ValidationError(message.into()))?;
        if let Some(id) = input.get("id").and_then(Value::as_str) {
            if LocalAppId::parse(id).is_none() {
                return Err(ValidationError(
                    format!("\"{id}\" is not a local app id.").into(),
                ));
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
                let file_path = Self::file_path(&input);
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
                        return Err(ValidationError(
                            format!("{path} is not an .html file.").into(),
                        ));
                    }
                }
                if let Some(id) = visualization_id {
                    if VisualizationId::parse(id).is_none() {
                        return Err(ValidationError(
                            format!("\"{id}\" is not a visualization id.").into(),
                        ));
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
                        normalize_name(name)
                            .map_err(|error| ValidationError(error.to_string().into()))?;
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
                        .map_err(|error| ValidationError(error.to_string().into()))?;
                }
                Ok(())
            }
        }
    }

    fn native_input_validation(&self) -> bool {
        // Nested RegistryToolInvoker calls only run `validate_input` for tools
        // that opt into native validation. This schema's enum/pattern fields
        // fall outside its flat parser, so the custom validator must still run
        // on that dispatch path.
        true
    }

    async fn check_permissions(
        &self,
        input: &Utf16JsonProjection,
        ctx: &ToolUseContext,
    ) -> PermissionResult {
        // A file is read under exactly the rules `Read` would get; everything
        // else only reaches the app's own library.
        if let Ok(input) = input_value(input) {
            if Action::of(&input) == Ok(Action::Publish) {
                if let Some(path) = Self::file_path(&input) {
                    if self.ctx.permission_gate.is_some() && ctx.tool_use_id.is_none() {
                        let message =
                            "LocalApp file reads require a tool-use id for permission binding.";
                        return PermissionResult::Deny {
                            reason: PermissionDecisionReason::Other {
                                reason: message.into(),
                            },
                            explanation: Some(message.into()),
                            metadata: PermissionMetadata {
                                blocked_path: Some(path.to_owned()),
                                ..PermissionMetadata::default()
                            },
                        };
                    }

                    let approval = match self.resolve_file(path).await {
                        Ok(approval) => approval,
                        Err(error) => {
                            let message = error.model_facing_message();
                            return PermissionResult::Deny {
                                reason: PermissionDecisionReason::Other {
                                    reason: message.clone(),
                                },
                                explanation: Some(message),
                                metadata: PermissionMetadata {
                                    blocked_path: Some(path.to_owned()),
                                    ..PermissionMetadata::default()
                                },
                            };
                        }
                    };
                    let read_path = approval.requested.to_string_lossy().into_owned();
                    let read_input =
                        Utf16JsonProjection::object(vec![("file_path".into(), read_path.into())])
                            .expect("generated Read permission input is valid JSON");
                    let approval_key = Self::approval_key(ctx, &approval.requested);
                    if let Some(gate) = &self.ctx.permission_gate {
                        let check_ctx = PermissionCheckContext {
                            tool_use_id: ctx.tool_use_id.as_ref().map(|id| id.as_str().to_string()),
                            is_non_interactive_session: ctx.options.is_non_interactive_session,
                            ..Default::default()
                        };
                        let result = match gate
                            .resolve_detailed_or_abort("Read", &read_input, &check_ctx)
                            .await
                        {
                            Ok(PermissionResolution::Allow { .. }) => PermissionResult::Allow {
                                reason: PermissionDecisionReason::Other {
                                    reason: format!(
                                        "LocalApp file_path is allowed as Read: {path}"
                                    )
                                    .into(),
                                },
                                updated_input: None,
                                update_destination: None,
                                metadata: PermissionMetadata {
                                    blocked_path: Some(path.to_owned()),
                                    ..PermissionMetadata::default()
                                },
                            },
                            Ok(PermissionResolution::Deny { reason, .. }) => {
                                PermissionResult::Deny {
                                    reason: PermissionDecisionReason::Other {
                                        reason: reason.clone(),
                                    },
                                    explanation: Some(reason),
                                    metadata: PermissionMetadata {
                                        blocked_path: Some(path.to_owned()),
                                        ..PermissionMetadata::default()
                                    },
                                }
                            }
                            Err(abort) => PermissionResult::Deny {
                                reason: PermissionDecisionReason::Other {
                                    reason: abort.message.clone().into(),
                                },
                                explanation: Some(abort.message.into()),
                                metadata: PermissionMetadata {
                                    blocked_path: Some(path.to_owned()),
                                    ..PermissionMetadata::default()
                                },
                            },
                            Ok(PermissionResolution::Ask)
                            | Ok(PermissionResolution::AskWithContext { .. }) => {
                                let message =
                                    format!("LocalApp needs Read permission for file_path: {path}");
                                PermissionResult::Ask {
                                    reason: PermissionDecisionReason::Other {
                                        reason: message.clone().into(),
                                    },
                                    prompt: permission::result::PermissionPrompt {
                                        title: "Read local app document".into(),
                                        message: message.into(),
                                        options: Vec::new(),
                                    },
                                    pending_classifier_check: None,
                                    metadata: PermissionMetadata {
                                        blocked_path: Some(path.to_owned()),
                                        ..PermissionMetadata::default()
                                    },
                                }
                            }
                        };
                        if let Some(key) = approval_key {
                            if matches!(
                                &result,
                                PermissionResult::Allow { .. } | PermissionResult::Ask { .. }
                            ) {
                                self.remember_approval(key, approval);
                            } else {
                                self.discard_approval(&key);
                            }
                        }
                        return result;
                    }

                    // Hosts without a live gate use the same boot policy as
                    // Read. Preserve its mode fallback exactly; only an
                    // explicit matched Ask rule needs the path marker that
                    // keeps an outer LocalApp allow from swallowing the ask.
                    let mut result = self.ctx.permission_policy.authorize("Read", &read_input);
                    if let PermissionResult::Ask {
                        reason: PermissionDecisionReason::MatchedRule { .. },
                        metadata,
                        ..
                    } = &mut result
                    {
                        metadata.blocked_path = Some(path.to_owned());
                    }
                    if let Some(key) = approval_key {
                        if matches!(
                            &result,
                            PermissionResult::Allow { .. } | PermissionResult::Ask { .. }
                        ) {
                            self.remember_approval(key, approval);
                        } else {
                            self.discard_approval(&key);
                        }
                    }
                    return result;
                }
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

    async fn description(
        &self,
        _input: &Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        DESCRIPTION.to_string()
    }

    async fn prompt(&self, _opts: &PromptOptions) -> Utf16JsonProjection {
        source_text_projection(PROMPT)
    }

    fn get_activity_description(&self, input: &Utf16JsonProjection) -> Option<String> {
        let action = input_value(input)
            .ok()
            .and_then(|input| Action::of(&input).ok());
        Some(
            match action {
                Some(Action::List) => "Listing local apps",
                Some(Action::Read) => "Reading local app",
                _ if input_value(input).is_ok_and(|v| v.get("visualization_id").is_some()) => {
                    "Saving visualization as an app"
                }
                _ => "Publishing local app",
            }
            .to_string(),
        )
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let input =
            input_value(&input).map_err(|message| ToolError::InvalidInput(message.into()))?;
        match Action::of(&input).map_err(|message| ToolError::InvalidInput(message.into()))? {
            Action::Publish => self.publish(&input, &ctx).await,
            Action::List => self.list().await,
            Action::Read => self.read(&input).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::ToolInvoker;
    use mobile_linux_api::ProcessOutput;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};

    const DOC: &str = "<!doctype html><html><head><title>t</title></head><body>x</body></html>";

    fn projection(value: &Value) -> Utf16JsonProjection {
        Utf16JsonProjection::from_serde_value(value).expect("test input is valid JSON")
    }

    fn data_value(result: &ToolCallResult) -> Value {
        serde_json::from_str(
            &result
                .data
                .to_json_string()
                .expect("generated LocalApp result serializes"),
        )
        .expect("generated LocalApp result is serde JSON")
    }

    fn model_text(result: &ToolCallResult) -> &str {
        result
            .model_content
            .as_ref()
            .and_then(|content| content.string_at(""))
            .expect("LocalApp results provide model text")
    }

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
        tool.call(projection(&input), fresh_ctx(), fresh_tx()).await
    }

    fn nested_context() -> lingxi_core::host::tool_invoker::SubagentInvocationContext {
        use lingxi_core::host::tool_invoker::{SubagentInvocationContext, ToolExecutionPolicy};

        SubagentInvocationContext {
            available_tool_names: None,
            cancellation_token: lingxi_core::host::CancellationToken::new(),
            permission_pause_observer: None,
            parent_agent_id: None,
            native_query_depth: None,
            origin_session_id: None,
            tool_execution_policy: ToolExecutionPolicy::Ordinary,
            instruction_context: None,
            fork_context: None,
            agent_name: None,
            team_name: None,
            is_async: false,
            is_non_interactive_session: false,
            can_show_permission_prompts: true,
            cwd: None,
            tool_use_id: Some("toolu_local_app_nested".into()),
            assistant_message_id: None,
            depth: 0,
            observer: None,
            parent_model: None,
            parent_model_profile: None,
            agent_spawn_provenance: Default::default(),
            tool_context_state: None,
            current_history: Vec::new(),
            assistant_message: None,
            same_turn_tool_uses: Vec::new(),
            mode_override: None,
            request_source: None,
            frozen_command_denies: Vec::new(),
        }
    }

    #[tokio::test]
    async fn nested_registry_invoker_rejects_invalid_id_before_store_write() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let store = Arc::clone(&tool.store);
        let mut registry = tool_api::ToolRegistry::new();
        registry.register_builtin(Arc::new(tool));
        let invoker = tool_api::RegistryToolInvoker::new(Arc::new(registry));

        for id in [json!("../invalid"), json!(123), Value::Null] {
            let error = invoker
                .invoke(
                    TOOL_NAME,
                    projection(&json!({
                        "name": "Must not be created",
                        "html": DOC,
                        "id": id,
                    })),
                    nested_context(),
                )
                .await
                .expect_err("nested dispatch must run LocalApp's custom validator");

            assert!(
                matches!(
                    error,
                    lingxi_core::host::tool_invoker::ToolInvokerError::Validation(_)
                ),
                "got {error:?}"
            );
        }
        assert!(store.list().await.unwrap().is_empty());
    }

    struct ReadAskOuterAllowGate {
        resolutions: std::sync::Mutex<Vec<String>>,
        asks: std::sync::atomic::AtomicUsize,
        ask_request: std::sync::Mutex<Option<(String, Utf16JsonProjection)>>,
        allow_ask: bool,
        retarget: Option<(PathBuf, PathBuf)>,
    }

    #[async_trait::async_trait]
    impl lingxi_core::host::permission_gate::PermissionGate for ReadAskOuterAllowGate {
        async fn check(
            &self,
            _name: &str,
            _input: &Utf16JsonProjection,
        ) -> lingxi_core::host::permission_gate::PermissionDecision {
            lingxi_core::host::permission_gate::PermissionDecision::Allow
        }

        async fn resolve_detailed(
            &self,
            name: &str,
            _input: &Utf16JsonProjection,
        ) -> lingxi_core::host::permission_gate::PermissionResolution {
            self.resolutions.lock().unwrap().push(name.to_owned());
            if name == "Read" {
                lingxi_core::host::permission_gate::PermissionResolution::Ask
            } else {
                lingxi_core::host::permission_gate::PermissionResolution::Allow {
                    rule_source: Some("userSettings".into()),
                    classifier_approved: false,
                }
            }
        }

        async fn ask_via_transport(
            &self,
            name: &str,
            input: &Utf16JsonProjection,
            _ctx: &PermissionCheckContext,
        ) -> lingxi_core::host::permission_gate::PermissionOutcome {
            self.asks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *self.ask_request.lock().unwrap() = Some((name.to_owned(), input.clone()));
            if let Some((link, target)) = &self.retarget {
                #[cfg(unix)]
                {
                    std::fs::remove_file(link).unwrap();
                    std::os::unix::fs::symlink(target, link).unwrap();
                }
                #[cfg(not(unix))]
                unreachable!("the symlink-retarget test is Unix-only");
            }
            if self.allow_ask {
                lingxi_core::host::permission_gate::PermissionOutcome::Allow {
                    updated_input: None,
                    permission_updates: Vec::new(),
                    decision_classification: None,
                }
            } else {
                lingxi_core::host::permission_gate::PermissionOutcome::Deny {
                    reason: "the explicit Read ask was not approved".into(),
                }
            }
        }
    }

    #[tokio::test]
    async fn nested_read_ask_is_not_suppressed_by_outer_local_app_allow() {
        use lingxi_core::host::permission_gate::PermissionGate;

        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("app.html"), DOC).unwrap();
        let gate = Arc::new(ReadAskOuterAllowGate {
            resolutions: std::sync::Mutex::new(Vec::new()),
            asks: std::sync::atomic::AtomicUsize::new(0),
            ask_request: std::sync::Mutex::new(None),
            allow_ask: false,
            retarget: None,
        });
        let mut ctx = quiet_ctx();
        ctx.session_cwd = tool_api::SessionCwd::new(
            workspace.path().to_path_buf(),
            vec![workspace.path().to_path_buf()],
        );
        ctx.permission_gate = Some(gate.clone());
        let store = Arc::new(LocalAppStore::new(
            ctx.fs.clone(),
            home.path().to_path_buf(),
        ));
        let tool = LocalAppTool::new(ctx, Arc::clone(&store));
        let mut registry = tool_api::ToolRegistry::new();
        registry.register_builtin(Arc::new(tool));
        let invoker = tool_api::RegistryToolInvoker::new(Arc::new(registry))
            .with_gate(gate.clone() as Arc<dyn PermissionGate>);

        let input = projection(&json!({
            "name": "Must wait for Read approval",
            "file_path": "app.html",
        }));
        let error = invoker
            .invoke(TOOL_NAME, input.clone(), nested_context())
            .await
            .expect_err("a protected Read ask must reach the prompt transport");

        assert!(matches!(
            error,
            lingxi_core::host::tool_invoker::ToolInvokerError::Internal(_)
        ));
        assert_eq!(
            *gate.resolutions.lock().unwrap(),
            vec!["Read".to_owned(), TOOL_NAME.to_owned()],
            "the tool asks Read first and the outer LocalApp rule allows the tool"
        );
        assert_eq!(
            gate.asks.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the LocalApp allow must not suppress its explicit Read ask"
        );
        let (asked_name, asked_input) = gate
            .ask_request
            .lock()
            .unwrap()
            .clone()
            .expect("the LocalApp prompt transport must receive the protected ask");
        assert_eq!(asked_name, TOOL_NAME);
        assert_eq!(
            asked_input.to_json_string().unwrap(),
            input.to_json_string().unwrap(),
            "the prompt must retain the complete LocalApp input"
        );
        assert!(store.list().await.unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nested_read_approval_rejects_retarget_to_another_trusted_file() {
        use lingxi_core::host::permission_gate::PermissionGate;
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let first = workspace.path().join("first.html");
        let second = workspace.path().join("second.html");
        let link = workspace.path().join("link.html");
        std::fs::write(&first, DOC).unwrap();
        std::fs::write(&second, DOC).unwrap();
        symlink(&first, &link).unwrap();

        let gate = Arc::new(ReadAskOuterAllowGate {
            resolutions: std::sync::Mutex::new(Vec::new()),
            asks: std::sync::atomic::AtomicUsize::new(0),
            ask_request: std::sync::Mutex::new(None),
            allow_ask: true,
            retarget: Some((link.clone(), second)),
        });
        let mut ctx = quiet_ctx();
        ctx.session_cwd = tool_api::SessionCwd::new(
            workspace.path().to_path_buf(),
            vec![workspace.path().to_path_buf()],
        );
        ctx.permission_gate = Some(gate.clone());
        let store = Arc::new(LocalAppStore::new(
            ctx.fs.clone(),
            home.path().to_path_buf(),
        ));
        let tool = LocalAppTool::new(ctx, Arc::clone(&store));
        let mut registry = tool_api::ToolRegistry::new();
        registry.register_builtin(Arc::new(tool));
        let invoker = tool_api::RegistryToolInvoker::new(Arc::new(registry))
            .with_gate(gate.clone() as Arc<dyn PermissionGate>);

        let error = invoker
            .invoke(
                TOOL_NAME,
                projection(&json!({
                    "name": "Must stay bound to first.html",
                    "file_path": "link.html",
                })),
                nested_context(),
            )
            .await
            .expect_err("permission for first.html must not authorize the retargeted link");

        assert!(matches!(
            error,
            lingxi_core::host::tool_invoker::ToolInvokerError::Internal(_)
        ));
        assert_eq!(
            gate.asks.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the file link changes only after the explicit Read prompt is accepted"
        );
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn gated_file_call_without_snapshot_fails_closed() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("app.html"), DOC).unwrap();
        let gate = Arc::new(ReadAskOuterAllowGate {
            resolutions: std::sync::Mutex::new(Vec::new()),
            asks: std::sync::atomic::AtomicUsize::new(0),
            ask_request: std::sync::Mutex::new(None),
            allow_ask: true,
            retarget: None,
        });
        let mut builtin = quiet_ctx();
        builtin.session_cwd = tool_api::SessionCwd::new(
            workspace.path().to_path_buf(),
            vec![workspace.path().to_path_buf()],
        );
        builtin.permission_gate = Some(gate);
        let store = Arc::new(LocalAppStore::new(
            builtin.fs.clone(),
            home.path().to_path_buf(),
        ));
        let tool = LocalAppTool::new(builtin, Arc::clone(&store));
        let mut ctx = fresh_ctx();
        ctx.tool_use_id = Some(lingxi_core::types::ToolUseId::from(
            "toolu_local_app_missing_snapshot",
        ));

        let error = tool
            .call(
                projection(&json!({
                    "name": "Must not be read without approval",
                    "file_path": "app.html",
                })),
                ctx,
                fresh_tx(),
            )
            .await
            .expect_err("a gated file call without its one-shot snapshot must fail closed");

        assert!(matches!(error, ToolError::InvalidInput(_)));
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn live_gate_file_permission_without_tool_use_id_fails_closed() {
        let home = tempfile::tempdir().unwrap();
        let gate = Arc::new(ReadAskOuterAllowGate {
            resolutions: std::sync::Mutex::new(Vec::new()),
            asks: std::sync::atomic::AtomicUsize::new(0),
            ask_request: std::sync::Mutex::new(None),
            allow_ask: true,
            retarget: None,
        });
        let mut builtin = quiet_ctx();
        builtin.permission_gate = Some(gate.clone());
        let store = Arc::new(LocalAppStore::new(
            builtin.fs.clone(),
            home.path().to_path_buf(),
        ));
        let tool = LocalAppTool::new(builtin, store);
        let mut ctx = fresh_ctx();
        ctx.tool_use_id = None;

        let result = tool
            .check_permissions(
                &projection(&json!({
                    "name": "Unbound file app",
                    "file_path": "app.html",
                })),
                &ctx,
            )
            .await;

        assert!(matches!(result, PermissionResult::Deny { .. }));
        assert!(gate.resolutions.lock().unwrap().is_empty());
    }

    fn approval_for_test(path: &str) -> ApprovedLocalAppFile {
        let path = PathBuf::from(path);
        ApprovedLocalAppFile {
            requested: path.clone(),
            canonical: path.clone(),
            root: path.parent().unwrap().to_path_buf(),
            relative: PathBuf::from(path.file_name().unwrap()),
        }
    }

    #[test]
    fn pending_approval_cache_is_bounded_fifo_and_consumes_once() {
        let mut pending = PendingLocalAppApprovals::default();
        let first = ("toolu_0".to_owned(), PathBuf::from("/workspace/0.html"));
        for index in 0..=MAX_PENDING_LOCAL_APP_APPROVALS {
            let key = (
                format!("toolu_{index}"),
                PathBuf::from(format!("/workspace/{index}.html")),
            );
            pending.insert(key, approval_for_test(&format!("/workspace/{index}.html")));
        }

        assert_eq!(pending.by_key.len(), MAX_PENDING_LOCAL_APP_APPROVALS);
        assert!(
            !pending.by_key.contains_key(&first),
            "the oldest entry is evicted"
        );

        let newest = (
            format!("toolu_{MAX_PENDING_LOCAL_APP_APPROVALS}"),
            PathBuf::from(format!("/workspace/{MAX_PENDING_LOCAL_APP_APPROVALS}.html")),
        );
        assert!(pending.remove(&newest).is_some());
        assert!(pending.remove(&newest).is_none(), "approvals are one-shot");

        let replacement = (
            "toolu_replace".to_owned(),
            PathBuf::from("/workspace/x.html"),
        );
        pending.insert(
            replacement.clone(),
            approval_for_test("/workspace/old.html"),
        );
        pending.insert(
            replacement.clone(),
            approval_for_test("/workspace/new.html"),
        );
        assert_eq!(
            pending.by_key.get(&replacement).unwrap().canonical,
            PathBuf::from("/workspace/new.html")
        );
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
        assert_eq!(data_value(&first)["action"], "publish");
        assert_eq!(data_value(&first)["revision"], 1);
        assert_eq!(data_value(&first)["name"], "Water");
        let id = data_value(&first)["id"].as_str().unwrap().to_string();
        assert!(
            !data_value(&first).to_string().contains("<!doctype"),
            "the client-state result never carries the document"
        );
        assert!(model_text(&first).contains(&id));
        let second = call(&tool, json!({"html": DOC, "id": id})).await.unwrap();
        assert_eq!(
            (
                data_value(&second)["revision"].as_u64(),
                data_value(&second)["name"].as_str()
            ),
            (Some(2), Some("Water"))
        );
    }

    #[tokio::test]
    async fn list_and_read_return_the_library() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let empty = call(&tool, json!({"action": "list"})).await.unwrap();
        assert_eq!(data_value(&empty)["apps"].as_array().unwrap().len(), 0);
        let timer = call(&tool, json!({"name": "Timer", "html": DOC}))
            .await
            .unwrap();
        let id = data_value(&timer)["id"].as_str().unwrap().to_string();
        let listed = call(&tool, json!({"action": "list"})).await.unwrap();
        assert_eq!(data_value(&listed)["apps"][0]["id"], id.as_str());
        assert!(model_text(&listed).contains("\"Timer\""));
        let read = call(&tool, json!({"action": "read", "id": id}))
            .await
            .unwrap();
        assert_eq!(data_value(&read)["action"], "read");
        assert_eq!(data_value(&read)["bytes"], DOC.len());
        assert!(model_text(&read).contains(DOC));
        assert!(!data_value(&read).to_string().contains("<!doctype"));
    }

    #[tokio::test]
    async fn rejected_documents_explain_why_and_store_nothing() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let error = call(&tool, json!({"name": "t", "html": "<div>fragment</div>"}))
            .await
            .unwrap_err();
        let message = error.model_facing_message();
        assert!(
            message
                .string_at("")
                .is_some_and(|text| text.contains("complete HTML document")),
            "{message:?}"
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
        assert_eq!(data_value(&result)["warnings"].as_array().unwrap().len(), 1);
        assert!(model_text(&result).contains("Warnings:"));
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
            assert!(
                tool.validate_input(&projection(&input), &ctx)
                    .await
                    .is_err(),
                "{input}"
            );
        }
        for input in [
            json!({"name": "t", "file_path": "/tmp/a.HTML"}),
            json!({"id": "a1", "html": DOC}),
            json!({"action": "list"}),
            json!({"action": "read", "id": "a1"}),
        ] {
            assert!(
                tool.validate_input(&projection(&input), &ctx).await.is_ok(),
                "{input}"
            );
        }
    }

    #[tokio::test]
    async fn output_parser_validates_schema_fields_and_preserves_source() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let source = r#"{"action":"read","name":"\ud800","extra":{"keep":true}}"#;
        let output = Utf16JsonProjection::parse(source).unwrap();
        let parsed = tool.parse_output(&output).await.unwrap();
        assert_eq!(parsed.to_json_string().unwrap(), source);

        let invalid = projection(&json!({"action": "read", "revision": "1"}));
        assert!(tool.parse_output(&invalid).await.is_err());
    }

    #[tokio::test]
    async fn unknown_ids_are_rejected_instead_of_minting_a_new_app() {
        let home = tempfile::tempdir().unwrap();
        let tool = tool(home.path());
        let error = call(&tool, json!({"html": DOC, "id": "nope"}))
            .await
            .unwrap_err();
        assert!(error
            .model_facing_message()
            .string_at("")
            .is_some_and(|message| message.contains("omit id")));
        let missing = call(&tool, json!({"action": "read", "id": "nope"}))
            .await
            .unwrap_err();
        assert!(missing
            .model_facing_message()
            .string_at("")
            .is_some_and(|message| message.contains("not in this device")));
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
        assert!(error
            .model_facing_message()
            .string_at("")
            .is_some_and(|message| message.contains("built from")));
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
        assert_eq!(data_value(&published)["revision"], 1);
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

    #[cfg(unix)]
    #[tokio::test]
    async fn file_documents_reject_leaf_symlink_retargeted_after_approval() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
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
        let inside = workspace.path().join("inside.html");
        let link = workspace.path().join("link.html");
        let external = outside.path().join("external.html");
        std::fs::write(&inside, DOC).unwrap();
        std::fs::write(&external, "<html>outside</html>").unwrap();
        symlink(&inside, &link).unwrap();

        let approved = tool.resolve_file("link.html").await.unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink(&external, &link).unwrap();

        let error = tool
            .read_approved_document("link.html", &approved)
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::PathBlocked { .. }), "{error:?}");
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
            assert!(
                tool.validate_input(&projection(&input), &ctx).await.is_ok(),
                "{input}"
            );
        }
        for input in [
            json!({"visualization_id": "not an id"}),
            json!({"visualization_id": "v1", "visualization_revision": 0}),
            json!({"visualization_id": "v1", "fragment": "<p>x</p>", "id": "a1"}),
        ] {
            assert!(
                tool.validate_input(&projection(&input), &ctx)
                    .await
                    .is_err(),
                "{input}"
            );
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
                projection(&json!({"visualization_id": reference.id.as_str()})),
                session_ctx(session),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(
            data_value(&saved)["action"],
            "publish",
            "the same card as any publish"
        );
        assert_eq!(data_value(&saved)["name"], "Counter");
        assert_eq!(data_value(&saved)["revision"], 1);
        assert!(!data_value(&saved).to_string().contains("<script"));
        let id = LocalAppId::parse(data_value(&saved)["id"].as_str().unwrap()).unwrap();

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
        assert_eq!(data_value(&read)["format"], "fragment");
        assert!(model_text(&read).contains(FRAGMENT));
    }

    #[tokio::test]
    async fn saving_the_same_revision_again_returns_the_saved_app() {
        let home = tempfile::tempdir().unwrap();
        let (tool, visualizations) = tool_with_visualizations(home.path());
        let session = lingxi_core::types::SessionId::new();
        let first =
            publish_visualization(&visualizations, session, None, "Counter", FRAGMENT).await;
        let save = |input: Value| tool.call(projection(&input), session_ctx(session), fresh_tx());
        let one = save(json!({"visualization_id": first.id.as_str()}))
            .await
            .unwrap();
        let again = save(json!({"visualization_id": first.id.as_str()}))
            .await
            .unwrap();
        assert_eq!(data_value(&again)["id"], data_value(&one)["id"]);
        assert_eq!(data_value(&again)["revision"], 1);
        assert_eq!(data_value(&again)["warnings"].as_array().unwrap().len(), 1);
        assert!(model_text(&again).contains("already saved"));

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
            data_value(&newer)["id"],
            data_value(&one)["id"],
            "the latest revision is a new app"
        );
        assert_eq!(data_value(&newer)["name"], "Counter v2");
        let older =
            save(json!({"visualization_id": first.id.as_str(), "visualization_revision": 1}))
                .await
                .unwrap();
        assert_eq!(data_value(&older)["id"], data_value(&one)["id"]);
        assert_eq!(tool.store.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn fragment_apps_change_through_their_fragment() {
        let home = tempfile::tempdir().unwrap();
        let (tool, visualizations) = tool_with_visualizations(home.path());
        let session = lingxi_core::types::SessionId::new();
        let reference =
            publish_visualization(&visualizations, session, None, "Counter", FRAGMENT).await;
        let save = |input: Value| tool.call(projection(&input), session_ctx(session), fresh_tx());
        let id = save(json!({"visualization_id": reference.id.as_str()}))
            .await
            .unwrap()
            .data
            .string_at("/id")
            .unwrap()
            .to_string();

        let changed = call(&tool, json!({"id": id, "fragment": "<p>v2</p>"}))
            .await
            .unwrap();
        assert_eq!(
            (
                data_value(&changed)["revision"].as_u64(),
                data_value(&changed)["name"].as_str()
            ),
            (Some(2), Some("Counter"))
        );
        let read = call(&tool, json!({"action": "read", "id": id}))
            .await
            .unwrap();
        assert!(model_text(&read).contains("<p>v2</p>"));
        let wrapped = call(&tool, json!({"id": id, "fragment": DOC}))
            .await
            .unwrap_err();
        assert!(
            wrapped
                .model_facing_message()
                .string_at("")
                .unwrap()
                .contains("<!doctype>"),
            "{}",
            wrapped.model_facing_message().string_at("").unwrap()
        );

        // A changed app is no longer "already saved": saving again replaces
        // its code only when asked to, by id.
        let replaced = save(json!({"id": id, "visualization_id": reference.id.as_str()}))
            .await
            .unwrap();
        assert_eq!(
            (
                data_value(&replaced)["id"].as_str(),
                data_value(&replaced)["revision"].as_u64()
            ),
            (Some(id.as_str()), Some(3))
        );
        assert!(model_text(&replaced).contains("keeps its stored data"));

        let document = call(&tool, json!({"name": "Plain", "html": DOC}))
            .await
            .unwrap()
            .data
            .string_at("/id")
            .unwrap()
            .to_string();
        for input in [
            json!({"id": document, "fragment": "<p>x</p>"}),
            json!({"id": document, "visualization_id": reference.id.as_str()}),
        ] {
            let error = save(input.clone()).await.unwrap_err();
            assert!(
                error
                    .model_facing_message()
                    .string_at("")
                    .unwrap()
                    .contains("complete document"),
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
                projection(&json!({"visualization_id": reference.id.as_str()})),
                session_ctx(root),
                fresh_tx(),
            )
            .await
            .unwrap();
        assert_eq!(data_value(&saved)["name"], "Root chart");
        assert_eq!(tool.store.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unknown_visualizations_and_long_titles_are_explained() {
        let home = tempfile::tempdir().unwrap();
        let (tool, visualizations) = tool_with_visualizations(home.path());
        let session = lingxi_core::types::SessionId::new();
        let save = |input: Value| tool.call(projection(&input), session_ctx(session), fresh_tx());
        let missing = save(json!({"visualization_id": "nope"})).await.unwrap_err();
        assert!(missing
            .model_facing_message()
            .string_at("")
            .unwrap()
            .contains("no visualization"));
        let title = "T".repeat(MAX_NAME_CHARS + 1);
        let reference =
            publish_visualization(&visualizations, session, None, &title, FRAGMENT).await;
        let missing_revision =
            save(json!({"visualization_id": reference.id.as_str(), "visualization_revision": 9}))
                .await
                .unwrap_err();
        assert!(missing_revision
            .model_facing_message()
            .string_at("")
            .unwrap()
            .contains("no revision 9"));
        let long = save(json!({"visualization_id": reference.id.as_str()}))
            .await
            .unwrap_err();
        assert!(long
            .model_facing_message()
            .string_at("")
            .unwrap()
            .contains("pass a name"));
        let named = save(json!({"visualization_id": reference.id.as_str(), "name": "Short"}))
            .await
            .unwrap();
        assert_eq!(data_value(&named)["name"], "Short");
        assert!(tool.store.list().await.unwrap().len() == 1);
    }

    #[test]
    fn name_and_description() {
        assert_eq!(TOOL_NAME, "LocalApp");
        assert_ne!(TOOL_NAME, visualization::TOOL_NAME);
        assert!(DESCRIPTION.contains("Nothing is uploaded"));
        assert!(PROMPT.contains("`local-app` skill"));
    }

    fn tool_with_permission_policy(
        home: &std::path::Path,
        permission_policy: Arc<permission::PermissionPolicy>,
    ) -> LocalAppTool {
        let mut ctx = quiet_ctx();
        ctx.permission_policy = permission_policy;
        ctx.session_cwd = tool_api::SessionCwd::new(home.to_path_buf(), vec![home.to_path_buf()]);
        let store = Arc::new(LocalAppStore::new(ctx.fs.clone(), home.to_path_buf()));
        LocalAppTool::new(ctx, store)
    }

    #[tokio::test]
    async fn file_publish_delegates_exact_read_input_and_preserves_policy_deny() {
        use permission::{
            PermissionBehavior, PermissionMode, PermissionPolicy, PermissionRule,
            PermissionRuleSource, PermissionRuleValue,
        };

        let home = tempfile::tempdir().unwrap();
        let file_path = home
            .path()
            .join("target.html")
            .to_string_lossy()
            .into_owned();
        std::fs::write(&file_path, DOC).unwrap();
        let policy = Arc::new(
            PermissionPolicy::from_rules(
                PermissionMode::Default,
                [PermissionRule {
                    value: PermissionRuleValue::from_rule_string("Read(./target.html)"),
                    behavior: PermissionBehavior::Deny,
                    source: PermissionRuleSource::Command,
                }],
            )
            .with_roots(permission::filesystem::FsRoots {
                cwd: home.path().to_path_buf(),
                home: Some(home.path().to_path_buf()),
                lingxi_home: home.path().join("test-config-home"),
            }),
        );
        let tool = tool_with_permission_policy(home.path(), Arc::clone(&policy));
        let publish_input = projection(&json!({
            "name": "Denied file app",
            "file_path": file_path,
        }));
        let expected_read_input = projection(&json!({ "file_path": file_path }));
        let expected = policy.authorize("Read", &expected_read_input);
        assert!(
            matches!(
                &expected,
                PermissionResult::Deny {
                    reason: PermissionDecisionReason::MatchedRule { rule },
                    ..
                } if rule.value.tool_name == "Read"
                    && rule.value.rule_content_text() == Some("./target.html")
            ),
            "the exact Read path rule must deny the delegated input: {expected:?}"
        );

        let actual = tool.check_permissions(&publish_input, &fresh_ctx()).await;
        assert!(
            matches!(&actual, PermissionResult::Deny { .. }),
            "file-backed publish must return the Read denial unchanged: {actual:?}"
        );
        assert_eq!(
            serde_json::to_value(&actual).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "LocalApp must return the exact permission result produced for Read(file_path)"
        );
    }

    #[tokio::test]
    async fn inline_publish_list_and_read_do_not_request_read_permission() {
        use permission::{
            PermissionBehavior, PermissionMode, PermissionPolicy, PermissionRule,
            PermissionRuleSource, PermissionRuleValue,
        };

        let home = tempfile::tempdir().unwrap();
        let policy = Arc::new(PermissionPolicy::from_rules(
            PermissionMode::Default,
            [PermissionRule {
                value: PermissionRuleValue::from_rule_string("Read"),
                behavior: PermissionBehavior::Deny,
                source: PermissionRuleSource::Command,
            }],
        ));
        let tool = tool_with_permission_policy(home.path(), policy);
        let cases = [
            (
                "inline publish",
                json!({ "name": "Inline app", "html": DOC }),
            ),
            ("list", json!({ "action": "list" })),
            ("read", json!({ "action": "read", "id": "app_123" })),
        ];

        for (label, input) in cases {
            let result = tool
                .check_permissions(&projection(&input), &fresh_ctx())
                .await;
            assert!(
                matches!(&result, PermissionResult::Allow { .. }),
                "{label} must not be routed through the tool-wide Read denial: {result:?}"
            );
        }
    }
}
