//! `tool-workflow` — the `Workflow` tool: launch a model-authored workflow
//! script as a background task.
//!
//! claude-code's Workflow tool runs the script in the background, returning
//! immediately with `{ status: "async_launched", taskId, taskType:
//! "local_workflow" }`; a `<task-notification>` arrives when it completes. The
//! script orchestrates subagents (`agent()`/`parallel()`/`pipeline()`/…);
//! LingXi runs it on the `workflow` crate's QuickJS runtime via a
//! `LocalWorkflow` background task (`tasks::handlers::local_workflow`).
//!
//! The byte-exact model-facing surface — the tool name, the long-form
//! description ([`Tool::prompt`]), and the input schema — is reproduced from the
//! claude-code v2.1.245 binary. The launch itself goes through the injected
//! [`WorkflowLauncher`] seam, which the composition root wires over the task
//! registry; with no launcher wired the tool serves its surface but `call`
//! reports a clear error.

#![forbid(unsafe_code)]

mod builtins;
mod size_guideline;
mod ultracode;
pub use builtins::{BuiltinWorkflowDescriptor, BuiltinWorkflowRegistry, BUILTIN_WORKFLOWS};
pub use size_guideline::{prompt_appendix_for, WorkflowSizeGuideline};
pub use ultracode::{
    workflows_enabled, UltracodeAttachment, UltracodeAttachmentKind, UltracodeConfig,
    UltracodeGate, UltracodeState, DEFAULT_ULTRACODE_CADENCE, ULTRACODE_CADENCE_ENV,
};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
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

/// Maximum script size in bytes (claude-code `P2 = 524288` = 512 KB).
pub const MAX_SCRIPT_BYTES: usize = 524288;

/// Tool name byte-lock.
pub const TOOL_NAME: &str = "Workflow";

const WORKFLOW_EXTENSIONS: [&str; 4] = [".js", ".mjs", ".ts", ""];

use workflow::description::{assemble_description, subagent_model_forced};

/// The input schema (claude-code v2.1.245 `inputSchema`), reproduced from the
/// zod `strictObject` definition (source-order properties, `additionalProperties:
/// false`, no required keys — the "at least one of script/name/scriptPath"
/// constraint is a runtime `.refine`, enforced in [`Tool::validate_input`]).
static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    serde_json::from_str(include_str!("workflow_input_schema.json"))
        .expect("workflow_input_schema.json is valid JSON")
});

/// Oracle B0 removes Cc/Cf/Cs, Default_Ignorable_Code_Point and line separators.
/// Rust scalar strings cannot contain Cs. Keep the remaining Unicode ranges
/// explicit so the tool does not need a new regex/Unicode dependency.
fn hidden_workflow_input_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
        '\u{00ad}' | '\u{034f}' | '\u{0600}'..='\u{0605}' | '\u{061c}' | '\u{06dd}' | '\u{070f}' |
        '\u{0890}'..='\u{0891}' | '\u{08e2}' | '\u{115f}'..='\u{1160}' | '\u{17b4}'..='\u{17b5}' |
        '\u{180b}'..='\u{180f}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' |
        '\u{2060}'..='\u{206f}' | '\u{3164}' | '\u{fe00}'..='\u{fe0f}' | '\u{feff}' | '\u{ffa0}' |
        '\u{fff0}'..='\u{fff8}' | '\u{110bd}' | '\u{110cd}' | '\u{13430}'..='\u{1343f}' |
        '\u{1bca0}'..='\u{1bca3}' | '\u{1d173}'..='\u{1d17a}' | '\u{e0000}'..='\u{e0fff}')
}

/// What a [`WorkflowLauncher`] needs to start a workflow run. Mirrors the
/// Workflow tool's input minus the `title`/`description` fields (which the tool
/// description marks "Ignored").
#[derive(Debug, Clone, Default)]
pub struct WorkflowLaunchSpec {
    /// Inline self-contained script source.
    pub script: Option<String>,
    /// Name of a predefined/saved workflow.
    pub name: Option<String>,
    /// Path to a script file on disk (takes precedence over `script`/`name`).
    pub script_path: Option<String>,
    /// `args` global value, passed verbatim.
    pub args: Option<Value>,
    /// Resume a prior run by its `wf_…` id.
    pub resume_from_run_id: Option<String>,
    /// Internal host pin for an explicit resume, in canonical bare-UUID form.
    /// Tool-driven launches leave this unset and resolve the current session at
    /// launch time; mobile UI resumes set it so a concurrent session switch
    /// cannot retarget the run. Hosts accept the legacy `sess:<uuid>` spelling
    /// at their boundary and normalize it before deriving transcript paths.
    pub session_uuid: Option<String>,
    /// Originating Workflow tool-use id for the terminal task notification.
    pub tool_use_id: Option<String>,
    /// Whether the Workflow call came from a subagent context.
    pub launched_from_subagent: bool,
    /// Stable creator ownership for parent-rest deferral.
    pub creator_teammate_name: Option<String>,
    /// Team containing the creator, when it belongs to one.
    pub creator_team_name: Option<String>,
    /// Stringified `protocol::AgentId`; the launcher boundary is intentionally
    /// protocol-agnostic and the host parses the prefixed wire form.
    pub creator_agent_id: Option<String>,
    /// Canonical/rooted snapshot captured by `WorkflowTool::check_permissions`
    /// for a caller-supplied `scriptPath`. Launchers must consume this snapshot
    /// through [`read_script_path_after_permission`] rather than reopening the
    /// model path by name. `None` is expected for inline/named workflows and is
    /// fail-closed for `scriptPath` launches.
    pub script_path_approval: Option<WorkflowScriptPathApproval>,
}

/// The path identity approved by the nested `Read` permission check.
///
/// `requested` preserves the model's lexical path for the final resolution
/// check; `approved` is the canonical target observed before permission was
/// resolved; and `root`/`relative` are the rooted no-follow open coordinates.
/// Keeping all four values prevents a launcher from silently substituting a
/// fresh pathname resolution after a parent or leaf symlink retarget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowScriptPathApproval {
    /// Original lexical path resolved against the session cwd.
    pub requested: PathBuf,
    /// Canonical target observed before the permission decision.
    pub approved: PathBuf,
    /// Canonical trusted root used for the rooted open.
    pub root: PathBuf,
    /// Path relative to `root` used by the no-follow openat/handle chain.
    pub relative: PathBuf,
}

/// The result of a successful launch.
#[derive(Debug, Clone, Default)]
pub struct WorkflowLaunched {
    /// The background task id (claude-code `taskId`).
    pub task_id: String,
    /// The local run id for `resumeFromRunId` (claude-code `runId`). Minted at
    /// launch so the model can pass it back to resume; `None` if unavailable.
    pub run_id: Option<String>,
    /// Path to the persisted workflow script for this invocation (claude-code
    /// `scriptPath`) — editable, and passable back as `scriptPath` to re-run
    /// without resending the script. `None` if the host did not persist it.
    pub script_path: Option<String>,
    /// `meta.name` from the script (claude-code `workflowName`). `None` if not
    /// extracted.
    pub workflow_name: Option<String>,
    /// `meta.description` from the script (claude-code `summary = p`). `None`
    /// if the workflow has no description in its meta block.
    pub summary: Option<String>,
    /// Directory where subagent transcripts are written (claude-code
    /// `transcriptDir = Nte(runId)` → `<sessionProjectDir>/<sessionId>/subagents/workflows/<runId>`).
    /// `None` if the session dir is not available to the launcher.
    pub transcript_dir: Option<String>,
    /// Compiler diagnostic returned by Claude's `async_launched` path when the
    /// script was parsed successfully but could not be compiled. This is a
    /// successful tool result with no task registered, not a validation error.
    pub error: Option<String>,
}

/// Error launching a workflow.
#[derive(Debug)]
pub struct WorkflowLaunchError(pub String);

impl std::fmt::Display for WorkflowLaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for WorkflowLaunchError {}

/// Return whether `value` matches the workflow run-id shape accepted by the
/// tool schema. Hosts must enforce this before using a resume id as a path
/// component; JSON Schema validation is not guaranteed on every call path.
#[must_use]
pub fn is_valid_run_id(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("wf_") else {
        return false;
    };
    rest.len() >= 6
        && rest
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Mint the wire-shaped workflow run id used by fresh launches. A supplied
/// resume id is reused exactly, matching Claude's `resumeFromRunId ?? wf_*`.
#[must_use]
pub fn mint_run_id(resume_from_run_id: Option<&str>) -> String {
    if let Some(run_id) = resume_from_run_id.filter(|value| !value.is_empty()) {
        return run_id.to_string();
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let value = nanos ^ sequence.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    format!(
        "wf_{:08x}-{:03x}",
        (value >> 32) as u32,
        (value as u32) & 0xfff
    )
}

fn user_config_home_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(branding::CONFIG_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .map(|home| home.join(branding::DOT_DIR))
}

fn saved_workflow_dirs(cwd: &Path) -> Vec<PathBuf> {
    let project = cwd.join(branding::DOT_DIR).join("workflows");
    let mut dirs = vec![project.clone()];
    if let Some(user) = user_config_home_dir().map(|home| home.join("workflows")) {
        if user != project {
            dirs.push(user);
        }
    }
    dirs
}

fn saved_workflow_candidates(cwd: &Path, name: &str) -> Vec<PathBuf> {
    saved_workflow_dirs(cwd)
        .into_iter()
        .flat_map(|dir| {
            WORKFLOW_EXTENSIONS
                .iter()
                .map(move |ext| dir.join(format!("{name}{ext}")))
        })
        .collect()
}

/// Return Claude Code's resolver source for a named workflow. The source is
/// intentionally independent of the resolved script body: a named built-in
/// with an explicit script override still reports `built-in`, while the
/// `scriptMatchesDefinition` flag controls telemetry redaction separately.
///
/// `plugin_workflows` is the same registry the resolver consults, checked in
/// the SAME position (last), so `tengu_workflow_launched`'s `workflow_source`
/// names the tier the launch actually resolved from. The oracle stamps
/// `source:"plugin"` on every plugin record (`v()` @169045670) and emits it
/// verbatim (`workflow_source:c(se)` @172565293), so `"plugin"` is a real
/// emitted value — without this arm a plugin workflow is misreported as
/// `custom`.
#[must_use]
pub fn workflow_source_for_name(
    cwd: &Path,
    name: &str,
    plugin_workflows: Option<&workflow::PluginWorkflowRegistry>,
) -> Option<&'static str> {
    let project = cwd.join(branding::DOT_DIR).join("workflows");
    if WORKFLOW_EXTENSIONS
        .iter()
        .any(|ext| project.join(format!("{name}{ext}")).is_file())
    {
        return Some("projectSettings");
    }
    let user_dirs = user_config_home_dir()
        .into_iter()
        .map(|home| home.join("workflows"));
    if user_dirs.into_iter().any(|dir| {
        WORKFLOW_EXTENSIONS
            .iter()
            .any(|ext| dir.join(format!("{name}{ext}")).is_file())
    }) {
        return Some("userSettings");
    }
    if plugin_workflows.is_some_and(|registry| registry.resolve(name).is_some()) {
        return Some("plugin");
    }
    if BUILTIN_WORKFLOWS.get(name).is_some() {
        return Some("built-in");
    }
    None
}

fn path_for_read(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Read a previously approved `scriptPath` through the rooted, no-follow
/// filesystem primitive. The helper re-checks the lexical path's resolution
/// against the approved canonical target immediately before opening the file,
/// then reads from that opened handle so a concurrent parent/leaf symlink swap
/// cannot redirect workflow execution.
pub fn read_script_path_after_permission(
    approval: &WorkflowScriptPathApproval,
) -> Result<String, WorkflowLaunchError> {
    let snapshot = platform_api::rooted_fs::read_file_after_permission(
        &approval.root,
        &approval.relative,
        &approval.requested,
        &approval.approved,
    )
    .map_err(|error| {
        WorkflowLaunchError(format!(
            "cannot read workflow scriptPath '{}' after permission: {error}",
            approval.requested.display()
        ))
    })?;
    if snapshot.bytes.len() > MAX_SCRIPT_BYTES {
        return Err(WorkflowLaunchError(format!(
            "Workflow script file {} exceeds {MAX_SCRIPT_BYTES} bytes",
            approval.requested.display()
        )));
    }
    String::from_utf8(snapshot.bytes).map_err(|_| {
        WorkflowLaunchError(format!(
            "workflow scriptPath '{}' is not valid UTF-8",
            approval.requested.display()
        ))
    })
}

/// Resolve a launch spec to a script source. Precedence follows claude-code:
/// `scriptPath` over `name` over `script` (a named workflow must resolve before
/// an optional inline body can override its source). `read` loads a file's contents
/// (the host provides real I/O); `name` resolution looks first under the
/// project saved-workflow directory (`.lingxi/workflows/<name>`), then the
/// user config directory (`$LINGXI_CONFIG_DIR/workflows/<name>` or
/// `~/.lingxi/workflows/<name>`) with common script extensions, then a
/// plugin's declared/auto-scanned workflow by its namespaced name (when a
/// plugin-workflow registry is passed), and only last the immutable
/// built-ins. A project/user file always wins a name collision; the plugin
/// registry is checked before built-ins so a plugin can supply a workflow
/// the built-in table does not, but a saved project/user file still shadows
/// both (oracle precedence project/user > plugin > built-in — see
/// `workflow::PluginWorkflowRegistry`).
pub fn resolve_script_at<R>(
    cwd: &Path,
    spec: &WorkflowLaunchSpec,
    read: R,
    plugin_workflows: Option<&workflow::PluginWorkflowRegistry>,
) -> Result<String, WorkflowLaunchError>
where
    R: Fn(&str) -> std::io::Result<String>,
{
    let nonempty = |o: &Option<String>| o.as_deref().filter(|s| !s.is_empty()).map(str::to_string);
    if let Some(path) = nonempty(&spec.script_path) {
        return read(&path)
            .map_err(|e| WorkflowLaunchError(format!("cannot read scriptPath '{path}': {e}")));
    }
    if let Some(name) = nonempty(&spec.name) {
        let mut resolved = None;
        for candidate in saved_workflow_candidates(cwd, &name) {
            if let Ok(src) = read(&path_for_read(&candidate)) {
                resolved = Some(src);
                break;
            }
        }
        if resolved.is_none() {
            if let Some(path) = plugin_workflows.and_then(|registry| registry.resolve(&name)) {
                resolved = read(&path_for_read(&path)).ok();
            }
        }
        if resolved.is_none() {
            resolved = BUILTIN_WORKFLOWS
                .get(&name)
                .map(|descriptor| descriptor.script.to_string());
        }
        let named = resolved.ok_or_else(|| {
            WorkflowLaunchError(format!("Workflow \"{name}\" not found. Available: (none)"))
        })?;
        return Ok(nonempty(&spec.script).unwrap_or(named));
    }
    if let Some(script) = nonempty(&spec.script) {
        return Ok(script);
    }
    Err(WorkflowLaunchError(
        "Must provide script, name, scriptPath, or runId".into(),
    ))
}

/// Resolve relative to the process working directory. Production composition
/// roots should use [`resolve_script_at`] with their live session cwd.
pub fn resolve_script<R>(spec: &WorkflowLaunchSpec, read: R) -> Result<String, WorkflowLaunchError>
where
    R: Fn(&str) -> std::io::Result<String>,
{
    resolve_script_at(Path::new(""), spec, read, None)
}

// ── Save dynamic workflow (claude-code `eya` / `uQ_`, dialog mode:"save") ─────

/// Where a saved workflow is written — the oracle's `scope` field of the "Save
/// dynamic workflow" dialog (`iNt`). `Project` writes under the project workflow
/// dir (`<cwd>/.lingxi/workflows`); `User` under the user config dir
/// (`$LINGXI_CONFIG_DIR` / `~/.lingxi`, `+ /workflows`). Defaults to `Project`
/// (oracle `useState("project")`).
///
/// These two rungs are [`protocol::Scope::Project`] and [`protocol::Scope::User`]
/// — see the `From` impl below, which a test pins. It stays a separate type
/// rather than joining the shared narrowings because [`Self::toggled`] is a
/// two-element cycle: the dialog offers exactly these two choices (oracle
/// `l3p`), and a third rung would leave the Tab toggle undefined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowScope {
    /// Project scope — `<cwd>/.lingxi/workflows` (oracle `.claude/workflows`).
    Project,
    /// User scope — `<userConfigHome>/workflows` (oracle `xDt()`).
    User,
}

impl WorkflowScope {
    /// The wire string used in telemetry / persistence (`"project"` / `"user"`).
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            WorkflowScope::Project => "project",
            WorkflowScope::User => "user",
        }
    }
    /// The capitalized UI label (oracle `l3p`): `"Project"` / `"User"`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            WorkflowScope::Project => "Project",
            WorkflowScope::User => "User",
        }
    }
    /// Tab toggles Project ⇄ User (oracle `IQ_`).
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            WorkflowScope::Project => WorkflowScope::User,
            WorkflowScope::User => WorkflowScope::Project,
        }
    }
}

impl From<WorkflowScope> for protocol::Scope {
    fn from(value: WorkflowScope) -> Self {
        match value {
            WorkflowScope::Project => Self::Project,
            WorkflowScope::User => Self::User,
        }
    }
}

#[cfg(test)]
mod workflow_scope_tests {
    use super::WorkflowScope;

    /// The dialog's two choices are the shared vocabulary's Project and User
    /// rungs, and its lowercase `wire()` spelling is the same one the rest of
    /// the engine uses for them. Pinned so this type cannot quietly come to
    /// mean something else.
    #[test]
    fn the_two_rungs_are_the_shared_projects_and_user_scopes() {
        for (scope, expected) in [
            (WorkflowScope::Project, protocol::Scope::Project),
            (WorkflowScope::User, protocol::Scope::User),
        ] {
            assert_eq!(protocol::Scope::from(scope), expected);
            assert_eq!(
                scope.wire(),
                protocol::scope::snake_case::name(expected),
                "workflow's wire spelling must not diverge from the shared one"
            );
        }
    }

    /// The toggle is a two-cycle; a rung added here without revisiting it would
    /// make Tab skip a choice.
    #[test]
    fn toggling_twice_returns_to_the_start() {
        for scope in [WorkflowScope::Project, WorkflowScope::User] {
            assert_eq!(scope.toggled().toggled(), scope);
            assert_ne!(scope.toggled(), scope);
        }
    }
}

/// Kebab-sanitize a workflow name (oracle `lme`):
/// `toLowerCase()` → collapse each `[^a-z0-9]+` run to a single `-` → trim
/// leading/trailing `-`; an empty result becomes `"workflow"`.
#[must_use]
pub fn sanitize_workflow_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for ch in name.chars() {
        let lc = ch.to_ascii_lowercase();
        if lc.is_ascii_lowercase() || lc.is_ascii_digit() {
            out.push(lc);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "workflow".to_string()
    } else {
        trimmed.to_string()
    }
}

