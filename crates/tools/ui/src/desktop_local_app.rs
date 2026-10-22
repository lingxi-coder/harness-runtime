//! Desktop's project-capable LocalApp adapter. The Electron host owns the
//! library and runtime; publishing crosses the same nested Read permission
//! gate and rooted, no-follow file boundary as the document-only mobile tool.
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
use tool_api::util::path_validation::{canonicalize_and_validate, resolve_against_cwd};
use tool_api::BuiltinToolContext;
use visualization::{VisualizationId, VisualizationRef};

const PROMPT: &str = "Create and manage desktop Local Apps. Load the local-app skill first. Desktop supports multi-file projects, frameworks, network, workers, local Node/Python backends and application-scoped native capabilities. Use Bash to install missing runtimes and dependencies, including Node, Python, Vite and TypeScript. Publish through this tool so the application appears in Apps and opens in its own window; a file path or a development server alone is not delivery. Use bundle_path for a directory with lingxi-app.json, or html/file_path for a simple document. Use start, status and logs to verify the registered app, and stop to stop its owned processes. Never pass private host transport fields.";
// Matches the desktop host's transport budget, not mobile document limits.
const MAX_HOST_REQUEST_BYTES: usize = 64 * 1024 * 1024;
static SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    generated_builtin_schema(&json!({
      "type":"object", "additionalProperties":false,
      "properties": {
        "action":{"type":"string","enum":["publish","list","read","start","stop","status","logs"]},
        "id":{"type":"string","pattern":"^a[a-f0-9]{20}$"},
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
}
fn invalid(message: impl ToString) -> ToolError {
    ToolError::InvalidInput(message.to_string().into())
}
fn io(error: impl ToString) -> ToolError {
    ToolError::Io(error.to_string().into())
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

/// The executor may drop a tool future instead of letting its error path run.
/// A private, invocation-owned import must not survive that cancellation.
struct StagedSource(PathBuf);

impl Drop for StagedSource {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
    // queued mkdir must not recreate imports after StagedSource has cleaned
    // them. These private, shallow parent creations cannot yield to cancel.
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
        }
    }
    fn source(&self, value: &str) -> Result<PathBuf, ToolError> {
        let (cwd, trusted) = self.ctx.cwd_and_trusted();
        let requested = resolve_against_cwd(PathBuf::from(value), &cwd);
        canonicalize_and_validate(&requested, &trusted)
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
    ) -> Result<(), ToolError> {
        check_cancelled(ctx)?;
        let requested = source.join(relative);
        let canonical = self.source(&requested.to_string_lossy())?;
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
        // Create namespace entries before yielding. An uncancellable queued
        // create_new job could otherwise race the staging guard's rmdir.
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
    ) -> Result<(), ToolError> {
        let mut directories = vec![PathBuf::new()];
        while let Some(relative) = directories.pop() {
            check_cancelled(ctx)?;
            // Revalidate before enumeration and never descend into a link.
            let directory = self.source(&source.join(&relative).to_string_lossy())?;
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
                if matches!(
                    name_text.as_ref(),
                    ".git"
                        | "node_modules"
                        | ".venv"
                        | "venv"
                        | "__pycache__"
                        | ".DS_Store"
                        | ".lingxi-prepared.json"
                ) || name_text == ".env"
                    || (name_text.starts_with(".env.") && name_text != ".env.example")
                {
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
                    self.copy_file(source, &rel, &destination.join(&rel), ctx, None)
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
        let discovery: Value = serde_json::from_slice(&tokio::fs::read(self.config_home.join("desktop-local-app-host.json")).await
            .map_err(|_| invalid("Desktop Local App host is unavailable. Open or restart the LingXi desktop application."))?).map_err(io)?;
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
        Some("create local desktop app 本地应用 application library backend".into())
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
        "Publish and run desktop Local Apps: multi-file projects, Node/Python backends, native capabilities, library and independent windows.".into()
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
            if id.len() != 21
                || !id.starts_with('a')
                || !id[1..]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(ValidationError("Invalid desktop application id".into()));
            }
        }
        let action = value["action"].as_str().unwrap_or("publish");
        if !["publish", "list", "read", "start", "stop", "status", "logs"].contains(&action) {
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
        let mut input: Value =
            serde_json::from_str(&input.to_json_string().map_err(io)?).map_err(invalid)?;
        if input.get("action").is_none() {
            input["action"] = json!("publish");
        }
        let imports = self.config_home.join("local-apps/desktop/imports");
        let staging = StagedSource(imports.join(uuid::Uuid::new_v4().simple().to_string()));
        let staged = &staging.0;
        let operation = async {
            if input.get("visualization_id").is_some() {
                self.visualization_document(&mut input, &ctx).await?;
            }
            if let Some(path) = input["bundle_path"].as_str().map(str::to_owned) {
                let source = self.source(&path)?;
                create_staging_directory(staged)?;
                if tokio::fs::canonicalize(&staged)
                    .await
                    .map_err(io)?
                    .starts_with(&source)
                {
                    return Err(invalid(
                        "Application source cannot contain its runtime imports directory",
                    ));
                }
                self.stage(&source, &staged, &ctx).await?;
                input.as_object_mut().unwrap().remove("bundle_path");
                input["staged_path"] = json!(staged);
                input["source_path"] = json!(source);
            } else if let Some(path) = input["file_path"].as_str().map(str::to_owned) {
                let source = self.source(&path)?;
                let root = source
                    .parent()
                    .ok_or_else(|| invalid("Invalid application file"))?;
                create_staging_directory(staged)?;
                self.copy_file(
                    root,
                    Path::new(source.file_name().unwrap()),
                    &staged.join("index.html"),
                    &ctx,
                    Some(MAX_HOST_REQUEST_BYTES as u64),
                )
                .await?;
                input["html"] = json!(tokio::fs::read_to_string(staged.join("index.html"))
                    .await
                    .map_err(io)?);
                input.as_object_mut().unwrap().remove("file_path");
                input["source_path"] = json!(root);
            }
            if let Some(session) = &self.ctx.session_id {
                input["source_session"] =
                    json!({"projectPath":self.ctx.cwd(),"sessionId":session.as_uuid().to_string()});
            }
            check_cancelled(&ctx)?;
            self.host(&input).await
        };
        // Dropping the HTTP future closes this request's connection. The host
        // cancels only work owned by that request; no blind app-wide Stop can
        // accidentally terminate an application that was already running.
        let result = tokio::select! {
            biased;
            _ = cancelled(&ctx) => Err(ToolError::Aborted),
            result = operation => result,
        };
        let _ = tokio::fs::remove_dir_all(staged).await;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::permission_gate::{PermissionDecision, PermissionGate};
    use mobile_linux_api::ProcessOutput;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};

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
        ctx
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
                Some(8)
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
                None
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
                None
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
            (".lingxi-prepared.json", "{\"revision\":1}"),
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
        tool.stage(&source, &dest, &call_context()).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("index.html")).unwrap(),
            "app"
        );
        assert!(dest.join(".env.example").exists());
        assert!(!dest.join(".env").exists());
        assert!(!dest.join(".lingxi-prepared.json").exists());
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
        tool.stage(&source, &dest, &call_context()).await.unwrap();
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
        assert_eq!(
            std::fs::read_dir(tool.config_home.join("local-apps/desktop/imports"))
                .unwrap()
                .count(),
            0
        );
    }

    struct PendingReadGate {
        entered: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl PermissionGate for PendingReadGate {
        async fn check(&self, _: &str, _: &Utf16JsonProjection) -> PermissionDecision {
            self.entered.notify_one();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn cancellation_or_a_dropped_future_cleans_pending_source_imports() {
        for drop_future in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let root = root.path().canonicalize().unwrap();
            let source = root.join("project");
            std::fs::create_dir(&source).unwrap();
            std::fs::write(source.join("index.html"), "app").unwrap();
            let entered = Arc::new(tokio::sync::Notify::new());
            let tool = tool(
                &root,
                Arc::new(PendingReadGate {
                    entered: entered.clone(),
                }),
            );
            let imports = tool.config_home.join("local-apps/desktop/imports");
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
            assert_eq!(std::fs::read_dir(&imports).unwrap().count(), 1);
            if drop_future {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                cancel.cancel();
                assert!(matches!(task.await.unwrap(), Err(ToolError::Aborted)));
            }
            assert_eq!(std::fs::read_dir(imports).unwrap().count(), 0);
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
        let imports = tool.config_home.join("local-apps/desktop/imports");
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
            let remaining = if imports.exists() {
                std::fs::read_dir(imports).unwrap().count()
            } else {
                0
            };
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
    async fn cancelling_a_host_request_drops_transport_and_removes_staged_sources() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let source = root.join("project");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("index.html"), "app").unwrap();
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
        let imports = tool.config_home.join("local-apps/desktop/imports");
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
        assert_eq!(std::fs::read_dir(imports).unwrap().count(), 0);
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
