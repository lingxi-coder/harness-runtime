//! LSP builtin tool — `LSPTool` exposes 9 operations (goToDefinition /
//! findReferences / hover / documentSymbol / workspaceSymbol /
//! goToImplementation / prepareCallHierarchy / incomingCalls / outgoingCalls)
//! over `lsp::LspClient` (M2-03; surface realigned to `LSPTool.ts:62-72`).
//!
//! Input position is 1-based (claude-code UI convention); we convert to
//! 0-based via `lsp::tool_operations::position_from_one_based`
//! before issuing the LSP request.
//!
//! Wire identifiers locked in spec §7 line 695.
//!
//! Result formatting (LSP.5): `LSPTool.ts:636 formatResult` plus every
//! sub-formatter in `LSPTool/formatters.ts` are ported here as
//! [`format_result`] and the per-operation `format_*_result` helpers. The
//! model is given the brief human-readable string TS produces — in TS this
//! is `mapToolResultToToolResultBlockParam` returning `content:
//! output.result`, where `output.result` is the formatted string (the raw
//! structured payload is never retained). We mirror that exactly: the
//! formatted string is placed in `model_content` (which
//! `tool_result_to_model_text` routes to the model, the Rust analogue of
//! `mapToolResultToToolResultBlockParam`) and ALSO in `result`, alongside
//! `result_count` / `file_count`, to reproduce the TS `Output` shape for the
//! UI.
//!
//! no-truncation: the model-facing string is the `formatResult` summary —
//! `path:line:col` location lines, symbol outlines, and call-hierarchy
//! entries grouped by file. These are bounded by the LSP server's own result
//! set; free-form text comes only from hover contents, which the LSP protocol
//! itself bounds. The TS tool likewise applies no inline truncation
//! (`maxResultSizeChars` gates length at the framework layer).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use lsp::registry::LspRegistry;
use lsp::tool_operations as ops;
use lsp::Url;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{LSP_COMPLETED, LSP_FAILED, LSP_STARTED};
use telemetry::AnalyticsBus;

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext, ValidationError,
};

// -- Wire identifier locks (spec §7 line 695) --------------------------------

/// Registry name for the LSP tool (the Rust struct keeps the
/// `LSPTool` identifier).
pub const LSP_TOOL_NAME: &str = "LSP";

/// Operation literal (`textDocument/definition`).
pub const LSP_OPERATION_GO_TO_DEFINITION: &str = "goToDefinition";
/// Operation literal (`textDocument/references`).
pub const LSP_OPERATION_FIND_REFERENCES: &str = "findReferences";
/// Operation literal (`textDocument/hover`).
pub const LSP_OPERATION_HOVER: &str = "hover";
/// Operation literal (`textDocument/documentSymbol`).
pub const LSP_OPERATION_DOCUMENT_SYMBOL: &str = "documentSymbol";
/// Operation literal (`workspace/symbol`).
pub const LSP_OPERATION_WORKSPACE_SYMBOL: &str = "workspaceSymbol";
/// Operation literal (`textDocument/implementation`).
pub const LSP_OPERATION_GO_TO_IMPLEMENTATION: &str = "goToImplementation";
/// Operation literal (`textDocument/prepareCallHierarchy`).
pub const LSP_OPERATION_PREPARE_CALL_HIERARCHY: &str = "prepareCallHierarchy";
/// Operation literal (`callHierarchy/incomingCalls`).
pub const LSP_OPERATION_INCOMING_CALLS: &str = "incomingCalls";
/// Operation literal (`callHierarchy/outgoingCalls`).
pub const LSP_OPERATION_OUTGOING_CALLS: &str = "outgoingCalls";

/// All nine operations in the locked surface order (`LSPTool.ts:62-72`).
pub const LSP_OPERATIONS_LOCKED: [&str; 9] = [
    LSP_OPERATION_GO_TO_DEFINITION,
    LSP_OPERATION_FIND_REFERENCES,
    LSP_OPERATION_HOVER,
    LSP_OPERATION_DOCUMENT_SYMBOL,
    LSP_OPERATION_WORKSPACE_SYMBOL,
    LSP_OPERATION_GO_TO_IMPLEMENTATION,
    LSP_OPERATION_PREPARE_CALL_HIERARCHY,
    LSP_OPERATION_INCOMING_CALLS,
    LSP_OPERATION_OUTGOING_CALLS,
];

/// The model-facing LSP tool description — byte-exact to claude-code's `udo`
/// (binary @202037362). The `\u{2014}` is the em-dash in the workspaceSymbol
/// note.
pub const LSP_TOOL_DESCRIPTION: &str = "Interact with Language Server Protocol (LSP) servers to get code intelligence features.\n\
\n\
Supported operations:\n\
- goToDefinition: Find where a symbol is defined\n\
- findReferences: Find all references to a symbol\n\
- hover: Get hover information (documentation, type info) for a symbol\n\
- documentSymbol: Get all symbols (functions, classes, variables) in a document\n\
- workspaceSymbol: Search for symbols matching a query across the entire workspace\n\
- goToImplementation: Find implementations of an interface or abstract method\n\
- prepareCallHierarchy: Get call hierarchy item at a position (functions/methods)\n\
- incomingCalls: Find all functions/methods that call the function at a position\n\
- outgoingCalls: Find all functions/methods called by the function at a position\n\
\n\
All operations require:\n\
- filePath: The file to operate on\n\
- line: The line number (1-based, as shown in editors)\n\
- character: The character offset (1-based, as shown in editors)\n\
\n\
The workspaceSymbol operation also takes:\n\
- query: The symbol name or partial name to search for. Always provide it \u{2014} most language servers return no results for an empty query.\n\
\n\
Note: LSP servers must be configured for the file type. If no server is available, an error will be returned.";

/// Position-validation error literal.
///
/// [RESIDUAL — message not in 2.1.206] 2.1.206's LSP tool rejects an
/// out-of-range position at the SCHEMA layer (`line`/`character` carry
/// `minimum: 1`, mirrored below), so it has no such runtime string — the
/// wording would be the validator's own output. This in-tool check is a
/// port-only belt-and-suspenders reached only if a caller bypasses schema
/// validation.
pub const LSP_POSITION_ERROR: &str = "LSP position must be 1-based (line >= 1, character >= 1)";

// -- Helpers -----------------------------------------------------------------

fn pii(s: &str) -> AnalyticsValue {
    use telemetry::pii::PiiTagged;
    AnalyticsValue::String(PiiTagged::assert_pii_tagged_column(s.to_string()).into_inner())
}
fn verified_int(n: u64) -> AnalyticsValue {
    AnalyticsValue::Int(n as i64)
}
fn verified_str(s: &str) -> AnalyticsValue {
    use telemetry::pii::Verified;
    AnalyticsValue::String(Verified::assert_safe(s.to_string()).into_inner())
}

async fn emit(bus: &Arc<AnalyticsBus>, event: &'static str, fields: &[(&str, AnalyticsValue)]) {
    let mut md: LogEventMetadata = HashMap::new();
    for (k, v) in fields {
        md.insert((*k).into(), v.clone());
    }
    bus.log_event(event, md).await;
}

fn allow_lsp() -> PermissionResult {
    PermissionResult::Allow {
        reason: PermissionDecisionReason::Other {
            reason: "LSP read-only operation".into(),
        },
        updated_input: None,
        update_destination: None,
        metadata: PermissionMetadata::default(),
    }
}

// -- Path expansion (`utils/path.ts:expandPath`) -----------------------------

/// User home directory — the `os.homedir()` equivalent. `tool-lsp` does not
/// depend on the `dirs` crate, so we resolve via the conventional env vars
/// (`HOME` on posix, `USERPROFILE` on Windows).
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map_or_else(PathBuf::new, PathBuf::from)
}

/// Current working directory — the `getCwd()` equivalent.
fn current_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// The graceful "file too large" message from `LSPTool.ts:268`. The size is
/// reported in MB via `Math.ceil(size / 1_000_000)` — NOT in bytes.
fn file_too_large_message(size: u64) -> String {
    let mb = size.div_ceil(1_000_000);
    format!("File too large for LSP analysis ({mb}MB exceeds 10MB limit)")
}

/// Core of `validateInput` (`LSPTool.ts:166-208`), split out for direct
/// testing: confirm the (expanded) path exists and is a regular file, with
/// byte-exact TS messages. Returns `Ok(())` for UNC paths (skipped for the
/// NTLM-leak security reason in the TS source). Relative paths resolve against
/// the process cwd; the tool uses [`validate_file_path_in`] with the live cwd.
fn validate_file_path(file_path: &str) -> Result<(), ValidationError> {
    validate_file_path_in(file_path, &current_cwd())
}

