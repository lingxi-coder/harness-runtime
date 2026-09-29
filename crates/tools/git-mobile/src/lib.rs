//! Mobile-only structured `Git` tool crate (shared mobile git surface, spec
//! G1–G8).
//!
//! `GitTool` is the model-facing git tool on mobile, backed by libgit2 (the
//! `git2` crate) running **in-process** — there is no `git` binary, no exec, no
//! sandbox/minijail involvement. Operations are a fixed enum (clone / fetch /
//! pull / status / diff / log / show / `branch_list` / checkout / add / commit
//! / `branch_create` / merge / push),
//! NOT a free-form shell string. v1 is read + local-write + **push**;
//! merge/pull/push are **fast-forward-only**, and remotes are **HTTPS-only**
//! (token supplied in-memory by the Kotlin host via the libgit2 credential
//! callback — never to disk or a child-process env).
//!
//! The tool is registered ONLY when the device + config gate passes
//! (`ctx.mobile_git().is_some_and(|c| c.enabled)`). The gate is computed in the
//! mobile engine composition root so a selected but blocked/unlinked
//! mobile-linux runtime never silently falls back to the legacy Android git
//! carrier. On builds with no mobile git carrier the tool is absent (not
//! erroring).
//!
//! `git2` is a safe wrapper; the only C is `libgit2-sys` at build time. The
//! crate is `#![deny(unsafe_code)]` (NOT `forbid`) for TWO audited carve-outs,
//! both `unsafe` libgit2 process-global setters the vendored `git2` exposes:
//! [`auth::set_ca_location`] (`set_ssl_cert_dir` over the CA-location option —
//! spec G6 assumed a safe `set_ssl_cert_locations` absent in `git2 0.21`) and
//! [`auth::ensure_ssh_homedir`] (`set_homedir` so libssh2 can expand
//! `~/.ssh/known_hosts` on Android, which has no `HOME`). Each `unsafe` block is
//! isolated under a localized `#[allow(unsafe_code)]`; `deny` keeps every other
//! line unsafe-free. See `auth.rs` for the full carve-out rationale.

#![deny(unsafe_code)]

pub mod auth;
pub mod ops;

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
use tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH;
use tool_api::BuiltinToolContext;

/// Tool name byte-lock — the model-facing name for the mobile Git tool.
pub const TOOL_NAME: &str = "Git";

/// The fixed set of supported operations (schema `operation` enum). The tool is
/// structured: the model picks one of these, NOT a free-form command.
const OPERATIONS: &[&str] = &[
    "clone",
    "fetch",
    "pull",
    "status",
    "diff",
    "log",
    "show",
    "branch_list",
    "checkout",
    "add",
    "commit",
    "branch_create",
    "merge",
    "push",
];

/// `GitTool` — run a structured, in-process git operation via libgit2.
#[derive(Clone)]
pub struct GitTool {
    ctx: BuiltinToolContext,
}

impl GitTool {
    /// Construct from the builtin tool context.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "operation":   { "type": "string", "enum": OPERATIONS, "description": "The git operation to run." },
            "repo":        { "type": "string", "description": "Repository path relative to the workspace root; defaults to '.'." },
            "max":         { "type": "integer", "description": "Max number of commits to walk (log); defaults to a built-in cap." },
            "repo_url":    { "type": "string", "description": "Remote HTTPS URL (clone)." },
            "remote":      { "type": "string", "description": "Remote name (fetch/pull); defaults to origin." },
            "branch":      { "type": "string", "description": "Branch name (checkout/branch_create/merge target; push source — defaults to the current branch)." },
            "refspec":     { "type": "string", "description": "Refspec for fetch." },
            "paths":       { "type": "array", "items": { "type": "string" }, "description": "Paths to stage (add) or limit diff/status to." },
            "message":     { "type": "string", "description": "Commit message (commit)." },
            "rev":         { "type": "string", "description": "A single revision/oid (show/checkout)." },
            "rev_range":   { "type": "string", "description": "A revision range (log/diff)." },
            "new_branch":  { "type": "string", "description": "Name of the branch to create (branch_create)." },
            "description": { "type": "string", "description": "Optional short description of what this operation does." }
        },
        "required": ["operation"]
    })
});

