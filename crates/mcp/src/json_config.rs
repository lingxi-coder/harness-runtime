//! `.mcp.json` parser (M6-07).
//!
//! Reads the claude-code-compatible shape:
//! ```json
//! {
//!   "mcpServers": {
//!     "memory":     { "command": "mcp-memory",     "args": [], "env": {} },
//!     "filesystem": { "command": "mcp-filesystem", "args": ["/tmp"], "env": {} }
//!   }
//! }
//! ```
//! Each entry is projected into a [`crate::McpServerConfig`] with
//! [`crate::ConfigScope`] supplied by the caller. URL-based ("http"/"sse")
//! entries are accepted via the `url` field.

use crate::connection::{ConfigScope, McpServerConfig, McpServerMetadata, McpServerRole};
use crate::env_expansion::expand_env_vars_in_string;
use lingxi_core::host::{
    McpConfiguredToolPolicyDto, McpHeaders, McpOAuthConfigDto, McpPermissionCeiling,
    McpToolPermissionPolicy, McpTransportSpec,
};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// Expand `${VAR}` / `${VAR:-default}` references in one string against the
/// process environment, appending any missing-variable names to `missing`.
/// 1:1 with the inner `expandString` of claude-code `expandEnvVars`
/// (`services/mcp/config.ts:562-566`).
fn expand_field(value: &str, missing: &mut Vec<String>) -> String {
    let r = expand_env_vars_in_string(value);
    missing.extend(r.missing_vars);
    r.expanded
}

/// Expand the VALUES of a `HashMap` (keys untouched), mirroring the TS
/// `mapValues(map, expandString)` used for `env` and `headers`
/// (`services/mcp/config.ts:579,595`).
fn expand_map_values(
    map: HashMap<String, String>,
    missing: &mut Vec<String>,
) -> HashMap<String, String> {
    map.into_iter()
        .map(|(k, v)| {
            let v = expand_field(&v, missing);
            (k, v)
        })
        .collect()
}

/// Order-preserving variant of [`expand_map_values`] for `headers`. The header
/// key order must survive parse → spec so the `getServerKey` config hash
/// byte-matches claude-code (see [`lingxi_core::host::McpHeaders`]).
fn expand_header_values(map: McpHeaders, missing: &mut Vec<String>) -> McpHeaders {
    map.into_iter()
        .map(|(k, v)| {
            let v = expand_field(&v, missing);
            (k, v)
        })
        .collect()
}

/// Errors raised while parsing a `.mcp.json` file.
#[derive(Debug, thiserror::Error)]
pub enum McpJsonError {
    /// Invalid JSON.
    #[error("invalid .mcp.json: {0}")]
    Json(#[from] serde_json::Error),
    /// An entry had neither `command` nor `url` set.
    #[error("server '{0}' missing both command and url")]
    UnknownTransport(String),
}

#[derive(Debug, Deserialize)]
struct McpJsonEntry {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    url: Option<String>,
    /// `sdk`-only: the host control-channel identifier. Oracle `MAn`
    /// (2.1.251 Mach-O @154585319) is
    /// `f({type:N("sdk"),name:i(),timeout:o().optional(),alwaysLoad:q().optional()})`
    /// — `name` is a REQUIRED `i()`, in pointed contrast to its `.optional()`
    /// siblings, so `{"type":"sdk"}` fails `safeParse` and the entry is
    /// skipped. It is a plain `i()` with no `.min(1)`, so an EMPTY string is
    /// schema-valid. No other member of the union declares `name`, so zod
    /// strips it everywhere else (mirrored here: only the sdk branch reads
    /// it). The declared value is what the oracle carries on the config and
    /// compares in `tUt` (@160913341:
    /// `Object.values(e).every(t=>t.type==="sdk"&&t.name==="claude-vscode")`)
    /// — it is NOT forced to equal the entry's map key.
    #[serde(default)]
    name: Option<String>,
    /// "http" | "sse" — only honoured when `url` is set.
    #[serde(default, rename = "type")]
    transport_type: Option<String>,
    #[serde(default)]
    headers: McpHeaders,
    #[serde(default, rename = "headersHelper", alias = "headers_helper")]
    headers_helper: Option<String>,
    #[serde(default)]
    oauth: Option<McpOAuthConfigDto>,
    /// LingXi-original extension: a per-entry on/off switch checked directly
    /// on the raw JSON server object. Confirmed ABSENT from every member of
    /// the oracle's on-disk union (2.1.251 Mach-O @154583724 — none of
    /// `fYe`/`OAn`/`l`/`d`/`sGt`/`LAn`/`MAn`/`NAn` declare a `disabled` key),
    /// so a real claude-code `.mcp.json` sharing this field would have it
    /// silently stripped by zod (schemas here are not `.strict()`) and the
    /// server would load enabled regardless. Kept anyway: real Claude Code's
    /// only per-server disable knobs live OUTSIDE `.mcp.json` — the
    /// `disabledMcpServers` / `disabledMcpjsonServers` project-settings lists
    /// (see [`crate::server_gate`]) — so this field adds a strictly
    /// *additional* way to suppress a server from within its own config
    /// entry; it never makes the port accept something the oracle would
    /// reject. Do not remove without a replacement — this is load-bearing
    /// for [`crate::registry`]'s disabled-server handling.
    #[serde(default)]
    disabled: bool,
    /// Per-server `tools/call` timeout in ms. claude-code zod schema
    /// (all transports): `timeout: RKe().optional()` where `RKe =
    /// E.number().int().positive()`. A present-but-invalid value (non-integer,
    /// non-positive, wrong type) fails the entry's `safeParse` → the entry is
    /// skipped (mirrored here: a non-`u64` value fails `serde` decode →
    /// [`build_servers_from_map`] logs + skips the entry).
    #[serde(default, deserialize_with = "de_positive_timeout")]
    timeout: Option<u64>,
    /// sse/http-only alias for `timeout`. claude-code schema:
    /// `request_timeout_ms: nil()` where `nil =
    /// E.number().int().positive().optional().catch(void 0)` — the `.catch`
    /// means an invalid value is coerced to `undefined` (NOT an entry failure).
    /// Held as opaque JSON so a bad value never fails the whole entry; coerced
    /// to a positive integer by [`as_positive_int_ms`] and folded into
    /// `timeout` via the `RAn` transform below (sse/http only).
    #[serde(default, rename = "request_timeout_ms")]
    request_timeout_ms: Option<serde_json::Value>,
    /// `alwaysLoad`: force all of this server's tools into the prompt (never
    /// deferred behind tool search). claude-code schema (all transports):
    /// `alwaysLoad: E.boolean().optional()`.
    #[serde(default, rename = "alwaysLoad")]
    always_load: Option<bool>,
    /// Optional per-tool rule/ceiling records from dynamic remote MCP config.
    #[serde(default, deserialize_with = "de_tool_policy_entries")]
    tools: Vec<McpConfiguredToolPolicyDto>,
    /// Optional tighten-only per-tool ceilings keyed by upstream tool name.
    #[serde(
        default,
        rename = "toolPermissions",
        deserialize_with = "de_tool_permissions"
    )]
    tool_permissions: BTreeMap<String, McpPermissionCeiling>,
    /// `claudeai-proxy`-only: the claude.ai-issued connector identifier.
    /// Oracle `NAn` (2.1.251 Mach-O @154585377) is
    /// `f({type:N("claudeai-proxy"),url:i(),id:i(),displayName:i().optional(),
    /// iconUrl:i().optional(),timeout:o().optional(),alwaysLoad:q().optional(),
    /// toolPermissions:...,stateless:...,cachedInitResponse:...,...})` — `id`
    /// is a REQUIRED `i()` (no `.optional()`, in pointed contrast to
    /// `displayName`/`iconUrl` and the rest of the schema's tail), so a
    /// `claudeai-proxy` entry with no `id` fails `safeParse` and the whole
    /// entry is skipped — the same shape as `sdk`'s required `name`. `i()`
    /// has no `.min(1)`, so an EMPTY string is schema-valid; only presence is
    /// checked. No other union member declares `id`, so zod strips it
    /// everywhere else (mirrored here: only the claudeai-proxy branch reads
    /// it). The claude.ai connector surface is out of scope, so the value
    /// itself is validated for presence and otherwise unused.
    ///
    /// Held as OPAQUE JSON, not `Option<String>`: a serde-declared field is
    /// type-checked for EVERY `type`, whereas zod strips `id` off every union
    /// member but `NAn`. `f` (@154568943) is `new BEt({type:"object",shape,
    /// ...})` with NO `catchall` — contrast its siblings `ot` (`catchall:
    /// UEt()`, strict) and `un` (`catchall:_e()`, loose) — so
    /// `{"command":"c","id":7}` parses fine against `fYe` and the stdio
    /// server LOADS. Typing this `Option<String>` made that entry fail
    /// `McpJsonEntry::deserialize`, so [`build_entry`] skipped it (and
    /// `config_diagnostics`, whose `stdio` arm never looks at `id`, stayed
    /// silent — a loader/diagnostics disagreement), and
    /// [`server_entry_shape_is_valid`] returned `false`, dropping the WHOLE
    /// JSON agent. Only the `claudeai-proxy` arm reads it, and it requires a
    /// `Value::String` there — mirroring `NAn`'s required `i()`.
    #[serde(default)]
    id: Option<serde_json::Value>,
    /// `sse-ide`/`ws-ide`-only: the human-readable IDE name. Oracle `l`/`d`
    /// (2.1.251 Mach-O @154584571/@154584715) both declare `ideName:i()`
    /// REQUIRED (no `.optional()`, in the same pointed contrast as `sdk`'s
    /// `name` / `claudeai-proxy`'s `id`) — a missing `ideName` fails
    /// `safeParse` and the whole entry is skipped. `i()` has no `.min(1)`, so
    /// an empty string is schema-valid; only presence is checked.
    ///
    /// Held as OPAQUE JSON for exactly the reason [`Self::id`] is: a
    /// serde-declared `Option<String>` is type-checked for EVERY `type`,
    /// whereas `l`/`d` are the ONLY union members that declare this key and
    /// `f` (@154568943) is catchall-free, so zod strips it — unlooked-at —
    /// off `stdio`/`sse`/`http`/`ws`/`sdk`/`claudeai-proxy`. The string check
    /// lives in the two IDE arms of [`build_entry`] /
    /// [`entry_satisfies_schema`], where the schema actually declares it.
    #[serde(default, rename = "ideName")]
    ide_name: Option<serde_json::Value>,
    /// `ws-ide`-only: an optional bearer token presented at the WebSocket
    /// handshake. Oracle `d` @154584715: `authToken:i().optional()`. Opaque
    /// for the same per-variant-strip reason as [`Self::ide_name`]; `d` is
    /// the only member that declares it, so a wrong-typed `authToken` may
    /// reject a `ws-ide` entry and NOTHING else.
    #[serde(default, rename = "authToken")]
    auth_token: Option<serde_json::Value>,
    /// `sse-ide`/`ws-ide`-only: true when the IDE is hosted on Windows
    /// (affects path normalization). Oracle: `ideRunningInWindows:q().optional()`.
    /// Opaque for the same per-variant-strip reason as [`Self::ide_name`].
    #[serde(default, rename = "ideRunningInWindows")]
    ide_running_in_windows: Option<serde_json::Value>,
}

/// The `l`/`d` (sse-ide/ws-ide) schema checks for the three IDE-only keys,
/// run where the schema actually declares them. Returns `None` when the entry
/// fails `safeParse` and must be skipped:
///
/// * `ideName: i()` — REQUIRED, must be a string;
/// * `authToken: i().optional()` — when present, must be a string
///   (`ws-ide` only: `l` does not declare it, so `sse-ide` passes `false`
///   for `declares_auth_token` and the key is treated as stripped);
/// * `ideRunningInWindows: q().optional()` — when present, must be a boolean.
fn ide_schema_fields(
    ide_name: Option<&serde_json::Value>,
    auth_token: Option<&serde_json::Value>,
    ide_running_in_windows: Option<&serde_json::Value>,
    declares_auth_token: bool,
) -> Option<(String, Option<String>, bool)> {
    let ide_name = match ide_name {
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => return None,
    };
    let auth_token = if declares_auth_token {
        match auth_token {
            None => None,
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(_) => return None,
        }
    } else {
        None
    };
    let ide_running_in_windows = match ide_running_in_windows {
        None => false,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(_) => return None,
    };
    Some((ide_name, auth_token, ide_running_in_windows))
}

/// The keys oracle `NAn` (@154585377) declares, plus the LingXi-original
/// `disabled` extension (see [`McpJsonEntry::disabled`]).
///
/// `NAn` is built with `f` (@154568943), a plain `z.object` with NO
/// `catchall` — its siblings `ot`/`un` are the strict/loose factories, and
/// `NAn` uses neither — so every key NOT listed here is STRIPPED off a
/// `claudeai-proxy` entry before `me.data` is used: never type-checked, never
/// carried onto the config. Serde cannot strip per-variant (a declared field
/// is type-checked for every `type`), so [`strip_to_claudeai_proxy_schema`]
/// mirrors the strip on the raw value instead.
const CLAUDEAI_PROXY_SCHEMA_KEYS: &[&str] = &[
    "type",
    "url",
    "id",
    "displayName",
    "iconUrl",
    "timeout",
    "alwaysLoad",
    "toolPermissions",
    "stateless",
    "cachedInitResponse",
    "discoverSupport",
    "cachedDiscoverResponse",
    "eligible",
    "ineligibleReason",
    "enterpriseManaged",
    // LingXi-original extension, read off the raw entry like every other
    // transport's — NOT part of `NAn`, but load-bearing for `crate::registry`.
    "disabled",
];

/// Mirror `f`'s unknown-key strip for `claudeai-proxy` — the ONE union member
/// whose schema (`NAn`) diverges wholesale from the shared [`McpJsonEntry`]
/// shape: it declares no `headers`, no `headersHelper`, no `oauth`, no
/// `command`/`args`/`env`/`name`, and no `request_timeout_ms`.
///
/// Without this the port both (a) DROPS entries the oracle keeps — e.g.
/// `{"type":"claudeai-proxy","url":"…","id":"c","headers":"nope"}` fails the
/// `McpHeaders` decode, so the whole entry is skipped where zod strips
/// `headers` and loads the server — and (b) FORWARDS `headers` /
/// `headersHelper` / `oauth` onto the dialled `McpTransportSpec::Http`, which
/// for `headersHelper` means [`crate::registry`] EXECUTES a shell command the
/// oracle never runs (`crate::headers_helper::resolve_headers_helper_in`).
///
/// Every other `type` is returned borrowed and untouched: the same strip is
/// owed to `stdio`/`sdk`/`sse-ide`/`ws-ide` for `headers`/`name`/… but that
/// is a separate, pre-existing family (see the batch report), and widening it
/// here would break the loader↔diagnostics agreement in the other direction.
fn strip_to_claudeai_proxy_schema(
    raw: &serde_json::Value,
) -> std::borrow::Cow<'_, serde_json::Value> {
    use std::borrow::Cow;
    let Some(object) = raw.as_object() else {
        return Cow::Borrowed(raw);
    };
    if object.get("type").and_then(serde_json::Value::as_str) != Some("claudeai-proxy") {
        return Cow::Borrowed(raw);
    }
    if object
        .keys()
        .all(|k| CLAUDEAI_PROXY_SCHEMA_KEYS.contains(&k.as_str()))
    {
        return Cow::Borrowed(raw);
    }
    Cow::Owned(serde_json::Value::Object(
        object
            .iter()
            .filter(|(k, _)| CLAUDEAI_PROXY_SCHEMA_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    ))
}

/// Coerce a JSON value to a positive-integer millisecond count, mirroring the
/// claude-code `request_timeout_ms` zod schema `E.number().int().positive()
/// .optional().catch(void 0)`: a positive integer is kept; anything else
/// (missing, non-number, non-integer, `<= 0`, or overflowing `u64`) becomes
/// `None` (the `.catch(void 0)` leniency).
fn as_positive_int_ms(v: Option<&serde_json::Value>) -> Option<u64> {
    // `as_u64` already rejects negatives, fractional numbers, and non-numbers.
    let n = v?.as_u64()?;
    (n > 0).then_some(n)
}

/// Deserialize the per-server `timeout` field, mirroring the claude-code zod
/// schema `timeout: E.number().int().positive().optional()` (NO `.catch`): an
/// absent field is `None`; a present positive integer is `Some(n)`; a present
/// non-positive `0` — a valid `u64` that `.positive()` rejects — fails the
/// deserialize so [`build_servers_from_map`] logs + SKIPS the whole entry
/// (matching CC's `safeParse` failure). Negatives, fractional, and non-numeric
/// values are already rejected by the `u64` decode itself.
fn de_positive_timeout<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<u64>::deserialize(deserializer)? {
        Some(0) => Err(serde::de::Error::custom(
            "timeout must be a positive integer",
        )),
        other => Ok(other),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct RawToolPolicyEntry {
    name: String,
    #[serde(default, alias = "permissionPolicy")]
    permission_policy: Option<String>,
    #[serde(default, alias = "orgMaxPermission")]
    org_max_permission: Option<String>,
}

fn de_tool_policy_entries<'de, D>(
    deserializer: D,
) -> Result<Vec<McpConfiguredToolPolicyDto>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Vec::<RawToolPolicyEntry>::deserialize(deserializer)?;
    raw.into_iter()
        .map(|entry| {
            let permission_policy =
                entry.permission_policy.as_deref().map_or(Ok(None), |value| {
                    McpToolPermissionPolicy::from_policy_str(value)
                        .map(Some)
                        .ok_or_else(|| {
                            serde::de::Error::custom(format!(
                                "tools[].permission_policy must be allow/ask/deny or always_allow/always_ask/always_deny, got {value:?}"
                            ))
                        })
                })?;
            let org_max_permission =
                entry.org_max_permission.as_deref().map_or(Ok(None), |value| {
                    McpPermissionCeiling::from_policy_str(value)
                        .map(Some)
                        .ok_or_else(|| {
                            serde::de::Error::custom(format!(
                                "tools[].org_max_permission must be allow/ask/blocked, got {value:?}"
                            ))
                        })
                })?;
            Ok(McpConfiguredToolPolicyDto {
                name: entry.name,
                permission_policy,
                org_max_permission,
            })
        })
        .collect()
}

fn de_tool_permissions<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, McpPermissionCeiling>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = BTreeMap::<String, String>::deserialize(deserializer)?;
    raw.into_iter()
        .map(|(tool, value)| {
            let Some(ceiling) = McpPermissionCeiling::from_policy_str(&value) else {
                return Err(serde::de::Error::custom(format!(
                    "toolPermissions[{tool:?}] must be allow/ask/blocked, got {value:?}"
                )));
            };
            Ok((tool, ceiling))
        })
        .collect()
}

