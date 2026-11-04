//! Desktop's project-capable LocalApp adapter. The Electron host owns the
//! library and runtime; publishing crosses the same nested Read permission
//! gate and rooted, no-follow file boundary as the document-only mobile tool.
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lingxi_core::host::permission_gate::{PermissionCheckContext, PermissionResolution};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{HttpMethod, HttpRequest};
use once_cell::sync::Lazy;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    generated_builtin_result, generated_builtin_schema, source_text_projection, DescriptionOptions,
    PromptOptions, Tool, ToolCallResult, ToolError, ToolInputRepairMetadata, ToolInputRepairSchema,
    ToolInputSchemaProvenance, ToolStaticContext, ValidationError,
};
use tool_api::util::path_validation::{
    canonicalize_and_validate, resolve_against_cwd, translate_model_path,
};
use tool_api::BuiltinToolContext;
use visualization::{VisualizationId, VisualizationRef};

const PROMPT: &str = "Local App source development, previews, publication and recovery require the user's existing Code session. If asked in Chat, explain that the user must switch to Code; do not switch sessions, create a Code session or delegate around this restriction. Browsing and using installed apps remain available. In Code, load the local-app skill first. Publish a multi-file project with schema-2 lingxi-app.json using bundle_path, or a simple document using html/file_path. Apps have independent versions and data and survive their authoring conversation. Read the latest revision and pass expectedRevision when updating; never silently overwrite concurrent work. Node/Python services run only after the user authorizes the exact release in the native Apps page, including install/build scripts. Keep dependencies inside the app environment, never change the shared Linux toolchain. Host and guest architectures and supported capabilities determine compatibility. Local publication does not upload to a market. Use start/status/logs to verify and stop for the app's own processes. Use agent_catalog to discover exact host model and tool IDs; discovery does not grant access. Never pass private host transport, source-session or permission fields. On iOS, opening the App prepares it in the foreground, not as a permanently running service. User-initiated Agent work may receive a finite, revocable background lease; termination, force quit and device restart do not guarantee immediate relaunch. After the host is available, obtain fresh service URLs for a restarted service and inspect the same saved Agent run and thread read-only. Preserve durable data and never automatically replay requests or tool actions; a new attempt requires the user's explicit choice.";
// Matches the desktop host's transport budget, not mobile document limits.
const MAX_HOST_REQUEST_BYTES: usize = 64 * 1024 * 1024;
static SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    generated_builtin_schema(&json!({
      "type":"object", "additionalProperties":false,
      "properties": {
        "action":{"type":"string","enum":["publish","list","read","start","stop","status","logs","agent_catalog"]},
        "id":{"type":"string","pattern":"^a[a-f0-9]{32}$"},
        "expectedRevision":{"type":"integer","minimum":0,"description":"Required latest revision when updating an existing application; zero for a new app."},
        "name":{"type":"string","maxLength":60}, "summary":{"type":"string","maxLength":160},
        "html":{"type":"string"}, "file_path":{"type":"string"},
        "bundle_path":{"type":"string","description":"Absolute path of the application source directory containing lingxi-app.json. Files are copied into the desktop library; dependencies are installed using the manifest."},
        "visualization_id":{"type":"string"}, "visualization_revision":{"type":"integer","minimum":1}
      }
    }))
});

/// Permission-aware adapter to the desktop application's persistent library.
pub struct DesktopLocalAppTool {
    ctx: BuiltinToolContext,
    config_home: PathBuf,
    visualizations: Option<Arc<visualization::VisualizationStore>>,
    project_host: Option<Arc<dyn ProjectLocalAppHost>>,
}

/// An authenticated, application-owned service supplied by the native host.
/// Private staged/source fields are produced by this adapter after Read checks,
/// never accepted from model input. Execution consent remains owned by the host.
#[async_trait]
pub trait ProjectLocalAppHost: Send + Sync {
    /// Dispatch the already validated/staged local application request.
    async fn request(&self, request: Value) -> Result<Value, String>;
}
fn invalid(message: impl ToString) -> ToolError {
    ToolError::InvalidInput(message.to_string().into())
}
fn io(error: impl ToString) -> ToolError {
    ToolError::Io(error.to_string().into())
}

fn excluded_staging_entry(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".DS_Store"
            | branding::LOCAL_APP_PREPARATION_MARKER
    ) || name == ".env"
        || (name.starts_with(".env.") && name != ".env.example")
}

fn request_body(input: &Value, limit: usize) -> Result<String, ToolError> {
    let body = input.to_string();
    if body.len() > limit {
        return Err(invalid(format!(
            "Desktop Local App request is {} bytes; maximum is {limit} bytes. Use bundle_path for large projects.",
            body.len()
        )));
    }
    Ok(body)
}

/// One request's private capture record. The rooted lock serializes retries
/// across processes and remains held until the host request settles or drops.
struct PublicationCapture {
    imports: PathBuf,
    pin: lingxi_core::host::rooted_fs::RootIdentity,
    lock: lingxi_core::host::rooted_fs::RootedFileLock,
    record: PathBuf,
    staged: PathBuf,
}

impl PublicationCapture {
    async fn open(config_home: &Path, request_id: &str) -> Result<Self, ToolError> {
        let config_home = config_home.to_owned();
        let request_id = request_id.to_owned();
        tokio::task::spawn_blocking(move || {
            use lingxi_core::host::rooted_fs as fs;
            create_staging_directory(&config_home)?;
            let config_pin = fs::root_identity(&config_home).map_err(io)?;
            let imports = config_home.join("local-app-imports");
            let pin =
                fs::ensure_private_directory(&config_home, Path::new("local-app-imports"), 0o700)
                    .map_err(io)?;
            fs::sync_parent_pinned(
                &config_home,
                Path::new("local-app-imports"),
                Some(&config_pin),
            )
            .map_err(io)?;
            let lock_name = format!(".tool-publish-{request_id}.lock");
            let lock = fs::lock_exclusive_pinned(
                &imports,
                Path::new(&lock_name),
                0o700,
                0o600,
                Some(&pin),
            )
            .map_err(io)?;
            Ok(Self {
                staged: imports.join(format!("tool-publish-{request_id}")),
                record: PathBuf::from(format!(".tool-publish-{request_id}.json")),
                imports,
                pin,
                lock,
            })
        })
        .await
        .map_err(io)?
    }

    fn validate(&self) -> Result<(), ToolError> {
        use lingxi_core::host::rooted_fs::{self as fs, FlockGuard};
        self.lock.validate_current().map_err(io)?;
        if fs::root_identity(&self.imports).map_err(io)? != self.pin {
            return Err(invalid("Local App publication staging root changed"));
        }
        Ok(())
    }

