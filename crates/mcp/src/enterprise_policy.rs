//! Enterprise MCP policy — a byte-faithful port of claude-code 2.1.206's
//! managed-MCP gates.
//!
//! When an organization ships a managed MCP configuration, `claude mcp add`
//! runs three checks (in `TPe`/`addMcpServer`, after the reserved-name check
//! and before any scope write):
//!
//! 1. **`D1()`** — is enterprise MCP configuration *active*? If a managed
//!    `managed-mcp.json` exists and parses, the enterprise config has exclusive
//!    control and every add is refused.
//! 2. **`bPe(name, config)`** — is the server *explicitly denied* by policy?
//! 3. **`gPe(name, config)`** — is the server *allowed* by policy?
//!
//! This module ports the substrate. Stage 1 (this file) implements the managed
//! path resolution and `D1()`; the `bPe`/`gPe` allow/deny matchers land in a
//! follow-up so each step stays non-divergent (`D1` is checked first, so when
//! it fires the matchers never run).

use std::path::{Path, PathBuf};

/// Env override relocating the managed (policy) settings root — the same
/// variable the engine's `managed_settings_dir` honors, so a test (or an
/// unusual deployment) can point both at one directory. Unset in production.
pub const MANAGED_DIR_ENV: &str = "LINGXI_MANAGED_DIR";

/// The OS-specific managed settings root — claude-code's `XM()`
/// (`getManagedFilePath`), LingXi-branded (see [`branding`]). Honors
/// [`MANAGED_DIR_ENV`] first (test/relocation), else the hardcoded OS path.
#[must_use]
pub fn managed_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(MANAGED_DIR_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    if cfg!(target_os = "macos") {
        PathBuf::from(branding::MANAGED_DIR_MACOS)
    } else if cfg!(target_os = "windows") {
        PathBuf::from(branding::MANAGED_DIR_WINDOWS)
    } else {
        PathBuf::from(branding::MANAGED_DIR_UNIX)
    }
}

/// claude-code `qFr()` — the managed MCP config path,
/// `<managed_dir>/managed-mcp.json`.
#[must_use]
pub fn managed_mcp_config_path() -> PathBuf {
    managed_dir().join("managed-mcp.json")
}

/// claude-code `D1()` — is enterprise MCP configuration active?
///
/// `D1` memoizes `$7t({filePath: qFr(), scope:"enterprise"}).config !== null`:
/// the managed MCP config file is read and parsed, and the config is non-null
/// exactly when it is a regular file within the size limit holding a valid JSON
/// object. An absent, empty, oversized, or malformed file → not active.
#[must_use]
pub fn enterprise_mcp_active() -> bool {
    enterprise_mcp_active_at(&managed_mcp_config_path())
}

/// claude-code's byte-exact `mcp add` rejection when [`enterprise_mcp_active`].
pub const ENTERPRISE_EXCLUSIVE_CONTROL_MESSAGE: &str =
    "Cannot add MCP server: enterprise MCP configuration is active and has exclusive control over MCP servers";

/// The [`enterprise_mcp_active`] core, parameterized on the config path so it is
/// testable without touching the process-global managed dir / env.
#[must_use]
pub fn enterprise_mcp_active_at(path: &Path) -> bool {
    // claude's `$7t` IS `Iqe` at `scope:"enterprise"` (`zit`'s
    // `case"enterprise"` calls `Iqe({filePath:J$t(),expandVars:t,
    // scope:"enterprise"})`, 2.1.251 @160901500), so the read goes through
    // `Atr(d,t,mcn)` — `if(!o.isFile()||o.size>r)return null`, @156004982 —
    // and is REFUSED for a non-regular file (a FIFO would otherwise block
    // boot forever waiting for a writer) or one over 2 MiB. A refused or
    // failed read (ENOENT / not-a-file / oversized / unreadable) is the same
    // not-active signal.
    let Ok(raw) = crate::config_diagnostics::read_mcp_config_file(path, ConfigScope::Enterprise)
    else {
        return false;
    };
    if raw.trim().is_empty() {
        return false;
    }
    // `config !== null` ⟺ the file parsed to a JSON object. Malformed JSON or a
    // non-object top level yields a null config (not active).
    matches!(
        serde_json::from_str::<serde_json::Value>(&raw),
        Ok(serde_json::Value::Object(_))
    )
}

// ────────────────────────────────────────────────────────────────────────────
// Stage 2 — the `bPe`/`edt` (deny) / `gPe`/`ZFe` (allow) policy matchers.
//
// claude reads the allow/deny lists from the *effective merged* settings
// (`Je()`) via [`McpPolicy::from_effective_settings`] — see
// [`allow_managed_mcp_servers_only`] just below for the `allowManagedMcpServersOnly`
// gate that decides whether `allowedMcpServers` reads from every tier (like
// `deniedMcpServers` always does) or from managed policy alone.
//
// [`read_managed_mcp_policy`]/[`read_managed_mcp_policy_in`] read ONLY the
// managed policy tiers (`managed-settings.json` + `managed-settings.d`) —
// a narrower, managed-tiers-only view kept for the one remaining call site
// that has no ordinary-tier sources to hand it (the agent-frontmatter MCP
// merge). A deny/allow list placed in a *personal* user-tier settings file is
// not consulted by that narrow reader.
//
// Since 2.1.219 the POLICY side of every predicate expands against a dedicated
// policy expansion env (frozen startup snapshot + managed-source env — see the
// `PolicyExpansionEnv` section below), never the ambient process env, and
// allowlist URL entries whose expansion is UNSAFE fail closed (`dWu`).
// ────────────────────────────────────────────────────────────────────────────

use serde::Deserialize;
use serde_json::Value;

/// One allow/deny matcher entry (claude's `allowedMcpServers` /
/// `deniedMcpServers` items). Any subset of the three keys may be present; the
/// shape predicates `a3t`/`ZCn`/`ewn` test which is set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerMatcher {
    /// `a3t`: match by server name.
    #[serde(default)]
    pub server_name: Option<String>,
    /// `ZCn`: match by the exact `[command, ...args]` array (env-expanded).
    #[serde(default)]
    pub server_command: Option<Vec<String>>,
    /// `ewn`: match by URL (via the `UFr` glob).
    #[serde(default)]
    pub server_url: Option<String>,
}

/// The enterprise MCP allow/deny policy (`deniedMcpServers` / `allowedMcpServers`).
#[derive(Debug, Clone, Default)]
pub struct McpPolicy {
    /// claude `deniedMcpServers` (absent ⇒ nothing denied).
    pub denied: Option<Vec<McpServerMatcher>>,
    /// claude `allowedMcpServers` (absent ⇒ everything allowed).
    pub allowed: Option<Vec<McpServerMatcher>>,
}

/// Fold `key`'s matcher-list entries across every source that DEFINES it,
/// accumulating de-duplicated matchers into one list — claude's "merges from
/// all sources" arrays. This is `deniedMcpServers`'s permanent shape (`pAn()`
/// always reads the full merged effective settings) and is also
/// `allowedMcpServers`'s shape whenever [`allow_managed_mcp_servers_only`] is
/// false (`uAn()` then also returns the full merged effective settings,
/// `Je()`, not the managed-only slice).
///
/// Returns `(field present in any source, matchers)`; an entry failing the
/// zod-mirrored per-element validation ([`parse_matcher_entry`]) is dropped,
/// same as [`read_managed_mcp_policy_in`].
fn accumulate_matcher_list<'a>(
    sources: impl Iterator<Item = &'a Value>,
    key: &str,
    kind: MatcherListKind,
) -> (bool, Vec<McpServerMatcher>) {
    let mut out = Vec::new();
    let mut present = false;
    for source in sources {
        let Some(raw) = source.get(key) else {
            continue;
        };
        present = true;
        let Some(entries) = raw.as_array() else {
            continue;
        };
        for entry in entries {
            if let Ok(matcher) = parse_matcher_entry(entry, kind) {
                if !out.contains(&matcher) {
                    out.push(matcher);
                }
            }
        }
    }
    (present, out)
}

/// claude's byte-exact warning when `allowManagedMcpServersOnly` is present
/// but is not a boolean (@154802763). Unlike the two list warnings below, an
/// invalid value here fails closed to the MORE restrictive managed-only
/// reading — `.catch(() => true)` — never to unrestricted.
const ALLOW_MANAGED_MCP_SERVERS_ONLY_INVALID: &str = "\"allowManagedMcpServersOnly\" was present but invalid; treating it as true until it is fixed.";

/// claude's `ghr()` (@160901593): `ye("policySettings")?.allowManagedMcpServersOnly===!0`.
/// Only the managed/policy settings tiers participate — a user- or
/// project-scope `allowManagedMcpServersOnly` cannot opt itself into (or out
/// of) the managed-only reading. Managed tiers are ordered low → high, same
/// as every other managed-settings field (a later tier's value replaces an
/// earlier one wholesale, it does not merge); a present non-boolean value
/// fails closed to `true`.
fn allow_managed_mcp_servers_only(managed: &[Value]) -> bool {
    let mut raw = None;
    for source in managed {
        if let Some(v) = source.get("allowManagedMcpServersOnly") {
            raw = Some(v);
        }
    }
    match raw {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            tracing::warn!("allowManagedMcpServersOnly: {ALLOW_MANAGED_MCP_SERVERS_ONLY_INVALID}");
            true
        }
    }
}

