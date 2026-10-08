//! `FileEditTool` — literal-search-and-replace inside a UTF-8 file.
//!
//! Wire-locked semantics (no regex by default — claude-code parity):
//! - `replace_all: false` (default): EXACTLY ONE match of `old_string` must
//!   appear in the file; otherwise reject.
//! - `replace_all: true`: replace every occurrence via `String::replace`.
//! - Patch preview truncation suffix template: `"\n\n... [{N} lines truncated] ..."`
//!   (spec §7; `{N}` is the literal placeholder substituted via `replace`).

use crate::structured_patch::count_patch_lines;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;
use telemetry::otel;
use telemetry::pii::{PiiTagged, Verified};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{EDIT_COMPLETED, EDIT_FAILED, EDIT_STARTED};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
};
use tool_api::util::path_validation::{
    canonicalize_and_validate, emit_blocked_event, translate_model_path,
};
use tool_api::BuiltinToolContext;

/// Tool name byte-lock — matches claude-code tool registry.
pub const TOOL_NAME: &str = "Edit";

/// Verbatim port of claude-code `FileEditTool/prompt.ts` `getEditToolDescription()`
/// (== the tool's DESCRIPTION). The TS builder has two runtime variations, both of
/// which resolve deterministically for this 3P port:
///   - `isCompactLinePrefixEnabled()` is `true` by default (killswitch off), and the
///     Read-tool port already canonicalized the compact format → `prefixFormat` is
///     `"line number + tab"` (the padded-arrow legacy form is intentionally not ported).
///   - `process.env.USER_TYPE === 'ant'` is the Anthropic-internal build; this 3P port
///     is never `ant` → `minimalUniquenessHint` is empty.
///   - `getPreReadInstruction()` interpolates `FILE_READ_TOOL_NAME = 'Read'`, and begins
///     with `\n-` (so `Usage:` is immediately followed by the first bullet) and ends with
///     `...reading the file.` (v2.1.183 dropped the trailing space the older builds had —
///     the binary's `wBp()`/`RBp()` template literal closes immediately after the period).
const EDIT_DESCRIPTION: &str = r#"Performs exact string replacements in files.

Usage:
- You must use your `Read` tool at least once in the conversation before editing. This tool will error if you attempt an edit without reading the file.
- When editing text from Read tool output, ensure you preserve the exact indentation (tabs/spaces) as it appears AFTER the line number prefix. The line number prefix format is: line number + tab. Everything after that is the actual file content to match. Never include any part of the line number prefix in the old_string or new_string.
- ALWAYS prefer editing existing files in the codebase. NEVER write new files unless explicitly required.
- Only use emojis if the user explicitly requests it. Avoid adding emojis to files unless asked.
- The edit will FAIL if `old_string` is not unique in the file. Either provide a larger string with more surrounding context to make it unique or use `replace_all` to change every instance of `old_string`.
- Use `replace_all` for replacing and renaming strings across the file. This parameter is useful if you want to rename a variable for instance."#;

/// The SHORT Edit prompt — byte-locked VERBATIM to claude-code `RBp(e)`'s
/// `Dh(e)===true` branch (binary offset 202614310), served to current-gen
/// default models. `${Ws}`=Read; the line-prefix slot uses `t=N$e()`
/// (`tengu_tab_read_sep`, defaults false) ⇒ `"line number + tab"` (matching the
/// LONG body's `${n}`). The "fails otherwise" em-dash is U+2014.
const EDIT_PROMPT_SHORT: &str = r#"Performs exact string replacement in a file.

- You must Read the file in this conversation before editing, or the call will fail.
- `old_string` must match the file exactly, including indentation, and be unique — the edit fails otherwise. Strip the Read line prefix (line number + tab) before matching.
- `replace_all: true` replaces every occurrence instead."#;

/// Patch-preview truncation template — spec §7. `{N}` is a literal that the
/// emitter substitutes with the elided-line count via `String::replace`.
pub const PATCH_TRUNCATION_SUFFIX_TEMPLATE: &str = "\n\n... [{N} lines truncated] ...";

/// Maximum number of patch-preview lines retained before the truncation
/// suffix is appended. Chosen so a typical diff fits.
pub const PATCH_PREVIEW_LINE_LIMIT: usize = 30;

/// Maximum editable file size — byte-faithful to claude-code `FileEditTool`
/// `validateInput` constant `DYa = 1073741824` (1 GiB; binary offset
/// 202620510). A file whose on-disk size is *strictly greater* than this is
/// rejected (`errorCode: 10`) before its body is read, with the
/// [`MAX_EDIT_FILE_SIZE`]-derived "File is too large to edit" message.
pub const MAX_EDIT_FILE_SIZE: u64 = 1_073_741_824;

/// Owner-write permission bit (`S_IWUSR` == `0o200` == `128`) — the binary's
/// `e & 128` test in `H7e`. The Perforce read-only guard fires when this bit is
/// *unset* (the file is not owner-writable).
const OWNER_WRITE_BIT: u32 = 0o200;

/// Byte-locked Perforce read-only message — claude-code `validateInput`
/// `errorCode: 11` string `k7e` (binary offset 193229321). The `—` is a real
/// em-dash (`U+2014`), matching the binary's `—`.
pub const PERFORCE_READ_ONLY_MESSAGE: &str = "File is read-only — it has not been opened for edit in Perforce. Run `p4 edit <file>` to check it out, then retry. Do not chmod the file writable; that bypasses Perforce tracking.";

