//! §11 — the MCP **discovery cache**: a persisted, per-server record of the
//! last successful `initialize`/`tools/list`/`prompts/list`/`resources/list`
//! round so a reconnect can skip re-discovery when nothing has changed.
//!
//! This is a **greenfield port** — the oracle's on-disk shape (an encrypted-
//! at-rest-adjacent JSON blob keyed by an OAuth-grant-token-bound fingerprint,
//! written through a pluggable KV storage backend with symlink/oversize
//! refusal) is not wire-visible to any other client, so nothing here needs to
//! byte-match it. What *is* ported, because it IS externally observable
//! (config keys a user can write, log lines, telemetry bucket names), is:
//!
//! * the two new config keys, `discoveryCache` (sse/http only) and `role`
//!   (see [`crate::json_config::discovery_cache_flag`] /
//!   [`crate::json_config::role_flag`]); both are threaded through
//!   [`crate::connection::McpServerConfig`]. `discoveryCache` drives runtime
//!   eligibility, while `role` participates in logical identity and MCP tool
//!   routing;
//! * the miss-reason vocabulary and the fresh/stale/miss decision oracle
//!   `cot` implements (2.1.251 Mach-O @176266197), recovered byte-exact from
//!   the binary:
//!   ```text
//!   async function cot(e,t,r=Date.now()){
//!     let o=me(t);
//!     if(o!==void 0&&!Ie(o))return{kind:"miss",reason:o==="transport"?"transport":"disabled"};
//!     if(o!==void 0){ /* purge this server's cache-key family */ return{kind:"miss",reason:"disabled"} }
//!     let s; try{s=await we(e,t)}catch{return{kind:"miss",reason:"no-fingerprint"}}
//!     if(s===void 0)return{kind:"miss",reason:"no-fingerprint"};
//!     let d; try{d=await Q(s,_())}catch(S){ if(refused-entry) return corrupt; return{kind:"miss",reason:"absent"} }
//!     if(d===void 0)return{kind:"miss",reason:"absent"};
//!     let w=schema.safeParse(...); if(!w.success) return corrupt;
//!     let k=w.data;
//!     if(k.cacheKey!==A(e,t)) return corrupt; // keyed for another server
//!     if(k.consecutiveRefreshFailures>=pe()) return{kind:"miss",reason:"strike-threshold"};
//!     let R=ye(); if(oe(k)-r>R) return{kind:"miss",reason:"expired"};
//!     let u=max(0,r-k.savedAt); if(u>=R) return{kind:"miss",reason:"expired"};
//!     if(u<Oe() && !(k.capabilities.tools&&k.tools.length===0)) return{kind:"fresh",entry:k,ageMs:u};
//!     return{kind:"stale",entry:k,ageMs:u}
//!   }
//!   ```
//!   `me(t)` (@176260900) is the eligibility gate, in this exact order:
//!   the env/kill-switch feature gate (`Sln`, @161529583) FIRST, then
//!   `identity-changed`, then a `type!=="http"&&type!=="sse"` transport gate
//!   (`"transport"`), then `cli-owned`/`env-placeholder`/`ambient-credential`
//!   guards, then a `W`-table walk that yields `"opt-out"` (config
//!   `discoveryCache:false`) or `"headers-helper"` (a configured
//!   `headersHelper`). `Ie(o)` (@176260899: `W.some(t=>t.reason===o)`) is
//!   true ONLY for `"opt-out"`/`"headers-helper"` — exactly the two reasons
//!   that ALSO purge the on-disk entry family before reporting the miss;
//!   every other disable reason (including the env/kill-switch gate) reports
//!   the miss WITHOUT a purge. [`CacheGateReason::purges_existing_entry`]
//!   mirrors that split.
//!
//! Recovered numeric defaults (2.1.251 @176259300, `Pe/Me/Ae/xe`):
//! TTL 900s (`MCP_DISCOVERY_CACHE_TTL_S`), max-stale 14 400s
//! (`MCP_DISCOVERY_CACHE_MAX_STALE_S`), a 604 800s (7-day) hard ceiling on
//! max-stale, and a strike threshold of 1 (`MCP_DISCOVERY_CACHE_STRIKES`) —
//! i.e. by default a SINGLE recorded refresh failure already misses the
//! cache. `MCP_DISCOVERY_CACHE` itself defaults OFF (`Sln`'s `"not-enabled"`
//! arm) — the feature is opt-IN.
//!
//! Provenance and post-hit capability guards are wired in this module. The
//! provenance guards (`cli-owned`, unresolved environment placeholders, and
//! explicit MCP-only ambient credentials) run before the two user-controlled
//! purging guards (`opt-out` and `headers-helper`). Skills takes precedence
//! over channel, and skills uses the independent `tengu_mcp_skills` feature
//! flag rather than the discovery-cache flag. A live-connection short circuit
//! remains in the registry before this cache is consulted.
//!
//!
//! ## What §11 Stage 1 wires in (this revision)
//!
//! `mcp::registry::McpRegistry` gained an optional
//! `Arc<DiscoveryCacheStore>` (set via `with_discovery_cache_store`; `None`
//! by default, so every existing caller is unaffected). When set:
//!
//! * **Write.** After a successful LIVE discovery round, `connect` persists
//!   the full catalog (tools/resources/resource_templates/prompts +
//!   capabilities) for a cache-ELIGIBLE server ([`cache_gate`] returning
//!   `None`) via [`DiscoveryCacheEntry::new`] and a grant-partitioned
//!   [`DiscoveryCacheStore`], resetting `consecutive_refresh_failures` to 0
//!   (oracle `Wo`/`Mt`). The partition is captured after authenticated
//!   transport setup and checked again before write, so a concurrent MCP
//!   refresh-grant rotation cannot cross-write catalogs. A gate reason that
//!   [`CacheGateReason::purges_existing_entry`] flags (`HeadersHelper` or
//!   `OptOut`) instead purges the server's entire on-disk family, best-effort.
//! * **Strikes.** Only a failed `Stale` background revalidation increments
//!   `consecutive_refresh_failures`, matching oracle `_6e`. The lazy-upgrade
//!   slot retains the exact partition that served the stale hit, so a grant
//!   rotation cannot move the strike to the new partition. Ordinary initial
//!   connection failures never strike cached data.
//! * **Telemetry (MISS side).** Before every dial, [`crate::registry`] calls
//!   [`decide`] and reports [`MissReason`]s that oracle `Ko` (2.1.251, same
//!   chunk as `cot`) surfaces (`absent`/`expired`/`corrupt`/
//!   `strike-threshold`/`no-fingerprint`) as `tengu_mcp_discovery_source`
//!   with `source` = [`miss_telemetry_value`] — see
//!   [`miss_emits_discovery_source_telemetry`] for the exact set and the
//!   recovered `Ko`/`Jo` source. The HIT side (`Fresh`/`Stale`) is emitted by
//!   a separate code path — see "What §11 Stage 2 wires in" below.
//!
//! ## What §11 Stage 2 wires in (this revision)
//!
//! **Serving from cache.** `mcp::connection::McpConnectionState` gained a
//! `Cached` variant carrying the entry's full catalog plus a freshly
//! allocated [`platform_api::McpTransportSpec`]-agnostic connection id with NO live
//! transport behind it. `McpRegistry::connect_locked_inner` consults
//! [`decide`] BEFORE dialing (unless the call is itself the lazy-dial
//! upgrade of an already-`Cached` entry — see below): on `Fresh`/`Stale` it
//! installs `Cached` and returns WITHOUT ever calling
//! `McpTransport::connect`, emitting `tengu_mcp_discovery_source` with
//! `source` `"cache_fresh"`/`"cache_stale"` and the real `entryAgeMs`
//! (oracle @182536408's hit branch) — the previously-deferred half of the
//! telemetry story above. An already-`Connected` server still short-circuits
//! to a live reuse before the cache is ever consulted (the oracle's
//! `$o`/`"live-connection"` check).
//!
//! **Lazy dial.** A `Cached` server has no registered [`crate::client::McpClient`]
//! (`clients` is a map separate from `connections`, and a cache hit never
//! populates it), so `McpRegistry::has_callable_server` still reports it
//! callable, and the FIRST tool dispatch against it
//! (`McpRegistry::call_tool_with_auth_retry`) upgrades it to a real
//! `Connected` by running the ordinary connect path — a lazily-dialed cached
//! server IS a fresh connection, since the transport was simply never opened
//! yet. Single-flighted through the SAME per-server lifecycle lock every
//! other connect path already uses, so two concurrent tool calls against one
//! cached server dial exactly once.
//!
//! Every match site that projects `McpConnectionState` was audited for a
//! `Cached` arm; see `mcp::registry`'s and `tool_mcp::mcp_tool`'s doc
//! comments at each call site for which got one and why (most notably
//! `build_registered_mcp_tools`, `servers_with_tools`, and
//! `has_callable_server` — a cached server MUST appear in tool listings and
//! report callable, or the cache would hide servers instead of accelerating
//! them).
//!
//! ## What §11 Stage 3 wires in (this revision)
//!
//! `mcp::registry::McpRegistry` now completes the stale-while-revalidate
//! path for `Decision::Stale`: serving the cached catalog immediately,
//! kicking off a single-flight background live discovery, atomically
//! upgrading `Cached` to `Connected` only if the cached generation and config
//! snapshot still match, and recording strikes only against the still-current
//! cached entry on a refresh failure.
//!
//! The same registry pass also generalized the lazy-dial seam from generic
//! `mcp__<server>__<tool>` dispatch to the cached resources/prompts surfaces
//! and to lag-recovery catalog reconciliation: cached servers now contribute
//! to `catalog_refresh_snapshot()` as active SHARED generations, while lagged
//! listeners rebuild the entire shared MCP partition set from those current
//! generations before applying best-effort catalog refreshes.
//!
//! The desktop composition root supplies a persistent store under
//! `<lingxi_home>/mcp-discovery-cache`. Mobile currently exposes only
//! `InProcess` MCP transports, which are cache-ineligible, so it deliberately
//! leaves the optional store unwired.

use platform_api::{
    McpPromptDto, McpResourceDto, McpResourceTemplateDto, McpToolDto, McpTransportSpec,
    ServerCapabilitiesDto,
};
use sha2::{Digest, Sha256};

/// Fixed byte-level compatibility domain used when reproducing Claude Code's
/// discovery-cache fingerprint. This is not LingXi login state and must never
/// be replaced with an LLM-provider credential, profile id, or account UUID.
/// The only variable authentication input is the remote MCP server's grant.
pub const PROVIDER_NEUTRAL_IDENTITY_DOMAIN: &str = "acct:logged-out";

fn sha256_hex(input: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input);
    format!("{:x}", hasher.finalize())
}

#[must_use]
pub(crate) fn fingerprint(grant_token: &str) -> String {
    sha256_hex(format!("{PROVIDER_NEUTRAL_IDENTITY_DOMAIN}\0{grant_token}").as_bytes())
}

#[must_use]
pub(crate) fn partition_key_for_era(
    logical_cache_key: &str,
    fingerprint: &str,
    era: &str,
) -> String {
    let material = format!(
        "{logical_cache_key}\0{fingerprint}\0era:{era}\0{}",
        platform_api::CLAUDE_CODE_VERSION
    );
    sha256_hex(material.as_bytes())[..32].to_string()
}

/// Backward-compatible legacy partition vector.  Callers that have not
/// negotiated a protocol continue to produce the original bytes.
pub(crate) fn partition_key(logical_cache_key: &str, fingerprint: &str) -> String {
    partition_key_for_era(logical_cache_key, fingerprint, "legacy")
}

fn canonicalize_logical_key_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            let mut canonical = serde_json::Map::with_capacity(entries.len());
            for (key, value) in entries {
                canonical.insert(key, canonicalize_logical_key_value(value));
            }
            serde_json::Value::Object(canonical)
        }
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .into_iter()
                .map(canonicalize_logical_key_value)
                .collect(),
        ),
        other => other,
    }
}