    fn read(&self) -> Result<Option<Value>, ToolError> {
        use lingxi_core::host::rooted_fs as fs;
        self.validate()?;
        let file = match fs::open_read_file_pinned(&self.imports, &self.record, Some(&self.pin)) {
            Ok(file) => file,
            Err(fs::FsError::NotFound(_)) => return Ok(None),
            Err(error) => return Err(io(error)),
        };
        fs::validate_open_file_single_link(&file).map_err(io)?;
        let mut bytes = Vec::new();
        file.take((2 * MAX_HOST_REQUEST_BYTES + 65_536) as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.len() > 2 * MAX_HOST_REQUEST_BYTES + 65_536 {
            return Err(invalid("Local App publication capture is too large"));
        }
        let value = serde_json::from_slice(&bytes).map_err(io)?;
        Ok(Some(value))
    }

    async fn read_async(self: &Arc<Self>) -> Result<Option<Value>, ToolError> {
        let capture = Arc::clone(self);
        tokio::task::spawn_blocking(move || capture.read())
            .await
            .map_err(io)?
    }

    fn write(&self, value: &Value, overwrite: bool) -> Result<(), ToolError> {
        use lingxi_core::host::rooted_fs as fs;
        self.validate()?;
        let bytes = serde_json::to_vec(value).map_err(io)?;
        if bytes.len() > 2 * MAX_HOST_REQUEST_BYTES + 65_536 {
            return Err(invalid("Local App publication capture is too large"));
        }
        let result = fs::atomic_write_pinned(
            &self.imports,
            &self.record,
            &bytes,
            fs::AtomicWriteOptions {
                overwrite,
                create_parents: false,
                ..Default::default()
            },
            &self.pin,
        );
        #[cfg(test)]
        if overwrite && result.is_ok() && self.imports.join(".test-ready-write-uncertain").exists()
        {
            return Err(io("injected uncertain ready-record write"));
        }
        result.map_err(io)
    }

    async fn write_async(self: &Arc<Self>, value: Value, overwrite: bool) -> Result<(), ToolError> {
        let capture = Arc::clone(self);
        tokio::task::spawn_blocking(move || capture.write(&value, overwrite))
            .await
            .map_err(io)?
    }

    fn create_stage(&self) -> Result<std::fs::File, ToolError> {
        use lingxi_core::host::rooted_fs as fs;
        self.validate()?;
        let relative = PathBuf::from(
            self.staged
                .file_name()
                .ok_or_else(|| invalid("Invalid staging path"))?,
        );
        fs::create_directory_new_pinned(&self.imports, &relative, 0o700, &self.pin).map_err(io)
    }

    fn stage_exists(&self) -> Result<bool, ToolError> {
        match std::fs::symlink_metadata(&self.staged) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
            Ok(_) => Err(invalid(
                "Local App publication staging is not a private directory",
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io(error)),
        }
    }
}

/// Durably retain staged bytes before recording them as ready for host admission.
fn sync_publication_tree(root: &Path) -> Result<(), ToolError> {
    use lingxi_core::host::rooted_fs as fs;
    let pin = fs::root_identity(root).map_err(io)?;
    let mut directories = vec![PathBuf::new()];
    while let Some(relative) = directories.pop() {
        for entry in std::fs::read_dir(root.join(&relative)).map_err(io)? {
            let entry = entry.map_err(io)?;
            let path = relative.join(entry.file_name());
            let kind = entry.file_type().map_err(io)?;
            if kind.is_dir() {
                directories.push(path);
            } else if kind.is_file() {
                fs::open_read_file_pinned(root, &path, Some(&pin))
                    .map_err(io)?
                    .sync_all()
                    .map_err(io)?;
                fs::sync_parent_pinned(root, &path, Some(&pin)).map_err(io)?;
            } else {
                return Err(invalid("Local App staging contains a link or special file"));
            }
        }
        fs::sync_parent_pinned(root, &relative.join(".sync"), Some(&pin)).map_err(io)?;
    }
    if fs::root_identity(root).map_err(io)? != pin {
        return Err(invalid("Local App staging changed while syncing"));
    }
    Ok(())
}

/// A ready capture retains its original project even if the same owner later
/// changes cwd. A different owner, invocation or raw model input must fail.
fn same_captured_invocation(saved: &Value, current: &Value) -> bool {
    let old_session = &saved["sourceSession"];
    let new_session = &current["sourceSession"];
    saved["version"] == 1
        && saved["requestId"] == current["requestId"]
        && saved["request"] == current["request"]
        && old_session["sessionId"].as_str().is_some()
        && old_session["sessionId"] == new_session["sessionId"]
        && old_session["mode"] == "code"
        && old_session["mode"] == new_session["mode"]
        && old_session["requestId"] == current["requestId"]
        && old_session["requestId"] == new_session["requestId"]
        && old_session["projectPath"]
            .as_str()
            .is_some_and(|path| Path::new(path).is_absolute())
}

fn check_cancelled(ctx: &ToolUseContext) -> Result<(), ToolError> {
    if ctx
        .cancel
        .as_ref()
        .is_some_and(|token| token.is_cancelled())
    {
        Err(ToolError::Aborted)
    } else {
        Ok(())
    }
}

async fn cancelled(ctx: &ToolUseContext) {
    if let Some(token) = &ctx.cancel {
        token.cancelled().await;
    } else {
        std::future::pending::<()>().await;
    }
}

fn create_staging_directory(path: &Path) -> Result<(), ToolError> {
    // Tokio's mkdir uses a blocking job that survives a dropped future. A
    // A queued mkdir must not outlive a cancelled capture. These private,
    // shallow parent creations cannot yield to cancellation.
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(io)
}

impl DesktopLocalAppTool {
    /// Bind a tool to the same config home as its Electron host.
    pub fn new(
        ctx: BuiltinToolContext,
        config_home: PathBuf,
        visualizations: Option<Arc<visualization::VisualizationStore>>,
    ) -> Self {
        Self {
            ctx,
            config_home,
            visualizations,
            project_host: None,
        }
    }
    /// Use the native standalone application service instead of Desktop HTTP.
    #[must_use]
    pub fn with_host(mut self, host: Arc<dyn ProjectLocalAppHost>) -> Self {
        self.project_host = Some(host);
        self
    }
    fn source(&self, value: &str, scope: &(PathBuf, Vec<PathBuf>)) -> Result<PathBuf, ToolError> {
        let (cwd, trusted) = scope;
        let requested = resolve_against_cwd(PathBuf::from(value), cwd);
        let requested = translate_model_path(&self.ctx.fs, requested, false).map_err(invalid)?;
        canonicalize_and_validate(&requested, trusted)
            .map_err(|_| ToolError::PathBlocked { path: requested })
    }
    async fn read_allowed(&self, path: &Path, ctx: &ToolUseContext) -> Result<(), ToolError> {
        let input = generated_builtin_schema(&json!({"file_path":path.to_string_lossy()}));
        if let Some(gate) = &self.ctx.permission_gate {
            if ctx.tool_use_id.is_none() {
                return Err(invalid(
                    "Source reads require a tool-use id for permission binding",
                ));
            }
            let check = PermissionCheckContext {
                tool_use_id: ctx.tool_use_id.as_ref().map(|id| id.as_str().to_string()),
                is_non_interactive_session: ctx.options.is_non_interactive_session,
                ..Default::default()
            };
            match gate
                .resolve_detailed_or_abort("Read", &input, &check)
                .await
                .map_err(|e| invalid(e.message))?
            {
                PermissionResolution::Allow { .. } => Ok(()),
                PermissionResolution::Deny { reason, .. } => Err(ToolError::InvalidInput(reason)),
                _ => Err(ToolError::InteractionRequired(format!(
                    "Read approval is required for {}",
                    path.display()
                ))),
            }
        } else {
            match self.ctx.permission_policy.authorize("Read", &input) {
                PermissionResult::Allow { .. } => Ok(()),
                _ => Err(ToolError::InteractionRequired(format!(
                    "Read approval is required for {}",
                    path.display()
                ))),
            }
        }
    }
    async fn copy_file(
        &self,
        source: &Path,
        relative: &Path,
        target: &Path,
        ctx: &ToolUseContext,
        limit: Option<u64>,
        scope: &(PathBuf, Vec<PathBuf>),
    ) -> Result<(), ToolError> {
        check_cancelled(ctx)?;
        let requested = source.join(relative);
        let canonical = self.source(&requested.to_string_lossy(), scope)?;
        self.read_allowed(&requested, ctx).await?;
        check_cancelled(ctx)?;
        let input = lingxi_core::host::rooted_fs::open_file_after_permission(
            source, relative, &requested, &canonical,
        )
        .map_err(io)?;
        let metadata = input.metadata().map_err(io)?;
        if let Some(limit) = limit {
            if metadata.len() > limit {
                return Err(ToolError::FileTooLarge {
                    size: metadata.len(),
                    limit,
                });
            }
        }
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            // Keep staged sources private, but retain the ability to execute
            // project-owned install/start scripts. Read metadata from the
            // approved handle rather than reopening a mutable source path.
            let source_mode = metadata.permissions().mode();
            0o600 | if source_mode & 0o111 != 0 { 0o100 } else { 0 }
        };
        let mut input = tokio::fs::File::from_std(input);
        if let Some(parent) = target.parent() {
            create_staging_directory(parent)?;
        }
        // Create namespace entries before yielding, so cancellation cannot
        // leave an unobserved queued create_new job behind.
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = tokio::fs::File::from_std(options.open(target).map_err(io)?);
        if let Some(limit) = limit {
            // Keep the read bounded even if a file grows after fstat.
            let copied = tokio::io::copy(&mut input.take(limit + 1), &mut output)
                .await
                .map_err(io)?;
            if copied > limit {
                return Err(ToolError::FileTooLarge {
                    size: copied,
                    limit,
                });
            }
        } else {
            tokio::io::copy(&mut input, &mut output).await.map_err(io)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            output
                .set_permissions(std::fs::Permissions::from_mode(mode))
                .await
                .map_err(io)?;
        }
        Ok(())
    }
    async fn stage(
        &self,
        source: &Path,
        destination: &Path,
        ctx: &ToolUseContext,
        scope: &(PathBuf, Vec<PathBuf>),
    ) -> Result<(), ToolError> {
        let mut directories = vec![PathBuf::new()];
        while let Some(relative) = directories.pop() {
            check_cancelled(ctx)?;
            // Revalidate before enumeration and never descend into a link.
            let directory = self.source(&source.join(&relative).to_string_lossy(), scope)?;
            if directory != source.join(&relative) {
                return Err(invalid(
                    "Application source directory changed while publishing",
                ));
            }
            let mut entries = tokio::fs::read_dir(&directory).await.map_err(io)?;
            while let Some(entry) = entries.next_entry().await.map_err(io)? {
                check_cancelled(ctx)?;
                let name = entry.file_name();
                let name_text = name.to_string_lossy();
                if excluded_staging_entry(name_text.as_ref()) {
                    continue;
                }
                let kind = entry.file_type().await.map_err(io)?;
                let rel = relative.join(name);
                if kind.is_symlink() {
                    return Err(invalid(format!(
                        "Application source contains a symlink: {}",
                        rel.display()
                    )));
                }
                if kind.is_dir() {
                    // Python and uv support arbitrary virtual-environment
                    // names. Never import their interpreter links or packages;
                    // check only child directories, not the project root.
                    match tokio::fs::symlink_metadata(source.join(&rel).join("pyvenv.cfg")).await {
                        Ok(metadata) if metadata.is_file() => continue,
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(io(error)),
                    }
                    directories.push(rel);
                } else if kind.is_file() {
                    self.copy_file(source, &rel, &destination.join(&rel), ctx, None, scope)
                        .await?;
                } else {
                    return Err(invalid("Application source contains a special file"));
                }
            }
        }
        Ok(())
    }
    async fn host(&self, input: &Value) -> Result<ToolCallResult, ToolError> {
        let body = request_body(input, MAX_HOST_REQUEST_BYTES)?;
        if let Some(host) = &self.project_host {
            let result = host.request(input.clone()).await.map_err(invalid)?;
            return Ok(ToolCallResult::from_data(generated_builtin_result(&result)));
        }
        let discovery: Value = serde_json::from_slice(&tokio::fs::read(self.config_home.join("desktop-local-app-host.json")).await
            .map_err(|_| invalid(format!("Desktop Local App host is unavailable. Open or restart the {} desktop application.", branding::PRODUCT_NAME)))?).map_err(io)?;
        let port = discovery["port"]
            .as_u64()
            .filter(|p| *p > 0 && *p <= 65535)
            .ok_or_else(|| invalid("Invalid Local App host port"))?;
        let token = discovery["token"]
            .as_str()
            .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| invalid("Invalid Local App host identity"))?;
        let response = self
            .ctx
            .http
            .request(HttpRequest {
                method: HttpMethod::Post,
                url: format!("http://127.0.0.1:{port}/local-app"),
                headers: vec![
                    ("Content-Type".into(), "application/json".into()),
                    ("Authorization".into(), format!("Bearer {token}")),
                ],
                body: Some(body),
                body_bytes: None,
                timeout: Some(Duration::from_secs(600)),
            })
            .await
            .map_err(io)?;
        let result: Value = serde_json::from_str(&response.body).map_err(io)?;
        if response.status != 200 {
            return Err(invalid(
                result["error"]
                    .as_str()
                    .unwrap_or("Desktop Local App operation failed"),
            ));
        }
        Ok(ToolCallResult::from_data(generated_builtin_result(&result)))
    }
    async fn visualization_document(
        &self,
        input: &mut Value,
        ctx: &ToolUseContext,
    ) -> Result<(), ToolError> {
        let store = self
            .visualizations
            .as_ref()
            .ok_or_else(|| invalid("Visualizations are unavailable"))?;
        let id = VisualizationId::parse(input["visualization_id"].as_str().unwrap_or_default())
            .ok_or_else(|| invalid("Invalid visualization id"))?;
        let session = crate::inline_visualization::root_session(&self.ctx, ctx)
            .await
            .ok_or_else(|| invalid("No visualization session"))?;
        let revision = if let Some(revision) = input["visualization_revision"].as_u64() {
            u32::try_from(revision).map_err(invalid)?
        } else {
            store
                .list(session)
                .await
                .map_err(io)?
                .into_iter()
                .find(|row| row.id == id)
                .and_then(|row| row.revisions.last().map(|r| r.revision))
                .ok_or_else(|| invalid("Visualization was not found in this conversation"))?
        };
        let reference = VisualizationRef { id, revision };
        let stored = store.read_revision(session, &reference).await.map_err(io)?;
        let state = store.read_state(session, &reference).await.map_err(io)?;
        let name = input["name"].as_str().unwrap_or(&stored.title).to_string();
        let boot = serde_json::to_string(&state)
            .map_err(io)?
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('&', "\\u0026")
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029");
        let html =
            visualization::document::render_standalone_document(&name, &stored.fragment, &boot);
        input["name"] = json!(name);
        input["html"] = json!(html);
        input.as_object_mut().unwrap().remove("visualization_id");
        input
            .as_object_mut()
            .unwrap()
            .remove("visualization_revision");
        Ok(())
    }
}

