//! MCP config-load diagnostics — a byte-faithful port of claude-code 2.1.206's
//! `F7t` per-entry warnings (2.1.220's `klr`), including the
//! leading/trailing-whitespace scan (`ty_`) added in 2.1.219.
//!
//! `F7t` validates one config source's `mcpServers` and, for every entry it
//! drops, records a user-facing warning. The port's loader
//! ([`crate::json_config`]) already drops invalid entries (via `tracing::warn!`);
//! this module is an ADDITIVE pass that reproduces `F7t`'s exact warning
//! strings so they can be surfaced at startup and in `/doctor`, without
//! touching the loader.
//!
//! One string is not byte-reproducible: the `<issues>` detail inside
//! `Skipped — invalid MCP server config for "<name>": <issues>` comes from
//! Zod's validator; the port emits the exact message *shell* with a best-effort
//! reason. Every other `F7t` warning is byte-exact.

use crate::connection::ConfigScope;
use crate::normalization::is_reserved_mcp_server_name;
use serde_json::Value;
use std::path::Path;

/// claude `mcpErrorMetadata.severity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpConfigSeverity {
    /// A whole-source shape error (`F7t`'s top-level `!i.success`).
    Fatal,
    /// A per-entry skip (`F7t`'s `l(...)` warnings).
    Warning,
}

/// One MCP config diagnostic — claude `F7t`'s error/warning records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfigWarning {
    /// Originating config file, when known (claude `...o&&{file:o}`).
    pub file: Option<String>,
    /// Dotted path (`mcpServers.<name>` for per-entry, empty for shape errors).
    pub path: String,
    /// User-facing message (byte-exact vs `F7t`, except the invalid-config
    /// `<issues>` detail).
    pub message: String,
    /// Optional remediation hint (claude `...d&&{suggestion:d}`).
    pub suggestion: Option<String>,
    /// Config scope this source was loaded at.
    pub scope: ConfigScope,
    /// The offending server name, for per-entry warnings.
    pub server_name: Option<String>,
    /// Fatal (shape) vs warning (per-entry skip).
    pub severity: McpConfigSeverity,
}

impl McpConfigWarning {
    /// Render as a single stderr line, mirroring how claude logs these
    /// (`message` then a ` (<suggestion>)` tail when present).
    #[must_use]
    pub fn to_stderr_line(&self) -> String {
        match &self.suggestion {
            Some(s) => format!("{} ({s})", self.message),
            None => self.message.clone(),
        }
    }

    /// True for the "file not found" variant produced by
    /// [`read_mcp_config_file`]. Claude-code's own `"project"`-scope loader
    /// filters exactly this variant out before logging/surfacing it
    /// (`Iqe`'s callers: `F.filter(B=>!B.message.startsWith("MCP config file
    /// not found"))`) — a missing ancestor `.mcp.json` is routine, not an
    /// anomaly. Callers that surface [`McpConfigWarning`]s to a user (rather
    /// than just loading servers) should apply the same filter.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        self.message.starts_with("MCP config file not found")
    }
}

/// Byte cap claude-code 2.1.251 applies to every NON-dynamic-scope MCP config
/// file read (`Iqe`'s `var mcn=2097152`, binary offset 160911363).
pub const MCP_CONFIG_MAX_BYTES: u64 = 2_097_152;

/// The lowercase scope tag claude-code's `Iqe`/`F7t` embed in log lines and
/// `mcpErrorMetadata.scope` (`"local"|"user"|"project"|"dynamic"|...` — the
/// `ConfigScope` variant names are NOT used verbatim, oracle's are lowercase
/// single words).
fn oracle_scope_label(scope: ConfigScope) -> &'static str {
    match scope {
        ConfigScope::Settings(lingxi_core::types::SettingsScope::Local) => "local",
        ConfigScope::Settings(lingxi_core::types::SettingsScope::User) => "user",
        ConfigScope::Settings(lingxi_core::types::SettingsScope::Project) => "project",
        ConfigScope::Dynamic => "dynamic",
        ConfigScope::Enterprise => "enterprise",
        ConfigScope::ClaudeAi => "claudeai",
        ConfigScope::Settings(lingxi_core::types::SettingsScope::Managed) => "managed",
        ConfigScope::Agent => "agent",
    }
}

/// Byte-faithful port of claude-code 2.1.251's `Iqe`: the shape/size-guarded
/// read that precedes MCP config parsing.
///
/// Binary evidence (`Iqe` @160911493, offset range 160911363-160912816):
/// ```text
/// var mcn=2097152;
/// function Iqe(e){
///   let{filePath:t,expandVars:r,scope:o,bridgeSessionId:u}=e,d=le(),_;
///   try{
///     let A=o==="dynamic" ? d.readFileSync(t,{encoding:"utf8"}) : Atr(d,t,mcn);
///     if(A===null) return /* shape/size rejection */ ...
///     _=A
///   }catch(A){
///     if(E(A)==="ENOENT") return /* not-found */ ...
///     return /* other read error */ ...
///   }
///   ...
/// }
/// ```
///
/// - `scope == Dynamic` (the `--mcp-config <path>` CLI flag, confirmed at
///   binary offset 166778831 with `scope:"dynamic"`): reads the file with NO
///   shape/size check at all, matching the oracle's own `readFileSync`
///   branch. That flag is already gated on [`Path::is_file`] at its own call
///   site (`apps/cli/src/init.rs`) — the ONLY guard the oracle itself applies
///   to a dynamic-scope path.
/// - every other scope: the path must be a regular file (`Path::is_file`,
///   which follows symlinks — a symlink to a device/FIFO fails this exactly
///   as the suggestion text implies, a symlink to a regular file passes) of
///   at most [`MCP_CONFIG_MAX_BYTES`] bytes, or the read is rejected with a
///   typed, byte-exact [`McpConfigWarning`] instead of being attempted.
///
/// Binary-confirmed evidence for WHICH scopes actually reach `Iqe`: the
/// caller switch at offset 160900580 shows `case"project"` (walking the
/// ancestor directories for `.mcp.json`) and `case"enterprise"`
/// (`Iqe({filePath:J$t(),expandVars:t,scope:"enterprise"})`) calling
/// `Iqe(...)`, and the `--mcp-config` dynamic path at 166778794 doing the
/// same; but `case"user"` and `case"local"` call `xqe({configObject:...})`
/// directly on an ALREADY-PARSED settings object (`oe().mcpServers` /
/// `li().mcpServers`) and never invoke `Iqe` at all — so this guard is wired
/// into this port's project-scope `.mcp.json` reads AND both enterprise
/// `managed-mcp.json` reads ([`crate::enterprise_policy::load_enterprise_servers_at`]
/// and [`crate::enterprise_policy::enterprise_mcp_active_at`], the port's
/// `$7t`), never the global config file read (see the §23b report: applying
/// it there would be a fabrication, not a port).
///
/// # Errors
/// Returns a fatal [`McpConfigWarning`] for: shape/size rejection, "file not
/// found" (see [`McpConfigWarning::is_not_found`]), and any other I/O error.
pub fn read_mcp_config_file(path: &Path, scope: ConfigScope) -> Result<String, McpConfigWarning> {
    let file = path.to_string_lossy().into_owned();
    let label = oracle_scope_label(scope);
    if scope != ConfigScope::Dynamic {
        match std::fs::metadata(path) {
            Ok(meta) => {
                if !meta.is_file() || meta.len() > MCP_CONFIG_MAX_BYTES {
                    tracing::warn!(
                        path = %file,
                        scope = label,
                        "MCP config skipped for {file} (scope={label}): not a regular file or exceeds {MCP_CONFIG_MAX_BYTES} byte limit"
                    );
                    telemetry::emit_mcp_config_parse_gate(Some(telemetry::MCP_CONFIG_SHAPE_GATE));
                    return Err(McpConfigWarning {
                        file: Some(file.clone()),
                        path: String::new(),
                        message: format!(
                            "MCP config is not a regular file or exceeds {MCP_CONFIG_MAX_BYTES} bytes: {file}"
                        ),
                        suggestion: Some(
                            "Check that the path is a plain JSON file (not a device, FIFO, or symlink to one)"
                                .to_string(),
                        ),
                        scope,
                        server_name: None,
                        severity: McpConfigSeverity::Fatal,
                    });
                }
            }
            Err(e) => return Err(io_error_warning(e, &file, scope, label)),
        }
    }
    std::fs::read_to_string(path).map_err(|e| io_error_warning(e, &file, scope, label))
}