impl McpPolicy {
    /// Compose the effective policy from ordinary settings sources and managed
    /// sources — claude's `uAn()`/`pAn()` pair, gated on `ghr()`
    /// (`allowManagedMcpServersOnly`).
    ///
    /// `deniedMcpServers` ALWAYS accumulates across every source (ordinary
    /// then managed) and always wins — deny is never policy-locked, so users
    /// can deny servers for themselves even under managed-only mode (the
    /// setting's own description: *"deniedMcpServers still merges from all
    /// sources, so users can deny servers for themselves"*).
    ///
    /// `allowedMcpServers` depends on [`allow_managed_mcp_servers_only`]:
    /// - **true** (or an invalid value, failing closed) — allow entries are
    ///   accepted ONLY from managed policy (`ye("policySettings")??{}`); a
    ///   later managed tier that supplies the field replaces the previous
    ///   allowlist wholesale, and an invalid list value fails closed as an
    ///   empty allowlist (nothing admitted).
    /// - **false or absent** (the default) — `allowedMcpServers` merges from
    ///   EVERY settings tier like `deniedMcpServers` does (`Je()`, the full
    ///   merged effective settings) — a user- or project-scope allowlist is
    ///   honoured exactly like the oracle's default (unlocked) behaviour.
    #[must_use]
    pub fn from_effective_settings(ordinary: &[Value], managed: &[Value]) -> Self {
        let (denied_present, denied) = accumulate_matcher_list(
            ordinary.iter().chain(managed),
            "deniedMcpServers",
            MatcherListKind::Denied,
        );

        let allowed = if allow_managed_mcp_servers_only(managed) {
            // ye("policySettings")??{} — the single merged managed/policy
            // settings object; a later managed tier that supplies the field
            // replaces the previous allowlist wholesale (Object.assign-style
            // per key, not accumulated), and an invalid value fails closed as
            // an empty allowlist.
            let mut allowed = None;
            for source in managed {
                let Some(raw) = source.get("allowedMcpServers") else {
                    continue;
                };
                allowed = Some(
                    raw.as_array()
                        .map(|entries| {
                            entries
                                .iter()
                                .filter_map(|entry| {
                                    parse_matcher_entry(entry, MatcherListKind::Allowed).ok()
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                );
            }
            allowed
        } else {
            // Je() — not policy-locked, so allow merges from every tier
            // exactly like deny does.
            let (present, list) = accumulate_matcher_list(
                ordinary.iter().chain(managed),
                "allowedMcpServers",
                MatcherListKind::Allowed,
            );
            present.then_some(list)
        };

        Self {
            denied: denied_present.then_some(denied),
            allowed,
        }
    }
}

/// claude's byte-exact `mcp add` rejection when a server is denied (`bPe`).
#[must_use]
pub fn denied_message(name: &str) -> String {
    format!("Cannot add MCP server \"{name}\": server is explicitly blocked by enterprise policy")
}

/// claude's byte-exact `mcp add` rejection when a server is not allowed (`gPe`).
#[must_use]
pub fn not_allowed_message(name: &str) -> String {
    format!("Cannot add MCP server \"{name}\": not allowed by enterprise policy")
}

/// Read the enterprise MCP policy from the managed settings tiers, folded in
/// ascending priority (`managed-settings.json` base, then `managed-settings.d/`
/// `*.json` sorted, later wins) — the same tier order as the engine's
/// `managed_settings_raw_tiers`.
#[must_use]
pub fn read_managed_mcp_policy() -> McpPolicy {
    read_managed_mcp_policy_in(&managed_dir())
}

/// Compose MCP policy from the standard user/project/local settings paths, an
/// optional `--settings` value, and the managed tiers. The returned policy is
/// shared by CLI commands and engine boot.
#[must_use]
pub fn read_effective_mcp_policy(
    lingxi_home: &Path,
    cwd: &Path,
    flag_settings: Option<&Value>,
) -> McpPolicy {
    let mut ordinary = Vec::new();
    for path in [
        lingxi_home.join("settings.json"),
        cwd.join(branding::DOT_DIR).join("settings.json"),
        cwd.join(branding::DOT_DIR).join("settings.local.json"),
    ] {
        if let Ok(raw) = std::fs::read_to_string(path) {
            if let Ok(value) = serde_json::from_str(&raw) {
                ordinary.push(value);
            }
        }
    }
    if let Some(flag) = flag_settings {
        ordinary.push(flag.clone());
    }
    let mut managed = Vec::new();
    for_each_managed_settings_tier(&managed_dir(), &mut |source| {
        managed.push(Value::Object(source.clone()));
    });
    McpPolicy::from_effective_settings(&ordinary, &managed)
}

/// Walk the managed settings tiers in ascending priority
/// (`managed-settings.json` base, then `managed-settings.d/` `*.json` sorted,
/// later wins), calling `f` with each tier's top-level JSON object. Shared by
/// the policy reader and the policy-expansion env fold.
fn for_each_managed_settings_tier(dir: &Path, f: &mut dyn FnMut(&serde_json::Map<String, Value>)) {
    let mut fold = |raw: &str| {
        if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(raw) {
            f(&m);
        }
    };
    if let Ok(raw) = std::fs::read_to_string(dir.join("managed-settings.json")) {
        fold(&raw);
    }
    let drop_in = dir.join("managed-settings.d");
    if let Ok(rd) = std::fs::read_dir(&drop_in) {
        let mut names: Vec<std::ffi::OsString> = rd
            .flatten()
            .map(|e| e.file_name())
            .filter(|n| {
                let s = n.to_string_lossy();
                s.ends_with(".json") && !s.starts_with('.')
            })
            .collect();
        names.sort();
        for name in names {
            if let Ok(raw) = std::fs::read_to_string(drop_in.join(&name)) {
                fold(&raw);
            }
        }
    }
}

/// Which policy list a matcher entry came from — the two lists validate
/// `serverName` differently (claude's `gqn` allowed-entry vs `_qn`
/// denied-entry zod schemas).
#[derive(Clone, Copy, PartialEq, Eq)]
enum MatcherListKind {
    Allowed,
    Denied,
}

/// claude's byte-exact warning when `allowedMcpServers` is present but is not
/// an array (`RLi`'s array-level `.catch`). The fallback is `[]`, and an EMPTY
/// allowlist admits nothing (`ZFe`: `length===0 ⇒ false`) — this side fails
/// CLOSED.
const ALLOWED_LIST_INVALID: &str = "\"allowedMcpServers\" was present but invalid; enforcing an empty allowlist (no MCP servers admitted) until it is fixed.";

/// claude's byte-exact warning when `deniedMcpServers` is present but is not an
/// array. `RLi`'s catch returns `undefined` here, so the denylist is simply not
/// enforced (no fail-closed counterpart — a bad denylist cannot be inverted
/// into "deny everything" without locking the user out).
const DENIED_LIST_INVALID: &str = "\"deniedMcpServers\" was present but invalid and was dropped; its entries cannot be enforced until it is fixed.";

/// zod v4's `Mjm` (@225600556) — the `received` half of an `invalid_type`
/// message. (JS-only outcomes — `NaN`, class names — cannot arise from JSON.)
fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// zod v4's default `invalid_type` message (@225601574).
fn invalid_type(expected: &str, got: &Value) -> String {
    format!(
        "Invalid input: expected {expected}, received {}",
        json_type_name(got)
    )
}

/// The `serverName` refinements of claude `gqn` (allowed) / `_qn` (denied),
/// byte-exact messages. `None` ⇒ valid.
fn server_name_error(name: &str, kind: MatcherListKind) -> Option<&'static str> {
    match kind {
        MatcherListKind::Allowed => {
            // gqn: `/^[a-zA-Z0-9_-]+$/`.
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Some(
                    "Server name can only contain letters, numbers, hyphens, and underscores",
                );
            }
        }
        MatcherListKind::Denied => {
            if name.is_empty() {
                return Some("Server name must be non-empty");
            }
            if name.trim().is_empty() {
                return Some("Server name must not be whitespace-only");
            }
            if name != name.trim() {
                return Some(
                    "Server name has leading or trailing whitespace and will never match (names are compared verbatim)",
                );
            }
        }
    }
    None
}

/// Validate ONE raw allow/deny list element against claude `gqn`/`_qn`,
/// returning the message of the FIRST zod issue on failure — what `Xql`'s
/// element-level `.catch` reports.
///
/// The element is parsed from the raw [`Value`] rather than through serde
/// because the two halves of `gqn`/`_qn` are inseparable in zod: the TYPE
/// checks (`serverName: E.string()`, `serverCommand: E.array(E.string())`) and
/// the refinements live in one schema behind one `.catch`, so a type error must
/// drop only its own element. Deserializing the list as a `Vec` first made a
/// single mistyped entry collapse the WHOLE list to `None` — i.e. fail OPEN.
/// Checks run in zod's shape order (`serverName`, `serverCommand`, `serverUrl`,
/// then the object-level refine), each field's type check before its own
/// refinements, so the reported message is `issues[0]`.
///
/// `.optional()` is `undefined | T`: a key present with a JSON `null` is a type
/// error, NOT an absent field.
fn parse_matcher_entry(raw: &Value, kind: MatcherListKind) -> Result<McpServerMatcher, String> {
    let Some(obj) = raw.as_object() else {
        return Err(invalid_type("object", raw));
    };
    let mut m = McpServerMatcher::default();
    if let Some(v) = obj.get("serverName") {
        let Some(name) = v.as_str() else {
            return Err(invalid_type("string", v));
        };
        if let Some(msg) = server_name_error(name, kind) {
            return Err(msg.to_string());
        }
        m.server_name = Some(name.to_string());
    }
    if let Some(v) = obj.get("serverCommand") {
        let Some(items) = v.as_array() else {
            return Err(invalid_type("array", v));
        };
        let mut cmd = Vec::with_capacity(items.len());
        for item in items {
            let Some(s) = item.as_str() else {
                return Err(invalid_type("string", item));
            };
            cmd.push(s.to_string());
        }
        if cmd.is_empty() {
            return Err("Server command must have at least one element (the command)".to_string());
        }
        m.server_command = Some(cmd);
    }
    if let Some(v) = obj.get("serverUrl") {
        let Some(url) = v.as_str() else {
            return Err(invalid_type("string", v));
        };
        m.server_url = Some(url.to_string());
    }
    let present = [
        m.server_name.is_some(),
        m.server_command.is_some(),
        m.server_url.is_some(),
    ];
    if present.iter().filter(|p| **p).count() != 1 {
        return Err(
            "Entry must have exactly one of \"serverName\", \"serverCommand\", or \"serverUrl\""
                .to_string(),
        );
    }
    Ok(m)
}

/// [`read_managed_mcp_policy`] rooted at an explicit dir (testable). Entries
/// failing the zod-mirrored validation ([`parse_matcher_entry`]) are dropped
/// with a warning — they never participate in matching, exactly as an entry the
/// oracle's typed settings never contained.
#[must_use]
pub fn read_managed_mcp_policy_in(dir: &Path) -> McpPolicy {
    let mut merged = serde_json::Map::new();
    for_each_managed_settings_tier(dir, &mut |m| {
        for (k, v) in m {
            merged.insert(k.clone(), v.clone());
        }
    });
    // claude `Xql(key, elementSchema, warn)`: the `.catch` sits on the ELEMENT
    // schema and substitutes the `Jql` sentinel, which a `.transform` then
    // filters out — one malformed entry drops itself and its valid siblings
    // survive. Only a non-ARRAY value escapes to `RLi`'s outer `.catch`, whose
    // two sides are deliberately ASYMMETRIC (see the two consts above).
    let parse = |key: &str, kind: MatcherListKind| -> Option<Vec<McpServerMatcher>> {
        let raw = merged.get(key)?; // absent ⇒ `.optional()` ⇒ undefined
        let Some(items) = raw.as_array() else {
            return match kind {
                MatcherListKind::Allowed => {
                    tracing::warn!("{key}: {ALLOWED_LIST_INVALID}");
                    Some(Vec::new())
                }
                MatcherListKind::Denied => {
                    tracing::warn!("{key}: {DENIED_LIST_INVALID}");
                    None
                }
            };
        };
        Some(
            items
                .iter()
                .filter_map(|item| match parse_matcher_entry(item, kind) {
                    Ok(m) => Some(m),
                    Err(msg) => {
                        tracing::warn!("{key}[]: Invalid entry was ignored: {msg}");
                        None
                    }
                })
                .collect(),
        )
    };
    McpPolicy {
        denied: parse("deniedMcpServers", MatcherListKind::Denied),
        allowed: parse("allowedMcpServers", MatcherListKind::Allowed),
    }
}

/// claude `byo` — expand `${VAR}` / `${VAR:-default}` against the LIVE
/// process environment. This is the CANDIDATE-config side only; POLICY
/// strings expand against [`PolicyExpansionEnv`] (see below), never the live
/// env.
fn get(s: &str) -> String {
    crate::env_expansion::expand_env_vars_in_string(s).expanded
}

/// Prime the frozen startup env snapshot ([`crate::env_expansion::startup_env_snapshot`])
/// before anything can apply a settings-file `env` to the process environment.
///
/// The oracle gets this ordering for free because `Dut()` calls `NQr()` as its
/// first statement. Here it is explicit, so the composition root calls it from
/// its boot path. Calling it late is not fatal — the snapshot is simply taken
/// then — but the freeze is the whole point, so call it early.
pub fn prime_startup_env() {
    let _ = crate::env_expansion::startup_env_snapshot();
}

// ────────────────────────────────────────────────────────────────────────────
// Policy expansion environment (claude `NQr`/`cWu`/`U__`/`uWu`, 2.1.219+).
//
// Policy predicates (`serverCommand` items, `serverUrl` patterns) do NOT
// expand against the ambient process env: the oracle expands them with the
// frozen STARTUP env snapshot overlaid by the managed settings sources' own
// `env` blocks (`cWu`), so a settings-file `env` entry from a lower-trust
// scope can never satisfy — or dodge — an enterprise predicate. The DENY side
// additionally gets a fallback env assembled from the global config,
// userSettings, flagSettings, and policySettings `env` blocks (`U__`), letting
// denylist entries keep matching values a user configured anywhere. (The
// `--settings` flagSettings tier is not plumbed into this crate; its slot in
// the fallback fold is documented here and intentionally empty.)
// ────────────────────────────────────────────────────────────────────────────

use indexmap::IndexMap;
use std::sync::OnceLock;

/// Process-start `--settings` env snapshot. The CLI installs this before the
/// MCP policy is evaluated; it is set-once because flag settings are immutable
/// for the lifetime of one CLI process.
static FLAG_SETTINGS_ENV: OnceLock<IndexMap<String, String>> = OnceLock::new();
static FLAG_SETTINGS_POLICY: OnceLock<Value> = OnceLock::new();

/// Freeze the parsed `--settings` policy layer for CLI subcommands that do not
/// construct a desktop runtime.
pub fn install_flag_settings_policy(settings: Value) -> bool {
    FLAG_SETTINGS_POLICY.set(settings).is_ok()
}

/// Return the frozen `--settings` policy layer, when present.
#[must_use]
pub fn flag_settings_policy() -> Option<&'static Value> {
    FLAG_SETTINGS_POLICY.get()
}

/// One settings source's `env` block → string (key, value) pairs — claude
/// `ELt`: only string values participate and `NO_COLOR`/`FORCE_COLOR` are
/// stripped. (The oracle additionally filters host-orchestration control keys
/// (`aqu`…`cqu`) guarding hosted deployments this port does not model.)
fn settings_env_pairs(env: &Value) -> Vec<(String, String)> {
    let Value::Object(m) = env else {
        return Vec::new();
    };
    m.iter()
        .filter(|(k, _)| k.as_str() != "NO_COLOR" && k.as_str() != "FORCE_COLOR")
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
        .collect()
}

/// claude `cWu`'s settings-source env fold (the `for t of VQ()` loop) over the
/// managed tiers. `VQ()` folds first-seen-wins over DESCENDING priority; our
/// tier walk is ascending (drop-ins override base), so later-wins here is the
/// same fold viewed from the other end.
fn managed_sources_env_in(dir: &Path) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    for_each_managed_settings_tier(dir, &mut |m| {
        if let Some(env) = m.get("env") {
            for (k, v) in settings_env_pairs(env) {
                out.insert(k, v);
            }
        }
    });
    out
}

/// The global config's `env` block (claude `Rt().env` — `~/.lingxi.json`), a
/// deny-side fallback tier.
fn global_config_env() -> IndexMap<String, String> {
    let Some(path) = migrations::global_config::global_config_path() else {
        return IndexMap::new();
    };
    let Ok(map) = migrations::global_config::read_map(&path) else {
        return IndexMap::new();
    };
    map.get("env")
        .map(|v| settings_env_pairs(v).into_iter().collect())
        .unwrap_or_default()
}

/// The user settings' `env` block (claude `Hr("userSettings")?.env` —
/// `<config-home>/settings.json`), a deny-side fallback tier.
fn user_settings_env() -> IndexMap<String, String> {
    let Some(home) = migrations::global_config::lingxi_config_home() else {
        return IndexMap::new();
    };
    let Ok(raw) = std::fs::read_to_string(home.join("settings.json")) else {
        return IndexMap::new();
    };
    let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&raw) else {
        return IndexMap::new();
    };
    m.get("env")
        .map(|v| settings_env_pairs(v).into_iter().collect())
        .unwrap_or_default()
}

/// The environments enterprise policy predicates expand against — claude
/// `U__()` (whose `env` field is exactly `cWu()`, the allow-side env).
pub struct PolicyExpansionEnv {
    /// claude `cWu()` — frozen startup env snapshot overlaid by the managed
    /// settings sources' `env` blocks (managed values win). The ONLY env the
    /// allow side sees, and the deny side's primary env.
    pub env: IndexMap<String, String>,
    /// claude `U__().fallbackEnv` — globalConfig → userSettings →
    /// (flagSettings, unplumbed) → policySettings `env` blocks, later wins.
    /// Deny-side only.
    pub fallback_env: IndexMap<String, String>,
}

/// Install the immutable `flagSettings.env` tier used by deny expansion.
///
/// Returns `true` when this call installed the snapshot, `false` when an
/// earlier boot path already installed it.
pub fn install_flag_settings_env(env: impl IntoIterator<Item = (String, String)>) -> bool {
    FLAG_SETTINGS_ENV.set(env.into_iter().collect()).is_ok()
}

/// Compose the production [`PolicyExpansionEnv`] (claude `U__()`/`cWu()`).
#[must_use]
pub fn policy_expansion_env() -> PolicyExpansionEnv {
    policy_expansion_env_with_flag_settings(
        &managed_dir(),
        crate::env_expansion::startup_env_snapshot(),
        &global_config_env(),
        &user_settings_env(),
        FLAG_SETTINGS_ENV.get().unwrap_or(&IndexMap::new()),
    )
}

/// The [`policy_expansion_env`] core, parameterized on every input (testable
/// without touching the process env or the real managed dir).
#[must_use]
pub fn policy_expansion_env_with(
    dir: &Path,
    startup_snapshot: &IndexMap<String, String>,
    global_config: &IndexMap<String, String>,
    user_settings: &IndexMap<String, String>,
) -> PolicyExpansionEnv {
    policy_expansion_env_with_flag_settings(
        dir,
        startup_snapshot,
        global_config,
        user_settings,
        &IndexMap::new(),
    )
}