fn oauth_logical_key_config(oauth: &platform_api::McpOAuthConfigDto) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if let Some(client_id) = &oauth.client_id {
        map.insert("clientId".into(), client_id.clone().into());
    }
    if let Some(callback_port) = oauth.callback_port {
        map.insert("callbackPort".into(), callback_port.into());
    }
    if let Some(metadata_url) = &oauth.auth_server_metadata_url {
        map.insert("authServerMetadataUrl".into(), metadata_url.clone().into());
    }
    if let Some(scopes) = &oauth.scopes {
        map.insert("scopes".into(), scopes.clone().into());
    }
    if let Some(xaa) = oauth.xaa {
        map.insert("xaa".into(), xaa.into());
    }
    serde_json::Value::Object(map)
}

/// Canonicalize a server URL for the reconnect-identity key: sort the query
/// parameters.
///
/// 2.1.269 — "Fixed MCP servers reconnecting when an updated config only
/// changed the order of the server URL's query parameters." The key is built
/// from the config, and the URL rode it as an OPAQUE STRING, so `?b=2&a=1` and
/// `?a=1&b=2` hashed differently and the server was torn down and reconnected
/// for a config that had not actually changed.
///
/// Only the query ORDER is normalized. Everything else — scheme, host, port,
/// path, fragment, duplicate keys and their relative order, percent-encoding —
/// is preserved, because any of those genuinely identifies a different endpoint.
/// A URL that does not parse is returned untouched rather than guessed at.
fn canonical_server_url(url: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    if parsed.query().is_none() {
        return url.to_string();
    }
    let mut pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    // Stable sort: two entries with the SAME key keep their original order, so
    // a repeated parameter whose order the server cares about is unchanged.
    pairs.sort_by(|(left, _), (right, _)| left.cmp(right));
    {
        let mut serializer = parsed.query_pairs_mut();
        serializer.clear();
        for (key, value) in &pairs {
            serializer.append_pair(key, value);
        }
    }
    parsed.to_string()
}

fn spec_logical_key_config(spec: &McpTransportSpec) -> serde_json::Value {
    let mut value = match spec {
        McpTransportSpec::Stdio { command, args, env } => serde_json::json!({
            "type": "stdio",
            "command": command,
            "args": args,
            "env": env,
        }),
        McpTransportSpec::Sse { url, headers, .. } => serde_json::json!({
            "type": "sse",
            "url": canonical_server_url(url),
            "headers": headers,
        }),
        McpTransportSpec::Http { url, headers, .. } => serde_json::json!({
            "type": "http",
            "url": canonical_server_url(url),
            "headers": headers,
        }),
        McpTransportSpec::WebSocket { url, headers, .. } => serde_json::json!({
            "type": "ws",
            "url": canonical_server_url(url),
            "headers": headers,
        }),
        McpTransportSpec::InProcess { registry_key } => serde_json::json!({
            "type": "inProcess",
            "registryKey": registry_key,
        }),
        McpTransportSpec::SseIde { url, ide_name, .. } => serde_json::json!({
            "type": "sse-ide",
            "url": canonical_server_url(url),
            "ideName": ide_name,
        }),
        McpTransportSpec::WsIde { url, ide_name, .. } => serde_json::json!({
            "type": "ws-ide",
            "url": canonical_server_url(url),
            "ideName": ide_name,
        }),
        McpTransportSpec::SdkControl { control_channel_id } => serde_json::json!({
            "type": "sdk",
            "name": control_channel_id,
        }),
    };
    let serde_json::Value::Object(map) = &mut value else {
        unreachable!("all MCP logical-key configs are JSON objects")
    };
    match spec {
        McpTransportSpec::Sse {
            url,
            headers_helper,
            oauth,
            ..
        }
        | McpTransportSpec::Http {
            url,
            headers_helper,
            oauth,
            ..
        } => {
            if let Some(helper) = headers_helper {
                map.insert("headersHelper".into(), helper.clone().into());
            }
            if let Some(oauth) = oauth {
                map.insert("oauth".into(), oauth_logical_key_config(oauth));
            }
            if url.trim().is_empty() {
                map.insert("unconfigured".into(), true.into());
            }
        }
        McpTransportSpec::WebSocket {
            url,
            headers_helper,
            ..
        } => {
            if let Some(helper) = headers_helper {
                map.insert("headersHelper".into(), helper.clone().into());
            }
            if url.trim().is_empty() {
                map.insert("unconfigured".into(), true.into());
            }
        }
        McpTransportSpec::SseIde {
            url,
            ide_running_in_windows,
            ..
        } => {
            if *ide_running_in_windows {
                map.insert("ideRunningInWindows".into(), true.into());
            }
            if url.trim().is_empty() {
                map.insert("unconfigured".into(), true.into());
            }
        }
        McpTransportSpec::WsIde {
            url,
            auth_token,
            ide_running_in_windows,
            ..
        } => {
            if let Some(token) = auth_token {
                map.insert("authToken".into(), token.clone().into());
            }
            if *ide_running_in_windows {
                map.insert("ideRunningInWindows".into(), true.into());
            }
            if url.trim().is_empty() {
                map.insert("unconfigured".into(), true.into());
            }
        }
        McpTransportSpec::Stdio { .. }
        | McpTransportSpec::InProcess { .. }
        | McpTransportSpec::SdkControl { .. } => {}
    }
    value
}

/// Provider-facing logical discovery-cache key. This follows the recovered
/// oracle shape more closely than the legacy transport hash by hashing a
/// canonicalized config object (closest Rust equivalent of the source config,
/// with unavailable source-only fields omitted) and prefixing it with the
/// server name.
#[must_use]
pub(crate) fn logical_cache_key(config: &crate::connection::McpServerConfig) -> String {
    let mut raw = spec_logical_key_config(&config.spec);
    let serde_json::Value::Object(map) = &mut raw else {
        unreachable!("all MCP logical-key configs are JSON objects")
    };
    // The parsed Rust model collapses absent and explicit false for these two
    // booleans. Omitting false is the closest oracle representation and keeps
    // the distinction honest rather than inventing an explicit source value.
    if config.disabled {
        map.insert("disabled".into(), true.into());
    }
    if config.always_load {
        map.insert("alwaysLoad".into(), true.into());
    }
    if let Some(timeout) = config.timeout_ms {
        map.insert("timeout".into(), timeout.into());
    }
    // Metadata is absent for ordinary servers, preserving their established
    // fixed vectors.  These fields are only identity-bearing when explicitly
    // supplied by the MCP config/agent host.
    if let Some(transport) = &config.metadata.transport {
        map.insert("transport".into(), transport.clone().into());
    }
    if config.metadata.role.is_some() {
        map.insert("role".into(), "comms".into());
    }
    if let Some(source) = config.metadata.agent_source {
        let value = match source {
            crate::connection::McpAgentSource::BuiltIn => "built-in",
            crate::connection::McpAgentSource::Plugin => "plugin",
            crate::connection::McpAgentSource::UserSettings => "userSettings",
            crate::connection::McpAgentSource::ProjectSettings => "projectSettings",
            crate::connection::McpAgentSource::LocalSettings => "localSettings",
            crate::connection::McpAgentSource::PolicySettings => "policySettings",
            crate::connection::McpAgentSource::FlagSettings => "flagSettings",
            crate::connection::McpAgentSource::AdditionalDirectory => "additionalDirectory",
        };
        map.insert("agentSource".into(), value.into());
    }
    let canonical = canonicalize_logical_key_value(raw);
    let canonical_json = serde_json::to_string(&canonical)
        .expect("canonical discovery-cache key config is serializable");
    let hash = sha256_hex(canonical_json.as_bytes());
    format!("{}-{}", config.name, &hash[..16])
}

// ── config-independent constants (oracle `Pe`/`Me`/`Ae`/`xe`) ──────────────

/// Schema version tag written into every persisted entry (`v` in the oracle
/// schema `B`, literal `1` there — this port's own numbering, bumped to `2`
/// when the entry grew a full catalog payload; see [`DiscoveryCacheEntry`]).
/// A version this store doesn't recognize is treated as
/// [`EntryLookup::Corrupt`] — never trusted, never an error. A `v1` entry
/// (this module's original metadata-only shape) still PARSES cleanly —
/// every field schema v2 added is `#[serde(default)]`, on purpose, so a
/// future additive change never needs another version bump — but is
/// rejected by the explicit `entry.version != CACHE_SCHEMA_VERSION` check in
/// [`DiscoveryCacheStore::load`], never trusted with a silently-defaulted
/// (empty) catalog.
pub const CACHE_SCHEMA_VERSION: u32 = 2;

/// The feature opt-in env var (oracle `MCP_DISCOVERY_CACHE`, read through a
/// boolean-coerced env schema — `a.MCP_DISCOVERY_CACHE===true`/`===false`).
/// Reused here via [`platform_api::env::is_env_truthy`]/[`platform_api::env::is_env_defined_falsy`],
/// this port's established idiom for a coerced-boolean env var.
pub const ENV_ENABLED: &str = "MCP_DISCOVERY_CACHE";

/// Independent GrowthBook feature flag for the MCP skills capability gate.
/// This is deliberately not derived from [`ENV_ENABLED`]: the discovery cache
/// may be enabled while skills handling remains disabled (and vice versa).
const FLAG_SKILLS: &str = "tengu_mcp_skills";

/// Shared serial guard for tests that mutate `ENV_ENABLED`. Env vars are
/// process-global, so the registry's eligibility tests must take the same
/// lock this module's own tests use or they race.
#[cfg(test)]
pub(crate) fn tests_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(test)]
const TEST_ENV_KEYS: [&str; 4] = [
    ENV_ENABLED,
    ENV_TTL_SECONDS,
    ENV_MAX_STALE_SECONDS,
    ENV_STRIKES,
];