/// Byte-locked escape-swap note appended to the string-not-found message when
/// `pUa(old_string)` holds (the lookup tried the `\uXXXX`-escape-swap /
/// non-ASCII fallbacks and still missed). Binary offset 202624182:
/// `g = pUa(r) ? "\n(note: Edit also tried swapping \\uXXXX escapes …)" : ""`.
/// Leading `\n` is part of the literal.
pub const ESCAPE_SWAP_NOTE: &str = "\n(note: Edit also tried swapping \\uXXXX escapes and their characters; neither form matched, so the mismatch is likely elsewhere in old_string. Re-read the file and copy the exact surrounding text.)";

/// `H7e(_)` (binary offset 193220147): `cfr() && (mode & 128) === 0`.
///
/// Returns `true` when Perforce mode is enabled (`cfr()` ==
/// `is_env_truthy(LINGXI_PERFORCE_MODE)`) AND the file's owner-write bit
/// ([`OWNER_WRITE_BIT`]) is unset — a read-only file that has not been checked
/// out via `p4 edit`. On non-Unix targets the POSIX mode is unavailable, so the
/// guard never fires (the binary's `mode & 128` is a POSIX concept).
fn is_perforce_read_only(mode: Option<u32>) -> bool {
    if !is_perforce_mode_enabled() {
        return false;
    }
    #[cfg(unix)]
    {
        mode.is_some_and(|mode| (mode & OWNER_WRITE_BIT) == 0)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// `cfr()` (binary offset 193219993): `st(process.env.LINGXI_PERFORCE_MODE)`
/// — env-truthiness (`1`/`true`/`yes`/`on`) of `LINGXI_PERFORCE_MODE`,
/// mirrored by [`lingxi_core::host::env::is_env_truthy`].
fn is_perforce_mode_enabled() -> bool {
    lingxi_core::host::env::is_env_truthy(std::env::var("LINGXI_PERFORCE_MODE").ok().as_deref())
}

/// Build the byte-locked patch-truncation suffix with `n` substituted.
#[must_use]
pub fn patch_truncation_suffix(n: usize) -> String {
    PATCH_TRUNCATION_SUFFIX_TEMPLATE.replace("{N}", &n.to_string())
}

/// Build the model-facing `tool_result` message for an Edit, byte-faithful to
/// claude-code `FileEditTool.mapToolResultToToolResultBlockParam`
/// (`FileEditTool.ts:575-594`).
///
/// `path` is the ORIGINAL `file_path` input string (claude-code echoes the
/// caller's path verbatim, not a canonicalized form).
///
/// claude-code's `xYa` suffix has THREE branches:
/// `staleRecovered ? "<modified-on-disk note>" : (userModified ? "" : Pyn)`.
/// - `staleRecovered` — the `tengu_cedar_sundial` stale-recovery path (see
///   [`stale_edit_applies`]) — inserts [`STALE_RECOVERED_NOTE`].
/// - `userModified` (interactive-only) — which inserts `".  The user modified your
///   proposed changes before accepting them. "` — requires a human-in-the-loop
///   accept step that does not exist here; documented-out rather than dead-coded
///   (R-F5 / R-V1 disposition).
/// So the suffix is [`STALE_RECOVERED_NOTE`] on a recovered edit, else `Pyn`
/// (the current-state suffix).
#[must_use]
pub fn edit_result_message(path: &str, replace_all: bool, stale_recovered: bool) -> String {
    let base = if replace_all {
        format!("The file {path} has been updated. All occurrences were successfully replaced.")
    } else {
        format!("The file {path} has been updated successfully.")
    };
    let suffix = if stale_recovered {
        STALE_RECOVERED_NOTE
    } else {
        crate::FILE_STATE_CURRENT_SUFFIX
    };
    format!("{base}{suffix}")
}

/// The suffix appended to a stale-recovered edit's success message (claude's
/// `a = i ? <this note> : …` in the Edit result mapper; `—` = em-dash).
pub const STALE_RECOVERED_NOTE: &str = " (note: the file had been modified on disk since you last read it \u{2014} the edit applied cleanly, but the file contains other changes not in your context. Read it before edits that depend on surrounding content.)";

/// Stale-recovery test: an edit whose file changed on disk since the last Read
/// may still proceed when it "applies" cleanly to the CURRENT content — oracle
/// `GKe` (was `ZVi`): a non-empty `old_string` found via the quote-normalizing
/// matcher (`Ytt` = [`crate::quotes::find_actual_string`]), unique unless
/// `replace_all`. Anything else keeps the stale error. That much is ported and
/// tested below.
///
/// `tengu_cedar_sundial` GRADUATED — it appears nowhere in the 2.1.270 binary,
/// and `Mjs` calls `GKe(...)==="applies"` with no flag. This is that pure
/// predicate. The second half of `Mjs`'s condition — `&& !readNotAutoAllowed()`,
/// oracle `kq` — is applied at the CALL SITE, where the path is in hand; see
/// [`lingxi_core::host::read_auto_allow`].
///
/// Keeping them separate matters: `GKe` asks whether the edit still applies,
/// `kq` whether the model was ever allowed to READ the file. Folded together,
/// a failing test could not say which question was answered wrongly.
#[must_use]
pub fn stale_edit_applies(content: &str, old_string: &str, replace_all: bool) -> bool {
    if old_string.is_empty() {
        return false; // ZVi: "" → no_match
    }
    let Some(actual) = crate::quotes::find_actual_string(content, old_string) else {
        return false; // ZVi: !Ytt → no_match
    };
    if !replace_all {
        // ZVi: a second occurrence ⇒ ambiguous.
        if let Some(first) = content.find(actual.as_str()) {
            if content[first + actual.len()..].contains(actual.as_str()) {
                return false;
            }
        }
    }
    true // "applies"
}

/// Normalize claude-code's accepted Edit input aliases to the canonical keys
/// (claude-code coerces these — the `tengu_tool_input_coerced` path): `path` →
/// `file_path`, `old_str` → `old_string`, `new_str` → `new_string`,
/// `replace_name` → `replace_all` (truthy when the value is `true` or `"true"`).
/// Only fills a canonical key when it is ABSENT, so an explicit canonical wins.
fn normalize_edit_aliases(input: &mut Value) {
    let Some(obj) = input.as_object_mut() else {
        return;
    };
    for (alias, canonical) in [
        ("path", "file_path"),
        ("old_str", "old_string"),
        ("new_str", "new_string"),
    ] {
        if !obj.contains_key(canonical) {
            if let Some(v) = obj.get(alias).cloned() {
                obj.insert(canonical.to_string(), v);
            }
        }
    }
    if !obj.contains_key("replace_all") {
        if let Some(v) = obj.get("replace_name") {
            let truthy = v.as_bool() == Some(true) || v.as_str() == Some("true");
            obj.insert("replace_all".to_string(), Value::Bool(truthy));
        }
    }
}

/// `FileEditTool` — literal-search replacement in a UTF-8 file.
pub struct FileEditTool {
    ctx: BuiltinToolContext,
}

fn edit_rooted_snapshot(
    requested: &std::path::Path,
    approved: &std::path::Path,
    trusted_dirs: &[std::path::PathBuf],
) -> Result<
    lingxi_core::host::rooted_fs::RootedFileSnapshot,
    lingxi_core::host::rooted_fs::RootedFsError,
> {
    let Some((root, relative)) = crate::shared::rooted_location(approved, trusted_dirs) else {
        return Err(lingxi_core::host::rooted_fs::RootedFsError::Fs(
            lingxi_core::host::FsError::OutsideWorkspace(approved.display().to_string()),
        ));
    };
    lingxi_core::host::rooted_fs::read_file_after_permission(&root, &relative, requested, approved)
}

fn edit_resolution_error(
    path: &str,
    error: lingxi_core::host::rooted_fs::RootedFsError,
) -> ToolError {
    match error {
        lingxi_core::host::rooted_fs::RootedFsError::LeafSymlink => ToolError::InvalidInput(format!(
            "Refusing to write {path}: it is a symbolic link. Write to the link's target path instead."
        )),
        lingxi_core::host::rooted_fs::RootedFsError::ParentSymlinkResolutionChanged => {
            ToolError::InvalidInput(format!(
                "Refusing to write {path}: its parent-directory symlink resolution changed after permission was checked."
            ))
        }
        lingxi_core::host::rooted_fs::RootedFsError::SymlinkResolutionChanged => {
            if std::fs::symlink_metadata(path)
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(false)
            {
                ToolError::InvalidInput(format!(
                    "Refusing to write {path}: it is a symbolic link. Write to the link's target path instead."
                ))
            } else {
                ToolError::InvalidInput(format!(
                    "Refusing to write {path}: its parent-directory symlink resolution changed after permission was checked."
                ))
            }
        }
        lingxi_core::host::rooted_fs::RootedFsError::NotRegularFile => {
            ToolError::Io(format!("File {path} is not a regular file"))
        }
        lingxi_core::host::rooted_fs::RootedFsError::Fs(error) => ToolError::Io(error.to_string()),
    }
}

impl FileEditTool {
    /// Construct a new tool.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }

    async fn emit_started(&self, invocation_id: &str, path: &std::path::Path) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".to_string(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert(
            "_PROTO_file_path".to_string(),
            AnalyticsValue::String(
                PiiTagged::assert_pii_tagged_column(path.display().to_string()).into_inner(),
            ),
        );
        self.ctx.bus.log_event(EDIT_STARTED, md).await;
    }

    async fn emit_completed(&self, invocation_id: &str, replacements: u32, duration_ms: u64) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".to_string(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert(
            "replacements".to_string(),
            AnalyticsValue::Int(replacements as i64),
        );
        md.insert(
            "duration_ms".to_string(),
            AnalyticsValue::Int(duration_ms as i64),
        );
        self.ctx.bus.log_event(EDIT_COMPLETED, md).await;
    }

    async fn emit_failed(&self, invocation_id: &str, kind: &str) {
        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "invocation_id".to_string(),
            AnalyticsValue::String(Verified::assert_safe(invocation_id.to_string()).into_inner()),
        );
        md.insert(
            "failure_kind".to_string(),
            AnalyticsValue::String(Verified::assert_safe(kind.to_string()).into_inner()),
        );
        self.ctx.bus.log_event(EDIT_FAILED, md).await;
    }

    /// Build a `+`/`-`/` ` diff preview, capped at `PATCH_PREVIEW_LINE_LIMIT`
    /// lines. If truncation occurs, appends the byte-locked suffix.
    pub(crate) fn build_patch_preview(before: &str, after: &str) -> String {
        let before_lines: Vec<&str> = before.lines().collect();
        let after_lines: Vec<&str> = after.lines().collect();
        let mut diff: Vec<String> = Vec::new();
        let max = before_lines.len().max(after_lines.len());
        for i in 0..max {
            match (before_lines.get(i), after_lines.get(i)) {
                (Some(b), Some(a)) if b == a => diff.push(format!(" {b}")),
                (Some(b), Some(a)) => {
                    diff.push(format!("-{b}"));
                    diff.push(format!("+{a}"));
                }
                (Some(b), None) => diff.push(format!("-{b}")),
                (None, Some(a)) => diff.push(format!("+{a}")),
                (None, None) => {}
            }
        }
        if diff.len() <= PATCH_PREVIEW_LINE_LIMIT {
            return diff.join("\n");
        }
        let kept: Vec<String> = diff
            .iter()
            .take(PATCH_PREVIEW_LINE_LIMIT)
            .cloned()
            .collect();
        let elided = diff.len() - PATCH_PREVIEW_LINE_LIMIT;
        let mut out = kept.join("\n");
        out.push_str(&patch_truncation_suffix(elided));
        out
    }
}