/// Shared tail of `Iqe`'s `catch` block: ENOENT vs every other I/O error.
fn io_error_warning(
    e: std::io::Error,
    file: &str,
    scope: ConfigScope,
    label: &str,
) -> McpConfigWarning {
    if e.kind() == std::io::ErrorKind::NotFound {
        // Oracle's `E(A)==="ENOENT"` branch returns immediately with NO
        // `n(...)` log call and NO `p(...)` telemetry — a missing config is
        // routine, not logged at all at this layer.
        return McpConfigWarning {
            file: Some(file.to_string()),
            path: String::new(),
            message: format!("MCP config file not found: {file}"),
            suggestion: Some("Check that the file path is correct".to_string()),
            scope,
            server_name: None,
            severity: McpConfigSeverity::Fatal,
        };
    }
    tracing::error!(
        path = %file,
        scope = label,
        error = %e,
        "MCP config read error for {file} (scope={label}): {e}"
    );
    telemetry::emit_mcp_config_parse_gate(Some(telemetry::MCP_CONFIG_READ_FAILED));
    McpConfigWarning {
        file: Some(file.to_string()),
        path: String::new(),
        message: format!("Failed to read file: {e}"),
        suggestion: Some("Check file permissions and ensure the file exists".to_string()),
        scope,
        server_name: None,
        severity: McpConfigSeverity::Fatal,
    }
}

/// Byte-faithful port of `Iqe`'s post-read JSON-parse guard
/// (`let C=Ut(_,!1);if(!C)return ...`).
///
/// The oracle emits TWO different strings here, not one: an internal log
/// line with full detail (`` `MCP config is not valid JSON: ${t} (scope=${o},
/// length=${_.length}, first100=${b(_.slice(0,100))})` ``) and a SHORT, FIXED
/// (non-interpolated) user-facing message — `"MCP config is not a valid
/// JSON"` — with no path/scope/length in it at all. This port reproduces
/// both, but the `length`/`first100` reproduction in the log line is
/// best-effort (Rust `char` count vs JS UTF-16 `.length`; `first100` quoted
/// via [`serde_json::to_string`], matching this port's existing convention
/// for the oracle's `b()` stringify helper — see
/// `json_config::McpTransportSpec` construction's `url_invalid` diagnostic).
///
/// # Errors
/// Returns a fatal [`McpConfigWarning`] when `raw` is not valid JSON.
pub fn parse_mcp_config_json(
    raw: &str,
    path: &Path,
    scope: ConfigScope,
) -> Result<Value, McpConfigWarning> {
    serde_json::from_str::<Value>(raw).map_err(|_| {
        let file = path.to_string_lossy().into_owned();
        let label = oracle_scope_label(scope);
        let length = raw.chars().count();
        let first100: String = raw.chars().take(100).collect();
        let quoted =
            serde_json::to_string(&first100).unwrap_or_else(|_| format!("{first100:?}"));
        tracing::error!(
            path = %file,
            scope = label,
            length,
            "MCP config is not valid JSON: {file} (scope={label}, length={length}, first100={quoted})"
        );
        telemetry::emit_mcp_config_parse_gate(Some(telemetry::MCP_CONFIG_INVALID_JSON));
        McpConfigWarning {
            file: Some(file),
            path: String::new(),
            message: "MCP config is not a valid JSON".to_string(),
            suggestion: Some("Fix the JSON syntax errors in the file".to_string()),
            scope,
            server_name: None,
            severity: McpConfigSeverity::Fatal,
        }
    })
}

/// The MCP server `type` values claude recognizes (`Alu`'s keys). An entry with
/// any other `type` string triggers the unknown-type warning.
const KNOWN_MCP_TYPES: &[&str] = &[
    "stdio",
    "sse",
    "http",
    "streamable-http",
    "ws",
    "sdk",
    "claudeai-proxy",
];

/// claude's byte-exact suggestion for the unknown-type warning.
const VALID_TYPES_SUGGESTION: &str =
    "Valid types are: stdio, sse, http (or streamable-http), ws, sdk";