/// [`policy_expansion_env_with`] including the CLI `flagSettings.env` tier.
#[must_use]
pub fn policy_expansion_env_with_flag_settings(
    dir: &Path,
    startup_snapshot: &IndexMap<String, String>,
    global_config: &IndexMap<String, String>,
    user_settings: &IndexMap<String, String>,
    flag_settings: &IndexMap<String, String>,
) -> PolicyExpansionEnv {
    let managed = managed_sources_env_in(dir);
    // cWu: `{...NQr(), ...e}` — the managed-source env OVERRIDES the snapshot.
    let mut env = startup_snapshot.clone();
    for (k, v) in &managed {
        env.insert(k.clone(), v.clone());
    }
    // U__: `Object.assign({}, globalConfig, userSettings, flagSettings,
    // policySettings)` — later wins.
    let mut fallback_env = IndexMap::new();
    for tier in [global_config, user_settings, flag_settings] {
        for (k, v) in tier {
            fallback_env.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in managed {
        fallback_env.insert(k, v);
    }
    PolicyExpansionEnv { env, fallback_env }
}

/// claude `uWu(value, env, fallback)` — expand a NON-URL policy string,
/// warning (byte-exact) when it references variables absent from the policy
/// expansion env. The unresolved `${...}` stays literal in the result.
fn expand_policy_string(
    value: &str,
    env: &IndexMap<String, String>,
    fallback: Option<&IndexMap<String, String>>,
) -> String {
    let r = crate::env_expansion::expand_with_env(value, env, fallback);
    if !r.missing_vars.is_empty() {
        tracing::warn!(
            "MCP policy predicate references environment variable(s) not present in the policy expansion env: {}",
            r.missing_vars.join(", ")
        );
    }
    r.expanded
}

/// claude `oVn` — the `[command, ...args]` array for a stdio config, or `None`
/// when the config is a non-stdio type (or has no command to match).
fn config_command(config: &Value) -> Option<Vec<String>> {
    if let Some(ty) = config.get("type").and_then(Value::as_str) {
        if ty != "stdio" {
            return None;
        }
    }
    let command = config.get("command").and_then(Value::as_str)?;
    let mut out = vec![command.to_string()];
    if let Some(args) = config.get("args").and_then(Value::as_array) {
        for a in args {
            if let Some(s) = a.as_str() {
                out.push(s.to_string());
            }
        }
    }
    Some(out)
}

/// claude `iVn` — the config's `url`, or `None`.
fn config_url(config: &Value) -> Option<&str> {
    config.get("url").and_then(Value::as_str)
}

/// claude `Ilu` — element-wise array equality.
fn arrays_eq(a: &[String], b: &[String]) -> bool {
    a == b
}

// ── Policy URL expansion with unsafe-expansion detection (claude `dWu`) ──

/// claude `VLt` — the sentinel protecting PATTERN-AUTHORED `*` through env
/// expansion, so a `*` INJECTED by an expanded value can be told apart from
/// one the policy author wrote. Random per-process in claude
/// (`zzadminwc<hex>zz`); fixed here for the same determinism/collision
/// reasoning as [`WILDCARD_SENTINEL`] (it must survive URL parsing, so
/// lowercase-alnum).
const ADMIN_WILDCARD_SENTINEL: &str = "zzadminwcsentinelzz";

/// claude `q__` — the stand-in for arbitrary env values in the masked
/// expansion pass (`iSs`'s fallback arm).
const ENV_VALUE_MASK: &str = "zzenvsubzz";

/// claude `iSs(value)` — mask an env value with a shape-preserving stand-in:
/// trailing dots are kept verbatim (recursing on the prefix), an all-numeric
/// dotted value keeps its dots with digit runs zeroed (IPv4-ish), a
/// hex-and-colon value becomes `::` (IPv6-ish), anything else becomes
/// [`ENV_VALUE_MASK`]. The masked pass parses the URL with these stand-ins to
/// detect values that RESTRUCTURE the URL.
fn mask_value(value: &str) -> String {
    static NUMERIC: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static DIGIT_RUN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static IPV6ISH: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let trimmed = value.trim_end_matches('.');
    if trimmed.len() < value.len() && !trimmed.is_empty() {
        return format!("{}{}", mask_value(trimmed), &value[trimmed.len()..]);
    }
    let numeric = NUMERIC.get_or_init(|| regex::Regex::new(r"^[0-9]+(\.[0-9]+)*$").unwrap());
    if numeric.is_match(value) {
        let digit_run = DIGIT_RUN.get_or_init(|| regex::Regex::new(r"[0-9]+").unwrap());
        return digit_run.replace_all(value, "0").into_owned();
    }
    let ipv6ish = IPV6ISH.get_or_init(|| regex::Regex::new(r"^[0-9a-fA-F:.]+$").unwrap());
    if value.contains(':') && ipv6ish.is_match(value) {
        return "::".to_string();
    }
    ENV_VALUE_MASK.to_string()
}

/// The JS `URL` component view claude's `pyo` returns: `protocol` keeps its
/// `:`, `search` its `?`, `hash` its `#`, and `port` is `""` when
/// absent/default — matching the `js_*` helpers used by [`url_matches`].
struct JsUrlParts {
    protocol: String,
    hostname: String,
    username: String,
    password: String,
    port: String,
    pathname: String,
    search: String,
    hash: String,
}

/// First-match-only `:(?:<sentinel>)(?=[/?#]|$)` → `:0` (claude `pyo`'s retry
/// regex is non-global). Among the sentinels, the EARLIEST valid occurrence
/// wins. `None` when nothing replaced.
fn replace_first_port_sentinel(s: &str, sentinels: &[&str]) -> Option<String> {
    let mut best: Option<(usize, usize)> = None;
    for needle in sentinels {
        let pat = format!(":{needle}");
        let mut from = 0;
        while let Some(rel) = s[from..].find(&pat) {
            let pos = from + rel;
            let after = &s[pos + pat.len()..];
            if after.is_empty() || after.starts_with(['/', '?', '#']) {
                if best.is_none_or(|(b, _)| pos < b) {
                    best = Some((pos, pat.len()));
                }
                break;
            }
            from = pos + 1;
        }
    }
    let (pos, len) = best?;
    Some(format!("{}:0{}", &s[..pos], &s[pos + len..]))
}

/// claude `pyo(str, VLt)` — parse a URL whose VALUES may have injected `*`
/// (protected via [`WILDCARD_SENTINEL`] so the parse survives) and whose
/// pattern may carry a wildcard PORT (`:<sentinel>` retried as `:0`). Returns
/// the JS component view with the value-wildcard sentinel restored to `*`
/// ([`ADMIN_WILDCARD_SENTINEL`] is deliberately left in place — the caller
/// compares its survival across the real and masked passes). `None` when the
/// string does not parse as a URL either way.
fn parse_url_with_wildcards(input: &str, extra_port_sentinel: Option<&str>) -> Option<JsUrlParts> {
    let r = input.replace('*', WILDCARD_SENTINEL);
    let parsed = match url::Url::parse(&r) {
        Ok(u) => u,
        Err(_) => {
            let mut sentinels = vec![WILDCARD_SENTINEL];
            sentinels.extend(extra_port_sentinel);
            let retried = replace_first_port_sentinel(&r, &sentinels)?;
            url::Url::parse(&retried).ok()?
        }
    };
    let restore = |x: String| x.replace(WILDCARD_SENTINEL, "*");
    Some(JsUrlParts {
        protocol: restore(js_protocol(&parsed)),
        hostname: restore(parsed.host_str().unwrap_or("").to_string()),
        username: restore(parsed.username().to_string()),
        password: restore(parsed.password().unwrap_or("").to_string()),
        port: restore(js_port(&parsed)),
        pathname: restore(parsed.path().to_string()),
        search: restore(js_search(&parsed)),
        hash: restore(
            parsed
                .fragment()
                .map(|f| format!("#{f}"))
                .unwrap_or_default(),
        ),
    })
}

/// Where an env var lands inside the expanded URL (claude `j__`'s
/// `"scheme" | "authority" | "rest"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UrlVarPosition {
    Scheme,
    Authority,
    Rest,
}

/// claude `j__(pattern, env)` — classify every env var the pattern references
/// by URL position. Each var is probed with a positional sentinel
/// (`zzv<idx>zz`) while OTHER vars get masked values; a second pass re-probes
/// with scheme-classified vars at their REAL values (a scheme var changes how
/// the rest of the URL parses).
fn classify_url_var_positions(
    pattern: &str,
    env: &IndexMap<String, String>,
) -> Vec<(String, UrlVarPosition)> {
    let mut referenced: Vec<String> = Vec::new();
    for caps in crate::env_expansion::env_ref_regex().captures_iter(pattern) {
        let name = &caps[1];
        if !referenced.iter().any(|r| r == name) && env.contains_key(name) {
            referenced.push(name.to_string());
        }
    }
    let classify = |target: &str, real: &std::collections::HashSet<String>| -> UrlVarPosition {
        let idx = referenced
            .iter()
            .position(|x| x == target)
            .unwrap_or_default();
        let c = format!("zzv{idx}zz");
        let lookup = |name: &str| -> Option<String> {
            if name == target {
                return Some(c.clone());
            }
            let v = env.get(name)?;
            Some(if real.contains(name) {
                v.clone()
            } else {
                mask_value(v)
            })
        };
        let d = crate::env_expansion::expand_with_lookups(pattern, &lookup, None).expanded;
        let Some(p) = parse_url_with_wildcards(&d, Some(ADMIN_WILDCARD_SENTINEL)) else {
            return UrlVarPosition::Authority;
        };
        if p.protocol.contains(&c) {
            return UrlVarPosition::Scheme;
        }
        if p.username.contains(&c)
            || p.password.contains(&c)
            || p.hostname.contains(&c)
            || p.port.contains(&c)
        {
            return UrlVarPosition::Authority;
        }
        if p.pathname.contains(&c) || p.search.contains(&c) || p.hash.contains(&c) {
            return UrlVarPosition::Rest;
        }
        UrlVarPosition::Scheme
    };
    let empty = std::collections::HashSet::new();
    let scheme_vars: std::collections::HashSet<String> = referenced
        .iter()
        .filter(|a| classify(a, &empty) == UrlVarPosition::Scheme)
        .cloned()
        .collect();
    referenced
        .iter()
        .map(|a| {
            let pos = if scheme_vars.contains(a) {
                UrlVarPosition::Scheme
            } else {
                classify(a, &scheme_vars)
            };
            (a.clone(), pos)
        })
        .collect()
}

/// claude `V__(classes, env)` — does any scheme/authority-position var carry a
/// value with URL-structure characters? Scheme position rejects
/// `[:/@#?\\\s]` (`G__`), authority `[/@#?\\\s]` (`W__` — a `:` is a legal
/// port separator there). (JS `\s` and Rust `\s` differ only on exotica like
/// U+FEFF.)
fn value_injects_url_structure(
    classes: &[(String, UrlVarPosition)],
    env: &IndexMap<String, String>,
) -> bool {
    static SCHEME_UNSAFE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"[:/@#?\\\s]").unwrap());
    static AUTHORITY_UNSAFE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"[/@#?\\\s]").unwrap());
    for (name, pos) in classes {
        if *pos == UrlVarPosition::Rest {
            continue;
        }
        let Some(v) = env.get(name) else { continue };
        let re = if *pos == UrlVarPosition::Scheme {
            &*SCHEME_UNSAFE
        } else {
            &*AUTHORITY_UNSAFE
        };
        if re.is_match(v) {
            return true;
        }
    }
    false
}

/// claude `K__(pattern, env)` — does any referenced var's value smuggle a
/// query/fragment (`[?#]`) or a dot-segment traversal? The value is
/// normalized first (strip `\t\n\r`, backslashes → `/`, `%2e` → `.`), then
/// tested against `z__` `(^|/)(\.|%2e)(\.|%2e)?(/|$)` case-insensitively.
fn value_injects_path_traversal(pattern: &str, env: &IndexMap<String, String>) -> bool {
    static PERCENT_2E: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)%2e").unwrap());
    static TRAVERSAL: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)(^|/)(\.|%2e)(\.|%2e)?(/|$)").unwrap());
    for caps in crate::env_expansion::env_ref_regex().captures_iter(pattern) {
        let Some(v) = env.get(&caps[1]) else { continue };
        if v.contains('?') || v.contains('#') {
            return true;
        }
        let stripped: String = v
            .chars()
            .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
            .collect();
        let slashed = stripped.replace('\\', "/");
        let normalized = PERCENT_2E.replace_all(&slashed, ".");
        if TRAVERSAL.is_match(&normalized) {
            return true;
        }
    }
    false
}

/// Result of a policy `serverUrl` expansion (claude `dWu`'s
/// `{expanded, unsafeExpansion}`).
struct PolicyUrlExpansion {
    expanded: String,
    unsafe_expansion: bool,
}