/// Validate the `oauth` child, mirroring the oracle's `a()` zod schema
/// (2.1.251 Mach-O @154583724):
/// ```text
/// a = z.object({
///   clientId: z.string().optional(),
///   callbackPort: z.number().int().positive().optional(),
///   authServerMetadataUrl: z.string().url()
///     .startsWith("https://", {message:"authServerMetadataUrl must use https://"})
///     .optional(),
///   scopes: z.string().min(1).optional(),
///   xaa: z.boolean().optional(),
/// })
/// ```
/// Unlike `role`/`request_timeout_ms` (which use `.catch(void 0)` to
/// silently discard a bad value), `oauth: a().optional()` has NO `.catch`:
/// the child schema only short-circuits when the KEY is absent, so a
/// PRESENT-but-invalid `oauth` object fails the containing entry's whole
/// `safeParse`, exactly like a malformed `command`/`url`/`headers`. Returns
/// `None` when the entry must be rejected outright; `Some(oauth)` otherwise
/// (including `Some(None)` when no `oauth` block was given at all).
///
/// `callbackPort` is only checked against `Some(0)` here — a value the oracle
/// schema also rejects. Ports above `u16::MAX` are schema-valid at the oracle
/// (`z.number().int().positive()` has no upper bound) but already fail to
/// deserialize into this port's `McpOAuthConfigDto::callback_port: u16`
/// (`platform-api/src/mcp.rs`, owned by another batch) before this function ever
/// runs — a stricter, not more permissive, divergence, and out of scope for
/// this fix (changing the DTO's field type is a wire-shape change).
fn validate_oauth_child(oauth: Option<McpOAuthConfigDto>) -> Option<Option<McpOAuthConfigDto>> {
    let Some(cfg) = oauth else {
        return Some(None);
    };
    if cfg.callback_port == Some(0) {
        return None;
    }
    if let Some(url) = &cfg.auth_server_metadata_url {
        if !url.starts_with("https://") || url::Url::parse(url).is_err() {
            return None;
        }
    }
    if let Some(scopes) = &cfg.scopes {
        if scopes.is_empty() {
            return None;
        }
    }
    Some(Some(cfg))
}

// ── §11 — `discoveryCache` / `role` recognition ─────────────────────────────
//
// `discoveryCache` is recognized here and threaded onto `McpServerConfig`
// as a plain `Option<bool>` consumed by discovery-cache gating. `role`
// remains validation-only and is still not threaded further.

/// §11 — is `discoveryCache` a declared schema key for `type`? Oracle `OAn`/
/// `sGt` (2.1.251 Mach-O @154584436/@154585104 — `sse` and `http`/
/// `streamable-http`) declare `discoveryCache:q().optional()`; no other
/// union member does. zod's non-`catchall` object schemas silently drop an
/// UNKNOWN key without ever looking at its value, so on any other `type` the
/// key (whatever its shape) can never affect validation.
#[must_use]
pub fn discovery_cache_is_schema_key_for(transport_type: Option<&str>) -> bool {
    matches!(transport_type, Some("sse" | "http" | "streamable-http"))
}

/// [`discovery_cache_flag`]'s failure: `discoveryCache` was present, on a
/// type that declares it, with a non-boolean value — the whole entry must be
/// rejected. Carries no data (there is nothing to add beyond "reject this
/// entry"); a named unit-like type reads better at a call site than `Err(())`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidDiscoveryCacheFlag;

/// §11 — decode `discoveryCache` against `type`'s schema: `q().optional()`,
/// a plain boolean with NO `.catch`. On a type that declares the key
/// ([`discovery_cache_is_schema_key_for`]), a PRESENT-but-non-boolean value
/// fails that schema and the whole entry's `safeParse` — mirroring
/// [`validate_oauth_child`]'s all-or-nothing child-object handling — so the
/// caller must reject the entry outright ([`InvalidDiscoveryCacheFlag`]). On
/// a type that doesn't declare the key, the raw value (of any shape) is
/// schema-unknown and therefore inert: always `Ok(None)`.
///
/// # Errors
/// [`InvalidDiscoveryCacheFlag`] when `type` declares the key and its raw
/// value is present but not a JSON boolean.
pub fn discovery_cache_flag(
    transport_type: Option<&str>,
    raw_entry: &serde_json::Value,
) -> Result<Option<bool>, InvalidDiscoveryCacheFlag> {
    if !discovery_cache_is_schema_key_for(transport_type) {
        return Ok(None);
    }
    match raw_entry.get("discoveryCache") {
        None => Ok(None),
        Some(serde_json::Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(InvalidDiscoveryCacheFlag),
    }
}

/// Native 2.1.287 declares this boolean only on ordinary external transports.
pub(crate) fn bare_elicitation_is_schema_key_for(transport_type: Option<&str>) -> bool {
    matches!(
        transport_type,
        None | Some("stdio" | "sse" | "http" | "streamable-http" | "ws")
    )
}

fn bare_elicitation_flag(
    transport_type: Option<&str>,
    raw_entry: &serde_json::Value,
) -> Result<Option<bool>, &'static str> {
    if !bare_elicitation_is_schema_key_for(transport_type) {
        return Ok(None);
    }
    match raw_entry.get("bareElicitationCapability") {
        None => Ok(None),
        Some(serde_json::Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err("bareElicitationCapability must be a boolean"),
    }
}

/// §11 — is `role` a declared schema key for `type`? Oracle: present on
/// `stdio`/`sse`/`sse-ide`/`ws-ide`/`http` (+ `streamable-http`)/`ws`;
/// ABSENT from `sdk` (`MAn` @154585319) and `claudeai-proxy` (`NAn`
/// @154585377) — the same pointed omission pattern already established for
/// `request_timeout_ms`/`discoveryCache` on those two arms.
///
/// Enumerated POSITIVELY, like [`discovery_cache_is_schema_key_for`]: a
/// `type` string outside the 8-arm union `KY` declares no schema at all, so
/// it declares no `role` either. Phrased as `!matches!(…, "sdk" |
/// "claudeai-proxy")` this returned `true` for `Some("bogus")` /
/// `Some("websocket")` — types the union has no arm for.
#[must_use]
pub fn role_is_schema_key_for(transport_type: Option<&str>) -> bool {
    matches!(
        transport_type,
        // An absent `type` is `fYe` (stdio), which declares `role`.
        None | Some("stdio" | "sse" | "http" | "streamable-http" | "ws" | "sse-ide" | "ws-ide")
    )
}

/// §11 — `role: N("comms").optional().catch(void 0)`: the ONLY schema-valid
/// value is the literal string `"comms"`. Unlike [`discovery_cache_flag`]
/// this schema HAS `.catch`, so a present-but-invalid value (wrong JSON
/// type, or any string other than `"comms"`) is silently normalized to
/// `None` — it can NEVER reject the entry. Returns `None` outright when
/// `type` doesn't declare the key at all ([`role_is_schema_key_for`]).
#[must_use]
pub fn role_flag(transport_type: Option<&str>, raw_entry: &serde_json::Value) -> Option<String> {
    if !role_is_schema_key_for(transport_type) {
        return None;
    }
    match raw_entry.get("role") {
        Some(serde_json::Value::String(s)) if s == "comms" => Some(s.clone()),
        _ => None,
    }
}

/// Parse a `.mcp.json` payload (raw file contents) into a list of configs.
///
/// `scope` propagates onto every returned config so the approval policy can
/// distinguish project from user origins.
///
/// Returns `Ok(vec![])` when the input has no usable server map.
///
/// Mirrors claude-code `loadMcpServersFromFile`
/// (`utils/plugins/mcpPluginIntegration.ts:240-259`): the server map is
/// `parsed.mcpServers || parsed` — a top-level object WITHOUT an `mcpServers`
/// wrapper is treated as a bare map of `{name: serverConfig}`. Each entry is
/// validated independently; an invalid entry (neither `command` nor `url`, or
/// a shape that fails to deserialize) is logged and SKIPPED — the valid
/// siblings are kept and the file is never dropped.
pub fn parse_mcp_json_string(
    raw: &str,
    scope: ConfigScope,
) -> Result<Vec<McpServerConfig>, McpJsonError> {
    // Parse to a generic value first so we can apply the `parsed.mcpServers ||
    // parsed` precedence before committing to the entry shape. Genuinely
    // invalid JSON still fails here (the `Json` error path), matching the JS
    // `try { jsonParse(content) }` outer guard.
    let parsed: serde_json::Value = serde_json::from_str(raw)?;

    // `parsed.mcpServers || parsed`: prefer an `mcpServers` object when present,
    // otherwise treat the whole top-level object as the server map. A
    // non-object top-level (array/string/number/bool/null) yields no servers,
    // matching JS where `Object.entries` over a non-record produces nothing
    // usable.
    let server_map = match parsed.get("mcpServers") {
        Some(v) => v,
        None => &parsed,
    };
    let serde_json::Value::Object(entries) = server_map else {
        return Ok(Vec::new());
    };
    Ok(build_servers_from_map(entries, scope, false))
}

/// Parse a PLUGIN `.mcp.json` payload (or a plugin manifest's inline
/// `mcpServers` record). Identical to [`parse_mcp_json_string`] except that
/// the two internal-only IDE transports ([`IDE_ONLY_TYPES`]) are recognized.
///
/// The oracle's plugin loader `vve` validates each entry against the FULL
/// 8-arm union — `let B=KY().safeParse(U); if(B.success) M[F]=B.data; else
/// n(`Invalid MCP server config for ${F} in ${d}: ...`)` — rather than the
/// 7-key config table `ZGn` that `xqe` uses for `.mcp.json` / settings /
/// `--mcp-config`. Same file name, different schema layer.
///
/// # Errors
/// Returns [`McpJsonError::Json`] when `raw` is not valid JSON.
pub fn parse_plugin_mcp_json_string(
    raw: &str,
    scope: ConfigScope,
) -> Result<Vec<McpServerConfig>, McpJsonError> {
    let parsed: serde_json::Value = serde_json::from_str(raw)?;
    let server_map = match parsed.get("mcpServers") {
        Some(v) => v,
        None => &parsed,
    };
    let serde_json::Value::Object(entries) = server_map else {
        return Ok(Vec::new());
    };
    Ok(build_servers_from_map(entries, scope, true))
}

/// Parse MCP servers from a GLOBAL CONFIG file (`~/.lingxi.json`): reads ONLY the
/// top-level `mcpServers` object. claude-code reads `config.mcpServers` directly
/// (`wt().mcpServers`) with NO `|| parsed` bare-map fallback — the global config
/// holds dozens of unrelated keys (`numStartups`, `projects`, `oauthAccount`, …)
/// that must never be mistaken for server entries. An absent / non-object
/// `mcpServers` yields no servers.
///
/// # Errors
/// Returns [`McpJsonError::Json`] when `raw` is not valid JSON.
pub fn parse_global_config_mcp_servers(
    raw: &str,
    scope: ConfigScope,
) -> Result<Vec<McpServerConfig>, McpJsonError> {
    let parsed: serde_json::Value = serde_json::from_str(raw)?;
    let Some(serde_json::Value::Object(entries)) = parsed.get("mcpServers") else {
        return Ok(Vec::new());
    };
    Ok(build_servers_from_map(entries, scope, false))
}

/// Build validated `McpServerConfig`s from a `{ name: entry }` server map.
/// Shared by [`parse_mcp_json_string`] (bare-map fallback) and
/// [`parse_global_config_mcp_servers`] (mcpServers-only). Invalid entries are
/// logged + skipped (keeping valid siblings); the result is sorted by name.
fn build_servers_from_map(
    entries: &serde_json::Map<String, serde_json::Value>,
    scope: ConfigScope,
    ide_transports_allowed: bool,
) -> Vec<McpServerConfig> {
    let mut out = Vec::new();
    for (name, raw_entry) in entries {
        if let Some(cfg) = build_entry(name, raw_entry, scope, ide_transports_allowed) {
            out.push(cfg);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The REMOTE transport `type` values the on-disk CONFIG layer recognizes.
///
/// Oracle `ZGn` (2.1.251 Mach-O @160905638) is the lookup table `xqe` uses for
/// every config source — `.mcp.json` (project/enterprise, via `Iqe`), the
/// `user`/`local` settings `configObject`s, and the `--mcp-config` inline
/// payload:
/// ```text
/// var ZGn={stdio:fYe,sse:OAn,http:sGt,"streamable-http":sGt,ws:LAn,sdk:MAn,"claudeai-proxy":NAn};
/// ```
/// `sse-ide` and `ws-ide` are ABSENT from it — see [`IDE_ONLY_TYPES`].
const CONFIG_REMOTE_TYPES: &[&str] = &["sse", "http", "streamable-http", "ws", "claudeai-proxy"];

/// The two internal-only IDE transports.
///
/// They ARE members of the 8-arm union `KY` (@154586195:
/// `KY=m(()=>dt([fYe(),OAn(),l(),d(),sGt(),LAn(),MAn(),NAn()]))`, where `l` is
/// `sse-ide` and `d` is `ws-ide`), which is what the PLUGIN `.mcp.json` loader
/// `vve` validates against (`let B=KY().safeParse(U)`, @160865300 region) and
/// what agent frontmatter declares (`Dnt=m(()=>dt([i(),De(i(),KY())]))`
/// @160484780) — but they are NOT keys of `ZGn`, so at the config layer
/// `xqe` emits `Skipped — unknown MCP server type "sse-ide" for server "…"`
/// and never loads the entry. Two different schema layers wear the same
/// on-disk file name; the port keeps them apart via `ide_transports_allowed`.
///
/// The agent layer accepts them at the SCHEMA level and then drops them per
/// entry in `esn` (@160485539: `Skipping internal-only MCP transport '…'`),
/// which the port does in `agent::mcp_servers` before this module is reached.
const IDE_ONLY_TYPES: &[&str] = &["sse-ide", "ws-ide"];

/// Does `entry` satisfy the REQUIRED fields of the union member its `type`
/// selects? The single source of truth shared by
/// [`server_entry_shape_is_valid`] (the quiet JSON-agent validator) and
/// [`build_server_from_json_entry`] (the loader), so the two can never again
/// disagree about which shapes are acceptable.
///
/// `ide_transports_allowed` selects the schema LAYER: `false` = the config
/// lookup table [`CONFIG_REMOTE_TYPES`] (oracle `ZGn`, 7 keys); `true` = the
/// full union `KY` (8 arms), which additionally admits [`IDE_ONLY_TYPES`].
fn entry_satisfies_schema(entry: &McpJsonEntry, ide_transports_allowed: bool) -> bool {
    match entry.transport_type.as_deref() {
        // `fYe`: `type: N("stdio").optional()`, `command: i().min(1,"Command
        // cannot be empty")`. An absent `type` is a valid stdio entry.
        None | Some("stdio") => entry.command.as_deref().is_some_and(|c| !c.is_empty()),
        // `MAn`: `name: i()` REQUIRED (no `.min(1)`, so `""` is valid); no
        // `command`, no `url`.
        Some("sdk") => entry.name.is_some(),
        // `NAn` @154585377: `url:i(),id:i()` — BOTH required, `id` with no
        // `.optional()` sibling in common with the rest of the union. `id` is
        // held opaque (see the field doc), so the STRING check lives here,
        // where the schema actually declares it.
        Some("claudeai-proxy") => {
            entry.url.is_some() && matches!(entry.id, Some(serde_json::Value::String(_)))
        }
        // Every remaining remote member declares a required `url: i()`.
        Some(t) if CONFIG_REMOTE_TYPES.contains(&t) => entry.url.is_some(),
        // `l`/`d` (sse-ide/ws-ide) additionally require `ideName: i()` — no
        // `.optional()` sibling, so a missing `ideName` fails `safeParse`
        // exactly like a nameless `sdk` or id-less `claudeai-proxy` entry.
        Some(t) if ide_transports_allowed && IDE_ONLY_TYPES.contains(&t) => {
            entry.url.is_some()
                && ide_schema_fields(
                    entry.ide_name.as_ref(),
                    entry.auth_token.as_ref(),
                    entry.ide_running_in_windows.as_ref(),
                    t == "ws-ide",
                )
                .is_some()
        }
        // A `type` outside the layer's union rejects the whole entry.
        _ => false,
    }
}

/// Quiet shape check for one raw `.mcp.json`-style entry value: does it
/// deserialize as a server entry AND satisfy the required fields of the union
/// member its `type` selects ([`entry_satisfies_schema`])?
///
/// Mirrors the zod `safeParse` success predicate WITHOUT logging — used by the
/// strict JSON-agent `mcpServers` validator (`z.array(AgentMcpServerSpecSchema)`,
/// where any invalid record value drops the whole agent) so validation at parse
/// time does not double-log the entry the conversion would log again later.
///
/// The agent layer validates against `KY` (`Dnt=m(()=>dt([i(),De(i(),KY())]))`,
/// @160484780), the 8-arm union, so [`IDE_ONLY_TYPES`] are accepted HERE and
/// dropped per entry later by `agent::mcp_servers` (oracle `esn`) — an
/// `sse-ide` entry must not take the whole agent down with it.
#[must_use]
pub fn server_entry_shape_is_valid(raw_entry: &serde_json::Value) -> bool {
    let stripped = strip_to_claudeai_proxy_schema(raw_entry);
    let Ok(entry) = McpJsonEntry::deserialize(stripped.as_ref()) else {
        return false;
    };
    // §11 — the same `discoveryCache: q().optional()` (no `.catch`) check
    // [`build_entry`] runs. It lives on the RAW value, not on
    // [`McpJsonEntry`], so it has to be repeated here or the two disagree:
    // the agent frontmatter validator would report VALID and the loader would
    // then drop the server, turning a config error into a silent amputation.
    if discovery_cache_flag(entry.transport_type.as_deref(), stripped.as_ref()).is_err() {
        return false;
    }
    if bare_elicitation_flag(entry.transport_type.as_deref(), stripped.as_ref()).is_err() {
        return false;
    }
    entry_satisfies_schema(&entry, true)
}

/// Build ONE validated [`McpServerConfig`] from a raw `{ name: entry }` map
/// entry (the loop body of [`build_servers_from_map`], extracted). `None` when
/// the entry is invalid — logged + skipped (TS `safeParse` failure →
/// `logForDebugging` + `continue`), keeping valid siblings.
///
/// Public because agent frontmatter `mcpServers` bodies share EXACTLY this
/// config shape: claude `agentMcpSpecsToScopedConfigs` (obs) spreads the parsed
/// config and stamps `scope:"agent"` — the agent crate's conversion calls this
/// with [`ConfigScope::Agent`].
#[must_use]
pub fn build_server_from_json_entry(
    name: &str,
    raw_entry: &serde_json::Value,
    scope: ConfigScope,
) -> Option<McpServerConfig> {
    build_entry(name, raw_entry, scope, false)
}

/// [`build_server_from_json_entry`] with the schema layer selected explicitly.
/// `ide_transports_allowed` = `false` is the config layer (oracle `ZGn`);
/// `true` is the plugin `.mcp.json` layer (oracle `KY`, see
/// [`parse_plugin_mcp_json_string`]).
fn build_entry(
    name: &str,
    raw_entry: &serde_json::Value,
    scope: ConfigScope,
    ide_transports_allowed: bool,
) -> Option<McpServerConfig> {
    {
        // Per-entry validation: a shape that fails to deserialize is logged and
        // skipped (TS `safeParse` failure → `logForDebugging` + `continue`),
        // keeping the valid siblings rather than dropping the whole file.
        // `f` strips every key the selected union member does not declare
        // BEFORE `me.data` is used; serde cannot do that per-variant, so
        // `claudeai-proxy` gets the strip explicitly (see
        // [`strip_to_claudeai_proxy_schema`]).
        let raw_entry = strip_to_claudeai_proxy_schema(raw_entry);
        let entry: McpJsonEntry = match McpJsonEntry::deserialize(raw_entry.as_ref()) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    server = %name,
                    error = %e,
                    "mcp.json: invalid server config; skipping entry"
                );
                return None;
            }
        };
        let name = name.to_string();
        // Expand `${VAR}` / `${VAR:-default}` references in the transport
        // fields, mirroring claude-code `expandEnvVars(config)`
        // (`services/mcp/config.ts:556-615`): stdio expands `command`/`args`/
        // `env` VALUES; remote expands `url`/`headers` VALUES. Missing-variable
        // names (deduped, as TS `[...new Set(missingVars)]`) are logged but do
        // NOT fail the parse — the literal `${VAR}` is left in place (TS surfaces
        // a non-fatal config error; a `warn!` is the closest non-breaking
        // analogue for this parser).
        let mut missing: Vec<String> = Vec::new();
        // claude `configError` (`configErrorReason:"url_invalid"`): set when a
        // remote `url` expands to empty; carried on the config, never fatal.
        let mut config_error: Option<String> = None;
        // Discriminate by `type` FIRST. The oracle's config loader `xqe`
        // (@160909118) does exactly this: `let W = typeof B.type==="string" ?
        // B.type : "stdio"` picks the key, `ZGn[W]` picks the schema, and the
        // ONE schema it picked is `safeParse`d — no other arm is tried. (`KY`
        // itself is `dt([...])` and `dt` is `z.union`, NOT
        // `z.discriminatedUnion` — @154569475
        // `function dt(e,t){return new yn({type:"union",options:e,...})}`,
        // `yn = ZodUnion`; the discriminated-union factory is the sibling
        // `ps`. Under a plain union every arm is tried and the entry survives
        // if ANY matches, which for these schemas selects the same arm the
        // `type` literal names and rejects the same inputs — but do not reason
        // from "the discriminator picks exactly one arm" when editing this.)
        // An absent `type` is `fYe`, i.e. stdio, and ONLY stdio: it is never
        // guessed from whichever other fields happen to be present. A `type`
        // value outside the layer's union fails the whole entry. This fixes
        // §12's shape-driven bugs: a bare `{"url":...}` (no `type`, no
        // `command`) used to fall through to Http; `{"type":"http",
        // "command":"x"}` used to be parsed as Stdio because the old code
        // checked `command` before `type`; `{"type":"bogus",...}` and the
        // non-oracle alias `"websocket"` used to silently default to
        // Http/WebSocket instead of being rejected.
        let ty = entry.transport_type.as_deref();
        let bare_elicitation_capability = match bare_elicitation_flag(ty, raw_entry.as_ref()) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(server = %name, error, "mcp.json: invalid server config; skipping entry");
                return None;
            }
        };
        // §11 — `discoveryCache: q().optional()` (sse/http only, NO
        // `.catch`): a present-but-non-boolean value on a type that declares
        // the key fails the whole entry's `safeParse`, exactly like a
        // malformed `oauth` child below. `role` (`.catch(void 0)`) never
        // rejects, so it needs no analogous check here — see
        // [`role_flag`]/[`discovery_cache_flag`]'s doc comments for why
        // neither value is threaded any further than this validation.
        let discovery_cache = match discovery_cache_flag(ty, raw_entry.as_ref()) {
            Ok(value) => value,
            Err(InvalidDiscoveryCacheFlag) => {
                tracing::warn!(
                    server = %name,
                    "mcp.json: invalid \"discoveryCache\" (must be a boolean); skipping entry"
                );
                return None;
            }
        };
        let is_stdio = matches!(ty, None | Some("stdio"));
        let spec = if ty == Some("sdk") {
            // Oracle `MAn` @154585319 (v2.1.251 Mach-O, minified `mcp-sdk.js`
            // chunk): `f({type:N("sdk"),name:i(),timeout:o().optional(),
            // alwaysLoad:q().optional()})` — no `command`, no `url`, and
            // `name` is REQUIRED (its siblings all carry `.optional()`), so
            // `{"type":"sdk"}` fails `safeParse` and the entry is skipped.
            //
            // The control-channel id is that DECLARED `name`, not the map
            // key: the oracle keeps it on the config and reads it back in
            // `tUt` (@160913341,
            // `every(t=>t.type==="sdk"&&t.name==="claude-vscode")`), which
            // gates the enterprise/remote sdk carve-out. The map key stays
            // the registry key (`A[U]=Oe`). A stray `url` on an sdk entry is
            // schema-unknown and silently stripped by zod, so it is ignored.
            let Some(control_channel_id) = entry.name.clone() else {
                tracing::warn!(
                    server = %name,
                    "mcp.json: sdk server is missing the required \"name\"; skipping entry"
                );
                return None;
            };
            McpTransportSpec::SdkControl { control_channel_id }
        } else if is_stdio {
            let Some(cmd) = entry.command else {
                // No `type` (or explicit `type:"stdio"`) and no `command`:
                // TS `safeParse` rejects it → log + skip, keeping valid
                // siblings. Covers the old bug where a bare `{"url":...}`
                // (no `type`) was accepted as Http — it is really an
                // implicit stdio attempt with a missing `command`.
                tracing::warn!(
                    server = %name,
                    "mcp.json: entry missing both command and url; skipping"
                );
                return None;
            };
            // Oracle `fYe`: `command: i().min(1,"Command cannot be empty")`
            // — an explicit empty string fails validation (a whitespace-only
            // string still satisfies `.min(1)`; this is a length check, not
            // a trim check).
            if cmd.is_empty() {
                tracing::warn!(
                    server = %name,
                    "mcp.json: stdio server has an empty \"command\"; skipping entry (oracle: \"Command cannot be empty\")"
                );
                return None;
            }
            McpTransportSpec::Stdio {
                command: expand_field(&cmd, &mut missing),
                args: entry
                    .args
                    .into_iter()
                    .map(|a| expand_field(&a, &mut missing))
                    .collect(),
                env: expand_map_values(entry.env, &mut missing),
            }
        } else {
            // Every remaining recognized `type` is a URL transport. WHICH
            // ones are recognized depends on the schema layer: the config
            // layer sees only `ZGn`'s keys ([`CONFIG_REMOTE_TYPES`]), the
            // plugin `.mcp.json` layer additionally sees the two
            // [`IDE_ONLY_TYPES`] arms of `KY`.
            let recognized = ty.is_some_and(|t| {
                CONFIG_REMOTE_TYPES.contains(&t)
                    || (ide_transports_allowed && IDE_ONLY_TYPES.contains(&t))
            });
            if !recognized {
                // A `type` string this layer's union has no arm for — e.g.
                // `"bogus"`, the non-oracle alias `"websocket"`, or (at the
                // config layer) `"sse-ide"`/`"ws-ide"` — fails the whole
                // entry; it must NOT silently default to Http/WebSocket.
                // Matches `xqe`'s `Skipped — unknown MCP server type "<t>"
                // for server "<name>"` and the port's own
                // `config_diagnostics::KNOWN_MCP_TYPES`, which has always
                // listed exactly `ZGn`'s 7 keys.
                tracing::warn!(
                    server = %name,
                    r#type = ?ty,
                    "mcp.json: unknown MCP server type; skipping entry"
                );
                return None;
            }
            let Some(raw_url) = entry.url else {
                // A recognized remote type with no `url` at all (e.g.
                // `{"type":"http","command":"x"}`, no `url`): the remote
                // schemas require `url` (a required `i()`, not `.optional()`),
                // so this must be rejected — NOT reinterpreted as stdio just
                // because `command` happens to be present.
                tracing::warn!(
                    server = %name,
                    "mcp.json: entry missing both command and url; skipping"
                );
                return None;
            };
            // Oracle `fAn` (@160896200) — the expander — switches on the
            // PARSED entry's `type`: `case"sse":case"http":case"ws":` expand
            // `url`/`headers`; `case"sse-ide":case"ws-ide":u=e;break` and
            // `case"claudeai-proxy":u=e;break` pass the entry straight
            // through UNCHANGED — no `${VAR}` expanded, no missing-var name
            // collected, and `r` (`urlExpandedToEmpty`) stays `false`, so the
            // `url_invalid` `configError` below can never be stamped for
            // either transport. `config_diagnostics::collect_missing_env_vars`
            // already routes `claudeai-proxy` (and, separately, sse-ide/ws-ide)
            // to their expands-nothing arm, so this also restores
            // loader↔diagnostics agreement.
            let expands_env_vars = !matches!(ty, Some("claudeai-proxy" | "sse-ide" | "ws-ide"));
            let url = if expands_env_vars {
                expand_field(&raw_url, &mut missing)
            } else {
                raw_url.clone()
            };
            // `ey_`'s `urlExpandedToEmpty` = `url.trim() !== "" && expanded
            // .trim() === ""`. A url that was ALREADY blank is not that case:
            // the remote schemas (`cLi` @226761199, `J5n` @226762069) declare
            // `url: E.string()` with NO `.min(1)`, so a blank url is
            // schema-VALID and the entry is kept with no `configError` — which
            // is precisely the `zar` shape `mcp list`/`get` render as
            // `- Not configured` (see `McpServerConfig::is_unconfigured`).
            let url = if !raw_url.trim().is_empty() && url.trim().is_empty() {
                // 2.1.220 `klr`/`ey_`: a NON-blank url that expanded to an
                // empty string KEEPS the server, tagged with the byte-exact
                // `configError` (reason `url_invalid` ⇒ INVALID_CONFIG, NOT
                // unconfigured); the connect path never dials it.
                // The spec keeps the UNEXPANDED url so list/get display shows
                // the `${VAR}` reference (the oracle's display view `bEp` maps
                // back to the `expandVars:false` config for the same effect).
                config_error = Some(format!(
                    "'url' {} expanded to an empty string. Set the referenced \
                     environment variable, or update the server's config and \
                     reconnect.",
                    serde_json::to_string(&raw_url).unwrap_or_else(|_| format!("\"{raw_url}\""))
                ));
                raw_url
            } else {
                url
            };
            let headers = expand_header_values(entry.headers, &mut missing);
            let headers_helper = entry
                .headers_helper
                .map(|helper| expand_field(&helper, &mut missing));
            match ty {
                Some("sse") => {
                    let Some(oauth) = validate_oauth_child(entry.oauth) else {
                        tracing::warn!(server = %name, "mcp.json: invalid oauth config; skipping entry");
                        return None;
                    };
                    McpTransportSpec::Sse {
                        url,
                        headers,
                        headers_helper,
                        oauth,
                    }
                }
                // `claudeai-proxy`: binary-confirmed at offsets 74175408 and
                // 81811504. Used for claude.ai hosted MCP servers; the proxy
                // URL + OAuth handling are resolved by the platform layer.
                // Parsed as Http so the transport chain receives the URL —
                // the platform recognises the `claudeai-proxy` discriminator
                // via the `type` tag when it serializes the spec.
                Some("claudeai-proxy") => {
                    // `NAn` @154585377 declares `id: i()` with NO
                    // `.optional()` — in pointed contrast to `displayName`/
                    // `iconUrl` and the rest of the schema's tail — so an
                    // entry with no `id` (or a non-string one) fails
                    // `safeParse` and must be skipped, the same way a
                    // nameless `sdk` entry is skipped above. The port has no
                    // use for the value itself (out of scope); only the
                    // presence of a STRING is checked.
                    if !matches!(entry.id, Some(serde_json::Value::String(_))) {
                        tracing::warn!(
                            server = %name,
                            "mcp.json: claudeai-proxy server is missing the required \"id\"; skipping entry"
                        );
                        return None;
                    }
                    // `NAn` declares NO `headers`, NO `headersHelper` and NO
                    // `oauth`, so `f` strips all three: they never reach
                    // `me.data` and can never reach the dialled transport.
                    // [`strip_to_claudeai_proxy_schema`] already removed them
                    // from the raw entry (so `headers`/`headers_helper` here
                    // are provably empty), but spell the intent out — the
                    // `headersHelper` leak in particular made
                    // [`crate::registry`] EXECUTE a command the oracle never
                    // runs.
                    McpTransportSpec::Http {
                        url,
                        headers: McpHeaders::default(),
                        headers_helper: None,
                        oauth: None,
                    }
                }
                // Reachable ONLY on the plugin `.mcp.json` layer
                // (`ide_transports_allowed`), which validates against `KY`.
                // `l` @154584571: `f({type:N("sse-ide"),url:i(),ideName:i(),
                // ideRunningInWindows:q().optional(),timeout:o().optional(),
                // alwaysLoad:q().optional(),role:t()})` — no `headers`, no
                // `headersHelper`, no `oauth`. `ideName` is a REQUIRED `i()`
                // (no `.optional()` sibling, the same pointed contrast as
                // `sdk`'s `name` / `claudeai-proxy`'s `id` above), so a
                // missing `ideName` fails `safeParse` and the whole entry is
                // skipped, keeping valid siblings.
                Some("sse-ide") => {
                    // `l` declares no `authToken`, so zod strips it here even
                    // when a ws-ide-shaped entry carries one.
                    let Some((ide_name, _, ide_running_in_windows)) = ide_schema_fields(
                        entry.ide_name.as_ref(),
                        entry.auth_token.as_ref(),
                        entry.ide_running_in_windows.as_ref(),
                        false,
                    ) else {
                        tracing::warn!(
                            server = %name,
                            "mcp.json: sse-ide server has a missing or malformed \"ideName\"/\"ideRunningInWindows\"; skipping entry"
                        );
                        return None;
                    };
                    McpTransportSpec::SseIde {
                        url,
                        ide_name,
                        auth_token: None,
                        ide_running_in_windows,
                    }
                }
                // `d` @154584715: `f({type:N("ws-ide"),url:i(),ideName:i(),
                // authToken:i().optional(),ideRunningInWindows:q().optional(),
                // timeout:o().optional(),alwaysLoad:q().optional(),role:t()})`
                // — same shape as `sse-ide` plus an optional `authToken`, and
                // the same required-`ideName` skip rule.
                Some("ws-ide") => {
                    let Some((ide_name, auth_token, ide_running_in_windows)) = ide_schema_fields(
                        entry.ide_name.as_ref(),
                        entry.auth_token.as_ref(),
                        entry.ide_running_in_windows.as_ref(),
                        true,
                    ) else {
                        tracing::warn!(
                            server = %name,
                            "mcp.json: ws-ide server has a missing or malformed \"ideName\"/\"authToken\"/\"ideRunningInWindows\"; skipping entry"
                        );
                        return None;
                    };
                    McpTransportSpec::WsIde {
                        url,
                        ide_name,
                        auth_token,
                        ide_running_in_windows,
                    }
                }
                Some("ws") => McpTransportSpec::WebSocket {
                    url,
                    headers,
                    headers_helper,
                },
                // Only "http" | "streamable-http" remain reachable here.
                _ => {
                    let Some(oauth) = validate_oauth_child(entry.oauth) else {
                        tracing::warn!(server = %name, "mcp.json: invalid oauth config; skipping entry");
                        return None;
                    };
                    McpTransportSpec::Http {
                        url,
                        headers,
                        headers_helper,
                        oauth,
                    }
                }
            }
        };
        if !missing.is_empty() {
            // Dedup preserving first-seen order (TS `[...new Set(missingVars)]`).
            let mut seen = std::collections::HashSet::new();
            let deduped: Vec<&String> = missing.iter().filter(|v| seen.insert(*v)).collect();
            tracing::warn!(
                server = %name,
                missing = ?deduped,
                "mcp.json: unresolved ${{VAR}} references left literal"
            );
        }
        // Resolve the per-server `tools/call` timeout. claude-code applies the
        // `RAn` transform to the sse/http schemas ONLY:
        //   RAn({request_timeout_ms:e, ...t}) =>
        //     {...t, ...(t.timeout===void 0 && e!==void 0 && {timeout: min(e, 300_000)})}
        // i.e. when `timeout` is unset and `request_timeout_ms` is set, fold the
        // alias in capped at 300_000ms (LTm). The stdio / sdk-control / ws /
        // ide schemas carry no `request_timeout_ms` field at all (oracle
        // `LAn`, the `ws` schema, has no such key — zod would strip it even
        // if present), so the alias is honoured for the sse/http-family
        // transports only.
        //
        // `McpTransportSpec::Http` also carries `claudeai-proxy`, whose schema
        // `NAn` (@154585377) declares neither `request_timeout_ms` nor
        // `.transform(iGt)` — the oracle strips the alias for it entirely.
        // Gate the fold on `ty` (not just the `Http` variant) so a
        // `claudeai-proxy` entry never gets it, matching real `http`/
        // `streamable-http`/`sse` byte for byte. The plugin-layer-only
        // `sse-ide`/`ws-ide` arms now build a `SseIde`/`WsIde` spec (§10), so
        // they fall into the `_` arm below and never see the fold either —
        // matching `l`/`d`, which also declare no `request_timeout_ms`.
        let timeout_ms = match &spec {
            McpTransportSpec::Sse { .. } | McpTransportSpec::Http { .. }
                if ty != Some("claudeai-proxy") =>
            {
                entry.timeout.or_else(|| {
                    as_positive_int_ms(entry.request_timeout_ms.as_ref()).map(|e| e.min(300_000))
                })
            }
            _ => entry.timeout,
        };
        let always_load = entry.always_load.unwrap_or(false);
        let spec_kind = spec.kind();
        Some(McpServerConfig {
            name,
            spec,
            scope,
            disabled: entry.disabled,
            timeout_ms,
            discovery_cache,
            always_load,
            tools: entry.tools,
            tool_permissions: entry.tool_permissions,
            config_error,
            metadata: McpServerMetadata {
                bare_elicitation_capability,
                // Keep the existing logical key bytes for ordinary config
                // transports.  Only labels that the enum projection loses
                // need an explicit transport discriminator.
                transport: ty.filter(|raw| *raw != spec_kind).map(str::to_string),
                role: role_flag(ty, raw_entry.as_ref()).map(|_| McpServerRole::Comms),
                ..McpServerMetadata::default()
            },
        })
    }
}