/// Collect the `F7t` diagnostics for ONE config source (`configObject` = the
/// parsed file / config object holding `mcpServers`). `file` is the source path
/// for the `file:` field (and the `servers`-typo suggestion).
#[must_use]
pub fn collect_mcp_config_warnings(
    config: &Value,
    scope: ConfigScope,
    file: Option<&str>,
) -> Vec<McpConfigWarning> {
    // Top-level shape: `mcpServers` must be an object. The one byte-reproducible
    // shape error is the `"servers"` typo (claude's special-cased message); other
    // shape failures surface a Zod message we do not reproduce, so we stay quiet
    // there (the port also tolerates a bare map on some sources).
    let Some(Value::Object(servers)) = config.get("mcpServers") else {
        if config.is_object()
            && config.get("servers").is_some()
            && config.get("mcpServers").is_none()
        {
            return vec![McpConfigWarning {
                file: file.map(str::to_string),
                path: String::new(),
                message: "Missing \"mcpServers\" \u{2014} found \"servers\" instead. MCP servers must be configured in the \"mcpServers\" key.".to_string(),
                suggestion: Some(format!(
                    "Rename the top-level \"servers\" key to \"mcpServers\" in {}",
                    file.unwrap_or("your MCP config")
                )),
                scope,
                server_name: None,
                severity: McpConfigSeverity::Fatal,
            }];
        }
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut warn = |name: &str, message: String, suggestion: Option<String>| {
        out.push(McpConfigWarning {
            file: file.map(str::to_string),
            path: format!("mcpServers.{name}"),
            message,
            suggestion,
            scope,
            server_name: Some(name.to_string()),
            severity: McpConfigSeverity::Warning,
        });
    };

    for (name, entry) in servers {
        // `type` = the string `type`, else "stdio" (claude `typeof u.type==="string"?u.type:"stdio"`).
        let ty = entry.get("type").and_then(Value::as_str).unwrap_or("stdio");
        if !KNOWN_MCP_TYPES.contains(&ty) {
            warn(
                name,
                format!("Skipped \u{2014} unknown MCP server type \"{ty}\" for server \"{name}\""),
                Some(VALID_TYPES_SUGGESTION.to_string()),
            );
            continue;
        }
        let issues = validation_issues(entry, ty);
        if !issues.is_empty() {
            // The specific "url but no type" case (claude's dedicated branch).
            if entry.is_object()
                && entry.get("type").is_none()
                && entry.get("url").is_some()
                && entry.get("command").is_none()
            {
                warn(
                    name,
                    format!("Skipped \u{2014} MCP server \"{name}\" has a \"url\" but no \"type\"; add \"type\": \"http\" (or \"sse\" / \"ws\") to this entry"),
                    None,
                );
                continue;
            }
            warn(
                name,
                format!(
                    "Skipped \u{2014} invalid MCP server config for \"{name}\": {}",
                    issues.join("; ")
                ),
                None,
            );
            continue;
        }
        // Reserved name (unless SDK) — claude `TEt(c) && m.type!=="sdk"`.
        if is_reserved_mcp_server_name(name) && ty != "sdk" {
            warn(
                name,
                format!("\"{name}\" is a reserved MCP server name and was not loaded"),
                Some(format!(
                    "Rename this server in your MCP config \u{2014} \"{name}\" is reserved for internal use"
                )),
            );
            continue;
        }
        // Leading/trailing whitespace (claude `ty_`, new in 2.1.219): the entry
        // is still LOADED — the warning flags values that are "used exactly as
        // written". Runs before the missing-env expansion pass, matching
        // `klr`'s `let y=ty_(g)` placement. Both warnings can fire for one
        // entry.
        let ws_fields = collect_whitespace_fields(entry, ty);
        if !ws_fields.is_empty() {
            warn(
                name,
                format!(
                    "Leading or trailing whitespace in: {}",
                    ws_fields.join(", ")
                ),
                Some(format!(
                    "Remove the whitespace from these values in the \"{name}\" entry \u{2014} they are used exactly as written"
                )),
            );
        }
        // Missing env vars (claude `Osg` — only stdio/sse/http/ws expand).
        let missing = collect_missing_env_vars(entry, ty);
        if !missing.is_empty() {
            let joined = missing.join(", ");
            warn(
                name,
                format!("Missing environment variables: {joined}"),
                Some(format!("Set the following environment variables: {joined}")),
            );
        }
    }
    out
}

/// Loader validity per type (aligned with [`crate::json_config`]): stdio needs
/// a `command`, every remote type needs a `url` — EXCEPT `sdk`, whose oracle
/// schema (`MAn`) carries neither `command` nor `url` and instead REQUIRES its
/// own `name: i()`. The remote schemas
/// (`cLi` @226761199, `J5n` @226762069) declare `url: E.string()` with NO
/// `.min(1)` — unlike stdio's `command: E.string().min(1)` — so a
/// present-but-blank `url` is schema-VALID and the entry loads (it then
/// reports as `- Not configured`, claude `zar`).
/// Complete, stable issue list for one MCP entry. Unlike the old single
/// best-effort reason, this retains every failing field path so users can fix a
/// malformed record in one pass.
fn validation_issues(entry: &Value, ty: &str) -> Vec<String> {
    let Some(object) = entry.as_object() else {
        return vec![format!(
            "<root>: expected object, received {}",
            json_type_name(entry)
        )];
    };
    let mut issues = Vec::new();
    let require_nonempty_string = |key: &str, issues: &mut Vec<String>| match object.get(key) {
        Some(Value::String(value)) if !value.trim().is_empty() => {}
        Some(Value::String(_)) => issues.push(format!(
            "{key}: Too small: expected string to have >=1 characters"
        )),
        None => issues.push(format!("{key}: expected string, received undefined")),
        Some(value) => issues.push(format!(
            "{key}: expected string, received {}",
            json_type_name(value)
        )),
    };
    match ty {
        "stdio" => {
            require_nonempty_string("command", &mut issues);
            if let Some(args) = object.get("args") {
                match args {
                    Value::Array(values) => {
                        for (index, value) in values.iter().enumerate() {
                            if !value.is_string() {
                                issues.push(format!(
                                    "args.{index}: expected string, received {}",
                                    json_type_name(value)
                                ));
                            }
                        }
                    }
                    value => issues.push(format!(
                        "args: expected array, received {}",
                        json_type_name(value)
                    )),
                }
            }
            if let Some(env) = object.get("env") {
                match env {
                    Value::Object(values) => {
                        for (key, value) in values {
                            if !value.is_string() {
                                issues.push(format!(
                                    "env.{key}: expected string, received {}",
                                    json_type_name(value)
                                ));
                            }
                        }
                    }
                    value => issues.push(format!(
                        "env: expected record, received {}",
                        json_type_name(value)
                    )),
                }
            }
        }
        // Oracle `MAn` @154585319:
        // `f({type:N("sdk"),name:i(),timeout:o().optional(),alwaysLoad:q().optional()})`
        // — no `url` and no `command` field at all, so an sdk entry must NOT
        // be flagged for lacking a `url`. But `name` is a REQUIRED `i()`
        // (every sibling carries `.optional()`), so `{"type":"sdk"}` fails
        // `safeParse` and the oracle reports it. `i()` has no `.min(1)`, so
        // an EMPTY string is schema-valid — hence a plain required-string
        // check, not `require_nonempty_string`. See
        // [`crate::json_config::build_server_from_json_entry`].
        "sdk" => match object.get("name") {
            Some(Value::String(_)) => {}
            None => issues.push("name: expected string, received undefined".to_string()),
            Some(value) => issues.push(format!(
                "name: expected string, received {}",
                json_type_name(value)
            )),
        },
        // Oracle `NAn` @154585377:
        // `f({type:N("claudeai-proxy"),url:i(),id:i(),displayName:i().optional(),
        // iconUrl:i().optional(), ...})` — `url` AND `id` are both REQUIRED
        // `i()` (no `.min(1)`, so an EMPTY string satisfies either), in
        // pointed contrast to `displayName`/`iconUrl` and the rest of the
        // schema's tail. `id` has no analogue in any other union member, so
        // this is the one arm that needs both checks. See
        // [`crate::json_config::build_server_from_json_entry`].
        "claudeai-proxy" => {
            let require_present_string = |key: &str, issues: &mut Vec<String>| match object.get(key)
            {
                Some(Value::String(_)) => {}
                None => issues.push(format!("{key}: expected string, received undefined")),
                Some(value) => issues.push(format!(
                    "{key}: expected string, received {}",
                    json_type_name(value)
                )),
            };
            require_present_string("url", &mut issues);
            require_present_string("id", &mut issues);
        }
        _ => match object.get("url") {
            Some(Value::String(_)) => {}
            None => issues.push("url: expected string, received undefined".to_string()),
            Some(value) => issues.push(format!(
                "url: expected string, received {}",
                json_type_name(value)
            )),
        },
    }
    // `headers: De(i(),i()).optional()` is declared by `OAn`/`sGt`/`LAn`
    // only. `fYe` (stdio) does not declare it — nor does `NAn`
    // (claudeai-proxy, @154585377), whose keys are exactly
    // `type,url,id,displayName,iconUrl,timeout,alwaysLoad,toolPermissions,
    // stateless,cachedInitResponse,discoverSupport,cachedDiscoverResponse,
    // eligible,ineligibleReason,enterpriseManaged`. `f` (@154568943) is a
    // catchall-free `z.object`, so a malformed `headers` on either of those
    // two is STRIPPED, not reported. Keeping the check for `claudeai-proxy`
    // made this warn about an entry
    // `json_config::strip_to_claudeai_proxy_schema` now loads.
    if !matches!(ty, "stdio" | "claudeai-proxy") {
        if let Some(headers) = object.get("headers") {
            match headers {
                Value::Object(values) => {
                    for (key, value) in values {
                        if !value.is_string() {
                            issues.push(format!(
                                "headers.{key}: expected string, received {}",
                                json_type_name(value)
                            ));
                        }
                    }
                }
                value => issues.push(format!(
                    "headers: expected record, received {}",
                    json_type_name(value)
                )),
            }
        }
    }
    if let Some(timeout) = object.get("timeout") {
        if timeout.as_u64().is_none_or(|value| value == 0) {
            issues.push("timeout: expected positive integer".to_string());
        }
    }
    if let Some(always_load) = object.get("alwaysLoad") {
        if !always_load.is_boolean() {
            issues.push("alwaysLoad: expected boolean".to_string());
        }
    }
    if crate::json_config::bare_elicitation_is_schema_key_for(Some(ty)) {
        if let Some(bare) = object.get("bareElicitationCapability") {
            if !bare.is_boolean() {
                issues.push(format!(
                    "bareElicitationCapability: expected boolean, received {}",
                    json_type_name(bare)
                ));
            }
        }
    }
    // §11 — `discoveryCache: q().optional()` is declared by `OAn`/`sGt`
    // (`sse` / `http` / `streamable-http`) ONLY, and with NO `.catch`, so a
    // present-but-non-boolean value fails `safeParse` there and is an
    // unknown-and-stripped key everywhere else. Without this the loader
    // rejected the entry (`json_config::discovery_cache_flag`) while
    // diagnostics stayed silent, so the server vanished from `mcp list` with
    // no `Skipped —` line naming the field.
    if crate::json_config::discovery_cache_is_schema_key_for(Some(ty)) {
        if let Some(discovery_cache) = object.get("discoveryCache") {
            if !discovery_cache.is_boolean() {
                issues.push(format!(
                    "discoveryCache: expected boolean, received {}",
                    json_type_name(discovery_cache)
                ));
            }
        }
    }
    issues
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// claude `ty_` (2.1.219) — config fields whose value carries leading or
/// trailing whitespace (`value !== value.trim()`). Field labels are byte-exact:
/// `command`, `url`, `args[<i>]`, `env.<key>` / `headers.<key>` for values, and
/// `env name <json>` / `header name <json>` (the key JSON-quoted via `Ie` =
/// `JSON.stringify`) when the KEY itself has whitespace — the value of such a
/// key is still checked under its raw `env.<key>` label.
///
/// `ty_` runs on the schema-VALIDATED entry, whose zod parse strips fields the
/// transport type does not declare; the port mirrors that by gating on the
/// type family (stdio → command/args/env, everything else → url/headers).
fn collect_whitespace_fields(entry: &Value, ty: &str) -> Vec<String> {
    let mut fields: Vec<String> = Vec::new();
    fn check(label: String, value: &str, fields: &mut Vec<String>) {
        if value != value.trim() {
            fields.push(label);
        }
    }
    // `Ie` = JSON.stringify (string serialization cannot fail).
    fn json_quote(s: &str) -> String {
        serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
    }
    match ty {
        "stdio" => {
            if let Some(c) = entry.get("command").and_then(Value::as_str) {
                check("command".to_string(), c, &mut fields);
            }
            if let Some(args) = entry.get("args").and_then(Value::as_array) {
                for (i, a) in args.iter().enumerate() {
                    if let Some(s) = a.as_str() {
                        check(format!("args[{i}]"), s, &mut fields);
                    }
                }
            }
            if let Some(env) = entry.get("env").and_then(Value::as_object) {
                for (k, v) in env {
                    if k != k.trim() {
                        fields.push(format!("env name {}", json_quote(k)));
                    }
                    if let Some(s) = v.as_str() {
                        check(format!("env.{k}"), s, &mut fields);
                    }
                }
            }
        }
        _ => {
            if let Some(u) = entry.get("url").and_then(Value::as_str) {
                check("url".to_string(), u, &mut fields);
            }
            if let Some(h) = entry.get("headers").and_then(Value::as_object) {
                for (k, v) in h {
                    if k != k.trim() {
                        fields.push(format!("header name {}", json_quote(k)));
                    }
                    if let Some(s) = v.as_str() {
                        check(format!("headers.{k}"), s, &mut fields);
                    }
                }
            }
        }
    }
    fields
}

/// claude `Osg` — the env-var references left unresolved after expanding the
/// fields Osg expands (stdio: command/args/env values; sse/http/ws: url/headers
/// values; other types expand nothing). Deduped, first-seen order (`Fo`).
///
/// `streamable-http` belongs to the url/headers family even though `Osg`
/// (`fAn` @160896200) has no `case "streamable-http"`: it runs on the PARSED
/// entry (`xqe` @160909118: `let fe=me.data; … ge=r?fAn(fe):void 0`) and
/// `sGt` (@154584848) declares `type: ie(["http","streamable-http"])
/// .transform(()=>"http")`, so a `streamable-http` entry reaches `fAn` already
/// retyped as `"http"` and DOES expand. `ty` here is the RAW config string
/// (pre-transform), so the alias must be listed explicitly.
fn collect_missing_env_vars(entry: &Value, ty: &str) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    let push = |s: &str, all: &mut Vec<String>| {
        all.extend(crate::env_expansion::expand_env_vars_in_string(s).missing_vars);
    };
    match ty {
        "stdio" => {
            if let Some(c) = entry.get("command").and_then(Value::as_str) {
                push(c, &mut all);
            }
            if let Some(args) = entry.get("args").and_then(Value::as_array) {
                for a in args {
                    if let Some(s) = a.as_str() {
                        push(s, &mut all);
                    }
                }
            }
            if let Some(env) = entry.get("env").and_then(Value::as_object) {
                for v in env.values() {
                    if let Some(s) = v.as_str() {
                        push(s, &mut all);
                    }
                }
            }
        }
        "sse" | "http" | "streamable-http" | "ws" => {
            if let Some(u) = entry.get("url").and_then(Value::as_str) {
                push(u, &mut all);
            }
            if let Some(h) = entry.get("headers").and_then(Value::as_object) {
                for v in h.values() {
                    if let Some(s) = v.as_str() {
                        push(s, &mut all);
                    }
                }
            }
        }
        // sdk / claudeai-proxy / ide → `fAn` passes the entry through
        // untouched (`case"claudeai-proxy":u=e;break`), so Osg expands nothing.
        _ => {}
    }
    // `Fo` — dedup preserving first-seen order.
    let mut seen = std::collections::HashSet::new();
    all.into_iter().filter(|v| seen.insert(v.clone())).collect()
}

/// Collect `F7t` diagnostics across every file-based MCP config source the
/// loader reads — the project `.mcp.json` (at `cwd`), and the user + local
/// `mcpServers` in the global config file — so startup and `/doctor` can report
/// them. Silent (empty) when configs are clean, so a healthy setup prints
/// nothing.
#[must_use]
pub fn collect_all_mcp_config_warnings(
    cwd: &std::path::Path,
    global_config_path: Option<&std::path::Path>,
) -> Vec<McpConfigWarning> {
    collect_all_mcp_config_warnings_at(&cwd.join(".mcp.json"), cwd, global_config_path)
}

/// [`collect_all_mcp_config_warnings`] with an EXPLICIT project `.mcp.json`
/// path, for callers whose loader discovers the project file by walking up
/// from `cwd` (the oracle resolves the project config at the workspace root,
/// not literally `<cwd>/.mcp.json`). `cwd` still keys the local scope.
#[must_use]
pub fn collect_all_mcp_config_warnings_at(
    project_mcp_path: &std::path::Path,
    cwd: &std::path::Path,
    global_config_path: Option<&std::path::Path>,
) -> Vec<McpConfigWarning> {
    let mut out = Vec::new();

    // Project scope: the discovered `.mcp.json`. Byte-faithful `Iqe` guard
    // (shape/size check, then JSON-parse) — see [`read_mcp_config_file`] and
    // [`parse_mcp_config_json`]. A missing file is routine (claude-code's own
    // `"project"`-scope loader filters this variant out before surfacing it,
    // see [`McpConfigWarning::is_not_found`]) so it is silently skipped here
    // too, matching the oracle; every OTHER rejection (shape/size, other I/O
    // error, invalid JSON) is surfaced as a typed warning.
    let project = project_mcp_path;
    match read_mcp_config_file(
        project,
        ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
    ) {
        Ok(raw) => match parse_mcp_config_json(
            &raw,
            project,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        ) {
            Ok(v) => {
                // oracle: `return y("mcp_config_parse"), xqe({...})` — the
                // success half of the gate, fired right where `Iqe` hands the
                // parsed object off to its caller.
                telemetry::emit_mcp_config_parse_gate(None);
                out.extend(collect_mcp_config_warnings(
                    &v,
                    ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
                    Some(&project.to_string_lossy()),
                ))
            }
            Err(warning) => out.push(warning),
        },
        Err(warning) if warning.is_not_found() => {}
        Err(warning) => out.push(warning),
    }

    // User + Local scope: the global config file's top-level `mcpServers`
    // (user) and `projects.<cwd-key>.mcpServers` (local). NOT behind `Iqe` in
    // the oracle: claude-code's `"user"`/`"local"` loaders consume an
    // ALREADY-PARSED settings object (`oe().mcpServers` / `li().mcpServers`)
    // and never call `Iqe` themselves (see [`read_mcp_config_file`]'s doc for
    // the binary evidence), so this read intentionally keeps its pre-existing
    // lenient handling rather than the shape/size guard above — applying that
    // guard's byte-exact strings here would misrepresent oracle behaviour,
    // not port it.
    let read_json_lenient = |p: &std::path::Path| -> Option<Value> {
        serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
    };
    if let Some(gp) = global_config_path {
        if let Some(v) = read_json_lenient(gp) {
            let file = gp.to_string_lossy();
            out.extend(collect_mcp_config_warnings(
                &v,
                ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
                Some(&file),
            ));
            let key = migrations::global_config::project_path_for_config(cwd);
            if let Some(proj) = v.get("projects").and_then(|p| p.get(&key)) {
                out.extend(collect_mcp_config_warnings(
                    proj,
                    ConfigScope::Settings(lingxi_core::types::SettingsScope::Local),
                    Some(&file),
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn only(config: &Value) -> Vec<McpConfigWarning> {
        collect_mcp_config_warnings(
            config,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
            Some("/p/.mcp.json"),
        )
    }

    #[test]
    fn unknown_type_warning_is_byte_exact() {
        let c = json!({"mcpServers":{"srv":{"type":"grpc","url":"x"}}});
        let w = only(&c);
        assert_eq!(w.len(), 1);
        assert_eq!(
            w[0].message,
            "Skipped \u{2014} unknown MCP server type \"grpc\" for server \"srv\""
        );
        assert_eq!(w[0].suggestion.as_deref(), Some(VALID_TYPES_SUGGESTION));
        assert_eq!(w[0].path, "mcpServers.srv");
        assert_eq!(w[0].server_name.as_deref(), Some("srv"));
    }

    #[test]
    fn url_without_type_warning_is_byte_exact() {
        let c = json!({"mcpServers":{"remote":{"url":"https://x"}}});
        let w = only(&c);
        assert_eq!(
            w[0].message,
            "Skipped \u{2014} MCP server \"remote\" has a \"url\" but no \"type\"; add \"type\": \"http\" (or \"sse\" / \"ws\") to this entry"
        );
        assert!(w[0].suggestion.is_none());
    }

    #[test]
    fn invalid_config_shell_is_byte_exact() {
        // stdio (explicit) with no command → invalid.
        let c = json!({"mcpServers":{"bad":{"type":"stdio"}}});
        let w = only(&c);
        assert!(w[0]
            .message
            .starts_with("Skipped \u{2014} invalid MCP server config for \"bad\": "));
    }

    /// A PRESENT-but-empty `url` is NOT a config warning.
    ///
    /// Verified by running both binaries over the same `.mcp.json`
    /// (`{"blank":{"type":"http","url":""}}`): the oracle listed the server and
    /// emitted NO diagnostic for it, warning only about a different server's
    /// unresolved `${VAR}`. The entry is accepted here and refused at CONNECT
    /// as UNCONFIGURED (`zar`).
    ///
    /// This test previously asserted a `Skipped - invalid MCP server config`
    /// warning. That was wrong twice over: the oracle does not emit it, and
    /// once the loader stopped dropping these entries the word "Skipped" was
    /// simply false — the server is listed.
    #[test]
    fn blank_remote_url_is_schema_valid_and_only_absent_url_is_required() {
        // `cLi`/`J5n` declare `url: E.string()` with no `.min(1)`: a blank url
        // PASSES the schema, so the entry loads (and reports as
        // `- Not configured`). A WHITESPACE-only url is schema-valid too; the
        // only thing said about it is `ty_`'s whitespace notice.
        let w = only(&json!({"mcpServers":{"bad":{"type":"http","url":"   "}}}));
        assert_eq!(
            w.len(),
            1,
            "whitespace url is not a schema violation: {w:?}"
        );
        assert_eq!(w[0].message, "Leading or trailing whitespace in: url");
        // A truly empty string is valid AND whitespace-clean ⇒ nothing at all.
        assert!(only(&json!({"mcpServers":{"bad":{"type":"http","url":""}}})).is_empty());
    }

    /// An ABSENT `url` on a remote type IS still invalid — that is the case the
    /// schema rejects, and it carries Zod 4's missing-value issue.
    #[test]
    fn a_missing_remote_url_is_still_reported_as_required() {
        let w = only(&json!({"mcpServers":{"bad":{"type":"http"}}}));
        assert_eq!(w.len(), 1);
        assert_eq!(
            w[0].message,
            "Skipped \u{2014} invalid MCP server config for \"bad\": url: expected string, received undefined"
        );
    }

    #[test]
    fn malformed_bare_elicitation_flag_is_reported_only_where_declared() {
        for ty in ["stdio", "http", "sse", "streamable-http", "ws"] {
            let warnings = only(
                &json!({"mcpServers":{"srv":{"type":ty,"command":"mcp-test","url":"https://mcp.test","bareElicitationCapability":"true"}}}),
            );
            assert_eq!(warnings.len(), 1, "{ty}: {warnings:?}");
            assert_eq!(warnings[0].message, "Skipped \u{2014} invalid MCP server config for \"srv\": bareElicitationCapability: expected boolean, received string");
        }
        for ty in ["sdk", "claudeai-proxy"] {
            assert!(only(&json!({"mcpServers":{"srv":{"type":ty,"name":"sdk-client","id":"connector","url":"https://mcp.test","bareElicitationCapability":"true"}}})).is_empty(), "{ty}");
        }
    }

    /// Oracle `MAn`: `{type:"sdk",name,timeout,alwaysLoad}` carries NO `url`.
    /// An `sdk` entry with neither `url` NOR `command` must NOT be reported as
    /// an invalid config — unlike every other KNOWN_MCP_TYPES member, `sdk`
    /// has no transport field to require. (Before this fix `validation_issues`
    /// fell through to the `_` arm's url-required check for every non-stdio
    /// type, so a bare sdk entry was flagged "invalid ... url: expected
    /// string, received undefined" even though the loader accepts it.)
    #[test]
    fn sdk_entry_with_no_url_is_not_flagged_invalid() {
        let w =
            only(&json!({"mcpServers":{"claude-vscode":{"type":"sdk","name":"claude-vscode"}}}));
        assert!(w.is_empty(), "sdk entry without url must not warn: {w:?}");
    }

    /// Oracle `MAn` @154585319 declares `name: i()` with NO `.optional()` —
    /// the ONLY required field the sdk arm has. `xqe` runs
    /// `ZGn["sdk"]().safeParse(entry)`, which fails, and reports
    /// `Skipped — invalid MCP server config for "x": name: expected string,
    /// received undefined`. The port previously suppressed EVERY diagnostic
    /// for the type (`"sdk" => {}`), so a phantom sdk server loaded in total
    /// silence.
    #[test]
    fn sdk_entry_without_name_is_flagged_invalid() {
        let w = only(&json!({"mcpServers":{"x":{"type":"sdk"}}}));
        assert_eq!(w.len(), 1, "a nameless sdk entry must warn: {w:?}");
        assert_eq!(
            w[0].message,
            "Skipped \u{2014} invalid MCP server config for \"x\": name: expected string, received undefined"
        );
        // A non-string `name` is the same schema failure, different received.
        let w = only(&json!({"mcpServers":{"x":{"type":"sdk","name":7}}}));
        assert_eq!(
            w[0].message,
            "Skipped \u{2014} invalid MCP server config for \"x\": name: expected string, received number"
        );
        // `i()` carries no `.min(1)`, so an EMPTY name is schema-valid.
        assert!(only(&json!({"mcpServers":{"x":{"type":"sdk","name":""}}})).is_empty());
    }

    /// Oracle `NAn` @154585377 declares `url:i(),id:i()` both required, with
    /// NO `.min(1)` on either — the mirror of the sdk `name` case above, but
    /// for the ONE union member that needs two required checks. Before this
    /// fix `validation_issues` fell through to the `_` arm, which checks
    /// `url` only, so an idless claudeai-proxy entry loaded (per the sibling
    /// loader fix) in total diagnostic silence.
    #[test]
    fn claudeai_proxy_entry_without_id_is_flagged_invalid() {
        let w = only(&json!({"mcpServers":{"x":{"type":"claudeai-proxy","url":"https://x.test"}}}));
        assert_eq!(
            w.len(),
            1,
            "an idless claudeai-proxy entry must warn: {w:?}"
        );
        assert_eq!(
            w[0].message,
            "Skipped \u{2014} invalid MCP server config for \"x\": id: expected string, received undefined"
        );
        // A non-string `id` is the same schema failure, different received.
        let w = only(
            &json!({"mcpServers":{"x":{"type":"claudeai-proxy","url":"https://x.test","id":7}}}),
        );
        assert_eq!(
            w[0].message,
            "Skipped \u{2014} invalid MCP server config for \"x\": id: expected string, received number"
        );
        // `i()` carries no `.min(1)`, so an EMPTY id is schema-valid.
        assert!(only(
            &json!({"mcpServers":{"x":{"type":"claudeai-proxy","url":"https://x.test","id":""}}})
        )
        .is_empty());
        // Both required fields missing report both, in `url`-then-`id` order.
        let w = only(&json!({"mcpServers":{"x":{"type":"claudeai-proxy"}}}));
        assert_eq!(
            w[0].message,
            "Skipped \u{2014} invalid MCP server config for \"x\": \
             url: expected string, received undefined; \
             id: expected string, received undefined"
        );
    }

    /// `NAn` (@154585377) declares no `headers` key and `f` (@154568943) is a
    /// catchall-free `z.object`, so a malformed `headers` on a
    /// `claudeai-proxy` entry is STRIPPED — the oracle loads the server and
    /// says nothing. The shared post-match `headers` check ran for every
    /// non-`stdio` type, so the port warned about (and, in the loader,
    /// dropped) an entry claude-code keeps. `sse`/`http`/`ws`, whose
    /// `OAn`/`sGt`/`LAn` DO declare `headers: De(i(),i()).optional()`, are the
    /// positive control.
    #[test]
    fn claudeai_proxy_headers_are_stripped_not_validated() {
        let c = json!({"mcpServers":{"p":{
            "type":"claudeai-proxy","url":"https://x.test","id":"c1","headers":"nope"
        }}});
        assert!(
            only(&c).is_empty(),
            "`NAn` has no `headers`: {:?}",
            only(&c)
        );
        // Positive control: the same malformed value under `http` IS reported.
        let c = json!({"mcpServers":{"h":{
            "type":"http","url":"https://x.test","headers":"nope"
        }}});
        let w = only(&c);
        assert_eq!(
            w[0].message,
            "Skipped \u{2014} invalid MCP server config for \"h\": headers: expected record, received string"
        );
    }

    /// `Osg`/`fAn` (@160896200) has no `case "streamable-http"`, but it runs
    /// on the PARSED entry (`xqe` @160909118: `let fe=me.data; …
    /// ge=r?fAn(fe):void 0`) and `sGt` (@154584848) declares
    /// `type: ie(["http","streamable-http"]).transform(()=>"http")` — so a
    /// `streamable-http` entry arrives already retyped as `"http"`, expands,
    /// and `M(U,Pe)` (@160910700) reports its missing vars. `ty` here is the
    /// RAW string, so the alias was falling into the expands-nothing arm and
    /// the warning vanished.
    #[test]
    fn streamable_http_reports_missing_env_vars_like_http() {
        let expected = "Missing environment variables: LINGXI_DIAG_UNSET_SHTTP";
        for ty in ["streamable-http", "http"] {
            let c = json!({"mcpServers":{"m":{
                "type": ty, "url":"https://${LINGXI_DIAG_UNSET_SHTTP}.example/mcp"
            }}});
            let w = only(&c);
            assert_eq!(
                w.iter().map(|x| x.message.as_str()).collect::<Vec<_>>(),
                vec![expected],
                "type {ty:?} must report the same missing var as its `sGt` twin"
            );
        }
    }

    #[test]
    fn reserved_name_warning_is_byte_exact() {
        let c = json!({"mcpServers":{"workspace":{"type":"stdio","command":"c"}}});
        let w = only(&c);
        assert_eq!(
            w[0].message,
            "\"workspace\" is a reserved MCP server name and was not loaded"
        );
        assert_eq!(
            w[0].suggestion.as_deref(),
            Some("Rename this server in your MCP config \u{2014} \"workspace\" is reserved for internal use")
        );
        // An SDK server with a reserved name is NOT flagged (claude `m.type!=="sdk"`).
        // `name` is `MAn`'s one required field; the stray `url` is stripped.
        let c2 = json!({"mcpServers":{"workspace":{"type":"sdk","name":"chan","url":"chan"}}});
        assert!(only(&c2).is_empty());
    }

    #[test]
    fn missing_env_warning_is_byte_exact_and_deduped() {
        let c = json!({"mcpServers":{"srv":{"type":"stdio","command":"${TOK}","args":["${TOK}","${OTHER}"]}}});
        let w = only(&c);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].message, "Missing environment variables: TOK, OTHER");
        assert_eq!(
            w[0].suggestion.as_deref(),
            Some("Set the following environment variables: TOK, OTHER")
        );
    }

    #[test]
    fn whitespace_warning_is_byte_exact_stdio() {
        // `ty_` (2.1.219): field labels `command`, `args[<i>]`,
        // `env name <json>` for a whitespace-carrying KEY (JSON-quoted via
        // `Ie` = JSON.stringify), `env.<key>` (raw key) for its value.
        let c = json!({"mcpServers":{"srv":{
            "type":"stdio",
            "command":"cmd ",
            "args":[" a","b"],
            "env":{" K ":"v ","OK":"clean"}
        }}});
        let w = only(&c);
        assert_eq!(w.len(), 1);
        assert_eq!(
            w[0].message,
            "Leading or trailing whitespace in: command, args[0], env name \" K \", env. K "
        );
        assert_eq!(
            w[0].suggestion.as_deref(),
            Some(
                "Remove the whitespace from these values in the \"srv\" entry \u{2014} they are used exactly as written"
            )
        );
        assert_eq!(w[0].severity, McpConfigSeverity::Warning);
    }

    #[test]
    fn whitespace_warning_is_byte_exact_remote() {
        let c = json!({"mcpServers":{"web":{
            "type":"http",
            "url":"https://x.test ",
            "headers":{"X-A ":"v","X-B":" v"}
        }}});
        let w = only(&c);
        assert_eq!(w.len(), 1);
        assert_eq!(
            w[0].message,
            "Leading or trailing whitespace in: url, header name \"X-A \", headers.X-B"
        );
    }

    #[test]
    fn whitespace_and_missing_env_both_fire_for_one_entry() {
        // `klr` runs `ty_` BEFORE the expansion pass; both warnings can fire.
        let c = json!({"mcpServers":{"srv":{
            "type":"stdio",
            "command":"${LINGXI_DIAG_UNSET_M5} "
        }}});
        let w = only(&c);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].message, "Leading or trailing whitespace in: command");
        assert_eq!(
            w[1].message,
            "Missing environment variables: LINGXI_DIAG_UNSET_M5"
        );
    }

    #[test]
    fn skipped_entries_get_no_whitespace_warning() {
        // A reserved-name (or otherwise skipped) entry never reaches `ty_` —
        // `klr` `continue`s before the whitespace scan.
        let c = json!({"mcpServers":{"workspace":{"type":"stdio","command":"c "}}});
        let w = only(&c);
        assert_eq!(w.len(), 1);
        assert!(w[0].message.contains("reserved MCP server name"));
    }

    #[test]
    fn servers_typo_shape_error_is_byte_exact() {
        let c = json!({"servers":{"a":{"type":"stdio","command":"c"}}});
        let w = collect_mcp_config_warnings(
            &c,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
            Some("/u/config.json"),
        );
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].severity, McpConfigSeverity::Fatal);
        assert_eq!(
            w[0].message,
            "Missing \"mcpServers\" \u{2014} found \"servers\" instead. MCP servers must be configured in the \"mcpServers\" key."
        );
        assert_eq!(
            w[0].suggestion.as_deref(),
            Some("Rename the top-level \"servers\" key to \"mcpServers\" in /u/config.json")
        );
    }

    #[test]
    fn valid_entries_and_absent_mcpservers_produce_no_warnings() {
        assert!(
            only(&json!({"mcpServers":{"ok":{"type":"stdio","command":"c","args":["a"]}}}))
                .is_empty()
        );
        assert!(only(&json!({"mcpServers":{"web":{"type":"http","url":"https://x"}}})).is_empty());
        // No mcpServers and no "servers" typo → nothing to diagnose.
        assert!(only(&json!({"other":1})).is_empty());
    }

    #[test]
    fn invalid_entry_reports_every_issue_with_paths() {
        let warnings = only(&json!({"mcpServers":{"bad":{
            "type":"stdio",
            "args":[1, "ok", false],
            "env":{"TOKEN":7},
            "timeout":0,
            "alwaysLoad":"yes"
        }}}));
        assert_eq!(warnings.len(), 1);
        let message = &warnings[0].message;
        for issue in [
            "command: expected string, received undefined",
            "args.0: expected string, received number",
            "args.2: expected string, received boolean",
            "env.TOKEN: expected string, received number",
            "timeout: expected positive integer",
            "alwaysLoad: expected boolean",
        ] {
            assert!(message.contains(issue), "missing diagnostic issue: {issue}");
        }
    }

    #[test]
    fn stderr_line_appends_suggestion() {
        let c = json!({"mcpServers":{"srv":{"type":"grpc","url":"x"}}});
        let line = only(&c)[0].to_stderr_line();
        assert_eq!(
            line,
            "Skipped \u{2014} unknown MCP server type \"grpc\" for server \"srv\" (Valid types are: stdio, sse, http (or streamable-http), ws, sdk)"
        );
    }

    // ── §23b: `read_mcp_config_file` / `parse_mcp_config_json` (`Iqe` port) ──

    use tempfile::TempDir;

    #[test]
    fn shape_gate_rejects_oversized_regular_file() {
        let (_cap, _guard) = install_gate_capture();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("huge.mcp.json");
        // One byte over the oracle's `mcn = 2097152` cap.
        let big = vec![b' '; (MCP_CONFIG_MAX_BYTES + 1) as usize];
        std::fs::write(&path, &big).unwrap();

        let err = read_mcp_config_file(
            &path,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap_err();
        assert!(!err.is_not_found());
        assert_eq!(err.severity, McpConfigSeverity::Fatal);
        assert_eq!(
            err.scope,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project)
        );
        assert_eq!(
            err.message,
            format!(
                "MCP config is not a regular file or exceeds {MCP_CONFIG_MAX_BYTES} bytes: {}",
                path.display()
            )
        );
        assert_eq!(
            err.suggestion.as_deref(),
            Some(
                "Check that the path is a plain JSON file (not a device, FIFO, or symlink to one)"
            )
        );
    }

    #[test]
    fn shape_gate_accepts_regular_file_at_exactly_the_cap() {
        let (_cap, _guard) = install_gate_capture();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exact.mcp.json");
        let mut body = vec![b' '; MCP_CONFIG_MAX_BYTES as usize - 2];
        body.extend_from_slice(b"{}");
        std::fs::write(&path, &body).unwrap();
        let raw = read_mcp_config_file(
            &path,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(raw.len() as u64, MCP_CONFIG_MAX_BYTES);
    }

    #[test]
    fn shape_gate_rejects_non_regular_file() {
        let (_cap, _guard) = install_gate_capture();
        // A directory is not a regular file — same "shape" branch the oracle's
        // suggestion text describes for devices/FIFOs/symlinks-to-those.
        let dir = TempDir::new().unwrap();
        let err = read_mcp_config_file(
            dir.path(),
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap_err();
        assert!(err
            .message
            .starts_with("MCP config is not a regular file or exceeds"));
        assert!(!err.is_not_found());
    }

    #[cfg(unix)]
    #[test]
    fn shape_gate_rejects_fifo() {
        let (_cap, _guard) = install_gate_capture();
        let dir = TempDir::new().unwrap();
        let fifo = dir.path().join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo");
        assert!(status.success());
        let err = read_mcp_config_file(
            &fifo,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap_err();
        assert!(err
            .message
            .starts_with("MCP config is not a regular file or exceeds"));
        assert_eq!(
            err.suggestion.as_deref(),
            Some(
                "Check that the path is a plain JSON file (not a device, FIFO, or symlink to one)"
            )
        );
    }

    #[test]
    fn missing_file_is_the_not_found_variant() {
        let (_cap, _guard) = install_gate_capture();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("absent.mcp.json");
        let err = read_mcp_config_file(
            &path,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap_err();
        assert!(err.is_not_found());
        assert_eq!(
            err.message,
            format!("MCP config file not found: {}", path.display())
        );
        assert_eq!(
            err.suggestion.as_deref(),
            Some("Check that the file path is correct")
        );
    }

    #[test]
    fn other_read_error_is_distinct_from_not_found() {
        let (_cap, _guard) = install_gate_capture();
        // ENAMETOOLONG (not ENOENT): a path component past NAME_MAX. Confirmed
        // at the oracle: `E(A)==="ENOENT"` is the ONLY branch that yields the
        // "file not found" shape — everything else falls to the generic
        // "Failed to read file: …" message.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a".repeat(300));
        let err = read_mcp_config_file(
            &path,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap_err();
        assert!(!err.is_not_found());
        assert!(
            err.message.starts_with("Failed to read file: "),
            "got: {}",
            err.message
        );
        assert_eq!(
            err.suggestion.as_deref(),
            Some("Check file permissions and ensure the file exists")
        );
    }

    #[test]
    fn dynamic_scope_bypasses_the_shape_gate() {
        let (_cap, _guard) = install_gate_capture();
        // Oracle: `o==="dynamic" ? readFileSync(...) : Atr(...,mcn)` — the
        // size/regular-file check is skipped entirely for Dynamic scope.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("huge.json");
        let big = vec![b' '; (MCP_CONFIG_MAX_BYTES + 1) as usize];
        std::fs::write(&path, &big).unwrap();
        let raw = read_mcp_config_file(&path, ConfigScope::Dynamic).unwrap();
        assert_eq!(raw.len() as u64, MCP_CONFIG_MAX_BYTES + 1);
    }

    #[test]
    fn well_formed_small_file_reads_through() {
        let (_cap, _guard) = install_gate_capture();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".mcp.json");
        std::fs::write(&path, r#"{"mcpServers":{}}"#).unwrap();
        let raw = read_mcp_config_file(
            &path,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(raw, r#"{"mcpServers":{}}"#);
    }

    #[test]
    fn invalid_json_returns_the_short_fixed_message() {
        let (_cap, _guard) = install_gate_capture();
        // Oracle returns a SHORT literal here — NOT the detailed
        // path/scope/length/first100 string, which is log-only.
        let path = Path::new("/p/.mcp.json");
        let err = parse_mcp_config_json(
            "{ not json",
            path,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap_err();
        assert_eq!(err.message, "MCP config is not a valid JSON");
        assert_eq!(
            err.suggestion.as_deref(),
            Some("Fix the JSON syntax errors in the file")
        );
        assert_eq!(err.file.as_deref(), Some("/p/.mcp.json"));
        assert_eq!(err.severity, McpConfigSeverity::Fatal);
    }

    #[test]
    fn valid_json_parses_through() {
        let (_cap, _guard) = install_gate_capture();
        let path = Path::new("/p/.mcp.json");
        let v = parse_mcp_config_json(
            r#"{"mcpServers":{}}"#,
            path,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(v, json!({"mcpServers":{}}));
    }

    // ── §23b telemetry: each fatal branch fires `mcp_config_parse` with its
    //    OWN reason; the success branch fires it with none ──────────────────

    use crate::tracing_capture::GateCapture;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::reload;
    use tracing_subscriber::Registry;

    fn install_gate_capture() -> (GateCapture, tracing::subscriber::DefaultGuard) {
        let cap = GateCapture::default();
        let (layer, handle) = reload::Layer::new(cap.clone());
        let guard = tracing::subscriber::set_default(Registry::default().with(layer));
        handle.modify(|_| {}).unwrap();
        (cap, guard)
    }

    #[test]
    fn shape_gate_rejection_reports_its_own_reason() {
        let (cap, _guard) = install_gate_capture();

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("huge.mcp.json");
        std::fs::write(&path, vec![b' '; (MCP_CONFIG_MAX_BYTES + 1) as usize]).unwrap();
        let _ = read_mcp_config_file(
            &path,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        );

        assert_eq!(
            cap.rows(),
            vec![(
                telemetry::MCP_CONFIG_PARSE_GATE.to_string(),
                Some(telemetry::MCP_CONFIG_SHAPE_GATE.to_string())
            )]
        );
    }

    #[test]
    fn read_failure_reports_its_own_reason() {
        let (cap, _guard) = install_gate_capture();

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a".repeat(300));
        let _ = read_mcp_config_file(
            &path,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        );

        assert_eq!(
            cap.rows(),
            vec![(
                telemetry::MCP_CONFIG_PARSE_GATE.to_string(),
                Some(telemetry::MCP_CONFIG_READ_FAILED.to_string())
            )]
        );
    }

    #[test]
    fn missing_file_fires_no_telemetry_at_all() {
        // Oracle: `E(A)==="ENOENT"` returns immediately with no `n(...)` log
        // and no `p(...)` call — a missing ancestor config is routine.
        let (cap, _guard) = install_gate_capture();

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("absent.mcp.json");
        let _ = read_mcp_config_file(
            &path,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        );

        assert_eq!(cap.rows(), Vec::<(String, Option<String>)>::new());
    }

    #[test]
    fn invalid_json_reports_its_own_reason() {
        let (cap, _guard) = install_gate_capture();

        let path = Path::new("/p/.mcp.json");
        let _ = parse_mcp_config_json(
            "{ not json",
            path,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        );

        assert_eq!(
            cap.rows(),
            vec![(
                telemetry::MCP_CONFIG_PARSE_GATE.to_string(),
                Some(telemetry::MCP_CONFIG_INVALID_JSON.to_string())
            )]
        );
    }

    #[test]
    fn successful_project_config_parse_reports_no_reason() {
        let (cap, _guard) = install_gate_capture();

        let dir = TempDir::new().unwrap();
        let project = dir.path().join(".mcp.json");
        std::fs::write(&project, r#"{"mcpServers":{}}"#).unwrap();
        let _ = collect_all_mcp_config_warnings_at(&project, dir.path(), None);

        assert_eq!(
            cap.rows(),
            vec![(telemetry::MCP_CONFIG_PARSE_GATE.to_string(), None)]
        );
    }

    // ── `collect_all_mcp_config_warnings_at` wiring ────────────────────────

    #[test]
    fn missing_project_mcp_json_yields_no_warnings() {
        let (_cap, _guard) = install_gate_capture();
        let dir = TempDir::new().unwrap();
        let warnings =
            collect_all_mcp_config_warnings_at(&dir.path().join(".mcp.json"), dir.path(), None);
        assert!(warnings.is_empty());
    }

    #[test]
    fn oversized_project_mcp_json_surfaces_the_shape_gate_warning() {
        let (_cap, _guard) = install_gate_capture();
        let dir = TempDir::new().unwrap();
        let project = dir.path().join(".mcp.json");
        let big = vec![b' '; (MCP_CONFIG_MAX_BYTES + 1) as usize];
        std::fs::write(&project, &big).unwrap();
        let warnings = collect_all_mcp_config_warnings_at(&project, dir.path(), None);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0]
                .message
                .starts_with("MCP config is not a regular file or exceeds"),
            "got: {}",
            warnings[0].message
        );
        assert_eq!(
            warnings[0].scope,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project)
        );
    }

    #[test]
    fn malformed_project_mcp_json_surfaces_the_invalid_json_warning() {
        let (_cap, _guard) = install_gate_capture();
        let dir = TempDir::new().unwrap();
        let project = dir.path().join(".mcp.json");
        std::fs::write(&project, "{ not json").unwrap();
        let warnings = collect_all_mcp_config_warnings_at(&project, dir.path(), None);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].message, "MCP config is not a valid JSON");
    }
}