/// claude `dWu(pattern, env, fallback)` — expand a policy URL pattern with
/// unsafe-expansion detection. Pattern-authored `*` is sentinel-protected,
/// then the pattern is expanded twice — real values vs shape-masked values —
/// and the two parses are compared: a value that injects wildcard semantics,
/// restructures the URL (scheme/authority characters, dropped hash/search,
/// unparseable masked form), or smuggles traversal marks the expansion
/// UNSAFE. Allowlist entries using an unsafe expansion fail closed
/// ([`is_allowed_with_env`] skips them); denylist entries keep using the
/// expanded pattern. Both warn messages are byte-exact.
fn expand_policy_url(
    pattern: &str,
    env: &IndexMap<String, String>,
    fallback: Option<&IndexMap<String, String>>,
) -> PolicyUrlExpansion {
    let o: String = pattern
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect::<String>()
        .replace('*', ADMIN_WILDCARD_SENTINEL);
    let i = crate::env_expansion::expand_with_env(&o, env, fallback);
    // Masked pass: every env value shape-masked, NO fallback (claude's proxy
    // covers `t` only — a fallback-resolved var stays literal here).
    let masked_lookup = |name: &str| env.get(name).map(|v| mask_value(v));
    let a = crate::env_expansion::expand_with_lookups(&o, &masked_lookup, None);
    let l = parse_url_with_wildcards(&i.expanded, Some(ADMIN_WILDCARD_SENTINEL));
    let c = parse_url_with_wildcards(&a.expanded, Some(ADMIN_WILDCARD_SENTINEL));
    let u = l.as_ref().map(|p| p.hostname.clone());
    let d = c.as_ref().map(|p| p.hostname.clone());
    let value_changed = a.expanded != o; // claude `p`
    let classes = if value_changed {
        classify_url_var_positions(&o, env)
    } else {
        Vec::new()
    };
    let unsafe_expansion = !i.wildcard_vars.is_empty()
        || (value_changed && value_injects_url_structure(&classes, env))
        || (u.is_none() != d.is_none())
        || (value_changed && u.is_none())
        || l.as_ref().is_some_and(|lp| {
            let c_hash = c.as_ref().map_or("", |p| p.hash.as_str());
            let c_search = c.as_ref().map_or("", |p| p.search.as_str());
            (!lp.hash.is_empty() && c_hash.is_empty())
                || (!lp.search.is_empty() && c_search.is_empty())
        })
        || (value_changed && value_injects_path_traversal(&o, env))
        || u.as_deref().is_some_and(|uh| {
            let dh = d.as_deref().unwrap_or("");
            (uh.contains('*') && !dh.contains('*'))
                || (uh.ends_with('.') && !dh.ends_with('.'))
                || (uh.contains(ADMIN_WILDCARD_SENTINEL) && !dh.contains(ADMIN_WILDCARD_SENTINEL))
        });
    if !i.missing_vars.is_empty() {
        tracing::warn!(
            "MCP policy predicate references environment variable(s) not present in the policy expansion env: {}",
            i.missing_vars.join(", ")
        );
    }
    if unsafe_expansion {
        let reason = if i.wildcard_vars.is_empty() {
            "a value restructured the URL, or the expansion is unparseable as a URL (e.g. a whole-URL ${VAR}: rewrite as https://${HOST}/path \u{2014} hostname-position variables are fully supported)"
        } else {
            "a value injected wildcard semantics"
        };
        let vars = if i.wildcard_vars.is_empty() {
            env.keys()
                .filter(|g| {
                    pattern.contains(&format!("${{{g}}}")) || pattern.contains(&format!("${{{g}:"))
                })
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            i.wildcard_vars.join(", ")
        };
        let vars = if vars.is_empty() {
            "unknown".to_string()
        } else {
            vars
        };
        tracing::warn!(
            "MCP policy URL predicate expansion was unsafe \u{2014} {reason} (variables: {vars}) \u{2014} allowlist URL entries using it fail closed; denylist entries are unaffected"
        );
    }
    PolicyUrlExpansion {
        expanded: i.expanded.replace(ADMIN_WILDCARD_SENTINEL, "*"),
        unsafe_expansion,
    }
}

/// claude `edt(name, config)` — is the server *explicitly denied* by policy?
/// Composes the production [`PolicyExpansionEnv`] per call (as `edt` calls
/// `U__()`); [`is_denied_with_env`] is the injectable core.
#[must_use]
pub fn is_denied(name: &str, config: &Value, policy: &McpPolicy) -> bool {
    is_denied_with_env(name, config, policy, &policy_expansion_env())
}

/// The [`is_denied`] core against an explicit [`PolicyExpansionEnv`]. POLICY
/// strings expand with `envs.env` + `envs.fallback_env` (`uWu(c, n, o)` /
/// `dWu(l.serverUrl, n, o)`); the CANDIDATE config side stays on the live
/// process env (`byo`). Deny URL entries use the expansion even when it was
/// unsafe — the fail-closed skip is allow-side only.
#[must_use]
pub fn is_denied_with_env(
    name: &str,
    config: &Value,
    policy: &McpPolicy,
    envs: &PolicyExpansionEnv,
) -> bool {
    let Some(denied) = policy.denied.as_ref() else {
        return false;
    };
    for m in denied {
        if m.server_name.as_deref() == Some(name) {
            return true;
        }
    }
    if let Some(cmd) = config_command(config) {
        let i: Vec<String> = cmd.iter().map(|s| get(s)).collect();
        for m in denied {
            if let Some(sc) = m.server_command.as_ref() {
                let sc_e: Vec<String> = sc
                    .iter()
                    .map(|s| expand_policy_string(s, &envs.env, Some(&envs.fallback_env)))
                    .collect();
                if arrays_eq(&sc_e, &i) {
                    return true;
                }
            }
        }
    }
    if let Some(url) = config_url(config) {
        let iu = get(url);
        for m in denied {
            if let Some(su) = m.server_url.as_ref() {
                let expansion = expand_policy_url(su, &envs.env, Some(&envs.fallback_env));
                if url_matches(&iu, &expansion.expanded) {
                    return true;
                }
            }
        }
    }
    false
}

/// claude `ZFe(name, config)` — is the server *allowed* by policy? Composes
/// the production [`PolicyExpansionEnv`] per call (as `ZFe` calls `cWu()`);
/// [`is_allowed_with_env`] is the injectable core.
#[must_use]
pub fn is_allowed(name: &str, config: &Value, policy: &McpPolicy) -> bool {
    is_allowed_with_env(name, config, policy, &policy_expansion_env())
}

/// The [`is_allowed`] core against an explicit [`PolicyExpansionEnv`]. The
/// allow side expands policy strings with `envs.env` ONLY — no fallback
/// (`uWu(u, i)` / `dWu(c.serverUrl, i)`) — and an allowlist URL entry whose
/// expansion was UNSAFE is skipped entirely (fail closed).
#[must_use]
pub fn is_allowed_with_env(
    name: &str,
    config: &Value,
    policy: &McpPolicy,
    envs: &PolicyExpansionEnv,
) -> bool {
    if is_denied_with_env(name, config, policy, envs) {
        return false;
    }
    let Some(allowed) = policy.allowed.as_ref() else {
        return true; // no allowlist ⇒ everything allowed
    };
    if allowed.is_empty() {
        return false; // empty allowlist ⇒ nothing allowed
    }
    let has_cmd = allowed.iter().any(|m| m.server_command.is_some());
    let has_url = allowed.iter().any(|m| m.server_url.is_some());
    let name_match = || {
        allowed
            .iter()
            .any(|m| m.server_name.as_deref() == Some(name))
    };

    if let Some(cmd) = config_command(config) {
        if has_cmd {
            let a: Vec<String> = cmd.iter().map(|s| get(s)).collect();
            for m in allowed {
                if let Some(sc) = m.server_command.as_ref() {
                    let sc_e: Vec<String> = sc
                        .iter()
                        .map(|s| expand_policy_string(s, &envs.env, None))
                        .collect();
                    if arrays_eq(&sc_e, &a) {
                        return true;
                    }
                }
            }
            false
        } else {
            name_match()
        }
    } else if let Some(url) = config_url(config) {
        if has_url {
            let a = get(url);
            for m in allowed {
                if let Some(su) = m.server_url.as_ref() {
                    let expansion = expand_policy_url(su, &envs.env, None);
                    if expansion.unsafe_expansion {
                        continue; // fail closed (claude `if(d)continue`)
                    }
                    if url_matches(&a, &expansion.expanded) {
                        return true;
                    }
                }
            }
            false
        } else {
            name_match()
        }
    } else {
        name_match()
    }
}

/// A fixed wildcard sentinel (claude's `L7t`/`YVe`, which is random
/// per-process only to avoid colliding with real URL content; a fixed
/// lowercase-alnum string is equally collision-free and deterministic, and
/// parses as a URL scheme so the protocol-wildcard branch works).
const WILDCARD_SENTINEL: &str = "zzwildcardsentinelzz";