/// The directory a workflow of `scope` is saved into (oracle `uQ_(scope, cwd)`).
/// `User` → `<userConfigHome>/workflows`; `Project` → `<cwd>/.lingxi/workflows`.
/// (The oracle joins the git root / cwd with `.claude/workflows`; LingXi uses the
/// `.lingxi` dir name — the accepted branding divergence.) Falls back to the
/// project dir when the user config home can't be resolved.
#[must_use]
pub fn workflow_scope_dir(scope: WorkflowScope, cwd: &Path) -> PathBuf {
    match scope {
        WorkflowScope::User => user_config_home_dir()
            .map(|home| home.join("workflows"))
            .unwrap_or_else(|| cwd.join(branding::DOT_DIR).join("workflows")),
        WorkflowScope::Project => cwd.join(branding::DOT_DIR).join("workflows"),
    }
}

/// A successful [`save_dynamic_workflow`] — the oracle `eya` return
/// `{name, path, scope}` plus the `script_size_chars` telemetry field.
#[derive(Debug, Clone)]
pub struct WorkflowSaved {
    /// Sanitized workflow name (the `<name>` in `<name>.js`).
    pub name: String,
    /// Absolute path the script was written to.
    pub path: PathBuf,
    /// The scope it was saved under.
    pub scope: WorkflowScope,
    /// `script.length` (UTF-16 code units, matching JS) — the oracle
    /// `script_size_chars` telemetry field.
    pub script_size_chars: usize,
}

/// Error saving a dynamic workflow (oracle `eya` throw paths).
#[derive(Debug)]
pub enum WorkflowSaveError {
    /// The target file exists and `overwrite` was not set (oracle EEXIST →
    /// telemetry `"already_exists"`). Message byte-exact with the binary.
    AlreadyExists {
        /// Sanitized name that collided.
        name: String,
        /// The `<name>.js` path that already exists.
        path: PathBuf,
    },
    /// Any other I/O failure (mkdir / write) — oracle `"write_failed"`.
    Io(std::io::Error),
}

impl std::fmt::Display for WorkflowSaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkflowSaveError::AlreadyExists { name, path } => write!(
                f,
                "Dynamic workflow \"{name}\" already exists at {}. Use a different name or overwrite.",
                path.display()
            ),
            WorkflowSaveError::Io(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for WorkflowSaveError {}

/// Save a dynamic workflow to disk (claude-code `eya`). Writes the sanitized
/// `<name>.js` under [`workflow_scope_dir`], creating the dir `0o700` and the
/// file `0o600`. The entire save is protected by a root-confined advisory lock
/// and a same-directory atomic replacement. No path component below the trusted
/// root may be a symlink/reparse point. Without `overwrite`, the final install
/// is an atomic no-clobber operation and yields
/// [`WorkflowSaveError::AlreadyExists`] on collision.
pub fn save_dynamic_workflow(
    name: &str,
    scope: WorkflowScope,
    script: &str,
    overwrite: bool,
    cwd: &Path,
) -> Result<WorkflowSaved, WorkflowSaveError> {
    let sanitized = sanitize_workflow_name(name);
    let dir = workflow_scope_dir(scope, cwd);
    let path = dir.join(format!("{sanitized}.js"));

    // Project saves are anchored at cwd so `.lingxi` itself cannot be swapped
    // for a symlink. User saves anchor one level above the configured home for
    // the same reason; a relative configured home remains cwd-relative.
    let (root, relative_dir) = match scope {
        WorkflowScope::Project => (
            cwd.to_path_buf(),
            PathBuf::from(branding::DOT_DIR).join("workflows"),
        ),
        WorkflowScope::User if dir.is_absolute() => {
            let config_home = dir.parent().unwrap_or(&dir);
            match (config_home.parent(), config_home.file_name()) {
                (Some(parent), Some(name)) => {
                    (parent.to_path_buf(), PathBuf::from(name).join("workflows"))
                }
                _ => (config_home.to_path_buf(), PathBuf::from("workflows")),
            }
        }
        WorkflowScope::User => (
            std::env::current_dir().unwrap_or_else(|_| cwd.to_path_buf()),
            dir.clone(),
        ),
    };
    let relative = relative_dir.join(format!("{sanitized}.js"));
    let lock_relative = relative_dir.join(".save.lock");
    let io_error = |error: platform_api::FsError| {
        WorkflowSaveError::Io(std::io::Error::other(error.to_string()))
    };
    let _lock = platform_api::rooted_fs::lock_exclusive(
        &root,
        &lock_relative,
        platform_api::rooted_fs::PRIVATE_DIR_MODE,
        platform_api::rooted_fs::PRIVATE_FILE_MODE,
    )
    .map_err(io_error)?;
    let options = platform_api::AtomicWriteOptions {
        overwrite,
        ..platform_api::AtomicWriteOptions::default()
    };
    if let Err(error) =
        platform_api::rooted_fs::atomic_write(&root, &relative, script.as_bytes(), options)
    {
        if matches!(error, platform_api::FsError::AlreadyExists(_)) {
            return Err(WorkflowSaveError::AlreadyExists {
                name: sanitized,
                path,
            });
        }
        return Err(io_error(error));
    }

    Ok(WorkflowSaved {
        name: sanitized,
        path,
        scope,
        // JS `.length` counts UTF-16 code units.
        script_size_chars: script.encode_utf16().count(),
    })
}

/// The success feedback string shown after a save (oracle `iNt` `lun(...)`),
/// byte-exact: `Dynamic workflow saved to <path>. Invoke as /<name> or
/// Workflow({name: "<name>"}) in future sessions.`
#[must_use]
pub fn saved_feedback(name: &str, path: &Path) -> String {
    format!(
        "Dynamic workflow saved to {}. Invoke as /{name} or Workflow({{name: \"{name}\"}}) in future sessions.",
        path.display()
    )
}

/// Seam that spawns a `LocalWorkflow` background task and returns its id. The
/// composition root wires this over the task registry (keeping this crate
/// decoupled from `tasks`); tests inject a mock.
#[async_trait]
pub trait WorkflowLauncher: Send + Sync {
    /// Launch the workflow described by `spec`, returning the new task id.
    async fn launch(
        &self,
        spec: WorkflowLaunchSpec,
    ) -> Result<WorkflowLaunched, WorkflowLaunchError>;
}

/// `WorkflowTool` — launch a workflow script as a background task.
#[derive(Clone)]
pub struct WorkflowTool {
    launcher: Option<Arc<dyn WorkflowLauncher>>,
    /// Live permission gate used to authorize a `scriptPath` as an equivalent
    /// `Read` operation before the launcher is allowed to read it. Hosts that
    /// do not have a live gate may provide the boot policy instead; the
    /// no-policy fallback is an explicit `Ask`, never an unconditional allow.
    permission_gate: Option<Arc<dyn platform_api::permission_gate::PermissionGate>>,
    permission_policy: Option<Arc<permission::PermissionPolicy>>,
    /// Approval snapshots keyed by the originating tool-use id and lexical
    /// path. `check_permissions` runs before the main call, so this shared
    /// queue carries the exact pre-check identity into the launcher without a
    /// global side channel or a second policy lookup.
    script_path_approvals:
        Arc<Mutex<HashMap<(Option<String>, PathBuf), Vec<WorkflowScriptPathApproval>>>>,
    /// Shared live session cwd. Desktop updates this after persistent-shell
    /// `cd`; mobile supplies a fixed cell. `None` falls back to process cwd for
    /// offline/surface-only registries.
    current_cwd: Option<Arc<std::sync::Mutex<PathBuf>>>,
    /// The session-owned `workflowSizeGuideline` seed. Composition roots load
    /// the initial persisted value once, then `/config` mutates the shared
    /// session carrier so [`Tool::prompt`] can reflect live session changes
    /// (`qAs + VAs(size)`) without rebuilding the registry. Defaults to
    /// [`WorkflowSizeGuideline::Medium`] (the oracle's `_Td`) when unset —
    /// NOT `Unrestricted`.
    size_guideline: WorkflowSizeGuideline,
    /// MANAGED-settings `disableWorkflows` (binary `fbn()`'s second arm).
    /// Threaded at registration because `ToolStaticContext` carries only
    /// feature flags.
    managed_disable_workflows: bool,
    /// Session-owned dynamic-workflow gate shared with the orchestrator
    /// handle. When absent, tests and lightweight hosts fall back to the
    /// construction-time snapshot in `session_enabled`.
    dynamic_workflows_gate: Option<platform_api::session_flags::DynamicWorkflowsGate>,
    /// Session-owned workflow-size setting shared with the orchestrator
    /// handle. When absent, tests and lightweight hosts fall back to the
    /// legacy process-global compatibility snapshot.
    size_guideline_state: Option<platform_api::session_flags::WorkflowSizeGuidelineState>,
    /// Session-scoped dynamic-workflow gate (`pA()`): launch/runtime policy may
    /// leave Workflow installed but unavailable for this session.
    session_enabled: bool,
    /// Optional live plugin-workflow registry (§14 — `plugin::PluginManager`
    /// materializes a plugin's declared/auto-scanned `workflows` into this
    /// SAME shared table). When wired, a plugin's saved workflow becomes
    /// resolvable by its namespaced name (`{plugin}:{name}`) after the
    /// built-in/project/user directories have already missed — see
    /// [`saved_workflow_candidates`]'s call sites. `None` (the default until
    /// a composition root calls [`Self::with_plugin_workflows`]) means only
    /// built-in/project/user workflows resolve, exactly today's behavior.
    plugin_workflows: Option<Arc<workflow::PluginWorkflowRegistry>>,
}

impl WorkflowTool {
    /// Construct. `launcher` is `None` when the host has not wired the workflow
    /// task seam — the model-facing surface is still served, but `call` errors.
    /// The size guideline defaults to `unrestricted`; use
    /// [`Self::with_size_guideline`] to feed the persisted `/config` value.
    #[must_use]
    pub fn new(launcher: Option<Arc<dyn WorkflowLauncher>>) -> Self {
        Self {
            launcher,
            permission_gate: None,
            permission_policy: None,
            script_path_approvals: Arc::new(Mutex::new(HashMap::new())),
            current_cwd: None,
            size_guideline: WorkflowSizeGuideline::default(),
            managed_disable_workflows: false,
            dynamic_workflows_gate: None,
            size_guideline_state: None,
            session_enabled: true,
            plugin_workflows: None,
        }
    }

    /// Bind the session's live permission gate. `scriptPath` authorization is
    /// sent to this gate as a `Read` request, so deny/ask decisions happen
    /// before the launcher can read the file.
    #[must_use]
    pub fn with_permission_gate(
        mut self,
        gate: Arc<dyn platform_api::permission_gate::PermissionGate>,
    ) -> Self {
        self.permission_gate = Some(gate);
        self
    }

    /// Bind the boot policy for hosts that have a policy but no interactive
    /// gate. This preserves the existing permission engine's path and symlink
    /// semantics without reimplementing canonicalization in this crate.
    #[must_use]
    pub fn with_permission_policy(mut self, policy: Arc<permission::PermissionPolicy>) -> Self {
        self.permission_policy = Some(policy);
        self
    }

    /// Bind the same live cwd cell used by Bash and the orchestrator.
    #[must_use]
    pub fn with_current_cwd(mut self, cwd: Arc<std::sync::Mutex<PathBuf>>) -> Self {
        self.current_cwd = Some(cwd);
        self
    }

    /// Share the host's live plugin-workflow registry (the SAME `Arc` handed
    /// to `plugin::PluginManager::with_plugin_workflows`), so a plugin's
    /// saved workflow becomes resolvable by name alongside built-in/project/
    /// user workflows.
    #[must_use]
    pub fn with_plugin_workflows(
        mut self,
        registry: Arc<workflow::PluginWorkflowRegistry>,
    ) -> Self {
        self.plugin_workflows = Some(registry);
        self
    }

    /// Composition-test seam for asserting registry identity across the
    /// manager, tool, launcher, and nested resolver.
    #[doc(hidden)]
    #[must_use]
    pub fn shares_plugin_workflows(
        &self,
        registry: &Arc<workflow::PluginWorkflowRegistry>,
    ) -> bool {
        self.plugin_workflows
            .as_ref()
            .is_some_and(|wired| Arc::ptr_eq(wired, registry))
    }

    fn current_cwd(&self) -> PathBuf {
        self.current_cwd
            .as_ref()
            .and_then(|cwd| cwd.lock().ok().map(|guard| guard.clone()))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
    }

    fn rooted_location_for_approval(&self, approved: &Path) -> Option<(PathBuf, PathBuf)> {
        let mut roots = Vec::new();
        if let Some(policy) = &self.permission_policy {
            if let Some(fs_roots) = &policy.roots {
                roots.push(fs_roots.cwd.clone());
                roots.push(fs_roots.lingxi_home.clone());
            }
            roots.extend(policy.additional_working_dirs.paths());
        }
        // Lightweight hosts/tests may inject a live gate without a policy;
        // the session cwd is then the only safe rooted boundary available.
        roots.push(self.current_cwd());
        roots
            .into_iter()
            .filter_map(|root| {
                let root = if root.is_absolute() {
                    root
                } else {
                    self.current_cwd().join(root)
                };
                let root = std::fs::canonicalize(root).ok()?;
                if approved.starts_with(&root) {
                    let relative = approved.strip_prefix(&root).ok()?.to_path_buf();
                    Some((root, relative))
                } else {
                    None
                }
            })
            .max_by_key(|(root, _)| root.components().count())
    }

    /// Capture the target identity before consulting the nested `Read` gate.
    /// This snapshot is intentionally content-free; the launcher later opens
    /// the same target through a rooted no-follow handle.
    fn script_path_approval_snapshot(
        &self,
        requested: &Path,
    ) -> Option<WorkflowScriptPathApproval> {
        let approved = std::fs::canonicalize(requested).ok()?;
        let (root, relative) = self.rooted_location_for_approval(&approved)?;
        Some(WorkflowScriptPathApproval {
            requested: requested.to_path_buf(),
            approved,
            root,
            relative,
        })
    }

    fn approval_key(ctx: &ToolUseContext, requested: &Path) -> (Option<String>, PathBuf) {
        (
            ctx.tool_use_id.as_ref().map(ToString::to_string),
            requested.to_path_buf(),
        )
    }

    fn remember_script_path_approval(
        &self,
        ctx: &ToolUseContext,
        approval: WorkflowScriptPathApproval,
    ) {
        let key = Self::approval_key(ctx, &approval.requested);
        let mut pending = self
            .script_path_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.entry(key).or_default().push(approval);
    }

    fn take_script_path_approval(
        &self,
        ctx: &ToolUseContext,
        requested: &Path,
    ) -> Option<WorkflowScriptPathApproval> {
        let key = Self::approval_key(ctx, requested);
        let mut pending = self
            .script_path_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let approvals = pending.get_mut(&key)?;
        let approval = approvals.pop();
        if approvals.is_empty() {
            pending.remove(&key);
        }
        approval
    }

    /// Seed the session-owned `workflowSizeGuideline` from startup settings.
    #[must_use]
    pub fn with_size_guideline(mut self, size: WorkflowSizeGuideline) -> Self {
        self.size_guideline = size;
        if let Some(state) = self.size_guideline_state.as_ref() {
            let managed = state.managed();
            let _ = state.set(size.as_wire(), managed);
        } else {
            let managed = platform_api::session_flags::workflow_size_guideline_is_managed();
            let _ =
                platform_api::session_flags::set_workflow_size_guideline(size.as_wire(), managed);
        }
        self
    }

    /// Seed the session-owned guideline without losing whether it came from
    /// the built-in default. Claude's 2.1.245 warning uses 25 agents for the
    /// default `medium`, but uses 15 when the user explicitly selected medium.
    #[must_use]
    pub fn with_size_guideline_source(
        mut self,
        size: WorkflowSizeGuideline,
        managed: bool,
        is_default: bool,
    ) -> Self {
        self.size_guideline = size;
        if let Some(state) = self.size_guideline_state.as_ref() {
            let _ = state.set_with_source(size.as_wire(), managed, is_default);
        } else {
            let _ = platform_api::session_flags::set_workflow_size_guideline_with_source(
                size.as_wire(),
                managed,
                is_default,
            );
        }
        self
    }

    /// Share the session-owned workflow-size setting with the tool.
    #[must_use]
    pub fn with_size_guideline_state(
        mut self,
        state: platform_api::session_flags::WorkflowSizeGuidelineState,
    ) -> Self {
        self.size_guideline = WorkflowSizeGuideline::from_wire(state.value());
        self.size_guideline_state = Some(state);
        self
    }

    /// Set the MANAGED-settings `disableWorkflows` gate (binary `fbn()`'s
    /// second arm, `$H()?.settings.disableWorkflows === true`).
    ///
    /// Threaded at registration like [`Self::with_size_guideline`] rather than
    /// through `ToolStaticContext`, which carries only feature flags. Without
    /// it an organization that set `disableWorkflows: true` still had the tool
    /// advertised and executable — the policy was parsed by nothing.
    #[must_use]
    pub fn with_disable_workflows(mut self, disabled: bool) -> Self {
        self.managed_disable_workflows = disabled;
        self
    }

    /// Share the session-owned dynamic-workflow gate with the tool.
    #[must_use]
    pub fn with_dynamic_workflows_gate(
        mut self,
        gate: platform_api::session_flags::DynamicWorkflowsGate,
    ) -> Self {
        self.dynamic_workflows_gate = Some(gate);
        self
    }

    /// Set the session-resolved dynamic-workflow availability (`pA()`).
    #[must_use]
    pub fn with_session_enabled(mut self, enabled: bool) -> Self {
        self.session_enabled = enabled;
        self
    }

    fn workflow_session_enabled(&self) -> bool {
        self.dynamic_workflows_gate
            .as_ref()
            .map(|gate| gate.enabled())
            .unwrap_or(self.session_enabled)
    }

    fn workflow_size_guideline(&self) -> WorkflowSizeGuideline {
        self.size_guideline_state
            .as_ref()
            .map(|state| WorkflowSizeGuideline::from_wire(state.value()))
            .unwrap_or_else(|| {
                WorkflowSizeGuideline::from_wire(
                    platform_api::session_flags::workflow_size_guideline(),
                )
            })
    }

    /// Upstream `nre(tools)` — can THIS request load the `workflow-authoring`
    /// skill?
    ///
    /// Upstream answers with six conditions: workflows enabled, bundled skills
    /// not disabled, slash commands not disabled, the entrypoint is not
    /// `local-agent`, no `skillOverrides` entry of `off` /
    /// `user-invocable-only`, the session skill allowlist (when there is one)
    /// includes it — and then, per call, that the `Skill` tool is among the
    /// advertised tools.
    ///
    /// This port has no substrate for three of them — `disableBundledSkills`,
    /// `skillOverrides` and the session skill allowlist do not exist here — so
    /// it answers the question those six conditions are asking, rather than
    /// reproducing the conditions: the registrar reports that the skill exists,
    /// and the wire-schema build reports whether this request advertises the
    /// `Skill` tool that loads it. Both are published by the code that knows
    /// the fact, and either being false takes the inline branch.
    ///
    /// ⛔ False must stay the safe answer. An unset flag inlines the reference,
    /// which costs tokens; a wrongly-set one points at a skill the model cannot
    /// load, which makes every documented hook unreachable.
    fn authoring_skill_reachable(&self, _options: &PromptOptions) -> bool {
        platform_api::session_flags::workflow_authoring_skill_reachable()
    }

    /// Is the tool disabled, by env var OR managed setting? Binary `fbn()`.
    fn workflows_disabled(&self) -> bool {
        !workflows_enabled(self.managed_disable_workflows)
    }

    fn spec_from_input(input: &Value) -> WorkflowLaunchSpec {
        let s = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
        WorkflowLaunchSpec {
            script: s("script"),
            name: s("name"),
            script_path: s("scriptPath"),
            args: input.get("args").cloned(),
            resume_from_run_id: s("resumeFromRunId"),
            session_uuid: None,
            tool_use_id: None,
            launched_from_subagent: false,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
            script_path_approval: None,
        }
    }

    /// Resolve a model-supplied script path against the live session cwd.
    /// This is intentionally lexical only: symlink and containment behavior
    /// belongs to the existing file/permission paths and must not be replaced
    /// by a looser Workflow-specific canonicalization.
    fn resolve_script_path_for_permission(&self, raw: &str) -> Result<PathBuf, String> {
        if raw.starts_with("\\\\") {
            return Err(format!(
                "UNC paths are not allowed for workflow scriptPath: {raw}"
            ));
        }
        if raw.contains('\0') {
            return Err("workflow scriptPath contains a NUL character".into());
        }
        let path = Path::new(raw);
        if path.as_os_str().is_empty() {
            return Err("workflow scriptPath must not be empty".into());
        }
        Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.current_cwd().join(path)
        })
    }

    fn script_path_permission_input(&self, raw: &str) -> Result<(PathBuf, Value), String> {
        let path = self.resolve_script_path_for_permission(raw)?;
        let wire = path.to_string_lossy().into_owned();
        Ok((path, json!({ "file_path": wire })))
    }

    fn allow_script_path_permission(path: &Path) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: format!(
                    "Workflow scriptPath is authorized as Read: {}",
                    path.display()
                ),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata {
                blocked_path: Some(path.to_string_lossy().into_owned()),
                ..PermissionMetadata::default()
            },
        }
    }

    fn deny_script_path_permission(path: &Path, reason: String) -> PermissionResult {
        PermissionResult::Deny {
            reason: PermissionDecisionReason::Other {
                reason: reason.clone(),
            },
            explanation: Some(reason),
            metadata: PermissionMetadata {
                blocked_path: Some(path.to_string_lossy().into_owned()),
                ..PermissionMetadata::default()
            },
        }
    }

    fn ask_script_path_permission(path: &Path) -> PermissionResult {
        let wire = path.to_string_lossy().into_owned();
        let message = format!("Workflow needs Read permission for scriptPath: {wire}");
        PermissionResult::Ask {
            reason: PermissionDecisionReason::Other {
                reason: message.clone(),
            },
            prompt: permission::result::PermissionPrompt {
                title: "Read workflow script".into(),
                message,
                options: Vec::new(),
            },
            pending_classifier_check: None,
            metadata: PermissionMetadata {
                blocked_path: Some(wire),
                ..PermissionMetadata::default()
            },
        }
    }

    /// List saved workflow names from project and user workflow directories.
    /// Returns a comma-joined string for the errorCode-1b message. Missing
    /// directories are ignored.
    fn list_available_workflow_names(&self) -> Option<String> {
        let mut saw_dir = false;
        let mut names: Vec<String> = BUILTIN_WORKFLOWS
            .names()
            .into_iter()
            .map(str::to_string)
            .collect();
        for dir in saved_workflow_dirs(&self.current_cwd()) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            saw_dir = true;
            names.extend(entries.filter_map(|e| {
                let fname = e.ok()?.file_name();
                let fname = fname.to_string_lossy();
                // Strip known extensions to get the bare name.
                for ext in [".js", ".mjs", ".ts"] {
                    if let Some(stem) = fname.strip_suffix(ext) {
                        return Some(stem.to_string());
                    }
                }
                Some(fname.into_owned())
            }));
        }
        if let Some(registry) = &self.plugin_workflows {
            names.extend(registry.names());
        }
        if !saw_dir && names.is_empty() {
            return None;
        }
        names.sort();
        names.dedup();
        Some(names.join(", "))
    }
}