#[cfg(test)]
fn snapshot_test_env() -> [(&'static str, Option<std::ffi::OsString>); 4] {
    TEST_ENV_KEYS.map(|key| (key, std::env::var_os(key)))
}

#[cfg(test)]
fn clear_test_env() {
    for key in TEST_ENV_KEYS {
        std::env::remove_var(key);
    }
}

#[cfg(test)]
fn restore_test_env(saved: &[(&'static str, Option<std::ffi::OsString>); 4]) {
    for (key, value) in saved {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}

#[cfg(test)]
#[derive(Clone)]
struct StagePathObserver {
    root: std::path::PathBuf,
    on_stage_path: std::sync::Arc<dyn Fn(&std::path::Path) + Send + Sync>,
}

#[cfg(test)]
fn stage_path_observer() -> &'static std::sync::Mutex<Option<StagePathObserver>> {
    static OBSERVER: std::sync::OnceLock<std::sync::Mutex<Option<StagePathObserver>>> =
        std::sync::OnceLock::new();
    OBSERVER.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
fn notify_stage_path_observer(path: &std::path::Path) {
    let observer = stage_path_observer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(observer) = observer {
        if path.starts_with(&observer.root) {
            (observer.on_stage_path)(path);
        }
    }
}

#[cfg(test)]
struct StagePathObserverGuard {
    previous: Option<StagePathObserver>,
}

#[cfg(test)]
impl StagePathObserverGuard {
    fn install(
        root: std::path::PathBuf,
        on_stage_path: std::sync::Arc<dyn Fn(&std::path::Path) + Send + Sync>,
    ) -> Self {
        let mut slot = stage_path_observer()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = slot.replace(StagePathObserver {
            root,
            on_stage_path,
        });
        Self { previous }
    }
}

#[cfg(test)]
impl Drop for StagePathObserverGuard {
    fn drop(&mut self) {
        *stage_path_observer()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = self.previous.take();
    }
}

#[cfg(test)]
pub(crate) struct TestEnvGuard {
    saved: [(&'static str, Option<std::ffi::OsString>); 4],
}

#[cfg(test)]
impl TestEnvGuard {
    pub(crate) fn new() -> Self {
        let saved = snapshot_test_env();
        clear_test_env();
        Self { saved }
    }

    pub(crate) fn set(&self, key: &'static str, value: &str) {
        std::env::set_var(key, value);
    }
}

#[cfg(test)]
impl Drop for TestEnvGuard {
    fn drop(&mut self) {
        restore_test_env(&self.saved);
    }
}
/// Fresh-window override, in SECONDS (oracle `MCP_DISCOVERY_CACHE_TTL_S`).
pub const ENV_TTL_SECONDS: &str = "MCP_DISCOVERY_CACHE_TTL_S";
/// Max-stale override, in SECONDS (oracle `MCP_DISCOVERY_CACHE_MAX_STALE_S`).
pub const ENV_MAX_STALE_SECONDS: &str = "MCP_DISCOVERY_CACHE_MAX_STALE_S";
/// Strike-threshold override (oracle `MCP_DISCOVERY_CACHE_STRIKES`).
pub const ENV_STRIKES: &str = "MCP_DISCOVERY_CACHE_STRIKES";

/// Oracle `Pe` — default fresh-window seconds (15 minutes).
const DEFAULT_TTL_SECONDS: u64 = 900;
/// Oracle `Me` — default max-stale seconds (4 hours).
const DEFAULT_MAX_STALE_SECONDS: u64 = 14_400;
/// Oracle `Ae` — hard ceiling on max-stale regardless of the env override (7 days).
const MAX_STALE_CEILING_SECONDS: u64 = 604_800;
/// Oracle `xe` — default strike threshold (a SINGLE recorded failure misses).
const DEFAULT_STRIKES: u32 = 1;

/// Oracle `Sln()`'s enabled check, restricted to the literal env var this
/// port has (no kill-switch/SDK-embedder concept exists here — see module
/// docs). Unset or unrecognized ⇒ `false` (the oracle's default-off
/// `"not-enabled"` arm); an explicit falsy value (`"false"`/`"0"`/…) is ALSO
/// `false` (oracle's `"env-disabled"`) — the two collapse to the same
/// disable reason at [`cache_gate`], matching `cot`'s own collapse.
#[must_use]
pub fn feature_enabled() -> bool {
    platform_api::env::is_env_truthy(std::env::var(ENV_ENABLED).ok().as_deref())
}

/// Whether the MCP skills capability gate is enabled. This reads the
/// independent `tengu_mcp_skills` feature flag rather than the discovery-cache
/// opt-in, matching the two separate gates in the reference implementation.
#[must_use]
fn skills_feature_enabled() -> bool {
    telemetry::flag_bool(FLAG_SKILLS, false)
}

/// Plain, non-negative-integer env parse shared by the three numeric knobs
/// below — mirrors this crate's existing `MCP_TIMEOUT` convention
/// (`registry::mcp_connection_timeout`): trim, parse as `u64`, `0` or
/// non-numeric falls back to `default`.
fn positive_seconds_from_env(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default)
}

/// Oracle `ye()` — max-stale window in milliseconds, clamped to the 7-day
/// ceiling regardless of how large an override is set.
#[must_use]
pub fn max_stale_ms() -> u64 {
    positive_seconds_from_env(ENV_MAX_STALE_SECONDS, DEFAULT_MAX_STALE_SECONDS)
        .min(MAX_STALE_CEILING_SECONDS)
        .saturating_mul(1000)
}

/// Oracle `Oe()` — fresh window in milliseconds: the TTL override, further
/// capped by [`max_stale_ms`] (a fresh window can never outlive the max-stale
/// window).
#[must_use]
pub fn ttl_ms() -> u64 {
    positive_seconds_from_env(ENV_TTL_SECONDS, DEFAULT_TTL_SECONDS)
        .saturating_mul(1000)
        .min(max_stale_ms())
}

/// Oracle `pe()` — the `consecutiveRefreshFailures` strike threshold.
#[must_use]
pub fn strike_threshold() -> u32 {
    let raw = std::env::var(ENV_STRIKES).ok();
    let Some(raw) = raw else {
        return DEFAULT_STRIKES;
    };
    raw.trim()
        .parse::<u32>()
        .ok()
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_STRIKES)
}

// ── eligibility gate (oracle `me`/`Ie`) ─────────────────────────────────────

/// Why [`cache_gate`] refused to consult the cache at all — oracle `me`'s
/// disable reasons evaluated by the production cache gate. Provenance-only
/// reasons are non-purging; only the user-controlled opt-out/helper reasons
/// purge an existing server family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheGateReason {
    /// `type!=="http"&&type!=="sse"` — only these two transports are ever
    /// cache-eligible.
    Transport,
    /// [`feature_enabled`] is `false` (unset, or an explicit falsy value).
    FeatureDisabled,
    /// The config declared `discoveryCache:false` (oracle `W`'s `"opt-out"`
    /// entry).
    OptOut,
    /// The config declared a `headersHelper` (oracle `W`'s `"headers-helper"`
    /// entry) — an executable-derived header can't be safely assumed stable
    /// across a cached round.
    HeadersHelper,
    /// Explicit `--mcp-config` ownership; this is a non-purging miss.
    CliOwned,
    /// Unresolved `${VAR}` remains in a remote URL/header; non-purging.
    EnvPlaceholder,
    /// Host-injected MCP-only temporary credential; non-purging.
    AmbientCredential,
}

impl CacheGateReason {
    /// Oracle `o==="transport"?"transport":"disabled"` — every reason BUT
    /// `Transport` collapses to the single generic [`MissReason::Disabled`].
    #[must_use]
    pub fn miss_reason(self) -> MissReason {
        match self {
            Self::Transport => MissReason::Transport,
            Self::FeatureDisabled | Self::OptOut | Self::HeadersHelper => MissReason::Disabled,
            Self::CliOwned => MissReason::CliOwned,
            Self::EnvPlaceholder => MissReason::EnvPlaceholder,
            Self::AmbientCredential => MissReason::AmbientCredential,
        }
    }

    /// Oracle `Ie(o)` — does this reason ALSO purge any existing on-disk
    /// entry for the server before reporting the miss? True only for the two
    /// reasons a user can flip at runtime (`opt-out`, `headers-helper`); a
    /// stale entry left behind by either would otherwise resurface the
    /// moment the config reverts.
    #[must_use]
    pub fn purges_existing_entry(self) -> bool {
        matches!(self, Self::OptOut | Self::HeadersHelper)
    }
}

/// Oracle `me(t)`, restricted to the reasons this port can evaluate (see
/// module docs). Checked in the SAME order as the oracle — the feature gate
/// fires before the transport check, so a disabled feature on a non-sse/http
/// spec reports `FeatureDisabled`, not `Transport`, exactly like `cot`.
/// Returns `None` when the server is cache-eligible.
#[must_use]
pub fn cache_gate(
    spec: &McpTransportSpec,
    discovery_cache_opt_out: Option<bool>,
    feature_enabled: bool,
) -> Option<CacheGateReason> {
    if !feature_enabled {
        return Some(CacheGateReason::FeatureDisabled);
    }
    let headers_helper = match spec {
        McpTransportSpec::Sse { headers_helper, .. }
        | McpTransportSpec::Http { headers_helper, .. } => headers_helper,
        _ => return Some(CacheGateReason::Transport),
    };
    if discovery_cache_opt_out == Some(false) {
        return Some(CacheGateReason::OptOut);
    }
    if headers_helper.is_some() {
        return Some(CacheGateReason::HeadersHelper);
    }
    None
}

/// Provenance-aware cache gate with the oracle's fixed ordering.  The legacy
/// [`cache_gate`] remains available for callers that have no metadata.
#[must_use]
pub fn cache_gate_with_metadata(
    spec: &McpTransportSpec,
    discovery_cache_opt_out: Option<bool>,
    feature_enabled: bool,
    metadata: &crate::connection::McpServerMetadata,
) -> Option<CacheGateReason> {
    if !feature_enabled {
        return Some(CacheGateReason::FeatureDisabled);
    }
    let headers_helper = match spec {
        McpTransportSpec::Sse { headers_helper, .. }
        | McpTransportSpec::Http { headers_helper, .. } => headers_helper,
        _ => return Some(CacheGateReason::Transport),
    };
    if metadata.cli_owned {
        return Some(CacheGateReason::CliOwned);
    }
    if spec_contains_env_placeholder(spec) {
        return Some(CacheGateReason::EnvPlaceholder);
    }
    if metadata.ambient_credential {
        return Some(CacheGateReason::AmbientCredential);
    }
    if discovery_cache_opt_out == Some(false) {
        return Some(CacheGateReason::OptOut);
    }
    if headers_helper.is_some() {
        return Some(CacheGateReason::HeadersHelper);
    }
    None
}

fn spec_contains_env_placeholder(spec: &McpTransportSpec) -> bool {
    let has_placeholder = |value: &str| value.contains("${") && value.contains('}');
    match spec {
        McpTransportSpec::Sse { url, headers, .. }
        | McpTransportSpec::Http { url, headers, .. } => {
            has_placeholder(url) || headers.values().any(|value| has_placeholder(value))
        }
        _ => false,
    }
}

// ── miss-reason vocabulary (oracle `rs`/`Vo`/`as`) ──────────────────────────

/// The full oracle miss-reason vocabulary. Some variants are retained for
/// compatibility with the recovered telemetry vocabulary even when a caller
/// has no corresponding runtime surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissReason {
    /// Non-cache-eligible transport.
    Transport,
    /// Any disable reason but `Transport` — see [`CacheGateReason::miss_reason`].
    Disabled,
    /// No on-disk entry for this server.
    Absent,
    /// The on-disk entry failed to parse, had the wrong schema version, was
    /// a symlink/non-regular/oversize file, or was keyed for a different
    /// server (a cache-key mismatch) — oracle collapses all of these into
    /// its own `"corrupt"` path (`h()`/`p("mcp_discovery_cache","corrupt_entry")`).
    Corrupt,
    /// The entry aged past the max-stale window (or carries an
    /// implausible future timestamp — a clock-skew guard).
    Expired,
    /// `consecutiveRefreshFailures` reached [`strike_threshold`].
    Strike,
    /// Grant-token fingerprint was unavailable.
    NoFingerprint,
    /// An in-flight live connection already exists for this server.
    LiveConnection,
    /// The cached entry declares MCP skills while the skills feature is on.
    SkillsCapable,
    /// The cached entry declares the experimental channel capability.
    ChannelCapable,
    /// Explicit CLI-owned config (non-purging).
    CliOwned,
    /// Unresolved environment placeholder (non-purging).
    EnvPlaceholder,
    /// Explicit MCP-only ambient credential (non-purging).
    AmbientCredential,
}

/// Oracle `as(e)` — the telemetry bucket name for a decision. Curiously,
/// `Transport`/`Absent`/`LiveConnection`/`SkillsCapable`/`ChannelCapable` all
/// map to the literal `"live"` (the oracle only tracks cache HEALTH —
/// disabled/expired/corrupt/strike/no-fingerprint — as `miss_*`; the rest
/// are "we did a live fetch, nothing to measure"), verified byte-exact at
/// 2.1.251 Mach-O @182315844.
#[must_use]
pub fn miss_telemetry_value(reason: MissReason) -> &'static str {
    match reason {
        MissReason::Disabled => "miss_disabled",
        MissReason::Expired => "miss_expired",
        MissReason::Corrupt => "miss_corrupt",
        MissReason::Strike => "miss_strike",
        MissReason::NoFingerprint => "miss_no_fingerprint",
        MissReason::CliOwned | MissReason::EnvPlaceholder | MissReason::AmbientCredential => "live",
        MissReason::Transport
        | MissReason::Absent
        | MissReason::LiveConnection
        | MissReason::SkillsCapable
        | MissReason::ChannelCapable => "live",
    }
}

/// Oracle `Ko(e)` (2.1.251, the SAME chunk as `cot`/`Jo`/`Vo` — resolved from
/// the region around @182512938, NOT the unrelated `Ko`/`Jo` functions that
/// share these minified names elsewhere in the binary; see this module's
/// header note on chunk-local names). Recovered source:
/// ```text
/// function Ko(e){switch(e.reason){
///   case"absent":case"expired":case"corrupt":
///   case"strike-threshold":case"no-fingerprint":return!0;
///   case"disabled":case"transport":case"live-connection":
///   case"skills-capable":case"channel-capable":return!1;
///   default:return e.reason}}
/// ```
/// This gates whether `tengu_mcp_discovery_source` fires at ALL on a MISS
/// decision — `true` for the five reasons meaning "a disk read was actually
/// attempted and came up unusable", `false` for the three gate-level reasons
/// meaning the cache was never consulted (`disabled`/`transport`/
/// `live-connection`) plus the two capability-miss reasons this port never
/// constructs. The fresh/stale HIT branch emits unconditionally — this gate
/// applies only to [`Decision::Miss`] (see the module doc's "What §11 Stage
/// 1 wires in" section for why a hit emits nothing in THIS port today).
#[must_use]
pub fn miss_emits_discovery_source_telemetry(reason: MissReason) -> bool {
    matches!(
        reason,
        MissReason::Absent
            | MissReason::Expired
            | MissReason::Corrupt
            | MissReason::Strike
            | MissReason::NoFingerprint
    )
}

/// Wall-clock "now", ms since the Unix epoch — the real-clock reading
/// production callers hand to [`DiscoveryCachePolicy::from_env`]/
/// [`DiscoveryCacheEntry::new`]. Tests use their own literal values instead
/// (both take `now_ms`/`saved_at_ms` as plain parameters precisely so they
/// don't need this).
#[must_use]
pub fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

// ── the persisted entry + decision (oracle `B`/`cot`) ───────────────────────

/// `serverInfo` from the server's `initialize` response (oracle schema `B`'s
/// `serverInfo` sub-object — spread onto the served "cached" client only
/// when present: `...v.serverInfo && {serverInfo:{name:...,version:...}}`).
///
/// This port's [`platform_api::McpTransport::initialize`] returns only
/// [`ServerCapabilitiesDto`] — the wire `serverInfo` block is discarded
/// before it reaches `mcp::registry`, so nothing populates this field today.
/// Kept as a real (rather than omitted) field so schema v2 is
/// forward-compatible with whichever future change threads `serverInfo`
/// through the transport trait.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DiscoveryCacheServerInfo {
    /// Server name as reported at `initialize`.
    pub name: String,
    /// Server version as reported at `initialize`.
    pub version: String,
}