/// [`validate_file_path`] resolving a relative `file_path` against an explicit
/// `cwd` — the live `getCwd()` the LSP tool passes so a Bash `cd` is honored.
fn validate_file_path_in(file_path: &str, cwd: &Path) -> Result<(), ValidationError> {
    let absolute_path = expand_path_in(file_path, cwd);
    let display = absolute_path.to_string_lossy();
    if display.starts_with("\\\\") || display.starts_with("//") {
        return Ok(());
    }
    match std::fs::metadata(&absolute_path) {
        Ok(stats) => {
            if stats.is_file() {
                Ok(())
            } else {
                Err(ValidationError(format!("Path is not a file: {file_path}")))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Err(ValidationError(format!("File does not exist: {file_path}")))
        }
        Err(err) => Err(ValidationError(format!(
            "Cannot access file: {file_path}. {err}"
        ))),
    }
}

/// Expand a user-supplied path the way `utils/path.ts:expandPath` does:
/// leading `~` -> home, `~/x` -> home/x, absolute -> unchanged, relative ->
/// resolved against the current working directory. Mirrors the trim and
/// empty-path handling of the TS implementation; the Windows POSIX-path
/// conversion branch is not relevant on the target platform.
fn expand_path(path: &str) -> PathBuf {
    expand_path_in(path, &current_cwd())
}

/// [`expand_path`] resolving relative/empty paths against an explicit `cwd`
/// (the live `getCwd()` the LSP tool passes) instead of the process cwd, so a
/// Bash `cd` is honored — 1:1 with claude-code's live `Ct()`.
fn expand_path_in(path: &str, cwd: &Path) -> PathBuf {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return cwd.to_path_buf();
    }
    if trimmed == "~" {
        return home_dir();
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    let candidate = Path::new(trimmed);
    if candidate.is_absolute() {
        return candidate.to_path_buf();
    }
    cwd.join(candidate)
}

// -- gitignore filtering (`LSPTool.ts:336-374`, `filterGitIgnoredLocations`) --

/// Percent-decode a `file://` path body, mirroring `decodeURIComponent`.
/// Returns `None` on malformed input so the caller can fall back to the raw
/// (un-decoded) path, exactly as the TS `try/catch` does.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push(u8::try_from(hi * 16 + lo).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Extract a filesystem path from a `file://` URI, decoding percent-encoded
/// characters. Mirrors `LSPTool.ts:uriToFilePath`.
fn uri_to_file_path(uri: &str) -> String {
    let mut file_path = uri.strip_prefix("file://").unwrap_or(uri).to_string();
    // On Windows, file:///C:/path becomes /C:/path — strip the leading slash.
    let b = file_path.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        file_path = file_path[1..].to_string();
    }
    percent_decode(&file_path).unwrap_or(file_path)
}

fn rewrite_server_uris_to_host(
    value: &mut Value,
    registry: &LspRegistry,
) -> Result<(), platform_api::LspError> {
    match value {
        Value::Array(items) => {
            for item in items {
                rewrite_server_uris_to_host(item, registry)?;
            }
        }
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if matches!(key.as_str(), "uri" | "targetUri") {
                    if let Some(raw_uri) = child.as_str() {
                        if let Ok(parsed) = Url::parse(raw_uri) {
                            if let Some(host_uri) = registry.host_uri_for_server_uri(&parsed)? {
                                *child = Value::String(host_uri.to_string());
                            }
                        }
                    }
                } else {
                    rewrite_server_uris_to_host(child, registry)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// `toLocation(item).uri` for a `Location` (`uri`) or `LocationLink`
/// (`targetUri`). Returns `None` when the URI is absent/non-string (the TS
/// `!loc.uri` keep-case).
fn location_uri(item: &Value) -> Option<&str> {
    if item.get("targetUri").is_some() {
        return item.get("targetUri").and_then(Value::as_str);
    }
    item.get("uri").and_then(Value::as_str)
}

/// `SymbolInformation.location.uri`.
fn symbol_location_uri(item: &Value) -> Option<&str> {
    item.get("location")
        .and_then(|l| l.get("uri"))
        .and_then(Value::as_str)
}

/// Run `git check-ignore <paths...>` in `cwd`. Returns the stdout only when
/// git exits 0 (≥1 path ignored). Exit 1 (none ignored) and 128 (not a repo)
/// yield `None`. Matches `execFileNoThrowWithCwd('git', ['check-ignore', …])`.
fn run_git_check_ignore(paths: &[String], cwd: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("check-ignore")
        .args(paths)
        .current_dir(cwd)
        .output()
        .ok()?;
    if output.status.code() == Some(0) {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

/// Compute the set of URIs whose backing file is gitignored, batching the
/// `git check-ignore` calls (50 paths per invocation) exactly like
/// `filterGitIgnoredLocations`.
fn ignored_uri_set(uris: &[&str], cwd: &Path) -> HashSet<String> {
    let mut ignored_uris: HashSet<String> = HashSet::new();
    // Unique URI -> filesystem path.
    let mut uri_to_path: Vec<(String, String)> = Vec::new();
    let mut seen_uri: HashSet<&str> = HashSet::new();
    for &uri in uris {
        if !uri.is_empty() && seen_uri.insert(uri) {
            uri_to_path.push((uri.to_string(), uri_to_file_path(uri)));
        }
    }
    // Unique paths (preserve first-seen order).
    let mut unique_paths: Vec<String> = Vec::new();
    let mut seen_path: HashSet<&str> = HashSet::new();
    for (_, p) in &uri_to_path {
        if seen_path.insert(p.as_str()) {
            unique_paths.push(p.clone());
        }
    }
    if unique_paths.is_empty() {
        return ignored_uris;
    }
    // Batch-check paths; collect the absolute paths git reports as ignored.
    let mut ignored_paths: HashSet<String> = HashSet::new();
    for batch in unique_paths.chunks(50) {
        if let Some(stdout) = run_git_check_ignore(batch, cwd) {
            for line in stdout.split('\n') {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    ignored_paths.insert(trimmed.to_string());
                }
            }
        }
    }
    if ignored_paths.is_empty() {
        return ignored_uris;
    }
    for (uri, p) in uri_to_path {
        if ignored_paths.contains(&p) {
            ignored_uris.insert(uri);
        }
    }
    ignored_uris
}

/// Filter gitignored files out of a location-bearing LSP result, mirroring
/// `LSPTool.ts:336-374`. Only array results of `findReferences`,
/// `goToDefinition`, `goToImplementation`, and `workspaceSymbol` are touched;
/// everything else is returned unchanged. Items whose URI is absent are kept.
fn filter_gitignored_results(operation: &str, raw: Value, cwd: &Path) -> Value {
    let is_location_op = matches!(
        operation,
        "findReferences" | "goToDefinition" | "goToImplementation" | "workspaceSymbol"
    );
    if !is_location_op {
        return raw;
    }
    let Value::Array(items) = &raw else {
        return raw;
    };
    let is_workspace_symbol = operation == "workspaceSymbol";
    let extract = |item: &Value| -> Option<String> {
        if is_workspace_symbol {
            symbol_location_uri(item)
        } else {
            location_uri(item)
        }
        .map(ToString::to_string)
    };
    let uris: Vec<String> = items.iter().filter_map(&extract).collect();
    let uri_refs: Vec<&str> = uris.iter().map(String::as_str).collect();
    let ignored = ignored_uri_set(&uri_refs, cwd);
    if ignored.is_empty() {
        return raw;
    }
    let filtered: Vec<Value> = items
        .iter()
        .filter(|item| extract(item).is_none_or(|uri| !ignored.contains(&uri)))
        .cloned()
        .collect();
    Value::Array(filtered)
}

// -- Result formatting (`LSPTool.ts:636 formatResult` + `formatters.ts`) ------

// Empty-result messages, byte-exact from `formatters.ts`.
const NO_DEFINITION: &str = "No definition found. This may occur if the cursor is not on a symbol, or if the definition is in an external library not indexed by the LSP server.";
const NO_REFERENCES: &str = "No references found. This may occur if the symbol has no usages, or if the LSP server has not fully indexed the workspace.";
const NO_HOVER: &str = "No hover information available. This may occur if the cursor is not on a symbol, or if the LSP server has not fully indexed the file.";
const NO_DOCUMENT_SYMBOLS: &str = "No symbols found in document. This may occur if the file is empty, not supported by the LSP server, or if the server has not fully indexed the file.";
const NO_WORKSPACE_SYMBOLS: &str = "No symbols found in workspace. This may occur if the workspace is empty, or if the LSP server has not finished indexing the project.";
const NO_CALL_HIERARCHY_ITEM: &str = "No call hierarchy item found at this position";
const NO_INCOMING_CALLS: &str = "No incoming calls found (nothing calls this function)";
const NO_OUTGOING_CALLS: &str = "No outgoing calls found (this function calls nothing)";

/// `stringUtils.ts:plural` — singular for `n == 1`, else `word + "s"`.
fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// UTF-16 code-unit length, matching JavaScript's `String.prototype.length`
/// (used by `formatUri` for the relative-vs-absolute length comparison).
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `formatters.ts:symbolKindToString` — LSP `SymbolKind` (1-26) to label;
/// anything out of range (or missing) maps to `Unknown`.
fn symbol_kind_to_string(item: &Value) -> &'static str {
    match item.get("kind").and_then(Value::as_u64) {
        Some(1) => "File",
        Some(2) => "Module",
        Some(3) => "Namespace",
        Some(4) => "Package",
        Some(5) => "Class",
        Some(6) => "Method",
        Some(7) => "Property",
        Some(8) => "Field",
        Some(9) => "Constructor",
        Some(10) => "Enum",
        Some(11) => "Interface",
        Some(12) => "Function",
        Some(13) => "Variable",
        Some(14) => "Constant",
        Some(15) => "String",
        Some(16) => "Number",
        Some(17) => "Boolean",
        Some(18) => "Array",
        Some(19) => "Object",
        Some(20) => "Key",
        Some(21) => "Null",
        Some(22) => "EnumMember",
        Some(23) => "Struct",
        Some(24) => "Event",
        Some(25) => "Operator",
        Some(26) => "TypeParameter",
        _ => "Unknown",
    }
}

/// Posix `path.relative(from, to)` for normalized absolute paths. Both inputs
/// are absolute (cwd from `getCwd()` / file paths from `file://` URIs), so the
/// segment-prefix algorithm reproduces Node's `posix.relative` output.
fn path_relative(from: &str, to: &str) -> String {
    fn segments(p: &str) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for seg in p.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    out.pop();
                }
                s => out.push(s),
            }
        }
        out
    }
    let f = segments(from);
    let t = segments(to);
    let mut i = 0;
    while i < f.len() && i < t.len() && f[i] == t[i] {
        i += 1;
    }
    let mut parts: Vec<&str> = vec![".."; f.len() - i];
    parts.extend_from_slice(&t[i..]);
    parts.join("/")
}

