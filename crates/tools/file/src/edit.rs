//! `FileEditTool` — literal-search-and-replace inside a UTF-8 file.
//!
//! Wire-locked semantics (no regex by default — claude-code parity):
//! - `replace_all: false` (default): EXACTLY ONE match of `old_string` must
//!   appear in the file; otherwise reject.
//! - `replace_all: true`: replace every occurrence via `String::replace`.
//! - Patch preview truncation suffix template: `"\n\n... [{N} lines truncated] ..."`
//!   (spec §7; `{N}` is the literal placeholder substituted via `replace`).

use crate::Utf16JsonProjection;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::json;
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
const EDIT_DESCRIPTION: &str = include_str!("templates/claude-code-2.1.319/edit-long.md");

/// The SHORT Edit prompt — byte-locked VERBATIM to claude-code `RBp(e)`'s
/// `Dh(e)===true` branch (binary offset 202614310), served to current-gen
/// default models. `${Ws}`=Read; the line-prefix slot uses `t=N$e()`
/// (`tengu_tab_read_sep`, defaults false) ⇒ `"line number + tab"` (matching the
/// LONG body's `${n}`). The "fails otherwise" em-dash is U+2014.
const EDIT_PROMPT_SHORT: &str = include_str!("templates/claude-code-2.1.319/edit-lean.md");

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
    edit_result_message_with_context(path, replace_all, stale_recovered, false)
}