/// Load + merge MCP configs from the project `.mcp.json` (cwd) and the user
/// GLOBAL CONFIG file (`~/.lingxi.json`). Project entries take precedence on
/// name collision.
///
/// The project file is parsed with the `mcpServers || parsed` bare-map fallback
/// (`.mcp.json` may be a bare server map); the global config is parsed
/// `mcpServers`-only ([`parse_global_config_mcp_servers`]) so its many unrelated
/// top-level keys are never mistaken for servers. Missing files yield empty
/// lists. Parse errors are logged via `tracing::warn!` and the corresponding
/// file is skipped — a malformed config must not break startup.
#[must_use]
pub fn load_mcp_json_with_precedence(
    project_path: &Path,
    global_path: &Path,
) -> Vec<McpServerConfig> {
    let mut by_name: HashMap<String, McpServerConfig> = HashMap::new();

    // User-global first (lower precedence). The global path is the `~/.lingxi.json`
    // global config, so read ONLY its `mcpServers` key (no bare-map fallback).
    if let Ok(raw) = std::fs::read_to_string(global_path) {
        match parse_global_config_mcp_servers(
            &raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        ) {
            Ok(cfgs) => {
                for c in cfgs {
                    by_name.insert(c.name.clone(), c);
                }
            }
            Err(e) => tracing::warn!(
                error = %e,
                path = %global_path.display(),
                "skipping malformed user mcp.json"
            ),
        }
    }

    // Project overrides on name collision. Byte-faithful `Iqe` shape/size
    // guard (`crate::config_diagnostics::read_mcp_config_file`) — a missing
    // file is routine and stays silent (matching oracle); a shape/size
    // rejection or other read error is already logged inside that call.
    if let Ok(raw) = crate::config_diagnostics::read_mcp_config_file(
        project_path,
        ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
    ) {
        match parse_mcp_json_string(
            &raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        ) {
            Ok(cfgs) => {
                for c in cfgs {
                    by_name.insert(c.name.clone(), c);
                }
            }
            Err(e) => tracing::warn!(
                error = %e,
                path = %project_path.display(),
                "skipping malformed project .mcp.json"
            ),
        }
    }

    let mut out: Vec<McpServerConfig> = by_name.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Parse LOCAL-scope MCP servers from a GLOBAL CONFIG file (`~/.lingxi.json`):
/// reads ONLY `projects.<project_key>.mcpServers` (claude-code local scope =
/// `wt().projects[Pt()].mcpServers`). NO bare-map fallback. An absent project
/// entry or `mcpServers` key yields no servers.
///
/// `project_key` is the canonical project key
/// ([`migrations::global_config::project_path_for_config`]) — the same key
/// claude-code stores per-project config under.
///
/// # Errors
/// Returns [`McpJsonError::Json`] when `raw` is not valid JSON.
pub fn parse_local_config_mcp_servers(
    raw: &str,
    project_key: &str,
    scope: ConfigScope,
) -> Result<Vec<McpServerConfig>, McpJsonError> {
    let parsed: serde_json::Value = serde_json::from_str(raw)?;
    let Some(serde_json::Value::Object(entries)) = parsed
        .get("projects")
        .and_then(|p| p.get(project_key))
        .and_then(|proj| proj.get("mcpServers"))
    else {
        return Ok(Vec::new());
    };
    Ok(build_servers_from_map(entries, scope, false))
}

/// Load + merge MCP servers across all THREE claude-code config scopes, with
/// precedence LOCAL > PROJECT > USER (claude-code's `["local","project","user"]`
/// priority order — a server defined in a higher scope overrides a same-named
/// one in a lower scope):
/// - USER    = `<global_config>` top-level `mcpServers`
/// - LOCAL   = `<global_config>` `projects.<cwd_key>.mcpServers`
/// - PROJECT = `<cwd>/.mcp.json` (the bare-map `mcpServers || parsed` fallback applies)
///
/// `global_config_path` is `~/.lingxi.json`; `cwd_key` is
/// `migrations::global_config::project_path_for_config(cwd)`. Missing files yield
/// empty lists; parse errors are logged and skipped — a malformed config must
/// not break startup.
#[must_use]
pub fn load_mcp_servers(
    project_mcp_path: &Path,
    global_config_path: &Path,
    cwd: &Path,
) -> Vec<McpServerConfig> {
    let mut by_name: HashMap<String, McpServerConfig> = HashMap::new();
    let global_raw = std::fs::read_to_string(global_config_path).ok();

    // USER (lowest precedence): global config top-level `mcpServers`.
    if let Some(raw) = &global_raw {
        match parse_global_config_mcp_servers(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        ) {
            Ok(cfgs) => {
                for c in cfgs {
                    by_name.insert(c.name.clone(), c);
                }
            }
            Err(e) => tracing::warn!(
                error = %e,
                path = %global_config_path.display(),
                "skipping malformed global config (user-scope mcpServers)"
            ),
        }
    }

    // PROJECT (middle): `<cwd>/.mcp.json` (bare-map fallback allowed). Byte-
    // faithful `Iqe` shape/size guard — see the sibling call in
    // [`load_mcp_json_with_precedence`] for the rationale.
    if let Ok(raw) = crate::config_diagnostics::read_mcp_config_file(
        project_mcp_path,
        ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
    ) {
        match parse_mcp_json_string(
            &raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        ) {
            Ok(cfgs) => {
                // Approval is evaluated before precedence. A pending/rejected
                // project entry remains visible when it is the only candidate,
                // but it may not shadow an already-loaded user server.
                let policy = crate::server_gate::McpPolicyContext::load(global_config_path, cwd);
                for mut c in cfgs {
                    let decision = policy.decide(&c);
                    let project_blocked = matches!(
                        decision,
                        crate::server_gate::McpServerDecision::Block(
                            crate::server_gate::McpServerBlockReason::ProjectPendingApproval
                                | crate::server_gate::McpServerBlockReason::ProjectRejected
                                | crate::server_gate::McpServerBlockReason::ProjectUnresolvedEnvRef
                        )
                    );
                    if project_blocked {
                        c.disabled = true;
                        by_name.entry(c.name.clone()).or_insert(c);
                    } else {
                        by_name.insert(c.name.clone(), c);
                    }
                }
            }
            Err(e) => tracing::warn!(
                error = %e,
                path = %project_mcp_path.display(),
                "skipping malformed project .mcp.json"
            ),
        }
    }

    // LOCAL (highest): global config `projects.<cwd_key>.mcpServers`.
    if let Some(raw) = &global_raw {
        let key = migrations::global_config::project_path_for_config(cwd);
        match parse_local_config_mcp_servers(
            raw,
            &key,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Local),
        ) {
            Ok(cfgs) => {
                for c in cfgs {
                    by_name.insert(c.name.clone(), c);
                }
            }
            Err(e) => tracing::warn!(
                error = %e,
                path = %global_config_path.display(),
                "skipping malformed global config (local-scope mcpServers)"
            ),
        }
    }

    let mut out: Vec<McpServerConfig> = by_name.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn bare_elicitation_capability_config_survives_current_declared_transports() {
        for ty in [
            None,
            Some("stdio"),
            Some("sse"),
            Some("http"),
            Some("streamable-http"),
            Some("ws"),
        ] {
            for bare in [None, Some(false), Some(true)] {
                let mut entry =
                    serde_json::json!({"command": "mcp-test", "url": "https://mcp.test"});
                if let Some(ty) = ty {
                    entry["type"] = ty.into();
                }
                if let Some(bare) = bare {
                    entry["bareElicitationCapability"] = bare.into();
                }
                let config =
                    build_server_from_json_entry("srv", &entry, ConfigScope::Dynamic).unwrap();
                assert_eq!(config.metadata.bare_elicitation_capability, bare, "{ty:?}");
                let serialized = serde_json::to_value(&config).unwrap();
                let decoded: McpServerConfig = serde_json::from_value(serialized.clone()).unwrap();
                assert_eq!(decoded.metadata.bare_elicitation_capability, bare);
                match bare {
                    Some(bare) => {
                        assert_eq!(serialized["metadata"]["bareElicitationCapability"], bare)
                    }
                    None => assert!(serialized["metadata"]
                        .get("bareElicitationCapability")
                        .is_none()),
                }
            }
        }
    }

    #[test]
    fn malformed_bare_elicitation_flag_skips_only_declaring_transport_entries() {
        for value in [
            serde_json::json!(null),
            serde_json::json!(1),
            serde_json::json!("true"),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            for ty in [
                None,
                Some("stdio"),
                Some("sse"),
                Some("http"),
                Some("streamable-http"),
                Some("ws"),
            ] {
                let mut entry = serde_json::json!({"command": "mcp-test", "url": "https://mcp.test", "bareElicitationCapability": value});
                if let Some(ty) = ty {
                    entry["type"] = ty.into();
                }
                let raw = serde_json::json!({"mcpServers": {"bad": entry, "good": {"command": "mcp-good"}}}).to_string();
                let configs = parse_mcp_json_string(&raw, ConfigScope::Dynamic).unwrap();
                assert_eq!(configs.len(), 1, "{ty:?}: {value}");
                assert_eq!(configs[0].name, "good");
            }
            for ty in ["sse-ide", "ws-ide", "sdk", "claudeai-proxy"] {
                let entry = serde_json::json!({"type": ty, "url": "https://mcp.test", "ideName": "IDE", "name": "sdk-client", "id": "connector", "bareElicitationCapability": value});
                let raw = serde_json::json!({"mcpServers": {"srv": entry}}).to_string();
                let configs = parse_plugin_mcp_json_string(&raw, ConfigScope::Dynamic).unwrap();
                assert_eq!(configs.len(), 1, "undeclared {ty} ignores {value}");
                assert_eq!(configs[0].metadata.bare_elicitation_capability, None);
            }
        }
    }

    #[test]
    fn parse_empty_json_yields_no_servers() {
        let cfgs = parse_mcp_json_string(
            "{}",
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert!(cfgs.is_empty());
    }

    #[test]
    fn parse_two_stdio_servers() {
        let raw = r#"{
          "mcpServers": {
            "memory":     { "command": "mcp-memory",     "args": [],         "env": {} },
            "filesystem": { "command": "mcp-filesystem", "args": ["/tmp"],   "env": {} }
          }
        }"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 2);
        // Sorted by name.
        assert_eq!(cfgs[0].name, "filesystem");
        assert_eq!(cfgs[1].name, "memory");
        match &cfgs[0].spec {
            McpTransportSpec::Stdio { command, args, .. } => {
                assert_eq!(command, "mcp-filesystem");
                assert_eq!(args, &vec!["/tmp".to_string()]);
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn parse_http_url_transport() {
        // §12: an http entry MUST name its type explicitly — the oracle's
        // remote schemas are only reached by naming the `type` key `xqe`
        // looks up in `ZGn`, never inferred from `url` alone (see
        // [`bare_url_without_type_is_rejected`] for the case this replaces).
        let raw = r#"{
          "mcpServers": {
            "remote": { "type": "http", "url": "https://example.test/mcp" }
          }
        }"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        match &cfgs[0].spec {
            McpTransportSpec::Http { url, .. } => {
                assert_eq!(url, "https://example.test/mcp");
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    /// §12 fix: before this change, `{"url":...}` with NO `type` (and no
    /// `command`) was silently accepted as Http. At the config layer `xqe`
    /// computes `let W = typeof B.type==="string" ? B.type : "stdio"`, so an
    /// absent `type` selects `ZGn["stdio"] = fYe`, which then fails
    /// `command: i().min(1)` because there is no `command` — the whole entry
    /// is skipped, not routed to Http. (The oracle even has a dedicated
    /// message for this exact shape: `Skipped — MCP server "<n>" has a "url"
    /// but no "type"; add "type": "http" (or "sse" / "ws") to this entry`,
    /// reproduced in `config_diagnostics`.) NOTE the union `KY` is built with
    /// `dt` = `z.union`, NOT `z.discriminatedUnion` (@154569475:
    /// `function dt(e,t){return new yn({type:"union",...})}`, `yn=ZodUnion`);
    /// under a plain union every arm is tried and all of them reject this
    /// shape, so the outcome is the same — but the mechanism is not a
    /// discriminator.
    #[test]
    fn bare_url_without_type_is_rejected() {
        let raw = r#"{"mcpServers":{"remote":{"url":"https://example.test/mcp"}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(
            cfgs.is_empty(),
            "a url-only entry with no type is an implicit (and invalid) stdio attempt, not Http"
        );
    }

    /// §12 fix: `{"type":"bogus","url":...}` must fail the whole entry — the
    /// oracle's config table `ZGn` has no key for an unrecognized `type`
    /// string (`Object.hasOwn(ZGn,W)` is false → `Skipped — unknown MCP
    /// server type`), so it does NOT fall back to Http just because a `url`
    /// happens to be present.
    #[test]
    fn unknown_type_with_url_is_rejected_not_defaulted_to_http() {
        let raw = r#"{"mcpServers":{"remote":{"type":"bogus","url":"https://example.test/mcp"}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(
            cfgs.is_empty(),
            "an unrecognized type must reject the entry"
        );
    }

    /// §12 fix: `{"type":"http","command":"x"}` (no `url`) used to be parsed
    /// as Stdio because the old loader checked `command` before `type`. The
    /// oracle's `http` schema requires `url` (a required field, not a
    /// `command`), so this entry must be rejected entirely.
    #[test]
    fn http_type_with_command_but_no_url_is_rejected() {
        let raw = r#"{"mcpServers":{"remote":{"type":"http","command":"x"}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(
            cfgs.is_empty(),
            "an http-typed entry must never be reinterpreted as stdio"
        );
    }

    /// §12 fix: oracle `fYe`: `command: i().min(1,"Command cannot be empty")`
    /// — an explicit empty string must fail the entry, not pass through as a
    /// (useless) empty-command stdio server.
    #[test]
    fn empty_stdio_command_is_rejected() {
        let raw = r#"{"mcpServers":{"s":{"command":""}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert!(
            cfgs.is_empty(),
            "an empty stdio command must reject the entry"
        );
    }

    /// Oracle `MAn`: `{type:"sdk",name,timeout,alwaysLoad}` — NO `url` and NO
    /// `command`, so the entry must not fall through to the "missing both
    /// command and url" branch. The control-channel id is the entry's own
    /// DECLARED `name`, never a url — sdk entries have none.
    #[test]
    fn sdk_entry_with_no_url_is_accepted_and_keyed_by_name() {
        let raw = r#"{
          "mcpServers": {
            "claude-vscode": { "type": "sdk", "name": "claude-vscode", "timeout": 5000, "alwaysLoad": true }
          }
        }"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1, "sdk entry without url/command must be kept");
        assert_eq!(cfgs[0].name, "claude-vscode");
        match &cfgs[0].spec {
            McpTransportSpec::SdkControl { control_channel_id } => {
                assert_eq!(control_channel_id, "claude-vscode");
            }
            other => panic!("expected SdkControl, got {other:?}"),
        }
        // timeout/alwaysLoad are still honoured (all-transports fields).
        assert_eq!(cfgs[0].timeout_ms, Some(5000));
        assert!(cfgs[0].always_load);
    }

    /// Oracle `MAn` @154585319:
    /// `f({type:N("sdk"),name:i(),timeout:o().optional(),alwaysLoad:q().optional()})`
    /// — `name` is the ONE field with no `.optional()`, so `{"type":"sdk"}`
    /// fails `ZGn["sdk"]().safeParse` and `xqe` emits `Skipped — invalid MCP
    /// server config for "x": name: expected string, received undefined`.
    /// The port used to accept it and register a phantom `SdkControl` server
    /// with no SDK instance behind it.
    #[test]
    fn sdk_entry_without_name_is_rejected() {
        let raw = r#"{"mcpServers":{"x":{"type":"sdk","timeout":5000}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(
            cfgs.is_empty(),
            "`MAn.name` is required; a nameless sdk entry must be skipped"
        );
        // `i()` has no `.min(1)`, so an EMPTY name is schema-valid and kept.
        let raw = r#"{"mcpServers":{"x":{"type":"sdk","name":""}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1, "an empty `name` still satisfies `i()`");
    }

    /// The oracle keeps the DECLARED `name` on the config — it is what `tUt`
    /// (@160913341, `every(t=>t.type==="sdk"&&t.name==="claude-vscode")`)
    /// compares for the enterprise/remote sdk carve-out — while the map key
    /// stays the registry key (`A[U]=Oe`). The port used to discard the
    /// declared name and substitute the map key.
    #[test]
    fn sdk_entry_uses_the_declared_name_not_the_map_key() {
        let raw = r#"{"mcpServers":{"vscode":{"type":"sdk","name":"claude-vscode"}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].name, "vscode", "registry key is the map key");
        match &cfgs[0].spec {
            McpTransportSpec::SdkControl { control_channel_id } => {
                assert_eq!(
                    control_channel_id, "claude-vscode",
                    "the control channel is the DECLARED name, not the map key"
                );
            }
            other => panic!("expected SdkControl, got {other:?}"),
        }
    }

    /// A stray `url` on an sdk entry is schema-unknown (oracle `MAn` has no
    /// `url` field) and must be ignored, not used as the control-channel id.
    #[test]
    fn sdk_entry_ignores_a_stray_url_field() {
        let raw = r#"{"mcpServers":{"srv":{"type":"sdk","name":"srv","url":"should-be-ignored"}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        match &cfgs[0].spec {
            McpTransportSpec::SdkControl { control_channel_id } => {
                assert_eq!(control_channel_id, "srv", "must key off name, not url");
            }
            other => panic!("expected SdkControl, got {other:?}"),
        }
    }

    // ── §12 follow-up: the CONFIG layer's type table is `ZGn` (7 keys), not
    //    the 8-arm union `KY`. `xqe` looks the entry's `type` up in
    //    `ZGn={stdio:fYe,sse:OAn,http:sGt,"streamable-http":sGt,ws:LAn,
    //    sdk:MAn,"claudeai-proxy":NAn}` (@160905638) and, finding no key,
    //    emits `Skipped — unknown MCP server type "sse-ide" for server "ide"`.
    //    The PLUGIN `.mcp.json` loader `vve` validates against `KY` instead
    //    (`let B=KY().safeParse(U)`), which DOES have `sse-ide`/`ws-ide`
    //    arms. Same file name, two schema layers. ──

    #[test]
    fn ide_only_types_are_rejected_at_the_config_layer() {
        for ty in ["sse-ide", "ws-ide"] {
            let raw = format!(
                r#"{{"mcpServers":{{"ide":{{"type":"{ty}","url":"http://127.0.0.1:9999/sse","ideName":"VS Code"}}}}}}"#
            );
            let cfgs = parse_mcp_json_string(
                &raw,
                ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
            )
            .unwrap();
            assert!(
                cfgs.is_empty(),
                "{ty} is absent from ZGn; a .mcp.json entry naming it must be skipped, not dialled as HTTP"
            );
            // Every config entry point shares the table.
            let global =
                format!(r#"{{"mcpServers":{{"ide":{{"type":"{ty}","url":"http://x/sse"}}}}}}"#);
            assert!(parse_global_config_mcp_servers(
                &global,
                ConfigScope::Settings(lingxi_core::types::SettingsScope::User)
            )
            .unwrap()
            .is_empty());
        }
    }

    /// The port's own `config_diagnostics::KNOWN_MCP_TYPES` already listed
    /// exactly `ZGn`'s 7 keys, so before this fix the loader CONNECTED a
    /// server that `/doctor` and startup reported as skipped. Pin the two
    /// against each other so they cannot drift apart again.
    #[test]
    fn loader_and_diagnostics_agree_on_the_config_type_table() {
        for ty in [
            "stdio",
            "sse",
            "http",
            "streamable-http",
            "ws",
            "sdk",
            "claudeai-proxy",
            "sse-ide",
            "ws-ide",
            "websocket",
            "bogus",
        ] {
            // Sweep BOTH an `id` the `claudeai-proxy` arm accepts and one it
            // rejects. Pinning only the string spelling let a shared
            // `id: Option<String>` field silently narrow every OTHER type's
            // acceptance: the loader dropped `{"command":"c","id":7}` (serde
            // type-checks a declared field for every `type`, where zod's
            // catchall-free `f` strips it) while the `stdio` arm of
            // `validation_issues`, which never looks at `id`, stayed silent.
            for id in [serde_json::json!("conn-1"), serde_json::json!(7)] {
                let entry = serde_json::json!({
                    "type": ty, "url": "https://x.test/mcp", "command": "c", "name": "n",
                    "id": id
                });
                let loader_kept = build_server_from_json_entry(
                    "srv",
                    &entry,
                    ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
                )
                .is_some();
                let diagnostics_kept = crate::config_diagnostics::collect_mcp_config_warnings(
                    &serde_json::json!({ "mcpServers": { "srv": entry.clone() } }),
                    ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
                    None,
                )
                .is_empty();
                assert_eq!(
                    loader_kept, diagnostics_kept,
                    "{entry}: loader kept={loader_kept} but diagnostics silent={diagnostics_kept}"
                );
            }
        }
    }

    /// §11's `discoveryCache` rejection must be visible in ALL THREE places
    /// that decide an entry's fate, not just the loader:
    ///
    /// * [`build_entry`] skips it (`q().optional()`, no `.catch`, on
    ///   `OAn`/`sGt` — a non-boolean fails `safeParse`);
    /// * `config_diagnostics::validation_issues` must name the field, so the
    ///   user sees `Skipped — invalid MCP server config for "srv": …` instead
    ///   of the server silently vanishing from `mcp list` (oracle `xqe`
    ///   @160909125 renders the zod issue list into that message);
    /// * [`server_entry_shape_is_valid`] must agree, or a JSON agent's
    ///   frontmatter passes validation (`agent::catalog`) and then silently
    ///   loses the very server it declared.
    #[test]
    fn a_malformed_discovery_cache_is_reported_not_silently_dropped() {
        for ty in ["sse", "http", "streamable-http"] {
            let entry = serde_json::json!({
                "type": ty, "url": "https://x.test/mcp", "discoveryCache": "true"
            });
            assert!(
                build_server_from_json_entry(
                    "srv",
                    &entry,
                    ConfigScope::Settings(lingxi_core::types::SettingsScope::Project)
                )
                .is_none(),
                "{entry}: a non-boolean discoveryCache fails safeParse"
            );
            assert!(
                !server_entry_shape_is_valid(&entry),
                "{entry}: the shape validator must agree with the loader"
            );
            let warnings = crate::config_diagnostics::collect_mcp_config_warnings(
                &serde_json::json!({ "mcpServers": { "srv": entry.clone() } }),
                ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
                None,
            );
            let joined = format!("{warnings:?}");
            assert!(
                joined.contains("discoveryCache"),
                "{entry}: diagnostics must NAME the offending field, got {joined}"
            );
        }
        // Only `OAn`/`sGt` declare the key: on every other type it is an
        // unknown key, stripped unlooked-at, and the entry still loads.
        for entry in [
            serde_json::json!({"command": "c", "discoveryCache": "true"}),
            serde_json::json!({"type": "ws", "url": "ws://x.test/ws", "discoveryCache": 7}),
            serde_json::json!({"type": "sdk", "name": "n", "discoveryCache": []}),
        ] {
            assert!(
                build_server_from_json_entry(
                    "srv",
                    &entry,
                    ConfigScope::Settings(lingxi_core::types::SettingsScope::Project)
                )
                .is_some(),
                "{entry}: discoveryCache is not a schema key for this type"
            );
            assert!(
                server_entry_shape_is_valid(&entry),
                "{entry}: an unknown key must not drop the whole JSON agent"
            );
            assert!(
                crate::config_diagnostics::collect_mcp_config_warnings(
                    &serde_json::json!({ "mcpServers": { "srv": entry.clone() } }),
                    ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
                    None,
                )
                .is_empty(),
                "{entry}: an inert unknown key must not be reported either"
            );
        }
        // The positive control: a well-typed `discoveryCache` still loads.
        let ok = serde_json::json!({
            "type": "http", "url": "https://x.test/mcp", "discoveryCache": false
        });
        assert!(build_server_from_json_entry(
            "srv",
            &ok,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project)
        )
        .is_some());
        assert!(server_entry_shape_is_valid(&ok));
    }

    /// The `id` lesson, re-applied to §10's three IDE keys. `ideName` /
    /// `authToken` / `ideRunningInWindows` are declared ONLY by `l`
    /// (sse-ide, @154584571) and `d` (ws-ide, @154584715). Every other union
    /// member is built by `f` (@154568943), a `z.object` with NO `catchall`,
    /// so zod STRIPS those keys off an `http`/`sse`/`stdio`/… entry without
    /// ever looking at their value — the server LOADS.
    ///
    /// A serde-declared `Option<String>`/`Option<bool>` field is type-checked
    /// for EVERY `type`, so declaring them that way made
    /// `{"type":"http","url":"https://x","authToken":{"v":1}}` fail
    /// `McpJsonEntry::deserialize`; [`build_entry`] then dropped the server
    /// silently (`config_diagnostics` has no `authToken` check, so no
    /// `Skipped —` warning either) and [`server_entry_shape_is_valid`]
    /// returned `false`, taking the WHOLE JSON agent down with it. That is
    /// exactly the regression the `id` field's doc comment records.
    #[test]
    fn ide_only_keys_are_inert_on_every_other_transport_type() {
        let bad = [
            ("ideName", serde_json::json!(7)),
            ("authToken", serde_json::json!({"v": 1})),
            ("ideRunningInWindows", serde_json::json!("true")),
        ];
        let bases = [
            serde_json::json!({"command": "c"}),
            serde_json::json!({"type": "stdio", "command": "c"}),
            serde_json::json!({"type": "http", "url": "https://x.test/mcp"}),
            serde_json::json!({"type": "sse", "url": "https://x.test/sse"}),
            serde_json::json!({"type": "ws", "url": "ws://x.test/ws"}),
            serde_json::json!({"type": "sdk", "name": "n"}),
        ];
        for base in &bases {
            for (key, value) in &bad {
                let mut entry = base.clone();
                entry
                    .as_object_mut()
                    .unwrap()
                    .insert((*key).to_string(), value.clone());
                assert!(
                    build_server_from_json_entry(
                        "srv",
                        &entry,
                        ConfigScope::Settings(lingxi_core::types::SettingsScope::Project)
                    )
                    .is_some(),
                    "{entry}: zod strips {key} off this type without type-checking it; \
                     the server must still load"
                );
                assert!(
                    server_entry_shape_is_valid(&entry),
                    "{entry}: a stripped {key} must not make the shape validator drop \
                     the whole JSON agent"
                );
            }
        }
    }

    /// The other half of the same contract: on the two types that DO declare
    /// them, a wrong-typed value fails `safeParse` and the entry is skipped —
    /// `ideName:i()` (required string), `authToken:i().optional()`,
    /// `ideRunningInWindows:q().optional()`. Holding the fields opaque must
    /// not lose those checks.
    #[test]
    fn ide_only_keys_are_type_checked_on_the_ide_transports() {
        let cases = [
            ("sse-ide", "ideName", serde_json::json!(7)),
            ("sse-ide", "ideRunningInWindows", serde_json::json!("true")),
            ("ws-ide", "ideName", serde_json::json!(7)),
            ("ws-ide", "authToken", serde_json::json!({"v": 1})),
            ("ws-ide", "ideRunningInWindows", serde_json::json!(1)),
        ];
        for (ty, key, value) in &cases {
            let mut entry = serde_json::json!({
                "type": ty, "url": "http://x.test/sse", "ideName": "VS Code"
            });
            entry
                .as_object_mut()
                .unwrap()
                .insert((*key).to_string(), value.clone());
            let raw = serde_json::json!({ "mcpServers": { "ide": entry.clone() } });
            let cfgs =
                parse_plugin_mcp_json_string(&raw.to_string(), ConfigScope::Dynamic).unwrap();
            assert!(
                cfgs.is_empty(),
                "{entry}: {ty} declares {key}, so a wrong-typed value must skip the entry"
            );
            assert!(
                !server_entry_shape_is_valid(&entry),
                "{entry}: the shape validator must agree with the loader"
            );
        }
        // The positive control: a well-typed entry still loads.
        let ok = serde_json::json!({"mcpServers": {"ide": {
            "type": "ws-ide", "url": "ws://x.test/ws", "ideName": "VS Code",
            "authToken": "t", "ideRunningInWindows": true
        }}});
        let cfgs = parse_plugin_mcp_json_string(&ok.to_string(), ConfigScope::Dynamic).unwrap();
        assert_eq!(cfgs.len(), 1, "a well-typed ws-ide entry must still load");
        match &cfgs[0].spec {
            McpTransportSpec::WsIde {
                ide_name,
                auth_token,
                ide_running_in_windows,
                ..
            } => {
                assert_eq!(ide_name, "VS Code");
                assert_eq!(auth_token.as_deref(), Some("t"));
                assert!(*ide_running_in_windows);
            }
            other => panic!("expected WsIde, got {other:?}"),
        }
    }

    /// The plugin layer is `KY`, so the two IDE arms survive there, and (§10)
    /// are now built as their own typed [`McpTransportSpec::SseIde`] /
    /// [`McpTransportSpec::WsIde`] — no longer dialled as plain `Http`. This
    /// is what keeps the config-layer tightening from silently amputating
    /// plugin-declared IDE servers.
    #[test]
    fn plugin_layer_still_accepts_the_ide_only_types() {
        for ty in ["sse-ide", "ws-ide"] {
            let raw = format!(
                r#"{{"mcpServers":{{"ide":{{"type":"{ty}","url":"http://127.0.0.1:9999/sse","ideName":"VS Code"}}}}}}"#
            );
            let cfgs = parse_plugin_mcp_json_string(&raw, ConfigScope::Dynamic).unwrap();
            assert_eq!(
                cfgs.len(),
                1,
                "{ty} is an arm of KY, which the plugin loader uses"
            );
        }
        // The plugin layer is not a free-for-all: a type outside KY still fails.
        let raw = r#"{"mcpServers":{"x":{"type":"bogus","url":"http://x"}}}"#;
        assert!(parse_plugin_mcp_json_string(raw, ConfigScope::Dynamic)
            .unwrap()
            .is_empty());
    }

    // ── §10: sse-ide/ws-ide build typed specs, not plain Http ───────────

    /// The core §10 regression: before this fix, `Some("sse-ide" | "ws-ide")`
    /// built a plain `McpTransportSpec::Http`, erasing `ideName`/`authToken`
    /// and (via `oauth::server_key`'s catch-all `other` arm returning the
    /// SAME `kind()` either way) never actually breaking a stored OAuth key —
    /// but the transport was unmodelled and undialable as an IDE connection.
    #[test]
    fn sse_ide_builds_a_typed_spec_with_ide_name() {
        let raw = r#"{"mcpServers":{"ide":{
            "type":"sse-ide","url":"http://127.0.0.1:9999/sse","ideName":"VS Code",
            "ideRunningInWindows":true
        }}}"#;
        let cfgs = parse_plugin_mcp_json_string(raw, ConfigScope::Dynamic).unwrap();
        assert_eq!(cfgs.len(), 1);
        match &cfgs[0].spec {
            McpTransportSpec::SseIde {
                url,
                ide_name,
                auth_token,
                ide_running_in_windows,
            } => {
                assert_eq!(url, "http://127.0.0.1:9999/sse");
                assert_eq!(ide_name, "VS Code");
                assert!(auth_token.is_none());
                assert!(ide_running_in_windows);
            }
            other => panic!("expected SseIde, got {other:?}"),
        }
    }

    /// `ws-ide` additionally carries an optional `authToken`.
    #[test]
    fn ws_ide_builds_a_typed_spec_with_ide_name_and_auth_token() {
        let raw = r#"{"mcpServers":{"ide":{
            "type":"ws-ide","url":"ws://127.0.0.1:9999/ws","ideName":"Cursor",
            "authToken":"secret-token"
        }}}"#;
        let cfgs = parse_plugin_mcp_json_string(raw, ConfigScope::Dynamic).unwrap();
        assert_eq!(cfgs.len(), 1);
        match &cfgs[0].spec {
            McpTransportSpec::WsIde {
                url,
                ide_name,
                auth_token,
                ide_running_in_windows,
            } => {
                assert_eq!(url, "ws://127.0.0.1:9999/ws");
                assert_eq!(ide_name, "Cursor");
                assert_eq!(auth_token.as_deref(), Some("secret-token"));
                assert!(!ide_running_in_windows);
            }
            other => panic!("expected WsIde, got {other:?}"),
        }
    }

    /// `ws-ide` with no `authToken` at all still builds (it is `.optional()`).
    #[test]
    fn ws_ide_auth_token_is_optional() {
        let raw = r#"{"mcpServers":{"ide":{
            "type":"ws-ide","url":"ws://x/ws","ideName":"VS Code"
        }}}"#;
        let cfgs = parse_plugin_mcp_json_string(raw, ConfigScope::Dynamic).unwrap();
        assert_eq!(cfgs.len(), 1);
        match &cfgs[0].spec {
            McpTransportSpec::WsIde { auth_token, .. } => assert!(auth_token.is_none()),
            other => panic!("expected WsIde, got {other:?}"),
        }
    }

    /// Oracle `l`/`d` declare `ideName: i()` REQUIRED (no `.optional()`
    /// sibling) — a missing `ideName` fails `safeParse` and the entry is
    /// SKIPPED, not defaulted or dialled anyway.
    #[test]
    fn sse_ide_and_ws_ide_without_ide_name_are_skipped() {
        for ty in ["sse-ide", "ws-ide"] {
            let raw =
                format!(r#"{{"mcpServers":{{"ide":{{"type":"{ty}","url":"http://x/sse"}}}}}}"#);
            let cfgs = parse_plugin_mcp_json_string(&raw, ConfigScope::Dynamic).unwrap();
            assert!(
                cfgs.is_empty(),
                "{ty} with no ideName must fail safeParse and be skipped, not dialled anyway"
            );
        }
        // The schema-shape validator (used by the JSON-agent path) must agree.
        for ty in ["sse-ide", "ws-ide"] {
            let entry = serde_json::json!({"type": ty, "url": "http://x/sse"});
            assert!(
                !server_entry_shape_is_valid(&entry),
                "{ty} with no ideName must fail the shape validator too"
            );
        }
    }

    /// `${VAR}` references in an ide transport's `url` are left LITERAL —
    /// oracle `fAn`'s `case"sse-ide":case"ws-ide":u=e;break` passes the entry
    /// through unchanged, unlike `sse`/`http`/`ws` which expand `url`.
    #[test]
    fn sse_ide_url_is_not_env_expanded() {
        // The variable must be SET, and to a value the expander would visibly
        // substitute. Probing with an UNSET name proves nothing: the expander
        // leaves an unresolved `${VAR}` literal in place on EVERY transport
        // (`env_expansion.rs`'s "Missing everywhere: record + leave the
        // literal `${...}` in place", pinned by
        // `missing_var_is_left_literal_and_does_not_fail_parse`), so the
        // assertion below would hold with or without the `expands_env_vars`
        // gate. Name unique to this test, so no sibling can observe it.
        std::env::set_var("LX_SSE_IDE_EXPAND_PROBE", "real.example");
        std::env::set_var("LX_SSE_IDE_EMPTY_PROBE", "");
        for ty in ["sse-ide", "ws-ide"] {
            let raw = format!(
                r#"{{"mcpServers":{{"ide":{{
                "type":"{ty}","url":"http://${{LX_SSE_IDE_EXPAND_PROBE}}/sse","ideName":"VS Code"
            }}}}}}"#
            );
            let cfgs = parse_plugin_mcp_json_string(&raw, ConfigScope::Dynamic).unwrap();
            assert_eq!(cfgs.len(), 1);
            let url = match &cfgs[0].spec {
                McpTransportSpec::SseIde { url, .. } | McpTransportSpec::WsIde { url, .. } => url,
                other => panic!("expected an IDE spec for {ty}, got {other:?}"),
            };
            assert_eq!(
                url, "http://${LX_SSE_IDE_EXPAND_PROBE}/sse",
                "`fAn`'s `case\"sse-ide\":case\"ws-ide\":u=e;break` passes the entry \
                 through UNCHANGED, so a SET variable must still not be substituted"
            );
            // No config_error is stamped either — the oracle's `r`
            // (urlExpandedToEmpty) stays false because the url never reaches
            // the expander.
            assert!(cfgs[0].config_error.is_none());
        }
        // Positive control: the same url under `sse` — a transport `fAn` DOES
        // expand — substitutes the value. Without this the test cannot tell
        // "not expanded" from "expander did nothing".
        let raw = r#"{"mcpServers":{"s":{
            "type":"sse","url":"http://${LX_SSE_IDE_EXPAND_PROBE}/sse"
        }}}"#;
        let cfgs = parse_plugin_mcp_json_string(raw, ConfigScope::Dynamic).unwrap();
        match &cfgs[0].spec {
            McpTransportSpec::Sse { url, .. } => assert_eq!(url, "http://real.example/sse"),
            other => panic!("expected Sse, got {other:?}"),
        }
        // Second control, on the `configError` half: a url that expands to
        // EMPTY stamps `url_invalid` on `sse` but never on an IDE transport.
        let raw = r#"{"mcpServers":{"ide":{
            "type":"ws-ide","url":"${LX_SSE_IDE_EMPTY_PROBE}","ideName":"VS Code"
        }}}"#;
        let cfgs = parse_plugin_mcp_json_string(raw, ConfigScope::Dynamic).unwrap();
        assert_eq!(cfgs[0].config_error, None);
        let raw = r#"{"mcpServers":{"s":{"type":"sse","url":"${LX_SSE_IDE_EMPTY_PROBE}"}}}"#;
        let cfgs = parse_plugin_mcp_json_string(raw, ConfigScope::Dynamic).unwrap();
        assert!(
            cfgs[0].config_error.is_some(),
            "positive control: `sse` DOES stamp url_invalid on an empty expansion"
        );
        std::env::remove_var("LX_SSE_IDE_EXPAND_PROBE");
        std::env::remove_var("LX_SSE_IDE_EMPTY_PROBE");
    }

    /// `oauth::server_key`'s `{sse, http}` match arms are the ONLY ones with
    /// a hand-picked hash label — every other spec (including the two new
    /// [`McpTransportSpec::SseIde`]/[`McpTransportSpec::WsIde`] variants this
    /// task adds) falls into its `other => (other.kind(), "", &empty)`
    /// catch-all. This test is NOT the frozen byte-pin (that is
    /// `mcp/tests/server_key_pin_test.rs`, untouched by this change) — it
    /// only proves the new `WsIde` variant reaches that catch-all rather than
    /// accidentally matching the `sse`/`http` arms (which would hash a
    /// `ws-ide`-typed spec under the wrong `type` label and could collide
    /// with an unrelated server).
    #[test]
    fn ws_ide_hashes_as_its_own_kind_not_sse_or_http() {
        let ws_ide = McpTransportSpec::WsIde {
            url: "ws://x/ws".to_string(),
            ide_name: "VS Code".to_string(),
            auth_token: None,
            ide_running_in_windows: false,
        };
        let key = crate::oauth::server_key("srv", &ws_ide);
        // FROZEN pin. `assert_ne!` against an `sse`/`http` spec cannot fail:
        // those two arms fold their own `url` into the hash, so the digests
        // differ no matter what label `WsIde` hashes under — corrupting
        // `McpTransportSpec::kind()` to return `"sse"` for `WsIde` left both
        // inequalities true and the test green. Pin the DIGEST instead, so
        // the label itself is asserted: `sha256(r#"{"type":"ws-ide","url":"",
        // "headers":{}}"#)[..16]`, computed independently of this code.
        // A change here logs every ws-ide user out of their stored OAuth /
        // XAA tokens (see `server_key_is_frozen_because_it_keys_every_stored
        // _oauth_token`), so it must never move silently.
        assert_eq!(
            key, "srv|43935e76a02bd444",
            "ws-ide must hash under its OWN `type` label \"ws-ide\" with an              empty url and no headers"
        );
        // `server_key` hashes `{type: kind(), url: "", headers: {}}` for any
        // non-sse/http spec — url is NOT the transport's real url (a remote
        // spec's own url is folded in only for `sse`/`http`).
        let sse_with_same_name_and_url = McpTransportSpec::Sse {
            url: "ws://x/ws".to_string(),
            headers: McpHeaders::default(),
            headers_helper: None,
            oauth: None,
        };
        assert_ne!(
            key,
            crate::oauth::server_key("srv", &sse_with_same_name_and_url),
            "ws-ide must not hash identically to an sse spec sharing the same url"
        );
        let http_with_same_name = McpTransportSpec::Http {
            url: String::new(),
            headers: McpHeaders::default(),
            headers_helper: None,
            oauth: None,
        };
        assert_ne!(
            key,
            crate::oauth::server_key("srv", &http_with_same_name),
            "ws-ide must not hash identically to an http spec"
        );
        // The sibling §10 variant, pinned the same way.
        let sse_ide = McpTransportSpec::SseIde {
            url: "http://x/sse".to_string(),
            ide_name: "VS Code".to_string(),
            auth_token: None,
            ide_running_in_windows: false,
        };
        assert_eq!(
            crate::oauth::server_key("srv", &sse_ide),
            "srv|d91885c01a8cb281",
            "sse-ide must hash under its OWN `type` label \"sse-ide\""
        );
    }

    /// [`server_entry_shape_is_valid`] documents itself as mirroring the zod
    /// `safeParse` success predicate, and the JSON-agent validator
    /// (`agent::catalog::validate_mcp_servers_schema`) drops a whole agent on
    /// a `false`. It was left on the old `command || url || type=="sdk"` rule
    /// while the loader was tightened four ways, so an agent could pass
    /// validation and then silently lose the very server the validator
    /// blessed. Both now route through `entry_satisfies_schema`; this pins
    /// them together over every shape the loader tests exercise.
    #[test]
    fn shape_validator_agrees_with_the_loader() {
        let cases = [
            // (entry, expected-accepted)
            (serde_json::json!({"command": "docs-server"}), true),
            (serde_json::json!({"type": "stdio", "command": "c"}), true),
            (serde_json::json!({"command": ""}), false),
            (serde_json::json!({"url": "https://x.test/mcp"}), false),
            (
                serde_json::json!({"type": "bogus", "url": "https://x.test/mcp"}),
                false,
            ),
            (
                serde_json::json!({"type": "websocket", "url": "wss://x.test"}),
                false,
            ),
            (serde_json::json!({"type": "http", "command": "x"}), false),
            (
                serde_json::json!({"type": "http", "url": "https://x.test/mcp"}),
                true,
            ),
            (
                serde_json::json!({"type": "ws", "url": "wss://x.test"}),
                true,
            ),
            (serde_json::json!({"type": "sdk"}), false),
            (serde_json::json!({"type": "sdk", "name": "n"}), true),
            (
                serde_json::json!({"type": "claudeai-proxy", "url": "https://x.test/mcp"}),
                false,
            ),
            (
                serde_json::json!({
                    "type": "claudeai-proxy", "url": "https://x.test/mcp", "id": "conn-1"
                }),
                true,
            ),
            (serde_json::json!({"timeout": 0, "command": "c"}), false),
        ];
        // Collect ALL mismatches rather than aborting on the first, so a
        // regression names every shape it broke.
        let mut bad: Vec<String> = Vec::new();
        for (entry, expected) in cases {
            let validator = server_entry_shape_is_valid(&entry);
            let loader = build_server_from_json_entry("srv", &entry, ConfigScope::Agent).is_some();
            if validator != expected || loader != expected {
                bad.push(format!(
                    "{entry}: expected {expected}, validator={validator}, loader={loader}"
                ));
            }
        }
        assert!(bad.is_empty(), "validator/loader disagreed on: {bad:#?}");
        // The one deliberate difference: the agent schema is `KY`, so the two
        // internal-only IDE transports pass validation (the agent-level drop
        // happens later, in `agent::mcp_servers`, mirroring oracle `esn`) even
        // though the CONFIG-layer loader rejects them.
        let ide =
            serde_json::json!({"type": "sse-ide", "url": "http://x/sse", "ideName": "VS Code"});
        assert!(server_entry_shape_is_valid(&ide));
        assert!(build_server_from_json_entry("ide", &ide, ConfigScope::Agent).is_none());
    }

    #[test]
    fn parses_remote_headers_helper_and_pinned_oauth_scopes() {
        let raw = r#"{
          "mcpServers": {
            "remote": {
              "type": "http",
              "url": "https://example.test/mcp",
              "headers": {"Authorization": "static"},
              "headersHelper": "printf '{}';",
              "oauth": {
                "clientId": "client-id",
                "callbackPort": 8123,
                "authServerMetadataUrl": "https://auth.example/.well-known/oauth-authorization-server",
                "scopes": "read write",
                "xaa": false
              }
            }
          }
        }"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        let McpTransportSpec::Http {
            headers_helper,
            oauth: Some(oauth),
            ..
        } = &cfgs[0].spec
        else {
            panic!("expected HTTP OAuth config")
        };
        assert_eq!(headers_helper.as_deref(), Some("printf '{}';"));
        assert_eq!(oauth.client_id.as_deref(), Some("client-id"));
        assert_eq!(oauth.callback_port, Some(8123));
        assert_eq!(oauth.scopes.as_deref(), Some("read write"));
    }

    // ── §12 OAuth-child validation (oracle `a()`, 2.1.251 Mach-O @154583724):
    //    `callbackPort: v().int().positive()`, `authServerMetadataUrl: i()
    //    .url().startsWith("https://")`, `scopes: i().min(1)` — all
    //    `.optional()` but NONE `.catch()`, so a PRESENT-but-invalid value
    //    fails the WHOLE entry, exactly like a malformed `command`/`url`. ──

    #[test]
    fn oauth_callback_port_zero_rejects_the_whole_entry() {
        let raw = r#"{"mcpServers":{"remote":{"type":"http","url":"https://x.test","oauth":{"callbackPort":0}}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(
            cfgs.is_empty(),
            "callbackPort:0 fails `.int().positive()`; the server must be skipped, not connected with port 0"
        );
    }

    #[test]
    fn oauth_non_https_auth_server_metadata_url_rejects_the_whole_entry() {
        let raw = r#"{"mcpServers":{"remote":{"type":"http","url":"https://x.test","oauth":{"authServerMetadataUrl":"http://auth.example/.well-known/oauth-authorization-server"}}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(
            cfgs.is_empty(),
            "a non-https authServerMetadataUrl must fail `.startsWith(\"https://\")`"
        );

        // An arbitrary non-URL string must also reject (the `.url()` half).
        let raw = r#"{"mcpServers":{"remote":{"type":"http","url":"https://x.test","oauth":{"authServerMetadataUrl":"not-a-url"}}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(
            cfgs.is_empty(),
            "a non-URL authServerMetadataUrl must fail `.url()`"
        );
    }

    #[test]
    fn oauth_empty_scopes_rejects_the_whole_entry() {
        let raw = r#"{"mcpServers":{"remote":{"type":"http","url":"https://x.test","oauth":{"scopes":""}}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(cfgs.is_empty(), "an empty scopes string fails `.min(1)`");
    }

    #[test]
    fn oauth_valid_https_metadata_url_is_kept() {
        let raw = r#"{"mcpServers":{"remote":{"type":"http","url":"https://x.test","oauth":{"authServerMetadataUrl":"https://auth.example/.well-known/oauth-authorization-server","callbackPort":8123,"scopes":"read"}}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(
            cfgs.len(),
            1,
            "a fully valid oauth child must keep the server"
        );
    }

    #[test]
    fn websocket_accepts_ordered_headers_and_helper() {
        // §12: only the oracle's literal `"ws"` names the ws schema
        // (`LAn`, 2.1.251 Mach-O @154585377) — the non-oracle alias
        // `"websocket"` is exercised separately in
        // [`non_oracle_websocket_alias_is_rejected`].
        let raw = r#"{
          "mcpServers": {
            "remote": {
              "type": "ws",
              "url": "wss://example.test/mcp",
              "headers": {"X-First": "1", "X-Second": "2"},
              "headersHelper": "helper"
            }
          }
        }"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        let McpTransportSpec::WebSocket {
            headers,
            headers_helper,
            ..
        } = &cfgs[0].spec
        else {
            panic!("expected WebSocket config")
        };
        assert_eq!(
            headers.keys().map(String::as_str).collect::<Vec<_>>(),
            ["X-First", "X-Second"]
        );
        assert_eq!(headers_helper.as_deref(), Some("helper"));
    }

    /// §12 fix: `"websocket"` is not one of the oracle's union
    /// literals (only `"ws"` is) — it must reject, not silently alias to
    /// `McpTransportSpec::WebSocket`.
    #[test]
    fn non_oracle_websocket_alias_is_rejected() {
        let raw =
            r#"{"mcpServers":{"remote":{"type":"websocket","url":"wss://example.test/mcp"}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(cfgs.is_empty(), "\"websocket\" is not an oracle-valid type");
    }

    /// §12 fix: oracle `LAn` (the `ws` schema) has no `request_timeout_ms`
    /// field at all — unlike `sse`/`http`, the alias must never be folded
    /// into `timeout` for a `ws` transport.
    #[test]
    fn ws_ignores_request_timeout_ms() {
        let raw =
            r#"{"mcpServers":{"r":{"type":"ws","url":"wss://x.test","request_timeout_ms":45000}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        assert_eq!(
            cfgs[0].timeout_ms, None,
            "ws must not fold request_timeout_ms (oracle schema strips it)"
        );
    }

    /// Oracle `NAn` @154585377 declares `timeout:o().optional()` but NO
    /// `request_timeout_ms` field and no `.transform(iGt)` — unlike
    /// `sse`/`http`'s `sGt`/`OAn` (which DO fold the alias via `RAn`),
    /// `claudeai-proxy` must never receive it, even though it is parsed onto
    /// the same `McpTransportSpec::Http` Rust variant those two use.
    #[test]
    fn claudeai_proxy_ignores_request_timeout_ms() {
        let raw = r#"{"mcpServers":{"r":{"type":"claudeai-proxy","url":"https://x.test","id":"conn-1","request_timeout_ms":45000}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        assert_eq!(
            cfgs[0].timeout_ms, None,
            "claudeai-proxy must not fold request_timeout_ms (oracle schema NAn has no such field)"
        );
        // An explicit `timeout` is still honoured — only the alias is stripped.
        let raw = r#"{"mcpServers":{"r":{"type":"claudeai-proxy","url":"https://x.test","id":"conn-1","timeout":9000,"request_timeout_ms":45000}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(cfgs[0].timeout_ms, Some(9000));
    }

    /// `id` is declared by `NAn` ALONE. `f` (@154568943) — the factory every
    /// member of `ZGn`/`KY` is built with — is `new BEt({type:"object",shape,
    /// ...})` with NO `catchall` (contrast `ot`, `catchall:UEt()`, and `un`,
    /// `catchall:_e()`), so an `id` of ANY type is STRIPPED off every other
    /// member and the server loads. Modelling `id` as `Option<String>` on the
    /// shared [`McpJsonEntry`] type-checked it for every `type` instead:
    /// `{"command":"c","id":7}` failed `McpJsonEntry::deserialize`, so the
    /// loader skipped it in total diagnostic silence (the `stdio` arm of
    /// `validation_issues` never looks at `id`) and
    /// [`server_entry_shape_is_valid`] returned `false`, which
    /// `agent::catalog` turns into dropping the WHOLE JSON agent.
    #[test]
    fn a_non_string_id_is_stripped_everywhere_but_claudeai_proxy() {
        for entry in [
            serde_json::json!({"command": "mcp-memory", "id": 123}),
            serde_json::json!({"type": "stdio", "command": "c", "id": 123}),
            serde_json::json!({"type": "http", "url": "https://x.test/mcp", "id": 42}),
            serde_json::json!({"type": "sse", "url": "https://x.test/sse", "id": [1]}),
            serde_json::json!({"type": "ws", "url": "wss://x.test", "id": {"a": 1}}),
            serde_json::json!({"type": "sdk", "name": "n", "id": 7}),
        ] {
            assert!(
                build_server_from_json_entry(
                    "srv",
                    &entry,
                    ConfigScope::Settings(lingxi_core::types::SettingsScope::Project)
                )
                .is_some(),
                "`f` has no catchall, so a non-string `id` is stripped, not \
                 type-checked: {entry}"
            );
            assert!(
                server_entry_shape_is_valid(&entry),
                "the shape validator drops the whole JSON agent on `false`: {entry}"
            );
        }
        // `NAn` is the one member that DECLARES `id: i()`, so there — and only
        // there — a non-string value really does fail `safeParse`.
        let proxy = serde_json::json!({
            "type": "claudeai-proxy", "url": "https://x.test", "id": 7
        });
        assert!(build_server_from_json_entry(
            "p",
            &proxy,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project)
        )
        .is_none());
        assert!(!server_entry_shape_is_valid(&proxy));
    }

    /// `NAn` (@154585377) declares NO `headers`, NO `headersHelper` and NO
    /// `oauth`, and `f` has no catchall, so all three are STRIPPED off a
    /// `claudeai-proxy` entry: a malformed one must not drop the server, and
    /// a well-formed one must not reach the dialled transport. The
    /// `headersHelper` half is the one with teeth —
    /// `registry::has_headers_helper` / `resolve_headers_helper_in` EXECUTE
    /// the string as a shell command on connect.
    #[test]
    fn claudeai_proxy_strips_the_keys_nan_does_not_declare() {
        // (a) A malformed `headers` is stripped, not decoded: the server loads.
        let malformed = serde_json::json!({
            "type": "claudeai-proxy", "url": "https://x.test", "id": "c1", "headers": "nope"
        });
        assert!(
            build_server_from_json_entry(
                "p",
                &malformed,
                ConfigScope::Settings(lingxi_core::types::SettingsScope::Project)
            )
            .is_some(),
            "`NAn` has no `headers` key, so `f` strips it and the server loads"
        );
        assert!(server_entry_shape_is_valid(&malformed));

        // (b) Well-formed `headers`/`headersHelper`/`oauth` never reach the spec.
        let raw = r#"{"mcpServers":{"p":{"type":"claudeai-proxy","url":"https://x.test",
            "id":"c1","headers":{"X":"y"},"headersHelper":"echo leak",
            "oauth":{"clientId":"z"}}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        match &cfgs[0].spec {
            McpTransportSpec::Http {
                headers,
                headers_helper,
                oauth,
                ..
            } => {
                assert!(
                    headers.is_empty(),
                    "`NAn` declares no `headers`: {headers:?}"
                );
                assert_eq!(
                    headers_helper.as_deref(),
                    None,
                    "a forwarded `headersHelper` is EXECUTED on connect"
                );
                assert!(oauth.is_none(), "`NAn` declares no `oauth`");
            }
            other => panic!("claudeai-proxy dials as Http: {other:?}"),
        }
        assert!(!crate::headers_helper::has_headers_helper(&cfgs[0].spec));
    }

    /// Oracle `fAn` (@160896200) passes `claudeai-proxy` through untouched —
    /// `case"claudeai-proxy":u=e;break` — so no `${VAR}` in `url` is expanded
    /// and `urlExpandedToEmpty` (`r`) stays `false`, which means the
    /// `url_invalid` `configError` can never be stamped for this transport.
    /// `sse`/`http`/`ws` (the `case"sse":case"http":case"ws"` arm) still do
    /// both — the positive control below.
    #[test]
    fn claudeai_proxy_never_expands_env_vars() {
        // Names unique to this test, so no sibling test can observe them.
        std::env::set_var("LX_PROXY_EXPAND_PROBE", "real.example");
        std::env::set_var("LX_PROXY_EMPTY_PROBE", "");
        let raw = r#"{"mcpServers":{"p":{"type":"claudeai-proxy",
            "url":"https://${LX_PROXY_EXPAND_PROBE}/mcp","id":"c1"}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        match &cfgs[0].spec {
            McpTransportSpec::Http { url, .. } => assert_eq!(
                url, "https://${LX_PROXY_EXPAND_PROBE}/mcp",
                "`fAn` does not expand claudeai-proxy"
            ),
            other => panic!("{other:?}"),
        }
        // Positive control: the same url under `http` DOES expand.
        let raw = r#"{"mcpServers":{"h":{"type":"http",
            "url":"https://${LX_PROXY_EXPAND_PROBE}/mcp"}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        match &cfgs[0].spec {
            McpTransportSpec::Http { url, .. } => assert_eq!(url, "https://real.example/mcp"),
            other => panic!("{other:?}"),
        }
        // A url that WOULD expand to empty gets no `configError` either.
        let raw = r#"{"mcpServers":{"p":{"type":"claudeai-proxy",
            "url":"${LX_PROXY_EMPTY_PROBE}","id":"c1"}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(
            cfgs[0].config_error, None,
            "`r` stays false for claudeai-proxy, so `Oe=fe` with no configError"
        );
        // Positive control: `http` stamps it.
        let raw = r#"{"mcpServers":{"h":{"type":"http","url":"${LX_PROXY_EMPTY_PROBE}"}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(cfgs[0].config_error.is_some());
        std::env::remove_var("LX_PROXY_EXPAND_PROBE");
        std::env::remove_var("LX_PROXY_EMPTY_PROBE");
    }

    /// Oracle `NAn` @154585377 declares `id: i()` with NO `.optional()` — in
    /// pointed contrast to `displayName`/`iconUrl` and the rest of the
    /// schema's tail — so a `claudeai-proxy` entry with no `id` fails
    /// `safeParse` and must be skipped, the same way a nameless `sdk` entry
    /// is skipped (see `sdk_entry_without_name_is_rejected`).
    #[test]
    fn claudeai_proxy_entry_without_id_is_rejected() {
        let raw = r#"{"mcpServers":{"x":{"type":"claudeai-proxy","url":"https://x.test"}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(
            cfgs.is_empty(),
            "`NAn.id` is required; an idless claudeai-proxy entry must be skipped"
        );
        // `i()` has no `.min(1)`, so an EMPTY id still satisfies the schema.
        let raw =
            r#"{"mcpServers":{"x":{"type":"claudeai-proxy","url":"https://x.test","id":""}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1, "an empty `id` still satisfies `i()`");
    }

    #[test]
    fn missing_command_and_url_is_skipped_not_error() {
        // claude-code `loadMcpServersFromFile` logs+skips an invalid entry
        // (safeParse failure → continue) instead of dropping the file. A lone
        // bad entry therefore yields an empty list, NOT an Err.
        let raw = r#"{"mcpServers":{"bogus":{}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert!(cfgs.is_empty(), "bad entry skipped, no Err");
    }

    /// An empty remote url is KEPT, not dropped.
    ///
    /// Verified by running both binaries over the same `.mcp.json`: the oracle
    /// LISTS a `{"type":"http","url":""}` server (`a[u]=T` retains every entry)
    /// and refuses it at CONNECT as UNCONFIGURED (`zar`). Dropping it here made
    /// the server vanish from `mcp list` and `mcp get` with no explanation, so
    /// a typo or an unset env var looked like the server had never been
    /// configured at all.
    #[test]
    fn blank_remote_url_is_kept_as_unconfigured() {
        // `cLi`/`J5n` declare `url: E.string()` with NO `.min(1)` (contrast
        // stdio's `command: E.string().min(1)`), so a blank url is
        // schema-VALID: `klr` keeps the entry with NO configError, which is
        // exactly `zar`'s shape ⇒ `- Not configured`.
        for raw in [
            r#"{"mcpServers":{"remote":{"type":"http","url":"   "}}}"#,
            r#"{"mcpServers":{"remote":{"type":"http","url":""}}}"#,
        ] {
            let cfgs = parse_mcp_json_string(
                raw,
                ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
            )
            .unwrap();
            assert_eq!(cfgs.len(), 1, "blank remote url must keep the server");
            assert_eq!(cfgs[0].config_error, None, "blank url is not a configError");
            assert!(
                cfgs[0].is_unconfigured(),
                "blank url with no configError is claude `zar`"
            );
        }
    }

    #[test]
    fn url_expanded_to_empty_is_kept_with_config_error() {
        // 2.1.220 `klr`/`ey_` (`urlExpandedToEmpty`): a NON-empty url that
        // expands to an empty string KEEPS the server, tagged with the
        // byte-exact configError. Reason `url_invalid` ⇒ `zar` is false and
        // `Nxe` classifies it INVALID_CONFIG (`✘ Failed to connect` + the
        // configError as the issue), NOT `- Not configured`.
        //
        // NB an UNSET `${VAR}` with no default stays LITERAL (bY returns the
        // match) so it does NOT empty the url; emptiness needs a var that IS
        // set to "" or — deterministic for a test — an empty `:-` default.
        let raw = r#"{"mcpServers":{"r":{"type":"http","url":"${LINGXI_MCP_TEST_UNSET_M5:-}"}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1, "expanded-to-empty url must keep the server");
        assert_eq!(
            cfgs[0].config_error.as_deref(),
            Some(
                "'url' \"${LINGXI_MCP_TEST_UNSET_M5:-}\" expanded to an empty string. \
                 Set the referenced environment variable, or update the server's \
                 config and reconnect."
            )
        );
        // The spec keeps the UNEXPANDED url so list/get display the `${VAR}`
        // reference (the oracle's display view maps back to the
        // `expandVars:false` config for the same effect).
        match &cfgs[0].spec {
            McpTransportSpec::Http { url, .. } => {
                assert_eq!(url, "${LINGXI_MCP_TEST_UNSET_M5:-}");
            }
            other => panic!("expected Http, got {other:?}"),
        }
        assert!(
            !cfgs[0].is_unconfigured(),
            "`configErrorReason:\"url_invalid\"` is INVALID_CONFIG, not UNCONFIGURED"
        );
        // A url that expands to something non-empty carries NO configError.
        let ok = parse_mcp_json_string(
            r#"{"mcpServers":{"r":{"type":"http","url":"${B:-https://x.test}"}}}"#,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(ok[0].config_error, None);
        assert!(!ok[0].is_unconfigured());
    }

    #[test]
    fn bare_map_without_mcpservers_wrapper_parses() {
        // claude-code: `const mcpServers = parsed.mcpServers || parsed` — a
        // top-level map of {name: serverConfig} with NO `mcpServers` wrapper is
        // accepted as the server map directly.
        let raw = r#"{"x":{"command":"foo"}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].name, "x");
        match &cfgs[0].spec {
            McpTransportSpec::Stdio { command, .. } => assert_eq!(command, "foo"),
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn invalid_entry_is_skipped_valid_kept() {
        // One invalid entry (no command/url) and one valid: the valid one
        // survives, the file is NOT dropped (per-entry skip, no Err).
        let raw = r#"{"mcpServers":{"bad":{},"good":{"command":"g"}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].name, "good");
        match &cfgs[0].spec {
            McpTransportSpec::Stdio { command, .. } => assert_eq!(command, "g"),
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn mcpservers_wrapper_takes_precedence_over_sibling_keys() {
        // `parsed.mcpServers || parsed`: when the wrapper exists, ONLY its
        // contents are used — sibling top-level keys are ignored, not merged.
        let raw = r#"{
          "mcpServers": { "wrapped": { "command": "w" } },
          "sibling": { "command": "s" }
        }"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].name, "wrapped");
    }

    #[test]
    fn project_overrides_user_on_name_collision() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join(".mcp.json");
        let global = dir.path().join("user-mcp.json");
        fs::write(&global, r#"{"mcpServers":{"x":{"command":"global-x"}}}"#).unwrap();
        fs::write(&project, r#"{"mcpServers":{"x":{"command":"project-x"}}}"#).unwrap();

        let cfgs = load_mcp_json_with_precedence(&project, &global);
        assert_eq!(cfgs.len(), 1);
        assert_eq!(
            cfgs[0].scope,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project)
        );
        match &cfgs[0].spec {
            McpTransportSpec::Stdio { command, .. } => assert_eq!(command, "project-x"),
            other => panic!("got {other:?}"),
        }
    }

    // ── Batch 5b: ${VAR} env-expansion wired into the parse site ──────────

    #[test]
    fn stdio_fields_expand_default_values() {
        // No env mutation needed: `${VAR:-default}` resolves to the default.
        let raw = r#"{
          "mcpServers": {
            "s": {
              "command": "${BIN:-mcp-memory}",
              "args": ["--port", "${PORT:-8080}"],
              "env": { "TOKEN": "${TOK:-abc}" }
            }
          }
        }"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        match &cfgs[0].spec {
            McpTransportSpec::Stdio { command, args, env } => {
                assert_eq!(command, "mcp-memory");
                assert_eq!(args, &vec!["--port".to_string(), "8080".to_string()]);
                assert_eq!(env.get("TOKEN").map(String::as_str), Some("abc"));
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn url_and_headers_expand_default_values() {
        let raw = r#"{
          "mcpServers": {
            "r": {
              "type": "http",
              "url": "${BASE:-https://example.test}/mcp",
              "headers": { "Authorization": "Bearer ${TOKEN:-xyz}" }
            }
          }
        }"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        match &cfgs[0].spec {
            McpTransportSpec::Http { url, headers, .. } => {
                assert_eq!(url, "https://example.test/mcp");
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer xyz")
                );
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn headers_preserve_config_insertion_order_for_server_key() {
        // Two headers declared Z-then-A (NON-alphabetical). The parsed spec
        // must keep that order so `oauth::server_key` byte-matches claude-code's
        // insertion-order `JSON.stringify` (see oauth::tests). A sorted map
        // would reorder to A,Z and diverge.
        let raw = r#"{
          "mcpServers": {
            "ordered": {
              "url": "https://mcp.example.com/v1",
              "type": "http",
              "headers": { "Z-Header": "z", "A-Header": "a" }
            }
          }
        }"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        match &cfgs[0].spec {
            McpTransportSpec::Http { headers, .. } => {
                let order: Vec<&str> = headers.keys().map(String::as_str).collect();
                assert_eq!(order, vec!["Z-Header", "A-Header"], "insertion order kept");
            }
            other => panic!("expected Http, got {other:?}"),
        }
        // End-to-end: the server key matches the pinned insertion-order
        // reference hash (Node-computed in oauth::tests), NOT the sorted one.
        let key = crate::oauth::server_key("ordered", &cfgs[0].spec);
        assert_eq!(key, "ordered|b555b45e666ffa13");
    }

    #[test]
    fn set_env_var_is_substituted_into_command() {
        // A uniquely-named var (set for this process) is substituted.
        std::env::set_var("LINGXI_MCP_TEST_BIN_5B", "/opt/mcp/bin");
        let raw = r#"{"mcpServers":{"s":{"command":"${LINGXI_MCP_TEST_BIN_5B}"}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        match &cfgs[0].spec {
            McpTransportSpec::Stdio { command, .. } => assert_eq!(command, "/opt/mcp/bin"),
            other => panic!("expected Stdio, got {other:?}"),
        }
        std::env::remove_var("LINGXI_MCP_TEST_BIN_5B");
    }

    #[test]
    fn missing_var_is_left_literal_and_does_not_fail_parse() {
        // An unset `${MISSING}` with no default is left verbatim; parsing still
        // succeeds (TS surfaces a non-fatal error, never aborts the config).
        let raw = r#"{"mcpServers":{"s":{"command":"${LINGXI_MCP_TEST_UNSET_5B}","args":["ok"]}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        match &cfgs[0].spec {
            McpTransportSpec::Stdio { command, args, .. } => {
                assert_eq!(command, "${LINGXI_MCP_TEST_UNSET_5B}");
                assert_eq!(args, &vec!["ok".to_string()]);
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn malformed_user_file_is_skipped_silently() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join(".mcp.json");
        let global = dir.path().join("user-mcp.json");
        fs::write(&global, "{ not json").unwrap();
        fs::write(&project, r#"{"mcpServers":{"y":{"command":"y"}}}"#).unwrap();
        let cfgs = load_mcp_json_with_precedence(&project, &global);
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].name, "y");
    }

    // ── Global config (`~/.lingxi.json`) user-scope MCP, mcpServers-only ──────

    #[test]
    fn global_config_reads_only_mcp_servers_key_not_siblings() {
        // A real `~/.lingxi.json` has many unrelated top-level keys. The global
        // reader must extract ONLY `mcpServers` and never treat e.g. `projects`
        // or `numStartups` as server entries (claude-code `wt().mcpServers`).
        let raw = r#"{
          "numStartups": 7,
          "oauthAccount": { "emailAddress": "x@y.z" },
          "projects": { "/some/proj": { "allowedTools": ["Bash"] } },
          "mcpServers": { "mem": { "command": "mcp-mem" } }
        }"#;
        let cfgs = parse_global_config_mcp_servers(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].name, "mem");
        assert_eq!(
            cfgs[0].scope,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User)
        );
    }

    #[test]
    fn global_config_without_mcp_servers_yields_none_no_bare_map_fallback() {
        // No `mcpServers` key: must yield NO servers. The bare-map `|| parsed`
        // fallback (valid for `.mcp.json`) must NOT apply to the global config —
        // otherwise `numStartups` / `projects` would be mis-parsed as servers.
        let raw = r#"{ "numStartups": 7, "projects": { "/p": {} } }"#;
        let cfgs = parse_global_config_mcp_servers(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(cfgs.is_empty());
    }

    #[test]
    fn precedence_loader_reads_global_config_mcp_servers_key() {
        // End-to-end: the precedence loader pointed at a global-config file reads
        // its `mcpServers`, ignoring sibling keys.
        let dir = TempDir::new().unwrap();
        let project = dir.path().join(".mcp.json"); // absent
        let global = dir.path().join(".lingxi.json");
        fs::write(
            &global,
            r#"{"numStartups":3,"mcpServers":{"g":{"command":"g"}}}"#,
        )
        .unwrap();
        let cfgs = load_mcp_json_with_precedence(&project, &global);
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].name, "g");
        assert_eq!(
            cfgs[0].scope,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User)
        );
    }

    // ── Local-scope MCP (`~/.lingxi.json` projects[<key>].mcpServers) ─────────

    #[test]
    fn local_scope_reads_projects_keyed_mcp_servers() {
        let raw = r#"{"projects":{"/some/proj":{"mcpServers":{"loc":{"command":"loc-cmd"}}}}}"#;
        let cfgs = parse_local_config_mcp_servers(
            raw,
            "/some/proj",
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Local),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].name, "loc");
        assert_eq!(
            cfgs[0].scope,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Local)
        );
    }

    #[test]
    fn local_scope_absent_project_key_yields_none() {
        // Project entry exists for a DIFFERENT dir → no local servers for ours.
        let raw = r#"{"projects":{"/other":{"mcpServers":{"x":{"command":"x"}}}}}"#;
        let cfgs = parse_local_config_mcp_servers(
            raw,
            "/some/proj",
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Local),
        )
        .unwrap();
        assert!(cfgs.is_empty());
    }

    #[test]
    fn three_scope_precedence_local_over_project_over_user() {
        // Same server name "s" in ALL three scopes → LOCAL wins (claude-code
        // priority order ["local","project","user"]).
        let dir = TempDir::new().unwrap();
        let cwd = dir.path();
        let project = cwd.join(".mcp.json");
        let global = cwd.join(".lingxi.json");
        // Key BOTH the fixture and the loader via the same canonical resolver, so
        // they match regardless of temp-dir symlink canonicalization.
        let key = migrations::global_config::project_path_for_config(cwd);
        let mut projects = serde_json::Map::new();
        projects.insert(
            key,
            serde_json::json!({ "mcpServers": { "s": { "command": "local-s" } } }),
        );
        let global_json = serde_json::json!({
            "mcpServers": { "s": { "command": "user-s" } },
            "projects": serde_json::Value::Object(projects),
        });
        fs::write(&global, serde_json::to_string(&global_json).unwrap()).unwrap();
        fs::write(&project, r#"{"mcpServers":{"s":{"command":"project-s"}}}"#).unwrap();

        let cfgs = load_mcp_servers(&project, &global, cwd);
        assert_eq!(cfgs.len(), 1);
        assert_eq!(
            cfgs[0].scope,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Local)
        );
        match &cfgs[0].spec {
            McpTransportSpec::Stdio { command, .. } => assert_eq!(command, "local-s"),
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn three_scope_distinct_names_all_present_with_correct_scopes() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path();
        let project = cwd.join(".mcp.json");
        let global = cwd.join(".lingxi.json");
        let key = migrations::global_config::project_path_for_config(cwd);
        let mut projects = serde_json::Map::new();
        projects.insert(
            key,
            serde_json::json!({ "mcpServers": { "loc": { "command": "loc" } } }),
        );
        let global_json = serde_json::json!({
            "numStartups": 9,
            "mcpServers": { "usr": { "command": "usr" } },
            "projects": serde_json::Value::Object(projects),
        });
        fs::write(&global, serde_json::to_string(&global_json).unwrap()).unwrap();
        fs::write(&project, r#"{"mcpServers":{"prj":{"command":"prj"}}}"#).unwrap();

        let cfgs = load_mcp_servers(&project, &global, cwd);
        assert_eq!(cfgs.len(), 3);
        let by_name: std::collections::HashMap<&str, ConfigScope> =
            cfgs.iter().map(|c| (c.name.as_str(), c.scope)).collect();
        assert_eq!(
            by_name["loc"],
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Local)
        );
        assert_eq!(
            by_name["prj"],
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project)
        );
        assert_eq!(
            by_name["usr"],
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User)
        );
    }

    #[test]
    fn pending_project_server_does_not_shadow_user_server() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path();
        let project = cwd.join(".mcp.json");
        let global = cwd.join(".lingxi.json");
        fs::write(
            &global,
            r#"{"mcpServers":{"docs":{"command":"user-docs"}}}"#,
        )
        .unwrap();
        fs::write(
            &project,
            r#"{"mcpServers":{"docs":{"command":"project-docs"}}}"#,
        )
        .unwrap();

        let cfgs = load_mcp_servers(&project, &global, cwd);
        assert_eq!(cfgs.len(), 1);
        assert_eq!(
            cfgs[0].scope,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User)
        );
        assert!(matches!(
            &cfgs[0].spec,
            McpTransportSpec::Stdio { command, .. } if command == "user-docs"
        ));
    }

    #[test]
    fn a_refused_project_server_is_disabled_even_when_all_are_enabled() {
        // The wiring half of the unresolved-env-ref refusal. `load_mcp_servers`
        // decides whether a blocked entry is disabled by MATCHING on the block
        // reason, so a new reason that the match does not list is inert: the
        // server would be inserted live, and could even shadow a user server of
        // the same name. Nothing else in this crate covers that match --
        // removing the arm leaves the whole 688-test suite green -- so this
        // test exists specifically to pin it.
        let dir = TempDir::new().unwrap();
        let cwd = dir.path();
        let project = cwd.join(".mcp.json");
        let global = cwd.join(".lingxi.json");
        let key = migrations::global_config::project_path_for_config(cwd);
        // enableAllProjectMcpServers: even a blanket approval must not admit it.
        let global_json = serde_json::json!({
            "projects": { key: { "enableAllProjectMcpServers": true } }
        });
        fs::write(&global, serde_json::to_vec(&global_json).unwrap()).unwrap();
        fs::write(
            &project,
            r#"{"mcpServers":{"leaky":{"command":"${LINGXI_TEST_NEVER_SET_XYZ}/srv"},"fine":{"command":"/bin/srv"}}}"#,
        )
        .unwrap();

        let cfgs = load_mcp_servers(&project, &global, cwd);
        let leaky = cfgs
            .iter()
            .find(|c| c.name == "leaky")
            .expect("the refused server stays VISIBLE, like pending/rejected");
        assert!(
            leaky.disabled,
            "a project entry whose command still carries ${{...}} after expansion \
             must not be connectable"
        );
        let fine = cfgs.iter().find(|c| c.name == "fine").expect("sibling");
        assert!(
            !fine.disabled,
            "the refusal is per-entry; a clean sibling still loads"
        );
    }

    #[test]
    fn approved_project_server_can_shadow_user_server() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path();
        let project = cwd.join(".mcp.json");
        let global = cwd.join(".lingxi.json");
        let key = migrations::global_config::project_path_for_config(cwd);
        let global_json = serde_json::json!({
            "mcpServers": { "docs": { "command": "user-docs" } },
            "projects": {
                key: { "enabledMcpjsonServers": ["docs"] }
            }
        });
        fs::write(&global, serde_json::to_vec(&global_json).unwrap()).unwrap();
        fs::write(
            &project,
            r#"{"mcpServers":{"docs":{"command":"project-docs"}}}"#,
        )
        .unwrap();

        let cfgs = load_mcp_servers(&project, &global, cwd);
        assert_eq!(cfgs.len(), 1);
        assert_eq!(
            cfgs[0].scope,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project)
        );
        assert!(matches!(
            &cfgs[0].spec,
            McpTransportSpec::Stdio { command, .. } if command == "project-docs"
        ));
    }

    // ── Per-server `timeout` / `request_timeout_ms` / `alwaysLoad` (parity
    //    2.1.207 P2-01): zod schemas `timeout: RKe().optional()` (all
    //    transports), `request_timeout_ms: nil()` (sse/http only, folded by
    //    `RAn`), `alwaysLoad: E.boolean().optional()`. ─────────────────────

    #[test]
    fn stdio_timeout_is_parsed() {
        let raw = r#"{"mcpServers":{"s":{"command":"c","timeout":5000}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(cfgs[0].timeout_ms, Some(5000));
        assert!(!cfgs[0].always_load);
    }

    #[test]
    fn http_request_timeout_ms_folds_into_timeout_capped_at_300_000() {
        // RAn: `timeout` unset + `request_timeout_ms` set → timeout =
        // min(request_timeout_ms, 300_000). A huge alias is capped.
        let raw = r#"{"mcpServers":{"r":{"type":"http","url":"https://x.test","request_timeout_ms":999999999}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(cfgs[0].timeout_ms, Some(300_000), "capped at LTm=300_000");

        // Under the cap it is folded verbatim.
        let raw = r#"{"mcpServers":{"r":{"type":"http","url":"https://x.test","request_timeout_ms":45000}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(cfgs[0].timeout_ms, Some(45000));
    }

    #[test]
    fn http_timeout_wins_over_request_timeout_ms() {
        // RAn only folds when `timeout` is UNSET; an explicit `timeout` wins and
        // is NOT capped at 300_000.
        let raw = r#"{"mcpServers":{"r":{"type":"http","url":"https://x.test","timeout":600000,"request_timeout_ms":10}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(cfgs[0].timeout_ms, Some(600000));
    }

    #[test]
    fn stdio_ignores_request_timeout_ms() {
        // The stdio zod schema carries no `request_timeout_ms` field (zod strips
        // it); the alias must NOT be folded for a stdio transport.
        let raw = r#"{"mcpServers":{"s":{"command":"c","request_timeout_ms":45000}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(cfgs[0].timeout_ms, None);
    }

    #[test]
    fn always_load_is_parsed() {
        let raw = r#"{"mcpServers":{"s":{"command":"c","alwaysLoad":true}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert!(cfgs[0].always_load);
    }

    #[test]
    fn tool_permission_policy_uses_oracle_snake_case_keys() {
        let raw = r#"{
          "mcpServers": {
            "s": {
              "command": "c",
              "tools": [{
                "name": "write_file",
                "permission_policy": "deny",
                "org_max_permission": "ask"
              }],
              "toolPermissions": {"write_file": "blocked"}
            }
          }
        }"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].tools.len(), 1);
        assert_eq!(
            cfgs[0].tools[0].permission_policy,
            Some(McpToolPermissionPolicy::AlwaysDeny)
        );
        assert_eq!(
            cfgs[0].tools[0].org_max_permission,
            Some(McpPermissionCeiling::Ask)
        );
        let serialized = serde_json::to_value(&cfgs[0]).unwrap();
        let tool = &serialized["tools"][0];
        assert_eq!(tool["permission_policy"], "always_deny");
        assert_eq!(tool["org_max_permission"], "ask");
        assert!(tool.get("permissionPolicy").is_none());
        assert!(tool.get("orgMaxPermission").is_none());
    }

    #[test]
    fn legacy_camel_case_tool_policy_aliases_remain_readable() {
        let raw = r#"{
          "mcpServers": {
            "s": {
              "command": "c",
              "tools": [{
                "name": "write_file",
                "permissionPolicy": "allow",
                "orgMaxPermission": "blocked"
              }]
            }
          }
        }"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(
            cfgs[0].tools[0].permission_policy,
            Some(McpToolPermissionPolicy::AlwaysAllow)
        );
        assert_eq!(
            cfgs[0].tools[0].org_max_permission,
            Some(McpPermissionCeiling::Deny)
        );
    }

    #[test]
    fn invalid_request_timeout_ms_is_caught_not_fatal() {
        // `nil()` has `.catch(void 0)`: a bad value becomes undefined rather than
        // failing the entry. A string alias must NOT skip the server; it is just
        // ignored (no fold).
        let raw = r#"{"mcpServers":{"r":{"type":"http","url":"https://x.test","request_timeout_ms":"nope"}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(
            cfgs.len(),
            1,
            "invalid request_timeout_ms must not drop the server"
        );
        assert_eq!(cfgs[0].timeout_ms, None);

        // Non-positive alias is also coerced away (.positive()).
        let raw =
            r#"{"mcpServers":{"r":{"type":"http","url":"https://x.test","request_timeout_ms":0}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(cfgs[0].timeout_ms, None);
    }

    #[test]
    fn invalid_timeout_type_skips_entry() {
        // `timeout: RKe()` has NO `.catch`, so a non-integer value fails the
        // entry's safeParse → the whole server is skipped (valid siblings kept).
        let raw =
            r#"{"mcpServers":{"bad":{"command":"c","timeout":"soon"},"good":{"command":"g"}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].name, "good");
    }

    #[test]
    fn zero_timeout_skips_entry() {
        // `timeout: RKe()` = `number().int().positive()` with NO `.catch`, so a
        // present non-positive `0` fails the entry's safeParse → the whole
        // server is skipped (valid siblings kept). `0` is a valid `u64`, so
        // serde alone accepts it; the positive-integer guard is what drops the
        // entry, matching CC's `.positive()` (review RV10).
        let raw = r#"{"mcpServers":{"bad":{"command":"c","timeout":0},"good":{"command":"g"}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].name, "good");

        // Same for a remote entry with a non-positive `timeout` (the direct
        // `timeout` field, distinct from the `.catch`-lenient
        // `request_timeout_ms` alias). `type` must be explicit here — without
        // it this would already be rejected as a type-less (implicit-stdio)
        // entry for an unrelated reason, which would pin nothing about
        // `timeout:0` specifically.
        let raw = r#"{"mcpServers":{"r":{"type":"http","url":"https://x.test","timeout":0}}}"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert!(cfgs.is_empty(), "timeout:0 must drop the remote server");
    }

    #[test]
    fn new_fields_do_not_change_server_key_hash() {
        // The per-server timeout/alwaysLoad live on McpServerConfig, NOT on the
        // McpTransportSpec, so `oauth::server_key` (which hashes only the spec)
        // is identical with or without them.
        let bare = r#"{"mcpServers":{"ordered":{"url":"https://mcp.example.com/v1","type":"http","headers":{"Z-Header":"z","A-Header":"a"}}}}"#;
        let with = r#"{"mcpServers":{"ordered":{"url":"https://mcp.example.com/v1","type":"http","headers":{"Z-Header":"z","A-Header":"a"},"timeout":12345,"alwaysLoad":true}}}"#;
        let a = parse_mcp_json_string(
            bare,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        let b = parse_mcp_json_string(
            with,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(
            crate::oauth::server_key("ordered", &a[0].spec),
            crate::oauth::server_key("ordered", &b[0].spec),
            "timeout/alwaysLoad must not perturb the server-key config hash",
        );
        // ...and the fields were actually captured on the config.
        assert_eq!(b[0].timeout_ms, Some(12345));
        assert!(b[0].always_load);
    }

    // ── §23b: the `Iqe` shape/size guard wired into the loaders ───────────

    /// A valid-JSON `.mcp.json` (one "big" server) padded with trailing
    /// whitespace past `MCP_CONFIG_MAX_BYTES`. `serde_json` accepts trailing
    /// whitespace, so this is ONLY rejected by the shape/size guard — if that
    /// guard were removed, both loaders below would happily read and parse
    /// it, and "big" would appear in the result.
    fn oversized_but_well_formed_mcp_json() -> Vec<u8> {
        let mut body = br#"{"mcpServers":{"big":{"command":"c"}}}"#.to_vec();
        let target = crate::config_diagnostics::MCP_CONFIG_MAX_BYTES as usize + 1;
        body.resize(target, b' ');
        body
    }

    #[test]
    fn precedence_loader_skips_an_oversized_project_mcp_json() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join(".mcp.json");
        let global = dir.path().join("user-mcp.json"); // absent
        fs::write(&project, oversized_but_well_formed_mcp_json()).unwrap();

        let cfgs = load_mcp_json_with_precedence(&project, &global);
        assert!(
            cfgs.is_empty(),
            "an over-cap .mcp.json must be rejected by the shape/size guard, not parsed"
        );
    }

    #[test]
    fn load_mcp_servers_skips_an_oversized_project_mcp_json() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join(".mcp.json");
        let global = dir.path().join(".lingxi.json"); // absent
        fs::write(&project, oversized_but_well_formed_mcp_json()).unwrap();

        let cfgs = load_mcp_servers(&project, &global, dir.path());
        assert!(
            cfgs.is_empty(),
            "an over-cap .mcp.json must be rejected by the shape/size guard, not parsed"
        );
    }

    // ── §11 — discoveryCache / role recognition ────────────────────────────

    #[test]
    fn discovery_cache_is_a_schema_key_only_for_sse_and_http_family() {
        for ty in ["sse", "http", "streamable-http"] {
            assert!(
                discovery_cache_is_schema_key_for(Some(ty)),
                "{ty} should declare discoveryCache"
            );
        }
        for ty in [
            None,
            Some("stdio"),
            Some("ws"),
            Some("sdk"),
            Some("claudeai-proxy"),
            Some("sse-ide"),
            Some("ws-ide"),
        ] {
            assert!(
                !discovery_cache_is_schema_key_for(ty),
                "{ty:?} should NOT declare discoveryCache"
            );
        }
    }

    #[test]
    fn discovery_cache_flag_reads_a_boolean_on_a_declaring_type() {
        let raw = serde_json::json!({"type":"http","url":"https://x","discoveryCache":false});
        assert_eq!(discovery_cache_flag(Some("http"), &raw), Ok(Some(false)));
        let raw = serde_json::json!({"type":"sse","url":"https://x","discoveryCache":true});
        assert_eq!(discovery_cache_flag(Some("sse"), &raw), Ok(Some(true)));
        let raw = serde_json::json!({"type":"http","url":"https://x"});
        assert_eq!(discovery_cache_flag(Some("http"), &raw), Ok(None));
    }

    #[test]
    fn discovery_cache_flag_rejects_a_non_boolean_on_a_declaring_type() {
        let raw = serde_json::json!({"type":"http","url":"https://x","discoveryCache":"nope"});
        assert_eq!(
            discovery_cache_flag(Some("http"), &raw),
            Err(InvalidDiscoveryCacheFlag)
        );
    }

    #[test]
    fn discovery_cache_flag_ignores_any_value_on_a_non_declaring_type() {
        // A bad-typed discoveryCache on stdio is a schema-unknown key: zod
        // never looks at it, so it must NOT reject the entry.
        let raw = serde_json::json!({"command":"c","discoveryCache":"nope"});
        assert_eq!(discovery_cache_flag(None, &raw), Ok(None));
        assert_eq!(discovery_cache_flag(Some("stdio"), &raw), Ok(None));
    }

    /// End-to-end: a `discoveryCache` of the wrong TYPE on an sse/http entry
    /// must drop the WHOLE server, keeping any valid sibling.
    #[test]
    fn build_entry_rejects_a_non_boolean_discovery_cache_on_http() {
        let raw = r#"{
          "mcpServers": {
            "bad":  { "type": "http", "url": "https://x.example", "discoveryCache": "nope" },
            "good": { "type": "http", "url": "https://y.example" }
          }
        }"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(
            cfgs.len(),
            1,
            "the malformed discoveryCache entry must be dropped alone"
        );
        assert_eq!(cfgs[0].name, "good");
    }

    /// The SAME bad-typed `discoveryCache` value on `stdio` is schema-unknown
    /// there and must NOT reject the entry.
    #[test]
    fn build_entry_ignores_discovery_cache_on_stdio() {
        let raw = r#"{
          "mcpServers": {
            "srv": { "command": "c", "discoveryCache": "nope" }
          }
        }"#;
        let cfgs = parse_mcp_json_string(
            raw,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].name, "srv");
        assert_eq!(cfgs[0].discovery_cache, None);
    }

    #[test]
    fn build_entry_threads_discovery_cache_true_false_and_absent() {
        let false_cfg = build_server_from_json_entry(
            "falsey",
            &serde_json::json!({
                "type": "http",
                "url": "https://false.example",
                "discoveryCache": false
            }),
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .expect("http entry with false discoveryCache loads");
        assert_eq!(false_cfg.discovery_cache, Some(false));

        let true_cfg = build_server_from_json_entry(
            "truthy",
            &serde_json::json!({
                "type": "sse",
                "url": "https://true.example",
                "discoveryCache": true
            }),
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .expect("sse entry with true discoveryCache loads");
        assert_eq!(true_cfg.discovery_cache, Some(true));

        let absent_cfg = build_server_from_json_entry(
            "absent",
            &serde_json::json!({
                "type": "http",
                "url": "https://absent.example"
            }),
            ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .expect("http entry without discoveryCache loads");
        assert_eq!(absent_cfg.discovery_cache, None);
    }

    #[test]
    fn role_is_a_schema_key_for_every_transport_but_sdk_and_claudeai_proxy() {
        for ty in [
            None,
            Some("stdio"),
            Some("sse"),
            Some("http"),
            Some("streamable-http"),
            Some("ws"),
            Some("sse-ide"),
            Some("ws-ide"),
        ] {
            assert!(role_is_schema_key_for(ty), "{ty:?} should declare role");
        }
        // `sdk`/`claudeai-proxy` omit the key; a `type` the union has no arm
        // for declares no schema at all, so it declares no `role` either.
        for ty in [
            Some("sdk"),
            Some("claudeai-proxy"),
            Some("bogus"),
            Some("websocket"),
            Some(""),
        ] {
            assert!(
                !role_is_schema_key_for(ty),
                "{ty:?} should NOT declare role"
            );
        }
    }

    #[test]
    fn role_flag_accepts_only_the_literal_comms_and_catches_everything_else() {
        let comms = serde_json::json!({"role":"comms"});
        assert_eq!(role_flag(Some("stdio"), &comms), Some("comms".to_string()));

        for bad in [
            serde_json::json!({"role":"other"}),
            serde_json::json!({"role":123}),
            serde_json::json!({"role":null}),
            serde_json::json!({}),
        ] {
            assert_eq!(
                role_flag(Some("stdio"), &bad),
                None,
                "an invalid role must be silently caught to None, never reject the entry"
            );
        }
    }

    #[test]
    fn role_flag_is_none_for_sdk_and_claudeai_proxy_even_when_present() {
        let raw = serde_json::json!({"role":"comms"});
        assert_eq!(role_flag(Some("sdk"), &raw), None);
        assert_eq!(role_flag(Some("claudeai-proxy"), &raw), None);
    }
}