/// `formatters.ts:formatUri` — render a URI as a relative path against `cwd`
/// when that is shorter and does not climb past `../..`; otherwise the
/// absolute file path. A missing/empty URI renders `<unknown location>`.
fn format_uri(uri: Option<&str>, cwd: &Path) -> String {
    let Some(uri) = uri.filter(|u| !u.is_empty()) else {
        return "<unknown location>".to_string();
    };
    // Strip the `file://` scheme, then the leading slash of a Windows
    // drive-letter path (`/C:/...` -> `C:/...`).
    let mut file_path = uri.strip_prefix("file://").unwrap_or(uri).to_string();
    let b = file_path.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        file_path = file_path[1..].to_string();
    }
    // Decode percent-escapes; keep the un-decoded path on malformed input.
    let file_path = percent_decode(&file_path).unwrap_or(file_path);

    let cwd_str = cwd.to_string_lossy();
    if !cwd_str.is_empty() {
        let relative_path = path_relative(&cwd_str, &file_path).replace('\\', "/");
        if utf16_len(&relative_path) < utf16_len(&file_path) && !relative_path.starts_with("../../")
        {
            return relative_path;
        }
    }
    file_path.replace('\\', "/")
}

/// `formatters.ts:toLocation` — yield `(uri, range)` for a `Location`
/// (`uri`/`range`) or a `LocationLink` (`targetUri`,
/// `targetSelectionRange || targetRange`).
fn to_location(item: &Value) -> (Option<&str>, Option<&Value>) {
    if item.get("targetUri").is_some() {
        let uri = item.get("targetUri").and_then(Value::as_str);
        let range = item
            .get("targetSelectionRange")
            .or_else(|| item.get("targetRange"));
        (uri, range)
    } else {
        (item.get("uri").and_then(Value::as_str), item.get("range"))
    }
}