#[async_trait]
impl Tool for GitTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }

    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        // Defensive double-gate: registration (`register_all`) already filters
        // on this same flag, but keep the tool inert if it ever lands in a
        // registry without the gate set.
        self.ctx.mobile_git().is_some_and(|g| g.enabled)
    }

    fn max_result_size_chars(&self) -> usize {
        MAX_TOOL_OUTPUT_LENGTH
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        // Conservative: a git operation can mutate the index / worktree / refs,
        // so never run it concurrently with other tools.
        false
    }

    fn is_read_only(&self, _input: &Value) -> bool {
        // Git mutates (add/commit/checkout/merge/clone/fetch/pull); even the
        // read ops share a tool that is not read-only overall.
        false
    }

    async fn check_permissions(&self, _input: &Value, _ctx: &ToolUseContext) -> PermissionResult {
        // Mirrors the desktop/shell-mobile stub: real allow/ask gating lives in
        // the engine's `AdapterPermissionGate` (wired on mobile via
        // `PermissionRequestSink` -> Kotlin UI), NOT in tool-level rule code.
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "android-git (engine AdapterPermissionGate handles allow/ask)".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, input: &Value, _opts: &DescriptionOptions) -> String {
        match input.get("description").and_then(Value::as_str) {
            Some(d) if !d.is_empty() => d.to_string(),
            _ => "Run a structured git operation".into(),
        }
    }

    async fn prompt(&self, _opts: &PromptOptions) -> String {
        let has_token = self.ctx.mobile_git().is_some_and(|g| g.has_token);

        let mut prompt = String::new();
        prompt.push_str(
            "Run a STRUCTURED git operation on this mobile device. This is NOT a \
             shell: pick one `operation` from a fixed set (it is an enum, not a \
             free-form command line) and supply the relevant typed parameters \
             (repo_url, remote, branch, refspec, paths, message, rev, rev_range, \
             new_branch).\n\n",
        );
        prompt.push_str(&format!(
            "Supported operations: {}.\n\n",
            OPERATIONS.join(", ")
        ));
        prompt.push_str(
            "Note: unlike desktop `git diff` (which hides untracked files by \
             default), the `diff` operation ALSO includes untracked files, so a \
             brand-new file is reported in the diff (its content is not printed as \
             additions) — one call surfaces new files too.\n\n",
        );
        prompt.push_str(
            "v1 is READ + LOCAL-WRITE + PUSH. merge and pull are FAST-FORWARD-ONLY \
             and push is fast-forward-only too (a non-fast-forward is reported as \
             a named error — pull/rebase first — never forced). Remotes are \
             HTTPS-ONLY (git@/ssh URLs are rejected).\n\n",
        );
        if has_token {
            prompt.push_str(
                "Network operations (clone/fetch/pull/push) use the host-supplied HTTPS \
                 credentials.\n",
            );
        } else {
            prompt.push_str(
                "No git credentials are configured: network operations \
                 (clone/fetch/pull/push) are unavailable until HTTPS credentials are \
                 provided by the host. Local operations (status/diff/log/show/\
                 branch_list/checkout/add/commit/branch_create/merge) still work.\n",
            );
        }
        prompt
    }

    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let op = input
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| ValidationError("missing `operation`".into()))?;
        if op.is_empty() {
            return Err(ValidationError("`operation` must not be empty".into()));
        }
        Ok(())
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress_tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        // Extract + validate the operation.
        let operation = input
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("missing operation".into()))?;
        if operation.is_empty() {
            return Err(ToolError::InvalidInput(
                "operation must not be empty".into(),
            ));
        }

        // Anchor every op to the host-supplied workspace root; the repo sub-path
        // comes from the `repo` param (default ".").
        let workspace_root = self
            .ctx
            .mobile_git()
            .as_ref()
            .map(|g| g.workspace_root.clone())
            .ok_or_else(|| ToolError::InvalidInput("mobile_git context is absent".into()))?;
        let repo_rel = input.get("repo").and_then(Value::as_str).unwrap_or(".");
        let workspace_path = std::path::Path::new(&workspace_root);

        // Network ops (clone/fetch/pull/push) read the in-memory secret (token +
        // CA dir) from the SEPARATE `android_git_secret` seam — never the public
        // `android_git` carrier — and never touch disk/env.
        let dispatch_result = if matches!(operation, "clone" | "fetch" | "pull" | "push") {
            let net = self.git_net_config();
            dispatch_network(&net, workspace_path, repo_rel, operation, &input)
        } else {
            // All local ops open the repo first, then dispatch.
            dispatch_local(workspace_path, repo_rel, operation, &input)
        };

        match dispatch_result {
            Ok(data) => Ok(ToolCallResult {
                data,
                model_content: None,
                new_messages: Vec::new(),
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            }),
            Err(e) => Err(map_git_op_error(e)),
        }
    }
}