#[async_trait]
impl Tool for DesktopLocalAppTool {
    fn name(&self) -> &str {
        "LocalApp"
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        &SCHEMA
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn native_input_validation(&self) -> bool {
        true
    }
    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("create local app 本地应用 application library backend".into())
    }
    fn is_read_only(&self, input: &Utf16JsonProjection) -> bool {
        matches!(
            input.string_at("/action"),
            Some("list" | "read" | "status" | "logs")
        )
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        300_000
    }
    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "Publish and run Local Apps: versioned multi-file projects, Node/Python services, native capabilities and independent application data.".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        source_text_projection(PROMPT)
    }
    async fn validate_input(
        &self,
        input: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let value: Value = serde_json::from_str(
            &input
                .to_json_string()
                .map_err(|e| ValidationError(e.to_string().into()))?,
        )
        .map_err(|e| ValidationError(e.to_string().into()))?;
        let object = value
            .as_object()
            .ok_or_else(|| ValidationError("Expected LocalApp input".into()))?;
        for key in object.keys() {
            if ![
                "action",
                "id",
                "name",
                "summary",
                "html",
                "file_path",
                "bundle_path",
                "visualization_id",
                "visualization_revision",
                "expectedRevision",
            ]
            .contains(&key.as_str())
            {
                return Err(ValidationError(
                    format!("Unknown LocalApp field: {key}").into(),
                ));
            }
        }
        for key in [
            "action",
            "id",
            "name",
            "summary",
            "html",
            "file_path",
            "bundle_path",
            "visualization_id",
        ] {
            if let Some(value) = object.get(key) {
                if !value.is_string() {
                    return Err(ValidationError(format!("{key} must be a string").into()));
                }
            }
        }
        if let Some(id) = value["id"].as_str() {
            if id.len() != 33
                || !id.starts_with('a')
                || !id[1..]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(ValidationError("Invalid desktop application id".into()));
            }
        }
        let action = value["action"].as_str().unwrap_or("publish");
        if value
            .get("expectedRevision")
            .is_some_and(|revision| revision.as_u64().is_none())
        {
            return Err(ValidationError(
                "expectedRevision must be a non-negative integer".into(),
            ));
        }
        if action == "publish"
            && value.get("id").is_some()
            && value.get("expectedRevision").is_none()
        {
            return Err(ValidationError(
                "Updating an application requires expectedRevision from its latest record".into(),
            ));
        }
        if ![
            "publish",
            "list",
            "read",
            "start",
            "stop",
            "status",
            "logs",
            "agent_catalog",
        ]
        .contains(&action)
        {
            return Err(ValidationError("Unknown LocalApp action".into()));
        }
        if action == "publish"
            && ["html", "file_path", "bundle_path", "visualization_id"]
                .iter()
                .filter(|key| value.get(**key).is_some())
                .count()
                != 1
        {
            return Err(ValidationError(
                "Publish needs exactly one of html, file_path, bundle_path or visualization_id"
                    .into(),
            ));
        }
        if action != "publish" && action != "list" && value["id"].as_str().is_none() {
            return Err(ValidationError(
                "This action needs the application id".into(),
            ));
        }
        Ok(())
    }
    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> PermissionResult {
        // Publication checks every source file with the live nested Read gate;
        // native execution additionally requires the host's app-owned grant.
        PermissionResult::Allow { reason: PermissionDecisionReason::Other { reason: "uses the Desktop application library; host grants and source Read permissions are checked separately".into() }, updated_input: None, update_destination: None, metadata: Default::default() }
    }
    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        check_cancelled(&ctx)?;
        // Pin workspace and trust roots together before the origin-row ACK or
        // any other await; an admitted request must not follow a later /cd.
        let source_scope = self.ctx.cwd_and_trusted();
        // Freeze the originating turn's owner before any asynchronous staging.
        // A later workspace/session retarget must not relabel this request.
        let originating_session = if let Some(origin) = &ctx.origin_session_id {
            Some(origin.as_uuid().to_string())
        } else if let Some(session) = &ctx.session {
            Some(session.lock().await.session_id.as_uuid().to_string())
        } else {
            self.ctx
                .session_id
                .as_ref()
                .map(|session| session.as_uuid().to_string())
        };
        let action = input.string_at("/action").unwrap_or("publish");
        if matches!(action, "publish" | "read") {
            let authorized = ctx.trusted_session_profile.as_ref().is_some_and(|profile| {
                profile.mode == lingxi_core::host::session_context::ConversationMode::Code
                    && originating_session.as_deref() == Some(profile.session_id.as_str())
            });
            if !authorized {
                return Err(invalid("local_app_development_requires_code: Local App development requires a Code session. Switch to Code to continue."));
            }
        }
        let publication_request_id = if action == "publish" {
            let session = originating_session.as_deref().ok_or_else(|| {
                invalid("local_app_origin_unavailable: session owner is unavailable")
            })?;
            let assistant = ctx
                .assistant_message_id
                .ok_or_else(|| {
                    invalid("local_app_origin_unavailable: assistant origin is unavailable")
                })?
                .as_uuid()
                .to_string();
            let tool = ctx.tool_use_id.as_ref().ok_or_else(|| {
                invalid("local_app_origin_unavailable: tool invocation is unavailable")
            })?;
            let origins = ctx.durable_tool_origins.as_ref().ok_or_else(|| {
                invalid("local_app_origin_unavailable: durable transcript is unavailable")
            })?;
            let acknowledge = async {
                loop {
                    let key = (tool.as_str().to_owned(), assistant.clone());
                    if let Some(result) = origins.sources.lock().await.remove(&key) {
                        break result.map_err(|error| {
                            invalid(format!("local_app_origin_unavailable: {error}"))
                        });
                    }
                    tokio::select! {
                        _ = origins.changed.notified() => {},
                        _ = tokio::time::sleep(Duration::from_millis(50)) => {},
                        _ = cancelled(&ctx) => return Err(ToolError::Aborted),
                    }
                }
            };
            let source_row = acknowledge.await?;
            Some(
                local_app::v2::native_tool_publication_request_id(
                    session,
                    &source_row,
                    tool.as_str(),
                )
                .map_err(invalid)?,
            )
        } else {
            None
        };
        let originating_project = source_scope.0.clone();
        self.validate_input(&input, &ctx).await.map_err(invalid)?;
        let mut input: Value =
            serde_json::from_str(&input.to_json_string().map_err(io)?).map_err(invalid)?;
        if input.get("action").is_none() {
            input["action"] = json!("publish");
        }
        let source_session = originating_session.as_ref().map(|session| {
            json!({
                "projectPath":originating_project,"sessionId":session,
                "mode":ctx.trusted_session_profile.as_ref().map(|profile| profile.mode.as_str()),
                "requestId":publication_request_id
            })
        });
        if action != "publish" {
            if let Some(session) = source_session {
                input["source_session"] = session;
            }
            return tokio::select! {
                biased;
                _ = cancelled(&ctx) => Err(ToolError::Aborted),
                result = self.host(&input) => result,
            };
        }
        let request_id = publication_request_id
            .as_deref()
            .ok_or_else(|| invalid("Publication request identity is unavailable"))?;
        let operation = async {
            let capture = Arc::new(PublicationCapture::open(&self.config_home, request_id).await?);
            let expected = json!({"version":1,"requestId":request_id,"request":input,
                "sourceSession":source_session});
            let existing = capture.read_async().await?;
            let (saved, fresh) = match existing {
                Some(saved) => {
                    let identity = &saved["identity"];
                    let matches = if saved["state"] == "ready" {
                        same_captured_invocation(identity, &expected)
                    } else {
                        identity == &expected
                    };
                    if !matches {
                        return Err(invalid(
                            "Local App publication request conflicts with its saved input",
                        ));
                    }
                    (saved, false)
                }
                None => {
                    let preparing = json!({"identity":expected,"state":"preparing"});
                    capture.write_async(preparing.clone(), false).await?;
                    (preparing, true)
                }
            };
            let host_input = if saved["state"] == "ready" {
                let host_input = saved
                    .get("hostInput")
                    .filter(|v| v.is_object())
                    .ok_or_else(|| invalid("Local App publication capture is invalid"))?
                    .clone();
                if host_input.get("source_session") != saved["identity"].get("sourceSession") {
                    return Err(invalid("Local App publication owner changed"));
                }
                if let Some(path) = host_input.get("staged_path") {
                    if path.as_str() != capture.staged.to_str() {
                        return Err(invalid("Local App publication staging path changed"));
                    }
                    let digest = saved["stagedDigest"]
                        .as_str()
                        .ok_or_else(|| invalid("Local App publication digest is missing"))?;
                    if capture.stage_exists()? {
                        let config = self.config_home.clone();
                        let staged = capture.staged.clone();
                        let observed = tokio::task::spawn_blocking(move || {
                            local_app::v2::AppLibrary::open(config)
                                .and_then(|library| library.fingerprint_publication(&staged))
                        })
                        .await
                        .map_err(io)?
                        .map_err(io)?;
                        if observed != digest {
                            return Err(invalid("Local App publication staged content changed"));
                        }
                    }
                }
                host_input
            } else if saved["state"] == "preparing" {
                if !fresh {
                    return Err(invalid(
                        "Local App publication capture is incomplete; its outcome is unknown",
                    ));
                }
                if capture.stage_exists()? {
                    return Err(invalid(
                        "Local App publication capture is incomplete; its outcome is unknown",
                    ));
                }
                let mut staging = None;
                let staged = &capture.staged;
                let mut prepared = input.clone();
                let mut staged_digest = None;
                if prepared.get("visualization_id").is_some() {
                    self.visualization_document(&mut prepared, &ctx).await?;
                }
                if let Some(path) = prepared["bundle_path"].as_str().map(str::to_owned) {
                    let source = self.source(&path, &source_scope)?;
                    if tokio::fs::canonicalize(&capture.imports)
                        .await
                        .map_err(io)?
                        .starts_with(&source)
                    {
                        return Err(invalid(
                            "Application source cannot contain its runtime imports directory",
                        ));
                    }
                    staging = Some(capture.create_stage()?);
                    self.stage(&source, staged, &ctx, &source_scope).await?;
                    let config = self.config_home.clone();
                    let staged_for_digest = staged.to_owned();
                    staged_digest = Some(
                        tokio::task::spawn_blocking(move || {
                            sync_publication_tree(&staged_for_digest)?;
                            local_app::v2::AppLibrary::open(config)
                                .and_then(|library| {
                                    library.fingerprint_publication(&staged_for_digest)
                                })
                                .map_err(io)
                        })
                        .await
                        .map_err(io)??,
                    );
                    prepared.as_object_mut().unwrap().remove("bundle_path");
                    prepared["staged_path"] = json!(staged);
                    prepared["source_path"] = json!(source);
                } else if let Some(path) = prepared["file_path"].as_str().map(str::to_owned) {
                    let source = self.source(&path, &source_scope)?;
                    let root = source
                        .parent()
                        .ok_or_else(|| invalid("Invalid application file"))?;
                    staging = Some(capture.create_stage()?);
                    self.copy_file(
                        root,
                        Path::new(source.file_name().unwrap()),
                        &staged.join("index.html"),
                        &ctx,
                        Some(MAX_HOST_REQUEST_BYTES as u64),
                        &source_scope,
                    )
                    .await?;
                    prepared["html"] = json!(tokio::fs::read_to_string(staged.join("index.html"))
                        .await
                        .map_err(io)?);
                    prepared.as_object_mut().unwrap().remove("file_path");
                    prepared["source_path"] = json!(root);
                }
                if let Some(session) = &source_session {
                    prepared["source_session"] = session.clone();
                }
                request_body(&prepared, MAX_HOST_REQUEST_BYTES)?;
                check_cancelled(&ctx)?;
                // Even an interrupted capture is retained. Only this fresh
                // invocation may collect bytes; later retries require a ready record.
                capture
                    .write_async(
                        json!({"identity":expected,"state":"ready",
                    "hostInput":prepared,"stagedDigest":staged_digest}),
                        true,
                    )
                    .await?;
                drop(staging);
                prepared
            } else {
                return Err(invalid("Local App publication capture is invalid"));
            };
            capture.validate()?;
            check_cancelled(&ctx)?;
            self.host(&host_input).await
        };
        // Host ownership may be unknown when this future is cancelled. Even
        // after success, Mobile currently needs these staged bytes to check a
        // replay; leave them for a future explicit, receipt-aware cleanup.
        tokio::select! {
            biased;
            _ = cancelled(&ctx) => Err(ToolError::Aborted),
            result = operation => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::permission_gate::{PermissionDecision, PermissionGate};
    use mobile_linux_api::ProcessOutput;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};

    #[test]
    fn staging_filter_keeps_examples_and_omits_private_entries() {
        for name in [
            ".git",
            "node_modules",
            ".venv",
            "venv",
            "__pycache__",
            ".DS_Store",
            branding::LOCAL_APP_PREPARATION_MARKER,
            ".env",
            ".env.production",
            ".env.example.local",
        ] {
            assert!(excluded_staging_entry(name), "{name}");
        }
        for name in [
            "index.html".to_owned(),
            ".env.example".to_owned(),
            ".envish".to_owned(),
            format!("{}.backup", branding::LOCAL_APP_PREPARATION_MARKER),
        ] {
            assert!(!excluded_staging_entry(&name), "{name}");
        }
    }

    struct ReadGate {
        deny: bool,
        replace: Option<(PathBuf, PathBuf)>,
    }
    #[async_trait]
    impl PermissionGate for ReadGate {
        async fn check(&self, name: &str, _: &Utf16JsonProjection) -> PermissionDecision {
            assert_eq!(name, "Read");
            if let Some((file, target)) = &self.replace {
                #[cfg(unix)]
                {
                    std::fs::remove_file(file).unwrap();
                    std::os::unix::fs::symlink(target, file).unwrap();
                }
            }
            if self.deny {
                PermissionDecision::Deny {
                    reason: "source is private".into(),
                }
            } else {
                PermissionDecision::Allow
            }
        }
    }
    fn tool(root: &Path, gate: Arc<dyn PermissionGate>) -> DesktopLocalAppTool {
        let mut ctx = shell_test_ctx(ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        });
        ctx.session_cwd = tool_api::SessionCwd::new(root.to_owned(), vec![root.to_owned()]);
        ctx.permission_gate = Some(gate);
        DesktopLocalAppTool::new(ctx, root.join("home"), None)
    }
    fn call_context() -> ToolUseContext {
        let mut ctx = fresh_ctx();
        ctx.tool_use_id = Some(lingxi_core::types::ToolUseId::new());
        let assistant = lingxi_core::types::MessageId::new();
        ctx.assistant_message_id = Some(assistant);
        ctx.durable_tool_origins = Some(tool_api::context::DurableToolOrigins {
            sources: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::from([
                (
                    (
                        ctx.tool_use_id.as_ref().unwrap().as_str().to_owned(),
                        assistant.as_uuid().to_string(),
                    ),
                    Ok(assistant.as_uuid().to_string()),
                ),
            ]))),
            changed: Arc::new(tokio::sync::Notify::new()),
        });
        let owner = lingxi_core::types::SessionId::new();
        ctx.origin_session_id = Some(owner);
        ctx.trusted_session_profile =
            Some(lingxi_core::host::session_context::SessionModeSnapshot {
                session_id: owner.as_uuid().to_string(),
                mode: lingxi_core::host::session_context::ConversationMode::Code,
            });
        ctx
    }

    fn staged_count(imports: &Path) -> usize {
        std::fs::read_dir(imports)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with("tool-publish-")
                            && entry.file_type().is_ok_and(|kind| kind.is_dir())
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    fn write_bundle_manifest(source: &Path) {
        std::fs::write(source.join("lingxi-app.json"), json!({
            "schema":2,"name":"Test app","entry":{"type":"static","path":"index.html"},
            "targets":[{"platform":"ios","environment":"web"},{"platform":"android","environment":"web"},
                {"platform":"macos","environment":"web"}],
            "package":{"files":["index.html"],"dependencyLocks":[]}
        }).to_string()).unwrap();
    }

    struct UnusedProjectHost;

    #[async_trait]
    impl ProjectLocalAppHost for UnusedProjectHost {
        async fn request(&self, _: Value) -> Result<Value, String> {
            panic!("a prompt lookup must not call the project host")
        }
    }

    #[tokio::test]
    async fn desktop_and_mobile_tool_prompts_include_scoped_ios_recovery() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let gate = || {
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            })
        };
        let desktop = tool(&root, gate());
        let mobile = tool(&root, gate()).with_host(Arc::new(UnusedProjectHost));
        let options = PromptOptions::default();
        let desktop_prompt = desktop.prompt(&options).await;
        let mobile_prompt = mobile.prompt(&options).await;
        let desktop_text = desktop_prompt.string_at("").unwrap();
        let mobile_text = mobile_prompt.string_at("").unwrap();
        assert_eq!(desktop_text, mobile_text);
        for requirement in [
            "require the user's existing Code session",
            "user must switch to Code",
            "do not switch sessions, create a Code session or delegate",
            "On iOS, opening the App prepares it in the foreground",
            "finite, revocable background lease",
            "termination, force quit and device restart",
            "fresh service URLs",
            "same saved Agent run and thread read-only",
            "never automatically replay requests or tool actions",
            "user's explicit choice",
        ] {
            assert!(mobile_text.contains(requirement), "missing: {requirement}");
        }
    }

    #[tokio::test]
    async fn document_copy_accepts_the_limit_and_rejects_oversized_files_before_copying() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        );
        let source = root.join("index.html");
        let output = root.join("copy.html");
        std::fs::write(&source, "12345678").unwrap();
        tool.copy_file(
            &root,
            Path::new("index.html"),
            &output,
            &call_context(),
            Some(8),
            &tool.ctx.cwd_and_trusted(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "12345678");
        std::fs::remove_file(&output).unwrap();
        std::fs::write(source, "123456789").unwrap();
        assert!(matches!(
            tool.copy_file(
                &root,
                Path::new("index.html"),
                &output,
                &call_context(),
                Some(8),
                &tool.ctx.cwd_and_trusted()
            )
            .await,
            Err(ToolError::FileTooLarge { size: 9, limit: 8 })
        ));
        assert!(!output.exists());
    }

    #[test]
    fn host_request_limit_counts_encoded_json_bytes_including_escapes() {
        let value = json!({"html":"\n\"\\"});
        let encoded = value.to_string();
        assert_eq!(request_body(&value, encoded.len()).unwrap(), encoded);
        assert!(matches!(
            request_body(&value, encoded.len() - 1),
            Err(ToolError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn source_read_denial_prevents_copy_even_when_local_app_is_allowed() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        std::fs::write(root.join("index.html"), "private").unwrap();
        let tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: true,
                replace: None,
            }),
        );
        let output = root.join("copy.html");
        assert!(tool
            .copy_file(
                &root,
                Path::new("index.html"),
                &output,
                &call_context(),
                None,
                &tool.ctx.cwd_and_trusted()
            )
            .await
            .is_err());
        assert!(!output.exists());
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn read_approval_cannot_retarget_source_to_a_symlink() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        std::fs::write(root.join("index.html"), "approved").unwrap();
        std::fs::write(root.join("private.html"), "private").unwrap();
        let tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: Some((root.join("index.html"), root.join("private.html"))),
            }),
        );
        let output = root.join("copy.html");
        assert!(tool
            .copy_file(
                &root,
                Path::new("index.html"),
                &output,
                &call_context(),
                None,
                &tool.ctx.cwd_and_trusted()
            )
            .await
            .is_err());
        assert!(!output.exists());
    }
    #[tokio::test]
    async fn bundle_omits_dependency_trees_and_dotenv_secrets() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let source = root.join("project");
        let dest = root.join("stage");
        std::fs::create_dir_all(source.join("node_modules")).unwrap();
        std::fs::create_dir_all(source.join(".python-env/bin")).unwrap();
        std::fs::write(source.join(".python-env/pyvenv.cfg"), "home = /python").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            source.join("index.html"),
            source.join(".python-env/bin/python"),
        )
        .unwrap();
        for (file, text) in [
            ("index.html", "app"),
            ("pyvenv.cfg", "project-owned root file"),
            (".env", "secret"),
            (".env.example", "TOKEN="),
            (branding::LOCAL_APP_PREPARATION_MARKER, "{\"revision\":1}"),
            ("node_modules/cache", "dependency"),
        ] {
            std::fs::write(source.join(file), text).unwrap();
        }
        let tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        );
        tool.stage(&source, &dest, &call_context(), &tool.ctx.cwd_and_trusted())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("index.html")).unwrap(),
            "app"
        );
        assert!(dest.join(".env.example").exists());
        assert!(!dest.join(".env").exists());
        assert!(!dest.join(branding::LOCAL_APP_PREPARATION_MARKER).exists());
        assert!(!dest.join("node_modules").exists());
        assert!(!dest.join(".python-env").exists());
        assert!(dest.join("pyvenv.cfg").exists());
    }
    #[tokio::test]
    #[cfg(unix)]
    async fn bundle_preserves_executable_scripts_without_copying_public_or_special_mode_bits() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let source = root.join("project");
        let dest = root.join("stage");
        std::fs::create_dir_all(&source).unwrap();
        for (name, mode, contents) in [
            ("install.sh", 0o755, "#!/bin/sh\nexit 0\n"),
            ("settings.json", 0o644, "{}"),
        ] {
            let path = source.join(name);
            std::fs::write(&path, contents).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        );
        tool.stage(&source, &dest, &call_context(), &tool.ctx.cwd_and_trusted())
            .await
            .unwrap();
        assert!(std::process::Command::new(dest.join("install.sh"))
            .status()
            .unwrap()
            .success());
        for (name, expected) in [("install.sh", 0o700), ("settings.json", 0o600)] {
            assert_eq!(
                std::fs::metadata(dest.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o7777,
                expected
            );
        }
    }

    #[tokio::test]
    async fn a_bundle_cannot_recursively_import_its_own_staging_directory() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        );
        let result = tool
            .call(
                generated_builtin_schema(&json!({"bundle_path":root})),
                call_context(),
                fresh_tx(),
            )
            .await;
        assert!(
            matches!(result, Err(ToolError::InvalidInput(ref text)) if text.string_at("").is_some_and(|text| text.contains("imports directory")))
        );
        assert_eq!(staged_count(&tool.config_home.join("local-app-imports")), 0);
    }

    struct AfterOneReadGate {
        entered: Arc<tokio::sync::Notify>,
        reads: std::sync::atomic::AtomicUsize,
    }

    struct PausedReadGate {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl PermissionGate for PausedReadGate {
        async fn check(&self, _: &str, _: &Utf16JsonProjection) -> PermissionDecision {
            self.entered.notify_one();
            self.release.notified().await;
            PermissionDecision::Allow
        }
    }

    struct CaptureProjectHost(Arc<tokio::sync::Mutex<Option<Value>>>);
    #[async_trait]
    impl ProjectLocalAppHost for CaptureProjectHost {
        async fn request(&self, value: Value) -> Result<Value, String> {
            *self.0.lock().await = Some(value);
            Ok(json!({"ok":true}))
        }
    }

    fn acknowledged_replay(
        base: &ToolUseContext,
        source_row: &str,
        new_assistant: bool,
    ) -> ToolUseContext {
        let mut ctx = base.clone();
        if new_assistant {
            ctx.assistant_message_id = Some(lingxi_core::types::MessageId::new());
        }
        let key = (
            ctx.tool_use_id.as_ref().unwrap().as_str().to_owned(),
            ctx.assistant_message_id.unwrap().as_uuid().to_string(),
        );
        ctx.durable_tool_origins = Some(tool_api::context::DurableToolOrigins {
            sources: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::from([
                (key, Ok(source_row.to_owned())),
            ]))),
            changed: Arc::new(tokio::sync::Notify::new()),
        });
        ctx
    }

    struct ReplayHost {
        calls: tokio::sync::Mutex<Vec<(Value, String)>>,
        take_first: bool,
    }
    #[async_trait]
    impl ProjectLocalAppHost for ReplayHost {
        async fn request(&self, input: Value) -> Result<Value, String> {
            let mut calls = self.calls.lock().await;
            let content = if let Some(path) = input["staged_path"].as_str() {
                let path = PathBuf::from(path);
                let transferred = path.with_file_name("host-taken-source");
                let existing = if path.exists() { &path } else { &transferred };
                let content = std::fs::read_to_string(existing.join("index.html"))
                    .map_err(|e| e.to_string())?;
                if self.take_first && calls.is_empty() {
                    std::fs::rename(path, transferred).map_err(|e| e.to_string())?;
                }
                content
            } else {
                input["html"].as_str().unwrap_or_default().to_owned()
            };
            calls.push((input, content));
            if calls.len() == 1 {
                Err("host outcome unknown".into())
            } else {
                Ok(json!({"ok":true}))
            }
        }
    }

    #[tokio::test]
    async fn publication_retries_reuse_original_bundle_for_main_deferred_and_nested_origins() {
        for origin in ["main", "deferred", "nested"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let source = root.join("project");
            std::fs::create_dir(&source).unwrap();
            std::fs::write(source.join("index.html"), "original").unwrap();
            write_bundle_manifest(&source);
            let host = Arc::new(ReplayHost {
                calls: tokio::sync::Mutex::new(Vec::new()),
                take_first: false,
            });
            let tool = tool(
                &root,
                Arc::new(ReadGate {
                    deny: false,
                    replace: None,
                }),
            )
            .with_host(host.clone());
            let mut ctx = call_context();
            if origin == "main" {
                let id = ctx.origin_session_id.take().unwrap();
                ctx.session = Some(Arc::new(tokio::sync::Mutex::new(
                    lingxi_core::SessionState::empty(id, "model".into()),
                )));
            }
            let source_row = ctx.assistant_message_id.unwrap().as_uuid().to_string();
            let input = json!({"bundle_path":source});
            assert!(tool
                .call(generated_builtin_schema(&input), ctx.clone(), fresh_tx())
                .await
                .is_err());
            std::fs::write(source.join("index.html"), "changed after admission").unwrap();
            let retry = acknowledged_replay(&ctx, &source_row, origin == "deferred");
            tool.call(generated_builtin_schema(&input), retry, fresh_tx())
                .await
                .unwrap();
            let calls = host.calls.lock().await;
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].1, "original");
            assert_eq!(calls[1].1, "original");
            assert_eq!(calls[0].0, calls[1].0);
            drop(calls);
            let changed = json!({"bundle_path":source,"summary":"different input"});
            let retry = acknowledged_replay(&ctx, &source_row, false);
            assert!(tool
                .call(generated_builtin_schema(&changed), retry, fresh_tx())
                .await
                .is_err());
            assert_eq!(host.calls.lock().await.len(), 2);
        }
    }

    #[tokio::test]
    async fn ready_retry_after_cwd_change_keeps_the_original_workspace_and_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let a = root.join("workspace-a");
        let b = root.join("workspace-b");
        for (path, text) in [(&a, "source from A"), (&b, "source from B")] {
            std::fs::create_dir_all(path.join("project")).unwrap();
            std::fs::write(path.join("project/index.html"), text).unwrap();
            write_bundle_manifest(&path.join("project"));
        }
        let host = Arc::new(ReplayHost {
            calls: tokio::sync::Mutex::new(Vec::new()),
            take_first: false,
        });
        let mut tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        )
        .with_host(host.clone());
        tool.ctx.session_cwd = tool_api::SessionCwd::new(a.clone(), vec![a.clone()]);
        let ctx = call_context();
        let row = ctx.assistant_message_id.unwrap().as_uuid().to_string();
        let input = json!({"bundle_path":"project"});
        assert!(tool
            .call(generated_builtin_schema(&input), ctx.clone(), fresh_tx())
            .await
            .is_err());
        tool.ctx.session_cwd.swap(b.clone(), vec![b]);
        tool.call(
            generated_builtin_schema(&input),
            acknowledged_replay(&ctx, &row, false),
            fresh_tx(),
        )
        .await
        .unwrap();
        let calls = host.calls.lock().await;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], calls[1]);
        assert_eq!(calls[1].0["source_session"]["projectPath"], json!(a));
        assert_eq!(calls[1].1, "source from A");
    }

    #[tokio::test]
    async fn retry_uses_original_path_after_host_takes_the_staged_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("project");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("index.html"), "original").unwrap();
        write_bundle_manifest(&source);
        let host = Arc::new(ReplayHost {
            calls: tokio::sync::Mutex::new(Vec::new()),
            take_first: true,
        });
        let tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        )
        .with_host(host.clone());
        let ctx = call_context();
        let row = ctx.assistant_message_id.unwrap().as_uuid().to_string();
        let input = json!({"bundle_path":source});
        assert!(tool
            .call(generated_builtin_schema(&input), ctx.clone(), fresh_tx())
            .await
            .is_err());
        std::fs::write(source.join("index.html"), "changed").unwrap();
        tool.call(
            generated_builtin_schema(&input),
            acknowledged_replay(&ctx, &row, false),
            fresh_tx(),
        )
        .await
        .unwrap();
        let calls = host.calls.lock().await;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], calls[1]);
        assert_eq!(calls[1].1, "original");
    }

    #[tokio::test]
    async fn document_retry_preserves_the_authorized_original_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("index.html");
        std::fs::write(&source, "original").unwrap();
        let host = Arc::new(ReplayHost {
            calls: tokio::sync::Mutex::new(Vec::new()),
            take_first: false,
        });
        let tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        )
        .with_host(host.clone());
        let ctx = call_context();
        let row = ctx.assistant_message_id.unwrap().as_uuid().to_string();
        let input = json!({"file_path":source});
        assert!(tool
            .call(generated_builtin_schema(&input), ctx.clone(), fresh_tx())
            .await
            .is_err());
        std::fs::write(&source, "changed").unwrap();
        tool.call(
            generated_builtin_schema(&input),
            acknowledged_replay(&ctx, &row, false),
            fresh_tx(),
        )
        .await
        .unwrap();
        let calls = host.calls.lock().await;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], calls[1]);
        assert_eq!(calls[1].1, "original");
    }

    #[tokio::test]
    async fn changed_staged_bytes_are_rejected_without_calling_the_host_again() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("project");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("index.html"), "original").unwrap();
        write_bundle_manifest(&source);
        let host = Arc::new(ReplayHost {
            calls: tokio::sync::Mutex::new(Vec::new()),
            take_first: false,
        });
        let tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        )
        .with_host(host.clone());
        let ctx = call_context();
        let row = ctx.assistant_message_id.unwrap().as_uuid().to_string();
        let input = json!({"bundle_path":source});
        assert!(tool
            .call(generated_builtin_schema(&input), ctx.clone(), fresh_tx())
            .await
            .is_err());
        let staged = host.calls.lock().await[0].0["staged_path"]
            .as_str()
            .unwrap()
            .to_owned();
        std::fs::write(Path::new(&staged).join("index.html"), "tampered").unwrap();
        assert!(tool
            .call(
                generated_builtin_schema(&input),
                acknowledged_replay(&ctx, &row, false),
                fresh_tx()
            )
            .await
            .is_err());
        assert_eq!(host.calls.lock().await.len(), 1);
        assert_eq!(
            std::fs::read_to_string(Path::new(&staged).join("index.html")).unwrap(),
            "tampered"
        );
    }

    #[tokio::test]
    async fn uncertain_ready_record_write_preserves_the_exact_capture_for_retry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("project");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("index.html"), "original").unwrap();
        write_bundle_manifest(&source);
        let host = Arc::new(ReplayHost {
            calls: tokio::sync::Mutex::new(Vec::new()),
            take_first: false,
        });
        let tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        )
        .with_host(host.clone());
        let imports = tool.config_home.join("local-app-imports");
        std::fs::create_dir_all(&imports).unwrap();
        let fault = imports.join(".test-ready-write-uncertain");
        std::fs::write(&fault, "fault").unwrap();
        let ctx = call_context();
        let row = ctx.assistant_message_id.unwrap().as_uuid().to_string();
        let input = json!({"bundle_path":source});
        assert!(tool
            .call(generated_builtin_schema(&input), ctx.clone(), fresh_tx())
            .await
            .is_err());
        assert_eq!(host.calls.lock().await.len(), 0);
        assert_eq!(staged_count(&imports), 1);
        std::fs::remove_file(fault).unwrap();
        std::fs::write(source.join("index.html"), "changed").unwrap();
        // The fake host deliberately loses its first response, after seeing the
        // bytes recovered from the landed record.
        assert!(tool
            .call(
                generated_builtin_schema(&input),
                acknowledged_replay(&ctx, &row, false),
                fresh_tx()
            )
            .await
            .is_err());
        assert_eq!(host.calls.lock().await[0].1, "original");
        tool.call(
            generated_builtin_schema(&input),
            acknowledged_replay(&ctx, &row, false),
            fresh_tx(),
        )
        .await
        .unwrap();
        assert_eq!(host.calls.lock().await[1].1, "original");
    }

    #[tokio::test]
    async fn interrupted_preparing_capture_is_preserved_and_never_recaptured() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("project");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("index.html"), "new source").unwrap();
        write_bundle_manifest(&source);
        let host = Arc::new(ReplayHost {
            calls: tokio::sync::Mutex::new(Vec::new()),
            take_first: false,
        });
        let tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        )
        .with_host(host.clone());
        let ctx = call_context();
        let session = ctx.origin_session_id.unwrap().as_uuid().to_string();
        let row = ctx.assistant_message_id.unwrap().as_uuid().to_string();
        let id = local_app::v2::native_tool_publication_request_id(
            &session,
            &row,
            ctx.tool_use_id.as_ref().unwrap().as_str(),
        )
        .unwrap();
        let input = json!({"action":"publish","bundle_path":source});
        let session_value = json!({"projectPath":tool.ctx.cwd(),"sessionId":session,
            "mode":"code","requestId":id});
        let capture = PublicationCapture::open(&tool.config_home, &id)
            .await
            .unwrap();
        capture
            .write(
                &json!({"identity":{"version":1,"requestId":id,"request":input,
            "sourceSession":session_value},"state":"preparing"}),
                false,
            )
            .unwrap();
        let partial = capture.create_stage().unwrap();
        std::fs::write(capture.staged.join("partial"), "original bytes").unwrap();
        drop(partial);
        let staged = capture.staged.clone();
        drop(capture);
        assert!(tool
            .call(generated_builtin_schema(&input), ctx.clone(), fresh_tx())
            .await
            .is_err());
        assert_eq!(
            std::fs::read_to_string(staged.join("partial")).unwrap(),
            "original bytes"
        );
        let other = root.join("other-workspace");
        std::fs::create_dir(&other).unwrap();
        tool.ctx.session_cwd.swap(other.clone(), vec![other]);
        assert!(tool
            .call(
                generated_builtin_schema(&input),
                acknowledged_replay(&ctx, &row, false),
                fresh_tx()
            )
            .await
            .is_err());
        assert_eq!(
            std::fs::read_to_string(staged.join("partial")).unwrap(),
            "original bytes"
        );
        assert!(host.calls.lock().await.is_empty());
    }

    #[tokio::test]
    async fn existing_preparing_without_stage_does_not_recapture_or_call_host() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("project");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("index.html"), "changed source").unwrap();
        write_bundle_manifest(&source);
        let host = Arc::new(ReplayHost {
            calls: tokio::sync::Mutex::new(Vec::new()),
            take_first: false,
        });
        let tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        )
        .with_host(host.clone());
        let ctx = call_context();
        let session = ctx.origin_session_id.unwrap().as_uuid().to_string();
        let row = ctx.assistant_message_id.unwrap().as_uuid().to_string();
        let id = local_app::v2::native_tool_publication_request_id(
            &session,
            &row,
            ctx.tool_use_id.as_ref().unwrap().as_str(),
        )
        .unwrap();
        let input = json!({"action":"publish","bundle_path":source});
        let source_session = json!({"projectPath":tool.ctx.cwd(),"sessionId":session,
            "mode":"code","requestId":id});
        let capture = PublicationCapture::open(&tool.config_home, &id)
            .await
            .unwrap();
        capture
            .write(
                &json!({"identity":{"version":1,"requestId":id,"request":input,
                    "sourceSession":source_session},"state":"preparing"}),
                false,
            )
            .unwrap();
        let staged = capture.staged.clone();
        drop(capture);

        let result = tool
            .call(
                generated_builtin_schema(&input),
                acknowledged_replay(&ctx, &row, true),
                fresh_tx(),
            )
            .await;
        assert!(matches!(result, Err(ToolError::InvalidInput(ref reason))
            if reason.string_at("").is_some_and(|s| s.contains("capture is incomplete"))));
        assert!(!staged.exists());
        assert!(host.calls.lock().await.is_empty());
    }

    struct PausedProjectHost {
        entered: Arc<tokio::sync::Notify>,
        input: Arc<tokio::sync::Mutex<Option<Value>>>,
    }
    #[async_trait]
    impl ProjectLocalAppHost for PausedProjectHost {
        async fn request(&self, input: Value) -> Result<Value, String> {
            *self.input.lock().await = Some(input);
            self.entered.notify_one();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn cancel_during_host_request_leaves_retry_bound_to_original_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("project");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("index.html"), "original").unwrap();
        write_bundle_manifest(&source);
        let entered = Arc::new(tokio::sync::Notify::new());
        let recorded = Arc::new(tokio::sync::Mutex::new(None));
        let host = Arc::new(PausedProjectHost {
            entered: entered.clone(),
            input: recorded.clone(),
        });
        let first_tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        )
        .with_host(host);
        let mut ctx = call_context();
        ctx.cancel = Some(Default::default());
        let cancel = ctx.cancel.clone().unwrap();
        let row = ctx.assistant_message_id.unwrap().as_uuid().to_string();
        let input = json!({"bundle_path":source});
        let first_input = input.clone();
        let first_ctx = ctx.clone();
        let task = tokio::spawn(async move {
            first_tool
                .call(
                    generated_builtin_schema(&first_input),
                    first_ctx,
                    fresh_tx(),
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        cancel.cancel();
        assert!(matches!(task.await.unwrap(), Err(ToolError::Aborted)));
        let captured = recorded.lock().await.clone().unwrap();
        assert!(Path::new(captured["staged_path"].as_str().unwrap()).is_dir());
        std::fs::write(source.join("index.html"), "changed").unwrap();
        let host = Arc::new(ReplayHost {
            calls: tokio::sync::Mutex::new(Vec::new()),
            take_first: false,
        });
        let retry_tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        )
        .with_host(host.clone());
        let mut retry = acknowledged_replay(&ctx, &row, false);
        retry.cancel = None;
        assert!(retry_tool
            .call(generated_builtin_schema(&input), retry, fresh_tx())
            .await
            .is_err());
        assert_eq!(
            host.calls.lock().await[0],
            (captured, "original".to_owned())
        );
    }

    #[tokio::test]
    async fn originating_session_is_captured_before_asynchronous_staging() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("project");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("index.html"), "app").unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let captured = Arc::new(tokio::sync::Mutex::new(None));
        let tool = tool(
            &root,
            Arc::new(PausedReadGate {
                entered: entered.clone(),
                release: release.clone(),
            }),
        )
        .with_host(Arc::new(CaptureProjectHost(captured.clone())));
        let first = lingxi_core::types::SessionId::new();
        let second = lingxi_core::types::SessionId::new();
        let session = Arc::new(tokio::sync::Mutex::new(lingxi_core::SessionState::empty(
            first,
            "model".into(),
        )));
        let mut ctx = call_context();
        ctx.origin_session_id = None;
        ctx.trusted_session_profile.as_mut().unwrap().session_id = first.as_uuid().to_string();
        ctx.session = Some(session.clone());
        let task = tokio::spawn(async move {
            tool.call(
                generated_builtin_schema(&json!({"file_path":source.join("index.html")})),
                ctx,
                fresh_tx(),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        session.lock().await.session_id = second;
        release.notify_one();
        task.await.unwrap().unwrap();
        let value = captured.lock().await.clone().unwrap();
        assert_eq!(
            value["source_session"]["sessionId"],
            first.as_uuid().to_string()
        );
        assert_eq!(value["source_session"]["mode"], "code");
    }

    #[tokio::test]
    async fn awaiting_durable_origin_does_not_retarget_workspace_or_source() {
        use std::future::poll_fn;
        use std::task::Poll;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let a = root.join("workspace-a");
        let b = root.join("workspace-b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        std::fs::write(a.join("index.html"), "source from A").unwrap();
        std::fs::write(b.join("index.html"), "source from B").unwrap();
        let captured = Arc::new(tokio::sync::Mutex::new(None));
        let mut tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        )
        .with_host(Arc::new(CaptureProjectHost(captured.clone())));
        tool.ctx.session_cwd = tool_api::SessionCwd::new(a.clone(), vec![a.clone()]);
        let workspace = tool.ctx.session_cwd.clone();
        let ctx = call_context();
        let origins = ctx.durable_tool_origins.as_ref().unwrap().clone();
        let row = ctx.assistant_message_id.unwrap().as_uuid().to_string();
        let key = (
            ctx.tool_use_id.as_ref().unwrap().as_str().to_owned(),
            row.clone(),
        );
        origins.sources.lock().await.clear();
        let mut call = tool.call(
            generated_builtin_schema(&json!({"file_path":"index.html"})),
            ctx,
            fresh_tx(),
        );
        // Poll to the durable-origin wait before changing the live workspace.
        poll_fn(|cx| {
            assert!(call.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        workspace.swap(b.clone(), vec![b]);
        origins.sources.lock().await.insert(key, Ok(row));
        origins.changed.notify_one();
        call.await.unwrap();
        let value = captured.lock().await.clone().unwrap();
        assert_eq!(value["source_session"]["projectPath"], json!(a));
        assert_eq!(value["source_path"], json!(a));
        assert_eq!(value["html"], "source from A");
    }

    #[tokio::test]
    async fn chat_cannot_publish_or_create_staging_but_can_browse_apps() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let source = root.join("project");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("index.html"), "app").unwrap();
        let captured = Arc::new(tokio::sync::Mutex::new(None));
        let tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        )
        .with_host(Arc::new(CaptureProjectHost(captured.clone())));
        let mut ctx = call_context();
        ctx.trusted_session_profile.as_mut().unwrap().mode =
            lingxi_core::host::session_context::ConversationMode::Chat;
        assert!(tool
            .call(
                generated_builtin_schema(&json!({"bundle_path":source})),
                ctx.clone(),
                fresh_tx()
            )
            .await
            .is_err());
        assert!(!tool.config_home.join("local-app-imports").exists());
        assert!(captured.lock().await.is_none());
        tool.call(
            generated_builtin_schema(&json!({"action":"list"})),
            ctx,
            fresh_tx(),
        )
        .await
        .unwrap();
        assert_eq!(captured.lock().await.as_ref().unwrap()["action"], "list");
    }
    #[async_trait]
    impl PermissionGate for AfterOneReadGate {
        async fn check(&self, _: &str, _: &Utf16JsonProjection) -> PermissionDecision {
            if self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                return PermissionDecision::Allow;
            }
            self.entered.notify_one();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn interrupted_partial_capture_cannot_recollect_changed_source_on_deferred_replay() {
        for drop_future in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let root = root.path().canonicalize().unwrap();
            let source = root.join("project");
            std::fs::create_dir(&source).unwrap();
            std::fs::write(source.join("first.txt"), "first original").unwrap();
            std::fs::write(source.join("second.txt"), "second original").unwrap();
            let entered = Arc::new(tokio::sync::Notify::new());
            let first_tool = tool(
                &root,
                Arc::new(AfterOneReadGate {
                    entered: entered.clone(),
                    reads: std::sync::atomic::AtomicUsize::new(0),
                }),
            );
            let imports = first_tool.config_home.join("local-app-imports");
            let mut ctx = call_context();
            ctx.cancel = Some(Default::default());
            let cancel = ctx.cancel.clone().unwrap();
            let row = ctx.assistant_message_id.unwrap().as_uuid().to_string();
            let input = json!({"bundle_path":source});
            let first_input = input.clone();
            let first_ctx = ctx.clone();
            let task = tokio::spawn(async move {
                first_tool
                    .call(
                        generated_builtin_schema(&first_input),
                        first_ctx,
                        fresh_tx(),
                    )
                    .await
            });
            tokio::time::timeout(Duration::from_secs(5), entered.notified())
                .await
                .unwrap();
            assert_eq!(staged_count(&imports), 1);
            let stage = std::fs::read_dir(&imports)
                .unwrap()
                .filter_map(Result::ok)
                .find(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("tool-publish-")
                        && entry.file_type().unwrap().is_dir()
                })
                .unwrap()
                .path();
            let partial = std::fs::read_dir(&stage)
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| (entry.file_name(), std::fs::read(entry.path()).unwrap()))
                .collect::<Vec<_>>();
            assert_eq!(partial.len(), 1, "the first source file was fully copied");
            assert!(
                partial[0].1.as_slice() == b"first original"
                    || partial[0].1.as_slice() == b"second original"
            );
            if drop_future {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                cancel.cancel();
                assert!(matches!(task.await.unwrap(), Err(ToolError::Aborted)));
            }
            std::fs::write(source.join("first.txt"), "first changed").unwrap();
            std::fs::write(source.join("second.txt"), "second changed").unwrap();
            let host = Arc::new(ReplayHost {
                calls: tokio::sync::Mutex::new(Vec::new()),
                take_first: false,
            });
            let retry_tool = tool(
                &root,
                Arc::new(ReadGate {
                    deny: false,
                    replace: None,
                }),
            )
            .with_host(host.clone());
            let mut retry = acknowledged_replay(&ctx, &row, true);
            retry.cancel = None;
            let result = retry_tool
                .call(generated_builtin_schema(&input), retry, fresh_tx())
                .await;
            assert!(matches!(result, Err(ToolError::InvalidInput(ref reason))
                if reason.string_at("").is_some_and(|s| s.contains("capture is incomplete"))));
            assert!(host.calls.lock().await.is_empty());
            assert_eq!(staged_count(&imports), 1);
            assert_eq!(
                std::fs::read(stage.join(&partial[0].0)).unwrap(),
                partial[0].1
            );
        }
    }

    #[test]
    fn a_dropped_tool_cannot_leave_a_queued_mkdir_that_recreates_its_imports() {
        use std::future::poll_fn;
        use std::task::Poll;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let source = root.join("project");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("index.html"), "app").unwrap();
        let tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        );
        let imports = tool.config_home.join("local-app-imports");
        runtime.block_on(async {
            let (entered, started) = tokio::sync::oneshot::channel();
            let (release, blocked) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                entered.send(()).unwrap();
                let _ = blocked.recv();
            });
            started.await.unwrap();
            let mut call = tool.call(
                generated_builtin_schema(&json!({"bundle_path":source})),
                call_context(),
                fresh_tx(),
            );
            // Poll exactly through the first pending filesystem operation.
            // The occupied pool ensures no deferred filesystem work has run.
            poll_fn(|cx| {
                assert!(call.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            drop(call);
            release.send(()).unwrap();
            blocker.await.unwrap();
            // FIFO drain after the tool's queued filesystem job.
            tokio::task::spawn_blocking(|| {}).await.unwrap();
            let remaining = staged_count(&imports);
            assert_eq!(remaining, 0);
        });
    }

    struct PendingHttp {
        entered: Arc<tokio::sync::Notify>,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }
    #[async_trait]
    impl lingxi_core::host::http::HttpTransport for PendingHttp {
        async fn request(
            &self,
            _: HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::http::HttpError> {
            struct RequestGuard(Arc<std::sync::atomic::AtomicBool>);
            impl Drop for RequestGuard {
                fn drop(&mut self) {
                    self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let _guard = RequestGuard(self.dropped.clone());
            self.entered.notify_one();
            std::future::pending().await
        }
        async fn stream_sse(
            &self,
            _: HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::http::HttpError>
        {
            unreachable!("LocalApp never streams SSE")
        }
    }

    #[tokio::test]
    async fn cancelling_a_host_request_drops_transport_but_preserves_staged_sources() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let source = root.join("project");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("index.html"), "app").unwrap();
        write_bundle_manifest(&source);
        let mut tool = tool(
            &root,
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        );
        std::fs::create_dir_all(&tool.config_home).unwrap();
        std::fs::write(
            tool.config_home.join("desktop-local-app-host.json"),
            json!({"port":12345,"token":"a".repeat(64)}).to_string(),
        )
        .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        tool.ctx.http = Arc::new(PendingHttp {
            entered: entered.clone(),
            dropped: dropped.clone(),
        });
        let imports = tool.config_home.join("local-app-imports");
        let mut ctx = call_context();
        ctx.cancel = Some(Default::default());
        let cancel = ctx.cancel.clone().unwrap();
        let task = tokio::spawn(async move {
            tool.call(
                generated_builtin_schema(&json!({"bundle_path":source})),
                ctx,
                fresh_tx(),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        cancel.cancel();
        assert!(matches!(task.await.unwrap(), Err(ToolError::Aborted)));
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(staged_count(&imports), 1);
    }
    #[tokio::test]
    async fn native_dispatch_rejects_private_transport_fields_and_invalid_ids() {
        let root = tempfile::tempdir().unwrap();
        let tool = tool(
            root.path(),
            Arc::new(ReadGate {
                deny: false,
                replace: None,
            }),
        );
        assert!(tool.native_input_validation());
        for input in [
            json!({"html":"app","staged_path":"/private"}),
            json!({"action":"read","id":"../outside"}),
            json!({"html":true}),
            json!({"html":"app","bundle_path":"/project"}),
        ] {
            assert!(tool
                .validate_input(&generated_builtin_schema(&input), &fresh_ctx())
                .await
                .is_err());
        }
    }
}