/// One persisted discovery-cache entry.
///
/// Schema v2: carries the FULL catalog a cache hit would need to serve a
/// server without dialing (tools/resources/resource_templates/prompts +
/// capabilities), not just the metadata v1 needed to make the fresh/stale/
/// miss decision. Still narrower than the oracle's `B` schema; its
/// `negotiatedEra` field is retained here only to compare a stale hit with the
/// subsequent live revalidation result.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DiscoveryCacheEntry {
    /// Schema version — see [`CACHE_SCHEMA_VERSION`].
    #[serde(rename = "v")]
    pub version: u32,
    /// Negotiated protocol era used to populate this catalog.  Old entries
    /// omit it and are interpreted as legacy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negotiated_era: Option<String>,
    /// Logical server/config key from [`logical_cache_key`]. A mismatch
    /// (entry read from the right partition path but keyed for a different
    /// server/config) is treated as [`MissReason::Corrupt`].
    pub cache_key: String,
    /// When this entry was saved, ms since the Unix epoch.
    pub saved_at_ms: u64,
    /// A separate, possibly-later save timestamp for a partial (tools-only)
    /// refresh — oracle `toolsSavedAt`. `None` when never separately updated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_saved_at_ms: Option<u64>,
    /// Consecutive refresh failures recorded against this entry.
    #[serde(default)]
    pub consecutive_refresh_failures: u32,
    /// Server capability flags returned by `initialize`. [`decide`]'s
    /// degenerate check reads `capabilities.tools` directly (oracle
    /// `k.capabilities.tools`).
    #[serde(default)]
    pub capabilities: ServerCapabilitiesDto,
    /// `serverInfo`, when this port has one to store — see
    /// [`DiscoveryCacheServerInfo`]'s doc (always `None` today).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_info: Option<DiscoveryCacheServerInfo>,
    /// The cached `tools/list` result. [`decide`]'s degenerate check reads
    /// `tools.is_empty()` directly (oracle `k.tools.length===0`).
    #[serde(default)]
    pub tools: Vec<McpToolDto>,
    /// The cached `resources/list` result.
    #[serde(default)]
    pub resources: Vec<McpResourceDto>,
    /// The cached `resources/templates/list` result.
    #[serde(default)]
    pub resource_templates: Vec<McpResourceTemplateDto>,
    /// The cached `prompts/list` result.
    #[serde(default)]
    pub prompts: Vec<McpPromptDto>,
}

impl DiscoveryCacheEntry {
    /// Build a fresh entry for `cache_key`, saved "now" (`saved_at_ms`),
    /// with `consecutive_refresh_failures` reset to 0 and no `server_info`
    /// (see [`Self::with_server_info`] to attach one when a future change
    /// makes that possible).
    #[must_use]
    pub fn new(
        cache_key: String,
        saved_at_ms: u64,
        capabilities: ServerCapabilitiesDto,
        tools: Vec<McpToolDto>,
        resources: Vec<McpResourceDto>,
        resource_templates: Vec<McpResourceTemplateDto>,
        prompts: Vec<McpPromptDto>,
    ) -> Self {
        Self {
            version: CACHE_SCHEMA_VERSION,
            negotiated_era: None,
            cache_key,
            saved_at_ms,
            tools_saved_at_ms: None,
            consecutive_refresh_failures: 0,
            capabilities,
            server_info: None,
            tools,
            resources,
            resource_templates,
            prompts,
        }
    }

    /// Attach a `serverInfo` — chainable builder, kept separate from
    /// [`Self::new`] so the common (server-info-less) construction doesn't
    /// have to thread an extra `None` through every call site.
    #[must_use]
    pub fn with_server_info(mut self, server_info: DiscoveryCacheServerInfo) -> Self {
        self.server_info = Some(server_info);
        self
    }

    /// Attach the actual negotiated protocol era used for this catalog.
    #[must_use]
    pub fn with_negotiated_era(mut self, era: impl Into<String>) -> Self {
        self.negotiated_era = Some(era.into());
        self
    }
}

/// The outcome of a store read, already folded through the fail-safe
/// corruption checks ([`DiscoveryCacheStore::load`]) — the input [`decide`]
/// consumes.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryLookup {
    /// No on-disk entry.
    Absent,
    /// An on-disk entry existed but was unusable (see [`MissReason::Corrupt`]'s doc).
    Corrupt,
    /// A structurally valid, correctly-keyed entry.
    Found(DiscoveryCacheEntry),
}

/// The three-way outcome oracle `cot` returns (`{kind:"fresh"|"stale"|"miss",...}`).
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Cache hit, young enough to skip re-discovery outright.
    Fresh {
        /// The hit entry.
        entry: DiscoveryCacheEntry,
        /// Age of the entry at decision time, ms.
        age_ms: u64,
    },
    /// Cache hit, but old enough that a caller should refresh in the
    /// background while still serving the stale data immediately
    /// (stale-while-revalidate). Stage 3 callers install the cached catalog
    /// and then kick a detached revalidation path through the registry.
    Stale {
        /// The hit entry.
        entry: DiscoveryCacheEntry,
        /// Age of the entry at decision time, ms.
        age_ms: u64,
    },
    /// No usable cache; the caller must do a live discovery round.
    Miss {
        /// Why.
        reason: MissReason,
    },
}

/// The numeric knobs [`decide`] needs, bundled into one value so the
/// function itself stays under clippy's `too_many_arguments` threshold.
/// Construct directly from the env-backed defaults with [`Self::from_env`],
/// or with explicit values (tests, or a future caller with its own clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryCachePolicy {
    /// "Now", ms since the Unix epoch — threaded explicitly rather than read
    /// internally so [`decide`] stays a pure function of its inputs.
    pub now_ms: u64,
    /// Oracle `Oe()` — see [`ttl_ms`].
    pub ttl_ms: u64,
    /// Oracle `ye()` — see [`max_stale_ms`].
    pub max_stale_ms: u64,
    /// Oracle `pe()` — see [`strike_threshold`].
    pub strike_threshold: u32,
}

impl DiscoveryCachePolicy {
    /// Build from the env-backed defaults/overrides ([`ttl_ms`],
    /// [`max_stale_ms`], [`strike_threshold`]) plus a caller-supplied clock
    /// reading.
    #[must_use]
    pub fn from_env(now_ms: u64) -> Self {
        Self {
            now_ms,
            ttl_ms: ttl_ms(),
            max_stale_ms: max_stale_ms(),
            strike_threshold: strike_threshold(),
        }
    }
}

/// Oracle `cot` (2.1.251 Mach-O @176266197) — the pure decision, given
/// everything a store read + the config already resolved. This compatibility
/// wrapper has no server metadata argument; production callers use
/// [`decide_with_metadata`] so provenance and capability evidence are applied.
#[must_use]
pub fn decide(
    spec: &McpTransportSpec,
    discovery_cache_opt_out: Option<bool>,
    feature_enabled: bool,
    lookup: EntryLookup,
    policy: DiscoveryCachePolicy,
) -> Decision {
    decide_with_metadata(
        spec,
        discovery_cache_opt_out,
        feature_enabled,
        lookup,
        policy,
        &crate::connection::McpServerMetadata::default(),
    )
}

/// Metadata-aware discovery decision.  The post-hit capability checks are
/// intentionally performed before Fresh/Stale so a catalog that exposes a
/// coordinator-only capability is revalidated live rather than served from
/// disk.
#[must_use]
pub fn decide_with_metadata(
    spec: &McpTransportSpec,
    discovery_cache_opt_out: Option<bool>,
    feature_enabled: bool,
    lookup: EntryLookup,
    policy: DiscoveryCachePolicy,
    metadata: &crate::connection::McpServerMetadata,
) -> Decision {
    if let Some(reason) =
        cache_gate_with_metadata(spec, discovery_cache_opt_out, feature_enabled, metadata)
    {
        return Decision::Miss {
            reason: reason.miss_reason(),
        };
    }
    let entry = match lookup {
        EntryLookup::Absent => {
            return Decision::Miss {
                reason: MissReason::Absent,
            }
        }
        EntryLookup::Corrupt => {
            return Decision::Miss {
                reason: MissReason::Corrupt,
            }
        }
        EntryLookup::Found(entry) => entry,
    };
    if entry.consecutive_refresh_failures >= policy.strike_threshold {
        return Decision::Miss {
            reason: MissReason::Strike,
        };
    }
    // Oracle `oe(k)=max(k.savedAt,k.toolsSavedAt??0)`; `oe(k)-r>R` is a
    // clock-skew guard against a saved timestamp implausibly ahead of "now".
    let anchor = entry.saved_at_ms.max(entry.tools_saved_at_ms.unwrap_or(0));
    if anchor.saturating_sub(policy.now_ms) > policy.max_stale_ms {
        return Decision::Miss {
            reason: MissReason::Expired,
        };
    }
    let age_ms = policy.now_ms.saturating_sub(entry.saved_at_ms);
    if age_ms >= policy.max_stale_ms {
        return Decision::Miss {
            reason: MissReason::Expired,
        };
    }
    // `k.capabilities.tools&&k.tools.length===0` — a server that claims the
    // tools capability but returned none is treated as degenerate and never
    // served fresh, even within the TTL window (ties to the oracle's
    // `connected_zero_tools` degraded-connection telemetry). Schema v2 stores
    // the real fields, so this reads them directly — byte-exact with the
    // oracle expression, not a flattened approximation.
    let degenerate = entry.capabilities.tools && entry.tools.is_empty();
    let skills_capable = skills_feature_enabled()
        && entry.capabilities.resources
        && entry
            .capabilities
            .extensions
            .get("io.modelcontextprotocol/skills")
            .is_some();
    if skills_capable {
        return Decision::Miss {
            reason: MissReason::SkillsCapable,
        };
    }
    let channel_capable = entry
        .capabilities
        .experimental
        .get("claude/channel")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if channel_capable {
        return Decision::Miss {
            reason: MissReason::ChannelCapable,
        };
    }
    if age_ms < policy.ttl_ms && !degenerate {
        Decision::Fresh { entry, age_ms }
    } else {
        Decision::Stale { entry, age_ms }
    }
}