impl GitTool {
    /// Build the per-operation [`ops::GitNetConfig`] from the in-memory secret
    /// seam (`ctx.mobile_git_secret()`). The token is cloned out of the secret
    /// carrier only for the duration of the call; it is never logged, written
    /// to disk, or passed to a child process. When no secret is configured the
    /// config is empty (anonymous / public remotes, default CA).
    fn git_net_config(&self) -> ops::GitNetConfig {
        match self.ctx.mobile_git_secret() {
            Some(secret) => ops::GitNetConfig {
                provider: secret.credential_provider.clone(),
                ca_dir: secret.ca_dir.clone(),
                // SSH is configured only when a (non-empty) private-key path is
                // present; otherwise leave `None` so an SSH-URL op surfaces the
                // named `ssh_allowed` "not configured" error via dispatch.
                ssh: secret
                    .ssh_private_key_path
                    .as_ref()
                    .filter(|p| !p.is_empty())
                    .map(|private_key_path| auth::SshConfig {
                        private_key_path: private_key_path.clone(),
                        public_key_path: secret
                            .ssh_public_key_path
                            .clone()
                            .filter(|p| !p.is_empty()),
                        known_hosts_sha256_hex: secret.ssh_known_hosts_sha256_hex.clone(),
                    }),
            },
            None => ops::GitNetConfig::default(),
        }
    }
}

/// Dispatch a network op (clone/fetch/pull/push) to `ops::`. clone targets the
/// workspace-relative `dest` (`repo` param, default `cloned`); fetch/pull/push
/// open the existing repo at `repo` first.
fn dispatch_network(
    net: &ops::GitNetConfig,
    workspace_root: &std::path::Path,
    repo_rel: &str,
    operation: &str,
    input: &Value,
) -> Result<Value, ops::GitOpError> {
    let str_param = |key: &str| input.get(key).and_then(Value::as_str);
    match operation {
        "clone" => {
            let repo_url = str_param("repo_url")
                .ok_or_else(|| ops::GitOpError::InvalidInput("clone requires `repo_url`".into()))?;
            // Destination is the workspace-relative `repo` path; default to a
            // `cloned` sub-dir so a bare `clone` never targets the ws root.
            let dest = if repo_rel == "." { "cloned" } else { repo_rel };
            Ok(
                serde_json::to_value(ops::clone(net, workspace_root, repo_url, dest)?)
                    .unwrap_or(Value::Null),
            )
        }
        "fetch" => {
            let repo = ops::open_repo(workspace_root, repo_rel)?;
            let remote = str_param("remote").unwrap_or("origin");
            Ok(serde_json::to_value(ops::fetch(net, &repo, remote)?).unwrap_or(Value::Null))
        }
        "pull" => {
            let repo = ops::open_repo(workspace_root, repo_rel)?;
            let remote = str_param("remote").unwrap_or("origin");
            let branch = str_param("branch").unwrap_or("");
            Ok(serde_json::to_value(ops::pull(net, &repo, remote, branch)?).unwrap_or(Value::Null))
        }
        "push" => {
            let repo = ops::open_repo(workspace_root, repo_rel)?;
            let remote = str_param("remote").unwrap_or("origin");
            let branch = str_param("branch").unwrap_or("");
            Ok(serde_json::to_value(ops::push(net, &repo, remote, branch)?).unwrap_or(Value::Null))
        }
        other => Err(ops::GitOpError::InvalidInput(format!(
            "unknown network git operation '{other}'"
        ))),
    }
}