/// 1-based-ready `(line, character)` of a `range.start` (0-based on the wire).
fn range_start(range: Option<&Value>) -> (i64, i64) {
    let start = range.and_then(|r| r.get("start"));
    let line = start
        .and_then(|s| s.get("line"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let character = start
        .and_then(|s| s.get("character"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    (line, character)
}

/// `formatters.ts:formatLocation` — `path:line:character` (1-based).
fn format_location(item: &Value, cwd: &Path) -> String {
    let (uri, range) = to_location(item);
    let (line, character) = range_start(range);
    format!("{}:{}:{}", format_uri(uri, cwd), line + 1, character + 1)
}

/// String field that is present and non-empty (TS truthiness for `detail`,
/// `containerName`, `name`).
fn truthy_str(item: &Value, key: &str) -> Option<String> {
    item.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn name_of(item: &Value) -> &str {
    item.get("name").and_then(Value::as_str).unwrap_or("")
}

/// Number of unique URIs in the iterator (`countUniqueFiles*`).
fn unique_uri_count<'a>(uris: impl Iterator<Item = &'a str>) -> u64 {
    uris.collect::<HashSet<&str>>().len() as u64
}

/// Group items into `(filePath, items)` buckets preserving first-seen order
/// (a faithful stand-in for the insertion-ordered JS `Map`). `key` returns
/// `None` to skip an item entirely.
fn group_by_file<'a>(
    items: &[&'a Value],
    key: impl Fn(&Value) -> Option<String>,
) -> Vec<(String, Vec<&'a Value>)> {
    let mut order: Vec<String> = Vec::new();
    let mut buckets: HashMap<String, Vec<&'a Value>> = HashMap::new();
    for &item in items {
        if let Some(file_path) = key(item) {
            if !buckets.contains_key(&file_path) {
                order.push(file_path.clone());
            }
            buckets.entry(file_path).or_default().push(item);
        }
    }
    order
        .into_iter()
        .map(|k| {
            let v = buckets.remove(&k).unwrap_or_default();
            (k, v)
        })
        .collect()
}

/// `formatGoToDefinitionResult` (also reused for `goToImplementation`).
fn format_go_to_definition_result(result: &Value, cwd: &Path) -> String {
    if result.is_null() {
        return NO_DEFINITION.to_string();
    }
    if let Some(arr) = result.as_array() {
        let valid: Vec<&Value> = arr
            .iter()
            .filter(|it| to_location(it).0.is_some_and(|u| !u.is_empty()))
            .collect();
        if valid.is_empty() {
            return NO_DEFINITION.to_string();
        }
        if valid.len() == 1 {
            return format!("Defined in {}", format_location(valid[0], cwd));
        }
        let list = valid
            .iter()
            .map(|loc| format!("  {}", format_location(loc, cwd)))
            .collect::<Vec<_>>()
            .join("\n");
        return format!("Found {} definitions:\n{}", valid.len(), list);
    }
    // Single (non-array) result.
    format!("Defined in {}", format_location(result, cwd))
}

/// `formatFindReferencesResult`.
fn format_find_references_result(result: &Value, cwd: &Path) -> String {
    let Some(arr) = result.as_array() else {
        return NO_REFERENCES.to_string();
    };
    if arr.is_empty() {
        return NO_REFERENCES.to_string();
    }
    let valid: Vec<&Value> = arr
        .iter()
        .filter(|loc| {
            loc.get("uri")
                .and_then(Value::as_str)
                .is_some_and(|u| !u.is_empty())
        })
        .collect();
    if valid.is_empty() {
        return NO_REFERENCES.to_string();
    }
    if valid.len() == 1 {
        return format!("Found 1 reference:\n  {}", format_location(valid[0], cwd));
    }
    let by_file = group_by_file(&valid, |loc| {
        Some(format_uri(loc.get("uri").and_then(Value::as_str), cwd))
    });
    let mut lines: Vec<String> = vec![format!(
        "Found {} references across {} files:",
        valid.len(),
        by_file.len()
    )];
    for (file_path, locations) in &by_file {
        lines.push(format!("\n{file_path}:"));
        for loc in locations {
            let (line, character) = range_start(loc.get("range"));
            lines.push(format!("  Line {}:{}", line + 1, character + 1));
        }
    }
    lines.join("\n")
}

/// `formatters.ts:extractMarkupText` — flatten `Hover.contents` to text.
fn extract_markup_text(contents: &Value) -> String {
    if let Some(arr) = contents.as_array() {
        return arr
            .iter()
            .map(|item| {
                item.as_str().map_or_else(
                    || {
                        item.get("value")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    },
                    ToString::to_string,
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
    }
    if let Some(s) = contents.as_str() {
        return s.to_string();
    }
    // `MarkupContent` (has `kind`) or a `MarkedString` object — both `.value`.
    contents
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// `formatHoverResult`.
fn format_hover_result(result: &Value) -> String {
    if result.is_null() {
        return NO_HOVER.to_string();
    }
    let content = extract_markup_text(result.get("contents").unwrap_or(&Value::Null));
    if let Some(range) = result.get("range").filter(|r| !r.is_null()) {
        let (line, character) = range_start(Some(range));
        return format!(
            "Hover info at {}:{}:\n\n{}",
            line + 1,
            character + 1,
            content
        );
    }
    content
}

/// `formatters.ts:formatDocumentSymbolNode` — one DocumentSymbol (+children).
fn format_document_symbol_node(symbol: &Value, indent: usize, lines: &mut Vec<String>) {
    let prefix = "  ".repeat(indent);
    let kind = symbol_kind_to_string(symbol);
    let mut line = format!("{prefix}{} ({kind})", name_of(symbol));
    if let Some(detail) = truthy_str(symbol, "detail") {
        line.push_str(&format!(" {detail}"));
    }
    let (symbol_line, _) = range_start(symbol.get("range"));
    line.push_str(&format!(" - Line {}", symbol_line + 1));
    lines.push(line);
    if let Some(children) = symbol.get("children").and_then(Value::as_array) {
        for child in children {
            format_document_symbol_node(child, indent + 1, lines);
        }
    }
}

/// `formatDocumentSymbolResult` — hierarchical outline; `SymbolInformation[]`
/// results delegate to the workspace-symbol formatter (per LSP spec).
fn format_document_symbol_result(result: &Value, cwd: &Path) -> String {
    let Some(arr) = result.as_array() else {
        return NO_DOCUMENT_SYMBOLS.to_string();
    };
    if arr.is_empty() {
        return NO_DOCUMENT_SYMBOLS.to_string();
    }
    let is_symbol_information = arr[0].is_object() && arr[0].get("location").is_some();
    if is_symbol_information {
        return format_workspace_symbol_result(result, cwd);
    }
    let mut lines: Vec<String> = vec!["Document symbols:".to_string()];
    for symbol in arr {
        format_document_symbol_node(symbol, 0, &mut lines);
    }
    lines.join("\n")
}

/// `formatWorkspaceSymbolResult` — flat symbol list grouped by file.
fn format_workspace_symbol_result(result: &Value, cwd: &Path) -> String {
    let Some(arr) = result.as_array() else {
        return NO_WORKSPACE_SYMBOLS.to_string();
    };
    if arr.is_empty() {
        return NO_WORKSPACE_SYMBOLS.to_string();
    }
    let valid: Vec<&Value> = arr
        .iter()
        .filter(|sym| {
            sym.get("location")
                .and_then(|l| l.get("uri"))
                .and_then(Value::as_str)
                .is_some_and(|u| !u.is_empty())
        })
        .collect();
    if valid.is_empty() {
        return NO_WORKSPACE_SYMBOLS.to_string();
    }
    let mut lines: Vec<String> = vec![format!(
        "Found {} {} in workspace:",
        valid.len(),
        plural(valid.len(), "symbol")
    )];
    let by_file = group_by_file(&valid, |sym| {
        Some(format_uri(
            sym.get("location")
                .and_then(|l| l.get("uri"))
                .and_then(Value::as_str),
            cwd,
        ))
    });
    for (file_path, symbols) in &by_file {
        lines.push(format!("\n{file_path}:"));
        for symbol in symbols {
            let kind = symbol_kind_to_string(symbol);
            let (line, _) = range_start(symbol.get("location").and_then(|l| l.get("range")));
            let mut symbol_line = format!("  {} ({kind}) - Line {}", name_of(symbol), line + 1);
            if let Some(container) = truthy_str(symbol, "containerName") {
                symbol_line.push_str(&format!(" in {container}"));
            }
            lines.push(symbol_line);
        }
    }
    lines.join("\n")
}

/// `formatters.ts:formatCallHierarchyItem`.
fn format_call_hierarchy_item(item: &Value, cwd: &Path) -> String {
    let kind = symbol_kind_to_string(item);
    let uri = item
        .get("uri")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty());
    let Some(uri) = uri else {
        return format!("{} ({kind}) - <unknown location>", name_of(item));
    };
    let file_path = format_uri(Some(uri), cwd);
    let (line, _) = range_start(item.get("range"));
    let mut result = format!("{} ({kind}) - {file_path}:{}", name_of(item), line + 1);
    if let Some(detail) = truthy_str(item, "detail") {
        result.push_str(&format!(" [{detail}]"));
    }
    result
}

/// `formatPrepareCallHierarchyResult`.
fn format_prepare_call_hierarchy_result(result: &Value, cwd: &Path) -> String {
    let Some(arr) = result.as_array() else {
        return NO_CALL_HIERARCHY_ITEM.to_string();
    };
    if arr.is_empty() {
        return NO_CALL_HIERARCHY_ITEM.to_string();
    }
    if arr.len() == 1 {
        return format!(
            "Call hierarchy item: {}",
            format_call_hierarchy_item(&arr[0], cwd)
        );
    }
    let mut lines: Vec<String> = vec![format!("Found {} call hierarchy items:", arr.len())];
    for item in arr {
        lines.push(format!("  {}", format_call_hierarchy_item(item, cwd)));
    }
    lines.join("\n")
}

/// Render the `fromRanges` call-site suffix (`6:9, 10:3`).
fn call_sites(call: &Value) -> Option<String> {
    let ranges = call.get("fromRanges").and_then(Value::as_array)?;
    if ranges.is_empty() {
        return None;
    }
    Some(
        ranges
            .iter()
            .map(|r| {
                let (line, character) = range_start(Some(r));
                format!("{}:{}", line + 1, character + 1)
            })
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// `formatIncomingCallsResult`.
fn format_incoming_calls_result(result: &Value, cwd: &Path) -> String {
    let Some(arr) = result.as_array() else {
        return NO_INCOMING_CALLS.to_string();
    };
    if arr.is_empty() {
        return NO_INCOMING_CALLS.to_string();
    }
    let mut lines: Vec<String> = vec![format!(
        "Found {} incoming {}:",
        arr.len(),
        plural(arr.len(), "call")
    )];
    let items: Vec<&Value> = arr.iter().collect();
    let by_file = group_by_file(&items, |call| {
        let from = call.get("from")?;
        Some(format_uri(from.get("uri").and_then(Value::as_str), cwd))
    });
    for (file_path, calls) in &by_file {
        lines.push(format!("\n{file_path}:"));
        for call in calls {
            let Some(from) = call.get("from") else {
                continue;
            };
            let kind = symbol_kind_to_string(from);
            let (line, _) = range_start(from.get("range"));
            let mut call_line = format!("  {} ({kind}) - Line {}", name_of(from), line + 1);
            if let Some(sites) = call_sites(call) {
                call_line.push_str(&format!(" [calls at: {sites}]"));
            }
            lines.push(call_line);
        }
    }
    lines.join("\n")
}

/// `formatOutgoingCallsResult`.
fn format_outgoing_calls_result(result: &Value, cwd: &Path) -> String {
    let Some(arr) = result.as_array() else {
        return NO_OUTGOING_CALLS.to_string();
    };
    if arr.is_empty() {
        return NO_OUTGOING_CALLS.to_string();
    }
    let mut lines: Vec<String> = vec![format!(
        "Found {} outgoing {}:",
        arr.len(),
        plural(arr.len(), "call")
    )];
    let items: Vec<&Value> = arr.iter().collect();
    let by_file = group_by_file(&items, |call| {
        let to = call.get("to")?;
        Some(format_uri(to.get("uri").and_then(Value::as_str), cwd))
    });
    for (file_path, calls) in &by_file {
        lines.push(format!("\n{file_path}:"));
        for call in calls {
            let Some(to) = call.get("to") else {
                continue;
            };
            let kind = symbol_kind_to_string(to);
            let (line, _) = range_start(to.get("range"));
            let mut call_line = format!("  {} ({kind}) - Line {}", name_of(to), line + 1);
            if let Some(sites) = call_sites(call) {
                call_line.push_str(&format!(" [called from: {sites}]"));
            }
            lines.push(call_line);
        }
    }
    lines.join("\n")
}

/// Count DocumentSymbols including nested children (`countSymbols`).
fn count_symbols(symbols: &[Value]) -> u64 {
    let mut count = symbols.len() as u64;
    for symbol in symbols {
        if let Some(children) = symbol.get("children").and_then(Value::as_array) {
            if !children.is_empty() {
                count += count_symbols(children);
            }
        }
    }
    count
}

/// Port of `LSPTool.ts:636 formatResult` — returns the model-facing formatted
/// string plus the `resultCount` / `fileCount` summary fields. `result` is the
/// already-gitignore-filtered LSP payload.
fn format_result(operation: &str, result: &Value, cwd: &Path) -> (String, u64, u64) {
    match operation {
        "goToDefinition" | "goToImplementation" => {
            let formatted = format_go_to_definition_result(result, cwd);
            let raw_results: Vec<&Value> = if let Some(a) = result.as_array() {
                a.iter().collect()
            } else if result.is_null() {
                Vec::new()
            } else {
                vec![result]
            };
            let valid_uris: Vec<&str> = raw_results
                .iter()
                .filter_map(|it| to_location(it).0)
                .filter(|u| !u.is_empty())
                .collect();
            let result_count = valid_uris.len() as u64;
            let file_count = unique_uri_count(valid_uris.iter().copied());
            (formatted, result_count, file_count)
        }
        "findReferences" => {
            let formatted = format_find_references_result(result, cwd);
            let valid_uris: Vec<&str> = result
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|loc| loc.get("uri").and_then(Value::as_str))
                        .filter(|u| !u.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let result_count = valid_uris.len() as u64;
            let file_count = unique_uri_count(valid_uris.iter().copied());
            (formatted, result_count, file_count)
        }
        "hover" => {
            let formatted = format_hover_result(result);
            let n = u64::from(!result.is_null());
            (formatted, n, n)
        }
        "documentSymbol" => {
            let formatted = format_document_symbol_result(result, cwd);
            let symbols = result.as_array().map(Vec::as_slice).unwrap_or_default();
            let is_document_symbol =
                !symbols.is_empty() && symbols[0].is_object() && symbols[0].get("range").is_some();
            let count = if is_document_symbol {
                count_symbols(symbols)
            } else {
                symbols.len() as u64
            };
            let file_count = u64::from(!symbols.is_empty());
            (formatted, count, file_count)
        }
        "workspaceSymbol" => {
            let formatted = format_workspace_symbol_result(result, cwd);
            let valid_uris: Vec<&str> = result
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|sym| {
                            sym.get("location")
                                .and_then(|l| l.get("uri"))
                                .and_then(Value::as_str)
                        })
                        .filter(|u| !u.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let result_count = valid_uris.len() as u64;
            let file_count = unique_uri_count(valid_uris.iter().copied());
            (formatted, result_count, file_count)
        }
        "prepareCallHierarchy" => {
            let formatted = format_prepare_call_hierarchy_result(result, cwd);
            let items = result.as_array().map(Vec::as_slice).unwrap_or_default();
            let result_count = items.len() as u64;
            let file_count = if items.is_empty() {
                0
            } else {
                unique_uri_count(
                    items
                        .iter()
                        .filter_map(|i| i.get("uri").and_then(Value::as_str)),
                )
            };
            (formatted, result_count, file_count)
        }
        "incomingCalls" => {
            let formatted = format_incoming_calls_result(result, cwd);
            let calls = result.as_array().map(Vec::as_slice).unwrap_or_default();
            let result_count = calls.len() as u64;
            let file_count = if calls.is_empty() {
                0
            } else {
                unique_uri_count(calls.iter().filter_map(|c| {
                    c.get("from")
                        .and_then(|f| f.get("uri"))
                        .and_then(Value::as_str)
                }))
            };
            (formatted, result_count, file_count)
        }
        "outgoingCalls" => {
            let formatted = format_outgoing_calls_result(result, cwd);
            let calls = result.as_array().map(Vec::as_slice).unwrap_or_default();
            let result_count = calls.len() as u64;
            let file_count = if calls.is_empty() {
                0
            } else {
                unique_uri_count(calls.iter().filter_map(|c| {
                    c.get("to")
                        .and_then(|t| t.get("uri"))
                        .and_then(Value::as_str)
                }))
            };
            (formatted, result_count, file_count)
        }
        // Unreachable: `call` validates the operation before dispatch.
        _ => (serde_json::to_string(result).unwrap_or_default(), 0, 0),
    }
}

// -- Tool struct -------------------------------------------------------------

/// Builtin tool — dispatches 4 LSP operations.
pub struct LSPTool {
    pub(crate) ctx: tool_api::BuiltinToolContext,
    /// Optional shared live-cwd cell (claude-code `getCwd()`/`Ct()`). When
    /// injected (desktop), relative `filePath` expansion and the gitignore-filter
    /// root follow the post-`cd` directory; when absent (mobile/tests) they fall
    /// back to the process cwd ([`current_cwd`]) — the prior behavior.
    live_cwd: Option<tool_api::LiveCwdCell>,
}

impl LSPTool {
    /// Construct a new [`LSPTool`] over the supplied context.
    #[must_use]
    pub fn new(ctx: tool_api::BuiltinToolContext) -> Self {
        Self {
            ctx,
            live_cwd: None,
        }
    }

    /// Inject the shared live-cwd cell (builder; default is `None`). The desktop
    /// composition root passes the SAME cell the `BashTool` writes on a `cd`.
    #[must_use]
    pub fn with_live_cwd(mut self, cell: tool_api::LiveCwdCell) -> Self {
        self.live_cwd = Some(cell);
        self
    }

    /// The effective live cwd: the injected cell's value if present, else the
    /// process cwd ([`current_cwd`], i.e. `env::current_dir()`) — the claude-code
    /// `Ct()` fallback for the no-cell case.
    fn cwd_now(&self) -> PathBuf {
        self.live_cwd
            .as_ref()
            .map_or_else(current_cwd, |c| c.lock().unwrap().clone())
    }

    fn lsp_registry(&self) -> Option<&Arc<LspRegistry>> {
        self.ctx.lsp_registry.as_ref()
    }

    fn input_file_path(input: &Value) -> Option<&str> {
        input
            .get("filePath")
            .and_then(Value::as_str)
            .or_else(|| input.get("file_path").and_then(Value::as_str))
    }
}

static LSP_TOOL_SCHEMA: Lazy<Value> = Lazy::new(|| {
    // Byte-faithful to claude-code's LSP zod schema (@183057616): `operation`,
    // `filePath`, 1-based `line`/`character`, optional `query`. There is NO
    // `server_name` — the server is resolved from `filePath`.
    json!({
        "type": "object",
        "properties": {
            "operation":   { "type": "string", "enum": ["goToDefinition", "findReferences", "hover", "documentSymbol", "workspaceSymbol", "goToImplementation", "prepareCallHierarchy", "incomingCalls", "outgoingCalls"], "description": "The LSP operation to perform" },
            "filePath":    { "type": "string", "description": "The absolute or relative path to the file" },
            "line":        { "type": "integer", "minimum": 1, "description": "The line number (1-based, as shown in editors)" },
            "character":   { "type": "integer", "minimum": 1, "description": "The character offset (1-based, as shown in editors)" },
            "query":       { "type": "string", "description": "The symbol name or partial name to search for (workspaceSymbol only). Most language servers return no results for an empty query, so always provide it when using workspaceSymbol." }
        },
        "required": ["operation", "filePath", "line", "character"]
    })
});

#[async_trait]
impl Tool for LSPTool {
    fn name(&self) -> &str {
        LSP_TOOL_NAME
    }
    /// 2.1.206 tool-definition `searchHint` (byte-verified).
    fn search_hint(&self) -> Option<&str> {
        Some("code intelligence (definitions, references, symbols, hover)")
    }
    fn input_schema(&self) -> &Value {
        &LSP_TOOL_SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        self.lsp_registry()
            .is_some_and(|registry| registry.is_tool_available())
    }
    fn is_lsp(&self) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        // claude-code `maxOutputChars: 1e5` for the LSP tool.
        100_000
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn is_destructive(&self, _: &Value) -> bool {
        false
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Cancel
    }

    fn get_path(&self, input: &Value) -> Option<PathBuf> {
        // `LSPTool.ts:152-154` `getPath({ filePath }) => expandPath(filePath)`,
        // where `expandPath` resolves relatives against the live `getCwd()`.
        let file_path = Self::input_file_path(input)?;
        Some(expand_path_in(file_path, &self.cwd_now()))
    }

    /// Port of `LSPTool.ts:155-209` `validateInput`: confirm the (expanded)
    /// path exists and is a regular file. Byte-exact TS messages.
    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let file_path = Self::input_file_path(input).unwrap_or_default();
        validate_file_path_in(file_path, &self.cwd_now())
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        allow_lsp()
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        LSP_TOOL_DESCRIPTION.into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        LSP_TOOL_DESCRIPTION.into()
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let operation = input
            .get("operation")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ToolError::InvalidInput("LSPTool: missing or non-string operation".into())
            })?
            .to_string();
        // claude-code dispatches by `filePath` and resolves the server itself —
        // there is NO model-supplied `server_name`.
        let file_path = Self::input_file_path(&input)
            .ok_or_else(|| {
                ToolError::InvalidInput("LSPTool: missing or non-string filePath".into())
            })?
            .to_string();
        // `workspaceSymbol` query (optional; ignored by the other operations).
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let line = input
            .get("line")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| ToolError::InvalidInput("LSPTool: missing or non-integer line".into()))?
            as u32;
        let character = input
            .get("character")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                ToolError::InvalidInput("LSPTool: missing or non-integer character".into())
            })? as u32;

        // 1-based position guard — emits no STARTED to keep paired events.
        if line < 1 || character < 1 {
            return Err(ToolError::InvalidInput(LSP_POSITION_ERROR.into()));
        }

        // Unknown-operation guard — also pre-STARTED.
        if !LSP_OPERATIONS_LOCKED.contains(&operation.as_str()) {
            return Err(ToolError::InvalidInput(format!(
                "LSPTool: unknown operation {operation:?}; allowed: goToDefinition, findReferences, hover, documentSymbol, workspaceSymbol, goToImplementation, prepareCallHierarchy, incomingCalls, outgoingCalls"
            )));
        }

        let bus = &self.ctx.bus;
        emit(
            bus,
            LSP_STARTED,
            &[
                ("_PROTO_operation", pii(&operation)),
                ("_PROTO_file_path", pii(&file_path)),
                ("line", verified_int(line as u64)),
                ("character", verified_int(character as u64)),
            ],
        )
        .await;

        let registry = match self.lsp_registry() {
            Some(r) => r,
            None => {
                emit(
                    bus,
                    LSP_FAILED,
                    &[
                        ("_PROTO_file_path", pii(&file_path)),
                        ("error_kind", verified_str("registry_unconfigured")),
                    ],
                )
                .await;
                return Ok(lsp_tool_result(
                    &operation,
                    &file_path,
                    "LSP server manager not initialized. This may indicate a startup issue."
                        .to_string(),
                    None,
                ));
            }
        };

        // `LSPTool.ts:225` `expandPath(input.filePath)` — tilde / relative path
        // resolution (against the live `getCwd()`) before the file is routed
        // to a server or opened.
        let live_cwd = self.cwd_now();
        let expanded = expand_path_in(&file_path, &live_cwd);
        let path = expanded.as_path();
        // `getCwd()` for the gitignore filter (`LSPTool.ts:226`) — live cwd.
        let cwd = live_cwd;
        let document = match registry.document_path_for_host_path(path, &cwd) {
            Ok(document) => document,
            Err(error) => {
                return Ok(lsp_tool_result(
                    &operation,
                    &file_path,
                    format!("Error performing {operation}: {error}"),
                    None,
                ));
            }
        };

        // Resolve (and start, if needed) the server responsible for this file —
        // claude-code routes by filePath, never a model-supplied server name.
        // A file with no configured server yields the documented "no server
        // available" error.
        let (server_name, client, config) = match registry
            .ensure_client_for_file_in_workspace(path, &cwd)
            .await
        {
            Ok(triple) => triple,
            Err(platform_api::LspError::Unavailable) => {
                let extension = path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .map(|extension| format!(".{extension}"))
                    .unwrap_or_default();
                return Ok(lsp_tool_result(
                    &operation,
                    &file_path,
                    format!("No LSP server available for file type: {extension}"),
                    None,
                ));
            }
            Err(e) => {
                emit(
                    bus,
                    LSP_FAILED,
                    &[
                        ("_PROTO_file_path", pii(&file_path)),
                        ("error_kind", verified_str("no_server_for_file")),
                    ],
                )
                .await;
                return Ok(lsp_tool_result(
                    &operation,
                    &file_path,
                    format!("Error performing {operation}: {e}"),
                    None,
                ));
            }
        };

        let tracker = registry.open_file_tracker();

        // `LSPTool.ts:427-` `getMethodAndParams` — per-operation dispatch. Position-
        // based ops use `line`/`character`; `documentSymbol` is file-level;
        // `workspaceSymbol` ignores the file and queries with an empty string
        // ("returns all symbols", `LSPTool.ts:475`); the call-hierarchy ops do the
        // two-step prepare-then-calls round-trip inside their `ops::` impl.
        let res = match operation.as_str() {
            "goToDefinition" => {
                ops::go_to_definition(&client, &tracker, &config, &document, line, character).await
            }
            "findReferences" => {
                ops::find_references(&client, &tracker, &config, &document, line, character, true)
                    .await
            }
            "hover" => ops::hover(&client, &tracker, &config, &document, line, character).await,
            "documentSymbol" => ops::document_symbol(&client, &tracker, &config, &document).await,
            "workspaceSymbol" => ops::workspace_symbol(&client, query).await,
            "goToImplementation" => {
                ops::go_to_implementation(&client, &tracker, &config, &document, line, character)
                    .await
            }
            "prepareCallHierarchy" => {
                ops::prepare_call_hierarchy(&client, &tracker, &config, &document, line, character)
                    .await
            }
            "incomingCalls" => {
                ops::incoming_calls(&client, &tracker, &config, &document, line, character).await
            }
            "outgoingCalls" => {
                ops::outgoing_calls(&client, &tracker, &config, &document, line, character).await
            }
            _ => unreachable!("validated above"),
        };

        match res {
            Ok(r) => {
                emit(
                    bus,
                    LSP_COMPLETED,
                    &[
                        ("_PROTO_operation", pii(&operation)),
                        ("_PROTO_server_name", pii(&server_name)),
                        (
                            "duration_ms",
                            verified_int(started.elapsed().as_millis() as u64),
                        ),
                    ],
                )
                .await;
                // `LSPTool.ts:336-374` — drop gitignored files from
                // location-bearing array results before returning.
                let mut normalized = r.raw;
                // URI projection is best-effort: an unmappable guest URI is
                // still a valid LSP result and must not fail the tool call.
                let _ = rewrite_server_uris_to_host(&mut normalized, registry.as_ref());
                let filtered = filter_gitignored_results(&operation, normalized, &cwd);
                // `LSPTool.ts:376-389` — format the FILTERED result into the
                // brief human-readable string TS gives the model, plus the
                // `resultCount`/`fileCount` summary. `model_content` is what the
                // model sees (the `mapToolResultToToolResultBlockParam`
                // `content: output.result` split); `result` mirrors the TS
                // `Output.result` field for the UI.
                let (formatted, result_count, file_count) =
                    format_result(&operation, &filtered, &cwd);
                Ok(lsp_tool_result(
                    &operation,
                    &file_path,
                    formatted,
                    Some((result_count, file_count)),
                ))
            }
            // `LSPTool.ts:265-272` — a file over the 10 MB cap is NOT an error;
            // it returns a graceful success whose `result` states the size in
            // MB (`Math.ceil(size / 1_000_000)`), not a hard byte error.
            Err(ops::LspOperationError::FileTooLarge { size, .. }) => {
                emit(
                    bus,
                    LSP_COMPLETED,
                    &[
                        ("_PROTO_operation", pii(&operation)),
                        ("_PROTO_server_name", pii(&server_name)),
                        (
                            "duration_ms",
                            verified_int(started.elapsed().as_millis() as u64),
                        ),
                    ],
                )
                .await;
                // `LSPTool.ts:265-272` — `Output` carries the message as
                // `result`; the model sees it via `model_content` (no
                // `resultCount`/`fileCount`, matching the TS `Output`).
                let result = file_too_large_message(size);
                Ok(lsp_tool_result(&operation, &file_path, result, None))
            }
            Err(e) => {
                emit(
                    bus,
                    LSP_FAILED,
                    &[
                        ("_PROTO_operation", pii(&operation)),
                        ("_PROTO_server_name", pii(&server_name)),
                        ("error_kind", verified_str("operation")),
                    ],
                )
                .await;
                Ok(lsp_tool_result(
                    &operation,
                    &file_path,
                    format!("Error performing {operation}: {e}"),
                    None,
                ))
            }
        }
    }
}

fn lsp_tool_result(
    operation: &str,
    file_path: &str,
    result: String,
    counts: Option<(u64, u64)>,
) -> ToolCallResult {
    let mut data = serde_json::Map::new();
    data.insert(
        "operation".to_string(),
        Value::String(operation.to_string()),
    );
    data.insert("result".to_string(), Value::String(result.clone()));
    data.insert("filePath".to_string(), Value::String(file_path.to_string()));
    if let Some((result_count, file_count)) = counts {
        data.insert("resultCount".to_string(), json!(result_count));
        data.insert("fileCount".to_string(), json!(file_count));
    }
    ToolCallResult {
        data: Value::Object(data),
        model_content: Some(result),
        new_messages: vec![],
        context_modifier: None,
        is_error: false,
        mcp_meta: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use jsonrpc::Connection;
    use protocol::McpConnectionId;
    use serde_json::Value;

    #[test]
    fn lsp_name_locked() {
        assert_eq!(LSP_TOOL_NAME, "LSP");
    }

    #[test]
    fn output_shape_and_key_order_match_claude_code_2_1_251() {
        let result = lsp_tool_result("hover", "src/lib.rs", "details".to_string(), Some((1, 1)));
        assert_eq!(
            serde_json::to_string(&result.data).unwrap(),
            r#"{"operation":"hover","result":"details","filePath":"src/lib.rs","resultCount":1,"fileCount":1}"#
        );
        assert_eq!(result.model_content.as_deref(), Some("details"));
    }

    #[test]
    fn public_schema_does_not_add_a_port_only_file_path_constraint() {
        assert!(LSP_TOOL_SCHEMA["properties"]["filePath"]
            .get("minLength")
            .is_none());
    }

    #[test]
    fn lsp_operations_locked_array_matches_constants() {
        // Locked to the TS surface order (`LSPTool.ts:62-72`).
        assert_eq!(
            LSP_OPERATIONS_LOCKED,
            [
                "goToDefinition",
                "findReferences",
                "hover",
                "documentSymbol",
                "workspaceSymbol",
                "goToImplementation",
                "prepareCallHierarchy",
                "incomingCalls",
                "outgoingCalls"
            ]
        );
        assert_eq!(LSP_OPERATION_GO_TO_DEFINITION, "goToDefinition");
        assert_eq!(LSP_OPERATION_FIND_REFERENCES, "findReferences");
        assert_eq!(LSP_OPERATION_HOVER, "hover");
        assert_eq!(LSP_OPERATION_DOCUMENT_SYMBOL, "documentSymbol");
        assert_eq!(LSP_OPERATION_WORKSPACE_SYMBOL, "workspaceSymbol");
        assert_eq!(LSP_OPERATION_GO_TO_IMPLEMENTATION, "goToImplementation");
        assert_eq!(LSP_OPERATION_PREPARE_CALL_HIERARCHY, "prepareCallHierarchy");
        assert_eq!(LSP_OPERATION_INCOMING_CALLS, "incomingCalls");
        assert_eq!(LSP_OPERATION_OUTGOING_CALLS, "outgoingCalls");
    }

    #[test]
    fn lsp_position_error_byte_locked() {
        assert_eq!(
            LSP_POSITION_ERROR,
            "LSP position must be 1-based (line >= 1, character >= 1)"
        );
    }

    #[test]
    fn lsp_server_not_running_template() {
        let s = format!("LSP server '{}' is not running", "rust-analyzer");
        assert_eq!(s, "LSP server 'rust-analyzer' is not running");
    }

    // -- LSP.1: too-large file → graceful MB success message -----------------

    #[test]
    fn file_too_large_message_reports_mb_byte_exact() {
        // 20 MB exactly → 20MB; matches `Math.ceil(20_000_000 / 1_000_000)`.
        assert_eq!(
            file_too_large_message(20_000_000),
            "File too large for LSP analysis (20MB exceeds 10MB limit)"
        );
        // One byte over 10 MB → ceil(10.000001) = 11MB (NOT bytes).
        assert_eq!(
            file_too_large_message(10_000_001),
            "File too large for LSP analysis (11MB exceeds 10MB limit)"
        );
        // 15.5 MB → ceil = 16MB.
        assert_eq!(
            file_too_large_message(15_500_000),
            "File too large for LSP analysis (16MB exceeds 10MB limit)"
        );
    }

    // -- LSP.2: validate_input existence / not-a-file ------------------------

    #[test]
    fn validate_input_missing_file_message_byte_exact() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.rs");
        let p = missing.to_string_lossy().into_owned();
        let err = validate_file_path(&p).unwrap_err();
        assert_eq!(err.0, format!("File does not exist: {p}"));
    }

    #[test]
    fn validate_input_directory_is_not_a_file_message_byte_exact() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_string_lossy().into_owned();
        let err = validate_file_path(&p).unwrap_err();
        assert_eq!(err.0, format!("Path is not a file: {p}"));
    }

    #[test]
    fn validate_input_regular_file_ok() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ok.rs");
        std::fs::write(&file, b"fn main() {}").unwrap();
        let p = file.to_string_lossy().into_owned();
        assert!(validate_file_path(&p).is_ok());
    }

    // -- LSP.3: tilde / relative path expansion ------------------------------

    #[test]
    fn expand_path_tilde_forms() {
        assert_eq!(expand_path("~"), home_dir());
        assert_eq!(expand_path("~/foo/bar"), home_dir().join("foo/bar"));
    }

    #[test]
    fn expand_path_absolute_unchanged() {
        assert_eq!(
            expand_path("/abs/path/file.rs"),
            PathBuf::from("/abs/path/file.rs")
        );
    }

    #[test]
    fn expand_path_relative_resolves_against_cwd() {
        let got = expand_path("rel/dir/file.rs");
        assert!(got.is_absolute());
        assert_eq!(got, current_cwd().join("rel/dir/file.rs"));
    }

    #[test]
    fn uri_to_file_path_decodes_percent_encoding() {
        assert_eq!(uri_to_file_path("file:///tmp/a%20b/c.rs"), "/tmp/a b/c.rs");
        // Malformed percent escape → fall back to the un-decoded body.
        assert_eq!(uri_to_file_path("file:///tmp/x%2"), "/tmp/x%2");
    }

    // -- LSP.4: gitignore filtering ------------------------------------------

    fn file_uri(p: &std::path::Path) -> String {
        format!("file://{}", p.to_string_lossy())
    }

    #[derive(Debug)]
    struct TestPathMapper;

    struct DummyTransport;

    #[async_trait]
    impl platform_api::LspTransport for DummyTransport {
        async fn start_server(
            &self,
            _config: &platform_api::LspServerConfig,
        ) -> Result<platform_api::LspRawConnection, platform_api::LspError> {
            Err(platform_api::LspError::Unavailable)
        }

        async fn initialize(
            &self,
            _conn: &platform_api::LspRawConnection,
            _root_uri: &str,
        ) -> Result<platform_api::LspServerCapabilities, platform_api::LspError> {
            Err(platform_api::LspError::Unavailable)
        }

        async fn connection(
            &self,
            _conn_id: McpConnectionId,
        ) -> Result<Arc<Connection>, platform_api::LspError> {
            Err(platform_api::LspError::Unavailable)
        }

        async fn request(
            &self,
            _connection: &platform_api::LspRawConnection,
            _method: &str,
            _params: Value,
        ) -> Result<Value, platform_api::LspError> {
            Err(platform_api::LspError::Unavailable)
        }

        async fn notify(
            &self,
            _connection: &platform_api::LspRawConnection,
            _method: &str,
            _params: Value,
        ) -> Result<(), platform_api::LspError> {
            Err(platform_api::LspError::Unavailable)
        }

        async fn shutdown(
            &self,
            _connection_id: McpConnectionId,
        ) -> Result<(), platform_api::LspError> {
            Ok(())
        }

        async fn terminate(
            &self,
            _connection_id: McpConnectionId,
        ) -> Result<(), platform_api::LspError> {
            Ok(())
        }

        async fn is_alive(&self, _connection_id: McpConnectionId) -> bool {
            false
        }

        fn is_available(&self) -> bool {
            true
        }
    }

    impl lsp::LspPathMapper for TestPathMapper {
        fn map_host_path(
            &self,
            path: &Path,
            _workspace_cwd: &Path,
        ) -> Result<lsp::LspDocumentPath, platform_api::LspError> {
            let uri = self.uri_for_host_path(path)?;
            Ok(lsp::LspDocumentPath {
                host_path: path.to_path_buf(),
                server_path: path.to_path_buf(),
                uri,
            })
        }

        fn workspace_root_for(
            &self,
            workspace_cwd: &Path,
        ) -> Result<PathBuf, platform_api::LspError> {
            Ok(workspace_cwd.to_path_buf())
        }

        fn uri_for_host_path(&self, path: &Path) -> Result<Url, platform_api::LspError> {
            Url::from_file_path(path).map_err(|()| {
                platform_api::LspError::Transport(format!(
                    "cannot convert path to file URI: {}",
                    path.display()
                ))
            })
        }

        fn host_path_for_uri(&self, uri: &Url) -> Result<Option<PathBuf>, platform_api::LspError> {
            let Ok(path) = uri.to_file_path() else {
                return Ok(None);
            };
            Ok(Some(PathBuf::from(
                path.to_string_lossy().replace("/guest/", "/host/"),
            )))
        }
    }

    #[test]
    fn rewrite_server_uris_maps_guest_paths_back_to_host() {
        let registry =
            LspRegistry::new(Arc::new(DummyTransport)).with_path_mapper(Arc::new(TestPathMapper));
        let mut raw = json!({
            "uri": "file:///guest/app/main.jsx",
            "targetUri": "file:///guest/app/impl.jsx",
            "nested": { "uri": "file:///guest/lib/util.js" }
        });

        rewrite_server_uris_to_host(&mut raw, &registry).expect("rewrite");

        assert_eq!(raw["uri"], "file:///host/app/main.jsx");
        assert_eq!(raw["targetUri"], "file:///host/app/impl.jsx");
        assert_eq!(raw["nested"]["uri"], "file:///host/lib/util.js");
    }

    #[test]
    fn filter_gitignored_drops_ignored_locations() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Real git repo so `git check-ignore` resolves.
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        std::fs::write(root.join(".gitignore"), b"ignored/\n").unwrap();
        std::fs::create_dir(root.join("ignored")).unwrap();
        std::fs::write(root.join("ignored/secret.rs"), b"x").unwrap();
        std::fs::write(root.join("keep.rs"), b"y").unwrap();

        let kept = file_uri(&root.join("keep.rs"));
        let dropped = file_uri(&root.join("ignored/secret.rs"));
        let raw = json!([
            { "uri": kept, "range": {} },
            { "uri": dropped, "range": {} },
        ]);

        let out = filter_gitignored_results("findReferences", raw, root);
        let arr = out.as_array().unwrap();
        assert_eq!(arr.len(), 1, "ignored location should be filtered out");
        assert_eq!(arr[0]["uri"].as_str().unwrap(), kept);
    }

    #[test]
    fn filter_gitignored_workspace_symbol_uses_location_uri() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .output()
            .unwrap();
        std::fs::write(root.join(".gitignore"), b"target/\n").unwrap();
        std::fs::create_dir(root.join("target")).unwrap();
        std::fs::write(root.join("target/gen.rs"), b"x").unwrap();
        std::fs::write(root.join("src.rs"), b"y").unwrap();

        let kept = file_uri(&root.join("src.rs"));
        let dropped = file_uri(&root.join("target/gen.rs"));
        let raw = json!([
            { "name": "A", "location": { "uri": kept, "range": {} } },
            { "name": "B", "location": { "uri": dropped, "range": {} } },
        ]);

        let out = filter_gitignored_results("workspaceSymbol", raw, root);
        let arr = out.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"].as_str().unwrap(), "A");
    }

    #[test]
    fn filter_gitignored_ignores_non_location_ops_and_non_arrays() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // hover (not a location op) — returned verbatim.
        let hover = json!({ "contents": "doc" });
        assert_eq!(
            filter_gitignored_results("hover", hover.clone(), root),
            hover
        );
        // goToDefinition single (non-array) Location — TS only filters arrays.
        let single = json!({ "uri": "file:///whatever.rs", "range": {} });
        assert_eq!(
            filter_gitignored_results("goToDefinition", single.clone(), root),
            single
        );
    }

    #[test]
    fn filter_gitignored_keeps_all_when_nothing_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .output()
            .unwrap();
        std::fs::write(root.join("a.rs"), b"x").unwrap();
        std::fs::write(root.join("b.rs"), b"y").unwrap();
        let raw = json!([
            { "uri": file_uri(&root.join("a.rs")), "range": {} },
            { "uri": file_uri(&root.join("b.rs")), "range": {} },
        ]);
        let out = filter_gitignored_results("goToImplementation", raw.clone(), root);
        assert_eq!(out, raw);
    }

    #[test]
    fn filter_gitignored_location_link_uses_target_uri() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .output()
            .unwrap();
        std::fs::write(root.join(".gitignore"), b"dist/\n").unwrap();
        std::fs::create_dir(root.join("dist")).unwrap();
        std::fs::write(root.join("dist/out.rs"), b"x").unwrap();
        std::fs::write(root.join("in.rs"), b"y").unwrap();

        let kept = file_uri(&root.join("in.rs"));
        let dropped = file_uri(&root.join("dist/out.rs"));
        // LocationLink shape: `targetUri` rather than `uri`.
        let raw = json!([
            { "targetUri": kept, "targetRange": {} },
            { "targetUri": dropped, "targetRange": {} },
        ]);
        let out = filter_gitignored_results("goToDefinition", raw, root);
        let arr = out.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["targetUri"].as_str().unwrap(), kept);
    }

    // -- LSP.5: result formatting (byte-exact, with-results + empty cases) ---

    fn cwd() -> PathBuf {
        PathBuf::from("/home/user/project")
    }

    fn loc(rel: &str, line: i64, character: i64) -> Value {
        json!({
            "uri": format!("file:///home/user/project/{rel}"),
            "range": { "start": { "line": line, "character": character },
                       "end": { "line": line, "character": character } },
        })
    }

    #[test]
    fn format_uri_renders_relative_when_shorter() {
        // file:// → percent-decoded → relative to cwd, shorter and not ../..
        assert_eq!(
            format_uri(Some("file:///home/user/project/src/main.rs"), &cwd()),
            "src/main.rs"
        );
        // Missing/empty URI → defensive backstop string.
        assert_eq!(format_uri(None, &cwd()), "<unknown location>");
        assert_eq!(format_uri(Some(""), &cwd()), "<unknown location>");
    }

    #[test]
    fn format_uri_keeps_absolute_when_relative_climbs_two_levels() {
        // ../../ outside cwd → keep the absolute path.
        assert_eq!(format_uri(Some("file:///etc/hosts"), &cwd()), "/etc/hosts");
    }

    #[test]
    fn go_to_definition_single_and_empty_and_multi() {
        // Single Location.
        let single = loc("src/main.rs", 9, 4);
        let (s, rc, fc) = format_result("goToDefinition", &single, &cwd());
        assert_eq!(s, "Defined in src/main.rs:10:5");
        assert_eq!((rc, fc), (1, 1));

        // Empty array → no-definition message.
        let (e, rc, fc) = format_result("goToDefinition", &json!([]), &cwd());
        assert_eq!(e, NO_DEFINITION);
        assert_eq!((rc, fc), (0, 0));

        // Multiple definitions.
        let multi = json!([loc("src/a.rs", 0, 0), loc("src/b.rs", 1, 2)]);
        let (m, rc, fc) = format_result("goToImplementation", &multi, &cwd());
        assert_eq!(m, "Found 2 definitions:\n  src/a.rs:1:1\n  src/b.rs:2:3");
        assert_eq!((rc, fc), (2, 2));
    }

    #[test]
    fn go_to_definition_location_link_uses_target_selection_range() {
        let link = json!([{
            "targetUri": "file:///home/user/project/src/x.rs",
            "targetRange": { "start": { "line": 0, "character": 0 } },
            "targetSelectionRange": { "start": { "line": 41, "character": 7 } },
        }]);
        let (s, _, _) = format_result("goToDefinition", &link, &cwd());
        assert_eq!(s, "Defined in src/x.rs:42:8");
    }

    #[test]
    fn find_references_one_grouped_and_empty() {
        let one = json!([loc("src/main.rs", 9, 4)]);
        let (s, rc, fc) = format_result("findReferences", &one, &cwd());
        assert_eq!(s, "Found 1 reference:\n  src/main.rs:10:5");
        assert_eq!((rc, fc), (1, 1));

        let grouped = json!([loc("a.rs", 0, 0), loc("a.rs", 4, 2), loc("b.rs", 1, 1)]);
        let (g, rc, fc) = format_result("findReferences", &grouped, &cwd());
        assert_eq!(
            g,
            "Found 3 references across 2 files:\n\na.rs:\n  Line 1:1\n  Line 5:3\n\nb.rs:\n  Line 2:2"
        );
        assert_eq!((rc, fc), (3, 2));

        let (e, rc, fc) = format_result("findReferences", &json!([]), &cwd());
        assert_eq!(e, NO_REFERENCES);
        assert_eq!((rc, fc), (0, 0));
    }

    #[test]
    fn hover_with_range_markup_and_null() {
        let hover = json!({
            "contents": { "kind": "markdown", "value": "fn foo() -> ()" },
            "range": { "start": { "line": 9, "character": 4 } },
        });
        let (s, rc, fc) = format_result("hover", &hover, &cwd());
        assert_eq!(s, "Hover info at 10:5:\n\nfn foo() -> ()");
        assert_eq!((rc, fc), (1, 1));

        // Array contents joined by blank lines; no range → bare content.
        let arr = json!({ "contents": ["line one", { "value": "line two" }] });
        let (a, _, _) = format_result("hover", &arr, &cwd());
        assert_eq!(a, "line one\n\nline two");

        let (e, rc, fc) = format_result("hover", &Value::Null, &cwd());
        assert_eq!(e, NO_HOVER);
        assert_eq!((rc, fc), (0, 0));
    }

    #[test]
    fn document_symbol_hierarchy_and_empty() {
        let symbols = json!([{
            "name": "Foo",
            "kind": 5,
            "range": { "start": { "line": 0, "character": 0 } },
            "children": [{
                "name": "bar",
                "kind": 6,
                "detail": "() -> ()",
                "range": { "start": { "line": 2, "character": 4 } },
            }],
        }]);
        let (s, rc, fc) = format_result("documentSymbol", &symbols, &cwd());
        assert_eq!(
            s,
            "Document symbols:\nFoo (Class) - Line 1\n  bar (Method) () -> () - Line 3"
        );
        // Counts nested children: Foo + bar = 2; one file.
        assert_eq!((rc, fc), (2, 1));

        let (e, rc, fc) = format_result("documentSymbol", &json!([]), &cwd());
        assert_eq!(e, NO_DOCUMENT_SYMBOLS);
        assert_eq!((rc, fc), (0, 0));
    }

    #[test]
    fn document_symbol_information_delegates_to_workspace() {
        // A `SymbolInformation[]` payload (has `location`) renders like the
        // workspace-symbol formatter.
        let symbols = json!([{
            "name": "Foo",
            "kind": 5,
            "location": loc("src/lib.rs", 0, 0),
        }]);
        let (s, _, _) = format_result("documentSymbol", &symbols, &cwd());
        assert_eq!(
            s,
            "Found 1 symbol in workspace:\n\nsrc/lib.rs:\n  Foo (Class) - Line 1"
        );
    }

    #[test]
    fn workspace_symbol_with_container_plural_and_empty() {
        let symbols = json!([
            { "name": "Foo", "kind": 5, "location": loc("src/lib.rs", 0, 0), "containerName": "mod" },
            { "name": "bar", "kind": 12, "location": loc("src/lib.rs", 9, 0) },
        ]);
        let (s, rc, fc) = format_result("workspaceSymbol", &symbols, &cwd());
        assert_eq!(
            s,
            "Found 2 symbols in workspace:\n\nsrc/lib.rs:\n  Foo (Class) - Line 1 in mod\n  bar (Function) - Line 10"
        );
        assert_eq!((rc, fc), (2, 1));

        let (e, rc, fc) = format_result("workspaceSymbol", &json!([]), &cwd());
        assert_eq!(e, NO_WORKSPACE_SYMBOLS);
        assert_eq!((rc, fc), (0, 0));
    }

    #[test]
    fn prepare_call_hierarchy_single_multi_and_empty() {
        let single = json!([{
            "name": "foo",
            "kind": 12,
            "uri": "file:///home/user/project/src/main.rs",
            "range": { "start": { "line": 9, "character": 0 } },
            "detail": "fn foo()",
        }]);
        let (s, rc, fc) = format_result("prepareCallHierarchy", &single, &cwd());
        assert_eq!(
            s,
            "Call hierarchy item: foo (Function) - src/main.rs:10 [fn foo()]"
        );
        assert_eq!((rc, fc), (1, 1));

        let multi = json!([
            { "name": "a", "kind": 12, "uri": "file:///home/user/project/src/a.rs",
              "range": { "start": { "line": 0, "character": 0 } } },
            { "name": "b", "kind": 6, "uri": "file:///home/user/project/src/b.rs",
              "range": { "start": { "line": 4, "character": 0 } } },
        ]);
        let (m, rc, fc) = format_result("prepareCallHierarchy", &multi, &cwd());
        assert_eq!(
            m,
            "Found 2 call hierarchy items:\n  a (Function) - src/a.rs:1\n  b (Method) - src/b.rs:5"
        );
        assert_eq!((rc, fc), (2, 2));

        let (e, rc, fc) = format_result("prepareCallHierarchy", &json!([]), &cwd());
        assert_eq!(e, NO_CALL_HIERARCHY_ITEM);
        assert_eq!((rc, fc), (0, 0));
    }

    #[test]
    fn incoming_calls_with_sites_and_empty() {
        let calls = json!([{
            "from": {
                "name": "caller",
                "kind": 12,
                "uri": "file:///home/user/project/src/a.rs",
                "range": { "start": { "line": 4, "character": 0 } },
            },
            "fromRanges": [
                { "start": { "line": 5, "character": 8 } },
                { "start": { "line": 9, "character": 2 } },
            ],
        }]);
        let (s, rc, fc) = format_result("incomingCalls", &calls, &cwd());
        assert_eq!(
            s,
            "Found 1 incoming call:\n\nsrc/a.rs:\n  caller (Function) - Line 5 [calls at: 6:9, 10:3]"
        );
        assert_eq!((rc, fc), (1, 1));

        let (e, rc, fc) = format_result("incomingCalls", &json!([]), &cwd());
        assert_eq!(e, NO_INCOMING_CALLS);
        assert_eq!((rc, fc), (0, 0));
    }

    #[test]
    fn outgoing_calls_with_sites_and_empty() {
        let calls = json!([{
            "to": {
                "name": "callee",
                "kind": 6,
                "uri": "file:///home/user/project/src/b.rs",
                "range": { "start": { "line": 0, "character": 0 } },
            },
            "fromRanges": [{ "start": { "line": 1, "character": 0 } }],
        }]);
        let (s, rc, fc) = format_result("outgoingCalls", &calls, &cwd());
        assert_eq!(
            s,
            "Found 1 outgoing call:\n\nsrc/b.rs:\n  callee (Method) - Line 1 [called from: 2:1]"
        );
        assert_eq!((rc, fc), (1, 1));

        let (e, rc, fc) = format_result("outgoingCalls", &json!([]), &cwd());
        assert_eq!(e, NO_OUTGOING_CALLS);
        assert_eq!((rc, fc), (0, 0));
    }

    #[test]
    fn call_hierarchy_item_unknown_location_when_uri_missing() {
        let item = json!({ "name": "ghost", "kind": 12 });
        assert_eq!(
            format_call_hierarchy_item(&item, &cwd()),
            "ghost (Function) - <unknown location>"
        );
    }

    #[test]
    fn plural_matches_ts_string_utils() {
        assert_eq!(plural(1, "symbol"), "symbol");
        assert_eq!(plural(0, "symbol"), "symbols");
        assert_eq!(plural(3, "call"), "calls");
    }

    #[test]
    fn lsp_unknown_operation_template() {
        // `completion` is no longer a tool operation (TS LSPTool has no
        // completion op) — it is now rejected as unknown.
        let op = "completion";
        let s = format!(
            "LSPTool: unknown operation {op:?}; allowed: goToDefinition, findReferences, hover, documentSymbol, workspaceSymbol, goToImplementation, prepareCallHierarchy, incomingCalls, outgoingCalls"
        );
        assert_eq!(
            s,
            "LSPTool: unknown operation \"completion\"; allowed: goToDefinition, findReferences, hover, documentSymbol, workspaceSymbol, goToImplementation, prepareCallHierarchy, incomingCalls, outgoingCalls"
        );
    }

    // ── P2-08: relative-path expansion follows the live cwd ──────────────────

    #[test]
    fn expand_path_in_resolves_relative_against_the_given_cwd() {
        // A relative path resolves against the supplied cwd (the live getCwd()),
        // not the process cwd.
        assert_eq!(
            expand_path_in("rel/f.rs", Path::new("/live/cwd")),
            PathBuf::from("/live/cwd/rel/f.rs")
        );
        // The empty path yields the cwd itself.
        assert_eq!(
            expand_path_in("", Path::new("/live/cwd")),
            PathBuf::from("/live/cwd")
        );
        // Absolute + `~` forms ignore the cwd (unchanged by the parametrization).
        assert_eq!(
            expand_path_in("/abs/x.rs", Path::new("/live/cwd")),
            PathBuf::from("/abs/x.rs")
        );
        assert_eq!(expand_path_in("~", Path::new("/live/cwd")), home_dir());
    }

    #[test]
    fn injected_live_cwd_cell_drives_get_path() {
        // A tool built `with_live_cwd(cell)` expands a relative `filePath` against
        // the cell — so a Bash `cd` moves the LSP tool's relative-path root, 1:1
        // with claude-code's live `Ct()`.
        let ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            std::sync::Arc::new(AnalyticsBus::new()),
            vec![PathBuf::from("/workspace")],
        );
        let cell = std::sync::Arc::new(std::sync::Mutex::new(PathBuf::from("/live/cwd")));
        let tool = LSPTool::new(ctx).with_live_cwd(cell);
        assert_eq!(
            tool.get_path(&serde_json::json!({ "filePath": "rel/x.rs" })),
            Some(PathBuf::from("/live/cwd/rel/x.rs"))
        );
        assert_eq!(
            tool.get_path(&serde_json::json!({ "file_path": "legacy/x.rs" })),
            Some(PathBuf::from("/live/cwd/legacy/x.rs"))
        );
    }

    #[tokio::test]
    async fn validate_input_accepts_schema_file_path_field() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        let ctx = tool_api::test_support::ctx_for_file_tools(
            tool_api::test_support::make_dummy_fs(),
            std::sync::Arc::new(AnalyticsBus::new()),
            vec![dir.path().to_path_buf()],
        );
        let tool = LSPTool::new(ctx);
        let use_ctx = tool_api::test_support::fresh_ctx();
        assert!(tool
            .validate_input(
                &serde_json::json!({ "filePath": file.to_string_lossy() }),
                &use_ctx
            )
            .await
            .is_ok());
        assert!(tool
            .validate_input(
                &serde_json::json!({ "file_path": file.to_string_lossy() }),
                &use_ctx
            )
            .await
            .is_ok());
    }
}