#[async_trait]
impl Tool for WorkflowTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<&str> {
        Some("orchestrate subagents with deterministic JavaScript workflow")
    }
    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }
    fn input_validation_issues(&self, input: &Value) -> Vec<Value> {
        // 2.1.263 Xt retains refinements absent from its exported JSON Schema.
        let mut issues = Vec::new();
        if input
            .get("script")
            .and_then(Value::as_str)
            .is_some_and(|script| {
                script.chars().any(|c| {
                    let code = u32::from(c);
                    (code < 32 && code != 9 && code != 10) || (127..=159).contains(&code)
                })
            })
        {
            issues.push(serde_json::json!({"code":"custom","path":["script"],"message":"script contains control characters that would be hidden in the approval dialog"}));
        }
        for key in ["name", "scriptPath"] {
            if input
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|value| value.chars().any(hidden_workflow_input_char))
            {
                issues.push(serde_json::json!({"code":"custom","path":[key],"message":"contains control or invisible format characters"}));
            }
        }
        let base_types_parse = input.is_object()
            && [
                "script",
                "name",
                "description",
                "title",
                "scriptPath",
                "resumeFromRunId",
            ]
            .iter()
            .all(|key| input.get(*key).is_none_or(Value::is_string));
        if base_types_parse
            && !["script", "name", "scriptPath"].iter().any(|key| {
                input
                    .get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
            })
        {
            issues.push(serde_json::json!({"code":"custom","path":[],"message":"Must provide script, name, scriptPath, or runId"}));
        }
        issues
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        // Port of `fbn()` + `pA()` from claude-code v2.1.245.
        // `fbn()` returns true (= disable) when:
        //   `isEnvTruthy(process.env.LINGXI_DISABLE_WORKFLOWS)` OR
        //   `$H()?.settings.disableWorkflows === true`
        //
        // BOTH arms are implemented. The managed-setting arm is threaded at
        // REGISTRATION (`with_disable_workflows`) rather than through
        // `ToolStaticContext`, which carries only feature flags — the seam that
        // previously blocked it. Before this, an organization setting
        // `disableWorkflows: true` still got the tool advertised and executable.
        //
        // The org/launch (`Xs("allow_workflows")`), GrowthBook
        // (`tengu_workflows_enabled`), and plan-availability gates have no LingXi
        // backing and are treated as permissive (enabled), matching the
        // Max/Team/null-plan default.
        !self.workflows_disabled() && self.workflow_session_enabled()
    }
    fn max_result_size_chars(&self) -> usize {
        100000
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        // A workflow fans out many side-effecting agents.
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }

    async fn check_permissions(&self, input: &Value, ctx: &ToolUseContext) -> PermissionResult {
        let Some(raw_path) = input
            .get("scriptPath")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
        else {
            // Inline and named workflows do not read a caller-selected file at
            // this boundary. Their launcher still validates the resolved bytes
            // after the outer Workflow permission gate has completed.
            return PermissionResult::Allow {
                reason: PermissionDecisionReason::Other {
                    reason: "Workflow launch — spawned agents are individually permissioned".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: PermissionMetadata::default(),
            };
        };

        let (path, read_input) = match self.script_path_permission_input(raw_path) {
            Ok(value) => value,
            Err(reason) => {
                return Self::deny_script_path_permission(Path::new(raw_path), reason);
            }
        };
        // Capture the resolved target BEFORE the permission check. If a parent
        // or leaf symlink is retargeted after this point, the rooted launcher
        // will reject the change instead of following the new target.
        let approval = self.script_path_approval_snapshot(&path);

        // The gate is the live source of permission decisions, including
        // updated rules and the interactive prompt transport. Ask it about the
        // canonical file-read tool/input shape, not about Workflow, so a
        // `Read(...)` rule has the same scope and symlink behavior as FileRead.
        if let Some(gate) = &self.permission_gate {
            let check_ctx = platform_api::permission_gate::PermissionCheckContext {
                tool_use_id: ctx.tool_use_id.as_ref().map(|id| id.as_str().to_string()),
                is_non_interactive_session: ctx.options.is_non_interactive_session,
                ..Default::default()
            };
            let result = match gate
                .resolve_detailed_or_abort("Read", &read_input, &check_ctx)
                .await
            {
                Ok(platform_api::permission_gate::PermissionResolution::Allow { .. }) => {
                    Self::allow_script_path_permission(&path)
                }
                Ok(platform_api::permission_gate::PermissionResolution::Deny {
                    reason, ..
                })
                | Err(platform_api::permission_gate::PermissionAbort { message: reason }) => {
                    Self::deny_script_path_permission(&path, reason)
                }
                Ok(platform_api::permission_gate::PermissionResolution::Ask)
                | Ok(platform_api::permission_gate::PermissionResolution::AskWithContext {
                    ..
                }) => Self::ask_script_path_permission(&path),
            };
            if matches!(
                result,
                PermissionResult::Allow { .. } | PermissionResult::Ask { .. }
            ) {
                if let Some(approval) = approval.clone() {
                    self.remember_script_path_approval(ctx, approval);
                }
            }
            return result;
        }

        // A boot policy is useful for hosts without an interactive/live gate.
        // It is the same PermissionPolicy used by the file tools, so do not
        // duplicate its rule matching or invent a Workflow-specific path walk.
        if let Some(policy) = &self.permission_policy {
            let result = policy.authorize("Read", &read_input);
            if matches!(
                result,
                PermissionResult::Allow { .. } | PermissionResult::Ask { .. }
            ) {
                if let Some(approval) = approval {
                    self.remember_script_path_approval(ctx, approval);
                }
            }
            return result;
        }

        // Missing wiring must fail closed. The outer dispatcher can surface
        // this Ask through its normal permission transport; crucially, no
        // script bytes have been read yet.
        let result = Self::ask_script_path_permission(&path);
        if let Some(approval) = approval {
            self.remember_script_path_approval(ctx, approval);
        }
        result
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "Running a workflow".into()
    }

    async fn prompt(&self, options: &PromptOptions) -> String {
        // Binary (2.1.267): `async prompt(e){ return Epn(nre(e?.tools)) +
        // Kvn(ne().workflowSizeGuideline) + ct() }` — the assembled description
        // plus the (possibly-empty) size-guideline appendix. (`ct()` is
        // `return ""` in this build, so there is nothing to port for it.)
        // `/config` updates the live session snapshot, so prefer that value over
        // the construction-time fallback on every prompt build.
        format!(
            "{}{}",
            assemble_description(
                self.authoring_skill_reachable(options),
                subagent_model_forced()
            ),
            self.workflow_size_guideline().prompt_appendix()
        )
    }

    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        // ── Schema-level `.refine()` on the `script` field (claude-code 2.1.195) ──
        // The zod input schema is `script: A.string().max(z$).refine(UIe, hYp)`,
        // where `UIe`/`WRa` reject any control char (code < 32 except 9/10, or
        // 127–159). In claude-code this runs at input-parse time, BEFORE the
        // validateInput gates below — so it is the first check here. It applies
        // ONLY to the raw inline `script` field (not scriptPath-read content nor
        // name-resolved scripts). Message byte-exact (`hYp`).
        if let Some(raw_script) = input.get("script").and_then(Value::as_str) {
            if raw_script.chars().any(|c| {
                let code = c as u32;
                if code == 9 || code == 10 {
                    false
                } else {
                    code < 32 || (127..=159).contains(&code)
                }
            }) {
                return Err(ValidationError(
                    "script contains control characters that would be hidden in the approval dialog"
                        .into(),
                ));
            }
        }

        // ── Gate order mirrors claude-code v2.1.245 validateInput ──
        //
        // errorCode 7 — abort / input truncated
        // ⚠️ UNREACHABLE: the binary's `yke(t.abortController.signal)` is the
        // HTTP-layer server-retraction signal; LingXi's `ToolUseContext` exposes a
        // local `cancel` (CancellationToken) that fires on sibling errors / user
        // interrupt — a different thing. No server-fallback abort signal is threaded
        // to `validate_input`. Kept as a named constant for documentation; the gate
        // is not wired.
        //
        // errorCode 5 — `disableWorkflows`: env var OR managed setting
        // (binary `fbn()`). Both arms reach here now; the managed value is
        // threaded at registration (`with_disable_workflows`).
        if self.workflows_disabled() {
            return Err(ValidationError(
                "Dynamic workflows are disabled by managed settings (`disableWorkflows`).".into(),
            ));
        }

        // errorCode 6 — session gate (`pA()`)
        // The composition root resolves the effective `enableWorkflows`
        // setting once per session and threads it here, alongside the process-
        // global publication for `/effort`/TUI consumers.
        if !self.workflow_session_enabled() {
            return Err(ValidationError(
                "Dynamic workflows are not enabled for this session (org policy, launch gate, or the \"Dynamic workflows\" setting in /config).".into(),
            ));
        }

        // errorCode 1 — selector/path shape. Content resolution is deliberately
        // not part of validate_input: the dispatcher invokes this method before
        // permission checks, so reading a caller-selected script here would
        // bypass the `Read` policy and could leak file bytes/errors.
        let s = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
        let script_path = s("scriptPath").filter(|v| !v.is_empty());
        let script = s("script").filter(|v| !v.is_empty());
        let name = s("name").filter(|v| !v.is_empty());

        // Resolved script text (for errorCode 2 and 4 checks below). A
        // `scriptPath` is deliberately represented as `None`: its bytes must
        // not be read until the nested `Read` permission check has completed.
        let resolved_script: Option<String>;

        if let Some(ref path) = script_path {
            // Path checks are lexical and therefore safe before authorization;
            // in particular this does not stat/canonicalize/follow symlinks.
            self.resolve_script_path_for_permission(path)
                .map_err(ValidationError)?;
            resolved_script = None;
        } else if let Some(ref wf_name) = name {
            // Built-ins are immutable and must win before project/user files.
            // Keep validation on the same resolver order as the launcher:
            // otherwise an invalid project file named `deep-research.js` could
            // reject a launch whose execution would actually use the bundled
            // script.
            if let Some(descriptor) = BUILTIN_WORKFLOWS.get(wf_name) {
                resolved_script = Some(
                    script
                        .clone()
                        .unwrap_or_else(|| descriptor.script.to_string()),
                );
            } else {
                // Try to resolve from saved workflows (project first, then user).
                let mut found: Option<String> = None;
                for candidate in saved_workflow_candidates(&self.current_cwd(), wf_name) {
                    match std::fs::read_to_string(&candidate) {
                        Ok(src) => {
                            found = Some(src);
                            break;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(_) => continue,
                    }
                }
                // §14 — a plugin's declared/auto-scanned workflow joins the
                // search path LAST: project/user files above already win on a
                // name collision (oracle precedence project/user > plugin >
                // built-in; namespacing means a real collision only happens
                // if a project/user file is itself literally named
                // `<plugin>:<name>.js`).
                if found.is_none() {
                    if let Some(path) = self
                        .plugin_workflows
                        .as_ref()
                        .and_then(|registry| registry.resolve(wf_name))
                    {
                        found = std::fs::read_to_string(&path).ok();
                    }
                }
                if let Some(src) = found {
                    resolved_script = Some(script.clone().unwrap_or(src));
                } else {
                    // 1b — workflow name not found; list available names
                    let available: String =
                        self.list_available_workflow_names().unwrap_or_default();
                    let list = if available.is_empty() {
                        "(none)".to_string()
                    } else {
                        available
                    };
                    return Err(ValidationError(format!(
                        "Workflow \"{wf_name}\" not found. Available: {list}"
                    )));
                }
            }
        } else if let Some(ref inline) = script {
            resolved_script = Some(inline.clone());
        } else {
            // 1a — none of script/name/scriptPath provided
            return Err(ValidationError(
                "Must provide script, name, scriptPath, or runId".into(),
            ));
        }

        // errorCode 2 — parse/meta error (`Invalid workflow script: ${error}`)
        // Mirrors binary's `Bw(n.script)` → `validate_meta`.
        if let Some(resolved_script) = resolved_script.as_deref() {
            if let Err(e) = workflow::validate_meta(resolved_script) {
                return Err(ValidationError(format!("Invalid workflow script: {e}")));
            }
        }

        // errorCode 4 — determinism violation (inline script only)
        // Binary: `e.script && HKa(r.scriptBody)`. `e.script` is the RAW INLINE
        // `script` field from the input — it is falsy when `name` or `scriptPath`
        // is used. Only inline `script` input is checked; `name`-resolved saved
        // workflows and `scriptPath`-sourced files skip this gate.
        if script_path.is_none() && script.is_some() {
            if let Some(resolved_script) = resolved_script.as_deref() {
                if let Err(e) = workflow::check_determinism(resolved_script) {
                    // The WorkflowError Display wraps the message; we want the raw
                    // NON_DETERMINISTIC_MESSAGE, which lives inside WorkflowError::Script.
                    use workflow::WorkflowError;
                    let msg = match e {
                        WorkflowError::Script(m) => m,
                        WorkflowError::Engine(m) => m,
                    };
                    return Err(ValidationError(msg));
                }
            }
        } else if let Some(name) = name {
            // Named workflows are resolved and validated by the launcher after
            // permission. Keep this branch intentionally I/O-free.
            if name.contains('\0') {
                return Err(ValidationError(
                    "workflow name contains a NUL character".into(),
                ));
            }
        }

        // errorCode 3 — still-running resume target
        // The binary looks up a `local_workflow` task by `workflowRunId ===
        // resumeFromRunId` in the task registry. LingXi's `ToolUseContext` does
        // not carry a task-registry handle, so this gate is NOT enforced here;
        // instead it is enforced at LAUNCH time by `TaskRegistryWorkflowLauncher`
        // (the composition root, which owns the registry — see
        // `find_running_workflow_by_run_id`), which returns the byte-exact
        // "Workflow … is still running … Stop it first with TaskStop(…)" error.
        // Observable behaviour matches: a resume of a still-running workflow is
        // rejected before a second run starts.

        Ok(())
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        // 2.1.266 `a0`: refuse while THIS agent's own stop is still completing
        // (see `agent_processes::mark_stop_pending`).
        if let Some(agent_id) = ctx.agent_id {
            if platform_api::agent_processes::is_stop_pending(&agent_id.to_string()) {
                return Err(ToolError::InvalidInput(
                    platform_api::agent_processes::stop_pending_refusal(
                        "launch workflows or act on existing runs.",
                    ),
                ));
            }
        }

        let launcher = self.launcher.as_ref().ok_or_else(|| {
            ToolError::Internal("Workflow launching is not available in this host".into())
        })?;
        let mut spec = Self::spec_from_input(&input);
        // A Workflow task can outlive the turn that launched it. Capture the
        // owning session at this boundary so later `fusion()` calls continue
        // to use the original session-scoped budget after a clear/resume or
        // model/router change.
        if let Some(session) = ctx.session.as_ref() {
            // WorkflowLaunchSpec is a host boundary: desktop/mobile task
            // launchers use the bare UUID for transcript directory identity,
            // while Fusion requests may add the `sess:` wire prefix later.
            spec.session_uuid = Some(session.lock().await.session_id.as_uuid().to_string());
        } else if let Some(session_id) = ctx.origin_session_id {
            // Subagent tool invocations intentionally do not clone the live
            // SessionState mutex. Their trusted origin id is propagated on
            // the invocation context instead, so a background Workflow still
            // binds later Fusion calls to the session that launched it.
            spec.session_uuid = Some(session_id.as_uuid().to_string());
        }
        spec.tool_use_id = ctx.tool_use_id.as_ref().map(ToString::to_string);
        if let Some(raw_path) = spec.script_path.as_deref().filter(|path| !path.is_empty()) {
            let requested = self
                .resolve_script_path_for_permission(raw_path)
                .map_err(ToolError::InvalidInput)?;
            let approval = self
                .take_script_path_approval(&ctx, &requested)
                .ok_or_else(|| {
                    ToolError::Internal(
                        "Workflow scriptPath permission snapshot is missing; refusing to read it"
                            .into(),
                    )
                })?;
            spec.script_path_approval = Some(approval);
        }
        spec.launched_from_subagent = ctx.agent_id.is_some();
        spec.creator_teammate_name = ctx.agent_name.clone();
        spec.creator_team_name = ctx.team_name.clone();
        spec.creator_agent_id = ctx.agent_id.map(|id| id.to_string());
        // Plugin scripts are Host-owned; callers must resume through registry
        // lookup rather than edit/read the persisted copy outside their workspace.
        let plugin_resume = spec
            .name
            .as_deref()
            .filter(|name| {
                !spec
                    .script
                    .as_deref()
                    .is_some_and(|value| !value.is_empty())
                    && !spec
                        .script_path
                        .as_deref()
                        .is_some_and(|value| !value.is_empty())
                    && workflow_source_for_name(
                        &self.current_cwd(),
                        name,
                        self.plugin_workflows.as_deref(),
                    ) == Some("plugin")
            })
            .map(|name| {
                let mut request = json!({ "name": name });
                if let Some(args) = &spec.args {
                    request["args"] = args.clone();
                }
                request
            });
        let launch_result = launcher
            .launch(spec)
            .await
            .map_err(|error| ToolError::Internal(error.to_string()))?;
        if let Some(error) = launch_result.error {
            let model_content =
                format!("Workflow script has a syntax error and was not launched:\n{error}");
            return Ok(ToolCallResult {
                data: json!({
                    "error": error,
                    "model_content": model_content,
                }),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: true,
                mcp_meta: None,
            });
        }
        // claude-code result shape (output schema `sUp`, local path): status,
        // taskId, taskType, plus the optional workflowName / runId / scriptPath /
        // summary / transcriptDir. The "remote_launched"/"remote_agent" + sessionUrl
        // variants are the CCR/remote path, out of scope for the single-process
        // build. Optional fields are omitted when the host did not supply them.
        //
        // The model-facing launch text mirrors `mapToolResultToToolResultBlockParam`
        // (binary §1 verbatim, async_launched case):
        //   "Workflow launched in background. Task ID: {taskId}"
        //   + optional "\nSummary: {summary}"
        //   + optional "\nTranscript dir: {transcriptDir}"
        //   + optional "\nScript file: …\n(Edit …)"
        //   + optional "\nRun ID: …\nTo resume …"
        //   + "\n\nYou will be notified when it completes. Use /workflows to watch live progress."
        let task_id = launch_result.task_id.clone();
        let summary = launch_result.summary.as_deref();
        let transcript = launch_result.transcript_dir.as_deref();
        let script_p = launch_result.script_path.as_deref();
        let run_id_str = launch_result.run_id.as_deref();

        let n = summary.map_or_else(String::new, |s| format!("\nSummary: {s}"));
        let r = transcript.map_or_else(String::new, |t| format!("\nTranscript dir: {t}"));
        let o = script_p
            .filter(|_| plugin_resume.is_none())
            .map_or_else(String::new, |p| {
                format!(
                "\nScript file: {p}\n(Edit this file with Write/Edit and re-invoke Workflow with \
                 {{scriptPath: \"{p}\"}} to iterate without resending the script.)"
            )
            });
        let s = match (plugin_resume, script_p, run_id_str) {
            (Some(mut request), _, Some(rid)) => {
                request["resumeFromRunId"] = Value::String(rid.to_string());
                format!(
                    "\nRun ID: {rid}\nTo resume this Host-owned plugin workflow: Workflow({request}). \
                     The Host resolves the registered script; completed unchanged agents return cached results."
                )
            }
            (None, Some(p), Some(rid)) => format!(
                "\nRun ID: {rid}\nTo resume after editing the script: \
                 Workflow({{scriptPath: \"{p}\", resumeFromRunId: \"{rid}\"}}) \
                 — completed agents return cached results (cached results may themselves be empty — inspect journal.jsonl before assuming there is something to recover)."
            ),
            _ => String::new(),
        };
        let model_content = format!(
            "Workflow launched in background. Task ID: {task_id}{n}{r}{o}{s}\
             \n\nYou will be notified when it completes. Use /workflows to watch live progress."
        );

        let mut data = json!({
            "status": "async_launched",
            "taskId": task_id,
            "taskType": "local_workflow",
            "model_content": model_content,
        });
        let obj = data.as_object_mut().expect("json object");
        if let Some(name) = launch_result.workflow_name {
            obj.insert("workflowName".into(), Value::String(name));
        }
        if let Some(rid) = launch_result.run_id {
            obj.insert("runId".into(), Value::String(rid));
        }
        if let Some(path) = launch_result.script_path {
            obj.insert("scriptPath".into(), Value::String(path));
        }
        if let Some(sum) = launch_result.summary {
            obj.insert("summary".into(), Value::String(sum));
        }
        if let Some(td) = launch_result.transcript_dir {
            obj.insert("transcriptDir".into(), Value::String(td));
        }
        Ok(ToolCallResult {
            data,
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// Register the `Workflow` tool against `reg`. Without a wired launcher the
/// model-facing surface is served but `call` errors; the composition root
/// constructs [`WorkflowTool::new(Some(launcher))`] directly once the workflow
/// task seam is available.
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    let mut tool = WorkflowTool::new(None).with_permission_policy(ctx.permission_policy);
    if let Some(gate) = ctx.permission_gate {
        tool = tool.with_permission_gate(gate);
    }
    reg.register_builtin(Arc::new(tool));
}

#[cfg(test)]
mod tests {
    // The oracle texts and their register moved to the workflow runtime
    // crate (§8.1: a command crate may not depend on a tool crate, and the
    // `workflow-authoring` skill reads the same texts).
    use super::*;
    use std::sync::Mutex as StdMutex;
    use workflow::description::*;

    struct MockLauncher {
        task_id: String,
        seen: StdMutex<Option<WorkflowLaunchSpec>>,
    }
    impl MockLauncher {
        fn new(task_id: &str) -> Arc<Self> {
            Arc::new(Self {
                task_id: task_id.to_string(),
                seen: StdMutex::new(None),
            })
        }
    }
    #[async_trait]
    impl WorkflowLauncher for MockLauncher {
        async fn launch(
            &self,
            spec: WorkflowLaunchSpec,
        ) -> Result<WorkflowLaunched, WorkflowLaunchError> {
            *self.seen.lock().unwrap() = Some(spec);
            Ok(WorkflowLaunched {
                task_id: self.task_id.clone(),
                ..Default::default()
            })
        }
    }

    fn tool(launcher: Option<Arc<dyn WorkflowLauncher>>) -> WorkflowTool {
        WorkflowTool::new(launcher)
    }

    struct RecordingPermissionGate {
        calls: Arc<StdMutex<Vec<(String, Value)>>>,
        decision: platform_api::permission_gate::PermissionDecision,
    }

    #[async_trait]
    impl platform_api::permission_gate::PermissionGate for RecordingPermissionGate {
        async fn check(
            &self,
            name: &str,
            input: &Value,
        ) -> platform_api::permission_gate::PermissionDecision {
            self.calls
                .lock()
                .unwrap()
                .push((name.to_string(), input.clone()));
            self.decision.clone()
        }
    }

    struct ContextAwarePermissionGate {
        calls: Arc<StdMutex<Vec<(String, Value)>>>,
        contexts: Arc<StdMutex<Vec<platform_api::permission_gate::PermissionCheckContext>>>,
    }

    #[async_trait]
    impl platform_api::permission_gate::PermissionGate for ContextAwarePermissionGate {
        async fn check(
            &self,
            _name: &str,
            _input: &Value,
        ) -> platform_api::permission_gate::PermissionDecision {
            panic!("Workflow scriptPath must use resolve_detailed_or_abort")
        }

        async fn resolve_detailed_or_abort(
            &self,
            name: &str,
            input: &Value,
            ctx: &platform_api::permission_gate::PermissionCheckContext,
        ) -> Result<
            platform_api::permission_gate::PermissionResolution,
            platform_api::permission_gate::PermissionAbort,
        > {
            self.calls
                .lock()
                .unwrap()
                .push((name.to_string(), input.clone()));
            self.contexts.lock().unwrap().push(ctx.clone());
            if ctx.is_non_interactive_session {
                return Ok(platform_api::permission_gate::PermissionResolution::Deny {
                    reason: permission::headless_gate::headless_deny_message(name),
                    source: platform_api::permission_gate::PermissionDecisionSource::Unspecified,
                    rule_source: None,
                    decision_reason_type: None,
                    decision_reason: None,
                    behavior_ask: false,
                    content_blocks: Vec::new(),
                });
            }
            Ok(platform_api::permission_gate::PermissionResolution::Ask)
        }
    }

    fn unique_temp_path(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lingxi-workflow-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn with_config_dir_env<T>(path: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let old = std::env::var_os(branding::CONFIG_DIR_ENV);
        std::env::set_var(branding::CONFIG_DIR_ENV, path);
        let out = f();
        match old {
            Some(v) => std::env::set_var(branding::CONFIG_DIR_ENV, v),
            None => std::env::remove_var(branding::CONFIG_DIR_ENV),
        }
        out
    }

    #[test]
    fn name_is_workflow() {
        assert_eq!(tool(None).name(), "Workflow");
    }

    #[test]
    fn named_workflow_source_matches_claude_categories() {
        assert_eq!(
            workflow_source_for_name(std::path::Path::new("."), "deep-research", None),
            Some("built-in")
        );
        assert_eq!(
            workflow_source_for_name(std::path::Path::new("."), "missing", None),
            None
        );
    }

    /// `tengu_workflow_launched.workflow_source` must name the tier the launch
    /// actually resolved from. The oracle stamps `source:"plugin"` on every
    /// plugin record (`v()` @169045670) and emits it verbatim
    /// (`se=e.scriptPath?"scriptPath":h??"inline"`,
    /// `workflow_source:c(se)` @172565293), so `"plugin"` is a real emitted
    /// value — without the plugin arm a plugin workflow is reported as the
    /// `custom` fallback `engine-desktop`'s `named_source.unwrap_or("custom")`
    /// supplies.
    #[test]
    fn named_workflow_source_reports_plugin_for_a_registry_hit() {
        let registry = workflow::PluginWorkflowRegistry::new();
        registry.register(vec![workflow::PluginWorkflowEntry {
            name: "acme:deploy".into(),
            script_path: std::path::PathBuf::from("/plugins/acme/scripts/deploy.js"),
        }]);
        assert_eq!(
            workflow_source_for_name(std::path::Path::new("."), "acme:deploy", Some(&registry)),
            Some("plugin")
        );
        // The registry is consulted LAST — a built-in still wins.
        assert_eq!(
            workflow_source_for_name(std::path::Path::new("."), "deep-research", Some(&registry)),
            Some("built-in")
        );
        assert_eq!(
            workflow_source_for_name(std::path::Path::new("."), "acme:missing", Some(&registry)),
            None
        );
    }

    #[test]
    fn plugin_workflow_registry_reaches_resolver_and_source_telemetry() {
        let cwd = unique_temp_path("plugin-resolver");
        std::fs::create_dir_all(&cwd).expect("create resolver cwd");
        let script_path = cwd.join("plugin-workflow.js");
        std::fs::write(&script_path, VALID_SCRIPT).expect("write plugin workflow");
        let registry = workflow::PluginWorkflowRegistry::new();
        registry.register(vec![workflow::PluginWorkflowEntry {
            name: "acme:review".to_string(),
            script_path: script_path.clone(),
        }]);

        let resolved = resolve_script_at(
            &cwd,
            &WorkflowLaunchSpec {
                name: Some("acme:review".to_string()),
                ..Default::default()
            },
            |path| std::fs::read_to_string(path),
            Some(&registry),
        )
        .expect("plugin workflow name must resolve");
        assert_eq!(resolved, VALID_SCRIPT);
        assert_eq!(
            workflow_source_for_name(&cwd, "acme:review", Some(&registry)),
            Some("plugin")
        );
        let _ = std::fs::remove_dir_all(cwd);
    }

    #[test]
    fn max_result_size_is_100000() {
        assert_eq!(tool(None).max_result_size_chars(), 100000);
    }

    #[test]
    fn resolve_script_precedence_and_name_lookup() {
        use std::collections::HashMap;
        let files: HashMap<&str, &str> = HashMap::from([
            ("/abs/wf.js", "FROM_PATH"),
            (".lingxi/workflows/review.js", "FROM_NAME"),
        ]);
        let read = |p: &str| {
            files
                .get(p)
                .map(|s| (*s).to_string())
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "nope"))
        };

        // scriptPath wins over an inline script.
        let spec = WorkflowLaunchSpec {
            script: Some("INLINE".into()),
            script_path: Some("/abs/wf.js".into()),
            ..Default::default()
        };
        assert_eq!(resolve_script(&spec, read).unwrap(), "FROM_PATH");

        // inline script when there is no scriptPath.
        let spec = WorkflowLaunchSpec {
            script: Some("INLINE".into()),
            ..Default::default()
        };
        assert_eq!(resolve_script(&spec, read).unwrap(), "INLINE");

        // name → .lingxi/workflows/<name>.js.
        let spec = WorkflowLaunchSpec {
            name: Some("review".into()),
            ..Default::default()
        };
        assert_eq!(resolve_script(&spec, read).unwrap(), "FROM_NAME");

        // A named workflow resolves first, while an explicit body overrides
        // the resolved source. An unknown name must not fall through to the
        // inline body.
        let spec = WorkflowLaunchSpec {
            name: Some("review".into()),
            script: Some("NAMED_OVERRIDE".into()),
            ..Default::default()
        };
        assert_eq!(resolve_script(&spec, read).unwrap(), "NAMED_OVERRIDE");
        let spec = WorkflowLaunchSpec {
            name: Some("missing".into()),
            script: Some("DO_NOT_FALL_THROUGH".into()),
            ..Default::default()
        };
        assert!(resolve_script(&spec, read).is_err());

        // unknown name + nothing-provided → errors.
        let spec = WorkflowLaunchSpec {
            name: Some("missing".into()),
            ..Default::default()
        };
        assert!(resolve_script(&spec, read).is_err());
        assert!(resolve_script(&WorkflowLaunchSpec::default(), read).is_err());
    }

    #[test]
    fn saved_workflow_shadows_builtin_name() {
        let read = |path: &str| {
            if path == ".lingxi/workflows/deep-research.js" {
                Ok("MALICIOUS_PROJECT_OVERRIDE".to_string())
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"))
            }
        };
        let resolved = resolve_script(
            &WorkflowLaunchSpec {
                name: Some("deep-research".into()),
                ..Default::default()
            },
            read,
        )
        .expect("built-in resolves");
        assert_eq!(resolved, "MALICIOUS_PROJECT_OVERRIDE");
    }

    #[test]
    fn workflow_listing_always_includes_builtin_names() {
        let names = tool(None)
            .list_available_workflow_names()
            .expect("built-ins");
        assert!(names.split(", ").any(|name| name == "deep-research"));
    }

    #[test]
    fn resolve_script_name_falls_back_to_user_workflows_dir() {
        use std::collections::HashMap;
        let _g = ENV_LOCK.lock().unwrap();
        let config_dir = unique_temp_path("resolve-user");
        let user_workflow = config_dir.join("workflows").join("review.mjs");
        let mut files: HashMap<String, String> = HashMap::new();
        files.insert(
            user_workflow.to_string_lossy().into_owned(),
            "FROM_USER".into(),
        );
        let read = |p: &str| {
            files
                .get(p)
                .cloned()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "nope"))
        };

        let got = with_config_dir_env(&config_dir, || {
            resolve_script(
                &WorkflowLaunchSpec {
                    name: Some("review".into()),
                    ..Default::default()
                },
                read,
            )
        })
        .unwrap();

        assert_eq!(got, "FROM_USER");
    }

    /// The ORACLE half of the old `description_matches_the_binary_byte_for_byte`.
    /// Unchanged in substance: the oracle files are still pinned to the binary,
    /// byte for byte. What moved out is the claim that the SHIPPED text equals
    /// them — that is now [`the_shipped_description_is_the_oracle_plus_the_register`].
    ///
    /// 2.1.267 split the 19,200-byte 2.1.245 description in two, so this pins
    /// both halves. Every marker below was re-derived by resolving `Epn`'s two
    /// operands in `src_178675664.js`: each one occurs in exactly ONE half, and
    /// the test asserts its absence from the other — a marker that merely
    /// "still appears somewhere" would not notice a passage sliding between the
    /// two documents, which is precisely the drift this split introduces.
    #[test]
    fn oracle_description_matches_the_binary_byte_for_byte() {
        // ── The per-request head (`t`) ──
        assert_eq!(
            ORACLE_DESCRIPTION.len(),
            3031,
            "oracle description byte length drifted from Claude Code 2.1.267"
        );
        assert!(ORACLE_DESCRIPTION.starts_with(
            "Execute a workflow script that orchestrates multiple subagents deterministically."
        ));
        assert!(ORACLE_DESCRIPTION.ends_with("No wasted wall-clock."));
        assert!(ORACLE_DESCRIPTION.contains("Use the Agent tool (if available)"));

        // ── The on-demand reference (`wpn()`), unforced rendering ──
        assert_eq!(
            ORACLE_AUTHORING_SKILL.len(),
            17141,
            "oracle authoring reference byte length drifted from Claude Code 2.1.267"
        );
        assert!(ORACLE_AUTHORING_SKILL.starts_with("# Workflow authoring reference"));
        assert!(ORACLE_AUTHORING_SKILL.ends_with("hand-author a continuation script."));
        assert!(ORACLE_AUTHORING_SKILL.contains("min(16, available CPUs - 2)"));
        assert!(ORACLE_AUTHORING_SKILL.contains("e.g. 'general-purpose', 'code-reviewer'"));
        assert!(ORACLE_AUTHORING_SKILL.contains("Before diagnosing why a completed workflow returned an empty or unexpected result, Read <transcriptDir>/journal.jsonl"));
        // The ${PP} interpolation resolved to the ▸ group marker.
        assert!(ORACLE_AUTHORING_SKILL.contains("\"▸ name\" group in /workflows"));

        // Each marker lives in ONE half. Without this, a paragraph migrating
        // between the two files would keep every `contains` above green.
        for stray in [
            "min(16, available CPUs - 2)",
            "e.g. 'general-purpose', 'code-reviewer'",
            "\"▸ name\" group in /workflows",
        ] {
            assert!(
                !ORACLE_DESCRIPTION.contains(stray),
                "{stray:?} belongs to the authoring reference, not the per-request description"
            );
        }
        assert!(
            !ORACLE_AUTHORING_SKILL.contains("Use the Agent tool (if available)"),
            "the opt-in clauses belong to the per-request description"
        );

        // The pointer sentence (`i`) is oracle text too: it is what the model
        // reads INSTEAD of the reference, so it is pinned byte-for-byte rather
        // than merely checked for the skill name.
        assert_eq!(
            AUTHORING_SKILL_POINTER,
            "Before writing a script, load the `workflow-authoring` skill \u{2014} the workflow \
             authoring reference: script API and gotchas, resume, the **Ultracode** section, \
             quality patterns, worked examples."
        );
        assert_eq!(AUTHORING_SKILL_POINTER.len(), 192);

        // No leftover raw escape sequences. `\\x` matters as much as `\\u`: the
        // binary stores `×` as `\\xD7`, and an extractor that only unescaped
        // `\\uXXXX` left it raw — which read as a genuine oracle delta.
        for oracle in [&*ORACLE_DESCRIPTION, &*ORACLE_AUTHORING_SKILL] {
            assert!(!oracle.contains("\\u2014"));
            assert!(!oracle.contains("\\x"));
        }
    }

    /// Every entry in the register carries the two things a bare byte count
    /// could never carry: a name and a reason. An anchor that is missing or
    /// ambiguous is a defect in the register, not a licence to skip the entry.
    #[test]
    fn every_description_divergence_is_named_reasoned_and_uniquely_anchored() {
        assert!(
            !DESCRIPTION_DIVERGENCES.is_empty(),
            "the register is empty — if every divergence was retired, restore the plain \
             byte-for-byte lock rather than leaving this machinery answering nothing"
        );
        let mut seen: Vec<&str> = Vec::new();
        for divergence in DESCRIPTION_DIVERGENCES.iter() {
            assert!(!divergence.id.trim().is_empty(), "a divergence has no id");
            assert!(
                !seen.contains(&divergence.id.as_str()),
                "duplicate divergence id `{}`",
                divergence.id
            );
            seen.push(&divergence.id);
            assert!(
                divergence.reason.trim().len() > 40,
                "divergence `{}` has no real reason. An unexplained divergence is exactly what \
                 this register exists to prevent — say what the oracle text gets wrong here.",
                divergence.id
            );
            assert!(
                !divergence.text.trim().is_empty(),
                "divergence `{}` inserts nothing",
                divergence.id
            );
            let (target, other, target_name, other_name) = match divergence.target {
                DivergenceTarget::Description => (
                    &*ORACLE_DESCRIPTION,
                    &*ORACLE_AUTHORING_SKILL,
                    "description",
                    "authoring reference",
                ),
                DivergenceTarget::Skill => (
                    &*ORACLE_AUTHORING_SKILL,
                    &*ORACLE_DESCRIPTION,
                    "authoring reference",
                    "description",
                ),
            };
            assert_eq!(
                target.matches(divergence.anchor.as_str()).count(),
                1,
                "divergence `{}`: its anchor must occur exactly once in the ORACLE {target_name}, \
                 so an oracle refresh that moves the anchored passage fails here by name",
                divergence.id
            );
            // ...and ZERO times in the other half. 2.1.267 split the text in
            // two; a passage that migrates across would otherwise keep its stale
            // `target` and still compose, landing the insert in the document the
            // register no longer describes.
            assert_eq!(
                other.matches(divergence.anchor.as_str()).count(),
                0,
                "divergence `{}`: its anchor also occurs in the {other_name}, so `target` is \
                 ambiguous — the anchored passage moved between the two oracle texts",
                divergence.id
            );
        }
    }

    /// The replacement for the old single number. It still catches an
    /// unregistered edit to either file — the length has to be the oracle plus
    /// exactly what the register declares — but it no longer conflates a
    /// deliberate divergence with accidental drift.
    #[test]
    fn the_shipped_description_is_the_oracle_plus_the_register() {
        for (target, oracle, shipped, name) in [
            (
                DivergenceTarget::Description,
                &*ORACLE_DESCRIPTION,
                &*DESCRIPTION,
                "description",
            ),
            (
                DivergenceTarget::Skill,
                &*ORACLE_AUTHORING_SKILL,
                &*AUTHORING_SKILL,
                "authoring reference",
            ),
        ] {
            let entries = || {
                DESCRIPTION_DIVERGENCES
                    .iter()
                    .filter(|d| d.target == target)
            };
            let registered: usize = entries().map(|d| d.text.len()).sum();
            assert_eq!(
                shipped.len(),
                oracle.len() + registered,
                "shipped {name} length is not the oracle plus its registered divergences: \
                 either the oracle drifted, or something was edited into one of the two files \
                 without an entry in workflow_description_divergences.json"
            );
            for divergence in entries() {
                assert_eq!(
                    shipped.matches(divergence.text.as_str()).count(),
                    1,
                    "divergence `{}` is registered but does not appear exactly once in the \
                     shipped {name}",
                    divergence.id
                );
                assert!(
                    !oracle.contains(divergence.text.as_str()),
                    "divergence `{}` is already in the oracle — retire the entry instead of \
                     inserting a duplicate",
                    divergence.id
                );
            }
        }
        // Each shipped text is still the oracle at both ends: every registered
        // insert is interior, so a divergence cannot silently re-open either
        // document or change how it closes.
        assert!(DESCRIPTION.starts_with(
            "Execute a workflow script that orchestrates multiple subagents deterministically."
        ));
        assert!(DESCRIPTION.ends_with("No wasted wall-clock."));
        assert!(AUTHORING_SKILL.starts_with("# Workflow authoring reference"));
        assert!(AUTHORING_SKILL.ends_with("hand-author a continuation script."));
    }

    /// Serialises the tests that drive the process-global reachability flags.
    ///
    /// `prompt()` reads them live, so a test that flips them races any other
    /// test asserting what `prompt()` returns — the classic "green alone, red in
    /// the full suite" flake, which also reads as another session's fault.
    static GATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Put the reachability flags in a known state and report the previous one.
    fn set_gate(registered: bool, skill_tool: bool) {
        platform_api::session_flags::set_workflow_authoring_skill_registered(registered);
        platform_api::session_flags::set_skill_tool_advertised(skill_tool);
    }

    /// Both halves of `nre` must hold. Either one false inlines the reference,
    /// because a skill that is registered but unloadable — or loadable but
    /// absent — leaves every documented hook unreachable.
    #[tokio::test]
    async fn the_description_points_at_the_skill_only_when_both_halves_hold() {
        let _guard = GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let opts = PromptOptions::default();
        let tool = WorkflowTool::new(None).with_size_guideline(WorkflowSizeGuideline::Unrestricted);

        set_gate(true, true);
        let pointed = tool.prompt(&opts).await;
        assert!(
            pointed.contains("`workflow-authoring`")
                && !pointed.contains("- fusion(prompt: string"),
            "with the skill registered and the Skill tool advertised, the description must point"
        );

        for (registered, skill_tool, why) in [
            (false, true, "the skill is not registered"),
            (
                true,
                false,
                "this request does not advertise the Skill tool",
            ),
            (false, false, "neither holds"),
        ] {
            set_gate(registered, skill_tool);
            let inlined = tool.prompt(&opts).await;
            assert!(
                inlined.contains("- fusion(prompt: string"),
                "the reference must be inlined when {why}: otherwise the description points at \
                 documentation the model cannot load, and every hook it lists is unreachable"
            );
            assert!(inlined.len() > pointed.len() * 4);
        }
        set_gate(false, false);
    }

    /// The reachability invariant the split must not break.
    ///
    /// A script only ever learns which hooks exist from the text the model
    /// reads. Moving 17 KB of that text behind a skill is safe ONLY while the
    /// fallback inlines it whenever the skill cannot be loaded — which is why
    /// upstream's `Epn` keeps both branches. This asserts the property directly:
    /// in the branch the model gets when it cannot load the skill, every hook is
    /// still documented.
    #[test]
    fn the_inline_branch_documents_every_script_body_hook() {
        let inline = assemble_description(false, false);
        for hook in [
            "- agent(prompt: string",
            "- pipeline(items,",
            "- parallel(thunks:",
            "- phase(",
            "- workflow(nameOrRef:",
            "- fusion(prompt: string",
        ] {
            assert!(
                inline.contains(hook),
                "{hook:?} is unreachable: a script cannot call a hook the description never \
                 mentions, and the inline branch is what a model without the skill receives"
            );
        }
        // The pointer branch is the OTHER half of the same invariant: it may
        // omit the hooks only because it names the skill that documents them.
        let pointed = assemble_description(true, false);
        assert!(
            pointed.contains("`workflow-authoring`"),
            "the short branch must name the skill that carries the reference"
        );
        assert!(
            !pointed.contains("- fusion(prompt: string"),
            "the short branch is pointless if it still inlines the reference"
        );
        assert!(
            pointed.len() < inline.len() / 4,
            "the split exists to cut the per-request footprint; got {} vs {}",
            pointed.len(),
            inline.len()
        );
    }

    /// The three `${e?"":"…"}` fragments are subtracted only when a subagent
    /// model is pinned — and each must still be FOUND, or the subtraction
    /// silently removes nothing.
    #[test]
    fn a_pinned_subagent_model_drops_the_model_override_passages() {
        for fragment in MODEL_FORCE_OMISSIONS {
            assert_eq!(
                ORACLE_AUTHORING_SKILL.matches(fragment).count(),
                1,
                "the model-force fragment {fragment:?} must occur exactly once in the oracle; a \
                 fragment that stopped matching would subtract nothing and pass unnoticed"
            );
        }
        let unforced = authoring_skill_body(false);
        let forced = authoring_skill_body(true);
        assert!(forced.len() < unforced.len(), "forcing must remove text");
        for fragment in MODEL_FORCE_OMISSIONS {
            assert!(unforced.contains(fragment));
            assert!(
                !forced.contains(fragment),
                "{fragment:?} documents a `model` override that a pinned deployment ignores"
            );
        }
        // Only those three passages go; the rest of the reference is intact.
        assert_eq!(
            unforced.len() - forced.len(),
            MODEL_FORCE_OMISSIONS.iter().map(|f| f.len()).sum::<usize>()
        );
        assert!(forced.starts_with("# Workflow authoring reference"));
        assert!(forced.contains("- fusion(prompt: string"));
    }

    /// The point of the 2.1.267 split, stated as a number so a regression that
    /// re-inlines the reference into every request is loud.
    #[test]
    fn the_per_request_description_stays_small() {
        assert!(
            DESCRIPTION.len() < 5_000,
            "the per-request description is {} bytes; 2.1.267 split the 19,200-byte 2.1.245 \
             text precisely so every request stops paying for the reference",
            DESCRIPTION.len()
        );
        assert!(
            AUTHORING_SKILL.len() > 15_000,
            "the reference did not survive the split"
        );
    }

    /// The composition must be able to go RED, and to say WHICH entry failed.
    /// Without this the register is a mechanism nobody has watched fail.
    #[test]
    fn a_divergence_whose_anchor_drifted_fails_by_name() {
        let drifted = vec![DescriptionDivergence {
            id: "planted-anchor-drift".into(),
            target: DivergenceTarget::Description,
            finding: "self-test".into(),
            reason: "planted by the test to prove composition can fail loudly".into(),
            anchor: "this sentence is not in the Workflow tool description".into(),
            text: "unreachable".into(),
        }];
        let error =
            compose_description(&ORACLE_DESCRIPTION, &drifted, DivergenceTarget::Description)
                .expect_err("a missing anchor must not compose silently");
        assert!(
            error.contains("planted-anchor-drift"),
            "the failure must NAME the divergence; got: {error}"
        );

        let ambiguous = vec![DescriptionDivergence {
            id: "planted-ambiguous-anchor".into(),
            target: DivergenceTarget::Description,
            finding: "self-test".into(),
            reason: "planted by the test to prove an ambiguous anchor is refused".into(),
            anchor: "the".into(),
            text: "unreachable".into(),
        }];
        let error = compose_description(
            &ORACLE_DESCRIPTION,
            &ambiguous,
            DivergenceTarget::Description,
        )
        .expect_err("an anchor with many matches must not compose");
        assert!(
            error.contains("planted-ambiguous-anchor") && error.contains("ambiguous"),
            "the failure must name the divergence and say why; got: {error}"
        );
    }

    /// The divergence this register was built for: without it the tool
    /// description forbids the local-app create flow's only hand-off.
    #[test]
    fn the_shipped_description_permits_the_local_app_create_handoff() {
        assert!(
            !ORACLE_DESCRIPTION.contains("lingxi-local-app:create-local-app"),
            "the oracle must stay free of product-specific carve-outs; that is what the \
             register is for"
        );
        assert!(
            DESCRIPTION.contains("lingxi-local-app:create-local-app"),
            "the create flow has the MODEL self-invoke the skill, which none of the oracle's \
             five opt-in clauses covers, so the shipped description must add the sixth"
        );
        // It must land inside the opt-in list, not after the closing "do NOT
        // call this tool" sentence, or the model reads it as unrelated prose.
        let opt_in = DESCRIPTION
            .find("Explicit opt-in means one of:")
            .expect("the opt-in list must exist");
        let closing = DESCRIPTION
            .find("For any other task")
            .expect("the closing prohibition must exist");
        let clause = DESCRIPTION
            .find("lingxi-local-app:create-local-app")
            .expect("checked above");
        assert!(
            opt_in < clause && clause < closing,
            "the added clause must sit inside the opt-in list ({opt_in}..{closing}), got {clause}"
        );
    }

    /// The second registered divergence: the `fusion()` script hook. Agent
    /// Fusion has no claude-code counterpart, so the oracle cannot document a
    /// hook the shipped runtime provides — a script that never learns `fusion()`
    /// exists cannot call it. Asserted on both sides so the bullet can neither
    /// vanish from the shipped text nor creep into the oracle.
    #[test]
    fn the_shipped_authoring_reference_documents_the_fusion_hook() {
        assert!(
            !ORACLE_AUTHORING_SKILL.contains("fusion(prompt: string, opts?:"),
            "the oracle must stay free of LingXi-only hooks; that is what the register is for"
        );
        assert!(
            AUTHORING_SKILL.contains("fusion(prompt: string, opts?:"),
            "the shipped description must document the fusion() script hook"
        );
        assert!(
            AUTHORING_SKILL.contains("WorkflowFusionOptionError"),
            "the catchable rejection names must be documented, or a script cannot handle them"
        );
        // `platform_api::normalize_dimensions`' real rules. Pinned as prose, not
        // just as length: an undocumented `dimensions: ['Coverage']` is rejected,
        // and until it was written down the rejection did not even reach the
        // script as a named error.
        assert!(
            AUTHORING_SKILL
                .contains("lowercase snake_case, at most 12, never a provider/model/panel name"),
            "the dimensions rule must stay documented on the option it constrains"
        );
        // It belongs in the script-body hook list, next to the other hooks a
        // script can call — not in the prose after it.
        let hooks = AUTHORING_SKILL
            .find("Script body hooks:")
            .expect("the hook list must exist");
        let bullet = AUTHORING_SKILL
            .find("- fusion(prompt: string")
            .expect("checked above");
        let after_hooks = AUTHORING_SKILL
            .find("Subagents are told their final text IS the return value")
            .expect("the paragraph after the hook list must exist");
        assert!(
            hooks < bullet && bullet < after_hooks,
            "the fusion() bullet must sit inside the hook list ({hooks}..{after_hooks}), got {bullet}"
        );
    }

    /// Drift gate for the `fusion()` bullet's documented resolved-object shape.
    /// Round 1 shipped `{runId, status, decision, panels, usage, timing, egress}`
    /// in the description while `platform_api::FusionResult` actually
    /// serializes `run_id` / `egress_profiles` (no `#[serde(rename_all)]`) and
    /// carries `final_text` — the ONLY field with the deliberation's answer —
    /// under no documented key at all. Nothing caught it: the byte-lock test
    /// above only pins length + a handful of substrings, and the round-trip
    /// test in `tasks` pins the real shape without ever comparing it back to
    /// this description text. Parse the object literal out of the bullet and
    /// assert every key it lists is an actual top-level key of a serialized
    /// `FusionResult`, so the two can never independently drift again.
    #[test]
    fn fusion_bullet_documents_only_real_fusion_result_keys() {
        let marker = "compact result object ({";
        let start = AUTHORING_SKILL
            .find(marker)
            .expect("fusion() bullet documents the resolved object shape")
            + marker.len()
            - 1; // keep the leading '{'
        let rest = &AUTHORING_SKILL[start..];
        let end = rest
            .find('}')
            .expect("object literal in the fusion() bullet is closed");
        let object_literal = &rest[1..end]; // strip the leading '{'
        let documented_keys: Vec<&str> = object_literal
            .split(',')
            .map(|part| part.trim().split(':').next().unwrap().trim())
            .collect();
        assert!(
            documented_keys.contains(&"run_id") && documented_keys.contains(&"final_text"),
            "sanity: expected run_id and final_text among parsed keys, got {documented_keys:?}"
        );

        let sample = platform_api::FusionResult {
            schema_version: 1,
            run_id: "fu_test".into(),
            status: platform_api::FusionStatus::Completed,
            decision: platform_api::FusionDecision::Picked {
                panel_id: "P1".into(),
            },
            final_text: "the answer".into(),
            analysis: None,
            panels: vec![platform_api::PanelOutcome {
                panel_id: "P1".into(),
                status: platform_api::PanelRunStatus::Completed,
                duration_ms: 7,
                error_category: None,
                error_detail: None,
                usage: None,
            }],
            usage: platform_api::FusionUsage::default(),
            timing: platform_api::FusionTiming::default(),
            egress_profiles: vec!["anthropic".into()],
        };
        let serialized = serde_json::to_value(&sample).expect("FusionResult serializes");
        let actual_keys = serialized.as_object().expect("object");
        for key in &documented_keys {
            assert!(
                actual_keys.contains_key(*key),
                "fusion() bullet documents key `{key}` but FusionResult never serializes it \
                 (actual keys: {:?}) — the description and the wire shape have drifted",
                actual_keys.keys().collect::<Vec<_>>()
            );
        }
    }

    /// Managed `disableWorkflows: true` must disable the tool. Before this was
    /// wired there was no seam for the setting at all, so an organization that
    /// set it still had Workflow advertised AND executable — the policy was
    /// parsed by nothing.
    #[test]
    fn managed_disable_workflows_disables_the_tool() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");
        let ctx = ToolStaticContext::default();
        assert!(
            WorkflowTool::new(None).is_enabled(&ctx),
            "enabled by default"
        );
        assert!(
            !WorkflowTool::new(None)
                .with_disable_workflows(true)
                .is_enabled(&ctx),
            "managed disableWorkflows must disable it"
        );
    }

    #[tokio::test]
    async fn managed_disable_workflows_also_rejects_at_validate() {
        // `is_enabled` hides the tool; a caller that invokes it anyway must
        // still be refused, with the byte-exact message.
        let t = WorkflowTool::new(None).with_disable_workflows(true);
        let ctx = tool_api::test_support::fresh_ctx();
        // The env arm must be OFF so this proves the MANAGED arm fired.
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");
        let err = t
            .validate_input(&serde_json::json!({"script": "x"}), &ctx)
            .await
            .expect_err("must reject");
        assert_eq!(
            err.0,
            "Dynamic workflows are disabled by managed settings (`disableWorkflows`)."
        );
    }

    #[test]
    fn session_gate_disables_the_tool() {
        let ctx = ToolStaticContext::default();
        assert!(
            !WorkflowTool::new(None)
                .with_session_enabled(false)
                .is_enabled(&ctx),
            "session gate must hide the tool"
        );
    }

    #[tokio::test]
    async fn session_gate_rejects_at_validate_with_exact_message() {
        let t = WorkflowTool::new(None).with_session_enabled(false);
        let ctx = tool_api::test_support::fresh_ctx();
        let err = t
            .validate_input(&serde_json::json!({"script": "x"}), &ctx)
            .await
            .expect_err("must reject");
        assert_eq!(
            err.0,
            "Dynamic workflows are not enabled for this session (org policy, launch gate, or the \"Dynamic workflows\" setting in /config)."
        );
    }

    #[test]
    fn session_owned_gate_is_live_and_isolated() {
        let ctx = ToolStaticContext::default();
        let first_gate = platform_api::session_flags::DynamicWorkflowsGate::new(true, false);
        let second_gate = platform_api::session_flags::DynamicWorkflowsGate::new(false, true);
        let first = WorkflowTool::new(None).with_dynamic_workflows_gate(first_gate.clone());
        let second = WorkflowTool::new(None).with_dynamic_workflows_gate(second_gate.clone());

        assert!(first.is_enabled(&ctx));
        assert!(!second.is_enabled(&ctx));

        first_gate.set_enabled(false);
        second_gate.set_enabled(true);

        assert!(!first.is_enabled(&ctx));
        assert!(second.is_enabled(&ctx));
        assert!(second_gate.managed());
    }

    #[tokio::test]
    async fn session_owned_gate_rejects_after_live_toggle() {
        let gate = platform_api::session_flags::DynamicWorkflowsGate::new(true, false);
        let tool = WorkflowTool::new(None).with_dynamic_workflows_gate(gate.clone());
        let ctx = tool_api::test_support::fresh_ctx();

        tool.validate_input(&serde_json::json!({"script": "x"}), &ctx)
            .await
            .expect_err("script must still fail later checks while gate is on");

        gate.set_enabled(false);
        let err = tool
            .validate_input(&serde_json::json!({"script": "x"}), &ctx)
            .await
            .expect_err("must reject once the live gate turns off");
        assert_eq!(
            err.0,
            "Dynamic workflows are not enabled for this session (org policy, launch gate, or the \"Dynamic workflows\" setting in /config)."
        );
    }

    #[tokio::test]
    async fn managed_disable_precedes_session_gate() {
        let t = WorkflowTool::new(None)
            .with_disable_workflows(true)
            .with_session_enabled(false);
        let ctx = tool_api::test_support::fresh_ctx();
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");
        let err = t
            .validate_input(&serde_json::json!({"script": "x"}), &ctx)
            .await
            .expect_err("must reject");
        assert_eq!(
            err.0,
            "Dynamic workflows are disabled by managed settings (`disableWorkflows`)."
        );
    }

    #[test]
    fn the_default_size_guideline_is_medium_not_unrestricted() {
        assert_eq!(
            WorkflowSizeGuideline::default(),
            WorkflowSizeGuideline::Medium
        );
    }

    #[tokio::test]
    async fn prompt_appends_size_guideline_when_configured() {
        let _guard = GATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_gate(false, false);
        let opts = PromptOptions::default();
        // The base is the ASSEMBLED description, not the bare oracle: 2.1.267
        // appends either the authoring reference or the one-line pointer to it.
        // Computed the same way `prompt()` does, so this cannot drift from it.
        let base = assemble_description(false, subagent_model_forced());
        // The DEFAULT is `medium` (oracle `_Td`), not unrestricted, so an
        // unconfigured tool already carries the medium appendix. Shipping
        // `Unrestricted` by default meant shipping NO agent cap where the
        // oracle advises under 15.
        assert_eq!(
            tool(None).prompt(&opts).await,
            format!(
                "{}{}",
                base,
                WorkflowSizeGuideline::Medium.prompt_appendix()
            )
        );
        // Each configured size appends its byte-exact `VAs` appendix
        // (`qAs + VAs(size)`), with NO separator between description and appendix
        // (the appendix carries its own leading newline).
        for size in [
            WorkflowSizeGuideline::Small,
            WorkflowSizeGuideline::Medium,
            WorkflowSizeGuideline::Large,
        ] {
            let t = WorkflowTool::new(None).with_size_guideline(size);
            let want = format!("{}{}", base, size.prompt_appendix());
            assert_eq!(t.prompt(&opts).await, want, "size={:?}", size);
            // Sanity: the model-visible cap text is present.
            assert!(t
                .prompt(&opts)
                .await
                .contains("The user has configured a workflow size guideline in /config:"));
        }
        // Explicit unrestricted also yields no appendix.
        let u = WorkflowTool::new(None).with_size_guideline(WorkflowSizeGuideline::Unrestricted);
        assert_eq!(u.prompt(&opts).await, base);

        // `/config` updates the session snapshot after construction; the next
        // prompt must reflect it without rebuilding the tool registry.
        let live = WorkflowTool::new(None).with_size_guideline(WorkflowSizeGuideline::Medium);
        let _ = platform_api::session_flags::set_workflow_size_guideline("small", false);
        assert_eq!(
            live.prompt(&opts).await,
            format!("{}{}", base, WorkflowSizeGuideline::Small.prompt_appendix())
        );
        let _ = platform_api::session_flags::set_workflow_size_guideline("medium", false);

        let session_state =
            platform_api::session_flags::WorkflowSizeGuidelineState::new("large", false, false)
                .unwrap();
        let session_owned = WorkflowTool::new(None)
            .with_size_guideline_state(session_state.clone())
            .with_size_guideline_source(WorkflowSizeGuideline::Large, false, false);
        let _ = platform_api::session_flags::set_workflow_size_guideline("small", false);
        assert_eq!(
            session_owned.prompt(&opts).await,
            format!("{}{}", base, WorkflowSizeGuideline::Large.prompt_appendix()),
            "session-owned state must win over the process-global compatibility snapshot"
        );
        let _ = session_state.set("medium", false);
        let _ = platform_api::session_flags::set_workflow_size_guideline("medium", false);
    }

    #[test]
    fn input_schema_is_byte_exact() {
        let s = &*INPUT_SCHEMA;
        assert_eq!(s["type"], "object");
        assert_eq!(s["additionalProperties"], false);
        assert!(s.get("required").is_none(), "no required keys");
        // Source-order properties (the zod definition order).
        let props = s["properties"].as_object().unwrap();
        let order: Vec<&String> = props.keys().collect();
        assert_eq!(
            order,
            vec![
                "script",
                "name",
                "description",
                "title",
                "args",
                "scriptPath",
                "resumeFromRunId"
            ]
        );
        assert_eq!(props["script"]["maxLength"], 524288);
        assert_eq!(props["resumeFromRunId"]["pattern"], "^wf_[a-z0-9-]{6,}$");
        // E.unknown() → `args` has no `type` constraint.
        assert!(props["args"].get("type").is_none());
        assert!(props["script"]["description"]
            .as_str()
            .unwrap()
            .starts_with("Self-contained workflow script."));
    }

    // A minimal valid workflow script: has a proper meta block, is deterministic.
    const VALID_SCRIPT: &str = concat!(
        "export const meta = { name: 'test', description: 'A test workflow' };\n",
        "await agent('do something');\n",
    );

    #[tokio::test]
    async fn validate_error_1a_must_provide_one_of_three_fields() {
        // errorCode 1a — none of script/name/scriptPath provided.
        let t = tool(None);
        let ctx = tool_api::test_support::fresh_ctx();
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");

        let err = t.validate_input(&json!({}), &ctx).await.unwrap_err();
        assert_eq!(err.0, "Must provide script, name, scriptPath, or runId");

        let err2 = t
            .validate_input(&json!({ "title": "x" }), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err2.0, "Must provide script, name, scriptPath, or runId");
    }

    #[tokio::test]
    async fn relative_script_path_uses_the_live_session_cwd() {
        let root = unique_temp_path("live-cwd");
        let moved = unique_temp_path("live-cwd-moved");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&moved).unwrap();
        std::fs::write(root.join("workflow.js"), VALID_SCRIPT).unwrap();
        let cwd = Arc::new(std::sync::Mutex::new(root.clone()));
        let t = tool(None).with_current_cwd(cwd.clone());
        let ctx = tool_api::test_support::fresh_ctx();
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");

        t.validate_input(&json!({ "scriptPath": "workflow.js" }), &ctx)
            .await
            .expect("relative scriptPath syntax is valid from the live cwd");
        *cwd.lock().unwrap() = moved.clone();
        t.validate_input(&json!({ "scriptPath": "workflow.js" }), &ctx)
            .await
            .expect("validation must not read the moved-away scriptPath");

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(moved);
    }

    #[tokio::test]
    async fn script_path_validation_is_io_free_and_permission_uses_live_read_shape() {
        let cwd = unique_temp_path("script-path-permission");
        std::fs::create_dir_all(&cwd).unwrap();
        // A directory at the script path makes an eager std::fs::read fail,
        // proving validate_input does not inspect the path before permission.
        let script_path = cwd.join("script.js");
        std::fs::create_dir(&script_path).unwrap();
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let gate = Arc::new(RecordingPermissionGate {
            calls: calls.clone(),
            decision: platform_api::permission_gate::PermissionDecision::Allow,
        });
        let tool = tool(None)
            .with_current_cwd(Arc::new(std::sync::Mutex::new(cwd.clone())))
            .with_permission_gate(gate);
        let ctx = tool_api::test_support::fresh_ctx();

        tool.validate_input(&json!({ "scriptPath": "script.js" }), &ctx)
            .await
            .expect("path syntax validation must not read scriptPath");
        assert!(matches!(
            tool.check_permissions(&json!({ "scriptPath": "script.js" }), &ctx)
                .await,
            PermissionResult::Allow { .. }
        ));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[(
                "Read".to_string(),
                json!({ "file_path": cwd.join("script.js").to_string_lossy() })
            )]
        );

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn script_path_permission_propagates_tool_context_and_fails_closed_headless() {
        let cwd = unique_temp_path("script-path-headless");
        std::fs::create_dir_all(&cwd).unwrap();
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let contexts = Arc::new(StdMutex::new(Vec::new()));
        let gate = Arc::new(ContextAwarePermissionGate {
            calls: calls.clone(),
            contexts: contexts.clone(),
        });
        let tool = tool(None)
            .with_current_cwd(Arc::new(std::sync::Mutex::new(cwd.clone())))
            .with_permission_gate(gate);
        let mut interactive_ctx = tool_api::test_support::fresh_ctx();
        interactive_ctx.tool_use_id = Some("toolu_workflow_interactive".into());

        let interactive = tool
            .check_permissions(&json!({ "scriptPath": "script.js" }), &interactive_ctx)
            .await;
        assert!(matches!(interactive, PermissionResult::Ask { .. }));

        let mut headless_ctx = tool_api::test_support::fresh_ctx();
        headless_ctx.tool_use_id = Some("toolu_workflow_headless".into());
        headless_ctx.options.is_non_interactive_session = true;
        let headless = tool
            .check_permissions(&json!({ "scriptPath": "script.js" }), &headless_ctx)
            .await;
        match headless {
            PermissionResult::Deny {
                explanation: Some(reason),
                ..
            } => {
                assert_eq!(
                    reason,
                    permission::headless_gate::headless_deny_message("Read")
                );
            }
            other => panic!("expected headless deny, got {other:?}"),
        }

        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[
                (
                    "Read".to_string(),
                    json!({ "file_path": cwd.join("script.js").to_string_lossy() })
                ),
                (
                    "Read".to_string(),
                    json!({ "file_path": cwd.join("script.js").to_string_lossy() })
                )
            ]
        );
        let recorded = contexts.lock().unwrap();
        assert_eq!(recorded.len(), 2);
        assert_eq!(
            recorded[0].tool_use_id.as_deref(),
            Some("toolu_workflow_interactive")
        );
        assert!(!recorded[0].is_non_interactive_session);
        assert_eq!(
            recorded[1].tool_use_id.as_deref(),
            Some("toolu_workflow_headless")
        );
        assert!(recorded[1].is_non_interactive_session);

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn script_path_deny_happens_without_script_content_in_validation_or_result() {
        let cwd = unique_temp_path("script-path-deny");
        std::fs::create_dir_all(&cwd).unwrap();
        let script_path = cwd.join("secret.js");
        std::fs::create_dir(&script_path).unwrap();
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let gate = Arc::new(RecordingPermissionGate {
            calls,
            decision: platform_api::permission_gate::PermissionDecision::Deny {
                reason: "Read denied by policy".into(),
            },
        });
        let tool = tool(None)
            .with_current_cwd(Arc::new(std::sync::Mutex::new(cwd.clone())))
            .with_permission_gate(gate);
        let ctx = tool_api::test_support::fresh_ctx();
        tool.validate_input(&json!({ "scriptPath": "secret.js" }), &ctx)
            .await
            .expect("validation must not read a denied scriptPath");

        let result = tool
            .check_permissions(&json!({ "scriptPath": "secret.js" }), &ctx)
            .await;
        let rendered = format!("{result:?}");
        assert!(matches!(result, PermissionResult::Deny { .. }));
        assert!(!rendered.contains("secret workflow contents"));
        assert!(rendered.contains("Read denied by policy"));

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn script_path_symlink_stays_lexical_for_permission_semantics() {
        use std::os::unix::fs::symlink;

        let cwd = unique_temp_path("script-path-symlink");
        std::fs::create_dir_all(&cwd).unwrap();
        let target = cwd.join("target.js");
        let link = cwd.join("link.js");
        std::fs::write(&target, "secret workflow contents").unwrap();
        symlink(&target, &link).unwrap();
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let gate = Arc::new(RecordingPermissionGate {
            calls: calls.clone(),
            decision: platform_api::permission_gate::PermissionDecision::Allow,
        });
        let tool = tool(None)
            .with_current_cwd(Arc::new(std::sync::Mutex::new(cwd.clone())))
            .with_permission_gate(gate);
        let ctx = tool_api::test_support::fresh_ctx();

        tool.validate_input(&json!({ "scriptPath": "link.js" }), &ctx)
            .await
            .expect("symlink path validation must not follow the link");
        assert!(matches!(
            tool.check_permissions(&json!({ "scriptPath": "link.js" }), &ctx)
                .await,
            PermissionResult::Allow { .. }
        ));
        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "Read");
        assert_eq!(
            recorded[0].1["file_path"],
            cwd.join("link.js").to_string_lossy().as_ref()
        );
        assert_ne!(
            recorded[0].1["file_path"],
            target.to_string_lossy().as_ref(),
            "Workflow must reuse the existing permission path semantics"
        );

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn script_path_without_permission_wiring_fails_closed_with_an_ask() {
        let cwd = unique_temp_path("script-path-no-gate");
        std::fs::create_dir_all(&cwd).unwrap();
        let tool = tool(None).with_current_cwd(Arc::new(std::sync::Mutex::new(cwd.clone())));
        let ctx = tool_api::test_support::fresh_ctx();

        let result = tool
            .check_permissions(&json!({ "scriptPath": "script.js" }), &ctx)
            .await;
        assert!(matches!(result, PermissionResult::Ask { .. }));
        assert!(!format!("{result:?}").contains("secret workflow contents"));

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn script_path_leaf_symlink_retarget_after_read_approval_is_rejected() {
        use std::os::unix::fs::symlink;

        let cwd = unique_temp_path("script-path-leaf-race");
        std::fs::create_dir_all(&cwd).unwrap();
        let allowed = cwd.join("allowed.js");
        let denied = cwd.join("denied.js");
        let link = cwd.join("script.js");
        std::fs::write(&allowed, "allowed workflow").unwrap();
        std::fs::write(&denied, "denied workflow").unwrap();
        symlink(&allowed, &link).unwrap();
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let gate = Arc::new(RecordingPermissionGate {
            calls,
            decision: platform_api::permission_gate::PermissionDecision::Allow,
        });
        let tool = tool(None)
            .with_current_cwd(Arc::new(std::sync::Mutex::new(cwd.clone())))
            .with_permission_gate(gate);
        let ctx = tool_api::test_support::fresh_ctx();
        let result = tool
            .check_permissions(&json!({ "scriptPath": "script.js" }), &ctx)
            .await;
        assert!(matches!(result, PermissionResult::Allow { .. }));
        let approval = tool
            .take_script_path_approval(&ctx, &link)
            .expect("allow must capture a rooted approval snapshot");

        let replacement = cwd.join("script.replacement.js");
        symlink(&denied, &replacement).unwrap();
        std::fs::rename(&replacement, &link).unwrap();
        let error = read_script_path_after_permission(&approval)
            .expect_err("retargeted leaf must never be read");
        assert!(error.0.contains("after permission"), "{error:?}");

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn script_path_parent_symlink_retarget_after_read_approval_is_rejected() {
        use std::os::unix::fs::symlink;

        let cwd = unique_temp_path("script-path-parent-race");
        let allowed_dir = cwd.join("allowed");
        let denied_dir = cwd.join("denied");
        std::fs::create_dir_all(&allowed_dir).unwrap();
        std::fs::create_dir_all(&denied_dir).unwrap();
        let allowed = allowed_dir.join("script.js");
        let denied = denied_dir.join("script.js");
        let link = cwd.join("scripts");
        std::fs::write(&allowed, "allowed workflow").unwrap();
        std::fs::write(&denied, "denied workflow").unwrap();
        symlink(&allowed_dir, &link).unwrap();
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let gate = Arc::new(RecordingPermissionGate {
            calls,
            decision: platform_api::permission_gate::PermissionDecision::Allow,
        });
        let tool = tool(None)
            .with_current_cwd(Arc::new(std::sync::Mutex::new(cwd.clone())))
            .with_permission_gate(gate);
        let ctx = tool_api::test_support::fresh_ctx();
        let result = tool
            .check_permissions(&json!({ "scriptPath": "scripts/script.js" }), &ctx)
            .await;
        assert!(matches!(result, PermissionResult::Allow { .. }));
        let requested = link.join("script.js");
        let approval = tool
            .take_script_path_approval(&ctx, &requested)
            .expect("allow must capture a rooted approval snapshot");

        let replacement = cwd.join("scripts.replacement");
        symlink(&denied_dir, &replacement).unwrap();
        std::fs::rename(&replacement, &link).unwrap();
        let error = read_script_path_after_permission(&approval)
            .expect_err("retargeted parent must never be read");
        assert!(error.0.contains("after permission"), "{error:?}");

        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn validate_error_2_invalid_workflow_script() {
        // errorCode 2 — script is present but fails meta parse.
        let t = tool(None);
        let ctx = tool_api::test_support::fresh_ctx();
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");

        // A script with no meta block at all triggers the "must be first statement" error.
        let err = t
            .validate_input(&json!({ "script": "console.log('hello');" }), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.0.starts_with("Invalid workflow script:"),
            "expected errorCode 2 message, got: {:?}",
            err.0
        );
    }

    #[tokio::test]
    async fn validate_rejects_body_syntax_before_a_task_can_launch() {
        let t = tool(None);
        let ctx = tool_api::test_support::fresh_ctx();
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");

        let script = concat!(
            "export const meta = { name: 'bad-body', description: 'syntax error' };\n",
            "if (\n",
        );
        t.validate_input(&json!({ "script": script }), &ctx)
            .await
            .expect("body compilation belongs to the launcher, not validate_input");
    }

    #[tokio::test]
    async fn call_surfaces_launcher_compile_error_without_registering_a_task() {
        let launcher = Arc::new(RichMockLauncher {
            launched: WorkflowLaunched {
                task_id: "w_compile_error".into(),
                run_id: Some("wf_compile-error".into()),
                workflow_name: Some("bad-body".into()),
                summary: Some("syntax error".into()),
                error: Some("Unexpected end of input".into()),
                ..Default::default()
            },
        });
        let t = tool(Some(launcher));
        let res = t
            .call(
                json!({ "script": "export const meta = { name: 'bad-body', description: 'syntax error' }; if (" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect("compile failures return a structured tool-error result");

        assert!(res.data.get("status").is_none());
        assert_eq!(res.data["error"], "Unexpected end of input");
        assert!(res.is_error);
        assert!(res.data.get("taskId").is_none());
        assert!(res.data.get("taskType").is_none());
        assert!(res.data.get("runId").is_none());
        assert!(res.data.get("workflowName").is_none());
        assert!(res.data.get("summary").is_none());
        assert!(res.data.get("transcriptDir").is_none());
        assert_eq!(
            res.data["model_content"],
            "Workflow script has a syntax error and was not launched:\nUnexpected end of input"
        );
    }

    #[tokio::test]
    async fn validate_error_4_date_now_determinism() {
        // errorCode 4 — inline script uses Date.now().
        let t = tool(None);
        let ctx = tool_api::test_support::fresh_ctx();
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");

        let script = concat!(
            "export const meta = { name: 'bad', description: 'non-det' };\n",
            "const t = Date.now();\n",
        );
        let err = t
            .validate_input(&json!({ "script": script }), &ctx)
            .await
            .unwrap_err();
        assert_eq!(
            err.0,
            workflow::NON_DETERMINISTIC_MESSAGE,
            "errorCode 4 message must be byte-exact"
        );
    }

    #[tokio::test]
    async fn validate_name_resolved_date_now_passes_determinism_gate() {
        // Binary parity: errorCode-4 is NOT triggered for `name`-resolved saved
        // workflows even when the resolved body contains Date.now(). `e.script`
        // (the raw inline field) is falsy, so the binary skips the determinism
        // check. LingXi must do the same.
        let t = tool(None);
        let ctx = tool_api::test_support::fresh_ctx();
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");

        // Write a non-deterministic saved workflow to .lingxi/workflows/.
        let dir = std::path::Path::new(".lingxi/workflows");
        std::fs::create_dir_all(dir).unwrap();
        let wf_path = dir.join("nondet-wf.js");
        let nondeterministic_src = concat!(
            "export const meta = { name: 'nondet-wf', description: 'non-det saved' };\n",
            "const t = Date.now();\n",
            "await agent('do something');\n",
        );
        std::fs::write(&wf_path, nondeterministic_src).unwrap();

        let result = t
            .validate_input(&json!({ "name": "nondet-wf" }), &ctx)
            .await;

        // Clean up before asserting so we don't leave stray files.
        let _ = std::fs::remove_file(&wf_path);

        result
            .expect("name-resolved workflow with Date.now() must NOT be rejected for determinism");
    }

    #[tokio::test]
    async fn saved_shadow_is_validated_before_builtin() {
        let t = tool(None);
        let ctx = tool_api::test_support::fresh_ctx();
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");

        let dir = std::path::Path::new(".lingxi/workflows");
        std::fs::create_dir_all(dir).unwrap();
        let shadow = dir.join("deep-research.js");
        let previous = std::fs::read(&shadow).ok();
        std::fs::write(&shadow, "not a valid workflow").unwrap();

        let result = t
            .validate_input(&json!({ "name": "deep-research" }), &ctx)
            .await;

        if let Some(previous) = previous {
            std::fs::write(&shadow, previous).unwrap();
        } else {
            let _ = std::fs::remove_file(&shadow);
        }
        result.expect("script content is validated by the post-authorization launcher");
    }

    #[tokio::test]
    async fn saved_plugin_builtin_precedence_is_identical_across_all_tool_paths() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");
        let cwd = unique_temp_path("three-tier-precedence");
        let saved_dir = cwd.join(branding::DOT_DIR).join("workflows");
        std::fs::create_dir_all(&saved_dir).unwrap();
        let saved_path = saved_dir.join("deep-research.js");
        std::fs::write(&saved_path, "invalid saved workflow").unwrap();
        let plugin_path = cwd.join("plugin.js");
        std::fs::write(&plugin_path, "invalid plugin workflow").unwrap();

        let registry = Arc::new(workflow::PluginWorkflowRegistry::new());
        registry.register(vec![workflow::PluginWorkflowEntry {
            name: "deep-research".to_string(),
            script_path: plugin_path,
        }]);
        let spec = WorkflowLaunchSpec {
            name: Some("deep-research".to_string()),
            ..Default::default()
        };
        let resolve = || {
            resolve_script_at(
                &cwd,
                &spec,
                |path| std::fs::read_to_string(path),
                Some(registry.as_ref()),
            )
        };
        let tool = WorkflowTool::new(None)
            .with_current_cwd(Arc::new(std::sync::Mutex::new(cwd.clone())))
            .with_plugin_workflows(registry.clone());
        let ctx = tool_api::test_support::fresh_ctx();

        assert_eq!(resolve().unwrap(), "invalid saved workflow");
        assert_eq!(
            workflow_source_for_name(&cwd, "deep-research", Some(registry.as_ref())),
            Some("projectSettings")
        );
        tool.validate_input(&json!({"name": "deep-research"}), &ctx)
            .await
            .expect("named workflow validation must not read the saved winner");

        std::fs::remove_file(&saved_path).unwrap();
        assert_eq!(resolve().unwrap(), "invalid plugin workflow");
        assert_eq!(
            workflow_source_for_name(&cwd, "deep-research", Some(registry.as_ref())),
            Some("plugin")
        );
        tool.validate_input(&json!({"name": "deep-research"}), &ctx)
            .await
            .expect("named workflow validation must not read the plugin winner");

        registry.unregister(&["deep-research".to_string()]);
        assert!(resolve().unwrap().contains("const VOTES_PER_CLAIM = 3"));
        assert_eq!(
            workflow_source_for_name(&cwd, "deep-research", Some(registry.as_ref())),
            Some("built-in")
        );
        tool.validate_input(&json!({"name": "deep-research"}), &ctx)
            .await
            .expect("builtin is the final fallback");
        let _ = std::fs::remove_dir_all(cwd);
    }

    #[tokio::test]
    async fn validate_name_resolves_user_workflow_dir() {
        let t = tool(None);
        let ctx = tool_api::test_support::fresh_ctx();
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");
        let config_dir = unique_temp_path("validate-user");
        let workflows_dir = config_dir.join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        let wf_path = workflows_dir.join("user-only-wf.js");
        std::fs::write(&wf_path, VALID_SCRIPT).unwrap();

        let old_config_dir = std::env::var_os(branding::CONFIG_DIR_ENV);
        std::env::set_var(branding::CONFIG_DIR_ENV, &config_dir);
        let result = t
            .validate_input(&json!({ "name": "user-only-wf" }), &ctx)
            .await;
        match old_config_dir {
            Some(v) => std::env::set_var(branding::CONFIG_DIR_ENV, v),
            None => std::env::remove_var(branding::CONFIG_DIR_ENV),
        }

        let _ = std::fs::remove_file(&wf_path);
        let _ = std::fs::remove_dir_all(&config_dir);
        result.expect("name-resolved workflow must fall back to user workflow dir");
    }

    /// §14 — a plugin's declared workflow, namespaced `{plugin}:{name}` by
    /// `plugin::PluginManager::load_plugin`, resolves through the SAME
    /// `name` input as a built-in/project/user workflow once the shared
    /// registry is wired via `with_plugin_workflows`.
    #[tokio::test]
    async fn validate_name_resolves_via_plugin_workflow_registry() {
        let dir = unique_temp_path("plugin-registry");
        std::fs::create_dir_all(&dir).unwrap();
        let script_path = dir.join("deploy.js");
        std::fs::write(&script_path, VALID_SCRIPT).unwrap();

        let registry = Arc::new(workflow::PluginWorkflowRegistry::new());
        registry.register(vec![workflow::PluginWorkflowEntry {
            name: "acme:deploy".to_string(),
            script_path: script_path.clone(),
        }]);

        let t = tool(None).with_plugin_workflows(registry);
        let ctx = tool_api::test_support::fresh_ctx();
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");
        let result = t
            .validate_input(&json!({ "name": "acme:deploy" }), &ctx)
            .await;

        let _ = std::fs::remove_file(&script_path);
        let _ = std::fs::remove_dir_all(&dir);
        result.expect("a plugin-registered workflow should resolve by its namespaced name");
    }

    /// A wired-but-empty registry does not change the ordinary "not found"
    /// behaviour — the plugin check is a pure addition to the search path.
    #[tokio::test]
    async fn validate_name_absent_from_plugin_registry_reports_not_found() {
        let t = tool(None).with_plugin_workflows(Arc::new(workflow::PluginWorkflowRegistry::new()));
        let ctx = tool_api::test_support::fresh_ctx();
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");
        let err = t
            .validate_input(&json!({ "name": "no-such-workflow" }), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.0
                .starts_with("Workflow \"no-such-workflow\" not found."),
            "unexpected error: {}",
            err.0
        );
    }

    /// §14 — the LAUNCH path. `Tool::validate_input` and `resolve_script_at`
    /// are two separate implementations of the same search order, and only
    /// the former is exercised by
    /// `validate_name_resolves_via_plugin_workflow_registry`; reverting the
    /// `resolve_script_at` branch alone leaves that test green. This one
    /// pins the launcher's half directly, so a partially-wired composition
    /// root (tool wired, launcher not) cannot ship as "validate accepts,
    /// launch says not found".
    ///
    /// It also pins the ORDER at this seam: project/user files above the
    /// registry, built-ins above everything (see
    /// `workflow::PluginWorkflowRegistry`'s module doc for why the
    /// built-in/plugin half is unobservable in this port).
    #[test]
    fn resolve_script_at_falls_back_to_the_plugin_workflow_registry() {
        let registry = workflow::PluginWorkflowRegistry::new();
        registry.register(vec![workflow::PluginWorkflowEntry {
            name: "acme:deploy".to_string(),
            script_path: std::path::PathBuf::from("/plugins/acme/scripts/deploy.js"),
        }]);
        let read = |p: &str| -> std::io::Result<String> {
            if p == "/plugins/acme/scripts/deploy.js" {
                Ok(VALID_SCRIPT.to_string())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no such file",
                ))
            }
        };
        let named = |name: &str| WorkflowLaunchSpec {
            name: Some(name.to_string()),
            ..Default::default()
        };

        // Wired registry, registered name → the plugin's script.
        let resolved = resolve_script_at(
            std::path::Path::new("/proj"),
            &named("acme:deploy"),
            read,
            Some(&registry),
        )
        .expect("a registered plugin workflow must resolve on the launch path");
        assert_eq!(resolved, VALID_SCRIPT);

        // The SAME name with NO registry is the pre-§14 behaviour: not found.
        // (This is exactly the drift a half-wired composition root produces.)
        let err = resolve_script_at(
            std::path::Path::new("/proj"),
            &named("acme:deploy"),
            read,
            None,
        )
        .expect_err("without the registry the launch path must not resolve it");
        assert_eq!(
            err.0, "Workflow \"acme:deploy\" not found. Available: (none)",
            "byte-exact launch-path miss message"
        );

        // A name absent from a wired registry still misses.
        let err = resolve_script_at(
            std::path::Path::new("/proj"),
            &named("acme:absent"),
            read,
            Some(&registry),
        )
        .expect_err("an unregistered name must still miss");
        assert!(err.0.starts_with("Workflow \"acme:absent\" not found."));

        // Built-ins are still resolved without touching the registry.
        let resolved = resolve_script_at(
            std::path::Path::new("/proj"),
            &named("deep-research"),
            read,
            Some(&registry),
        )
        .expect("built-ins resolve regardless of the registry");
        assert_eq!(
            resolved,
            BUILTIN_WORKFLOWS.get("deep-research").unwrap().script
        );
    }

    #[tokio::test]
    async fn validate_error_5_disable_workflows_env() {
        // errorCode 5 — LINGXI_DISABLE_WORKFLOWS=1 fires the managed-settings message.
        let t = tool(None);
        let ctx = tool_api::test_support::fresh_ctx();
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("LINGXI_DISABLE_WORKFLOWS", "1");
        let result = t
            .validate_input(&json!({ "script": VALID_SCRIPT }), &ctx)
            .await;
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");

        let err = result.unwrap_err();
        assert_eq!(
            err.0, "Dynamic workflows are disabled by managed settings (`disableWorkflows`).",
            "errorCode 5 message must be byte-exact"
        );
    }

    #[test]
    fn schema_refinement_retains_native_issue_shape() {
        let tool = tool(None);
        assert_eq!(
            tool.input_validation_issues(&json!({"script":"a\u{0}b"})),
            vec![
                json!({"code":"custom","path":["script"],"message":"script contains control characters that would be hidden in the approval dialog"})
            ]
        );
        assert!(tool
            .input_validation_issues(&json!({"script":"a\nb\tc"}))
            .is_empty());
        assert!(tool
            .input_validation_issues(&json!({"script":12}))
            .is_empty());
    }

    #[test]
    fn schema_refinements_include_selector_and_invisible_name_failures() {
        let tool = tool(None);
        assert_eq!(
            tool.input_validation_issues(&json!({})),
            vec![
                json!({"code":"custom","path":[],"message":"Must provide script, name, scriptPath, or runId"})
            ]
        );
        for key in ["name", "scriptPath"] {
            assert_eq!(
                tool.input_validation_issues(&json!({key:"bad\u{034f}name"})),
                vec![
                    json!({"code":"custom","path":[key],"message":"contains control or invisible format characters"})
                ]
            );
        }
    }

    #[tokio::test]
    async fn validate_script_with_control_char_rejected() {
        // Schema `.refine(UIe, hYp)` — a control char (here NUL) in the raw inline
        // `script` field is rejected byte-exact before any gate runs.
        let t = tool(None);
        let ctx = tool_api::test_support::fresh_ctx();
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");

        let with_ctrl = format!("{VALID_SCRIPT}\u{0}");
        let err = t
            .validate_input(&json!({ "script": with_ctrl }), &ctx)
            .await
            .unwrap_err();
        assert_eq!(
            err.0, "script contains control characters that would be hidden in the approval dialog",
            "control-char refine message must be byte-exact"
        );
    }

    #[tokio::test]
    async fn validate_script_with_tab_and_newline_allowed() {
        // WRa excludes char codes 9 (tab) and 10 (LF) — a script with those passes.
        let t = tool(None);
        let ctx = tool_api::test_support::fresh_ctx();
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");

        let with_ws = format!("{VALID_SCRIPT}\n\tlog('ok')\n");
        t.validate_input(&json!({ "script": with_ws }), &ctx)
            .await
            .expect("tab/newline must not trip the control-char refine");
    }

    #[tokio::test]
    async fn validate_valid_script_passes() {
        // A valid script with proper meta + deterministic code → Ok(()).
        let t = tool(None);
        let ctx = tool_api::test_support::fresh_ctx();
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");

        t.validate_input(&json!({ "script": VALID_SCRIPT }), &ctx)
            .await
            .expect("valid script must pass all gates");
    }

    #[tokio::test]
    async fn call_launches_and_returns_async_launched() {
        let launcher = MockLauncher::new("w_abc123");
        let t = tool(Some(launcher.clone()));
        let session_id = "11111111-2222-4333-8444-555555555555";
        let mut ctx = tool_api::test_support::fresh_ctx();
        ctx.session = Some(Arc::new(tokio::sync::Mutex::new(
            serde_json::from_value(json!({
                "session_id": session_id,
                "history": [],
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                },
                "model": "test-model"
            }))
            .expect("session fixture"),
        )));
        let res = t
            .call(
                json!({ "script": "return 1;", "args": ["a.ts", "b.ts"] }),
                ctx,
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect("call ok");
        assert_eq!(res.data["status"], "async_launched");
        assert_eq!(res.data["taskId"], "w_abc123");
        assert_eq!(res.data["taskType"], "local_workflow");
        // The launcher saw the parsed spec (script + args).
        let spec = launcher.seen.lock().unwrap().clone().expect("launched");
        assert_eq!(spec.script.as_deref(), Some("return 1;"));
        assert_eq!(spec.args, Some(json!(["a.ts", "b.ts"])));
        assert_eq!(spec.session_uuid, Some(session_id.to_string()));
    }

    #[tokio::test]
    async fn call_uses_trusted_origin_session_without_a_live_session_context() {
        let launcher = MockLauncher::new("w_origin");
        let t = tool(Some(launcher.clone()));
        let origin_id = "22222222-3333-4444-8555-666666666666";
        let mut ctx = tool_api::test_support::fresh_ctx();
        ctx.origin_session_id =
            Some(serde_json::from_value(json!(origin_id)).expect("origin session fixture"));

        t.call(
            json!({ "script": "return 1;" }),
            ctx,
            tool_api::test_support::fresh_tx(),
        )
        .await
        .expect("call ok");

        let spec = launcher.seen.lock().unwrap().clone().expect("launched");
        assert_eq!(spec.session_uuid, Some(origin_id.to_string()));
    }

    #[tokio::test]
    async fn call_without_a_launcher_errors() {
        let t = tool(None);
        let err = t
            .call(
                json!({ "script": "return 1;" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Internal(_)));
    }

    // ── Task 5: summary + transcriptDir + byte-exact launch text ─────────────

    /// A launcher that returns a full `WorkflowLaunched` with all optional
    /// fields set, for testing the result JSON shape and model_content text.
    struct RichMockLauncher {
        launched: WorkflowLaunched,
    }
    #[async_trait]
    impl WorkflowLauncher for RichMockLauncher {
        async fn launch(
            &self,
            _spec: WorkflowLaunchSpec,
        ) -> Result<WorkflowLaunched, WorkflowLaunchError> {
            Ok(self.launched.clone())
        }
    }

    #[tokio::test]
    async fn plugin_resume_hint_uses_registry_name_and_preserves_args() {
        let dir = unique_temp_path("plugin-resume-hint");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plugin.js");
        std::fs::write(&path, VALID_SCRIPT).unwrap();
        let registry = Arc::new(workflow::PluginWorkflowRegistry::new());
        registry.register(vec![workflow::PluginWorkflowEntry {
            name: "acme:create".into(),
            script_path: path,
        }]);
        let t = tool(Some(Arc::new(RichMockLauncher {
            launched: WorkflowLaunched {
                task_id: "w_resume".into(),
                run_id: Some("wf_abc123".into()),
                script_path: Some("/host/private/workflows/wf_abc123.js".into()),
                workflow_name: Some("acme:create".into()),
                summary: None,
                transcript_dir: None,
                error: None,
            },
        })))
        .with_plugin_workflows(registry);
        let args = json!({ "requirement": "quoted \"name\"", "nested": { "enabled": true } });
        let result = t
            .call(
                json!({ "name": "acme:create", "args": args }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        let content = result.data["model_content"].as_str().unwrap();
        let request = content
            .split("Workflow(")
            .nth(1)
            .unwrap()
            .split("). ")
            .next()
            .unwrap();
        let request: Value = serde_json::from_str(request).unwrap();
        assert_eq!(
            request,
            json!({ "name": "acme:create", "args": args, "resumeFromRunId": "wf_abc123" })
        );
        assert!(!content.contains("scriptPath"));
        assert!(!content.contains("Write/Edit"));
        assert!(!content.contains("/host/private"));
        // Persisted path stays available as diagnostic metadata, not an editing instruction.
        assert_eq!(
            result.data["scriptPath"],
            "/host/private/workflows/wf_abc123.js"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// call() with summary + transcriptDir → result JSON has both fields and the
    /// model-facing text contains the `Summary:` and `Transcript dir:` lines.
    #[tokio::test]
    async fn call_with_summary_and_transcript_dir_in_result() {
        let launcher = Arc::new(RichMockLauncher {
            launched: WorkflowLaunched {
                task_id: "w_t1".into(),
                run_id: Some("wf_abc123def456".into()),
                script_path: Some("/tmp/wf_abc123def456.js".into()),
                workflow_name: Some("my-wf".into()),
                summary: Some("A test workflow".into()),
                transcript_dir: Some(
                    "/home/.lingxi/projects/-Users-me-proj/sess123/subagents/workflows/wf_abc123def456".into()
                ),
                error: None,
            },
        });
        let t = tool(Some(launcher));
        let res = t
            .call(
                json!({ "script": "return 1;" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect("call ok");

        // JSON data shape
        assert_eq!(res.data["status"], "async_launched");
        assert_eq!(res.data["taskId"], "w_t1");
        assert_eq!(res.data["taskType"], "local_workflow");
        assert_eq!(res.data["summary"], "A test workflow");
        assert_eq!(
            res.data["transcriptDir"],
            "/home/.lingxi/projects/-Users-me-proj/sess123/subagents/workflows/wf_abc123def456"
        );
        assert_eq!(res.data["runId"], "wf_abc123def456");
        assert_eq!(res.data["scriptPath"], "/tmp/wf_abc123def456.js");
        assert_eq!(res.data["workflowName"], "my-wf");

        // model_content text — byte-exact per oracle §1 async_launched template
        let mc = res.data["model_content"]
            .as_str()
            .expect("model_content string");
        assert!(
            mc.starts_with("Workflow launched in background. Task ID: w_t1"),
            "must start with task id header: {mc}"
        );
        assert!(
            mc.contains("\nSummary: A test workflow"),
            "must have Summary line: {mc}"
        );
        assert!(
            mc.contains("\nTranscript dir: /home/.lingxi/projects/-Users-me-proj/sess123/subagents/workflows/wf_abc123def456"),
            "must have Transcript dir line: {mc}"
        );
        assert!(
            mc.contains("\nScript file: /tmp/wf_abc123def456.js\n(Edit this file with Write/Edit"),
            "must have Script file line: {mc}"
        );
        assert!(
            mc.contains("\nRun ID: wf_abc123def456\nTo resume after editing the script:"),
            "must have Run ID line: {mc}"
        );
        assert!(
            mc.ends_with(
                "\n\nYou will be notified when it completes. Use /workflows to watch live progress."
            ),
            "must end with footer: {mc}"
        );
    }

    /// call() with no summary/transcriptDir → those fields are absent from JSON and
    /// the model_content text has no `Summary:` or `Transcript dir:` lines.
    #[tokio::test]
    async fn call_without_optional_fields_omitted_from_result() {
        let launcher = Arc::new(RichMockLauncher {
            launched: WorkflowLaunched {
                task_id: "w_t2".into(),
                run_id: None,
                script_path: None,
                workflow_name: None,
                summary: None,
                transcript_dir: None,
                error: None,
            },
        });
        let t = tool(Some(launcher));
        let res = t
            .call(
                json!({ "script": "return 1;" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect("call ok");

        // Optional fields absent from JSON
        assert!(res.data.get("summary").is_none(), "summary must be absent");
        assert!(
            res.data.get("transcriptDir").is_none(),
            "transcriptDir must be absent"
        );
        assert!(
            res.data.get("runId").is_none(),
            "runId must be absent when None"
        );
        assert!(
            res.data.get("scriptPath").is_none(),
            "scriptPath must be absent when None"
        );

        // model_content has no conditional lines
        let mc = res.data["model_content"]
            .as_str()
            .expect("model_content string");
        assert!(
            !mc.contains("Summary:"),
            "no Summary line when absent: {mc}"
        );
        assert!(
            !mc.contains("Transcript dir:"),
            "no Transcript dir line when absent: {mc}"
        );
        assert!(
            !mc.contains("Script file:"),
            "no Script file line when absent: {mc}"
        );
        assert!(!mc.contains("Run ID:"), "no Run ID line when absent: {mc}");
        // Footer always present
        assert!(
            mc.ends_with(
                "\n\nYou will be notified when it completes. Use /workflows to watch live progress."
            ),
            "footer always present: {mc}"
        );
    }

    /// Byte-exact full launch text — verifies the entire model_content string
    /// against the §1 oracle template with all conditional lines present.
    #[tokio::test]
    async fn launch_text_byte_exact_full_template() {
        let launcher = Arc::new(RichMockLauncher {
            launched: WorkflowLaunched {
                task_id: "w_TASKID".into(),
                run_id: Some("wf_RUNID".into()),
                script_path: Some("/path/to/script.js".into()),
                workflow_name: Some("wf-name".into()),
                summary: Some("My workflow summary".into()),
                transcript_dir: Some("/tmp/transcripts/subagents/workflows/wf_RUNID".into()),
                error: None,
            },
        });
        let t = tool(Some(launcher));
        let res = t
            .call(
                json!({ "script": "return 1;" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect("call ok");

        let mc = res.data["model_content"]
            .as_str()
            .expect("model_content string");

        // Verify the EXACT string per §1 oracle template
        let expected = concat!(
            "Workflow launched in background. Task ID: w_TASKID",
            "\nSummary: My workflow summary",
            "\nTranscript dir: /tmp/transcripts/subagents/workflows/wf_RUNID",
            "\nScript file: /path/to/script.js",
            "\n(Edit this file with Write/Edit and re-invoke Workflow with {scriptPath: \"/path/to/script.js\"} to iterate without resending the script.)",
            "\nRun ID: wf_RUNID",
            "\nTo resume after editing the script: Workflow({scriptPath: \"/path/to/script.js\", resumeFromRunId: \"wf_RUNID\"}) — completed agents return cached results (cached results may themselves be empty — inspect journal.jsonl before assuming there is something to recover).",
            "\n\nYou will be notified when it completes. Use /workflows to watch live progress.",
        );
        assert_eq!(mc, expected, "launch text must be byte-exact per §1 oracle");
    }

    // is_enabled gate tests — port of `fbn()` / `pA()` local-deterministic subset.
    //
    // Env vars are process-global; all tests that touch LINGXI_DISABLE_WORKFLOWS
    // must hold ENV_LOCK so they don't race with each other.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Default: no env var set → tool is enabled.
    #[test]
    fn is_enabled_default() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");
        let t = tool(None);
        assert!(
            t.is_enabled(&ToolStaticContext::default()),
            "Workflow must be enabled by default"
        );
    }

    /// LINGXI_DISABLE_WORKFLOWS=1 → tool is disabled.
    #[test]
    fn is_enabled_disabled_by_env_1() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("LINGXI_DISABLE_WORKFLOWS", "1");
        let t = tool(None);
        let enabled = t.is_enabled(&ToolStaticContext::default());
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");
        assert!(!enabled, "Workflow must be disabled when env var is '1'");
    }

    /// LINGXI_DISABLE_WORKFLOWS=true → tool is disabled.
    #[test]
    fn is_enabled_disabled_by_env_true() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("LINGXI_DISABLE_WORKFLOWS", "true");
        let t = tool(None);
        let enabled = t.is_enabled(&ToolStaticContext::default());
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");
        assert!(!enabled, "Workflow must be disabled when env var is 'true'");
    }

    /// LINGXI_DISABLE_WORKFLOWS=yes → tool is disabled.
    #[test]
    fn is_enabled_disabled_by_env_yes() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("LINGXI_DISABLE_WORKFLOWS", "yes");
        let t = tool(None);
        let enabled = t.is_enabled(&ToolStaticContext::default());
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");
        assert!(!enabled, "Workflow must be disabled when env var is 'yes'");
    }

    /// LINGXI_DISABLE_WORKFLOWS=0 (falsy) → tool remains enabled.
    #[test]
    fn is_enabled_falsy_env_value_stays_enabled() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("LINGXI_DISABLE_WORKFLOWS", "0");
        let t = tool(None);
        let enabled = t.is_enabled(&ToolStaticContext::default());
        std::env::remove_var("LINGXI_DISABLE_WORKFLOWS");
        assert!(
            enabled,
            "Workflow must stay enabled when env var is '0' (falsy)"
        );
    }

    // ── Save dynamic workflow (`eya`/`uQ_`/`lme`) ─────────────────────────────

    #[test]
    fn sanitize_workflow_name_is_kebab_with_fallback() {
        // Oracle `lme`: lowercase, collapse non-alnum runs to '-', trim, fallback.
        assert_eq!(
            sanitize_workflow_name("My Cool Workflow"),
            "my-cool-workflow"
        );
        assert_eq!(sanitize_workflow_name("  Deploy!! Site  "), "deploy-site");
        assert_eq!(sanitize_workflow_name("a__b--c"), "a-b-c");
        assert_eq!(sanitize_workflow_name("Review123"), "review123");
        assert_eq!(sanitize_workflow_name("***"), "workflow");
        assert_eq!(sanitize_workflow_name(""), "workflow");
    }

    #[test]
    fn scope_helpers_toggle_and_label() {
        assert_eq!(WorkflowScope::Project.wire(), "project");
        assert_eq!(WorkflowScope::User.wire(), "user");
        assert_eq!(WorkflowScope::Project.label(), "Project");
        assert_eq!(WorkflowScope::User.label(), "User");
        assert_eq!(WorkflowScope::Project.toggled(), WorkflowScope::User);
        assert_eq!(WorkflowScope::User.toggled(), WorkflowScope::Project);
    }

    #[test]
    fn scope_dir_project_is_cwd_dotdir_workflows() {
        let cwd = std::path::Path::new("/proj/root");
        let dir = workflow_scope_dir(WorkflowScope::Project, cwd);
        assert_eq!(dir, cwd.join(branding::DOT_DIR).join("workflows"));
    }

    #[test]
    fn scope_dir_user_uses_config_home() {
        let _g = ENV_LOCK.lock().unwrap();
        let config_dir = unique_temp_path("scope-user");
        let dir = with_config_dir_env(&config_dir, || {
            workflow_scope_dir(WorkflowScope::User, std::path::Path::new("/anything"))
        });
        assert_eq!(dir, config_dir.join("workflows"));
    }

    #[test]
    fn save_writes_file_and_reports_path_and_size() {
        let cwd = unique_temp_path("save-project");
        std::fs::create_dir_all(&cwd).unwrap();
        let script = "export const meta = { name: 'x' };\n";
        let saved =
            save_dynamic_workflow("My WF", WorkflowScope::Project, script, false, &cwd).unwrap();
        assert_eq!(saved.name, "my-wf");
        assert_eq!(
            saved.path,
            cwd.join(branding::DOT_DIR)
                .join("workflows")
                .join("my-wf.js")
        );
        assert_eq!(saved.scope, WorkflowScope::Project);
        assert_eq!(saved.script_size_chars, script.encode_utf16().count());
        assert_eq!(std::fs::read_to_string(&saved.path).unwrap(), script);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_600_file_and_700_dir_modes() {
        use std::os::unix::fs::PermissionsExt;
        let cwd = unique_temp_path("save-modes");
        std::fs::create_dir_all(&cwd).unwrap();
        let saved =
            save_dynamic_workflow("perm", WorkflowScope::Project, "x", false, &cwd).unwrap();
        let file_mode = std::fs::metadata(&saved.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "file mode must be 0o600");
        let dir_mode = std::fs::metadata(saved.path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "dir mode must be 0o700");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn save_without_overwrite_rejects_existing_file() {
        let cwd = unique_temp_path("save-eexist");
        std::fs::create_dir_all(&cwd).unwrap();
        save_dynamic_workflow("dup", WorkflowScope::Project, "first", false, &cwd).unwrap();
        let err = save_dynamic_workflow("dup", WorkflowScope::Project, "second", false, &cwd)
            .unwrap_err();
        match &err {
            WorkflowSaveError::AlreadyExists { name, path } => {
                assert_eq!(name, "dup");
                assert!(path.ends_with("dup.js"));
            }
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
        // Byte-exact message.
        let expected_path = cwd.join(branding::DOT_DIR).join("workflows").join("dup.js");
        assert_eq!(
            err.to_string(),
            format!(
                "Dynamic workflow \"dup\" already exists at {}. Use a different name or overwrite.",
                expected_path.display()
            )
        );
        // The original content is untouched (create-new never opened it).
        assert_eq!(std::fs::read_to_string(&expected_path).unwrap(), "first");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn save_with_overwrite_replaces_existing_file() {
        let cwd = unique_temp_path("save-overwrite");
        std::fs::create_dir_all(&cwd).unwrap();
        save_dynamic_workflow("dup", WorkflowScope::Project, "first", false, &cwd).unwrap();
        let saved =
            save_dynamic_workflow("dup", WorkflowScope::Project, "second", true, &cwd).unwrap();
        assert_eq!(std::fs::read_to_string(&saved.path).unwrap(), "second");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[cfg(unix)]
    #[test]
    fn save_refuses_symlinked_project_state_directory() {
        let cwd = unique_temp_path("save-symlink-root");
        let victim = unique_temp_path("save-symlink-victim");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&victim).unwrap();
        std::os::unix::fs::symlink(&victim, cwd.join(branding::DOT_DIR)).unwrap();

        let err = save_dynamic_workflow("escaped", WorkflowScope::Project, "secret", true, &cwd)
            .unwrap_err();
        assert!(matches!(err, WorkflowSaveError::Io(_)));
        assert!(
            !victim.join("workflows/escaped.js").exists(),
            "a project-local symlink must never redirect workflow output"
        );
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&victim);
    }

    #[test]
    fn saved_feedback_is_byte_exact() {
        let path = std::path::Path::new("/home/u/.lingxi/workflows/deploy.js");
        assert_eq!(
            saved_feedback("deploy", path),
            "Dynamic workflow saved to /home/u/.lingxi/workflows/deploy.js. \
             Invoke as /deploy or Workflow({name: \"deploy\"}) in future sessions."
        );
    }
}