static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["file_path", "old_string", "new_string"],
        "properties": {
            "file_path":   { "type": "string", "description": "The absolute path to the file to modify" },
            "old_string":  { "type": "string", "description": "The text to replace" },
            "new_string":  { "type": "string", "description": "The text to replace it with (must be different from old_string)" },
            "replace_all": { "type": "boolean", "default": false, "description": "Replace all occurrences of old_string (default false)" }
        }
    })
});

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified against the
    /// binary, 2 hits).
    fn search_hint(&self) -> Option<&str> {
        Some("modify file contents in place")
    }
    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH
    }
    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _input: &Value) -> bool {
        false
    }
    fn is_destructive(&self, _input: &Value) -> bool {
        true
    }

    async fn check_permissions(&self, _input: &Value, _ctx: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "allow-all-gate (M4-01 default)".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _input: &Value, _opts: &DescriptionOptions) -> String {
        // R-EditDesc: claude-code's Edit `description()` is the short display label
        // `"A tool for editing files"` (binary), NOT the long body — the long body
        // lives in `prompt()`. The model-facing wire uses `prompt()` (wire.rs:140),
        // so `description()` is a display label only; this keeps it 1:1 anyway.
        "A tool for editing files".to_string()
    }

    async fn prompt(&self, opts: &PromptOptions) -> String {
        // Model-gated, mirroring claude-code `prompt({model:e}){return RBp(e)}`
        // where `RBp(e){if(Dh(e))return SHORT; return LONG}` (binary offset
        // 202614289). Predicate shared with TodoWrite via `tool_api`.
        // NOTE: claude-code's Edit `description()` is the literal "A tool for
        // editing files" (not model-gated, not this body); LingXi returns
        // EDIT_DESCRIPTION there — a pre-existing divergence left untouched
        // (out of scope for this prompt-gate fix).
        if tool_api::dh_simple_system_prompt(opts.model.as_deref()) {
            EDIT_PROMPT_SHORT.to_string()
        } else {
            EDIT_DESCRIPTION.to_string()
        }
    }

    fn get_path(&self, input: &Value) -> Option<PathBuf> {
        input
            .get("file_path")
            .or_else(|| input.get("path"))
            .and_then(Value::as_str)
            .map(PathBuf::from)
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        // Coerce claude-code's input aliases (path/old_str/new_str/replace_name)
        // to canonical keys before reading them.
        let mut input = input;
        normalize_edit_aliases(&mut input);
        let invocation_id = tool_api::util::ids::ulid_or_uuid();
        let file_path = input
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("file_path is required".into()))?;
        let old_string = input
            .get("old_string")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("old_string is required".into()))?;
        let new_string = input
            .get("new_string")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidInput("new_string is required".into()))?;
        let replace_all = input
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // No-op edit: identical strings (claude-code validateInput, errorCode 1).
        if old_string == new_string {
            self.emit_failed(&invocation_id, "no_change").await;
            return Err(ToolError::InvalidInput(
                "No changes to make: old_string and new_string are exactly the same.".into(),
            ));
        }

        // Permission-deny-directory gate (claude-code `validateInput` errorCode
        // 2, binary offset 202622179):
        //   if(KH(i,Fr(t),"edit","deny")!==null)
        //     return{result:!1,behavior:"ask",
        //       message:"File is in a directory that is denied by your
        //                permission settings.",errorCode:2};
        // `KH(path, Fr(ctx), "edit", "deny")` walks the `toolPermissionContext`
        // (`Fr` folds the agent's permission layers over `getAppState().
        // toolPermissionContext`) for a matching deny rule. RESIDUAL: LingXi's
        // `ToolUseContext` carries NO permission handle (there is no
        // `toolPermissionContext` / permission-layer field — see
        // `tool-api/src/context.rs`), so this deny lookup cannot be performed
        // from `call()`. Deny enforcement instead lives in the permission
        // gate/policy layer (`permission/src/policy.rs`, currently inert by
        // default), which runs ahead of the tool. When that subsystem is
        // activated and threaded into the tool context, the byte-exact reject
        // above belongs here, ahead of the UNC early-allow and the body read.

        // UNC / network-path early-allow (claude-code `validateInput`, binary
        // offset 202622179, immediately after the deny check):
        //   if(i.startsWith("\\\\")||i.startsWith("//"))return{result:!0};
        // A path beginning with a Windows UNC prefix (`\\server\share`) or a
        // POSIX double-slash (`//host/share`) short-circuits `validateInput`
        // with `{result:!0}` — bypassing the `stat`-based size cap (errorCode
        // 10) and the Perforce read-only guard (errorCode 11) that follow. Both
        // run against a `stat` the binary skips for these paths, so we mirror
        // that by skipping the size-cap + Perforce gates below when `is_unc`.
        // The edit itself still proceeds (validateInput passing == allow). The
        // check is on the ORIGINAL `file_path` (`i`), before any canonical/
        // sandbox normalization.
        let is_unc = file_path.starts_with("\\\\") || file_path.starts_with("//");

        // Worktree parity plan (Task 3): a RELATIVE `file_path` resolves
        // against the CURRENT session cwd (`ctx.cwd()`, switchable by
        // `EnterWorktree`/`ExitWorktree`), not the frozen OS process cwd that
        // `std::fs::canonicalize` would otherwise consult below. An absolute
        // `file_path` (the documented/expected case) is unaffected.
        let requested_path = crate::normalize_model_file_path(file_path, &self.ctx.cwd());
        // Mobile-linux guest paths: rewrite onto the host-backed twin (or
        // refuse fenced guest space) BEFORE canonicalization/containment, so a
        // guest path validates as the host directory that actually backs it.
        // Desktop filesystems translate nothing and this is a no-op.
        let path = match translate_model_path(&self.ctx.fs, requested_path.clone(), true) {
            Ok(path) => path,
            Err(message) => {
                self.emit_failed(&invocation_id, "path_blocked").await;
                return Err(ToolError::InvalidInput(message));
            }
        };
        let started = Instant::now();
        self.emit_started(&invocation_id, &path).await;

        // `canonicalize_and_validate` tolerates a nonexistent target (it
        // canonicalizes the parent) so an empty `old_string` can create a file.
        let mut trusted_dirs = self.ctx.trusted_dirs();
        if let Some(root) =
            lingxi_core::host::teammate_plan::own_plan_file_root(ctx.agent_id.as_ref(), &path)
        {
            trusted_dirs.push(root);
        }
        let canon = match canonicalize_and_validate(&path, &trusted_dirs) {
            Ok(p) => p,
            Err(_) => {
                emit_blocked_event(&self.ctx.bus, TOOL_NAME, &path).await;
                self.emit_failed(&invocation_id, "path_blocked").await;
                return Err(ToolError::PathBlocked { path });
            }
        };

        // Read the approved target once through a fixed rooted handle. The
        // resulting bytes, size, mtime, and mode feed both validation gates and
        // the edit itself; no later pathname reopen can cross a symlink swap.
        let initial_snapshot = match edit_rooted_snapshot(&path, &canon, &trusted_dirs) {
            Ok(snapshot) => Some(snapshot),
            Err(lingxi_core::host::rooted_fs::RootedFsError::Fs(
                lingxi_core::host::FsError::NotFound(_),
            )) => None,
            Err(error) => {
                self.emit_failed(&invocation_id, "io_read").await;
                return Err(edit_resolution_error(file_path, error));
            }
        };

        // Stat-based gates (claude-code `validateInput`, binary offset
        // 202622179) are evaluated from the same rooted snapshot as the body
        // read. Missing-file creation is still allowed, and UNC paths retain
        // their upstream early-allow behavior by skipping these gates.
        if !is_unc {
            if let Some(snapshot) = initial_snapshot.as_ref() {
                if snapshot.size > MAX_EDIT_FILE_SIZE {
                    self.emit_failed(&invocation_id, "file_too_large").await;
                    return Err(ToolError::InvalidInput(format!(
                        "File is too large to edit ({}). Maximum editable file size is {}.",
                        crate::read::format_file_size(snapshot.size),
                        crate::read::format_file_size(MAX_EDIT_FILE_SIZE),
                    )));
                }
                if is_perforce_read_only(snapshot.mode) {
                    self.emit_failed(&invocation_id, "perforce_read_only").await;
                    return Err(ToolError::InvalidInput(PERFORCE_READ_ONLY_MESSAGE.into()));
                }
            }
        }

        // Distinguish "does not exist" from a real read error so an empty
        // `old_string` can mean new-file creation (claude-code FileEditTool).
        //
        // Read the raw bytes and detect encoding (UTF-16LE via BOM, else UTF-8)
        // and line endings, then present an LF-normalized in-memory `content`
        // for matching — byte-faithful to claude-code `readFileForEdit` →
        // `readFileSyncWithMetadata` (FileEditTool.ts:202-221, 444-449). The
        // detected encoding/line-ending are fed back into `write_with_metadata`
        // so a CRLF or UTF-16LE file round-trips without corruption and an
        // `old_string` spanning a line break matches against the LF view.
        // Read the raw bytes once; also capture the *current* mtime (floored to
        // ms) and a raw-UTF-8 decode that matches how the `Read` tool stores
        // its content, so the read-before-write staleness guard (Batch F) can
        // compare like with like.
        #[allow(clippy::type_complexity)]
        let existing: Option<(
            String,
            crate::file_meta::Encoding,
            crate::file_meta::LineEnding,
            i64,
            String,
        )> = match initial_snapshot {
            Some(snapshot) => {
                let bytes = snapshot.bytes;
                // Current mtime (floored ms) comes from the same opened handle
                // as the bytes; `None` falls back to epoch `0`.
                let mtime_ms = snapshot
                    .modified
                    .map(tool_api::read_file_state::mtime_ms_floor)
                    .unwrap_or(0);
                // Raw (non-LF-normalized) UTF-8 decode — the exact form the
                // `Read` tool records into `read_file_state`. Used only for the
                // content-equality fallback; falls back to the LF view if the
                // file is not strict UTF-8 (e.g. UTF-16LE), in which case the
                // mtime check alone governs.
                let raw_decoded = crate::shared::decode_utf8_strict(&bytes).ok();
                let (content, enc, ending) = crate::file_meta::read_with_metadata(&bytes);
                let raw_for_cmp = raw_decoded.unwrap_or_else(|| content.clone());
                Some((content, enc, ending, mtime_ms, raw_for_cmp))
            }
            None => None,
        };

        // New files are created as UTF-8/LF (TS `readFileForEdit` ENOENT
        // branch returns encoding `utf8`, lineEndings `LF`). `existing` carries
        // the LF-normalized content plus the staleness-guard inputs (mtime +
        // raw-decoded current content) when the file already exists.
        let (existing, enc, ending, guard_mtime_ms, guard_raw_content) = match existing {
            None => (
                None,
                crate::file_meta::Encoding::Utf8,
                crate::file_meta::LineEnding::Lf,
                0i64,
                String::new(),
            ),
            Some((content, enc, ending, mtime_ms, raw_for_cmp)) => {
                (Some(content), enc, ending, mtime_ms, raw_for_cmp)
            }
        };

        // `staleRecovered` (claude `xTg` → `D`): set when the flag-gated
        // stale-recovery lets a stale-but-cleanly-applying edit proceed.
        let mut stale_recovered = false;
        let (before, after, replacements): (String, String, u32) = match existing {
            // File does not exist.
            None => {
                if !old_string.is_empty() {
                    self.emit_failed(&invocation_id, "file_not_found").await;
                    // Byte-locked `FileEditTool.validateInput` errorCode-4 arm
                    // (claude-code 2.1.238): the SAME message Read's ENOENT arm
                    // builds — `File does not exist. Note: your current working
                    // directory is {cwd}.` plus the optional
                    // `" Did you mean {x}?"` suffix (corrected-path suggestion
                    // preferred over the same-stem sibling).
                    return Err(ToolError::InvalidInput(
                        crate::read::file_not_found_message(&canon, &self.ctx.cwd()),
                    ));
                }
                // Empty `old_string` on a nonexistent file → create it.
                (String::new(), new_string.to_string(), 1)
            }
            // File exists.
            Some(before) => {
                // Read-before-write staleness guard (Batch F). The file exists,
                // so a prior full `Read` is required and the file must not have
                // changed on disk since (TS `FileEditTool.ts:275-311` validate +
                // `:451-468` call-time re-check). New-file creation hits the
                // `None` arm above and skips this entirely (TS ENOENT →
                // `result:true`). `guard_raw_content` is the current on-disk
                // content decoded the same way `Read` records it, for the
                // full-read content-equality fallback.
                if let Err(e) = crate::check_read_before_write(
                    &self.ctx.read_file_state,
                    &requested_path,
                    guard_mtime_ms,
                    &guard_raw_content,
                    crate::read_requirement_waived(Some(&ctx.options.main_loop_model), &canon),
                ) {
                    // Stale-recovery (claude `xTg`'s `TEu(ZVi(...))` branch,
                    // flag-gated `tengu_cedar_sundial`, default-OFF): only the
                    // CONTENT-CHANGED error is recoverable — a never-read
                    // failure (no read-state entry at all) always propagates
                    // (`xTg` throws Y2n before the recovery check). Since
                    // 2.1.212 a ranged offset/limit read is NOT a distinct
                    // failure: it either passes the guard or surfaces as the
                    // recoverable CONTENT-CHANGED error. When the edit still
                    // applies cleanly to the CURRENT content, proceed and mark
                    // the result `staleRecovered`.
                    // FT-07: the guard now reports the oracle's validateInput
                    // staleness literal (errorCode 7), so the recoverable-error
                    // discriminator keys on THAT constant. A never-read failure
                    // still carries FILE_NOT_READ_ERROR and stays unrecoverable.
                    let is_stale_error = matches!(
                        &e,
                        tool_api::tool_trait::ToolError::InvalidInput(m)
                            if m == crate::FILE_UNEXPECTEDLY_MODIFIED_ERROR
                    );
                    // `Mjs`: `GKe(...)==="applies" && !readNotAutoAllowed()`.
                    // `kq` asks whether the model could ever have READ this
                    // file — it holds a reader, and the path is readable under
                    // the policy. Without it an Edit could recover a stale read
                    // on a file the model was never allowed to see, i.e. a write
                    // landing on unseen content. An un-wired host answers
                    // `false` and simply keeps the stale error.
                    if is_stale_error
                        && lingxi_core::host::read_auto_allow::read_auto_allowed(
                            &canon.to_string_lossy(),
                        )
                        && stale_edit_applies(&before, old_string, replace_all)
                    {
                        stale_recovered = true;
                    } else {
                        self.emit_failed(&invocation_id, "stale_read").await;
                        return Err(e);
                    }
                }

                if old_string.is_empty() {
                    // Empty `old_string` is only valid for an (effectively) empty
                    // file — otherwise it's a creation attempt on existing content.
                    if !before.trim().is_empty() {
                        self.emit_failed(&invocation_id, "file_exists").await;
                        return Err(ToolError::InvalidInput(
                            "Cannot create new file - file already exists.".into(),
                        ));
                    }
                    (before, new_string.to_string(), 1)
                } else {
                    // Edit must not corrupt notebooks — route to NotebookEdit.
                    if std::path::Path::new(file_path)
                        .extension()
                        .is_some_and(|e| e == "ipynb")
                    {
                        self.emit_failed(&invocation_id, "ipynb").await;
                        return Err(ToolError::InvalidInput(
                            "File is a Jupyter Notebook. Use the NotebookEdit to edit this file."
                                .into(),
                        ));
                    }
                    // Curly-quote normalization (Batch E): when the file uses
                    // typographic (curly) quotes but the model sent straight
                    // quotes, recover the actual curly text from the file so the
                    // match still locates its target, then re-apply the file's
                    // curly style to `new_string` so the rewrite preserves the
                    // typography. Match/count on `actual_old` and `actual_new`
                    // (claude-code FileEditTool.ts:316,471-479). `before` is the
                    // LF-normalized in-memory view from Batch D, so all curly
                    // matching happens against that normalized content.
                    // `find_actual_string` == binary `vIe(before, old_string)`:
                    // exact → curly-normalize → `\uXXXX`-escape-swap → non-ASCII
                    // escaped-form regex. It returns `None` (TS `vIe` → `null`)
                    // only when ALL four lookups miss; the binary then falls back
                    // to the raw `old_string` (`vIe(...)||t`). We mirror that with
                    // `unwrap_or_else` — when `vIe` missed, `actual_old` ==
                    // `old_string`, which (since `vIe` step 1 already found
                    // `before` does NOT contain `old_string`) yields `count == 0`
                    // and routes to the not-found branch, exactly as the binary's
                    // `if(!f){…errorCode 8…}` does.
                    let actual_old = crate::quotes::find_actual_string(&before, old_string)
                        .unwrap_or_else(|| old_string.to_string());
                    let actual_new =
                        crate::quotes::preserve_quote_style(old_string, &actual_old, new_string);
                    let count = before.matches(actual_old.as_str()).count();
                    if count == 0 {
                        self.emit_failed(&invocation_id, "no_match").await;
                        // Byte-locked string-to-replace-not-found message
                        // (binary offset 202624182). Echoes the ORIGINAL
                        // `old_string` input (TS `${r}`), not `actual_old`.
                        //
                        // The escape-swap note is appended iff `pUa(old_string)`
                        // (== `has_escape_or_non_ascii`) — i.e. the lookup *did*
                        // try the `\uXXXX`/non-ASCII fallbacks and still missed.
                        // The binary builds:
                        //   g = pUa(r) ? "\n(note: ...)" : "";
                        //   message: `String to replace not found in file.
                        //   String: ${r}${g}`
                        let note = if crate::quotes::has_escape_or_non_ascii(old_string) {
                            ESCAPE_SWAP_NOTE
                        } else {
                            ""
                        };
                        return Err(ToolError::InvalidInput(format!(
                            "String to replace not found in file.\nString: {old_string}{note}"
                        )));
                    }
                    if !replace_all && count > 1 {
                        self.emit_failed(&invocation_id, "ambiguous_match").await;
                        // Byte-locked multiple-match message (FileEditTool.ts:336).
                        // `count` == TS `matches`; trailing `String:` echoes the
                        // ORIGINAL `old_string` input (TS `${old_string}`).
                        return Err(ToolError::InvalidInput(format!(
                            "Found {count} matches of the string to replace, but replace_all is false. To replace all occurrences, set replace_all to true. To replace only one occurrence, please provide more context to uniquely identify the instance.\nString: {old_string}"
                        )));
                    }
                    // Apply the replacement, byte-faithful to claude-code
                    // `AUa` (binary offset 201328322):
                    //   if new_string !== "" -> replace old_string verbatim;
                    //   else (a pure DELETION) and old_string does NOT already
                    //   end with "\n" and the file contains `old_string + "\n"`,
                    //   replace `old_string + "\n"` instead — so deleting a line
                    //   also consumes its trailing newline and leaves no blank
                    //   line behind. The +\n logic runs on the curly-normalized
                    //   `actual_old`/`actual_new` (== TS `D`/`N` post-`vIe`).
                    let search_target = if actual_new.is_empty()
                        && !actual_old.ends_with('\n')
                        && before.contains(&format!("{actual_old}\n"))
                    {
                        format!("{actual_old}\n")
                    } else {
                        actual_old.clone()
                    };
                    let after = if replace_all {
                        before.replace(search_target.as_str(), &actual_new)
                    } else {
                        before.replacen(search_target.as_str(), &actual_new, 1)
                    };
                    let replacements = if replace_all { count as u32 } else { 1 };
                    (before, after, replacements)
                }
            }
        };

        // (/rewind) Back up the pre-edit content BEFORE writing so /rewind can
        // restore it. Best-effort; no-op when checkpointing isn't wired.
        if let Some(fh) = ctx.file_history.as_ref() {
            fh.track_edit(&canon.to_string_lossy()).await;
        }

        // Re-apply the original encoding + line endings on write so a CRLF or
        // UTF-16LE file round-trips byte-for-byte (claude-code
        // `writeTextContent`, file.ts:84-98). `after` is LF-normalized.
        let bytes = crate::file_meta::encode_with_metadata(&after, enc, ending);
        let Some((root, relative)) = crate::shared::rooted_location(&canon, &trusted_dirs) else {
            self.emit_failed(&invocation_id, "path_blocked").await;
            return Err(ToolError::PathBlocked { path });
        };
        let write_result = match lingxi_core::host::rooted_fs::write_file_after_permission(
            &root, &relative, &path, &canon, &bytes,
        ) {
            Ok(result) => result,
            Err(error) => {
                self.emit_failed(&invocation_id, "io_write").await;
                return Err(edit_resolution_error(file_path, error));
            }
        };

        // Post-write: update the read-state registry so an immediate second
        // Edit/Write succeeds and Read-dedup sees the new mtime (TS
        // `FileEditTool.ts:519-525` `readFileState.set({content: updatedFile,
        // timestamp: <new mtime>, offset: undefined, limit: undefined})`).
        // `after` is the LF-normalized written content (TS stores the same
        // LF-normalized `updatedFile`); offset/limit cleared so the next read
        // counts as a full read.
        let new_mtime_ms = write_result
            .modified
            .map_or(0, tool_api::read_file_state::mtime_ms_floor);
        tool_api::read_file_state::set_with_requested_aliases(
            &self.ctx.read_file_state,
            requested_path.clone(),
            tool_api::read_file_state::ReadFileEntry {
                content: after.clone(),
                mtime_ms: new_mtime_ms,
                offset: None,
                limit: None,
                // Post-edit entry — not a Read; the dedup gate skips it.
                from_read: false,
                seeded_from_context: false,
                is_partial_view: false,
            },
            true,
            vec![requested_path.clone(), path.clone()],
        );

        // Keep an installed language server synchronized immediately after a
        // successful edit so passive diagnostics can arrive before the next
        // model turn. Missing/unhealthy servers are best-effort and never turn
        // a completed file edit into a failure.
        self.ctx.sync_lsp_after_file_write(&canon, &after).await;

        let structured_patch = crate::structured_patch::build_structured_patch(&before, &after);
        let (lines_added, lines_removed) = count_patch_lines(&structured_patch);
        if lines_added > 0 || lines_removed > 0 {
            // The oracle records the MODEL that made the change, not the tool.
            otel::record_lines_of_code_change(
                &ctx.options.main_loop_model,
                lines_added,
                lines_removed,
            );
        }

        let duration_ms = started.elapsed().as_millis() as u64;
        self.emit_completed(&invocation_id, replacements, duration_ms)
            .await;

        // Model-facing result string is byte-faithful to claude-code
        // (`FileEditTool.ts:575-594`); it echoes the ORIGINAL `file_path` arg,
        // not the canonicalized path. Batch A's serialization rule emits
        // `data["content"]` verbatim to the model; `replacements` /
        // `patch_preview` remain for the TUI diff render only.
        let content = edit_result_message(file_path, replace_all, stale_recovered);

        // claude-code FileEditTool result data (2.1.191): {filePath, oldString,
        // newString, originalFile, structuredPatch, userModified, replaceAll}
        // (preserve_order). The model-facing message rides on `model_content`.
        // `structuredPatch` is the +/-/space preview string — LingXi's existing
        // structure, SAME as the already-merged Write tool; the binary's npm-`diff`
        // hunk array {oldStart,oldLines,newStart,newLines,lines} is a shared
        // residual (Write + Edit both emit the string preview). `userModified` is
        // false (no interactive human-accept step); `gitDiff` is OMITTED (no
        // git-diff capture); `staleRecovered` follows the binary's conditional
        // spread `...x&&{staleRecovered:!0}` — present only when true.
        let mut data = json!({
            "filePath": file_path,
            "oldString": old_string,
            "newString": new_string,
            "originalFile": before,
            "structuredPatch": structured_patch,
            "userModified": false,
            "replaceAll": replace_all,
        });
        if stale_recovered {
            data["staleRecovered"] = json!(true);
        }
        Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
            data,
            model_content: Some(content),
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

#[cfg(test)]
#[path = "edit_test.rs"]
mod edit_test;