/// Open the repo at `workspace_root/repo_rel` and run the local `operation`,
/// returning the op's JSON payload. Param extraction + `ops::` dispatch live
/// here so `GitTool::call` stays small. Network ops are handled by the caller.
fn dispatch_local(
    workspace_root: &std::path::Path,
    repo_rel: &str,
    operation: &str,
    input: &Value,
) -> Result<Value, ops::GitOpError> {
    let str_param = |key: &str| input.get(key).and_then(Value::as_str);
    let repo = ops::open_repo(workspace_root, repo_rel)?;
    match operation {
        "status" => Ok(json!({ "entries": ops::status(&repo)? })),
        "log" => {
            let max = input
                .get("max")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok());
            Ok(json!({ "commits": ops::log(&repo, max)? }))
        }
        "diff" => Ok(serde_json::to_value(ops::diff(&repo)?).unwrap_or(Value::Null)),
        "show" => {
            let rev = str_param("rev")
                .ok_or_else(|| ops::GitOpError::InvalidInput("show requires `rev`".into()))?;
            let (info, diff) = ops::show(&repo, rev)?;
            Ok(json!({ "commit": info, "diff": diff }))
        }
        "branch_list" => Ok(json!({ "branches": ops::branch_list(&repo)? })),
        "add" => {
            let paths: Vec<String> = input
                .get("paths")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            Ok(serde_json::to_value(ops::add(&repo, &paths)?).unwrap_or(Value::Null))
        }
        "commit" => {
            let message = str_param("message")
                .ok_or_else(|| ops::GitOpError::InvalidInput("commit requires `message`".into()))?;
            Ok(serde_json::to_value(ops::commit(&repo, message)?).unwrap_or(Value::Null))
        }
        "branch_create" => {
            let name = str_param("new_branch")
                .or_else(|| str_param("branch"))
                .ok_or_else(|| {
                    ops::GitOpError::InvalidInput("branch_create requires `new_branch`".into())
                })?;
            Ok(serde_json::to_value(ops::branch_create(&repo, name)?).unwrap_or(Value::Null))
        }
        "checkout" => {
            let target = str_param("branch")
                .or_else(|| str_param("rev"))
                .ok_or_else(|| {
                    ops::GitOpError::InvalidInput("checkout requires `branch` or `rev`".into())
                })?;
            Ok(serde_json::to_value(ops::checkout(&repo, target)?).unwrap_or(Value::Null))
        }
        "merge" => {
            let source = str_param("branch")
                .or_else(|| str_param("rev"))
                .ok_or_else(|| {
                    ops::GitOpError::InvalidInput(
                        "merge requires `branch` or `rev` (source)".into(),
                    )
                })?;
            Ok(serde_json::to_value(ops::merge(&repo, source)?).unwrap_or(Value::Null))
        }
        other => Err(ops::GitOpError::InvalidInput(format!(
            "unknown git operation '{other}'"
        ))),
    }
}