/// Escape the regex metacharacters claude's `UFr` escapes — the JS class
/// `/[.+?^${}()|[\]\\]/g` — deliberately leaving `*` for the wildcard step.
fn escape_re(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '.' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Replace `:<sentinel>` with `:0` when it stands where a port would (followed
/// by `/`, `?`, `#`, or end-of-string) — the look-ahead-free equivalent of
/// claude's `n.replace(/:${L7t}(?=[/?#]|$)/, ":0")`.
fn replace_wildcard_port(n: &str) -> String {
    let needle = format!(":{WILDCARD_SENTINEL}");
    let mut out = String::with_capacity(n.len());
    let mut rest = n;
    while let Some(pos) = rest.find(&needle) {
        let after = &rest[pos + needle.len()..];
        out.push_str(&rest[..pos]);
        if after.is_empty() || after.starts_with(['/', '?', '#']) {
            out.push_str(":0");
        } else {
            out.push_str(&needle);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// JS `url.protocol` (scheme + `:`).
fn js_protocol(u: &url::Url) -> String {
    format!("{}:", u.scheme())
}

/// JS `url.host` (hostname, plus `:port` only when the port is non-default).
fn js_host(u: &url::Url) -> String {
    match u.port() {
        Some(p) => format!("{}:{}", u.host_str().unwrap_or(""), p),
        None => u.host_str().unwrap_or("").to_string(),
    }
}

/// JS `url.port` ("" when default/absent, else the port string).
fn js_port(u: &url::Url) -> String {
    u.port().map(|p| p.to_string()).unwrap_or_default()
}

/// JS `url.search` (`?query` or "").
fn js_search(u: &url::Url) -> String {
    u.query().map(|q| format!("?{q}")).unwrap_or_default()
}

/// claude `UFr(url, pattern)` — glob-match a URL against a policy pattern.
/// A faithful port of the 2.1.206 matcher, mapping JS `URL` accessors onto the
/// `url` crate (protocol keeps its `:`, host folds in a non-default port, the
/// `L7t` sentinel stands in for `*` so `*://`, `host:*`, `*.host`, and path
/// globs all parse).
#[must_use]
pub fn url_matches(url_str: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let Ok(r) = url::Url::parse(url_str) else {
        return false;
    };
    let n = pattern.replace('*', WILDCARD_SENTINEL);
    let mut o = false; // "port is wildcard" flag
    let mut parsed = url::Url::parse(&n).ok();
    if parsed.is_none() {
        // A wildcard *port* (`host:*`) makes the sentinel an invalid port; retry
        // with `:0` (claude's `:${L7t}(?=[/?#]|$)` → `:0`). The `regex` crate has
        // no look-ahead, so the "followed by /?# or end" guard is done manually.
        let u = replace_wildcard_port(&n);
        if u != n {
            if let Ok(p) = url::Url::parse(&u) {
                parsed = Some(p);
                o = true;
            }
        }
    }
    let Some(i) = parsed else {
        // Pattern isn't a full URL → treat as a plain glob over the whole
        // reconstructed URL string. `*` → `[^/]*`; the ORIGINAL pattern's other
        // metachars are escaped.
        let reconstructed = format!(
            "{}//{}{}{}",
            js_protocol(&r),
            js_host(&r),
            r.path(),
            js_search(&r)
        );
        let d = escape_re(pattern).replace('*', "[^/]*");
        return regex::Regex::new(&format!("^{d}$"))
            .map(|re| re.is_match(&reconstructed))
            .unwrap_or(false);
    };

    // Protocol must match (or be the sentinel wildcard scheme).
    if js_protocol(&i) != format!("{WILDCARD_SENTINEL}:") && js_protocol(&i) != js_protocol(&r) {
        return false;
    }
    // Hostname (trailing dot stripped, lowercased) must match, `*` → `[^/]*`.
    let host = r
        .host_str()
        .unwrap_or("")
        .trim_end_matches('.')
        .to_lowercase();
    let pat_host = i
        .host_str()
        .unwrap_or("")
        .trim_end_matches('.')
        .to_lowercase()
        .replace(WILDCARD_SENTINEL, "*");
    let host_re = escape_re(&pat_host).replace('*', "[^/]*");
    if !regex::Regex::new(&format!("^{host_re}$"))
        .map(|re| re.is_match(&host))
        .unwrap_or(false)
    {
        return false;
    }
    // A wildcard in the hostname with an empty pattern port ⇒ port wildcard too.
    if js_port(&i).is_empty() && i.host_str().unwrap_or("").contains(WILDCARD_SENTINEL) {
        o = true;
    }
    if !o && js_port(&i) != js_port(&r) {
        return false;
    }
    // No meaningful path/search in the pattern (and it didn't end in `/`) ⇒ match.
    let ipath = i.path();
    if (ipath == "/" || ipath.is_empty()) && js_search(&i).is_empty() && !n.ends_with('/') {
        return true;
    }
    // Otherwise the path+search must match, `*` → `.*`.
    let pat_path = format!("{}{}", i.path(), js_search(&i)).replace(WILDCARD_SENTINEL, "*");
    let path_re = escape_re(&pat_path).replace('*', ".*");
    regex::Regex::new(&format!("^{path_re}$"))
        .map(|re| re.is_match(&format!("{}{}", r.path(), js_search(&r))))
        .unwrap_or(false)
}

// ────────────────────────────────────────────────────────────────────────────
// Stage 3 — load-time enforcement (claude `Ree` / `Qme` / `bC("enterprise")`).
//
// At MCP load time claude drops any server the policy blocks (`Ree`: keep iff
// `type === "sdk" || gPe(name, config)`), and when a managed MCP config is
// active it takes *exclusive* control — only its own servers load (`Qme`'s
// `D1()` branch). Both reuse the matchers above.
// ────────────────────────────────────────────────────────────────────────────

use crate::connection::{ConfigScope, McpServerConfig};
use lingxi_core::host::McpTransportSpec;

/// Project a loaded [`McpTransportSpec`] onto the `{type, command, args, url}`
/// config view the matchers extract from (claude `oVn`/`iVn` read those keys).
fn spec_matcher_view(spec: &McpTransportSpec) -> Value {
    match spec {
        McpTransportSpec::Stdio { command, args, .. } => serde_json::json!({
            "type": "stdio",
            "command": command,
            "args": args,
        }),
        McpTransportSpec::Sse { url, .. }
        | McpTransportSpec::SseIde { url, .. }
        | McpTransportSpec::WsIde { url, .. } => {
            serde_json::json!({ "type": "sse", "url": url })
        }
        McpTransportSpec::Http { url, .. } => serde_json::json!({ "type": "http", "url": url }),
        McpTransportSpec::WebSocket { url, .. } => serde_json::json!({ "type": "ws", "url": url }),
        // No command/url to match — a name-only matcher still applies; a plain
        // object with a non-stdio type makes `oVn` return `None`.
        McpTransportSpec::InProcess { .. } => serde_json::json!({ "type": "inprocess" }),
        McpTransportSpec::SdkControl { .. } => serde_json::json!({ "type": "sdk" }),
    }
}

/// claude `Fxo(entry)` — does a RAW `.mcp.json` entry read the environment?
///
/// Tested before expansion, on exactly the fields that expand:
///
/// ```js
/// case void 0: case "stdio": return HY(command) || args.some(HY) || Object.values(env??{}).some(HY)
/// case "sse": case "http": case "ws": return HY(url) || Object.values(headers??{}).some(HY)
/// case "sse-ide": case "ws-ide": case "sdk": case "claudeai-proxy": return !1
/// ```
///
/// The field list is the same one [`crate::config_diagnostics`] already uses
/// for unresolved-reference reporting (claude `Osg`).
#[must_use]
pub fn entry_reads_environment(entry: &Value) -> bool {
    let has_ref = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .is_some_and(|s| crate::env_expansion::env_ref_regex().is_match(s))
    };
    let any_value_has_ref = |v: Option<&Value>| {
        v.and_then(Value::as_object)
            .is_some_and(|map| map.values().any(|value| has_ref(Some(value))))
    };
    let any_item_has_ref = |v: Option<&Value>| {
        v.and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(|item| has_ref(Some(item))))
    };
    match entry.get("type").and_then(Value::as_str) {
        None | Some("stdio") => {
            has_ref(entry.get("command"))
                || any_item_has_ref(entry.get("args"))
                || any_value_has_ref(entry.get("env"))
        }
        Some("sse" | "http" | "ws") => {
            has_ref(entry.get("url")) || any_value_has_ref(entry.get("headers"))
        }
        // These expand nothing, so they can never have read the environment.
        _ => false,
    }
}

/// claude `F6`'s second line — a server the ORGANIZATION delivered needs no
/// `allowedMcpServers` entry:
///
/// ```js
/// if (n?.scope !== void 0 && XJ(n.scope) && !n.expandedFromEnv && !n.pluginSource) return !0;
/// ```
///
/// with `XJ(e) = ["enterprise","managed"].includes(e)`.
///
/// 🚨 The `expandedFromEnv` conjunct is the whole point, not a detail. A
/// managed document that cannot read the user's environment is fully trusted;
/// one that CAN is still checked against the allowlist, because its effective
/// command or url is not fully determined by the document the organization
/// signed off on.
///
/// The denylist is unaffected either way — upstream runs `Pme` before this line
/// and so does [`is_server_allowed`].
///
/// ⚠️ `!n.pluginSource` has no port analogue: `mcp` carries no plugin-source
/// provenance on a server config, and a plugin-contributed server is never
/// loaded at `Enterprise`/`Managed` scope, so the conjunct is vacuous here.
#[must_use]
fn org_delivered_needs_no_allowlist_entry(config: &McpServerConfig) -> bool {
    matches!(
        config.scope,
        ConfigScope::Enterprise | ConfigScope::Settings(lingxi_core::types::SettingsScope::Managed)
    ) && !config.metadata.expanded_from_env
}

/// claude `Ree`'s per-server predicate — a loaded server is kept iff it is an
/// SDK-control server or the allow/deny policy permits it.
#[must_use]
pub fn is_server_allowed(config: &McpServerConfig, policy: &McpPolicy) -> bool {
    if matches!(config.spec, McpTransportSpec::SdkControl { .. }) {
        return true; // claude `i.type === "sdk"` short-circuit
    }
    let view = spec_matcher_view(&config.spec);
    let envs = policy_expansion_env();
    // `F6`'s order, exactly: deny wins over everything, then the
    // organization-delivered exemption, then the allowlist.
    if is_denied_with_env(&config.name, &view, policy, &envs) {
        return false;
    }
    if org_delivered_needs_no_allowlist_entry(config) {
        return true;
    }
    is_allowed_with_env(&config.name, &view, policy, &envs)
}

/// Parse the managed MCP config (`managed-mcp.json`) into server configs
/// (claude `bC("enterprise")`), scoped [`ConfigScope::Enterprise`]. Empty on a
/// missing/malformed file.
#[must_use]
pub fn load_enterprise_servers() -> Vec<McpServerConfig> {
    load_enterprise_servers_at(&managed_mcp_config_path())
}

/// The [`load_enterprise_servers`] core, parameterized on the config path so it
/// is testable without touching the process-global managed dir / env.
///
/// The read goes through the same `Iqe` shape/size guard the project-scope
/// `.mcp.json` walk uses: `zit`'s `case"enterprise"` calls
/// `Iqe({filePath:J$t(),expandVars:t,scope:"enterprise"})` (2.1.251
/// @160901500), and every non-`dynamic` scope inside `Iqe` reads via
/// `Atr(d,t,mcn)` (`if(!o.isFile()||o.size>r)return null`, @156004982). A
/// managed `managed-mcp.json` that is a FIFO or larger than
/// [`crate::config_diagnostics::MCP_CONFIG_MAX_BYTES`] is refused rather than
/// read — an unguarded `read_to_string` on a writer-less FIFO blocks boot
/// forever.
#[must_use]
pub fn load_enterprise_servers_at(path: &Path) -> Vec<McpServerConfig> {
    let Ok(raw) = crate::config_diagnostics::read_mcp_config_file(path, ConfigScope::Enterprise)
    else {
        return Vec::new();
    };
    let mut configs = crate::json_config::parse_mcp_json_string(&raw, ConfigScope::Enterprise)
        .unwrap_or_default();
    // `if (o === "enterprise" && Fxo(entry)) xe = {...xe, expandedFromEnv: !0}`.
    // Computed from the RAW document, before expansion, because after it the
    // `${VAR}` is gone and the entry looks like any other literal.
    if let Ok(Value::Object(root)) = serde_json::from_str::<Value>(&raw) {
        let entries = root.get("mcpServers").and_then(Value::as_object);
        if let Some(entries) = entries {
            for config in &mut configs {
                if let Some(entry) = entries.get(&config.name) {
                    config.metadata.expanded_from_env = entry_reads_environment(entry);
                }
            }
        }
    }
    configs
}

/// claude's byte-exact shape rejection for the settings key.
pub const MANAGED_MCP_SERVERS_SHAPE_INVALID: &str = "\"managedMcpServers\" must be an object keyed by server name (the .mcp.json mcpServers shape; Claude Desktop's array form of its same-named key is not accepted here: use the server name as the key and \"type\" instead of \"transport\"). No managed MCP servers are installed from it until it is fixed.";

/// claude's byte-exact server-name rejection for the settings key.
pub const MANAGED_MCP_SERVER_NAME_INVALID: &str = "server names may only contain letters, numbers, hyphens and underscores, and may not be __proto__, constructor or prototype";

/// `a3t`-style name validation for `managedMcpServers` keys.
fn managed_settings_server_name_ok(name: &str) -> bool {
    if matches!(name, "__proto__" | "constructor" | "prototype") {
        return false;
    }
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `managedMcpServers` — the organization's OWN MCP servers, delivered inside
/// managed settings rather than through `managed-mcp.json`.
///
/// Upstream's `case "managed"` arm of `BTn`:
///
/// ```js
/// case "managed": { let o = cTn();
///   if (!o) return {servers: Wk(), errors: mTn()};
///   let {config:d, errors:p} = h$e({configObject:{mcpServers:o}, expandVars:!1, scope:"managed"});
///   return {servers: mde(d?.mcpServers, e), errors: p} }
/// ```
///
/// The key's own contract (schema `describe`): *"MCP servers the organization
/// provides to every user, keyed by server name, each with the .mcp.json entry
/// shape; **only "http" and "sse" servers are accepted (nothing that names a
/// program to run, no `${VAR}` references)**. Honored from managed settings
/// only; users cannot remove them, deniedMcpServers still applies, and they
/// need no allowedMcpServers entry."*
///
/// Three restrictions, all enforced here:
///
/// * **http/sse only.** An organization may push a URL to every user; it may
///   not push a program to run on their machine. A `stdio` entry is dropped.
/// * **No `${VAR}`.** Upstream has belt and braces: the managed-settings schema
///   refuses a `${VAR}` outright (*"a managed settings document must not read
///   the user's environment"*) AND the load runs with `expandVars: false`. This
///   port implements the belt — an entry that reads the environment is
///   rejected before it reaches the parser — which is why it needs no
///   no-expansion parse path. Strictly the safe direction: upstream would keep
///   such an entry with the reference unexpanded and let it fail at dial time.
/// * **Name shape**, byte-exact with [`MANAGED_MCP_SERVER_NAME_INVALID`].
///
/// Scope is [`ConfigScope::Settings(lingxi_core::types::SettingsScope::Managed)`], which is in claude's exempt set
/// `XJ = ["enterprise","managed"]`, so these need no `allowedMcpServers` entry.
/// `expanded_from_env` is never set for them — upstream only ever sets it at
/// `enterprise` scope — which is consistent: an entry that reads the
/// environment never becomes a managed server in the first place.
#[must_use]
pub fn load_managed_settings_servers() -> Vec<McpServerConfig> {
    load_managed_settings_servers_in(&managed_dir())
}

/// The [`load_managed_settings_servers`] core, parameterized on the managed dir.
#[must_use]
pub fn load_managed_settings_servers_in(dir: &Path) -> Vec<McpServerConfig> {
    let mut merged = serde_json::Map::new();
    let mut shape_rejected = false;
    for_each_managed_settings_tier(dir, &mut |source| {
        match source.get("managedMcpServers") {
            None => {}
            Some(Value::Object(entries)) => {
                // Later tier wins, the same fold order the policy read uses.
                for (name, entry) in entries {
                    merged.insert(name.clone(), entry.clone());
                }
            }
            Some(_) => shape_rejected = true,
        }
    });
    if shape_rejected {
        tracing::warn!(target: "lingxi_mcp::policy", "{MANAGED_MCP_SERVERS_SHAPE_INVALID}");
        // "No managed MCP servers are installed from it until it is fixed."
        return Vec::new();
    }
    if merged.is_empty() {
        return Vec::new();
    }

    let mut accepted = serde_json::Map::new();
    for (name, entry) in merged {
        if !managed_settings_server_name_ok(&name) {
            tracing::warn!(target: "lingxi_mcp::policy", server = %name, "{MANAGED_MCP_SERVER_NAME_INVALID}");
            continue;
        }
        let kind = entry.get("type").and_then(Value::as_str);
        if !matches!(kind, Some("http" | "sse")) {
            tracing::warn!(
                target: "lingxi_mcp::policy",
                server = %name,
                "managedMcpServers accepts only \"http\" and \"sse\" servers; nothing that names a program to run"
            );
            continue;
        }
        if entry_reads_environment(&entry) {
            tracing::warn!(
                target: "lingxi_mcp::policy",
                server = %name,
                "${{VAR}} references are not expanded in managed settings; use a literal value"
            );
            continue;
        }
        accepted.insert(name, entry);
    }
    if accepted.is_empty() {
        return Vec::new();
    }

    let document = Value::Object(
        [("mcpServers".to_string(), Value::Object(accepted))]
            .into_iter()
            .collect(),
    );
    crate::json_config::parse_mcp_json_string(
        &document.to_string(),
        ConfigScope::Settings(lingxi_core::types::SettingsScope::Managed),
    )
    .unwrap_or_default()
}

/// Apply the enterprise MCP policy to the assembled to-connect list, in place —
/// the load-site equivalent of claude's `Qme`/`Ree`:
///
/// - When [`enterprise_mcp_active`], replace the list with the managed
///   (`managed-mcp.json`) servers that pass the allow policy — exclusive
///   control (`D1()` branch).
/// - Otherwise drop every server the allow/deny policy blocks (`Ree`).
///
/// Inert when no managed config or policy is present (nothing is removed), so a
/// default deployment is byte-identical.
pub fn apply_enterprise_mcp_policy(configs: &mut Vec<McpServerConfig>) {
    let policy = read_managed_mcp_policy();
    apply_enterprise_mcp_policy_with(configs, &policy);
}

/// Apply enterprise exclusivity plus an already-composed effective policy.
pub fn apply_enterprise_mcp_policy_with(configs: &mut Vec<McpServerConfig>, policy: &McpPolicy) {
    // claude `$Tn = [...FTn, "local", "project", "user"]` with
    // `FTn = ["enterprise","managed"]` — an EARLIER scope wins a name, so the
    // organization's two delivery channels both outrank the user's tiers, and
    // `managed-mcp.json` outranks the settings key.
    let managed_settings = load_managed_settings_servers();
    if enterprise_mcp_active() {
        // Exclusive control: only what the ORGANIZATION delivered loads. That
        // is both org channels, not just the file — `managedMcpServers` is
        // documented as something "users cannot remove", and dropping the org's
        // other channel because the org also shipped a file would be odd.
        let mut kept: Vec<McpServerConfig> = load_enterprise_servers()
            .into_iter()
            .filter(|c| is_server_allowed(c, policy))
            .collect();
        for server in managed_settings {
            if !is_server_allowed(&server, policy) {
                continue;
            }
            if !kept.iter().any(|c| c.name == server.name) {
                kept.push(server);
            }
        }
        *configs = kept;
    } else {
        configs.retain(|c| is_server_allowed(c, policy));
        for server in managed_settings {
            if !is_server_allowed(&server, policy) {
                continue;
            }
            // `managed` sits ahead of local/project/user, so the organization's
            // entry takes the name rather than being skipped as a duplicate.
            configs.retain(|c| c.name != server.name);
            configs.push(server);
        }
    }
}

#[cfg(test)]
mod tests {

    /// The enterprise `managed-mcp.json` read goes through the same `Iqe`
    /// shape/size guard the project `.mcp.json` walk uses: `zit`'s
    /// `case"enterprise"` calls `Iqe({filePath:J$t(),expandVars:t,
    /// scope:"enterprise"})` (2.1.251 @160901500) and every non-`dynamic`
    /// scope inside `Iqe` reads via `Atr(d,t,mcn)` —
    /// `if(!o.isFile()||o.size>r)return null` with `mcn=2097152` (@156004982,
    /// @160911363). Both enterprise readers were doing a bare
    /// `std::fs::read_to_string`, so an oversized managed config was read in
    /// full and a FIFO would have blocked boot forever waiting for a writer.
    #[test]
    fn enterprise_managed_config_is_refused_when_it_exceeds_the_byte_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("managed-mcp.json");

        // Comfortably UNDER the cap: loads normally, so the test cannot pass
        // by simply failing to read anything.
        std::fs::write(
            &path,
            r#"{"mcpServers":{"srv":{"type":"stdio","command":"c"}}}"#,
        )
        .unwrap();
        assert!(super::enterprise_mcp_active_at(&path));
        let servers = super::load_enterprise_servers_at(&path);
        assert_eq!(servers.len(), 1, "a small managed config must still load");
        assert_eq!(servers[0].name, "srv");

        // Same content, padded past `mcn` with whitespace inside the object so
        // it stays valid JSON: the guard must refuse the read outright.
        let cap = crate::config_diagnostics::MCP_CONFIG_MAX_BYTES as usize;
        let padded = format!(
            "{{\"mcpServers\":{{\"srv\":{{\"type\":\"stdio\",\"command\":\"c\"}}}}{}}}",
            " ".repeat(cap + 1)
        );
        std::fs::write(&path, &padded).unwrap();
        assert!(
            std::fs::metadata(&path).unwrap().len()
                > crate::config_diagnostics::MCP_CONFIG_MAX_BYTES,
            "fixture must actually cross the {cap}-byte threshold it names"
        );
        assert!(
            !super::enterprise_mcp_active_at(&path),
            "an over-cap managed-mcp.json must not activate enterprise MCP control"
        );
        assert!(
            super::load_enterprise_servers_at(&path).is_empty(),
            "an over-cap managed-mcp.json must be refused, not read in full"
        );
    }

    /// Within the FILE tier, a `managed-settings.d` drop-in OVERRIDES the base
    /// `managed-settings.json`, and the drop-ins are applied in sorted order.
    ///
    /// Oracle `g4r`: `r = N7(r, base, woe)` then, for each sorted `.d` entry,
    /// `r = N7(r, dropIn, woe)`. `N7` is lodash `mergeWith` and `woe` only
    /// customizes ARRAYS (concat+dedup, or replace for `fallbackModel`), so a
    /// scalar like an `env` value takes the LAST writer.
    ///
    /// Do not confuse this with `cWu`'s `e[r] ??= n`, which is first-wins — that
    /// fold runs one level up, ACROSS the `VQ()` tiers (remote → plist/hklm →
    /// file), where the whole file tier below has already been collapsed by
    /// `g4r` into a single settings object.
    #[test]
    fn a_managed_drop_in_overrides_the_base_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("managed-settings.json"),
            r#"{"env":{"POLICY_HOST":"base.example","ONLY_BASE":"b"}}"#,
        )
        .unwrap();
        let dd = dir.path().join("managed-settings.d");
        std::fs::create_dir_all(&dd).unwrap();
        std::fs::write(
            dd.join("10-late.json"),
            r#"{"env":{"POLICY_HOST":"late.example","ONLY_LATE":"l"}}"#,
        )
        .unwrap();

        let env = super::managed_sources_env_in(dir.path());
        assert_eq!(
            env.get("POLICY_HOST").map(String::as_str),
            Some("late.example"),
            "the drop-in is merged ON TOP of the base file"
        );
        // Keys only one of them defines survive either way.
        assert_eq!(env.get("ONLY_BASE").map(String::as_str), Some("b"));
        assert_eq!(env.get("ONLY_LATE").map(String::as_str), Some("l"));
    }

    #[test]
    fn a_managed_dir_without_env_yields_an_empty_map() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("managed-settings.json"),
            r#"{"deniedMcpServers":[]}"#,
        )
        .unwrap();
        assert!(super::managed_sources_env_in(dir.path()).is_empty());
    }

    /// THE security property. `${VAR}` in a managed matcher expands against a
    /// FROZEN startup snapshot plus managed tiers — never the live process
    /// environment, which a settings file can write. Expanding against live env
    /// would let a lower-trust tier steer what an enterprise policy matches.
    #[test]
    fn policy_expansion_ignores_a_variable_set_after_startup() {
        // The snapshot is taken on first use; prime it now so the variable set
        // below is unambiguously "after startup".
        super::prime_startup_env();
        let key = "LINGXI_TEST_POLICY_EXPANSION_LATE";
        std::env::set_var(key, "attacker.example");
        let envs = super::policy_expansion_env();
        let expanded = super::expand_policy_string(&format!("${{{key}}}"), &envs.env, None);
        let via_get = super::get(&format!("${{{key}}}"));
        std::env::remove_var(key);
        assert_ne!(
            expanded, "attacker.example",
            "a variable set AFTER the startup freeze must not reach POLICY expansion"
        );
        // The candidate-config side is the deliberate contrast: `byo(e)` is
        // `bY(e)` with NO env argument (@231815041's neighbourhood), i.e. the
        // LIVE process env, while policy predicates go through
        // `uWu(e, cWu(), …)`. Asserting both directions here keeps a future
        // "unify these two" refactor from silently re-opening the hole.
        assert_eq!(
            via_get, "attacker.example",
            "the CANDIDATE side expands against the live env, by design"
        );
    }
    use super::*;

    fn write(dir: &tempfile::TempDir, name: &str, body: &str) -> PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn absent_file_is_not_active() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!enterprise_mcp_active_at(
            &dir.path().join("managed-mcp.json")
        ));
    }

    #[test]
    fn valid_json_object_is_active() {
        let dir = tempfile::tempdir().unwrap();
        // Any valid JSON object → config non-null → active (matches `$7t`,
        // which does not require an `mcpServers` key at this gate).
        let p = write(
            &dir,
            "managed-mcp.json",
            r#"{"mcpServers":{"corp":{"type":"stdio","command":"c"}}}"#,
        );
        assert!(enterprise_mcp_active_at(&p));
        let p2 = write(&dir, "empty-obj.json", "{}");
        assert!(enterprise_mcp_active_at(&p2));
    }

    #[test]
    fn empty_or_malformed_or_non_object_is_not_active() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!enterprise_mcp_active_at(&write(&dir, "empty.json", "")));
        assert!(!enterprise_mcp_active_at(&write(&dir, "ws.json", "   \n ")));
        assert!(!enterprise_mcp_active_at(&write(
            &dir,
            "bad.json",
            "{not json"
        )));
        // A valid JSON value that is not an object → null config.
        assert!(!enterprise_mcp_active_at(&write(
            &dir, "arr.json", "[1,2,3]"
        )));
        assert!(!enterprise_mcp_active_at(&write(
            &dir, "str.json", "\"hi\""
        )));
    }

    #[test]
    fn message_is_byte_exact() {
        assert_eq!(
            ENTERPRISE_EXCLUSIVE_CONTROL_MESSAGE,
            "Cannot add MCP server: enterprise MCP configuration is active and has exclusive control over MCP servers"
        );
    }

    #[test]
    fn managed_mcp_path_is_managed_dir_plus_filename() {
        // Isolate from any real managed dir via the env override.
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded test; restored immediately after.
        let prev = std::env::var_os(MANAGED_DIR_ENV);
        std::env::set_var(MANAGED_DIR_ENV, dir.path());
        assert_eq!(
            managed_mcp_config_path(),
            dir.path().join("managed-mcp.json")
        );
        match prev {
            Some(v) => std::env::set_var(MANAGED_DIR_ENV, v),
            None => std::env::remove_var(MANAGED_DIR_ENV),
        }
    }

    // ── bPe / gPe matcher tests ──

    fn matcher(json: &str) -> Vec<McpServerMatcher> {
        serde_json::from_str(json).unwrap()
    }
    fn stdio(cmd: &str, args: &[&str]) -> Value {
        serde_json::json!({"type":"stdio","command":cmd,"args":args})
    }
    fn http(u: &str) -> Value {
        serde_json::json!({"type":"http","url":u})
    }

    #[test]
    fn deny_by_name_command_url() {
        // name
        let p = McpPolicy {
            denied: Some(matcher(r#"[{"serverName":"corp"}]"#)),
            allowed: None,
        };
        assert!(is_denied("corp", &stdio("c", &[]), &p));
        assert!(!is_denied("other", &stdio("c", &[]), &p));
        // command (exact [command, ...args])
        let p = McpPolicy {
            denied: Some(matcher(r#"[{"serverCommand":["npx","-y","evil"]}]"#)),
            allowed: None,
        };
        assert!(is_denied("x", &stdio("npx", &["-y", "evil"]), &p));
        assert!(!is_denied("x", &stdio("npx", &["-y", "good"]), &p));
        // url (UFr)
        let p = McpPolicy {
            denied: Some(matcher(r#"[{"serverUrl":"https://evil.example.com"}]"#)),
            allowed: None,
        };
        assert!(is_denied("x", &http("https://evil.example.com/mcp"), &p));
        assert!(!is_denied("x", &http("https://ok.example.com/mcp"), &p));
    }

    #[test]
    fn allow_semantics_absent_empty_and_fallback() {
        let cfg = stdio("c", &[]);
        // absent allowlist ⇒ everything allowed
        assert!(is_allowed("x", &cfg, &McpPolicy::default()));
        // empty allowlist ⇒ nothing allowed
        assert!(!is_allowed(
            "x",
            &cfg,
            &McpPolicy {
                denied: None,
                allowed: Some(vec![])
            }
        ));
        // allowlist has only name matchers (no command) ⇒ name fallback
        let p = McpPolicy {
            denied: None,
            allowed: Some(matcher(r#"[{"serverName":"corp"}]"#)),
        };
        assert!(is_allowed("corp", &cfg, &p));
        assert!(!is_allowed("nope", &cfg, &p));
        // deny overrides allow
        let p = McpPolicy {
            denied: Some(matcher(r#"[{"serverName":"corp"}]"#)),
            allowed: Some(matcher(r#"[{"serverName":"corp"}]"#)),
        };
        assert!(!is_allowed("corp", &cfg, &p));
    }

    #[test]
    fn allow_by_command_when_command_matchers_present() {
        // With a command matcher present, a stdio config matches by command, not name.
        let p = McpPolicy {
            denied: None,
            allowed: Some(matcher(r#"[{"serverCommand":["npx","corp-mcp"]}]"#)),
        };
        assert!(is_allowed("anything", &stdio("npx", &["corp-mcp"]), &p));
        assert!(!is_allowed("anything", &stdio("npx", &["other"]), &p));
    }

    #[test]
    fn ufr_star_and_exact_and_scheme() {
        assert!(url_matches("https://a.com/x", "*"));
        assert!(url_matches("https://a.com/mcp", "https://a.com"));
        assert!(url_matches("https://a.com:8443/mcp", "https://a.com:8443"));
        // wrong scheme / host / port
        assert!(!url_matches("http://a.com/mcp", "https://a.com"));
        assert!(!url_matches("https://b.com/mcp", "https://a.com"));
        assert!(!url_matches("https://a.com:9000/mcp", "https://a.com:8443"));
        // invalid config url
        assert!(!url_matches("not a url", "https://a.com"));
    }

    #[test]
    fn ufr_wildcards_host_port_path_protocol() {
        // host wildcard
        assert!(url_matches(
            "https://api.example.com/mcp",
            "https://*.example.com"
        ));
        assert!(!url_matches(
            "https://example.com/mcp",
            "https://*.example.com"
        ));
        // protocol wildcard
        assert!(url_matches("http://a.com/x", "*://a.com"));
        assert!(url_matches("https://a.com/x", "*://a.com"));
        // port wildcard
        assert!(url_matches("https://a.com:1234/x", "https://a.com:*"));
        assert!(url_matches("https://a.com:9999/x", "https://a.com:*"));
        // path wildcard
        assert!(url_matches(
            "https://a.com/team/mcp",
            "https://a.com/team/*"
        ));
        assert!(!url_matches(
            "https://a.com/other/mcp",
            "https://a.com/team/*"
        ));
    }

    #[test]
    fn ufr_case_insensitive_host_and_trailing_dot() {
        assert!(url_matches(
            "https://API.Example.COM/x",
            "https://api.example.com"
        ));
        assert!(url_matches("https://a.com./x", "https://a.com"));
    }

    #[test]
    fn policy_messages_are_byte_exact() {
        assert_eq!(
            denied_message("corp"),
            "Cannot add MCP server \"corp\": server is explicitly blocked by enterprise policy"
        );
        assert_eq!(
            not_allowed_message("corp"),
            "Cannot add MCP server \"corp\": not allowed by enterprise policy"
        );
    }

    #[test]
    fn read_managed_policy_folds_base_then_dropins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("managed-settings.json"),
            r#"{"deniedMcpServers":[{"serverName":"base-deny"}]}"#,
        )
        .unwrap();
        let d = dir.path().join("managed-settings.d");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("10-org.json"),
            r#"{"allowedMcpServers":[{"serverName":"corp"}]}"#,
        )
        .unwrap();
        std::fs::write(d.join("README.md"), "ignored").unwrap();
        let p = read_managed_mcp_policy_in(dir.path());
        assert!(is_denied("base-deny", &stdio("c", &[]), &p));
        assert!(is_allowed("corp", &stdio("c", &[]), &p));
        assert!(!is_allowed("stranger", &stdio("c", &[]), &p));
    }

    #[test]
    fn no_managed_policy_is_inert() {
        // Empty dir ⇒ no denied/allowed ⇒ nothing denied, everything allowed.
        let dir = tempfile::tempdir().unwrap();
        let p = read_managed_mcp_policy_in(dir.path());
        assert!(p.denied.is_none() && p.allowed.is_none());
        assert!(!is_denied("x", &stdio("c", &[]), &p));
        assert!(is_allowed("x", &stdio("c", &[]), &p));
    }

    // ── policy expansion env (2.1.219 `cWu`/`U__`/`uWu`/`dWu`) ──

    fn map(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn policy_env_snapshot_overlaid_by_managed_sources() {
        let dir = tempfile::tempdir().unwrap();
        // Base file env + a drop-in that overrides one key; NO_COLOR is
        // stripped (`ELt`) and non-string values never participate.
        std::fs::write(
            dir.path().join("managed-settings.json"),
            r#"{"env":{"CORP_HOST":"base.example.com","KEEP":"base","NO_COLOR":"1","NUM":7}}"#,
        )
        .unwrap();
        let d = dir.path().join("managed-settings.d");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("10-org.json"),
            r#"{"env":{"CORP_HOST":"drop.example.com"}}"#,
        )
        .unwrap();
        let snapshot = map(&[("CORP_HOST", "startup"), ("ONLY_SNAP", "snap")]);
        let envs = policy_expansion_env_with(dir.path(), &snapshot, &map(&[]), &map(&[]));
        // cWu: `{...NQr(), ...settingsEnv}` — managed values WIN over the
        // snapshot, drop-ins win over the base file.
        assert_eq!(
            envs.env.get("CORP_HOST").map(String::as_str),
            Some("drop.example.com")
        );
        assert_eq!(envs.env.get("KEEP").map(String::as_str), Some("base"));
        assert_eq!(envs.env.get("ONLY_SNAP").map(String::as_str), Some("snap"));
        assert!(!envs.env.contains_key("NO_COLOR"));
        assert!(!envs.env.contains_key("NUM"));
    }

    #[test]
    fn policy_env_fallback_fold_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("managed-settings.json"),
            r#"{"env":{"X":"managed"}}"#,
        )
        .unwrap();
        // U__: Object.assign(globalConfig, userSettings, …, policySettings) —
        // later (higher-trust) tiers win.
        let envs = policy_expansion_env_with(
            dir.path(),
            &map(&[]),
            &map(&[("X", "global"), ("G", "global")]),
            &map(&[("X", "user"), ("U", "user")]),
        );
        assert_eq!(
            envs.fallback_env.get("X").map(String::as_str),
            Some("managed")
        );
        assert_eq!(
            envs.fallback_env.get("G").map(String::as_str),
            Some("global")
        );
        assert_eq!(envs.fallback_env.get("U").map(String::as_str), Some("user"));
    }

    #[test]
    fn flag_settings_env_is_between_user_and_policy_for_deny_fallback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("managed-settings.json"),
            r#"{"env":{"POLICY":"managed","X":"managed"}}"#,
        )
        .unwrap();
        let envs = policy_expansion_env_with_flag_settings(
            dir.path(),
            &map(&[]),
            &map(&[("X", "global")]),
            &map(&[("X", "user"), ("USER", "user")]),
            &map(&[("X", "flag"), ("FLAG", "flag")]),
        );
        assert_eq!(
            envs.fallback_env.get("X").map(String::as_str),
            Some("managed")
        );
        assert_eq!(
            envs.fallback_env.get("FLAG").map(String::as_str),
            Some("flag")
        );
        assert_eq!(
            envs.fallback_env.get("USER").map(String::as_str),
            Some("user")
        );
    }

    #[test]
    fn deny_expands_against_policy_env_not_live_process_env() {
        let p = McpPolicy {
            denied: Some(matcher(
                r#"[{"serverCommand":["${CORP_CMD}","-y","corp"]}]"#,
            )),
            allowed: None,
        };
        // The var lives ONLY in the injected policy env (the live process env
        // has no CORP_CMD) — the deny predicate still resolves.
        let envs = PolicyExpansionEnv {
            env: map(&[("CORP_CMD", "npx")]),
            fallback_env: map(&[]),
        };
        assert!(is_denied_with_env(
            "x",
            &stdio("npx", &["-y", "corp"]),
            &p,
            &envs
        ));
        // Conversely a var set in the LIVE env (PATH always is) must NOT leak
        // into the policy side: with an empty policy env the `${PATH}x`
        // predicate stays literal and can never equal the live expansion.
        let p = McpPolicy {
            denied: Some(matcher(r#"[{"serverCommand":["${PATH}x"]}]"#)),
            allowed: None,
        };
        let live = format!("{}x", std::env::var("PATH").unwrap());
        let empty = PolicyExpansionEnv {
            env: map(&[]),
            fallback_env: map(&[]),
        };
        assert!(!is_denied_with_env("x", &stdio(&live, &[]), &p, &empty));
    }

    #[test]
    fn deny_consults_fallback_env_but_allow_does_not() {
        // `edt` expands with {env, fallbackEnv} (`uWu(c, n, o)`); `ZFe` gets
        // `cWu()` alone (`uWu(u, i)`).
        let denied = McpPolicy {
            denied: Some(matcher(r#"[{"serverCommand":["${TOOL}"]}]"#)),
            allowed: None,
        };
        let allowed = McpPolicy {
            denied: None,
            allowed: Some(matcher(r#"[{"serverCommand":["${TOOL}"]}]"#)),
        };
        let envs = PolicyExpansionEnv {
            env: map(&[]),
            fallback_env: map(&[("TOOL", "corp-tool")]),
        };
        let cfg = stdio("corp-tool", &[]);
        assert!(is_denied_with_env("x", &cfg, &denied, &envs));
        // Allow side: TOOL unresolved ⇒ `${TOOL}` stays literal ⇒ no match.
        assert!(!is_allowed_with_env("x", &cfg, &allowed, &envs));
    }

    #[test]
    fn allow_url_fails_closed_on_wildcard_valued_var_deny_unaffected() {
        // dWu: a `*` INJECTED by an expanded value marks the expansion unsafe
        // — the allowlist entry is skipped (`if(d)continue`), while a deny
        // entry keeps using the expanded pattern.
        let envs = PolicyExpansionEnv {
            env: map(&[("H", "*")]),
            fallback_env: map(&[]),
        };
        let allowed = McpPolicy {
            denied: None,
            allowed: Some(matcher(r#"[{"serverUrl":"https://${H}/mcp"}]"#)),
        };
        let cfg = http("https://anything.example.com/mcp");
        assert!(!is_allowed_with_env("x", &cfg, &allowed, &envs));
        let denied = McpPolicy {
            denied: Some(matcher(r#"[{"serverUrl":"https://${H}/mcp"}]"#)),
            allowed: None,
        };
        assert!(is_denied_with_env("x", &cfg, &denied, &envs));
    }

    #[test]
    fn allow_url_hostname_position_var_is_safe() {
        // The documented supported shape: `https://${HOST}/path` with a clean
        // hostname value expands safely and matches.
        let envs = PolicyExpansionEnv {
            env: map(&[("HOST", "corp.example.com")]),
            fallback_env: map(&[]),
        };
        let p = McpPolicy {
            denied: None,
            allowed: Some(matcher(r#"[{"serverUrl":"https://${HOST}/mcp"}]"#)),
        };
        assert!(is_allowed_with_env(
            "x",
            &http("https://corp.example.com/mcp"),
            &p,
            &envs
        ));
        assert!(!is_allowed_with_env(
            "x",
            &http("https://other.example.com/mcp"),
            &p,
            &envs
        ));
    }

    #[test]
    fn allow_whole_url_var_fails_closed() {
        // A whole-URL `${VAR}`: the masked pass (`zzenvsubzz`) no longer
        // parses as a URL while the real pass does ⇒ restructuring ⇒ unsafe.
        let envs = PolicyExpansionEnv {
            env: map(&[("URL", "https://corp.example.com")]),
            fallback_env: map(&[]),
        };
        let p = McpPolicy {
            denied: None,
            allowed: Some(matcher(r#"[{"serverUrl":"${URL}"}]"#)),
        };
        assert!(!is_allowed_with_env(
            "x",
            &http("https://corp.example.com/mcp"),
            &p,
            &envs
        ));
    }

    #[test]
    fn read_managed_policy_drops_invalid_entries() {
        let dir = tempfile::tempdir().unwrap();
        // gqn: allowed serverName must match /^[a-zA-Z0-9_-]+$/; _qn: denied
        // serverName must equal its trim; both: exactly one predicate field.
        std::fs::write(
            dir.path().join("managed-settings.json"),
            r#"{
                "allowedMcpServers":[
                    {"serverName":"bad name!"},
                    {"serverName":"ok","serverUrl":"https://x.com"},
                    {"serverName":"good_1"}
                ],
                "deniedMcpServers":[
                    {"serverName":" corp "},
                    {"serverName":"corp"}
                ]
            }"#,
        )
        .unwrap();
        let p = read_managed_mcp_policy_in(dir.path());
        assert_eq!(
            p.allowed
                .as_ref()
                .unwrap()
                .iter()
                .filter_map(|m| m.server_name.as_deref())
                .collect::<Vec<_>>(),
            ["good_1"]
        );
        assert_eq!(
            p.denied
                .as_ref()
                .unwrap()
                .iter()
                .filter_map(|m| m.server_name.as_deref())
                .collect::<Vec<_>>(),
            ["corp"]
        );
    }

    #[test]
    fn read_managed_policy_drops_only_the_mistyped_entry() {
        // `Xql`'s `.catch` is on the ELEMENT schema: a structurally invalid
        // entry is replaced by the `Jql` sentinel and filtered out, keeping its
        // valid siblings. Deserializing the list as one `Vec` used to collapse
        // every list here to `None` — allow-all + deny-nothing, i.e. fail OPEN.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("managed-settings.json"),
            r#"{
                "allowedMcpServers":[
                    {"serverCommand":"npx -y evil"},
                    "junk",
                    {"serverName":123},
                    {"serverUrl":null},
                    {"serverCommand":["npx","-y","corp"]},
                    {"serverName":"good_1"}
                ],
                "deniedMcpServers":[
                    17,
                    {"serverCommand":["npx",7]},
                    {"serverName":"corp"}
                ]
            }"#,
        )
        .unwrap();
        let p = read_managed_mcp_policy_in(dir.path());
        let allowed = p.allowed.as_ref().unwrap();
        assert_eq!(allowed.len(), 2, "valid siblings must survive");
        assert_eq!(
            allowed[0].server_command.as_deref(),
            Some(["npx".to_string(), "-y".to_string(), "corp".to_string()].as_slice())
        );
        assert_eq!(allowed[1].server_name.as_deref(), Some("good_1"));
        assert_eq!(
            p.denied
                .as_ref()
                .unwrap()
                .iter()
                .filter_map(|m| m.server_name.as_deref())
                .collect::<Vec<_>>(),
            ["corp"]
        );
        // …and the surviving allowlist still gates. A remote candidate falls to
        // the name branch (no `serverUrl` entry survived), so `good_1` is
        // admitted and an unlisted name is not.
        let envs = PolicyExpansionEnv {
            env: map(&[]),
            fallback_env: map(&[]),
        };
        let remote = http("https://x.test/mcp");
        assert!(is_allowed_with_env("good_1", &remote, &p, &envs));
        assert!(!is_allowed_with_env("rogue", &remote, &p, &envs));
        assert!(is_denied_with_env("corp", &remote, &p, &envs));
    }

    #[test]
    fn matcher_entry_errors_are_zod_byte_exact() {
        use MatcherListKind::{Allowed, Denied};
        let err = |raw: &str, kind| {
            parse_matcher_entry(&serde_json::from_str::<Value>(raw).unwrap(), kind).unwrap_err()
        };
        assert_eq!(
            err(r#""junk""#, Allowed),
            "Invalid input: expected object, received string"
        );
        assert_eq!(
            err(r#"{"serverName":123}"#, Allowed),
            "Invalid input: expected string, received number"
        );
        assert_eq!(
            err(r#"{"serverCommand":"npx -y foo"}"#, Denied),
            "Invalid input: expected array, received string"
        );
        assert_eq!(
            err(r#"{"serverCommand":["npx",7]}"#, Denied),
            "Invalid input: expected string, received number"
        );
        // `.optional()` is `undefined | T` — an explicit JSON null is a type
        // error, not an absent field.
        assert_eq!(
            err(r#"{"serverUrl":null}"#, Allowed),
            "Invalid input: expected string, received null"
        );
        // The FIELD type check precedes the object-level refine (zod reports
        // `issues[0]`, and field issues are collected in shape order).
        assert_eq!(
            err(
                r#"{"serverName":true,"serverUrl":"https://x.com"}"#,
                Allowed
            ),
            "Invalid input: expected string, received boolean"
        );
        assert_eq!(
            err(r#"{"serverCommand":[]}"#, Allowed),
            "Server command must have at least one element (the command)"
        );
    }

    #[test]
    fn non_array_allowed_list_denies_everything_and_denied_list_is_dropped() {
        // `RLi`'s array-level catches are asymmetric: `allowedMcpServers` falls
        // back to `[]` (and `ZFe`'s `length===0` ⇒ deny all) while
        // `deniedMcpServers` falls back to `undefined` (nothing enforced).
        //
        // §23a note: this exercises [`read_managed_mcp_policy_in`], the
        // narrow managed-tiers-only reader — NOT
        // [`McpPolicy::from_effective_settings`], whose `allowedMcpServers`
        // handling now depends on `allowManagedMcpServersOnly` (see
        // `effective_policy_merges_all_denies_and_only_managed_allows_when_locked`
        // / `effective_policy_merges_allow_from_every_tier_when_unlocked`
        // below). This narrow reader has no ordinary-tier sources to
        // consult in the first place, so it stays the managed-only
        // projection unconditionally; unaffected by that fix.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("managed-settings.json"),
            r#"{"allowedMcpServers":{},"deniedMcpServers":"nope"}"#,
        )
        .unwrap();
        let p = read_managed_mcp_policy_in(dir.path());
        assert!(
            p.allowed.as_ref().is_some_and(Vec::is_empty),
            "a non-array allowlist must fail CLOSED (empty allowlist), not vanish"
        );
        assert!(p.denied.is_none(), "a non-array denylist is dropped");
        let envs = PolicyExpansionEnv {
            env: map(&[]),
            fallback_env: map(&[]),
        };
        assert!(!is_allowed_with_env(
            "anything",
            &stdio("c", &[]),
            &p,
            &envs
        ));
        assert!(!is_denied_with_env("anything", &stdio("c", &[]), &p, &envs));
    }

    #[test]
    fn invalid_list_warnings_are_byte_exact() {
        assert_eq!(
            ALLOWED_LIST_INVALID,
            "\"allowedMcpServers\" was present but invalid; enforcing an empty allowlist (no MCP servers admitted) until it is fixed."
        );
        assert_eq!(
            DENIED_LIST_INVALID,
            "\"deniedMcpServers\" was present but invalid and was dropped; its entries cannot be enforced until it is fixed."
        );
    }

    // ── load-time (Ree) enforcement ──

    fn cfg(name: &str, spec: McpTransportSpec) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            spec,
            scope: ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
            disabled: false,
            timeout_ms: None,
            always_load: false,
            discovery_cache: None,
            tools: Vec::new(),
            tool_permissions: std::collections::BTreeMap::new(),
            config_error: None,
            metadata: Default::default(),
        }
    }
    fn managed_settings_dir_with(json: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("managed-settings.json"), json).unwrap();
        dir
    }

    /// The organization's second delivery channel loads, at `Managed` scope.
    #[test]
    fn managed_settings_servers_load_as_managed_scope() {
        let dir = managed_settings_dir_with(
            r#"{"managedMcpServers":{"org-http":{"type":"http","url":"https://x.test/mcp"},
                                     "org-sse":{"type":"sse","url":"https://x.test/sse"}}}"#,
        );
        let servers = super::load_managed_settings_servers_in(dir.path());
        assert_eq!(servers.len(), 2, "both remote entries load");
        for s in &servers {
            assert_eq!(
                s.scope,
                ConfigScope::Settings(lingxi_core::types::SettingsScope::Managed)
            );
            assert!(
                !s.metadata.expanded_from_env,
                "a managed-settings entry never reads the environment"
            );
        }
        // …and being Managed scope, they need no allowlist entry.
        assert!(is_server_allowed(
            &servers[0],
            &allowlist_of(&["something-else"])
        ));
    }

    /// 🚨 An organization may push a URL to every user. It may NOT push a
    /// program to run on their machine.
    #[test]
    fn a_stdio_entry_is_not_accepted_from_managed_settings() {
        let dir = managed_settings_dir_with(
            r#"{"managedMcpServers":{"local-exec":{"type":"stdio","command":"/bin/sh"},
                                     "ok":{"type":"http","url":"https://x.test/mcp"}}}"#,
        );
        let names: Vec<String> = super::load_managed_settings_servers_in(dir.path())
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["ok".to_string()], "only http/sse survive");
    }

    /// A managed settings document must not read the user's environment.
    #[test]
    fn a_managed_settings_entry_that_reads_the_env_is_rejected() {
        let dir = managed_settings_dir_with(
            r#"{"managedMcpServers":{"leaky":{"type":"http","url":"https://x.test/${LINGXI_TEST_NEVER_SET_XYZ}"},
                                     "ok":{"type":"http","url":"https://x.test/mcp"}}}"#,
        );
        let names: Vec<String> = super::load_managed_settings_servers_in(dir.path())
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["ok".to_string()]);
    }

    /// Claude Desktop's array form of the same-named key must not be read as if
    /// it were this one, and it takes the WHOLE key down: "No managed MCP
    /// servers are installed from it until it is fixed."
    ///
    /// The valid sibling in the drop-in tier is the point. Asserting only that
    /// an array installs nothing proves nothing — an implementation that merely
    /// IGNORED the array would pass that too, because there would be nothing
    /// left to install. The sibling is what separates "rejected the key" from
    /// "skipped a value I did not understand".
    #[test]
    fn claude_desktops_array_form_installs_nothing_at_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("managed-settings.json"),
            r#"{"managedMcpServers":[{"name":"org","transport":"http","url":"https://x.test/mcp"}]}"#,
        )
        .unwrap();
        let drop_in = dir.path().join("managed-settings.d");
        std::fs::create_dir_all(&drop_in).unwrap();
        std::fs::write(
            drop_in.join("10-extra.json"),
            r#"{"managedMcpServers":{"perfectly-fine":{"type":"http","url":"https://y.test/mcp"}}}"#,
        )
        .unwrap();

        assert!(
            super::load_managed_settings_servers_in(dir.path()).is_empty(),
            "a malformed managedMcpServers anywhere installs NO managed servers, \
             not merely none of its own"
        );

        // Control: with the malformed tier gone, the sibling does install — so
        // the assertion above is about the rejection, not about the fixture.
        std::fs::write(dir.path().join("managed-settings.json"), "{}").unwrap();
        let names: Vec<String> = super::load_managed_settings_servers_in(dir.path())
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["perfectly-fine".to_string()]);
    }

    #[test]
    fn a_server_name_outside_the_allowed_shape_is_dropped() {
        let dir = managed_settings_dir_with(
            r#"{"managedMcpServers":{"__proto__":{"type":"http","url":"https://x.test/a"},
                                     "has space":{"type":"http","url":"https://x.test/b"},
                                     "ok-name_1":{"type":"http","url":"https://x.test/c"}}}"#,
        );
        let names: Vec<String> = super::load_managed_settings_servers_in(dir.path())
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["ok-name_1".to_string()]);
    }

    /// The stamp itself: the flag the exemption reads must actually be set by
    /// the loader, from the RAW document, per entry. The tests above set it by
    /// hand; this one proves nothing has to.
    #[test]
    fn the_loader_stamps_which_entries_read_the_environment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("managed-mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{
                 "literal":{"type":"stdio","command":"/bin/srv"},
                 "reads-env":{"type":"stdio","command":"/bin/srv","args":["--t","${LINGXI_TEST_NEVER_SET_XYZ}"]},
                 "url-literal":{"type":"http","url":"https://x.test/mcp"},
                 "url-reads-env":{"type":"http","url":"https://x.test/${LINGXI_TEST_NEVER_SET_XYZ}"}
               }}"#,
        )
        .unwrap();

        let servers = super::load_enterprise_servers_at(&path);
        let flag = |name: &str| {
            servers
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("{name} must load"))
                .metadata
                .expanded_from_env
        };
        assert!(
            !flag("literal"),
            "a literal stdio entry reads no environment"
        );
        assert!(
            flag("reads-env"),
            "a variable reference in args is an environment read"
        );
        assert!(!flag("url-literal"));
        assert!(
            flag("url-reads-env"),
            "a variable reference in a url is an environment read"
        );
    }

    fn managed_cfg(name: &str, spec: McpTransportSpec, expanded_from_env: bool) -> McpServerConfig {
        let mut c = cfg(name, spec);
        c.scope = ConfigScope::Enterprise;
        c.metadata.expanded_from_env = expanded_from_env;
        c
    }

    fn allowlist_of(names: &[&str]) -> McpPolicy {
        McpPolicy {
            denied: None,
            allowed: Some(
                names
                    .iter()
                    .map(|n| McpServerMatcher {
                        server_name: Some((*n).to_string()),
                        server_command: None,
                        server_url: None,
                    })
                    .collect(),
            ),
        }
    }

    /// `F6`'s second line: an organization-delivered server that did NOT read
    /// the user's environment needs no `allowedMcpServers` entry.
    ///
    /// This port previously filtered every managed server through the
    /// allowlist, which is STRICTER than upstream: an organization shipping
    /// both `managed-mcp.json` and an allowlist had its own literal servers
    /// dropped unless it also listed them.
    #[test]
    fn an_org_delivered_server_that_reads_no_env_needs_no_allowlist_entry() {
        let policy = allowlist_of(&["something-else"]);

        assert!(
            is_server_allowed(
                &managed_cfg("org-tool", stdio_spec("/bin/srv", &[]), false),
                &policy
            ),
            "a literal managed entry is delivered by the organization itself"
        );

        // …and the exemption is scoped: an ordinary user-scope server with the
        // same shape is still governed by the list.
        assert!(
            !is_server_allowed(&cfg("org-tool", stdio_spec("/bin/srv", &[])), &policy),
            "the exemption is for organization-delivered servers only"
        );
    }

    /// 🚨 The conjunct that carries the security property. A managed entry that
    /// reads the environment is NOT fully determined by the document the
    /// organization signed off on, so it stays on the allowlist's hook.
    #[test]
    fn an_org_delivered_server_that_reads_env_is_still_checked() {
        let policy = allowlist_of(&["something-else"]);
        assert!(
            !is_server_allowed(
                &managed_cfg("org-tool", stdio_spec("/bin/srv", &[]), true),
                &policy
            ),
            "an entry that expanded a variable reference must still be listed"
        );
        // Listing it is enough — the rule adds a check, it does not deny.
        assert!(is_server_allowed(
            &managed_cfg("org-tool", stdio_spec("/bin/srv", &[]), true),
            &allowlist_of(&["org-tool"])
        ));
    }

    /// Upstream runs the denylist BEFORE the exemption (`if(Pme(e,n)) return !1`),
    /// so an organization cannot deliver its way past its own denylist.
    #[test]
    fn the_denylist_still_wins_over_the_exemption() {
        let policy = McpPolicy {
            denied: Some(vec![McpServerMatcher {
                server_name: Some("org-tool".to_string()),
                server_command: None,
                server_url: None,
            }]),
            allowed: None,
        };
        assert!(!is_server_allowed(
            &managed_cfg("org-tool", stdio_spec("/bin/srv", &[]), false),
            &policy
        ));
    }

    /// `Fxo`, on the raw entry and only on the fields that expand.
    #[test]
    fn entry_reads_environment_matches_fxo() {
        use serde_json::json;
        // stdio: command, args, env values
        assert!(entry_reads_environment(&json!({"command": "${HOME}/srv"})));
        assert!(entry_reads_environment(
            &json!({"command": "/s", "args": ["--t", "${TOKEN}"]})
        ));
        assert!(entry_reads_environment(
            &json!({"command": "/s", "env": {"K": "${V}"}})
        ));
        assert!(!entry_reads_environment(
            &json!({"command": "/s", "args": ["--t"]})
        ));
        // http/sse/ws: url, header values
        assert!(entry_reads_environment(
            &json!({"type": "http", "url": "https://x/${P}"})
        ));
        assert!(entry_reads_environment(
            &json!({"type": "sse", "url": "https://x", "headers": {"A": "Bearer ${T}"}})
        ));
        assert!(!entry_reads_environment(
            &json!({"type": "http", "url": "https://x"})
        ));
        // A `${VAR}` on a field that never expands is not an env read.
        assert!(!entry_reads_environment(
            &json!({"type": "http", "url": "https://x", "description": "${NOPE}"})
        ));
        // These types expand nothing at all.
        assert!(!entry_reads_environment(
            &json!({"type": "sdk", "url": "${X}"})
        ));
    }

    fn stdio_spec(command: &str, args: &[&str]) -> McpTransportSpec {
        McpTransportSpec::Stdio {
            command: command.to_string(),
            args: args.iter().map(|s| (*s).to_string()).collect(),
            env: Default::default(),
        }
    }

    #[test]
    fn is_server_allowed_reuses_matchers_and_sdk_shortcircuits() {
        let policy = McpPolicy {
            denied: Some(matcher(r#"[{"serverName":"corp"}]"#)),
            allowed: None,
        };
        assert!(!is_server_allowed(
            &cfg("corp", stdio_spec("c", &[])),
            &policy
        ));
        assert!(is_server_allowed(&cfg("ok", stdio_spec("c", &[])), &policy));
        // SDK-control servers are always kept (claude `type === "sdk"`).
        assert!(is_server_allowed(
            &cfg(
                "corp",
                McpTransportSpec::SdkControl {
                    control_channel_id: "x".into()
                }
            ),
            &policy
        ));
    }

    #[test]
    fn ree_retain_drops_denied_and_keeps_rest() {
        let mut list = vec![
            cfg("corp", stdio_spec("c", &[])),
            cfg("keep", stdio_spec("c", &[])),
        ];
        let policy = McpPolicy {
            denied: Some(matcher(r#"[{"serverName":"corp"}]"#)),
            allowed: None,
        };
        list.retain(|c| is_server_allowed(c, &policy));
        assert_eq!(
            list.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["keep"]
        );
    }

    // §23a (round 7 inverted this, corrected round 9): the port was
    // unconditionally managed-only for `allowedMcpServers` regardless of
    // `allowManagedMcpServersOnly`. That is over-strict, not a hole — a
    // user/project allowlist the oracle would honour (flag absent or false)
    // was silently dropped. `from_effective_settings` now models the flag
    // (claude's `ghr()`/`uAn()`); this test renamed from
    // `effective_policy_merges_all_denies_but_only_managed_allows` (which
    // asserted the over-strict behaviour unconditionally) to make the
    // managed-only branch explicit by SETTING the flag, and a new sibling
    // test below covers the corrected default (flag absent) branch.
    #[test]
    fn effective_policy_merges_all_denies_and_only_managed_allows_when_locked() {
        let ordinary = vec![serde_json::json!({
            "deniedMcpServers": [{"serverName": "user-deny"}],
            "allowedMcpServers": [{"serverName": "user-allow-must-be-ignored"}]
        })];
        let managed = vec![serde_json::json!({
            "allowManagedMcpServersOnly": true,
            "deniedMcpServers": [{"serverName": "managed-deny"}],
            "allowedMcpServers": [{"serverName": "managed-allow"}]
        })];
        let policy = McpPolicy::from_effective_settings(&ordinary, &managed);
        assert!(is_denied(
            "user-deny",
            &serde_json::json!({"command": "x"}),
            &policy
        ));
        assert!(is_denied(
            "managed-deny",
            &serde_json::json!({"command": "x"}),
            &policy
        ));
        assert!(is_allowed(
            "managed-allow",
            &serde_json::json!({"command": "x"}),
            &policy
        ));
        assert!(!is_allowed(
            "user-allow-must-be-ignored",
            &serde_json::json!({"command": "x"}),
            &policy
        ));
    }

    /// The §23a fix's core behaviour: with `allowManagedMcpServersOnly`
    /// absent (the default), a user/project-scope `allowedMcpServers` is
    /// honoured exactly like `deniedMcpServers` always is — merged in from
    /// every tier, not silently dropped.
    #[test]
    fn effective_policy_merges_allow_from_every_tier_when_unlocked() {
        let ordinary = vec![serde_json::json!({
            "allowedMcpServers": [{"serverName": "user-allow"}]
        })];
        let managed = vec![serde_json::json!({
            "allowedMcpServers": [{"serverName": "managed-allow"}]
        })];
        let policy = McpPolicy::from_effective_settings(&ordinary, &managed);
        assert!(is_allowed(
            "user-allow",
            &serde_json::json!({"command": "x"}),
            &policy
        ));
        assert!(is_allowed(
            "managed-allow",
            &serde_json::json!({"command": "x"}),
            &policy
        ));
        assert!(!is_allowed(
            "not-listed",
            &serde_json::json!({"command": "x"}),
            &policy
        ));
    }

    /// `allowManagedMcpServersOnly:false` is byte-identical to absent —
    /// merges allow from every tier.
    #[test]
    fn effective_policy_treats_explicit_false_flag_same_as_absent() {
        let ordinary = vec![serde_json::json!({
            "allowedMcpServers": [{"serverName": "user-allow"}]
        })];
        let managed = vec![serde_json::json!({"allowManagedMcpServersOnly": false})];
        let policy = McpPolicy::from_effective_settings(&ordinary, &managed);
        assert!(is_allowed(
            "user-allow",
            &serde_json::json!({"command": "x"}),
            &policy
        ));
    }

    /// claude's `.catch(() => true)`: a non-boolean
    /// `allowManagedMcpServersOnly` fails closed to the MORE restrictive
    /// managed-only reading, not to unrestricted.
    #[test]
    fn effective_policy_invalid_flag_value_fails_closed_to_managed_only() {
        let ordinary = vec![serde_json::json!({
            "allowedMcpServers": [{"serverName": "user-allow"}]
        })];
        let managed = vec![serde_json::json!({
            "allowManagedMcpServersOnly": "yes",
            "allowedMcpServers": [{"serverName": "managed-allow"}]
        })];
        let policy = McpPolicy::from_effective_settings(&ordinary, &managed);
        assert!(is_allowed(
            "managed-allow",
            &serde_json::json!({"command": "x"}),
            &policy
        ));
        assert!(!is_allowed(
            "user-allow",
            &serde_json::json!({"command": "x"}),
            &policy
        ));
    }

    #[test]
    fn allow_managed_mcp_servers_only_warning_is_byte_exact() {
        assert_eq!(
            ALLOW_MANAGED_MCP_SERVERS_ONLY_INVALID,
            "\"allowManagedMcpServersOnly\" was present but invalid; treating it as true until it is fixed."
        );
    }
}