// ── persisted store ──────────────────────────────────────────────────────

/// Oversize guard. Matches the oracle's 8 MiB ceiling so large but legitimate
/// MCP catalogs are not treated as corrupt misses.
const MAX_ENTRY_BYTES: u64 = 8 * 1024 * 1024;

/// A directory of one-file-per-server discovery-cache entries. The root is
/// caller-supplied so the same implementation can use the desktop/mobile
/// app-private production root and remain testable against a tempdir.
#[derive(Debug, Clone)]
pub struct DiscoveryCacheStore {
    root: std::path::PathBuf,
}

impl DiscoveryCacheStore {
    fn valid_partition_key(key: &str) -> bool {
        key.len() == 32
            && key
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    fn staging_path(&self, key: &str, nonce: u64) -> std::path::PathBuf {
        self.root
            .join(format!("{key}.tmp-{}-{nonce}", std::process::id()))
    }

    /// Open a store rooted at `root`. Does not touch the filesystem — the
    /// directory is created lazily on first [`Self::store`].
    #[must_use]
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Legacy helper for older tests: a stable SHA-256 over `{name, transport
    /// kind, url, headers}`. Production discovery-cache partitioning now uses
    /// [`logical_cache_key`] instead.
    #[must_use]
    pub fn cache_key(name: &str, spec: &McpTransportSpec) -> String {
        use sha2::{Digest, Sha256};
        let (kind, url, headers_json) = match spec {
            McpTransportSpec::Sse { url, headers, .. }
            | McpTransportSpec::Http { url, headers, .. } => (
                spec.kind(),
                url.as_str(),
                serde_json::to_string(headers).unwrap_or_default(),
            ),
            other => (other.kind(), "", String::new()),
        };
        let material = format!("{name}\0{kind}\0{url}\0{headers_json}");
        let mut hasher = Sha256::new();
        hasher.update(material.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    fn entry_path(&self, key: &str) -> std::path::PathBuf {
        self.root.join(format!("{key}.json"))
    }

    fn partitioned_entry_path(&self, partition_key: &str) -> std::path::PathBuf {
        self.root.join(format!("{partition_key}.json"))
    }

    fn open_readonly_no_follow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        // `O_NOFOLLOW` closes the check/open race on the production Unix
        // targets without adding a platform dependency. Unsupported targets
        // still retain the post-open regular-file check below.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(0x20_000);
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(0x100);
        }
        options.open(path)
    }

    fn load_path(&self, path: &std::path::Path, expected_key: &str) -> EntryLookup {
        use std::io::Read as _;

        let file = match Self::open_readonly_no_follow(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return EntryLookup::Absent,
            Err(_) => return EntryLookup::Corrupt,
        };
        let Ok(meta) = file.metadata() else {
            return EntryLookup::Corrupt;
        };
        if !meta.is_file() {
            return EntryLookup::Corrupt;
        }
        if meta.len() > MAX_ENTRY_BYTES {
            return EntryLookup::Corrupt;
        }
        let mut raw = String::new();
        if file
            .take(MAX_ENTRY_BYTES.saturating_add(1))
            .read_to_string(&mut raw)
            .is_err()
        {
            return EntryLookup::Corrupt;
        }
        if raw.len() as u64 > MAX_ENTRY_BYTES {
            return EntryLookup::Corrupt;
        }
        let Ok(entry) = serde_json::from_str::<DiscoveryCacheEntry>(&raw) else {
            return EntryLookup::Corrupt;
        };
        if entry.version != CACHE_SCHEMA_VERSION || entry.cache_key != expected_key {
            return EntryLookup::Corrupt;
        }
        EntryLookup::Found(entry)
    }

    fn store_path(
        &self,
        entry: &DiscoveryCacheEntry,
        path: std::path::PathBuf,
        staging_key: &str,
    ) -> std::io::Result<()> {
        use std::io::Write as _;

        static NEXT_STAGE_NONCE: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);

        std::fs::create_dir_all(&self.root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let perms = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(&self.root, perms)?;
        }
        let bytes = serde_json::to_vec(entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if u64::try_from(bytes.len())
            .ok()
            .is_some_and(|len| len > MAX_ENTRY_BYTES)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "discovery cache entry exceeds max size",
            ));
        }
        loop {
            let nonce = NEXT_STAGE_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp_path = self.staging_path(staging_key, nonce);
            #[cfg(test)]
            notify_stage_path_observer(&tmp_path);
            let mut file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;

                let perms = std::fs::Permissions::from_mode(0o600);
                std::fs::set_permissions(&tmp_path, perms)?;
            }

            let write_result = file.write_all(&bytes);
            drop(file);
            if let Err(error) = write_result {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(error);
            }

            return match std::fs::rename(&tmp_path, &path) {
                Ok(()) => Ok(()),
                Err(error) => {
                    let _ = std::fs::remove_file(&tmp_path);
                    Err(error)
                }
            };
        }
    }

    /// Fail-safe read. A missing file is [`EntryLookup::Absent`]; a symlink,
    /// a non-regular file, an oversize file, invalid JSON, a schema-version
    /// mismatch, or a stored `cache_key` that doesn't match `expected_key`
    /// are ALL [`EntryLookup::Corrupt`] — this store never surfaces a raw
    /// I/O error and never trusts a garbled/mismatched entry, matching the
    /// oracle's fail-safe posture (`miss_corrupt` exists for exactly this).
    #[must_use]
    pub fn load(&self, expected_key: &str) -> EntryLookup {
        self.load_path(&self.entry_path(expected_key), expected_key)
    }

    #[must_use]
    pub(crate) fn load_partitioned(&self, logical_key: &str, partition_key: &str) -> EntryLookup {
        if !Self::valid_partition_key(partition_key) {
            return EntryLookup::Corrupt;
        }
        self.load_path(&self.partitioned_entry_path(partition_key), logical_key)
    }

    /// Load a partition selected by the caller's expected protocol era.
    ///
    /// The era is part of the partition key, but the entry's actual negotiated
    /// era is intentionally not compared here. An auto-negotiated connection
    /// may fall back to legacy while retaining the modern expected partition,
    /// and old entries that omit the field are legacy by compatibility rule.
    pub(crate) fn load_partitioned_for_era(
        &self,
        logical_key: &str,
        partition_key: &str,
        _expected_era: &str,
    ) -> EntryLookup {
        self.load_partitioned(logical_key, partition_key)
    }

    /// Atomic write: serialize to a sibling temp file with an exclusive
    /// per-attempt nonce, then rename into place. A crash mid-write leaves
    /// only an orphaned `.tmp-*` file behind — the real path is untouched
    /// until the rename commits.
    ///
    /// # Errors
    /// Any I/O failure creating the directory, writing the temp file, or
    /// renaming it into place.
    pub fn store(&self, entry: &DiscoveryCacheEntry) -> std::io::Result<()> {
        self.store_path(entry, self.entry_path(&entry.cache_key), &entry.cache_key)
    }

    pub(crate) fn store_partitioned(
        &self,
        entry: &DiscoveryCacheEntry,
        partition_key: &str,
    ) -> std::io::Result<()> {
        if !Self::valid_partition_key(partition_key) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid discovery cache partition key",
            ));
        }
        self.store_path(
            entry,
            self.partitioned_entry_path(partition_key),
            partition_key,
        )
    }

    /// Delete exactly one identity partition. A stale revalidation may need
    /// to retire the partition that served its cached entry without touching
    /// another grant/era partition for the same server.
    pub(crate) fn purge_partitioned(&self, partition_key: &str) -> std::io::Result<()> {
        if !Self::valid_partition_key(partition_key) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid discovery cache partition key",
            ));
        }
        match std::fs::remove_file(self.partitioned_entry_path(partition_key)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Delete the entry for `key`. A missing file is not an error (mirrors
    /// the oracle's best-effort purge on `opt-out`/`headers-helper` —
    /// [`CacheGateReason::purges_existing_entry`]).
    ///
    /// # Errors
    /// Any I/O failure other than the file already being absent.
    pub fn purge(&self, key: &str) -> std::io::Result<()> {
        match std::fs::remove_file(self.entry_path(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    #[cfg(test)]
    pub(crate) fn purge_family(&self, logical_key: &str) -> std::io::Result<()> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Ok(());
        };
        for entry in entries {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() || !file_type.is_file() {
                continue;
            }
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            let matches_legacy = file_name == format!("{logical_key}.json");
            if matches_legacy {
                std::fs::remove_file(entry.path())?;
                continue;
            }
            if !file_name.ends_with(".json") {
                continue;
            }
            let Some(cache_entry) = Self::read_entry_for_purge(&entry.path()) else {
                continue;
            };
            if cache_entry.cache_key == logical_key {
                std::fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }

    pub(crate) fn purge_server_family(&self, server_name: &str) -> std::io::Result<()> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Ok(());
        };
        let prefix = format!("{server_name}-");
        for entry in entries {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() || !file_type.is_file() {
                continue;
            }
            let Some(cache_entry) = Self::read_entry_for_purge(&entry.path()) else {
                continue;
            };
            let matches = cache_entry
                .cache_key
                .strip_prefix(&prefix)
                .is_some_and(|suffix| {
                    suffix.len() == 16 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
                });
            if matches {
                std::fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }

    fn read_entry_for_purge(path: &std::path::Path) -> Option<DiscoveryCacheEntry> {
        use std::io::Read as _;

        let file = Self::open_readonly_no_follow(path).ok()?;
        let meta = file.metadata().ok()?;
        if !meta.is_file() || meta.len() > MAX_ENTRY_BYTES {
            return None;
        }
        let mut raw = String::new();
        file.take(MAX_ENTRY_BYTES.saturating_add(1))
            .read_to_string(&mut raw)
            .ok()?;
        if raw.len() as u64 > MAX_ENTRY_BYTES {
            return None;
        }
        serde_json::from_str(&raw).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_spec(url: &str, headers_helper: Option<&str>) -> McpTransportSpec {
        McpTransportSpec::Http {
            url: url.to_string(),
            headers: platform_api::McpHeaders::new(),
            headers_helper: headers_helper.map(str::to_string),
            oauth: None,
        }
    }

    fn stdio_spec() -> McpTransportSpec {
        McpTransportSpec::Stdio {
            command: "srv".to_string(),
            args: vec![],
            env: std::collections::HashMap::new(),
        }
    }

    // ── cache_gate ──────────────────────────────────────────────────────

    #[test]
    fn cache_gate_non_remote_transport_is_transport_reason() {
        assert_eq!(
            cache_gate(&stdio_spec(), None, true),
            Some(CacheGateReason::Transport)
        );
    }

    /// Oracle order: the feature gate is checked BEFORE the transport check
    /// (`me`'s `Sln()` call precedes its `type!=="http"&&type!=="sse"`
    /// check) — so a disabled feature on a non-sse/http spec reports
    /// `FeatureDisabled`, never `Transport`.
    #[test]
    fn cache_gate_feature_disabled_outranks_transport_mismatch() {
        assert_eq!(
            cache_gate(&stdio_spec(), None, false),
            Some(CacheGateReason::FeatureDisabled)
        );
    }

    #[test]
    fn cache_gate_opt_out_reason() {
        assert_eq!(
            cache_gate(&http_spec("https://x.example", None), Some(false), true),
            Some(CacheGateReason::OptOut)
        );
        // `Some(true)` and `None` are NOT opt-out.
        assert_eq!(
            cache_gate(&http_spec("https://x.example", None), Some(true), true),
            None
        );
    }

    #[test]
    fn cache_gate_headers_helper_reason() {
        assert_eq!(
            cache_gate(
                &http_spec("https://x.example", Some("./helper")),
                None,
                true
            ),
            Some(CacheGateReason::HeadersHelper)
        );
    }

    #[test]
    fn cache_gate_eligible_when_no_disable_reason_applies() {
        assert_eq!(
            cache_gate(&http_spec("https://x.example", None), None, true),
            None
        );
    }

    #[test]
    fn provenance_gates_are_ordered_and_non_purging() {
        let spec = http_spec("https://${MCP_HOST}", None);
        let mut metadata = crate::connection::McpServerMetadata {
            cli_owned: true,
            ambient_credential: true,
            ..Default::default()
        };
        assert_eq!(
            cache_gate_with_metadata(&spec, Some(false), true, &metadata),
            Some(CacheGateReason::CliOwned)
        );
        metadata.cli_owned = false;
        assert_eq!(
            cache_gate_with_metadata(&spec, Some(false), true, &metadata),
            Some(CacheGateReason::EnvPlaceholder)
        );
        let placeholder_free = http_spec("https://x.example", None);
        assert_eq!(
            cache_gate_with_metadata(&placeholder_free, Some(false), true, &metadata),
            Some(CacheGateReason::AmbientCredential)
        );
        assert!(!CacheGateReason::CliOwned.purges_existing_entry());
        assert!(!CacheGateReason::EnvPlaceholder.purges_existing_entry());
        assert!(!CacheGateReason::AmbientCredential.purges_existing_entry());
        assert!(CacheGateReason::OptOut.purges_existing_entry());
        assert!(CacheGateReason::HeadersHelper.purges_existing_entry());
    }

    #[test]
    fn opt_out_and_headers_helper_purge_but_transport_and_feature_disabled_do_not() {
        assert!(CacheGateReason::OptOut.purges_existing_entry());
        assert!(CacheGateReason::HeadersHelper.purges_existing_entry());
        assert!(!CacheGateReason::Transport.purges_existing_entry());
        assert!(!CacheGateReason::FeatureDisabled.purges_existing_entry());
    }

    #[test]
    fn every_disable_reason_but_transport_collapses_to_disabled() {
        assert_eq!(
            CacheGateReason::Transport.miss_reason(),
            MissReason::Transport
        );
        assert_eq!(
            CacheGateReason::FeatureDisabled.miss_reason(),
            MissReason::Disabled
        );
        assert_eq!(CacheGateReason::OptOut.miss_reason(), MissReason::Disabled);
        assert_eq!(
            CacheGateReason::HeadersHelper.miss_reason(),
            MissReason::Disabled
        );
    }

    // ── miss_telemetry_value (oracle `as`) ────────────────────────────────

    #[test]
    fn telemetry_bucket_names_match_the_oracle_as_function() {
        assert_eq!(miss_telemetry_value(MissReason::Disabled), "miss_disabled");
        assert_eq!(miss_telemetry_value(MissReason::Expired), "miss_expired");
        assert_eq!(miss_telemetry_value(MissReason::Corrupt), "miss_corrupt");
        assert_eq!(miss_telemetry_value(MissReason::Strike), "miss_strike");
        assert_eq!(
            miss_telemetry_value(MissReason::NoFingerprint),
            "miss_no_fingerprint"
        );
        for live in [
            MissReason::Transport,
            MissReason::Absent,
            MissReason::LiveConnection,
            MissReason::SkillsCapable,
            MissReason::ChannelCapable,
            MissReason::CliOwned,
            MissReason::EnvPlaceholder,
            MissReason::AmbientCredential,
        ] {
            assert_eq!(miss_telemetry_value(live), "live");
        }
    }

    /// Oracle `Ko`'s exact true/false split, transcribed above
    /// [`miss_emits_discovery_source_telemetry`]'s doc. Note `Absent` is
    /// `Ko`-true (the event DOES fire, with `source:"live"` via
    /// [`miss_telemetry_value`]) even though it maps to the same `"live"`
    /// string `Transport` does, which is `Ko`-false — the two are easy to
    /// conflate since `miss_telemetry_value` alone can't tell them apart;
    /// this gate is a SEPARATE decision on the reason, not on the string.
    #[test]
    fn miss_emits_discovery_source_telemetry_matches_the_oracle_ko_split() {
        for should_emit in [
            MissReason::Absent,
            MissReason::Expired,
            MissReason::Corrupt,
            MissReason::Strike,
            MissReason::NoFingerprint,
        ] {
            assert!(
                miss_emits_discovery_source_telemetry(should_emit),
                "{should_emit:?} must emit"
            );
        }
        for should_not_emit in [
            MissReason::Disabled,
            MissReason::Transport,
            MissReason::LiveConnection,
            MissReason::SkillsCapable,
            MissReason::ChannelCapable,
            MissReason::CliOwned,
            MissReason::EnvPlaceholder,
            MissReason::AmbientCredential,
        ] {
            assert!(
                !miss_emits_discovery_source_telemetry(should_not_emit),
                "{should_not_emit:?} must NOT emit"
            );
        }
    }

    // ── decide ────────────────────────────────────────────────────────────

    /// `ServerCapabilitiesDto` with only `tools` set, the shape every
    /// existing `decide` test needs.
    fn caps_tools(tools: bool) -> ServerCapabilitiesDto {
        ServerCapabilitiesDto {
            tools,
            resources: false,
            prompts: false,
            logging: false,
            directory_read: false,
            experimental: std::collections::HashMap::new(),
            extensions: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn skills_flag_is_independent_and_skills_miss_precedes_channel_miss() {
        let mut capabilities = caps_tools(false);
        capabilities.resources = true;
        capabilities.extensions.insert(
            "io.modelcontextprotocol/skills".into(),
            serde_json::json!({}),
        );
        capabilities
            .experimental
            .insert("claude/channel".into(), serde_json::json!(true));
        let entry = DiscoveryCacheEntry::new(
            "srv-key".into(),
            100,
            capabilities,
            sample_tools(1),
            vec![],
            vec![],
            vec![],
        );
        let policy = DiscoveryCachePolicy {
            now_ms: 101,
            ttl_ms: 1000,
            max_stale_ms: 10000,
            strike_threshold: 3,
        };
        let spec = http_spec("https://x.example", None);
        telemetry::test_set_flag("tengu_mcp_skills", true);
        assert!(matches!(
            decide_with_metadata(
                &spec,
                None,
                true,
                EntryLookup::Found(entry.clone()),
                policy,
                &Default::default()
            ),
            Decision::Miss {
                reason: MissReason::SkillsCapable
            }
        ));
        telemetry::test_set_flag("tengu_mcp_skills", false);
        assert!(matches!(
            decide_with_metadata(
                &spec,
                None,
                true,
                EntryLookup::Found(entry.clone()),
                policy,
                &Default::default()
            ),
            Decision::Miss {
                reason: MissReason::ChannelCapable
            }
        ));
        let mut no_channel = entry.clone();
        no_channel.capabilities.experimental.clear();
        assert!(matches!(
            decide_with_metadata(
                &spec,
                None,
                true,
                EntryLookup::Found(no_channel),
                policy,
                &Default::default()
            ),
            Decision::Fresh { .. }
        ));
        let stale_policy = DiscoveryCachePolicy {
            now_ms: 1_101,
            ..policy
        };
        let mut no_channel = entry;
        no_channel.capabilities.experimental.clear();
        assert!(matches!(
            decide_with_metadata(
                &spec,
                None,
                true,
                EntryLookup::Found(no_channel),
                stale_policy,
                &Default::default()
            ),
            Decision::Stale { .. }
        ));
        telemetry::test_clear_flag("tengu_mcp_skills");
    }

    #[test]
    fn protocol_era_only_selects_partition_and_old_entries_default_to_legacy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiscoveryCacheStore::new(dir.path());
        let entry = DiscoveryCacheEntry::new(
            "logical".into(),
            1,
            caps_tools(false),
            sample_tools(1),
            vec![],
            vec![],
            vec![],
        );
        store
            .store_partitioned(&entry, "0123456789abcdef0123456789abcdef")
            .expect("store");
        assert!(matches!(
            store.load_partitioned_for_era("logical", "0123456789abcdef0123456789abcdef", "modern"),
            EntryLookup::Found(DiscoveryCacheEntry {
                negotiated_era: None,
                ..
            })
        ));
    }

    fn sample_tools(n: usize) -> Vec<McpToolDto> {
        (0..n)
            .map(|i| McpToolDto {
                server_name: "srv".into(),
                tool_name: format!("t{i}"),
                description: String::new(),
                input_schema: serde_json::json!({}),
                output_schema: None,
                annotations: None,
                icons: Vec::new(),
                meta: None,
                full_name: format!("mcp__srv__t{i}"),
                search_hint: None,
                always_load: None,
                requires_user_interaction: false,
            })
            .collect()
    }

    /// An entry with `capabilities.tools=true` and 3 tools — non-degenerate,
    /// matching every pre-schema-v2 test's implicit assumption.
    fn entry_at(saved_at_ms: u64) -> DiscoveryCacheEntry {
        DiscoveryCacheEntry::new(
            "k".into(),
            saved_at_ms,
            caps_tools(true),
            sample_tools(3),
            vec![],
            vec![],
            vec![],
        )
    }

    fn policy(
        now_ms: u64,
        ttl_ms: u64,
        max_stale_ms: u64,
        strike_threshold: u32,
    ) -> DiscoveryCachePolicy {
        DiscoveryCachePolicy {
            now_ms,
            ttl_ms,
            max_stale_ms,
            strike_threshold,
        }
    }

    #[test]
    fn decide_gate_reason_wins_before_any_entry_is_consulted() {
        let spec = http_spec("https://x.example", None);
        let d = decide(
            &spec,
            Some(false), // opt-out
            true,
            EntryLookup::Found(entry_at(1_000)),
            policy(1_000, 900_000, 14_400_000, 1),
        );
        assert_eq!(
            d,
            Decision::Miss {
                reason: MissReason::Disabled
            }
        );
    }

    #[test]
    fn decide_absent() {
        let spec = http_spec("https://x.example", None);
        let d = decide(
            &spec,
            None,
            true,
            EntryLookup::Absent,
            policy(1_000, 900_000, 14_400_000, 1),
        );
        assert_eq!(
            d,
            Decision::Miss {
                reason: MissReason::Absent
            }
        );
    }

    #[test]
    fn decide_corrupt() {
        let spec = http_spec("https://x.example", None);
        let d = decide(
            &spec,
            None,
            true,
            EntryLookup::Corrupt,
            policy(1_000, 900_000, 14_400_000, 1),
        );
        assert_eq!(
            d,
            Decision::Miss {
                reason: MissReason::Corrupt
            }
        );
    }

    #[test]
    fn decide_strike_threshold_reached() {
        let spec = http_spec("https://x.example", None);
        let mut entry = entry_at(1_000);
        entry.consecutive_refresh_failures = 1;
        let d = decide(
            &spec,
            None,
            true,
            EntryLookup::Found(entry),
            policy(1_000, 900_000, 14_400_000, 1), // threshold
        );
        assert_eq!(
            d,
            Decision::Miss {
                reason: MissReason::Strike
            }
        );
    }

    #[test]
    fn decide_below_strike_threshold_is_not_a_strike_miss() {
        let spec = http_spec("https://x.example", None);
        let mut entry = entry_at(1_000);
        entry.consecutive_refresh_failures = 1;
        let d = decide(
            &spec,
            None,
            true,
            EntryLookup::Found(entry),
            policy(1_000, 900_000, 14_400_000, 2), // threshold not yet reached
        );
        assert!(matches!(d, Decision::Fresh { .. }));
    }

    #[test]
    fn decide_expired_by_age() {
        let spec = http_spec("https://x.example", None);
        let entry = entry_at(0);
        let d = decide(
            &spec,
            None,
            true,
            EntryLookup::Found(entry),
            policy(15_000, 5_000, 10_000, 1), // age 15s >= 10s max-stale
        );
        assert_eq!(
            d,
            Decision::Miss {
                reason: MissReason::Expired
            }
        );
    }

    #[test]
    fn decide_expired_by_future_clock_skew() {
        let spec = http_spec("https://x.example", None);
        // saved_at is 20s AHEAD of "now" — implausible, treated as expired.
        let entry = entry_at(20_000);
        let d = decide(
            &spec,
            None,
            true,
            EntryLookup::Found(entry),
            policy(1_000, 5_000, 10_000, 1),
        );
        assert_eq!(
            d,
            Decision::Miss {
                reason: MissReason::Expired
            }
        );
    }

    #[test]
    fn decide_fresh_within_ttl() {
        let spec = http_spec("https://x.example", None);
        let entry = entry_at(1_000);
        let d = decide(
            &spec,
            None,
            true,
            EntryLookup::Found(entry.clone()),
            policy(2_000, 5_000, 10_000, 1),
        );
        assert_eq!(
            d,
            Decision::Fresh {
                entry,
                age_ms: 1_000
            }
        );
    }

    #[test]
    fn decide_stale_when_past_ttl_but_within_max_stale() {
        let spec = http_spec("https://x.example", None);
        let entry = entry_at(0);
        let d = decide(
            &spec,
            None,
            true,
            EntryLookup::Found(entry.clone()),
            policy(6_000, 5_000, 10_000, 1),
        );
        assert_eq!(
            d,
            Decision::Stale {
                entry,
                age_ms: 6_000
            }
        );
    }

    #[test]
    fn decide_degenerate_zero_tools_forces_stale_even_within_ttl() {
        let spec = http_spec("https://x.example", None);
        let entry = DiscoveryCacheEntry::new(
            "k".into(),
            1_000,
            caps_tools(true),
            vec![],
            vec![],
            vec![],
            vec![],
        );
        let d = decide(
            &spec,
            None,
            true,
            EntryLookup::Found(entry.clone()),
            policy(1_500, 5_000, 10_000, 1),
        );
        assert_eq!(d, Decision::Stale { entry, age_ms: 500 });
    }

    #[test]
    fn decide_zero_tools_without_the_tools_capability_is_not_degenerate() {
        let spec = http_spec("https://x.example", None);
        // capabilities.tools=false, tools=[]: the server never CLAIMED
        // tools, so an empty list is expected, not degenerate.
        let entry = DiscoveryCacheEntry::new(
            "k".into(),
            1_000,
            caps_tools(false),
            vec![],
            vec![],
            vec![],
            vec![],
        );
        let d = decide(
            &spec,
            None,
            true,
            EntryLookup::Found(entry.clone()),
            policy(1_500, 5_000, 10_000, 1),
        );
        assert_eq!(d, Decision::Fresh { entry, age_ms: 500 });
    }

    #[test]
    fn logged_out_fingerprint_and_partition_hashes_match_fixed_vectors() {
        assert_eq!(PROVIDER_NEUTRAL_IDENTITY_DOMAIN, "acct:logged-out");
        assert_eq!(
            fingerprint("grant:none"),
            "856f0d2375be22a510e79662f22d30c51c14dc3394b9d610af33a7116d81cda6"
        );
        // The partition vector is intentionally version-bound through
        // `platform_api::CLAUDE_CODE_VERSION`, so these two literals move with
        // every raise of it and with nothing else. Last moved 2026-09-10 for
        // 2.1.252 -> 2.1.267; if they move without the constant moving, the
        // partition scheme itself changed and that is the bug to look for.
        assert_eq!(
            partition_key(
                "logical-cache-key",
                "856f0d2375be22a510e79662f22d30c51c14dc3394b9d610af33a7116d81cda6"
            ),
            "c27ba42c0c0386de3ae9674d93fa4765"
        );
        assert_eq!(
            partition_key(
                "logical-cache-key",
                "991e0dadd79d2d72abf31cf52d2cbd1d4f1e0c49b62b4d5e820b2e78cd12f971"
            ),
            "f4e85c666c0b09ee1f312608556a4f5c"
        );
    }

    // ── env parsing ───────────────────────────────────────────────────────

    #[test]
    fn feature_enabled_matrix() {
        let _guard = tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = TestEnvGuard::new();
        assert!(!feature_enabled(), "unset defaults off");
        env.set(ENV_ENABLED, "true");
        assert!(feature_enabled());
        env.set(ENV_ENABLED, "false");
        assert!(!feature_enabled());
        env.set(ENV_ENABLED, "nonsense");
        assert!(!feature_enabled(), "unrecognized value defaults off");
    }

    #[test]
    fn ttl_and_max_stale_defaults() {
        let _guard = tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _env = TestEnvGuard::new();
        assert_eq!(max_stale_ms(), 14_400_000);
        assert_eq!(ttl_ms(), 900_000);
        assert_eq!(strike_threshold(), 1);
    }

    #[test]
    fn test_env_guard_clears_and_restores_all_cache_vars() {
        let _guard = tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _host_restore = TestEnvGuard {
            saved: snapshot_test_env(),
        };
        clear_test_env();
        std::env::set_var(ENV_ENABLED, "true");
        std::env::set_var(ENV_TTL_SECONDS, "17");
        std::env::set_var(ENV_STRIKES, "9");

        {
            let env = TestEnvGuard::new();
            for key in TEST_ENV_KEYS {
                assert!(
                    std::env::var_os(key).is_none(),
                    "{key} must be cleared while the guard is alive"
                );
            }
            env.set(ENV_MAX_STALE_SECONDS, "44");
        }

        assert_eq!(
            std::env::var_os(ENV_ENABLED).as_deref(),
            Some("true".as_ref())
        );
        assert_eq!(
            std::env::var_os(ENV_TTL_SECONDS).as_deref(),
            Some("17".as_ref())
        );
        assert!(std::env::var_os(ENV_MAX_STALE_SECONDS).is_none());
        assert_eq!(std::env::var_os(ENV_STRIKES).as_deref(), Some("9".as_ref()));
    }

    #[test]
    fn max_stale_env_override_is_clamped_to_the_seven_day_ceiling() {
        let _guard = tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = TestEnvGuard::new();
        env.set(ENV_MAX_STALE_SECONDS, "99999999");
        assert_eq!(max_stale_ms(), 604_800_000);
    }

    #[test]
    fn ttl_env_override_is_capped_by_max_stale() {
        let _guard = tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = TestEnvGuard::new();
        env.set(ENV_TTL_SECONDS, "999999");
        env.set(ENV_MAX_STALE_SECONDS, "100");
        assert_eq!(max_stale_ms(), 100_000);
        assert_eq!(ttl_ms(), 100_000, "ttl can never exceed max-stale");
    }

    #[test]
    fn strikes_env_override_and_non_positive_fallback() {
        let _guard = tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = TestEnvGuard::new();
        env.set(ENV_STRIKES, "3");
        assert_eq!(strike_threshold(), 3);
        env.set(ENV_STRIKES, "0");
        assert_eq!(strike_threshold(), 1, "non-positive falls back to default");
    }

    // ── store ─────────────────────────────────────────────────────────────

    #[test]
    fn store_roundtrips_an_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiscoveryCacheStore::new(dir.path());
        let entry = DiscoveryCacheEntry::new(
            "abc123".into(),
            42,
            caps_tools(true),
            sample_tools(5),
            vec![platform_api::McpResourceDto {
                uri: "file:///a".into(),
                name: "a".into(),
                description: None,
                mime_type: None,
                meta: None,
            }],
            vec![platform_api::McpResourceTemplateDto {
                uri_template: "file:///{path}".into(),
                name: "tmpl".into(),
                description: None,
                mime_type: None,
                annotations: Some(serde_json::json!({"vendor/rank": 7})),
                meta: Some(serde_json::json!({"vendor/template": "opaque"})),
            }],
            vec![platform_api::McpPromptDto {
                name: "p".into(),
                description: None,
                arguments: vec![],
            }],
        )
        .with_server_info(DiscoveryCacheServerInfo {
            name: "srv".into(),
            version: "1.0".into(),
        });
        store.store(&entry).expect("store");
        assert_eq!(store.load("abc123"), EntryLookup::Found(entry));
    }

    #[test]
    fn store_load_missing_file_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiscoveryCacheStore::new(dir.path());
        assert_eq!(store.load("nope"), EntryLookup::Absent);
    }

    #[test]
    fn store_load_corrupt_json_is_corrupt_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("k.json"), b"not json").expect("write");
        let store = DiscoveryCacheStore::new(dir.path());
        assert_eq!(store.load("k"), EntryLookup::Corrupt);
    }

    #[test]
    fn store_load_oversize_is_corrupt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let huge = vec![b'a'; (MAX_ENTRY_BYTES + 1) as usize];
        std::fs::write(dir.path().join("k.json"), huge).expect("write");
        let store = DiscoveryCacheStore::new(dir.path());
        assert_eq!(store.load("k"), EntryLookup::Corrupt);
    }

    #[test]
    fn store_load_wrong_schema_version_is_corrupt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiscoveryCacheStore::new(dir.path());
        // Otherwise-complete v2 shape, but a version this store doesn't
        // recognize — must be Corrupt via the explicit version check, not
        // the parse-failure path (unlike the v1-shape test below).
        let bad = serde_json::json!({
            "v": 999,
            "cache_key": "k",
            "saved_at_ms": 1,
            "consecutive_refresh_failures": 0,
            "capabilities": {"tools": false, "resources": false, "prompts": false, "logging": false, "experimental": {}},
            "tools": [],
            "resources": [],
            "resource_templates": [],
            "prompts": [],
        });
        std::fs::write(dir.path().join("k.json"), bad.to_string()).expect("write");
        assert_eq!(store.load("k"), EntryLookup::Corrupt);
    }

    #[test]
    fn store_load_old_v1_shape_is_corrupt() {
        // The pre-schema-v2 on-disk shape: correct `v`, but missing every
        // field schema v2 added. Must be Corrupt — proving the version bump
        // actually invalidates old entries rather than silently defaulting
        // their new fields to something `decide` might trust.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiscoveryCacheStore::new(dir.path());
        let old = serde_json::json!({
            "v": 1,
            "cache_key": "k",
            "saved_at_ms": 1,
            "consecutive_refresh_failures": 0,
            "capabilities_tools": true,
            "tool_count": 3,
        });
        std::fs::write(dir.path().join("k.json"), old.to_string()).expect("write");
        assert_eq!(store.load("k"), EntryLookup::Corrupt);
    }

    #[test]
    fn store_load_missing_directory_read_defaults_false() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiscoveryCacheStore::new(dir.path());
        let entry = serde_json::json!({
            "v": CACHE_SCHEMA_VERSION,
            "cache_key": "k",
            "saved_at_ms": 1,
            "consecutive_refresh_failures": 0,
            "capabilities": {
                "tools": true,
                "resources": false,
                "prompts": false,
                "logging": false,
                "experimental": {}
            },
            "tools": [],
            "resources": [],
            "resource_templates": [],
            "prompts": []
        });
        std::fs::write(dir.path().join("k.json"), entry.to_string()).expect("write");

        let EntryLookup::Found(entry) = store.load("k") else {
            panic!("entry should deserialize")
        };
        assert!(
            !entry.capabilities.directory_read,
            "missing directory_read must default false for old cache entries"
        );
    }

    #[test]
    fn store_load_cache_key_mismatch_is_corrupt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiscoveryCacheStore::new(dir.path());
        // Stored under path "k.json" but its OWN cacheKey field says "other" —
        // simulates the oracle's "entry keyed for another server" case.
        let entry = DiscoveryCacheEntry::new(
            "other".into(),
            1,
            caps_tools(false),
            vec![],
            vec![],
            vec![],
            vec![],
        );
        std::fs::write(
            dir.path().join("k.json"),
            serde_json::to_string(&entry).expect("serialize"),
        )
        .expect("write");
        assert_eq!(store.load("k"), EntryLookup::Corrupt);
    }

    #[test]
    fn partitioned_load_rejects_a_logical_cache_key_mismatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiscoveryCacheStore::new(dir.path());
        let entry = DiscoveryCacheEntry::new(
            "other".into(),
            1,
            caps_tools(false),
            vec![],
            vec![],
            vec![],
            vec![],
        );
        let partition = "5f8c808fb644f988305edbe6275249d1";
        store
            .store_partitioned(&entry, partition)
            .expect("store partitioned");
        assert_eq!(store.load_partitioned("k", partition), EntryLookup::Corrupt);
        assert_eq!(
            store.load_partitioned("other", partition),
            EntryLookup::Found(entry)
        );
    }

    #[cfg(unix)]
    #[test]
    fn store_load_symlink_is_corrupt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real.json");
        let entry = DiscoveryCacheEntry::new(
            "k".into(),
            1,
            caps_tools(false),
            vec![],
            vec![],
            vec![],
            vec![],
        );
        std::fs::write(&real, serde_json::to_string(&entry).expect("serialize")).expect("write");
        std::os::unix::fs::symlink(&real, dir.path().join("k.json")).expect("symlink");
        let store = DiscoveryCacheStore::new(dir.path());
        assert_eq!(store.load("k"), EntryLookup::Corrupt);
    }

    #[test]
    fn purge_of_a_missing_entry_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiscoveryCacheStore::new(dir.path());
        store.purge("nope").expect("purge of absent entry is Ok");
    }

    #[test]
    fn purge_removes_a_stored_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiscoveryCacheStore::new(dir.path());
        let entry = DiscoveryCacheEntry::new(
            "abc".into(),
            1,
            caps_tools(false),
            vec![],
            vec![],
            vec![],
            vec![],
        );
        store.store(&entry).expect("store");
        store.purge("abc").expect("purge");
        assert_eq!(store.load("abc"), EntryLookup::Absent);
    }

    #[test]
    fn purge_family_removes_all_identity_partitions_without_following_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = DiscoveryCacheStore::new(dir.path());
        let entry = DiscoveryCacheEntry::new(
            "family".into(),
            1,
            caps_tools(false),
            vec![],
            vec![],
            vec![],
            vec![],
        );
        store
            .store_partitioned(&entry, "6decc52ef8fe06ece487830919fa7647")
            .expect("store partition 1");
        store
            .store_partitioned(&entry, "b1a568969e63c74880dad66366c53bde")
            .expect("store partition 2");
        store
            .store(&DiscoveryCacheEntry::new(
                "family".into(),
                2,
                caps_tools(false),
                vec![],
                vec![],
                vec![],
                vec![],
            ))
            .expect("store legacy");
        store
            .store(&DiscoveryCacheEntry::new(
                "other".into(),
                3,
                caps_tools(false),
                vec![],
                vec![],
                vec![],
                vec![],
            ))
            .expect("store other");
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            dir.path().join("other.json"),
            dir.path().join("family.symlink.json"),
        )
        .expect("symlink");

        store.purge_family("family").expect("purge family");

        assert_eq!(store.load("family"), EntryLookup::Absent);
        assert_eq!(
            store.load_partitioned("family", "6decc52ef8fe06ece487830919fa7647"),
            EntryLookup::Absent
        );
        assert_eq!(
            store.load_partitioned("family", "b1a568969e63c74880dad66366c53bde"),
            EntryLookup::Absent
        );
        assert!(matches!(store.load("other"), EntryLookup::Found(_)));
        #[cfg(unix)]
        assert!(
            dir.path().join("family.symlink.json").exists(),
            "purge_family must not follow or delete symlink entries"
        );
    }

    #[test]
    fn concurrent_same_key_stores_leave_a_valid_entry_without_temp_collisions() {
        use std::collections::BTreeSet;
        use std::sync::{Arc, Barrier, Mutex};

        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(DiscoveryCacheStore::new(dir.path()));
        let writers = 8usize;
        let start = Arc::new(Barrier::new(writers));
        let observed_paths = Arc::new(Mutex::new(Vec::with_capacity(writers)));
        let _observer_guard = StagePathObserverGuard::install(dir.path().to_path_buf(), {
            let observed_paths = observed_paths.clone();
            Arc::new(move |path: &std::path::Path| {
                observed_paths.lock().unwrap().push(path.to_path_buf());
            })
        });

        let handles: Vec<_> = (0..writers)
            .map(|i| {
                let store = store.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    let entry = DiscoveryCacheEntry::new(
                        "same".into(),
                        i as u64,
                        caps_tools(true),
                        sample_tools(i + 1),
                        vec![],
                        vec![],
                        vec![],
                    );
                    start.wait();
                    store.store(&entry).expect("concurrent store");
                    entry
                })
            })
            .collect();

        let written: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("writer thread"))
            .collect();
        let staged_paths: BTreeSet<_> = observed_paths.lock().unwrap().iter().cloned().collect();
        assert_eq!(
            staged_paths.len(),
            writers,
            "each same-key writer must use a distinct staging path"
        );

        let loaded = match store.load("same") {
            EntryLookup::Found(entry) => entry,
            other => panic!("expected a valid entry, got {other:?}"),
        };
        assert!(
            written.iter().any(|entry| entry == &loaded),
            "final entry must deserialize as one complete writer payload"
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .map(|entry| entry.expect("dir entry").path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("same.tmp-"))
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "successful stores must not leave staging files behind: {leftovers:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn store_sets_owner_only_permissions_on_root_and_entry_files() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("cache");
        let store = DiscoveryCacheStore::new(&root);
        let entry = DiscoveryCacheEntry::new(
            "abc".into(),
            1,
            caps_tools(false),
            vec![],
            vec![],
            vec![],
            vec![],
        );

        store.store(&entry).expect("store");

        let root_mode = std::fs::metadata(&root)
            .expect("root metadata")
            .permissions()
            .mode()
            & 0o777;
        let entry_mode = std::fs::metadata(root.join("abc.json"))
            .expect("entry metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(root_mode, 0o700);
        assert_eq!(entry_mode, 0o600);
    }

    #[test]
    fn cache_key_is_stable_and_distinguishes_url() {
        let a = DiscoveryCacheStore::cache_key("srv", &http_spec("https://a.example", None));
        let b = DiscoveryCacheStore::cache_key("srv", &http_spec("https://b.example", None));
        let a_again = DiscoveryCacheStore::cache_key("srv", &http_spec("https://a.example", None));
        assert_eq!(a, a_again);
        assert_ne!(a, b);
    }

    #[test]
    fn logical_cache_key_ignores_scope_config_error_and_discovery_cache_but_tracks_timeout_and_always_load(
    ) {
        let mut base = crate::connection::McpServerConfig {
            name: "srv".into(),
            spec: http_spec("https://a.example", None),
            scope: crate::connection::ConfigScope::Settings(protocol::SettingsScope::User),
            disabled: false,
            timeout_ms: Some(10),
            discovery_cache: None,
            always_load: false,
            tools: Vec::new(),
            tool_permissions: std::collections::BTreeMap::new(),
            config_error: None,
            metadata: crate::connection::McpServerMetadata::default(),
        };
        let same = crate::connection::McpServerConfig {
            scope: crate::connection::ConfigScope::Settings(protocol::SettingsScope::Managed),
            config_error: Some("ignored".into()),
            ..base.clone()
        };
        let different_discovery_cache = crate::connection::McpServerConfig {
            discovery_cache: Some(false),
            ..base.clone()
        };
        let different_timeout = crate::connection::McpServerConfig {
            timeout_ms: Some(20),
            ..base.clone()
        };
        let different_always_load = crate::connection::McpServerConfig {
            always_load: true,
            discovery_cache: None,
            ..base.clone()
        };

        let base_key = logical_cache_key(&base);
        assert_eq!(base_key, "srv-3a9ea8118cd8b809");
        assert_eq!(base_key, logical_cache_key(&same));
        assert_eq!(base_key, logical_cache_key(&different_discovery_cache));
        assert_ne!(base_key, logical_cache_key(&different_timeout));
        assert_ne!(base_key, logical_cache_key(&different_always_load));

        if let McpTransportSpec::Http { headers_helper, .. } = &mut base.spec {
            *headers_helper = Some("./helper".into());
        }
        assert_ne!(base_key, logical_cache_key(&base));
    }

    #[test]
    fn logical_cache_key_omits_absent_oauth_fields_and_tracks_present_fields() {
        let mut empty_oauth = crate::connection::McpServerConfig {
            name: "srv".into(),
            spec: http_spec("https://a.example", None),
            scope: crate::connection::ConfigScope::Settings(protocol::SettingsScope::User),
            disabled: false,
            timeout_ms: Some(10),
            discovery_cache: None,
            always_load: false,
            tools: Vec::new(),
            tool_permissions: std::collections::BTreeMap::new(),
            config_error: None,
            metadata: crate::connection::McpServerMetadata::default(),
        };
        let McpTransportSpec::Http { oauth, .. } = &mut empty_oauth.spec else {
            unreachable!()
        };
        *oauth = Some(platform_api::McpOAuthConfigDto {
            client_id: None,
            callback_port: None,
            auth_server_metadata_url: None,
            scopes: None,
            xaa: None,
        });
        assert_eq!(logical_cache_key(&empty_oauth), "srv-3e065924e4160070");

        let mut partial_oauth = empty_oauth.clone();
        let McpTransportSpec::Http { oauth, .. } = &mut partial_oauth.spec else {
            unreachable!()
        };
        oauth.as_mut().expect("oauth config").client_id = Some("client".into());
        assert_eq!(logical_cache_key(&partial_oauth), "srv-95bfe547b37316e7");
    }

    #[test]
    fn logical_cache_key_separates_all_agent_sources_for_same_server_and_spec() {
        use std::collections::HashSet;

        let base = crate::connection::McpServerConfig {
            name: "same-agent-server".into(),
            spec: http_spec("https://a.example", None),
            scope: crate::connection::ConfigScope::Agent,
            disabled: false,
            timeout_ms: None,
            discovery_cache: None,
            always_load: false,
            tools: Vec::new(),
            tool_permissions: Default::default(),
            config_error: None,
            metadata: crate::connection::McpServerMetadata::default(),
        };
        let sources = [
            crate::connection::McpAgentSource::BuiltIn,
            crate::connection::McpAgentSource::Plugin,
            crate::connection::McpAgentSource::UserSettings,
            crate::connection::McpAgentSource::ProjectSettings,
            crate::connection::McpAgentSource::PolicySettings,
            crate::connection::McpAgentSource::FlagSettings,
            crate::connection::McpAgentSource::AdditionalDirectory,
        ];
        let keys: HashSet<_> = sources
            .into_iter()
            .map(|source| {
                let mut config = base.clone();
                config.metadata.agent_source = Some(source);
                logical_cache_key(&config)
            })
            .collect();
        assert_eq!(keys.len(), sources.len());
    }
}