fn edit_result_message_with_context(
    path: &str,
    replace_all: bool,
    stale_recovered: bool,
    content_not_in_model_context: bool,
) -> String {
    let base = if replace_all {
        format!("The file {path} has been updated. All occurrences were successfully replaced.")
    } else {
        format!("The file {path} has been updated successfully.")
    };
    let suffix = if stale_recovered {
        STALE_RECOVERED_NOTE
    } else if content_not_in_model_context {
        ""
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
    find_stale_edit_match(content, old_string, replace_all).is_some()
}

struct ResolvedEditMatch {
    actual_old: String,
    // A unique stale-recovery match has already been counted. Replace-all
    // defers counting until after the existing encoding/notebook checks.
    count: Option<usize>,
}

fn find_stale_edit_match(
    content: &str,
    old_string: &str,
    replace_all: bool,
) -> Option<ResolvedEditMatch> {
    if old_string.is_empty() {
        return None; // ZVi: "" → no_match
    }
    let actual_old = crate::quotes::find_actual_string(content, old_string)?;
    let count = if replace_all {
        None
    } else {
        // ZVi: a second occurrence ⇒ ambiguous.
        if let Some(first) = content.find(actual_old.as_str()) {
            if content[first + actual_old.len()..].contains(actual_old.as_str()) {
                return None;
            }
            Some(1)
        } else {
            Some(0)
        }
    };
    Some(ResolvedEditMatch { actual_old, count })
}

struct ResolvedUnitEditMatch {
    actual_old: Vec<u16>,
    count: Option<usize>,
}

fn find_stale_edit_match_units(
    content: &[u16],
    old_string: &[u16],
    replace_all: bool,
) -> Option<ResolvedUnitEditMatch> {
    if old_string.is_empty() {
        return None;
    }
    let actual_old = crate::quotes::find_actual_string_units(content, old_string)?;
    let count = if replace_all {
        None
    } else {
        let count = crate::utf16_mutation::count(content, &actual_old);
        if count > 1 {
            return None;
        }
        Some(count)
    };
    Some(ResolvedUnitEditMatch { actual_old, count })
}

/// Normalize claude-code's accepted Edit input aliases to the canonical keys
/// (claude-code coerces these — the `tengu_tool_input_coerced` path): `path` →
/// `file_path`, `old_str` → `old_string`, `new_str` → `new_string`,
/// `replace_name` → `replace_all` (truthy when the value is `true` or `"true"`).
/// Only fills a canonical key when it is ABSENT, so an explicit canonical wins.
fn normalize_edit_aliases(input: &mut Utf16JsonProjection) {
    for (alias, canonical) in [
        ("path", "file_path"),
        ("old_str", "old_string"),
        ("new_str", "new_string"),
    ] {
        if !input.contains_property(canonical) {
            if let Ok(value) = input.subprojection(&format!("/{alias}")) {
                let _ = input.set_field(canonical, value);
            }
        }
    }
    if !input.contains_property("replace_all") && input.contains_property("replace_name") {
        let truthy = input
            .get_property("replace_name")
            .is_some_and(|value| value.as_bool() == Some(true) || value.as_str() == Some("true"));
        let value = Utf16JsonProjection::parse(if truthy { "true" } else { "false" })
            .expect("static boolean JSON is valid");
        let _ = input.set_field("replace_all", value);
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
    max_bytes: Option<u64>,
    need_body: bool,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<
    lingxi_core::host::rooted_fs::RootedFileSnapshot,
    lingxi_core::host::rooted_fs::RootedFsError,
> {
    let Some((root, relative)) = crate::shared::rooted_location(approved, trusted_dirs) else {
        return Err(lingxi_core::host::rooted_fs::RootedFsError::Fs(
            lingxi_core::host::FsError::OutsideWorkspace(approved.display().to_string()),
        ));
    };
    use lingxi_core::host::rooted_fs::{RootedFileSnapshot, RootedFsError};
    use std::io::Read;
    let mut file = lingxi_core::host::rooted_fs::open_file_after_permission(
        &root, &relative, requested, approved,
    )?;
    let metadata = file
        .metadata()
        .map_err(|error| RootedFsError::Fs(lingxi_core::host::FsError::Io(error.to_string())))?;
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::MetadataExt;
        Some(metadata.mode())
    };
    #[cfg(not(unix))]
    let mode = None;
    let mut snapshot = RootedFileSnapshot {
        bytes: Vec::new(),
        size: metadata.len(),
        modified: metadata.modified().ok(),
        mode,
    };
    // Validate the native 1 GiB cap from this same handle before allocating
    // the body. A sparse oversized file must not be loaded just to reject it.
    if !need_body || max_bytes.is_some_and(|max| snapshot.size > max) {
        return Ok(snapshot);
    }
    let mut chunk = [0; 64 * 1024];
    loop {
        if cancel.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
            return Err(RootedFsError::Cancelled);
        }
        let remaining = max_bytes.map_or(chunk.len(), |max| {
            max.saturating_add(1)
                .saturating_sub(snapshot.bytes.len() as u64)
                .min(chunk.len() as u64) as usize
        });
        if remaining == 0 {
            break;
        }
        let length = file.read(&mut chunk[..remaining]).map_err(|error| {
            RootedFsError::Fs(lingxi_core::host::FsError::Io(error.to_string()))
        })?;
        if length == 0 {
            break;
        }
        snapshot.bytes.extend_from_slice(&chunk[..length]);
    }
    snapshot.size = snapshot.size.max(snapshot.bytes.len() as u64);
    Ok(snapshot)
}

fn edit_resolution_error(
    path: &str,
    error: lingxi_core::host::rooted_fs::RootedFsError,
) -> ToolError {
    match error {
        lingxi_core::host::rooted_fs::RootedFsError::ContentChanged => ToolError::InvalidInput(crate::FILE_CONTENT_CHANGED_LINTER_MESSAGE.into()),
        lingxi_core::host::rooted_fs::RootedFsError::Cancelled => ToolError::Aborted,
        lingxi_core::host::rooted_fs::RootedFsError::LeafSymlink => ToolError::InvalidInput(format!(
            "Refusing to write {path}: it is a symbolic link. Write to the link's target path instead."
        ).into()),
        lingxi_core::host::rooted_fs::RootedFsError::ParentSymlinkResolutionChanged => {
            ToolError::InvalidInput(format!(
                "Refusing to write {path}: its parent-directory symlink resolution changed after permission was checked."
            ).into())
        }
        lingxi_core::host::rooted_fs::RootedFsError::SymlinkResolutionChanged => {
            if std::fs::symlink_metadata(path)
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(false)
            {
                ToolError::InvalidInput(format!(
                    "Refusing to write {path}: it is a symbolic link. Write to the link's target path instead."
                ).into())
            } else {
                ToolError::InvalidInput(format!(
                    "Refusing to write {path}: its parent-directory symlink resolution changed after permission was checked."
                ).into())
            }
        }
        lingxi_core::host::rooted_fs::RootedFsError::NotRegularFile => {
            ToolError::Io(format!("File {path} is not a regular file").into())
        }
        lingxi_core::host::rooted_fs::RootedFsError::Fs(error) => ToolError::Io(error.to_string().into()),
    }
}

impl FileEditTool {
    /// Construct a new tool.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }

    async fn emit_started(&self, invocation_id: &str, path: &std::path::Path) {
        self.ctx
            .bus
            .log_event_lazy(EDIT_STARTED, || {
                let mut md: LogEventMetadata = HashMap::new();
                md.insert(
                    "invocation_id".to_string(),
                    AnalyticsValue::String(
                        Verified::assert_safe(invocation_id.to_string()).into_inner(),
                    ),
                );
                md.insert(
                    "_PROTO_file_path".to_string(),
                    AnalyticsValue::String(
                        PiiTagged::assert_pii_tagged_column(path.display().to_string())
                            .into_inner(),
                    ),
                );
                md
            })
            .await;
    }

    async fn emit_completed(&self, invocation_id: &str, replacements: u32, duration_ms: u64) {
        self.ctx
            .bus
            .log_event_lazy(EDIT_COMPLETED, || {
                let mut md: LogEventMetadata = HashMap::new();
                md.insert(
                    "invocation_id".to_string(),
                    AnalyticsValue::String(
                        Verified::assert_safe(invocation_id.to_string()).into_inner(),
                    ),
                );
                md.insert(
                    "replacements".to_string(),
                    AnalyticsValue::Int(replacements as i64),
                );
                md.insert(
                    "duration_ms".to_string(),
                    AnalyticsValue::Int(duration_ms as i64),
                );
                md
            })
            .await;
    }

    async fn emit_failed(&self, invocation_id: &str, kind: &str) {
        self.ctx
            .bus
            .log_event_lazy(EDIT_FAILED, || {
                let mut md: LogEventMetadata = HashMap::new();
                md.insert(
                    "invocation_id".to_string(),
                    AnalyticsValue::String(
                        Verified::assert_safe(invocation_id.to_string()).into_inner(),
                    ),
                );
                md.insert(
                    "failure_kind".to_string(),
                    AnalyticsValue::String(Verified::assert_safe(kind.to_string()).into_inner()),
                );
                md
            })
            .await;
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

static INPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    Utf16JsonProjection::parse(include_str!(
        "templates/claude-code-2.1.322/edit-input-schema.json"
    ))
    .expect("pinned edit input schema is valid JSON")
});

static OUTPUT_SCHEMA: Lazy<Utf16JsonProjection> = Lazy::new(|| {
    Utf16JsonProjection::parse(include_str!(
        "templates/claude-code-2.1.322/edit-output-schema.json"
    ))
    .expect("pinned Edit output schema is valid JSON")
});

#[async_trait]
impl Tool for FileEditTool {
    fn supports_persistent_chat(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified against the
    /// binary, 2 hits).
    fn search_hint(&self) -> Option<Utf16JsonProjection> {
        Some("modify file contents in place".into())
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        &INPUT_SCHEMA
    }
    fn output_schema(&self) -> Option<&Utf16JsonProjection> {
        Some(&OUTPUT_SCHEMA)
    }
    async fn parse_output(
        &self,
        output: &Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, ToolError> {
        crate::output_contract::parse(TOOL_NAME, output)
    }
    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        crate::input_repair_metadata()
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        tool_api::util::output_truncation::MAX_TOOL_OUTPUT_LENGTH
    }
    fn is_concurrency_safe(&self, _input: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_read_only(&self, _input: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_destructive(&self, _input: &Utf16JsonProjection) -> bool {
        true
    }

    async fn check_permissions(
        &self,
        _input: &Utf16JsonProjection,
        _ctx: &ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "allow-all-gate (M4-01 default)".into(),
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
        // R-EditDesc: claude-code's Edit `description()` is the short display label
        // `"A tool for editing files"` (binary), NOT the long body — the long body
        // lives in `prompt()`. The model-facing wire uses `prompt()` (wire.rs:140),
        // so `description()` is a display label only; this keeps it 1:1 anyway.
        "A tool for editing files".to_string()
    }

    async fn prompt(&self, opts: &PromptOptions) -> Utf16JsonProjection {
        // Model-gated, mirroring claude-code `prompt({model:e}){return RBp(e)}`
        // where `RBp(e){if(Dh(e))return SHORT; return LONG}` (binary offset
        // 202614289). Predicate shared with TodoWrite via `tool_api`.
        // NOTE: claude-code's Edit `description()` is the literal "A tool for
        // editing files" (not model-gated, not this body); LingXi returns
        // EDIT_DESCRIPTION there — a pre-existing divergence left untouched
        // (out of scope for this prompt-gate fix).
        if tool_api::prompt_uses_lean(opts) {
            EDIT_PROMPT_SHORT.into()
        } else {
            EDIT_DESCRIPTION.into()
        }
    }

    fn get_path(&self, input: &Utf16JsonProjection) -> Option<PathBuf> {
        crate::input_string(input, "file_path")
            .or_else(|| crate::input_string(input, "path"))
            .map(PathBuf::from)
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        crate::mutation_io::check_cancel(ctx.cancel.as_ref())?;
        // Coerce claude-code's input aliases (path/old_str/new_str/replace_name)
        // to canonical keys before reading them.
        let mut input = input;
        normalize_edit_aliases(&mut input);
        let invocation_id = tool_api::util::ids::ulid_or_uuid();
        let file_path = crate::input_string(&input, "file_path")
            .ok_or_else(|| ToolError::InvalidInput("file_path is required".into()))?;
        let old_units = input
            .string_units("/old_string")
            .ok_or_else(|| ToolError::InvalidInput("old_string is required".into()))?;
        let new_units = input
            .string_units("/new_string")
            .ok_or_else(|| ToolError::InvalidInput("new_string is required".into()))?;
        let old_string = old_units.as_slice();
        let new_string = new_units.as_slice();
        let replace_all = crate::input_bool(&input, "replace_all").unwrap_or(false);

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
        let requested_path = crate::checked_model_file_path(file_path, &self.ctx.cwd())?;
        // Mobile-linux guest paths: rewrite onto the host-backed twin (or
        // refuse fenced guest space) BEFORE canonicalization/containment, so a
        // guest path validates as the host directory that actually backs it.
        // Desktop filesystems translate nothing and this is a no-op.
        let path = match translate_model_path(&self.ctx.fs, requested_path.clone(), true) {
            Ok(path) => path,
            Err(message) => {
                self.emit_failed(&invocation_id, "path_blocked").await;
                return Err(ToolError::InvalidInput(message.into()));
            }
        };
        let memory_mutation =
            lingxi_core::host::memory_tool_scope::memory_file_mutation(TOOL_NAME, &input)
                .map_err(|error| ToolError::InvalidInput(error.to_string().into()))?;
        let started = Instant::now();
        self.emit_started(&invocation_id, &path).await;

        // `canonicalize_and_validate` tolerates a nonexistent target (it
        // canonicalizes the parent) so an empty `old_string` can create a file.
        let mut trusted_dirs = memory_mutation.as_ref().map_or_else(
            || self.ctx.trusted_dirs_for_tool(TOOL_NAME, &input),
            |scope| vec![scope.directory().to_owned()],
        );
        if let Some(root) =
            lingxi_core::host::teammate_plan::own_plan_file_root(ctx.agent_id.as_ref(), &path)
        {
            trusted_dirs.push(root);
        }
        let canon = if let Some(scope) = &memory_mutation {
            scope.target()
        } else {
            let requested = path.clone();
            let roots = trusted_dirs.clone();
            match crate::mutation_io::prepare(ctx.cancel.as_ref(), move || {
                canonicalize_and_validate(&requested, &roots)
            })
            .await?
            {
                Ok(p) => p,
                Err(_) => {
                    emit_blocked_event(&self.ctx.bus, TOOL_NAME, &path).await;
                    self.emit_failed(&invocation_id, "path_blocked").await;
                    return Err(ToolError::PathBlocked { path });
                }
            }
        };

        // Read the approved target once through a fixed rooted handle. The
        // resulting bytes, size, mtime, and mode feed both validation gates and
        // the edit itself; no later pathname reopen can cross a symlink swap.
        let snapshot_needs_body = old_string.is_empty()
            || tool_api::read_file_state::get_metadata(&self.ctx.read_file_state, &requested_path)
                .is_some()
            || crate::read_requirement_waived(Some(&ctx.options.main_loop_model), &canon);
        let snapshot_path = path.clone();
        let snapshot_canon = canon.clone();
        let snapshot_roots = trusted_dirs.clone();
        let snapshot_scope = memory_mutation.clone();
        let snapshot_cancel = ctx.cancel.clone();
        let snapshot = crate::mutation_io::prepare(ctx.cancel.as_ref(), move || {
            if let Some(scope) = &snapshot_scope {
                scope.read_file()
            } else {
                edit_rooted_snapshot(
                    &snapshot_path,
                    &snapshot_canon,
                    &snapshot_roots,
                    (!is_unc).then_some(MAX_EDIT_FILE_SIZE),
                    snapshot_needs_body,
                    snapshot_cancel.as_ref(),
                )
            }
        })
        .await?;
        let initial_snapshot = match snapshot {
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
                    return Err(ToolError::InvalidInput(
                        format!(
                            "File is too large to edit ({}). Maximum editable file size is {}.",
                            crate::read::format_file_size(snapshot.size),
                            crate::read::format_file_size(MAX_EDIT_FILE_SIZE),
                        )
                        .into(),
                    ));
                }
                if is_perforce_read_only(snapshot.mode) {
                    self.emit_failed(&invocation_id, "perforce_read_only").await;
                    return Err(ToolError::InvalidInput(PERFORCE_READ_ONLY_MESSAGE.into()));
                }
            }
        }

        // Stat/type/permission gates retain precedence, but a never-read file
        // needs no body allocation solely to return the known read requirement.
        if initial_snapshot.is_some() && !snapshot_needs_body {
            self.emit_failed(&invocation_id, "stale_read").await;
            return Err(ToolError::InvalidInput(crate::FILE_NOT_READ_ERROR.into()));
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
        let (existing, not_text, write_expectation) =
            crate::mutation_io::prepare(ctx.cancel.as_ref(), move || match initial_snapshot {
                Some(snapshot) => {
                    let bytes = snapshot.bytes;
                    let expectation =
                        lingxi_core::host::rooted_fs::RootedWriteExpectation::from_bytes(&bytes);
                    let not_text = crate::file_meta::classify_not_text(&bytes);
                    let mtime_ms = snapshot
                        .modified
                        .map(tool_api::read_file_state::mtime_ms_floor)
                        .unwrap_or(0);
                    let (content, enc, ending) = crate::file_meta::read_with_metadata_units(&bytes);
                    let raw_for_cmp = crate::utf16_mutation::without_bom(&content).to_owned();
                    (
                        Some((content, enc, ending, mtime_ms, raw_for_cmp)),
                        not_text,
                        expectation,
                    )
                }
                None => (
                    None,
                    None,
                    lingxi_core::host::rooted_fs::RootedWriteExpectation::Missing,
                ),
            })
            .await?;

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
                Vec::new(),
            ),
            Some((content, enc, ending, mtime_ms, raw_for_cmp)) => {
                (Some(content), enc, ending, mtime_ms, raw_for_cmp)
            }
        };

        // `staleRecovered` (claude `xTg` → `D`): set when the flag-gated
        // stale-recovery lets a stale-but-cleanly-applying edit proceed.
        let mut stale_recovered = false;
        let prior_partial_view = existing.is_some()
            && tool_api::read_file_state::get_metadata(&self.ctx.read_file_state, &requested_path)
                .is_some_and(|entry| entry.is_partial_view);
        let (before, after, replacements): (Vec<u16>, Vec<u16>, u32) = match existing {
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
                        crate::read::file_not_found_message(&canon, &self.ctx.cwd()).into(),
                    ));
                }
                // Empty `old_string` on a nonexistent file → create it.
                (Vec::new(), new_string.to_vec(), 1)
            }
            // File exists.
            Some(before) => {
                if old_string.is_empty()
                    && !lingxi_core::host::effort::trim_js_whitespace(&String::from_utf16_lossy(
                        &before,
                    ))
                    .is_empty()
                {
                    self.emit_failed(&invocation_id, "file_exists").await;
                    return Err(ToolError::InvalidInput(
                        "Cannot create new file - file already exists.".into(),
                    ));
                }
                let waived =
                    crate::read_requirement_waived(Some(&ctx.options.main_loop_model), &canon);
                if !waived
                    && !old_string.is_empty()
                    && tool_api::read_file_state::get_metadata(
                        &self.ctx.read_file_state,
                        &requested_path,
                    )
                    .is_some_and(|entry| entry.is_partial_view)
                {
                    return Err(ToolError::InvalidInput(crate::FILE_NOT_READ_ERROR.into()));
                }
                // Read-before-write staleness guard (Batch F). The file exists,
                // so a prior full `Read` is required and the file must not have
                // changed on disk since (TS `FileEditTool.ts:275-311` validate +
                // `:451-468` call-time re-check). New-file creation hits the
                // `None` arm above and skips this entirely (TS ENOENT →
                // `result:true`). `guard_raw_content` is the current on-disk
                // content decoded the same way `Read` records it, for the
                // full-read content-equality fallback.
                let mut recovered_match = None;
                if let Err(e) = if old_string.is_empty() {
                    Ok(())
                } else {
                    crate::check_read_before_write_units(
                        &self.ctx.read_file_state,
                        &requested_path,
                        guard_mtime_ms,
                        &guard_raw_content,
                        waived,
                    )
                } {
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
                            if m.string_at("") == Some(crate::FILE_UNEXPECTEDLY_MODIFIED_ERROR)
                    );
                    // `Mjs`: `GKe(...)==="applies" && !readNotAutoAllowed()`.
                    // `kq` asks whether the model could ever have READ this
                    // file — it holds a reader, and the path is readable under
                    // the policy. Without it an Edit could recover a stale read
                    // on a file the model was never allowed to see, i.e. a write
                    // landing on unseen content. An un-wired host answers
                    // `false` and simply keeps the stale error.
                    let applicable_match = if is_stale_error
                        && lingxi_core::host::read_auto_allow::read_auto_allowed(
                            &canon.to_string_lossy(),
                        ) {
                        find_stale_edit_match_units(&before, old_string, replace_all)
                    } else {
                        None
                    };
                    if let Some(actual) = applicable_match {
                        stale_recovered = true;
                        recovered_match = Some(actual);
                    } else {
                        self.emit_failed(&invocation_id, "stale_read").await;
                        return Err(e);
                    }
                }

                if let Some(kind) = not_text {
                    self.emit_failed(&invocation_id, "unsupported_encoding")
                        .await;
                    return Err(ToolError::InvalidInput(
                        kind.mutation_refusal_message().into(),
                    ));
                }

                if old_string.is_empty() {
                    // Empty `old_string` is only valid for an (effectively) empty
                    // file — otherwise it's a creation attempt on existing content.
                    let replacement =
                        crate::file_meta::preserve_bom_units(&before, new_string).into_owned();
                    (before, replacement, 1)
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
                    let ResolvedUnitEditMatch { actual_old, count } = recovered_match
                        .unwrap_or_else(|| ResolvedUnitEditMatch {
                            actual_old: crate::quotes::find_actual_string_units(
                                &before, old_string,
                            )
                            .unwrap_or_else(|| old_string.to_vec()),
                            count: None,
                        });
                    let actual_new = crate::quotes::preserve_escape_style_units(
                        old_string,
                        &actual_old,
                        &crate::quotes::preserve_quote_style_units(
                            old_string,
                            &actual_old,
                            new_string,
                        ),
                    );
                    let count =
                        count.unwrap_or_else(|| crate::utf16_mutation::count(&before, &actual_old));
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
                        let note = if crate::quotes::has_escape_or_non_ascii_units(old_string) {
                            ESCAPE_SWAP_NOTE
                        } else {
                            ""
                        };
                        return Err(crate::utf16_mutation::input_error(
                            "String to replace not found in file.\nString: ",
                            old_string,
                            note,
                        ));
                    }
                    if !replace_all && count > 1 {
                        self.emit_failed(&invocation_id, "ambiguous_match").await;
                        // Byte-locked multiple-match message (FileEditTool.ts:336).
                        // `count` == TS `matches`; trailing `String:` echoes the
                        // ORIGINAL `old_string` input (TS `${old_string}`).
                        return Err(crate::utf16_mutation::input_error(&format!(
                            "Found {count} matches of the string to replace, but replace_all is false. To replace all occurrences, set replace_all to true. To replace only one occurrence, please provide more context to uniquely identify the instance.\nString: "
                        ), old_string, ""));
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
                    let mut old_with_newline = actual_old.clone();
                    old_with_newline.push(10);
                    let search_target = if actual_new.is_empty()
                        && !actual_old.ends_with(&[10])
                        && crate::utf16_mutation::find(&before, &old_with_newline).is_some()
                    {
                        old_with_newline
                    } else {
                        actual_old
                    };
                    let after = crate::utf16_mutation::replace(
                        &before,
                        &search_target,
                        &actual_new,
                        replace_all,
                    );
                    let replacements = if replace_all { count as u32 } else { 1 };
                    (before, after, replacements)
                }
            }
        };

        // Exact patch generation stays off the async worker and precedes the
        // mutation. A cancelled/failed preparation must not leave a committed
        // edit without its result, even for expensive shuffled-line diffs.
        let (before, after, bytes, published_units, structured_patch, lines_added, lines_removed) =
            crate::mutation_io::prepare(ctx.cancel.as_ref(), move || {
                let (structured_patch, lines_added, lines_removed) =
                    crate::utf16_mutation::patch(&before, &after);
                // Preserve the original encoding and line endings. `after` is
                // LF-normalized and retains any leading UTF-16/UTF-8 BOM.
                let bytes = crate::file_meta::encode_units_with_metadata(&after, enc, ending);
                let published_units = crate::utf16_mutation::without_bom(&after).to_vec();
                (
                    before,
                    after,
                    bytes,
                    published_units,
                    structured_patch,
                    lines_added,
                    lines_removed,
                )
            })
            .await?;

        // (/rewind) Back up the pre-edit content BEFORE writing so /rewind can
        // restore it. Best-effort; no-op when checkpointing isn't wired.
        if let Some(fh) = ctx.file_history.as_ref() {
            fh.track_edit(&canon.to_string_lossy()).await;
        }

        let commit_path = path.clone();
        let commit_canon = canon.clone();
        let commit_requested = requested_path.clone();
        let commit_file_path = file_path.to_owned();
        let commit_scope = memory_mutation.clone();
        let state =
            tool_api::read_file_state::capture_read_file_state_write(&self.ctx.read_file_state);
        let history = ctx.file_history.clone();
        let result = crate::mutation_io::commit(ctx.cancel.as_ref(), move || {
            let result = if let Some(scope) = &commit_scope {
                scope.write_file_checked(&bytes, &write_expectation)
            } else {
                let Some((root, relative)) =
                    crate::shared::rooted_location(&commit_canon, &trusted_dirs)
                else {
                    return Err(ToolError::PathBlocked { path: commit_path });
                };
                lingxi_core::host::rooted_fs::write_file_after_permission_checked(
                    &root,
                    &relative,
                    &commit_path,
                    &commit_canon,
                    &bytes,
                    &write_expectation,
                )
            };
            let mut result =
                result.map_err(|error| edit_resolution_error(&commit_file_path, error))?;
            if let (Some(history), Some(parent)) = (history.as_ref(), result.parent.take()) {
                history.track_completed_write(&commit_canon.to_string_lossy(), parent);
            }
            let new_mtime_ms = result
                .modified
                .map_or(0, tool_api::read_file_state::mtime_ms_floor);
            state.set_after_write_with_identity(
                commit_requested.clone(),
                tool_api::read_file_state::ReadFileEntry {
                    content: String::from_utf16_lossy(&published_units),
                    mtime_ms: new_mtime_ms,
                    offset: None,
                    limit: None,
                    from_read: false,
                    seeded_from_context: false,
                    is_partial_view: prior_partial_view,
                },
                true,
                vec![commit_requested, commit_path],
                Some(published_units),
                result.identity,
            );
            Ok(result)
        })
        .await?;
        if let Err(error) = result {
            self.emit_failed(&invocation_id, "io_write").await;
            return Err(error);
        }

        // Keep an installed language server synchronized immediately after a
        // successful edit so passive diagnostics can arrive before the next
        // model turn. Missing/unhealthy servers are best-effort and never turn
        // a completed file edit into a failure.
        self.ctx
            .sync_lsp_after_file_write(&canon, &String::from_utf16_lossy(&after))
            .await;

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
        let content = edit_result_message_with_context(
            file_path,
            replace_all,
            stale_recovered,
            prior_partial_view,
        );

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
            "oldString": "",
            "newString": "",
            "originalFile": "",
            "structuredPatch": [],
            "userModified": false,
            "replaceAll": replace_all,
        });
        if stale_recovered {
            data["staleRecovered"] = json!(true);
        }
        if prior_partial_view {
            data["contentNotInModelContext"] = json!(true);
        }
        let mut data = crate::generated_result(&data);
        data.set_field("oldString", crate::utf16_mutation::string(old_string))
            .expect("result field");
        data.set_field("newString", crate::utf16_mutation::string(new_string))
            .expect("result field");
        data.set_field("originalFile", crate::utf16_mutation::string(&before))
            .expect("result field");
        data.set_field("structuredPatch", structured_patch)
            .expect("result field");
        Ok(ToolCallResult {
            data,
            model_content: Some(content.into()),
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
            subagent_hooks_fired: false,
        })
    }
}

#[cfg(test)]
#[path = "edit_test.rs"]
mod edit_test;