/// Map a [`ops::GitOpError`] into a [`ToolError`]. The named reason is preserved
/// in the message so the engine surface (and the model) can see *why* it
/// failed (dirty worktree, non-fast-forward, path escape, …).
fn map_git_op_error(e: ops::GitOpError) -> ToolError {
    use ops::GitOpError as G;
    match e {
        G::Dirty(_) | G::NonFastForward(_) | G::InvalidInput(_) | G::NotFound(_) => {
            ToolError::InvalidInput(e.to_string())
        }
        G::Escape(_) => ToolError::InvalidInput(e.to_string()),
        G::Libgit2(msg) => ToolError::Internal(format!("libgit2: {msg}")),
    }
}

/// Register the mobile `Git` tool against `reg` — ONLY when the gate passes.
///
/// The gate is `ctx.mobile_git().is_some_and(|c| c.enabled)`. When the gate is
/// unmet the tool is simply not registered — **absent, not erroring** (spec
/// invariant).
pub fn register_all(reg: &mut tool_api::ToolRegistry, ctx: tool_api::BuiltinToolContext) {
    use std::sync::Arc;
    if ctx.mobile_git().is_some_and(|g| g.enabled) {
        reg.register_builtin(Arc::new(GitTool::new(ctx)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobile_linux_api::ProcessOutput;
    use tool_api::test_support::shell_test_ctx;
    use tool_api::{AndroidGitToolCtx, ToolRegistry};

    fn ok_output() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    /// Build a ctx with `android_git` enabled and a token, anchored at a fresh
    /// tempdir workspace root.
    fn test_ctx_git_enabled() -> BuiltinToolContext {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ctx = shell_test_ctx(ok_output());
        ctx.android_git = Some(AndroidGitToolCtx {
            enabled: true,
            has_token: true,
            workspace_root: dir.path().to_string_lossy().into_owned(),
        });
        // Keep the tempdir alive for the duration of the process — the tests
        // only read the path string, never the directory, so leaking it is
        // fine and avoids a premature cleanup.
        std::mem::forget(dir);
        ctx
    }

    /// Same as `test_ctx_git_enabled` but with `has_token: false`.
    fn test_ctx_git_no_token() -> BuiltinToolContext {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut ctx = shell_test_ctx(ok_output());
        ctx.android_git = Some(AndroidGitToolCtx {
            enabled: true,
            has_token: false,
            workspace_root: dir.path().to_string_lossy().into_owned(),
        });
        std::mem::forget(dir);
        ctx
    }

    #[test]
    fn name_is_git_and_schema_has_operation() {
        let t = GitTool::new(test_ctx_git_enabled());
        assert_eq!(t.name(), "Git");
        let schema = t.input_schema();
        let props = &schema["properties"];
        assert!(props.get("operation").is_some(), "operation param required");
        assert!(schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "operation"));
    }

    #[test]
    fn is_enabled_follows_android_git_flag() {
        let static_ctx = ToolStaticContext::default();

        let enabled = GitTool::new(test_ctx_git_enabled());
        assert!(enabled.is_enabled(&static_ctx));

        // android_git: None -> disabled.
        let mut none_ctx = shell_test_ctx(ok_output());
        none_ctx.android_git = None;
        let none = GitTool::new(none_ctx);
        assert!(!none.is_enabled(&static_ctx));

        // Some(enabled: false) -> disabled.
        let mut off_ctx = shell_test_ctx(ok_output());
        off_ctx.android_git = Some(AndroidGitToolCtx {
            enabled: false,
            has_token: true,
            workspace_root: "/tmp".into(),
        });
        let off = GitTool::new(off_ctx);
        assert!(!off.is_enabled(&static_ctx));
    }

    #[tokio::test]
    async fn prompt_declares_structured_git_with_push() {
        let opts = PromptOptions {
            include_examples: false,
            model: None,
            model_profile: None,
        };

        let with_token = GitTool::new(test_ctx_git_enabled());
        let prompt = with_token.prompt(&opts).await;
        assert!(
            prompt.contains("git"),
            "prompt should mention git: {prompt}"
        );
        assert!(prompt.contains("mobile device"));
        assert!(!prompt.contains("Android device"));
        assert!(prompt.contains("push"), "push now listed: {prompt}");
        assert!(
            !prompt.to_lowercase().contains("no push"),
            "must not say 'no push': {prompt}"
        );
        assert!(
            prompt.contains("FAST-FORWARD-ONLY"),
            "ff-only still stated: {prompt}"
        );
        assert!(
            prompt.contains("status"),
            "prompt should list an operation name: {prompt}"
        );

        // has_token == false -> prompt mentions credentials.
        let no_token = GitTool::new(test_ctx_git_no_token());
        let prompt = no_token.prompt(&opts).await;
        assert!(
            prompt.contains("credentials"),
            "no-token prompt should mention credentials: {prompt}"
        );
    }

    #[tokio::test]
    async fn call_dispatches_commit() {
        use tool_api::test_support::{fresh_ctx, fresh_tx};

        // Prepare a real repo at <workspace>/r with one commit, then stage a new
        // file so `commit` has something to write.
        let ws = tempfile::tempdir().expect("tempdir");
        let repo_dir = ws.path().join("r");
        std::fs::create_dir(&repo_dir).unwrap();
        let repo = git2::Repository::init(&repo_dir).unwrap();
        let sig = git2::Signature::now("Tester", "tester@example.com").unwrap();
        std::fs::write(repo_dir.join("a.txt"), "alpha\n").unwrap();
        {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("a.txt")).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "first", &tree, &[])
                .unwrap();
        }
        // Stage a second file for the commit-under-test.
        std::fs::write(repo_dir.join("b.txt"), "beta\n").unwrap();
        {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("b.txt")).unwrap();
            index.write().unwrap();
        }

        let mut ctx = shell_test_ctx(ok_output());
        ctx.android_git = Some(AndroidGitToolCtx {
            enabled: true,
            has_token: true,
            workspace_root: ws.path().to_string_lossy().into_owned(),
        });
        let tool = GitTool::new(ctx);

        let result = tool
            .call(
                json!({ "operation": "commit", "repo": "r", "message": "dispatched" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("commit dispatch should succeed");

        let oid = result.data["oid"]
            .as_str()
            .expect("commit result carries an oid");
        // The reported oid is the new HEAD with the right message.
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.id().to_string(), oid, "data.oid is the new HEAD");
        assert_eq!(head.summary().unwrap(), Some("dispatched"));
    }

    /// `clone` of a `file://` bare remote through `GitTool::call` produces a
    /// working repo under the workspace — exercises the full call -> ops::clone
    /// path (no real network/token).
    #[tokio::test]
    async fn call_dispatches_clone_from_file_remote() {
        use tool_api::test_support::{fresh_ctx, fresh_tx};

        // Build a `file://` source repo with one commit.
        let remote_dir = tempfile::tempdir().expect("tempdir");
        let src = git2::Repository::init(remote_dir.path()).unwrap();
        let sig = git2::Signature::now("R", "r@example.com").unwrap();
        std::fs::write(remote_dir.path().join("f.txt"), "hi\n").unwrap();
        {
            let mut index = src.index().unwrap();
            index.add_path(std::path::Path::new("f.txt")).unwrap();
            index.write().unwrap();
            let tree = src.find_tree(index.write_tree().unwrap()).unwrap();
            src.commit(Some("HEAD"), &sig, &sig, "c1", &tree, &[])
                .unwrap();
        }
        let url = format!(
            "file://{}",
            remote_dir.path().canonicalize().unwrap().display()
        );

        // ctx anchored at a fresh workspace.
        let ws = tempfile::tempdir().expect("tempdir");
        let mut ctx = shell_test_ctx(ok_output());
        ctx.android_git = Some(AndroidGitToolCtx {
            enabled: true,
            has_token: false,
            workspace_root: ws.path().to_string_lossy().into_owned(),
        });
        let tool = GitTool::new(ctx);

        let result = tool
            .call(
                json!({ "operation": "clone", "repo_url": url, "repo": "cloned" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("clone dispatch should succeed");
        assert!(
            result.data["head"].as_str().is_some(),
            "clone result carries a head oid"
        );
        assert!(
            ws.path().join("cloned").join("f.txt").exists(),
            "cloned working file present"
        );
    }

    /// `push` of a `file://` bare remote through `GitTool::call` advances the
    /// remote ref — exercises the full call -> dispatch_network -> ops::push
    /// path (no real network/token).
    #[tokio::test]
    async fn call_dispatches_push_to_file_remote() {
        use tool_api::test_support::{fresh_ctx, fresh_tx};

        let work = tempfile::tempdir().unwrap();
        let bare = tempfile::tempdir().unwrap();
        let remote_repo = git2::Repository::init_bare(bare.path()).unwrap();
        let repo = git2::Repository::init(work.path()).unwrap();
        let sig = git2::Signature::now("T", "t@example.com").unwrap();
        std::fs::write(work.path().join("a.txt"), "x\n").unwrap();
        let oid = {
            let mut idx = repo.index().unwrap();
            idx.add_path(std::path::Path::new("a.txt")).unwrap();
            idx.write().unwrap();
            let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "c1", &tree, &[])
                .unwrap()
        };
        let branch = repo.head().unwrap().shorthand().unwrap().to_owned();
        repo.remote("origin", &format!("file://{}", bare.path().display()))
            .unwrap();

        // ctx anchored at the working repo root (SAME helper wiring the clone
        // dispatch test uses: set `android_git.workspace_root` on a shell_test_ctx).
        let mut ctx = shell_test_ctx(ok_output());
        ctx.android_git = Some(AndroidGitToolCtx {
            enabled: true,
            has_token: false,
            workspace_root: work.path().to_string_lossy().into_owned(),
        });
        let tool = GitTool::new(ctx);

        let input = json!({ "operation": "push", "repo": ".", "remote": "origin" });
        let res = tool
            .call(input, fresh_ctx(), fresh_tx())
            .await
            .expect("push call ok");
        let data = res.data;
        assert_eq!(data["branch"], branch);
        assert_eq!(data["pushed_oid"], oid.to_string());
        assert_eq!(
            remote_repo
                .find_reference(&format!("refs/heads/{branch}"))
                .unwrap()
                .target()
                .unwrap(),
            oid
        );
    }

    /// An ssh URL routed through `GitTool::call` is rejected (G7).
    #[tokio::test]
    async fn call_rejects_ssh_clone() {
        use tool_api::test_support::{fresh_ctx, fresh_tx};
        let tool = GitTool::new(test_ctx_git_enabled());
        let err = tool
            .call(
                json!({ "operation": "clone", "repo_url": "git@github.com:x/y.git" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect_err("ssh clone should be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("ssh") && msg.contains("HTTPS"),
            "ssh rejection should name ssh + HTTPS, got: {msg}"
        );
    }

    #[test]
    fn register_all_gates_on_enabled() {
        // Enabled -> registry contains "Git".
        let mut reg = ToolRegistry::new();
        register_all(&mut reg, test_ctx_git_enabled());
        assert!(
            reg.find_by_name("Git").is_some(),
            "enabled gate should register Git"
        );

        // android_git: None -> Git absent (not erroring).
        let mut reg = ToolRegistry::new();
        let mut none_ctx = shell_test_ctx(ok_output());
        none_ctx.android_git = None;
        register_all(&mut reg, none_ctx);
        assert!(
            reg.find_by_name("Git").is_none(),
            "absent gate should NOT register Git"
        );
    }
}