#[cfg(test)]
mod url_identity_tests {
    use super::canonical_server_url;

    /// 2.1.269 — a config edit that only reorders query parameters must NOT
    /// change the reconnect identity, or the server is torn down and
    /// reconnected for a config that did not actually change.
    #[test]
    fn query_parameter_order_does_not_change_the_identity() {
        assert_eq!(
            canonical_server_url("https://h.test/mcp?b=2&a=1"),
            canonical_server_url("https://h.test/mcp?a=1&b=2")
        );
    }

    /// Everything that genuinely identifies a different endpoint must still
    /// separate — otherwise the fix would fuse distinct servers into one.
    #[test]
    fn anything_other_than_order_still_separates() {
        let base = canonical_server_url("https://h.test/mcp?a=1&b=2");
        for other in [
            "https://h.test/mcp?a=1&b=3",      // different value
            "https://h.test/mcp?a=1&c=2",      // different key
            "https://h.test/mcp?a=1",          // dropped param
            "https://h.test/other?a=1&b=2",    // different path
            "https://other.test/mcp?a=1&b=2",  // different host
            "http://h.test/mcp?a=1&b=2",       // different scheme
            "https://h.test:8443/mcp?a=1&b=2", // different port
            "https://h.test/mcp?a=1&b=2#frag", // different fragment
        ] {
            assert_ne!(base, canonical_server_url(other), "{other} must differ");
        }
    }

    /// A repeated key keeps its relative order — a server may treat
    /// `?x=1&x=2` differently from `?x=2&x=1`.
    #[test]
    fn repeated_keys_keep_their_relative_order() {
        assert_ne!(
            canonical_server_url("https://h.test/mcp?x=1&x=2"),
            canonical_server_url("https://h.test/mcp?x=2&x=1")
        );
    }

    /// No query, or an unparseable URL, passes through untouched rather than
    /// being guessed at.
    #[test]
    fn urls_without_a_query_and_unparseable_urls_pass_through() {
        assert_eq!(
            canonical_server_url("https://h.test/mcp"),
            "https://h.test/mcp"
        );
        assert_eq!(canonical_server_url("not a url"), "not a url");
        assert_eq!(canonical_server_url(""), "");
    }
}
