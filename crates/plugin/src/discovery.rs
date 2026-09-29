//! On-disk discovery of installed plugins (bootstrap, cache-only).
//!
//! Mirrors claude-code's cache-only loader. At startup claude-code calls
//! `loadAllPluginsCacheOnly()` (`main.tsx:282`), whose shared body is
//! `loadPluginsFromMarketplaces({cacheOnly})` (`pluginLoader.ts:1887`). For
//! each installed plugin it runs `createPluginFromPath()`
//! (`pluginLoader.ts:1348`): read `<pluginPath>/.lingxi-plugin/plugin.json`
//! (`loadPluginManifest`) and **auto-detect** the optional `commands/`,
//! `agents/`, `skills/`, `output-styles/` directories (Step 3,
//! `pluginLoader.ts:1373-1385`), plus the standard `hooks/hooks.json`
//! (`pluginLoader.ts:1618`).
//!
//! Two entry points:
//!
//! * [`discover_enabled_plugins`] is the BOOTSTRAP-faithful one: it reads the
//!   `settings.enabledPlugins` allowlist and resolves each enabled
//!   `name@marketplace` entry to its versioned cache path
//!   `cache/{marketplace}/{plugin}/{version}/` — the real layout
//!   `loadAllPluginsCacheOnly` consumes. Against a real `~/.lingxi/plugins`
//!   (which holds `cache/`, `npm-cache/`, `installed_plugins.json` — none with
//!   a direct `.lingxi-plugin/plugin.json`) this is what discovers the
//!   actually-installed plugins.
//!
//! * [`discover_installed_plugins`] is a flat directory-walk that loads any
//!   directory holding a direct `.lingxi-plugin/plugin.json` child. It is the
//!   primitive used by the local-path install arm (a pre-fetched plugin dir
//!   passed by path, as with `--add-dir`) — NOT the real cache layout. Against
//!   a real claude-code plugins dir it finds nothing, by design.
//!
//! Both share [`load_plugin_from_path`], which mirrors `createPluginFromPath`
//! (`pluginLoader.ts:1348`): read the manifest (Step 1) and auto-detect the
//! optional component directories (Step 3).
//!
//! [`crate::manager::PluginManager::enable`] then materializes the discovered
//! manifests' commands + hooks into the live registries.
//!
//! Residual (still NOT ported in either path): marketplace-catalog source
//! resolution and enterprise allow/blocklist policy. Seed-cache fallback and
//! source precedence are handled by the enabled/effective resolver. Enabled
//! cache entries use the exact version in `installed_plugins.json`; cache-only
//! entries without a record retain the single-version compatibility probe.

use crate::manifest::{
    BinaryPin, ComponentPath, HljsLanguageEntry, MonitorTrigger, PluginChannel, PluginComponents,
    PluginManifest, PluginMonitor, UserConfigField, UserConfigSchema,
};
use crate::source::PluginSource;
use crate::trust::default_trust_for_source;

use hooks::loader::parse_hooks_from_settings_json;
use hooks::HookSource;
use indexmap::IndexMap;
use lingxi_core::types::PluginId;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use telemetry::tengu::plugin as plugin_telemetry;
use telemetry::{AnalyticsBus, AnalyticsValue, LogEventMetadata, PiiTagged, Verified};

/// A leading UTF-8 byte-order mark, as some editors on Windows write it.
/// `serde_json` does not treat U+FEFF as whitespace, so every manifest/config
/// JSON string read in this file must have it stripped before parsing (same
/// convention as `command-api::markdown_loader::UTF8_BOM`,
/// `skill-api::frontmatter`, `agent::catalog`).
const UTF8_BOM: char = '\u{feff}';

/// Raw shape of `.lingxi-plugin/plugin.json`.
///
/// Mirrors claude-code's `PluginManifestSchema` (the full union of
/// `PluginManifestMetadataSchema` + component declaration fields):
/// - Identity: `name`, `version`, `description`, `author`, `homepage`.
/// - Component declarations: `skills`, `commands`, `agents`, `outputStyles`,
///   `hooks`, `mcpServers`, `lspServers`, `channels`.
/// - Metadata: `keywords`, `license`, `repository`, `metadata`.
///
/// `name` is required; all other fields are optional. An unrecognized
/// top-level field never breaks parsing here — it is captured into
/// [`Self::unknown_fields`], which normal validation
/// ([`scan_unknown_manifest_fields`]) reports and strict validation
/// ([`validate_manifest_fields`]) rejects (§19.1 "manifest field / default /
/// unknown-field / strict validation"). Component path fields follow Claude
/// Code's field-specific merge rules in [`detect_components`]: commands,
/// agents, output styles, and workflows replace their default directories;
/// skills extend the default; hooks, MCP, and LSP declarations merge with
/// their conventional files.
/// Binary: `PluginManifestSchema` in `schemas.ts`.
#[derive(Debug, Deserialize)]
struct RawManifest {
    name: String,
    /// UI-only label; identity and component namespaces continue to use
    /// `name`.
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    /// Activation fallback when no settings scope has made an explicit
    /// decision. Claude Code defaults this field to true.
    #[serde(rename = "defaultEnabled", default = "default_plugin_enabled")]
    default_enabled: bool,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    author: Option<RawAuthor>,
    #[serde(default)]
    homepage: Option<String>,
    /// Explicitly declared skill directories. These extend the default
    /// `skills/` directory. Binary: `skills` in `PluginManifestSchema`.
    #[serde(default)]
    skills: Option<PathDecl>,
    /// Explicitly declared command directories, OR (commands-only) an object
    /// map of command name to `{source|content, description?, argumentHint?,
    /// model?, allowedTools?}`. Binary: `commands` (`Rs`, `union([path,
    /// path[], record(string, Ds)])` — the record form is commands-only;
    /// `skills`/`agents`/`outputStyles` stay `union([path, path[]])`, hence
    /// the separate [`CommandsDecl`] type instead of widening [`PathDecl`].
    #[serde(default)]
    commands: Option<CommandsDecl>,
    /// Explicitly declared agent directories. Binary: `agents`.
    #[serde(default)]
    agents: Option<PathDecl>,
    /// Explicitly declared output-style directories. Binary: `outputStyles`.
    #[serde(rename = "outputStyles", default)]
    output_styles: Option<PathDecl>,
    /// Explicitly declared theme directories/files. Presence suppresses the
    /// `themes/` auto-scan (replaces, does not merge — same rule as
    /// `outputStyles`). Binary `pt`: `themes: union([path, path[]])`.
    #[serde(default)]
    themes: Option<PathDecl>,
    /// Explicitly declared workflow directories/`.js` files. Presence
    /// suppresses the `workflows/` auto-scan. Binary `Ls`: `workflows:
    /// union([path, path[]]).optional()`.
    #[serde(default)]
    workflows: Option<PathDecl>,
    /// Components whose manifest shape may still change (oracle `Vs`). The
    /// ONLY layer that accepts `syntaxHighlighting` — see
    /// [`RawExperimental`]. A top-level `syntaxHighlighting` key is not in
    /// `PluginManifestSchema` at all and is silently stripped, like any other
    /// unknown top-level field.
    #[serde(default)]
    experimental: Option<RawExperimental>,
    /// sha256-pinned files fetched into `bin/` at install time (oracle
    /// `qs`/`n1e`) — a lenient value; see [`resolve_binaries`].
    #[serde(default)]
    binaries: Option<Value>,
    /// Background watch scripts the host can arm as persistent Monitor tasks
    /// (oracle `mt`) — a `./…json` path or an inline strict array; see
    /// [`RawMonitorsDecl`].
    #[serde(default)]
    monitors: Option<RawMonitorsDecl>,
    /// Explicitly declared MCP server configs (manifest-declared MCP servers).
    /// Binary: `mcpServers` in `PluginManifestSchema`.
    #[serde(rename = "mcpServers", default)]
    mcp_servers: Option<serde_json::Value>,
    /// Explicitly declared LSP server configs. Binary: `lspServers`.
    #[serde(rename = "lspServers", default)]
    lsp_servers: Option<serde_json::Value>,
    /// Explicit hook declarations. Binary: `hooks`.
    #[serde(default)]
    hooks: Option<serde_json::Value>,
    /// User-configurable field declarations (`userConfig` in
    /// `PluginManifestSchema`) — each key maps to a `{description, sensitive,
    /// required, default, type}` object. Sensitive fields route through secure
    /// storage; non-sensitive fields through settings `pluginConfigs`. Binary
    /// `Fs`: `record(i().regex(/^[A-Za-z_]\w*$/, ...), gt())` — unlike the
    /// per-channel `userConfig` (`RawPluginChannel::user_config`, a bare
    /// `record(i(), gt())` with NO key-shape constraint), the TOP-LEVEL map's
    /// keys must be identifiers, hence the dedicated [`RawUserConfigMap`]
    /// wrapper instead of a bare `HashMap`.
    #[serde(rename = "userConfig", default)]
    user_config: Option<RawUserConfigMap>,
    /// Plugin settings shipped in the manifest.
    #[serde(default)]
    settings: Option<HashMap<String, serde_json::Value>>,
    /// Channel declarations. Binary: `channels`.
    #[serde(default)]
    channels: Option<Vec<RawPluginChannel>>,
    /// Dependency declarations. Binary: `dependencies`.
    #[serde(default)]
    dependencies: Option<serde_json::Value>,
    /// Plugin keywords. Binary: `keywords`.
    #[serde(default)]
    keywords: Option<Vec<String>>,
    /// SPDX license identifier. Binary: `license`.
    #[serde(default)]
    license: Option<String>,
    /// Source-code repository URL. Binary `Cs`: `repository:i().optional()`
    /// ("Source code repository URL") — a plain string, NOT the npm-style
    /// `{type,url,directory}` object this field's type previously assumed.
    #[serde(default)]
    repository: Option<String>,
    /// Free-form author-owned metadata, preserved unread. Binary `Cs`:
    /// `metadata:Sa((e)=>He(e)?e:void 0,De(i(),_e()).optional())` —
    /// `z.preprocess` maps anything that is not a plain JSON object to
    /// `undefined` before validating as `record(string, unknown)`, so an
    /// array/string/number/bool value here is silently dropped, not an
    /// error. Kept as a generic `Value` here; [`load_plugin_from_path_with_mcp_gate`]
    /// applies the same "object or nothing" filter before it reaches
    /// [`PluginManifest::metadata`].
    #[serde(default)]
    metadata: Option<serde_json::Value>,
    /// Optional previous plugin name used by local manifests that have been
    /// renamed in place. Marketplace-wide rename maps are loaded separately
    /// from `marketplace.json`; this field is intentionally not copied into
    /// the public manifest because it is install provenance, not identity.
    #[serde(default, alias = "renamedFrom", alias = "previousName")]
    rename_from: Option<String>,
}

fn default_plugin_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum PathDecl {
    One(String),
    Many(Vec<String>),
}

impl PathDecl {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(v) => vec![v],
            Self::Many(v) => v,
        }
    }
}

/// The `commands` manifest field only: `union([path, path[], record(string,
/// Ds)])` (oracle `Rs`). Forked from [`PathDecl`] rather than widening it
/// because `skills`/`agents`/`outputStyles` stay `union([path, path[]])` —
/// serde tries an `untagged` enum's variants in declaration order, and `Map`
/// must come after `One`/`Many` since a JSON array/string can never satisfy
/// `Map`'s object shape, so this ordering is unambiguous either way.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum CommandsDecl {
    One(String),
    Many(Vec<String>),
    Map(BTreeMap<String, RawCommandEntry>),
}

impl CommandsDecl {
    fn declared_count(&self) -> u32 {
        match self {
            Self::One(_) => 1,
            Self::Many(values) => values.len() as u32,
            Self::Map(entries) => entries.len() as u32,
        }
    }
}

/// One value of the `commands` object-map form (oracle `Ds`). Exactly one of
/// `source` (a markdown file path) / `content` (inline markdown) must be
/// present — enforced in [`RawCommandEntry`]'s `Deserialize` impl below so a
/// violation fails the surrounding `serde_json::from_str::<RawManifest>` call
/// the same way any other malformed `plugin.json` does (byte-matching the
/// oracle's `.refine()`, which fails the WHOLE `PluginManifestSchema.parse`
/// on one bad command entry — not just that entry).
#[derive(Debug, Clone)]
struct RawCommandEntry {
    source: Option<String>,
    /// Inline markdown body. Recognized here to accept the oracle-valid
    /// shape (so it no longer sinks the whole plugin), but not yet
    /// materialized into a command: `PluginManager::load_plugin`
    /// (`plugin/src/manager.rs`, not owned by this change) always reads a
    /// component's markdown off disk via `ComponentPath::path` and has no
    /// slot for a file-less inline body.
    #[allow(dead_code)]
    content: Option<String>,
    /// Command description override. Not yet threaded to the materialized
    /// command (same manager.rs limitation as `content`); parsed so a
    /// declaring plugin still loads instead of vanishing.
    #[allow(dead_code)]
    description: Option<String>,
    #[allow(dead_code)]
    argument_hint: Option<String>,
    #[allow(dead_code)]
    model: Option<String>,
    #[allow(dead_code)]
    allowed_tools: Option<Vec<String>>,
}

impl<'de> Deserialize<'de> for RawCommandEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            source: Option<String>,
            #[serde(default)]
            content: Option<String>,
            #[serde(default)]
            description: Option<String>,
            #[serde(rename = "argumentHint", default)]
            argument_hint: Option<String>,
            #[serde(default)]
            model: Option<String>,
            #[serde(rename = "allowedTools", default)]
            allowed_tools: Option<Vec<String>>,
        }
        let raw = Raw::deserialize(deserializer)?;
        match (&raw.source, &raw.content) {
            (Some(_), None) | (None, Some(_)) => {}
            _ => {
                return Err(serde::de::Error::custom(
                    "Command must have either \"source\" (file path) or \"content\" \
                     (inline markdown), but not both",
                ))
            }
        }
        Ok(RawCommandEntry {
            source: raw.source,
            content: raw.content,
            description: raw.description,
            argument_hint: raw.argument_hint,
            model: raw.model,
            allowed_tools: raw.allowed_tools,
        })
    }
}

/// The top-level `userConfig` map only (oracle `Fs`): `record(i().regex(
/// /^[A-Za-z_]\w*$/, "Option keys must be valid identifiers (letters, digits,
/// underscore; no leading digit) — they become CLAUDE_PLUGIN_OPTION_<KEY> env
/// vars in hooks"), gt())`. A bare `HashMap<String, UserConfigField>` would
/// accept any JSON-object key; this wrapper validates each key during
/// deserialization so a non-identifier key fails the WHOLE `plugin.json`
/// parse (matching zod's `.parse()` failing atomically), the same convention
/// [`RawCommandEntry`] establishes for a malformed `commands` entry. The
/// per-channel `userConfig` (`RawPluginChannel::user_config`) uses the oracle's
/// bare-string-key variant instead and stays a plain `HashMap`.
#[derive(Debug, Clone)]
struct RawUserConfigMap(HashMap<String, UserConfigField>);

impl<'de> Deserialize<'de> for RawUserConfigMap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = HashMap::<String, UserConfigField>::deserialize(deserializer)?;
        for key in raw.keys() {
            if !is_valid_user_config_key(key) {
                return Err(serde::de::Error::custom(format!(
                    "userConfig option key {key:?} must be a valid identifier \
                     (letters, digits, underscore; no leading digit)"
                )));
            }
        }
        Ok(RawUserConfigMap(raw))
    }
}

/// `^[A-Za-z_]\w*$`: an ASCII identifier — first character a letter or
/// underscore, the rest letters/digits/underscore. No unicode-identifier
/// leniency: the oracle regex's `\w` is ASCII-only (no `u` flag).
fn is_valid_user_config_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `author` may be a string or an object (`{ name, email, url }`). Binary
/// `ke`: `f({name:i().min(1,...), email:i().optional()..., url:i().optional()...})`
/// — the oracle's own author schema is object-only (`name` required), but
/// this port additionally accepts a bare string for leniency (unflagged by
/// the byte-alignment audit; kept as-is). Accept both shapes and preserve all
/// three sub-fields; a bare string has no email/url.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawAuthor {
    Name(String),
    Object {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        email: Option<String>,
        #[serde(default)]
        url: Option<String>,
    },
}

/// Public `plugin.json` channel shape. `userConfig` is a direct field map,
/// just like top-level `userConfig`; [`UserConfigSchema`] is the engine's
/// internal wrapper. `displayName` (binary `Bs`'s channel entry:
/// `displayName:i().optional().describe('Human-readable name shown in the
/// config dialog title (e.g., "Telegram"). Defaults to the server name.')`)
/// is UI-only label carried through to [`PluginChannel::display_name`].
#[derive(Debug, Clone, Deserialize)]
struct RawPluginChannel {
    server: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(rename = "userConfig", default)]
    user_config: Option<HashMap<String, UserConfigField>>,
}

/// `experimental` (oracle `Vs`, @154647440):
/// `f({experimental: Sa((e)=>He(e)?e:void 0, f({...pt().partial().shape,
/// ...Hs().partial().shape, ...mt().partial().shape, ...ct().partial().shape,
/// evals:…}).passthrough().optional()…)})`.
///
/// Two properties of that shape matter here:
/// - the preprocess drops a NON-OBJECT `experimental` to `undefined` instead
///   of rejecting it (the same `Sa((e)=>He(e)?e:void 0,…)` guard
///   `RawManifest::metadata` gets), and
/// - the inner object is `.passthrough()`, so an unrecognized key inside
///   `experimental` is kept rather than failing — but a key the schema DOES
///   declare must still validate, and `Hs` is `.strict()`, so a malformed
///   `experimental.syntaxHighlighting` fails the whole `plugin.json`.
///
/// `Hs` is referenced exactly once in the 2.1.251 binary — right here. The
/// plugin-manifest schema `jpe` (@154647878) composes
/// `Cs,zs,Rs,Us,Es,ct,pt,Ls,Bs,js,Ws,mt,Ys,Fs,qs,Vs` and does NOT include
/// it, and `jpe` is a plain `f()` (z.object with no catchall, unlike the
/// strict `ot`), so a TOP-LEVEL `syntaxHighlighting` is stripped, never
/// rejected. `claude plugin validate` says as much: "Unknown field
/// 'syntaxHighlighting'. Claude Code ignores it at load time."
///
/// `themes` and `syntaxHighlighting` are read out of `experimental` here.
/// `experimental.themes` is NOT a second spelling of a key this port already
/// reads elsewhere — it is the HIGHER-precedence one: the oracle's manifest
/// record builder resolves the theme declaration as
/// `A.experimental?.themes ?? A.themes` (@162826143) and suppresses the
/// `themes/` auto-scan on the same coalesced value
/// (`Ie=!(A.experimental?.themes??A.themes)&&me`, @162824976), so a plugin
/// that declares `experimental.themes` must be read from THERE and its
/// `themes/` folder ignored. Both layers exist because `pt`
/// (`f({themes: union([path, path[]])})`) is spread into BOTH `jpe` (the
/// top-level manifest) and `Vs`'s inner `experimental` object — the same
/// schema shape at two layers, with the experimental one winning.
///
/// `experimental`'s remaining declared keys (`hooks`, `evals`) are not
/// wired to anything in this port yet. The stable top-level `monitors` field
/// is resolved below and materialized by `PluginManager` when a task registry
/// is available.
#[derive(Debug, Clone, Default)]
struct RawExperimental {
    syntax_highlighting: Option<RawSyntaxHighlighting>,
    /// `experimental.themes` — wins over the top-level `themes` key.
    themes: Option<PathDecl>,
}

impl<'de> Deserialize<'de> for RawExperimental {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let Some(map) = value.as_object() else {
            return Ok(RawExperimental::default());
        };
        // Both keys are DECLARED members of the inner object schema, so a
        // present-but-malformed value fails the whole `plugin.json` even
        // though the object is `.passthrough()` for keys it does not declare.
        let syntax_highlighting = match map.get("syntaxHighlighting") {
            Some(raw) => {
                Some(RawSyntaxHighlighting::deserialize(raw).map_err(serde::de::Error::custom)?)
            }
            None => None,
        };
        let themes = match map.get("themes") {
            Some(raw) => Some(PathDecl::deserialize(raw).map_err(serde::de::Error::custom)?),
            None => None,
        };
        Ok(RawExperimental {
            syntax_highlighting,
            themes,
        })
    }
}

/// `experimental.syntaxHighlighting` (oracle `Hs`): `.strict()` at BOTH the
/// wrapper object and every `hljsLanguages` entry — an unknown key at either
/// level, an invalid `id`/`remote`/`integrity` shape, or more than
/// [`MAX_HLJS_LANGUAGES`] entries all fail the WHOLE `plugin.json` parse,
/// the same "one bad shape sinks the manifest" convention
/// [`RawCommandEntry`]/[`UserConfigField`] establish. `#[serde(deny_unknown_fields)]`
/// gives the wrapper-level strictness for free since it has exactly one
/// field. Reached only through [`RawExperimental`] — never from the top
/// level.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSyntaxHighlighting {
    #[serde(rename = "hljsLanguages")]
    hljs_languages: RawHljsLanguageList,
}

/// oracle `Os`: at most 16 `hljsLanguages` entries.
const MAX_HLJS_LANGUAGES: usize = 16;

/// Bounded `hljsLanguages` array (oracle `H(Ks()).max(Os)`), already
/// converted into the public [`HljsLanguageEntry`].
#[derive(Debug, Clone)]
struct RawHljsLanguageList(Vec<HljsLanguageEntry>);

impl<'de> Deserialize<'de> for RawHljsLanguageList {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = Vec::<RawHljsLanguageEntry>::deserialize(deserializer)?;
        if raw.len() > MAX_HLJS_LANGUAGES {
            return Err(serde::de::Error::custom(format!(
                "syntaxHighlighting.hljsLanguages must have at most {MAX_HLJS_LANGUAGES} entries, got {}",
                raw.len()
            )));
        }
        Ok(RawHljsLanguageList(
            raw.into_iter()
                .map(RawHljsLanguageEntry::into_entry)
                .collect(),
        ))
    }
}

#[derive(Debug, Clone)]
struct RawHljsLanguageEntry {
    id: String,
    remote: Option<String>,
    integrity: Option<String>,
}

impl RawHljsLanguageEntry {
    fn into_entry(self) -> HljsLanguageEntry {
        HljsLanguageEntry {
            id: self.id,
            remote: self.remote,
            integrity: self.integrity,
        }
    }
}

/// `^[a-z][a-z0-9_-]*$`, <=64 chars (oracle `As`).
fn is_valid_hljs_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 64 {
        return false;
    }
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// oracle `Ks.remote`'s `npm:` alternative:
/// `^npm:[@a-z0-9/._-]+(@[a-z0-9._+-]+)?$`.
static HLJS_REMOTE_NPM_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"^npm:[@a-z0-9/._-]+(@[a-z0-9._+-]+)?$").unwrap()
});

/// oracle `Ks.remote`'s `github:` alternative:
/// `^github:[\w.-]+\/[\w.-]+@[\w./-]+#.+\.js$`.
static HLJS_REMOTE_GITHUB_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"^github:[\w.-]+/[\w.-]+@[\w./-]+#.+\.js$").unwrap()
});

/// oracle `Ks.integrity`: `^sha(256|384|512)-[A-Za-z0-9+/=]+$`.
static HLJS_INTEGRITY_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^sha(256|384|512)-[A-Za-z0-9+/=]+$").unwrap());

/// `npm:<pkg>[@version]` or `github:<owner>/<repo>@<ref>#<path>.js`, <=256
/// chars (oracle `Ks.remote` regex).
fn is_valid_hljs_remote(remote: &str) -> bool {
    remote.len() <= 256
        && (HLJS_REMOTE_NPM_RE.is_match(remote) || HLJS_REMOTE_GITHUB_RE.is_match(remote))
}

/// `^sha(256|384|512)-[A-Za-z0-9+/=]+$`, <=512 chars (oracle `Ks.integrity`).
fn is_valid_hljs_integrity(integrity: &str) -> bool {
    integrity.len() <= 512 && HLJS_INTEGRITY_RE.is_match(integrity)
}

impl<'de> Deserialize<'de> for RawHljsLanguageEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            id: String,
            #[serde(default)]
            remote: Option<String>,
            #[serde(default)]
            integrity: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        if !is_valid_hljs_id(&raw.id) {
            return Err(serde::de::Error::custom(format!(
                "hljsLanguages entry \"id\" {:?} must match ^[a-z][a-z0-9_-]*$ (max 64 chars)",
                raw.id
            )));
        }
        if let Some(remote) = &raw.remote {
            if !is_valid_hljs_remote(remote) {
                return Err(serde::de::Error::custom(
                    "hljsLanguages entry \"remote\" must be npm:<pkg>[@ver] or \
                     github:<owner>/<repo>@<ref>#<path>.js (max 256 chars)",
                ));
            }
        }
        if let Some(integrity) = &raw.integrity {
            if !is_valid_hljs_integrity(integrity) {
                return Err(serde::de::Error::custom(
                    "hljsLanguages entry \"integrity\" must be SRI form: sha256-, sha384-, \
                     or sha512-<base64> (max 512 chars)",
                ));
            }
        }
        Ok(RawHljsLanguageEntry {
            id: raw.id,
            remote: raw.remote,
            integrity: raw.integrity,
        })
    }
}

/// `monitors` field only (oracle `mt`): `union([V(), kAn()])` — a
/// `./…json`-shaped path STRING, or an inline array of `.strict()` monitor
/// objects with unique names. Reading a declared PATH's file content happens
/// later ([`resolve_monitors`]) and is a runtime/filesystem concern (warn +
/// skip on failure, the same convention the sibling `mcpServers`/`hooks`
/// path forms use); only the SHAPE is checked here — a malformed inline
/// entry, a duplicate name, or a bare string that is neither `./`-prefixed
/// nor `.json`-suffixed — fails the WHOLE `plugin.json` parse.
#[derive(Debug, Clone)]
enum RawMonitorsDecl {
    Path(String),
    Inline(Vec<PluginMonitor>),
}

impl<'de> Deserialize<'de> for RawMonitorsDecl {
    // Oracle `V()`/`K()`: `i().startsWith("./")` + `.endsWith(".json")`, a
    // literal case-SENSITIVE suffix check — kept byte-exact rather than
    // `Path::extension()`-based, same rationale as `is_mcpb_source`.
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::String(s) => {
                if s.starts_with("./") && s.ends_with(".json") {
                    Ok(RawMonitorsDecl::Path(s))
                } else {
                    Err(serde::de::Error::custom(
                        "monitors path must start with \"./\" and end with \".json\"",
                    ))
                }
            }
            Value::Array(items) => {
                let entries: Vec<RawPluginMonitor> = serde_json::from_value(Value::Array(items))
                    .map_err(serde::de::Error::custom)?;
                let mut seen = BTreeSet::new();
                for entry in &entries {
                    if !seen.insert(entry.name.clone()) {
                        return Err(serde::de::Error::custom(
                            "Monitor names must be unique within a plugin",
                        ));
                    }
                }
                Ok(RawMonitorsDecl::Inline(
                    entries
                        .into_iter()
                        .map(RawPluginMonitor::into_monitor)
                        .collect(),
                ))
            }
            _ => Err(serde::de::Error::custom(
                "monitors must be a \"./…json\" path string or an array of monitor objects",
            )),
        }
    }
}

/// One `monitors` entry (oracle `$s`, a strict object).
#[derive(Debug, Clone)]
struct RawPluginMonitor {
    name: String,
    command: String,
    description: String,
    when: MonitorTrigger,
}

impl RawPluginMonitor {
    fn into_monitor(self) -> PluginMonitor {
        PluginMonitor {
            name: self.name,
            command: self.command,
            description: self.description,
            when: self.when,
        }
    }
}

impl<'de> Deserialize<'de> for RawPluginMonitor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            name: String,
            command: String,
            description: String,
            #[serde(default)]
            when: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.name.is_empty() {
            return Err(serde::de::Error::custom(
                "monitor \"name\" must not be empty",
            ));
        }
        if raw.command.is_empty() {
            return Err(serde::de::Error::custom(
                "monitor \"command\" must not be empty",
            ));
        }
        if raw.description.is_empty() {
            return Err(serde::de::Error::custom(
                "monitor \"description\" must not be empty",
            ));
        }
        let when = match raw.when.as_deref() {
            None => MonitorTrigger::Always,
            Some(w) => MonitorTrigger::parse(w).map_err(serde::de::Error::custom)?,
        };
        Ok(RawPluginMonitor {
            name: raw.name,
            command: raw.command,
            description: raw.description,
            when,
        })
    }
}

/// The three sub-fields of a parsed `author` value: display name, email,
/// url. A bare string author yields only a name.
impl RawAuthor {
    fn into_parts(self) -> (Option<String>, Option<String>, Option<String>) {
        match self {
            RawAuthor::Name(s) => (Some(s), None, None),
            RawAuthor::Object { name, email, url } => (name, email, url),
        }
    }
}

/// Parse a `plugin@marketplace` identifier into `(name, Option<marketplace>)`.
///
/// Faithful to claude-code's `parsePluginIdentifier`
/// (`pluginIdentifier.ts:51`): only the FIRST `@` separates name from
/// marketplace; anything after a second `@` is ignored. A bare `name` (no
/// `@`) yields `marketplace = None`.
fn parse_plugin_identifier(id: &str) -> (&str, Option<&str>) {
    match id.split_once('@') {
        Some((name, rest)) => {
            // `rest` may itself contain another `@`; keep only up to the next.
            let marketplace = rest.split('@').next().unwrap_or(rest);
            (name, Some(marketplace))
        }
        None => (id, None),
    }
}

/// Sanitize one path segment exactly as claude-code's `getVersionedCachePathIn`
/// (`pluginLoader.ts:139`) does: marketplace/plugin replace any char outside
/// `[A-Za-z0-9\-_]` with `-`; version additionally keeps `.`.
pub(crate) fn sanitize_segment(s: &str, allow_dot: bool) -> String {
    let mapped: String = s
        .chars()
        .map(|c| {
            let keep = c.is_ascii_alphanumeric() || c == '-' || c == '_' || (allow_dot && c == '.');
            if keep {
                c
            } else {
                '-'
            }
        })
        .collect();
    // A pure-dot or empty segment would resolve to the parent (`..`) or current
    // (`.`) directory, letting an attacker-controlled name/version escape its
    // cache subdir (e.g. a malicious plugin.json `"version": ".."` would make a
    // join resolve to the SIBLING cache dir, which an unconditional
    // remove_dir_all would then wipe). Collapse these to a safe token — no real
    // semver / plugin / marketplace segment is ever exactly "", ".", or "..".
    if mapped.is_empty() || mapped == "." || mapped == ".." {
        "-".to_string()
    } else {
        mapped
    }
}

/// Build the versioned cache path from the authoritative plugin identity and
/// installed version.
fn versioned_cache_path(
    plugins_dir: &Path,
    marketplace: &str,
    name: &str,
    version: &str,
) -> PathBuf {
    plugins_dir
        .join("cache")
        .join(sanitize_segment(marketplace, false))
        .join(sanitize_segment(name, false))
        .join(sanitize_segment(version, true))
}

/// Resolve a legacy or hand-written installPath relative to the plugins root.
/// The production writer normally persists an absolute path; accepting a
/// relative path keeps older records readable without changing the cache
/// layout.
fn recorded_install_path(plugins_dir: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        plugins_dir.join(path)
    }
}

/// Return authoritative cache candidates for one enabledPlugins key. Some
/// means the installed database contains a matching record, including when the
/// record is malformed or points at a missing cache; callers must not fall back
/// to probing in that case. None means this is an old cache-only installation
/// with no record for the enabled key.
fn exact_installed_paths(
    plugins_dir: &Path,
    entry_id: &str,
    records: Option<&Value>,
) -> Option<Vec<PathBuf>> {
    let (name, Some(marketplace)) = parse_plugin_identifier(entry_id) else {
        return None;
    };
    if name.is_empty() || marketplace.is_empty() {
        return None;
    }
    let Some(records) = records.and_then(Value::as_object) else {
        return None;
    };

    let canonical_id = format!("{name}@{marketplace}");
    if let Some(value) = records
        .get(entry_id)
        .or_else(|| records.get(canonical_id.as_str()))
    {
        let mut seen = HashSet::new();
        let paths = match value {
            Value::Array(entries) => entries
                .iter()
                .filter_map(|record| {
                    let version = record
                        .get("version")
                        .and_then(Value::as_str)
                        .filter(|version| !version.is_empty());
                    if let Some(version) = version {
                        Some(versioned_cache_path(
                            plugins_dir,
                            marketplace,
                            name,
                            version,
                        ))
                    } else {
                        record
                            .get("installPath")
                            .and_then(Value::as_str)
                            .map(|path| recorded_install_path(plugins_dir, path))
                    }
                })
                .filter(|path| seen.insert(path.clone()))
                .collect(),
            _ => Vec::new(),
        };
        return Some(paths);
    }

    // Older files keyed records by marketplace, then plugin name.
    let Some(marketplace_records) = records.get(marketplace).and_then(Value::as_object) else {
        return None;
    };
    let Some(record) = marketplace_records.get(name) else {
        return None;
    };
    let version = record
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let path = if !version.is_empty() {
        versioned_cache_path(plugins_dir, marketplace, name, version)
    } else if let Some(raw) = record.get("installPath").and_then(Value::as_str) {
        recorded_install_path(plugins_dir, raw)
    } else {
        return Some(Vec::new());
    };
    Some(vec![path])
}

/// Oracle `yt` (2.1.251, `@~154669075`):
/// `/[\p{Cc}\u200E\u200F\u202A-\u202E\u2066-\u2069]/u` — Unicode control
/// characters plus the bidi-formatting marks/embeddings/isolates a name has no
/// legitimate reason to carry (LRM/RLM, LRE/RLE/PDF/LRO/RLO, LRI/RLI/FSI/PDI).
/// Shared by both the plugin- and marketplace-name validators below.
fn control_or_bidi_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"[\p{Cc}\u{200E}\u{200F}\u{202A}-\u{202E}\u{2066}-\u{2069}]")
            .expect("control/bidi regex is valid")
    })
}

/// Standalone predicate over the same oracle `yt` character class, for a
/// caller (e.g. `plugin init`'s own pre-existing path-safety guard) that
/// wants just this one check without the rest of [`validate_plugin_name`]'s
/// chain (which would also start enforcing the no-spaces rule this port's
/// `plugin init` has never applied to a CLI-typed name).
#[must_use]
pub fn has_control_or_bidi_formatting(name: &str) -> bool {
    control_or_bidi_regex().is_match(name)
}

/// Oracle `se` (`schemas.ts`): the plugin-name validator (2.1.247 hardening +
/// the 2.1.201-era empty/space checks). Ported previously ONLY on the
/// authoring path (`plugin tag`/`plugin init`); §8 wires it into the actual
/// manifest LOAD path ([`load_plugin_from_path_with_mcp_gate`]) so a
/// malformed name — including a bidi/control-character name crafted to spoof
/// another plugin in `plugin list` output — is rejected instead of trusted
/// verbatim.
pub fn validate_plugin_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Plugin name cannot be empty".to_string());
    }
    if name.contains(' ') {
        return Err(
            "Plugin name cannot contain spaces. Use kebab-case (e.g., \"my-plugin\")".to_string(),
        );
    }
    if control_or_bidi_regex().is_match(name) {
        return Err(
            "Plugin name cannot contain control or bidirectional-formatting characters".to_string(),
        );
    }
    Ok(())
}

/// Marketplace names literally reserved for Anthropic's own catalogs (oracle
/// `goe`, the union of `Kqt` ∪ `lYe`). Exact (case-insensitive) membership
/// here EXEMPTS a name from the impersonation check below — the oracle
/// enforces that these specific names may only be registered from an
/// `anthropics/*` GitHub/git source elsewhere (`validateOfficialNameSource`,
/// §16), which is deferred alongside the `command` plugin-entry source this
/// port does not yet have.
const RESERVED_OFFICIAL_MARKETPLACE_NAMES: &[&str] = &[
    "claude-code-marketplace",
    "claude-code-plugins",
    "claude-plugins-official",
    "anthropic-marketplace",
    "anthropic-plugins",
    "agent-skills",
    "anthropic-agent-skills",
    "life-sciences",
    "knowledge-work-plugins",
    "claude-for-legal",
    "claude-for-financial-services",
    "financial-services-plugins",
    "first-party-plugins",
    "claude-community",
    "claude-plugins-community",
    "healthcare",
];

/// Oracle `ws` (2.1.251): case-insensitive impersonation pattern —
/// `/(?:official[^a-z0-9]*(anthropic|claude)|(?:anthropic|claude)[^a-z0-9]*official|^(?:anthropic|claude)[^a-z0-9]*(marketplace|plugins|official))/i`.
fn impersonation_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::RegexBuilder::new(
            r"(?:official[^a-z0-9]*(anthropic|claude)|(?:anthropic|claude)[^a-z0-9]*official|^(?:anthropic|claude)[^a-z0-9]*(marketplace|plugins|official))",
        )
        .case_insensitive(true)
        .build()
        .expect("impersonation regex is valid")
    })
}

/// Oracle `RAn(e)` (2.1.251): a reserved literal name (exact, case-insensitive
/// match against [`RESERVED_OFFICIAL_MARKETPLACE_NAMES`]) is never flagged
/// here (its legitimacy is judged by source instead, §16); otherwise ANY
/// non-printable-ASCII character (`Ps`: `/[^ -~]/`, a broader net
/// than the control/bidi check above — it also catches homoglyph/Unicode
/// impersonation attempts) or the impersonation pattern itself is a match.
fn is_impersonating_official_marketplace(name: &str) -> bool {
    if RESERVED_OFFICIAL_MARKETPLACE_NAMES.contains(&name.to_ascii_lowercase().as_str()) {
        return false;
    }
    if name.chars().any(|c| !(' '..='~').contains(&c)) {
        return true;
    }
    impersonation_regex().is_match(name)
}

/// Names reserved for this port's internal scope kinds (oracle `lt`: `inline`
/// / `builtin` / `skills-dir` / `synced`) — a marketplace cannot be
/// registered under one of these literally, since they identify where a
/// plugin *record* came from rather than a real catalog.
fn reserved_internal_scope_description(name: &str) -> Option<&'static str> {
    match name {
        "inline" => Some("--plugin-dir session plugins"),
        "builtin" => Some("built-in plugins"),
        "skills-dir" => Some("plugins auto-loaded from .claude/skills/"),
        "synced" => Some("plugins synced from your claude.ai account"),
        _ => None,
    }
}

/// Oracle `ut` (`schemas.ts`): the marketplace-name validator. Ported
/// previously nowhere at all — §8's structural gap is that discovery,
/// install, and marketplace ingestion never called it. Wired into
/// `plugin_marketplace::run_add`'s three admission paths (directory / github+
/// git clone / hosted URL), all of which resolve to a THIRD-PARTY-CONTROLLED
/// name (the local `marketplace.json`'s own `name` field, or the cloned/
/// fetched catalog's declared name) before it is written to the registry.
pub fn validate_marketplace_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Marketplace must have a name".to_string());
    }
    if name.contains(' ') {
        return Err(
            "Marketplace name cannot contain spaces. Use kebab-case (e.g., \"my-marketplace\")"
                .to_string(),
        );
    }
    if control_or_bidi_regex().is_match(name) {
        return Err(
            "Marketplace name cannot contain control or bidirectional-formatting characters"
                .to_string(),
        );
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") || name == "." {
        return Err(
            "Marketplace name cannot contain path separators (/ or \\), \"..\" sequences, or be \".\""
                .to_string(),
        );
    }
    if is_impersonating_official_marketplace(name) {
        return Err(
            "Marketplace name impersonates an official Anthropic/Claude marketplace".to_string(),
        );
    }
    let lower = name.to_ascii_lowercase();
    if let Some(kind) = reserved_internal_scope_description(&lower) {
        return Err(format!(
            "Marketplace name \"{lower}\" is reserved for {kind}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod name_validation_tests {
    use super::*;

    #[test]
    fn plugin_name_rejects_empty_spaces_and_control_bidi() {
        assert_eq!(
            validate_plugin_name(""),
            Err("Plugin name cannot be empty".to_string())
        );
        assert_eq!(
            validate_plugin_name("my plugin"),
            Err(
                "Plugin name cannot contain spaces. Use kebab-case (e.g., \"my-plugin\")"
                    .to_string()
            )
        );
        assert_eq!(
            validate_plugin_name("my-plugin\u{202E}evil"),
            Err(
                "Plugin name cannot contain control or bidirectional-formatting characters"
                    .to_string()
            )
        );
        assert_eq!(
            validate_plugin_name("my-plugin\u{0007}"),
            Err(
                "Plugin name cannot contain control or bidirectional-formatting characters"
                    .to_string()
            )
        );
        assert!(validate_plugin_name("my-plugin").is_ok());
    }

    #[test]
    fn marketplace_name_rejects_empty_spaces_control_bidi_and_path_segments() {
        assert_eq!(
            validate_marketplace_name(""),
            Err("Marketplace must have a name".to_string())
        );
        assert_eq!(
            validate_marketplace_name("my market"),
            Err(
                "Marketplace name cannot contain spaces. Use kebab-case (e.g., \"my-marketplace\")"
                    .to_string()
            )
        );
        assert_eq!(
            validate_marketplace_name("evil\u{200E}name"),
            Err(
                "Marketplace name cannot contain control or bidirectional-formatting characters"
                    .to_string()
            )
        );
        for bad in ["a/b", "a\\b", "a..b", "."] {
            assert_eq!(
                validate_marketplace_name(bad),
                Err(
                    "Marketplace name cannot contain path separators (/ or \\), \"..\" sequences, or be \".\""
                        .to_string()
                ),
                "expected {bad:?} to be rejected"
            );
        }
        assert!(validate_marketplace_name("my-marketplace").is_ok());
    }

    #[test]
    fn marketplace_name_rejects_impersonation_of_an_official_marketplace() {
        for bad in [
            "anthropic-official",
            "claude-official-store",
            "official-anthropic-tools",
            "anthropic-marketplace-mirror",
            "claude-plugins",
        ] {
            assert_eq!(
                validate_marketplace_name(bad),
                Err(
                    "Marketplace name impersonates an official Anthropic/Claude marketplace"
                        .to_string()
                ),
                "expected {bad:?} to be rejected as impersonation"
            );
        }
        // An exact reserved literal is exempted HERE (judged by source instead).
        assert!(validate_marketplace_name("claude-code-marketplace").is_ok());
        // An ordinary name mentioning neither anthropic nor claude is fine.
        assert!(validate_marketplace_name("acme-plugins").is_ok());
    }

    #[test]
    fn marketplace_name_rejects_reserved_internal_scope_names() {
        assert_eq!(
            validate_marketplace_name("inline"),
            Err(
                "Marketplace name \"inline\" is reserved for --plugin-dir session plugins"
                    .to_string()
            )
        );
        assert_eq!(
            validate_marketplace_name("SKILLS-DIR"),
            Err(
                "Marketplace name \"skills-dir\" is reserved for plugins auto-loaded from .claude/skills/"
                    .to_string()
            )
        );
    }
}

/// Discover the plugins enabled by the `enabledPlugins` allowlist against the
/// REAL claude-code on-disk layout.
///
/// claude-code never flat-walks the plugins directory for manifests. Its
/// cache-only loader `loadAllPluginsCacheOnly()` (`main.tsx:282`) delegates to
/// `loadPluginsFromMarketplaces({cacheOnly})` (`pluginLoader.ts:1888`), which
/// is driven by `settings.enabledPlugins` — a map of `plugin@marketplace` →
/// enabled (`pluginLoader.ts:1898-1906`). Each enabled `name@marketplace`
/// entry resolves through `getVersionedCachePath` (`pluginLoader.ts:139`) to
/// the versioned cache directory
/// `<plugins>/cache/{marketplace}/{plugin}/{version}/`
/// whose `.lingxi-plugin/plugin.json` is then read by `createPluginFromPath`
/// (`pluginLoader.ts:1348`).
///
/// This port reads the `enabled` allowlist, skips disabled entries, and resolves
/// each remaining `name@marketplace` from the matching `installed_plugins.json`
/// record's exact version. Bare `name` entries (no marketplace) and
/// uninstalled/missing entries are skipped — never a flat walk. For old
/// cache-only installations with no matching record, the single-version probe
/// remains as a compatibility fallback.
///
/// What is still NOT ported (residual): marketplace-catalog source resolution
/// and enterprise allow/blocklist policy (`getStrictKnownMarketplaces` /
/// `getBlockedMarketplaces`). Seed-dir fallback is resolved before the primary
/// cache is loaded, with the same source precedence used by the final merge.
pub async fn discover_enabled_plugins(
    plugins_dir: &Path,
    enabled: &BTreeMap<String, bool>,
) -> Vec<(PluginId, PluginManifest, PathBuf)> {
    discover_enabled_plugins_with_bus(plugins_dir, enabled, None).await
}

pub async fn discover_enabled_plugins_with_bus(
    plugins_dir: &Path,
    enabled: &BTreeMap<String, bool>,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Vec<(PluginId, PluginManifest, PathBuf)> {
    discover_enabled_plugins_impl(plugins_dir, enabled, analytics_bus, true).await
}

async fn discover_enabled_plugins_impl(
    plugins_dir: &Path,
    enabled: &BTreeMap<String, bool>,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    resolve_collisions: bool,
) -> Vec<(PluginId, PluginManifest, PathBuf)> {
    let cache_root = plugins_dir.join("cache");
    let seed_dirs = plugin_seed_dirs();
    let installed_records = crate::installed::load_normalized(plugins_dir)
        .and_then(|value| value.get("plugins").cloned());
    let mut out = Vec::new();
    for (entry_id, is_enabled) in enabled {
        if !is_enabled {
            continue;
        }
        let (name, marketplace) = parse_plugin_identifier(entry_id);
        // Marketplace-qualified entries only — a bare name has no resolvable
        // cache path (claude-code skips non-`plugin@marketplace` keys via the
        // `PluginIdSchema` filter, `pluginLoader.ts:1909`).
        let Some(marketplace) = marketplace else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let mut candidates = if let Some(exact) =
            exact_installed_paths(plugins_dir, entry_id, installed_records.as_ref())
        {
            exact
        } else {
            let plugin_cache_dir = cache_root
                .join(sanitize_segment(marketplace, false))
                .join(sanitize_segment(name, false));
            resolve_installed_version_dir(&plugin_cache_dir)
                .await
                .into_iter()
                .collect()
        };
        // Claude probes the primary cache first and only consults the
        // configured seed cache when that exact cache entry is absent. Keep
        // that distinction: a malformed primary plugin must not silently turn
        // into a different seed plugin, while a missing primary version can
        // still boot from the administrator-provided seed tree.
        let primary_exists = candidates.iter().any(|path| path_exists(path));
        let seed_candidates = seed_plugin_cache_candidates(marketplace, name, &candidates).await;
        if !primary_exists {
            for seed in &seed_candidates {
                if !candidates.contains(seed) {
                    candidates.push(seed.clone());
                }
            }
        }
        for versioned in candidates {
            let mut allowed_roots = Vec::with_capacity(seed_dirs.len() + 1);
            allowed_roots.push(plugins_dir);
            allowed_roots.extend(seed_dirs.iter().map(PathBuf::as_path));
            let Some(versioned) = confined_plugin_install_dir(&versioned, &allowed_roots).await
            else {
                continue;
            };
            if let Some((id, manifest)) = load_plugin_from_path_with_mcp_gate_and_bus(
                &versioned,
                false,
                Some(entry_id.as_str()),
                analytics_bus,
            )
            .await
            {
                out.push((id, manifest, versioned));
            }
        }
        if primary_exists {
            // A present primary cache shadows a seed folder rather than
            // loading both. Still surface the folder-shadowed diagnostic for
            // each lower-precedence component, as Claude does when the source
            // resolver discards a same-name folder.
            for seed in seed_candidates {
                let Some((_, seed_manifest)) = load_plugin_from_path_with_mcp_gate_and_bus(
                    &seed,
                    false,
                    Some(entry_id.as_str()),
                    None,
                )
                .await
                else {
                    continue;
                };
                let seed_inventory =
                    plugin_component_inventory_for_path(&seed_manifest, &seed).await;
                let primary_plugins = out
                    .iter()
                    .filter(|(_, manifest, path)| {
                        *path != seed && manifest.name.eq_ignore_ascii_case(&seed_manifest.name)
                    })
                    .map(|(_, manifest, path)| (manifest.clone(), path.clone()))
                    .collect::<Vec<_>>();
                for (primary_manifest, _primary_path) in primary_plugins {
                    let primary_inventory =
                        plugin_component_inventory_for_path(&primary_manifest, &_primary_path)
                            .await;
                    for kind in seed_inventory.keys() {
                        if seed_inventory
                            .get(kind)
                            .zip(primary_inventory.get(kind))
                            .is_some_and(|(seed_names, primary_names)| {
                                seed_names.iter().any(|name| primary_names.contains(name))
                            })
                        {
                            emit_folder_shadowed_event(analytics_bus, kind, &seed_manifest, &seed)
                                .await;
                        }
                    }
                }
            }
        }
    }
    if resolve_collisions {
        resolve_discovered_plugins(out, analytics_bus).await
    } else {
        out.sort_by(|left, right| left.1.name.cmp(&right.1.name));
        out
    }
}

/// Probe `cache/{marketplace}/{plugin}/` for an installed version directory.
///
/// This is only used when no matching `installed_plugins.json` record exists.
/// If the plugin dir holds exactly one version subdirectory with content, use
/// it for compatibility with old cache-only installations. Zero or multiple
/// versions are ambiguous and return `None`.
async fn resolve_installed_version_dir(plugin_dir: &Path) -> Option<PathBuf> {
    let mut entries = tokio::fs::read_dir(plugin_dir).await.ok()?;
    let mut version_dirs = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Ok(file_type) = entry.file_type().await {
            if file_type.is_dir() && !file_type.is_symlink() {
                let path = entry.path();
                if tokio::fs::canonicalize(&path).await.is_ok() {
                    version_dirs.push(path);
                }
            }
        }
    }
    if version_dirs.len() == 1 {
        Some(version_dirs.into_iter().next().unwrap())
    } else {
        None
    }
}

fn path_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_dir())
        .unwrap_or(false)
}

/// Resolve a missing cache entry from the ordered seed roots. The seed layout
/// is `<seed>/cache/<marketplace>/<plugin>/<version>`, exactly the path used by
/// Claude's `oUt`/`sae` helpers. The first seed root with a usable version wins.
async fn seed_plugin_cache_candidates(
    marketplace: &str,
    name: &str,
    primary_candidates: &[PathBuf],
) -> Vec<PathBuf> {
    let versions = primary_candidates
        .iter()
        .filter_map(|path| path.file_name().and_then(|value| value.to_str()))
        .filter(|version| !version.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let mut out = Vec::new();
    for seed in plugin_seed_dirs() {
        let plugin_cache = seed
            .join("cache")
            .join(sanitize_segment(marketplace, false))
            .join(sanitize_segment(name, false));
        let mut found = Vec::new();
        if versions.is_empty() {
            if let Some(version) = resolve_installed_version_dir(&plugin_cache).await {
                found.push(version);
            }
        } else {
            for version in &versions {
                let candidate = plugin_cache.join(sanitize_segment(version, true));
                if path_exists(&candidate) {
                    found.push(candidate);
                }
            }
        }
        if !found.is_empty() {
            out.extend(found);
            break;
        }
    }
    out
}

/// Re-discover every plugin recorded in `installed_plugins.json`, resolving each
/// to its exact versioned cache dir `cache/<marketplace>/<plugin>/<version>/`.
///
/// This is the durable counterpart to [`crate::manager::PluginManager`]'s
/// record-on-install: the records carry the authoritative `(marketplace,
/// plugin, version)` triple, so resolution is exact (no single-version probe).
/// Each segment is sanitized identically to the install-time path so the read
/// path matches the write path. A record whose cache dir is missing / has no
/// manifest is skipped (resilient to a hand-deleted cache). Returned tuples are
/// `(freshly-minted id, manifest, install dir)`, sorted by plugin name.
async fn discover_recorded_plugins_identified(
    plugins_dir: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    resolve_collisions: bool,
) -> Vec<(String, PluginId, PluginManifest, PathBuf)> {
    let mut out = Vec::new();

    // v2 schema (claude-code 2.1.201): `plugins["<plugin>@<market>"] = [ {scope,
    // installPath, version, installedAt, lastUpdated} ]`. Each record carries the
    // exact `installPath` cache dir, so resolution is direct. Read the raw JSON so
    // the v2 array shape and the legacy `plugins[market][plugin]` object shape can
    // coexist during migration.
    if let Some(records) = crate::installed::load_normalized(plugins_dir)
        .and_then(|v| v.get("plugins").and_then(|p| p.as_object()).cloned())
    {
        let cache_root = plugins_dir.join("cache");
        for (key, value) in &records {
            match value {
                // v2: array of per-scope records; the version is authoritative
                // for the cache path. Fall back to installPath only for older
                // records that did not persist a version.
                serde_json::Value::Array(recs) => {
                    let (name, marketplace) = parse_plugin_identifier(key);
                    let mut seen = HashSet::new();
                    for rec in recs {
                        let dir = rec
                            .get("version")
                            .and_then(Value::as_str)
                            .filter(|version| !version.is_empty())
                            .and_then(|version| {
                                marketplace.map(|marketplace| {
                                    versioned_cache_path(plugins_dir, marketplace, name, version)
                                })
                            })
                            .or_else(|| {
                                rec.get("installPath")
                                    .and_then(Value::as_str)
                                    .map(|path| recorded_install_path(plugins_dir, path))
                            });
                        let Some(dir) = dir else {
                            continue;
                        };
                        if !seen.insert(dir.clone()) {
                            continue;
                        }
                        let Some(dir) = confined_plugin_install_dir(&dir, &[plugins_dir]).await
                        else {
                            continue;
                        };
                        if let Some((id, manifest)) = load_plugin_from_path_with_mcp_gate_and_bus(
                            &dir,
                            false,
                            Some(key.as_str()),
                            analytics_bus,
                        )
                        .await
                        {
                            out.push((key.clone(), id, manifest, dir));
                        }
                    }
                }
                // Legacy: `{ "<plugin>": {version, added} }` under a marketplace key.
                serde_json::Value::Object(plugins) => {
                    for (name, record) in plugins {
                        let version = record
                            .get("version")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        let dir = cache_root
                            .join(sanitize_segment(key, false))
                            .join(sanitize_segment(name, false))
                            .join(sanitize_segment(version, true));
                        let identifier = format!("{name}@{key}");
                        let Some(dir) = confined_plugin_install_dir(&dir, &[plugins_dir]).await
                        else {
                            continue;
                        };
                        if let Some((id, manifest)) = load_plugin_from_path_with_mcp_gate_and_bus(
                            &dir,
                            false,
                            Some(&identifier),
                            analytics_bus,
                        )
                        .await
                        {
                            out.push((identifier, id, manifest, dir));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let resolved = if resolve_collisions {
        resolve_discovered_plugins(
            out.iter()
                .map(|(_, id, manifest, path)| (*id, manifest.clone(), path.clone()))
                .collect(),
            analytics_bus,
        )
        .await
    } else {
        out.iter()
            .map(|(_, id, manifest, path)| (*id, manifest.clone(), path.clone()))
            .collect()
    };
    let paths = resolved
        .iter()
        .map(|(_, _, path)| path)
        .collect::<BTreeSet<_>>();
    out.retain(|(_, _, _, path)| paths.contains(path));
    out.sort_by(|a, b| {
        a.2.name
            .to_lowercase()
            .cmp(&b.2.name.to_lowercase())
            .then_with(|| a.3.cmp(&b.3))
    });
    out
}

/// Re-discover every plugin recorded in `installed_plugins.json`.
///
/// Listing and management callers intentionally receive every installed
/// plugin regardless of its activation state. Runtime startup should use
/// [`discover_effective_plugins`] instead.
pub async fn discover_recorded_plugins(
    plugins_dir: &Path,
) -> Vec<(PluginId, PluginManifest, PathBuf)> {
    discover_recorded_plugins_with_bus(plugins_dir, None).await
}

pub async fn discover_recorded_plugins_with_bus(
    plugins_dir: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Vec<(PluginId, PluginManifest, PathBuf)> {
    discover_recorded_plugins_identified(plugins_dir, analytics_bus, true)
        .await
        .into_iter()
        .map(|(_, id, manifest, path)| (id, manifest, path))
        .collect()
}

/// Resolve the effective runtime plugin set.
///
/// Explicit `enabledPlugins` entries win. Installed plugins with no explicit
/// setting fall back to their manifest's `defaultEnabled` value (defaulting to
/// true), while explicitly disabled plugins remain unloaded. The explicit
/// cache probe is retained for old/cache-only installations that have no
/// `installed_plugins.json` record.
pub async fn discover_effective_plugins(
    plugins_dir: &Path,
    enabled: &BTreeMap<String, bool>,
) -> Vec<(PluginId, PluginManifest, PathBuf)> {
    discover_effective_plugins_with_bus(plugins_dir, enabled, None).await
}

pub async fn discover_effective_plugins_with_bus(
    plugins_dir: &Path,
    enabled: &BTreeMap<String, bool>,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Vec<(PluginId, PluginManifest, PathBuf)> {
    let mut out = discover_enabled_plugins_impl(plugins_dir, enabled, analytics_bus, false).await;
    let mut seen_paths: BTreeSet<PathBuf> = out.iter().map(|(_, _, path)| path.clone()).collect();

    for (identifier, id, manifest, path) in
        discover_recorded_plugins_identified(plugins_dir, analytics_bus, false).await
    {
        let active = enabled
            .get(&identifier)
            .copied()
            .unwrap_or(manifest.default_enabled);
        if active && seen_paths.insert(path.clone()) {
            out.push((id, manifest, path));
        }
    }

    resolve_discovered_plugins(out, analytics_bus).await
}

/// Walk `plugins_dir` and return every installed plugin discovered on disk.
///
/// Each returned tuple is `(freshly-minted id, manifest, install dir)`. A
/// missing `plugins_dir` yields an empty vec (zero-cost on a fresh install,
/// matching claude-code's resilient boot). Directories without a readable
/// `.lingxi-plugin/plugin.json` are skipped.
///
/// `PluginId` is a UUID newtype with no string-stable derivation, so a fresh
/// id is minted per plugin and stamped onto the returned manifest; callers
/// keep the `id` to drive [`crate::manager::PluginManager::enable`] / unload.
pub async fn discover_installed_plugins(
    plugins_dir: &Path,
) -> Vec<(PluginId, PluginManifest, PathBuf)> {
    discover_installed_plugins_with_bus(plugins_dir, None).await
}

pub async fn discover_installed_plugins_with_bus(
    plugins_dir: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Vec<(PluginId, PluginManifest, PathBuf)> {
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(plugins_dir).await else {
        // Missing / unreadable plugins dir = no plugins (fresh install).
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let entry_path = entry.path();
        if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if let Some((id, manifest)) =
            load_plugin_from_path_with_bus(&entry_path, analytics_bus).await
        {
            out.push((id, manifest, entry_path));
        }
    }
    // Stable ordering by plugin name for deterministic bootstrap.
    resolve_discovered_plugins(out, analytics_bus).await
}

/// (M4 cc2.1.198) Load the `--plugin-dir <path>` session-only plugins — the
/// path arm of the binary's inline-plugin loader `EBm` (2.1.198):
///
/// * a missing path is a WARN + skip, never a boot failure
///   (`C(\`Plugin path does not exist: ${l} (${errno}), skipping\`,
///   {level:"warn"})`);
/// * a `.zip` is extracted into a fresh per-session temp dir
///   (`PXt()` = `join(os.tmpdir(), "claude-plugin-session-<hex>")`;
///   `inline-{i}-{name}` child), then wrapper-dir–unwrapped (`Yor`: a single
///   directory child holding the manifest dir becomes the plugin root);
/// * the resulting directory loads exactly like an installed plugin
///   (`zor` ≙ [`load_plugin_from_path`]); success logs
///   `Loaded inline plugin from path: {name}`;
/// * a summary `Loaded {n} session-only plugins from --plugin-dir` follows.
pub async fn discover_cli_plugin_dirs(
    paths: &[PathBuf],
) -> Vec<(PluginId, PluginManifest, PathBuf)> {
    discover_cli_plugin_dirs_with_bus(paths, None).await
}

pub async fn discover_cli_plugin_dirs_with_bus(
    paths: &[PathBuf],
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Vec<(PluginId, PluginManifest, PathBuf)> {
    discover_cli_plugin_dirs_impl(paths, false, analytics_bus).await
}

/// Sibling of [`discover_cli_plugin_dirs`] for plugin directories whose MCP
/// connections the CALLER already owns — oracle SDK-host
/// `extensionsConfig.inlinePluginsNoMcp()` (`M4()`), the `skipMcpDiscovery`
/// twin of `inlinePlugins()` (`L4()`, i.e. plain `discover_cli_plugin_dirs`).
/// This crate's stand-in entry point is the parsed-but-unwired
/// `--plugin-dir-no-mcp` flag (`apps/cli/src/argv.rs`'s
/// `plugin_dir_no_mcp` / `apps/cli/src/commands/agents.rs`'s twin) — wiring
/// it from `DesktopConfig` through to this function is a companion change,
/// out of this file's scope. Every plugin loaded through this path gets
/// [`PluginComponents::skip_mcp_discovery`] stamped `true`, so its
/// `.mcp.json` and manifest `mcpServers` are never read.
pub async fn discover_cli_plugin_dirs_no_mcp(
    paths: &[PathBuf],
) -> Vec<(PluginId, PluginManifest, PathBuf)> {
    discover_cli_plugin_dirs_impl(paths, true, None).await
}

/// Oracle `KMe` (2.1.265): a `--plugin-dir` path is either one plugin or a
/// folder of plugins. Zip paths are never classified here (`X1s` skips them).
enum CliPluginDirKind {
    Plugin,
    Collection {
        children: Vec<String>,
        skipped: Vec<String>,
    },
}

/// Oracle `W0e`: names that mark a directory as plugin content rather than a
/// collection of plugins. LingXi's manifest dir is `.lingxi-plugin`.
const CLI_PLUGIN_CONTENT_NAMES: &[&str] = &[
    branding::PLUGIN_MANIFEST_DIR,
    "commands",
    "skills",
    "agents",
    "hooks",
    "themes",
    "output-styles",
    "monitors",
    "workflows",
    "SKILL.md",
    ".mcp.json",
    ".lsp.json",
];

/// Oracle `VJt`: component directories. A parent that only has these (and
/// each child itself has a manifest) is a collection, not a plugin.
const CLI_PLUGIN_COMPONENT_DIR_NAMES: &[&str] = &[
    "commands",
    "skills",
    "agents",
    "hooks",
    "themes",
    "output-styles",
    "monitors",
    "workflows",
];

async fn child_has_readable_manifest(dir: &Path) -> bool {
    let manifest = dir.join(branding::PLUGIN_MANIFEST_DIR).join("plugin.json");
    match tokio::fs::OpenOptions::new()
        .read(true)
        .open(&manifest)
        .await
    {
        Ok(_) => true,
        Err(err) => {
            // Oracle `egn`: ENOENT / ENOTDIR → no manifest; any other error
            // (EACCES, EISDIR, …) still counts as "present" so a collection
            // does not swallow an unreadable child plugin.
            err.kind() != std::io::ErrorKind::NotFound && err.raw_os_error() != Some(20)
        }
    }
}

/// Oracle `KMe`/`Evo`/`xvo`.
async fn classify_cli_plugin_dir(path: &Path) -> CliPluginDirKind {
    let mut reader = match tokio::fs::read_dir(path).await {
        Ok(reader) => reader,
        Err(_) => return CliPluginDirKind::Plugin,
    };
    let mut entries = Vec::new();
    while let Ok(Some(entry)) = reader.next_entry().await {
        let file_type = entry.file_type().await.ok();
        entries.push((entry.file_name(), file_type));
    }

    let content: Vec<_> = entries
        .iter()
        .filter(|(name, _)| {
            name.to_str()
                .is_some_and(|n| CLI_PLUGIN_CONTENT_NAMES.contains(&n))
        })
        .collect();
    let component_dirs: Vec<_> = content
        .iter()
        .filter(|(name, _)| {
            name.to_str()
                .is_some_and(|n| CLI_PLUGIN_COMPONENT_DIR_NAMES.contains(&n))
        })
        .collect();

    let looks_like_plugin = component_dirs.len() < content.len();
    let mut component_missing_manifest = false;
    if !looks_like_plugin {
        for (name, _) in &component_dirs {
            if !child_has_readable_manifest(&path.join(name)).await {
                component_missing_manifest = true;
                break;
            }
        }
    }
    if looks_like_plugin || component_missing_manifest {
        return CliPluginDirKind::Plugin;
    }

    let mut candidates: Vec<String> = entries
        .iter()
        .filter(|(name, file_type)| {
            let Some(n) = name.to_str() else {
                return false;
            };
            if n.starts_with('.') {
                return false;
            }
            file_type
                .as_ref()
                .is_some_and(|kind| kind.is_dir() || kind.is_symlink())
        })
        .filter_map(|(name, _)| name.to_str().map(str::to_string))
        .collect();
    candidates.sort();

    let mut children = Vec::new();
    let mut skipped = Vec::new();
    for name in candidates {
        if child_has_readable_manifest(&path.join(&name)).await {
            children.push(name);
        } else {
            skipped.push(name);
        }
    }
    CliPluginDirKind::Collection { children, skipped }
}

/// Child directory names of a `--plugin-dir` folder-of-plugins, in sorted
/// order. `None` when `path` is a single plugin (or unreadable), matching
/// oracle `wAr`/`KMe` so a watcher can re-scan without reloading a plugin
/// root as a collection.
pub async fn cli_plugin_dir_collection_children(path: &Path) -> Option<Vec<String>> {
    match classify_cli_plugin_dir(path).await {
        CliPluginDirKind::Collection { children, .. } => Some(children),
        CliPluginDirKind::Plugin => None,
    }
}

async fn discover_cli_plugin_dirs_impl(
    paths: &[PathBuf],
    sdk_skip_mcp_discovery: bool,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Vec<(PluginId, PluginManifest, PathBuf)> {
    let mut out = Vec::new();
    for (i, raw) in paths.iter().enumerate() {
        if tokio::fs::symlink_metadata(raw)
            .await
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            tracing::warn!(path = %raw.display(), "refusing symlink-spelled session plugin path");
            continue;
        }
        let path = match tokio::fs::canonicalize(raw).await {
            Ok(p) => p,
            // `Cd.stat(l)` failed → warn + `path-not-found` record, skip.
            Err(e) => {
                tracing::warn!(
                    "Plugin path does not exist: {} ({}), skipping",
                    raw.display(),
                    e.raw_os_error()
                        .map_or_else(|| "UNKNOWN".to_string(), errno_name)
                );
                continue;
            }
        };
        let is_zip = path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("zip"));
        let plugin_roots: Vec<PathBuf> = if is_zip {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("download")
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect::<String>();
            let dest = session_temp_dir().join(format!("inline-{i}-{stem}"));
            let _ = tokio::fs::remove_dir_all(&dest).await;
            if let Err(e) = tokio::fs::create_dir_all(&dest).await {
                tracing::warn!("Failed to load session plugin from {}: {e}", raw.display());
                continue;
            }
            let bytes = match tokio::fs::read(&path).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("Failed to load session plugin from {}: {e}", raw.display());
                    continue;
                }
            };
            // Reuse the guarded zip extractor (`.mcpb` IS a zip; the same
            // path-traversal / zip-bomb limits protect inline plugin zips).
            if let Err(e) = crate::mcpb::unpack_mcpb(&bytes, &dest) {
                tracing::warn!("Failed to load session plugin from {}: {e}", raw.display());
                continue;
            }
            tracing::debug!("Extracted inline plugin zip to {}", dest.display());
            // `Yor`: unwrap a single wrapper directory holding the manifest.
            vec![unwrap_zip_root(&dest).await]
        } else {
            match classify_cli_plugin_dir(&path).await {
                CliPluginDirKind::Plugin => vec![path],
                CliPluginDirKind::Collection { children, skipped } => {
                    let loading = if children.is_empty() {
                        "none".to_string()
                    } else {
                        children.join(", ")
                    };
                    let skipped_note = if skipped.is_empty() {
                        String::new()
                    } else {
                        format!("; no manifest in {}", skipped.join(", "))
                    };
                    tracing::info!(
                        "--plugin-dir {} is a folder of plugins: loading {loading}{skipped_note}",
                        path.display()
                    );
                    children.into_iter().map(|name| path.join(name)).collect()
                }
            }
        };
        for plugin_root in plugin_roots {
            match load_plugin_from_path_with_mcp_gate_and_bus(
                &plugin_root,
                sdk_skip_mcp_discovery,
                None,
                analytics_bus,
            )
            .await
            {
                Some((id, manifest)) => {
                    tracing::debug!("Loaded inline plugin from path: {}", manifest.name);
                    out.push((id, manifest, plugin_root));
                }
                None => {
                    tracing::warn!(
                        "Failed to load session plugin from {}: no readable {}/plugin.json",
                        plugin_root.display(),
                        branding::PLUGIN_MANIFEST_DIR
                    );
                }
            }
        }
    }
    if !out.is_empty() {
        tracing::debug!(
            "Loaded {} session-only plugins from --plugin-dir",
            out.len()
        );
    }
    resolve_discovered_plugins(out, analytics_bus).await
}

/// The per-process inline-plugin extraction dir (`PXt()` port: a temp-root
/// session dir, created lazily and reused for every `--plugin-dir` zip).
fn session_temp_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        std::env::temp_dir().join(format!("lingxi-plugin-session-{}", std::process::id()))
    })
    .clone()
}

/// `Yor` port: when the extracted zip root holds EXACTLY ONE entry, a
/// directory that itself contains the plugin manifest dir, descend into it
/// (zips often wrap the plugin in a single top-level folder).
async fn unwrap_zip_root(root: &Path) -> PathBuf {
    let Ok(mut entries) = tokio::fs::read_dir(root).await else {
        return root.to_path_buf();
    };
    let mut only: Option<PathBuf> = None;
    let mut count = 0usize;
    while let Ok(Some(e)) = entries.next_entry().await {
        count += 1;
        if count > 1 {
            return root.to_path_buf();
        }
        if e.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            only = Some(e.path());
        }
    }
    if let Some(inner) = only {
        if tokio::fs::metadata(inner.join(branding::PLUGIN_MANIFEST_DIR))
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            tracing::debug!(
                "Inline plugin zip had wrapper directory; using {}",
                inner.display()
            );
            return inner;
        }
    }
    root.to_path_buf()
}

/// Best-effort errno → name mapping for the `path does not exist` warn line
/// (the binary logs node's errno code, e.g. `ENOENT`).
fn errno_name(code: i32) -> String {
    match code {
        2 => "ENOENT".to_string(),
        13 => "EACCES".to_string(),
        20 => "ENOTDIR".to_string(),
        other => format!("errno {other}"),
    }
}

/// Read + auto-detect a single plugin directory. Returns `None` when there is
/// no readable manifest (the directory is not a plugin).
///
/// Thin wrapper over [`load_plugin_from_path_with_mcp_gate`] for the common
/// case: no SDK-host `skipMcpDiscovery` request and no known
/// `name@marketplace` install-source identity. Kept so every existing call
/// site (production and test) is unaffected by the MCP-discovery gate added
/// for §3/§4.
pub(crate) async fn load_plugin_from_path(plugin_dir: &Path) -> Option<(PluginId, PluginManifest)> {
    load_plugin_from_path_with_bus(plugin_dir, None).await
}

pub(crate) async fn load_plugin_from_path_with_bus(
    plugin_dir: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Option<(PluginId, PluginManifest)> {
    load_plugin_from_path_with_mcp_gate_and_bus(plugin_dir, false, None, analytics_bus).await
}

/// Read + auto-detect a single plugin directory. Returns `None` when there is
/// no readable manifest (the directory is not a plugin).
///
/// Mirrors `createPluginFromPath` (`pluginLoader.ts:1348`): Step 1 loads the
/// manifest, Step 3 auto-detects the optional component directories.
///
/// `sdk_skip_mcp_discovery` is the oracle's per-plugin SDK-host
/// `skipMcpDiscovery` request (see [`PluginComponents::skip_mcp_discovery`]);
/// `install_source_id` is this plugin's `name@marketplace` install-source
/// identity when the caller resolved one (`discover_enabled_plugins` /
/// `discover_recorded_plugins_identified` always have one; an ad-hoc
/// directory load — `discover_installed_plugins`, `discover_cli_plugin_dirs`
/// — never does). Both feed [`resolve_skip_mcp_discovery`], which also
/// consults the process-wide `CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS` /
/// `_EXCEPT` env pair.
pub(crate) async fn load_plugin_from_path_with_mcp_gate(
    plugin_dir: &Path,
    sdk_skip_mcp_discovery: bool,
    install_source_id: Option<&str>,
) -> Option<(PluginId, PluginManifest)> {
    load_plugin_from_path_with_mcp_gate_and_bus(
        plugin_dir,
        sdk_skip_mcp_discovery,
        install_source_id,
        None,
    )
    .await
}

pub(crate) async fn load_plugin_from_path_with_mcp_gate_and_bus(
    plugin_dir: &Path,
    sdk_skip_mcp_discovery: bool,
    install_source_id: Option<&str>,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Option<(PluginId, PluginManifest)> {
    // Resolve the root once and refuse a symlink-spelled plugin directory.
    // Every declared component is subsequently resolved relative to this
    // canonical root, preventing a manifest from escaping through a symlink
    // even when its lexical path contains no `..` segment.
    let plugin_root = canonical_plugin_root(plugin_dir).await?;
    let plugin_dir = plugin_root.as_path();
    let manifest_path = plugin_dir
        .join(branding::PLUGIN_MANIFEST_DIR)
        .join("plugin.json");
    let manifest_path = canonical_regular_path_under(plugin_dir, &manifest_path)?;
    let raw = tokio::fs::read_to_string(&manifest_path).await.ok()?;
    let raw = raw.strip_prefix(UTF8_BOM).unwrap_or(raw.as_str());
    let mut parsed: RawManifest = match serde_json::from_str(raw) {
        Ok(m) => m,
        Err(e) => {
            emit_plugin_load_failed(
                analytics_bus,
                plugin_dir.file_name().and_then(|name| name.to_str()),
                install_source_id,
                "malformed-plugin-json",
                Some("manifest"),
            )
            .await;
            tracing::warn!(
                error = %e,
                path = %manifest_path.display(),
                "skipping plugin with malformed plugin.json"
            );
            return None;
        }
    };
    // §8: the name validator previously existed only on the authoring path
    // (`plugin tag`/`plugin init`); the actual LOAD path never called it, so a
    // manifest with an empty/space-containing/bidi-spoofed `name` was trusted
    // verbatim into `plugin list` and every downstream cache-path segment.
    if let Err(reason) = validate_plugin_name(&parsed.name) {
        emit_plugin_load_failed(
            analytics_bus,
            Some(parsed.name.as_str()),
            install_source_id,
            "invalid-name",
            Some("manifest"),
        )
        .await;
        tracing::warn!(
            reason = %reason,
            path = %manifest_path.display(),
            "skipping plugin with an invalid name"
        );
        return None;
    }

    let source = PluginSource::LocalPath {
        path: plugin_dir.to_path_buf(),
    };
    let trust_level = default_trust_for_source(&source);

    // Marketplace rename metadata is keyed by the durable install-source name,
    // not by the manifest's current display identity. Resolve the complete
    // chain before minting the runtime manifest so all registries use one
    // canonical namespace while `PluginSource::LocalPath` still points at the
    // original install tree.
    let (source_name, marketplace_name) = install_source_id
        .map(parse_plugin_identifier)
        .unwrap_or((parsed.name.as_str(), None));
    let mut renames = HashMap::new();
    let mut known_names = HashSet::new();
    if let Some(marketplace) = marketplace_name {
        let (catalog_renames, catalog_names) =
            load_marketplace_renames(plugin_dir, marketplace).await;
        renames.extend(catalog_renames);
        known_names.extend(catalog_names);
    }
    if let Some(previous_name) = parsed.rename_from.as_ref() {
        if validate_plugin_name(previous_name).is_ok() {
            renames.insert(previous_name.clone(), Some(parsed.name.clone()));
            known_names.insert(parsed.name.clone());
        }
    }
    if let Some(resolution) = resolve_rename_chain(source_name, &renames, &known_names) {
        let canonical_name_changed = matches!(
            &resolution,
            RenameResolution::Renamed { to, .. } if to != source_name
        );
        if canonical_name_changed || !matches!(&resolution, RenameResolution::Renamed { .. }) {
            emit_plugin_renamed_event(
                analytics_bus,
                source_name,
                marketplace_name,
                &source,
                plugin_dir,
                &resolution,
            )
            .await;
        }
        match resolution {
            RenameResolution::Renamed { to, .. } if to != parsed.name => {
                parsed.name = to;
            }
            RenameResolution::Removed { .. } => return None,
            _ => {}
        }
    }

    let id = PluginId::new();

    let skip_mcp_discovery =
        resolve_skip_mcp_discovery(&parsed.name, sdk_skip_mcp_discovery, install_source_id);
    // Oracle `pM(e)` (@157119272), verbatim:
    // `e.scope==="project" && e.source.endsWith(`@${Zc}`)` with
    // `Zc="skills-dir"` — TRUE only for a plugin AUTO-LOADED from
    // `.claude/skills/`. It is NOT "loaded from a directory": a
    // `--plugin-dir` session plugin is stamped `<name>@inline` (`Om`, the
    // default `marketplaceName` of the session loader @162854959) and an
    // installed plugin `<name>@<marketplace>`, so `pM` is false for both and
    // neither is confined. This port has no `.claude/skills/` plugin
    // auto-loader at all, so `pM` is uniformly false here; the `confined`
    // plumbing below stays wired for when one lands.
    let confined = false;
    let components = detect_components(plugin_dir, &parsed, skip_mcp_discovery, confined).await;
    let settings = load_plugin_settings(plugin_dir, parsed.settings.as_ref()).await;
    let channels = validate_plugin_channels(
        parsed.channels.as_deref().unwrap_or_default(),
        &components.mcp_servers,
        &manifest_path,
    );
    let dependencies = match crate::parse_dependencies(parsed.dependencies.as_ref()) {
        Ok(dependencies) => dependencies,
        Err(error) => {
            tracing::warn!(
                path = %manifest_path.display(),
                %error,
                "ignoring invalid plugin dependency declarations"
            );
            Vec::new()
        }
    };

    let (author, author_email, author_url) = match parsed.author {
        Some(raw) => raw.into_parts(),
        None => (None, None, None),
    };
    // Oracle preprocess: only a plain JSON object survives `metadata`; an
    // array/string/number/bool/null value is silently dropped to `undefined`
    // rather than rejected (`Sa((e)=>He(e)?e:void 0,...)`).
    let metadata = parsed.metadata.filter(|v| v.is_object());

    let manifest = PluginManifest {
        id,
        name: parsed.name,
        display_name: parsed.display_name,
        default_enabled: parsed.default_enabled,
        version: parsed.version.unwrap_or_default(),
        description: parsed.description.unwrap_or_default(),
        author,
        author_email,
        author_url,
        homepage: parsed.homepage,
        source,
        components,
        trust_level,
        depends_on: Vec::new(),
        dependencies,
        user_config: parsed
            .user_config
            .map(|fields| UserConfigSchema { fields: fields.0 }),
        channels,
        settings,
        settings_declared: parsed.settings.is_some(),
        keywords: parsed.keywords.unwrap_or_default(),
        license: parsed.license,
        repository: parsed.repository,
        metadata,
    };
    Some((id, manifest))
}

fn plugin_hash(value: &str) -> String {
    crate::plugin_source_sha256(value.as_bytes())[..16].to_string()
}

fn plugin_identity_hash(plugin_name: &str, marketplace_name: Option<&str>) -> String {
    plugin_hash(&format!(
        "{plugin_name}@{}",
        marketplace_name
            .map(str::to_ascii_lowercase)
            .unwrap_or_default()
    ))
}

fn load_failed_scope(install_source_id: Option<&str>) -> &'static str {
    if install_source_id.is_some() {
        "cache-installed"
    } else {
        "user-local"
    }
}

fn payload_metadata<T: serde::Serialize>(payload: &T) -> LogEventMetadata {
    let Ok(Value::Object(fields)) = serde_json::to_value(payload) else {
        return LogEventMetadata::new();
    };
    fields
        .into_iter()
        .filter_map(|(key, value)| {
            let value = match value {
                Value::Bool(value) => AnalyticsValue::Bool(value),
                Value::Number(value) => {
                    if let Some(value) = value.as_i64() {
                        AnalyticsValue::Int(value)
                    } else if let Some(value) = value.as_f64() {
                        AnalyticsValue::Float(value)
                    } else {
                        return None;
                    }
                }
                Value::String(value) => AnalyticsValue::String(value),
                Value::Null => AnalyticsValue::None,
                Value::Array(_) | Value::Object(_) => return None,
            };
            Some((key, value))
        })
        .collect()
}

fn is_official_marketplace_name(name: Option<&str>) -> bool {
    let Some(name) = name else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    RESERVED_OFFICIAL_MARKETPLACE_NAMES.contains(&lower.as_str())
}

async fn emit_plugin_load_failed(
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    plugin_name: Option<&str>,
    install_source_id: Option<&str>,
    error_category: &'static str,
    component: Option<&str>,
) {
    let (fallback_name, marketplace_name) = install_source_id
        .map(parse_plugin_identifier)
        .unwrap_or(("unknown", None));
    let plugin_name = plugin_name
        .filter(|name| !name.is_empty())
        .unwrap_or(fallback_name);
    if let Some(bus) = analytics_bus {
        let payload = plugin_telemetry::LoadFailedPayload {
            error_category: Verified::assert_safe(error_category.to_string()),
            cache_only: install_source_id.is_some(),
            component: component.map(|value| Verified::assert_safe(value.to_string())),
            errno: None,
            proto_plugin_name: PiiTagged::assert_pii_tagged_column(plugin_name.to_string()),
            proto_marketplace_name: marketplace_name
                .map(|value| PiiTagged::assert_pii_tagged_column(value.to_string())),
            plugin_id_hash: Verified::assert_safe(plugin_identity_hash(
                plugin_name,
                marketplace_name,
            )),
            plugin_scope: Verified::assert_safe(load_failed_scope(install_source_id).to_string()),
            plugin_name_redacted: Verified::assert_safe("(redacted)".to_string()),
            marketplace_name_redacted: Verified::assert_safe(
                if marketplace_name.is_some() {
                    "(redacted)"
                } else {
                    ""
                }
                .to_string(),
            ),
            is_official_plugin: is_official_marketplace_name(marketplace_name),
        };
        bus.log_event(plugin_telemetry::LOAD_FAILED, payload_metadata(&payload))
            .await;
    }
}

/// Stable provenance used by the resolver and plugin telemetry. The path is
/// deliberately reduced to a source class plus a content-independent hash;
/// absolute cache/seed paths must never reach a general-access event field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginSourceDescriptor {
    pub token: String,
    pub rank: u8,
    pub marketplace: Option<String>,
    pub scope: &'static str,
    pub official: bool,
}

/// Describe one discovered plugin's source using the same precedence as the
/// Claude loader: session/local paths outrank installed cache entries, while
/// a seed cache is only the fallback for an absent primary cache entry.
pub(crate) fn plugin_source_descriptor(
    manifest: &PluginManifest,
    install_dir: &Path,
) -> PluginSourceDescriptor {
    plugin_source_descriptor_for_name(&manifest.name, &manifest.source, install_dir)
}

fn plugin_source_descriptor_for_name(
    plugin_name: &str,
    source: &PluginSource,
    install_dir: &Path,
) -> PluginSourceDescriptor {
    let marketplace = cache_marketplace_name(install_dir);
    let seed = is_seed_path(install_dir);
    let (class, rank, scope) = match source {
        // `mergePluginSources` appends built-ins last, so they are the
        // lowest-precedence source when a session/local or cached plugin
        // declares the same canonical name.
        PluginSource::BuiltIn => ("builtin", 0, "builtin"),
        _ if seed => ("seed", 1, "cache-installed"),
        _ if marketplace.is_some() => ("cache", 2, "cache-installed"),
        _ => ("user-local", 3, "user-local"),
    };
    // A sibling set of local plugin directories is one source (the normal
    // flat-walk/reload case), while copies supplied from different roots are
    // distinct sources and must participate in deterministic collision
    // resolution. Hash the source root rather than exposing its path.
    let identity = if marketplace.is_some() || seed || matches!(source, PluginSource::BuiltIn) {
        format!(
            "{}@{}",
            plugin_name,
            marketplace.as_deref().unwrap_or_default()
        )
    } else {
        let source_root = install_dir
            .parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
            .unwrap_or_else(|| install_dir.parent().unwrap_or(install_dir).to_path_buf());
        format!(
            "{}@local:{}",
            plugin_name,
            plugin_hash(source_root.to_string_lossy().as_ref())
        )
    };
    PluginSourceDescriptor {
        token: format!("{class}:{}", plugin_hash(&identity)),
        rank,
        official: is_official_marketplace_name(marketplace.as_deref()),
        scope,
        marketplace,
    }
}

/// Component names after the plugin namespace is applied. Keeping this
/// inventory in discovery means the same collision policy is used before
/// materialization and by `PluginManager::enable` for direct loads.
pub(crate) fn plugin_component_inventory(
    manifest: &PluginManifest,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut inventory = BTreeMap::new();
    let mut add_paths = |kind: &str, paths: &[ComponentPath]| {
        let names = paths
            .iter()
            .filter_map(|component| component_name(&component.path))
            .map(|name| format!("{}:{name}", manifest.name))
            .collect::<BTreeSet<_>>();
        if !names.is_empty() {
            inventory.insert(kind.to_string(), names);
        }
    };
    add_paths("command", &manifest.components.commands);
    add_paths("skill", &manifest.components.skills);
    add_paths("agent", &manifest.components.agents);
    add_paths("outputStyle", &manifest.components.output_styles);
    add_paths("theme", &manifest.components.themes);
    add_paths("workflow", &manifest.components.workflows);

    if !manifest.components.hooks.is_empty() {
        inventory.insert(
            "hook".to_string(),
            BTreeSet::from([format!("{}:hooks", manifest.name)]),
        );
    }
    if !manifest.components.mcp_servers.is_empty() {
        inventory.insert(
            "mcp".to_string(),
            manifest
                .components
                .mcp_servers
                .keys()
                .map(|name| format!("{}:{name}", manifest.name))
                .collect(),
        );
    }
    if !manifest.components.lsp_servers.is_empty() {
        inventory.insert(
            "lsp".to_string(),
            manifest
                .components
                .lsp_servers
                .keys()
                .map(|name| format!("{}:{name}", manifest.name))
                .collect(),
        );
    }
    if !manifest.components.monitors.is_empty() {
        inventory.insert(
            "monitor".to_string(),
            manifest
                .components
                .monitors
                .iter()
                .map(|monitor| format!("{}:{}", manifest.name, monitor.name))
                .collect(),
        );
    }
    inventory
}

/// Build the component inventory with the same component-name parsers used by
/// materialization. The manifest stores paths rather than the parsed
/// frontmatter names for skills, agents, and output styles, so a basename-only
/// inventory can miss a real collision (or report one for a file that the
/// manager would skip). Discovery runs this once per loaded plugin before the
/// source resolver chooses a winner.
pub(crate) async fn plugin_component_inventory_for_path(
    manifest: &PluginManifest,
    install_dir: &Path,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut inventory = plugin_component_inventory(manifest);

    let mut commands = BTreeSet::new();
    for component in &manifest.components.commands {
        let root = component_root_from_metadata(component, install_dir.join("commands"));
        let name = command_api::command_name_from_path(&component.path, &root);
        if !name.is_empty() {
            commands.insert(format!("{}:{name}", manifest.name));
        }
    }
    inventory.insert("command".to_string(), commands);

    let mut skills = BTreeSet::new();
    for component in &manifest.components.skills {
        let Ok(raw) = tokio::fs::read_to_string(&component.path).await else {
            continue;
        };
        let Ok(skill) = skill_api::parse_skill_markdown(
            &raw,
            component.path.clone(),
            skill_api::SkillSource::Plugin,
            skill_api::LoadedFrom::Plugin,
        ) else {
            continue;
        };
        skills.insert(format!("{}:{}", manifest.name, skill.name));
    }
    inventory.insert("skill".to_string(), skills);

    let mut agents = BTreeSet::new();
    for component in &manifest.components.agents {
        let Ok(raw) = tokio::fs::read_to_string(&component.path).await else {
            continue;
        };
        let root = component_root_from_metadata(component, install_dir.join("agents"));
        let Ok(agent) = agent::parse_agent_markdown(
            &raw,
            agent::AgentSource::Plugin,
            root.clone(),
            &component.path,
        ) else {
            continue;
        };
        let namespace = component
            .path
            .parent()
            .and_then(|parent| parent.strip_prefix(&root).ok())
            .map(|relative| {
                relative
                    .components()
                    .filter_map(|part| match part {
                        std::path::Component::Normal(value) => {
                            Some(value.to_string_lossy().into_owned())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(":")
            })
            .filter(|value| !value.is_empty())
            .unwrap_or_default();
        let name = if namespace.is_empty() {
            format!("{}:{}", manifest.name, agent.agent_type)
        } else {
            format!("{}:{namespace}:{}", manifest.name, agent.agent_type)
        };
        agents.insert(name);
    }
    inventory.insert("agent".to_string(), agents);

    let mut output_styles = BTreeSet::new();
    for component in &manifest.components.output_styles {
        let stem = component
            .path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        let Ok(raw) = tokio::fs::read_to_string(&component.path).await else {
            continue;
        };
        let style = outputstyles::parse_output_style(&raw, stem);
        output_styles.insert(format!("{}:{}", manifest.name, style.name));
    }
    inventory.insert("outputStyle".to_string(), output_styles);

    let mut workflows = BTreeSet::new();
    for component in &manifest.components.workflows {
        let Ok(raw) = tokio::fs::read_to_string(&component.path).await else {
            continue;
        };
        if workflow::validate_meta(&raw).is_err() {
            continue;
        }
        let Some(name) = workflow::meta_string_value(&raw, "name") else {
            continue;
        };
        if !name.is_empty() {
            workflows.insert(format!("{}:{name}", manifest.name));
        }
    }
    inventory.insert("workflow".to_string(), workflows);
    inventory
}

fn component_root_from_metadata(component: &ComponentPath, fallback: PathBuf) -> PathBuf {
    component
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("root"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or(fallback)
}

fn component_name(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    if file_name.eq_ignore_ascii_case("skill.md") {
        return path.parent()?.file_name()?.to_str().map(ToOwned::to_owned);
    }
    path.file_stem()?.to_str().map(ToOwned::to_owned)
}

/// Apply the deterministic plugin-name/component resolver to a discovered
/// set. The first source in the oracle's effective-source order is selected;
/// rank ties are broken by the privacy-safe provenance token and then the
/// canonical path, so input enumeration order cannot affect the winner.
pub(crate) async fn resolve_discovered_plugins(
    mut plugins: Vec<(PluginId, PluginManifest, PathBuf)>,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Vec<(PluginId, PluginManifest, PathBuf)> {
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, (_, manifest, _)) in plugins.iter().enumerate() {
        groups
            .entry(manifest.name.to_lowercase())
            .or_default()
            .push(index);
    }

    let mut drop = BTreeSet::new();
    let mut emitted_collisions = BTreeSet::new();
    let mut emitted_folders = BTreeSet::new();
    for indexes in groups.values().filter(|indexes| indexes.len() > 1) {
        let winner = indexes
            .iter()
            .copied()
            .min_by(|left, right| {
                let left_desc = plugin_source_descriptor(&plugins[*left].1, &plugins[*left].2);
                let right_desc = plugin_source_descriptor(&plugins[*right].1, &plugins[*right].2);
                right_desc
                    .rank
                    .cmp(&left_desc.rank)
                    .then_with(|| left_desc.token.cmp(&right_desc.token))
                    .then_with(|| plugins[*left].2.cmp(&plugins[*right].2))
            })
            .expect("non-empty plugin collision group");
        let descriptors = indexes
            .iter()
            .map(|&index| plugin_source_descriptor(&plugins[index].1, &plugins[index].2))
            .collect::<Vec<_>>();
        // Multiple versions of one local/cache source are retained for the
        // listing/reload APIs. They share one provenance token, so there is no
        // cross-source collision to resolve; marketplace/seed/source-class
        // changes produce distinct tokens and continue through the policy.
        if descriptors
            .iter()
            .map(|descriptor| descriptor.token.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            < 2
        {
            continue;
        }
        let mut inventories = Vec::with_capacity(indexes.len());
        for &index in indexes {
            inventories.push(
                plugin_component_inventory_for_path(&plugins[index].1, &plugins[index].2).await,
            );
        }

        // Every component name is resolved independently for telemetry. This
        // matters when three marketplaces provide the same plugin: the event
        // must report all distinct sources once, not one pair per loser.
        let mut item_sources: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
        for (position, inventory) in inventories.iter().enumerate() {
            for (kind, names) in inventory {
                for name in names {
                    item_sources
                        .entry((kind.clone(), name.clone()))
                        .or_default()
                        .push(position);
                }
            }
        }
        for ((kind, name), positions) in item_sources {
            let mut source_positions = BTreeMap::<String, usize>::new();
            for position in positions {
                source_positions
                    .entry(descriptors[position].token.clone())
                    .or_insert(position);
            }
            if source_positions.len() < 2 {
                continue;
            }
            let component_winner_position = source_positions
                .values()
                .copied()
                .min_by(|left, right| {
                    descriptors[*right]
                        .rank
                        .cmp(&descriptors[*left].rank)
                        .then_with(|| descriptors[*left].token.cmp(&descriptors[*right].token))
                })
                .expect("non-empty component collision source set");
            let component_sources = source_positions
                .values()
                .map(|position| descriptors[*position].clone())
                .collect::<Vec<_>>();
            let event_key = format!(
                "{kind}\0{name}\0{}",
                component_sources
                    .iter()
                    .map(|source| source.token.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            );
            if emitted_collisions.insert(event_key) {
                emit_name_collision_event(
                    analytics_bus,
                    &kind,
                    &name,
                    &component_sources,
                    Some(&descriptors[component_winner_position]),
                )
                .await;
            }
            for position in source_positions.values().copied() {
                if position == component_winner_position {
                    continue;
                }
                let index = indexes[position];
                let folder_key = format!("{kind}\0{}", descriptors[position].token);
                if emitted_folders.insert(folder_key) {
                    emit_folder_shadowed_event(
                        analytics_bus,
                        &kind,
                        &plugins[index].1,
                        &plugins[index].2,
                    )
                    .await;
                }
            }
        }

        // The plugin resolver drops every lower-precedence plugin with the
        // same canonical name, even if that particular loser only contributed
        // a component not present in the winner. This mirrors Vgr's first-name
        // ownership rule while keeping collision telemetry component-specific.
        for &index in indexes {
            if index != winner {
                drop.insert(index);
            }
        }
    }

    plugins = plugins
        .into_iter()
        .enumerate()
        .filter_map(|(index, plugin)| (!drop.contains(&index)).then_some(plugin))
        .collect();
    plugins.sort_by(|left, right| {
        left.1
            .name
            .to_lowercase()
            .cmp(&right.1.name.to_lowercase())
            .then_with(|| left.1.name.cmp(&right.1.name))
            .then_with(|| left.2.cmp(&right.2))
    });
    plugins
}

/// Emit the oracle-shaped component collision event. Sources are safe opaque
/// provenance tokens, while only the proto-tagged component name is raw.
pub(crate) async fn emit_name_collision_event(
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    item_type: &str,
    item_name: &str,
    sources: &[PluginSourceDescriptor],
    winner: Option<&PluginSourceDescriptor>,
) {
    let Some(bus) = analytics_bus else {
        return;
    };
    let source_tokens = sources
        .iter()
        .map(|source| source.token.clone())
        .collect::<BTreeSet<_>>();
    if source_tokens.len() < 2 {
        return;
    }
    let winner_source = winner.map(|source| source.token.clone());
    let payload = plugin_telemetry::NameCollisionPayload {
        item_type: Verified::assert_safe(item_type.to_string()),
        proto_skill_name: PiiTagged::assert_pii_tagged_column(item_name.to_string()),
        item_name_hash: Verified::assert_safe(plugin_hash(item_name)),
        source_count: source_tokens.len() as u32,
        sources: Verified::assert_safe({
            let mut values = source_tokens.iter().cloned().collect::<Vec<_>>();
            values.sort();
            values.join(",")
        }),
        winner_source: winner_source.map(Verified::assert_safe),
    };
    bus.log_event(plugin_telemetry::NAME_COLLISION, payload_metadata(&payload))
        .await;
}

pub(crate) async fn emit_folder_shadowed_event(
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    component: &str,
    manifest: &PluginManifest,
    install_dir: &Path,
) {
    let Some(bus) = analytics_bus else {
        return;
    };
    let descriptor = plugin_source_descriptor(manifest, install_dir);
    let component = match component {
        "command" => "commands",
        "skill" => "skills",
        "agent" => "agents",
        "outputStyle" => "output-styles",
        "theme" => "themes",
        "workflow" => "workflows",
        "hook" => "hooks",
        "mcp" => "mcpServers",
        "lsp" => "lspServers",
        "monitor" => "monitors",
        other => other,
    };
    static EMITTED: OnceLock<StdMutex<HashSet<String>>> = OnceLock::new();
    let key = format!("{:p}:{}:{}", Arc::as_ptr(bus), descriptor.token, component);
    let should_emit = EMITTED
        .get_or_init(|| StdMutex::new(HashSet::new()))
        .lock()
        .map(|mut emitted| emitted.insert(key))
        .unwrap_or(true);
    if !should_emit {
        return;
    }
    let payload = plugin_telemetry::FolderShadowedPayload {
        component: Verified::assert_safe(component.to_string()),
        proto_plugin_name: PiiTagged::assert_pii_tagged_column(manifest.name.clone()),
        proto_marketplace_name: descriptor
            .marketplace
            .clone()
            .map(PiiTagged::assert_pii_tagged_column),
        plugin_id_hash: Verified::assert_safe(plugin_hash(&format!(
            "{}@{}",
            manifest.name,
            descriptor.marketplace.as_deref().unwrap_or_default()
        ))),
        plugin_scope: Verified::assert_safe(descriptor.scope.to_string()),
        plugin_name_redacted: Verified::assert_safe("(redacted)".to_string()),
        marketplace_name_redacted: Verified::assert_safe(
            descriptor
                .marketplace
                .as_ref()
                .map(|_| "(redacted)")
                .unwrap_or_default()
                .to_string(),
        ),
        is_official_plugin: descriptor.official,
    };
    bus.log_event(
        plugin_telemetry::FOLDER_SHADOWED,
        payload_metadata(&payload),
    )
    .await;
}

fn plugin_seed_dirs() -> Vec<PathBuf> {
    let value = std::env::var_os("CLAUDE_CODE_PLUGIN_SEED_DIR")
        .or_else(|| std::env::var_os("LINGXI_PLUGIN_SEED_DIR"));
    value
        .map(|value| {
            std::env::split_paths(&value)
                .filter(|path| !path.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn is_seed_path(path: &Path) -> bool {
    plugin_seed_dirs().into_iter().any(|seed| {
        let Ok(seed) = std::fs::canonicalize(seed) else {
            return false;
        };
        let Ok(path) = std::fs::canonicalize(path) else {
            return false;
        };
        path.starts_with(seed)
    })
}

fn cache_marketplace_name(install_dir: &Path) -> Option<String> {
    let plugin_dir = install_dir.parent()?;
    let marketplace_dir = plugin_dir.parent()?;
    let cache_dir = marketplace_dir.parent()?;
    (cache_dir.file_name()?.to_str()? == "cache")
        .then(|| marketplace_dir.file_name()?.to_str().map(ToOwned::to_owned))
        .flatten()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RenameResolution {
    Renamed { to: String, chain_depth: u32 },
    Removed { chain_depth: u32 },
    Unresolved { reason: &'static str },
}

/// Follow the marketplace rename table exactly like Claude's `SPe`: at most
/// sixteen links, a visited-name cycle check, `null` means removed, and a
/// target that is not in the marketplace catalog is unresolved. Returning
/// `None` means the old name has no rename entry and therefore must not create
/// a telemetry event.
fn resolve_rename_chain(
    old_name: &str,
    renames: &HashMap<String, Option<String>>,
    known_names: &HashSet<String>,
) -> Option<RenameResolution> {
    if !renames.contains_key(old_name) {
        return None;
    }
    const MAX_RENAME_DEPTH: usize = 16;
    let mut seen = HashSet::new();
    let mut current = old_name.to_string();
    for depth in 0..MAX_RENAME_DEPTH {
        if !seen.insert(current.clone()) {
            return Some(RenameResolution::Unresolved { reason: "cycle" });
        }
        let Some(target) = renames.get(&current) else {
            if known_names.contains(&current) {
                return Some(RenameResolution::Renamed {
                    to: current,
                    chain_depth: depth as u32,
                });
            }
            return Some(RenameResolution::Unresolved {
                reason: "target-missing",
            });
        };
        let Some(target) = target else {
            return Some(RenameResolution::Removed {
                chain_depth: (depth + 1) as u32,
            });
        };
        current.clone_from(target);
    }
    Some(RenameResolution::Unresolved {
        reason: "chain-too-deep",
    })
}

/// Read a marketplace's optional rename table. The catalog is intentionally
/// treated as untrusted JSON: malformed or non-object entries simply provide
/// no rename mapping and never prevent an otherwise valid plugin from loading.
async fn load_marketplace_renames(
    plugin_dir: &Path,
    marketplace: &str,
) -> (HashMap<String, Option<String>>, HashSet<String>) {
    let Some(plugins_root) = cache_root_for_plugin(plugin_dir) else {
        return (HashMap::new(), HashSet::new());
    };
    let path = plugins_root
        .join("marketplaces")
        .join(sanitize_segment(marketplace, false))
        .join(branding::PLUGIN_MANIFEST_DIR)
        .join("marketplace.json");
    let Ok(raw) = tokio::fs::read_to_string(path).await else {
        return (HashMap::new(), HashSet::new());
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return (HashMap::new(), HashSet::new());
    };
    let known_names = value
        .get("plugins")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("name").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect::<HashSet<_>>();
    let renames = value
        .get("renames")
        .and_then(Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(old, target)| match target {
                    Value::Null => Some((old.clone(), None)),
                    Value::String(target) if !target.is_empty() => {
                        Some((old.clone(), Some(target.clone())))
                    }
                    _ => None,
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    (renames, known_names)
}

fn cache_root_for_plugin(plugin_dir: &Path) -> Option<PathBuf> {
    let plugin_name = plugin_dir.parent()?;
    let marketplace = plugin_name.parent()?;
    let cache = marketplace.parent()?;
    (cache.file_name()?.to_str()? == "cache")
        .then(|| cache.parent().map(Path::to_path_buf))
        .flatten()
}

async fn emit_plugin_renamed_event(
    analytics_bus: Option<&Arc<AnalyticsBus>>,
    old_name: &str,
    marketplace_name: Option<&str>,
    source: &PluginSource,
    install_dir: &Path,
    resolution: &RenameResolution,
) {
    let Some(bus) = analytics_bus else {
        return;
    };
    static EMITTED: OnceLock<StdMutex<HashSet<String>>> = OnceLock::new();
    let (outcome_key, depth_key, reason_key) = match resolution {
        RenameResolution::Renamed { chain_depth, .. } => {
            ("renamed", chain_depth.to_string(), String::new())
        }
        RenameResolution::Removed { chain_depth } => {
            ("removed", chain_depth.to_string(), String::new())
        }
        RenameResolution::Unresolved { reason } => {
            ("unresolved", String::new(), (*reason).to_string())
        }
    };
    let key = format!(
        "{:p}:{}:{}:{}:{}",
        Arc::as_ptr(bus),
        plugin_hash(&format!(
            "{old_name}@{}",
            marketplace_name.unwrap_or_default()
        )),
        plugin_hash(install_dir.to_string_lossy().as_ref()),
        outcome_key,
        plugin_hash(&format!("{depth_key}:{reason_key}")),
    );
    let should_emit = EMITTED
        .get_or_init(|| StdMutex::new(HashSet::new()))
        .lock()
        .map(|mut emitted| emitted.insert(key))
        .unwrap_or(true);
    if !should_emit {
        return;
    }
    let descriptor = plugin_source_descriptor_for_name(old_name, source, install_dir);
    let (outcome, chain_depth, reason) = match resolution {
        RenameResolution::Renamed { chain_depth, .. } => ("renamed", Some(*chain_depth), None),
        RenameResolution::Removed { chain_depth } => ("removed", Some(*chain_depth), None),
        RenameResolution::Unresolved { reason } => ("unresolved", None, Some(*reason)),
    };
    let payload = plugin_telemetry::RenamedPayload {
        outcome: Verified::assert_safe(outcome.to_string()),
        chain_depth,
        reason: reason.map(|reason| Verified::assert_safe(reason.to_string())),
        proto_plugin_name: PiiTagged::assert_pii_tagged_column(old_name.to_string()),
        proto_marketplace_name: marketplace_name
            .map(|name| PiiTagged::assert_pii_tagged_column(name.to_string())),
        plugin_id_hash: Verified::assert_safe(plugin_identity_hash(old_name, marketplace_name)),
        plugin_scope: Verified::assert_safe(descriptor.scope.to_string()),
        plugin_name_redacted: Verified::assert_safe("(redacted)".to_string()),
        marketplace_name_redacted: Verified::assert_safe(
            marketplace_name
                .map(|_| "(redacted)")
                .unwrap_or_default()
                .to_string(),
        ),
        is_official_plugin: is_official_marketplace_name(marketplace_name),
    };
    bus.log_event(plugin_telemetry::RENAMED, payload_metadata(&payload))
        .await;
}

/// Retain only channel declarations that can bind to a server contributed by
/// this plugin. A malformed reference must not silently become a connection to
/// a same-named user/project MCP server.
fn validate_plugin_channels(
    declared: &[RawPluginChannel],
    mcp_servers: &HashMap<String, mcp::McpServerConfig>,
    manifest_path: &Path,
) -> Vec<PluginChannel> {
    let mut seen = BTreeSet::new();
    let mut channels = Vec::with_capacity(declared.len());

    for channel in declared {
        if channel.server.is_empty() || !mcp_servers.contains_key(&channel.server) {
            tracing::warn!(
                path = %manifest_path.display(),
                server = %channel.server,
                "skipping plugin channel whose server is not declared by this plugin"
            );
            continue;
        }
        if !seen.insert(channel.server.clone()) {
            tracing::warn!(
                path = %manifest_path.display(),
                server = %channel.server,
                "skipping duplicate plugin channel declaration"
            );
            continue;
        }
        channels.push(PluginChannel {
            server: channel.server.clone(),
            display_name: channel.display_name.clone(),
            user_config: channel
                .user_config
                .clone()
                .map(|fields| UserConfigSchema { fields }),
        });
    }

    channels
}

/// Auto-detect the component directories of a plugin (Step 3 of
/// `createPluginFromPath`): `commands/`, `agents/`, `output-styles/` are
/// globbed for `*.md` when present; `skills/` uses the `<name>/SKILL.md`
/// one-level layout (`validatePlugin.ts:731-739`); the standard
/// `hooks/hooks.json` is parsed when present; and MCP / LSP server configs are
/// read from the plugin-root `.mcp.json` / `.lsp.json` files
/// (`mcpPluginIntegration.ts:137`, `lspPluginIntegration.ts:64`).
///
/// `skip_mcp_discovery` mirrors oracle `OL` (`pluginLoader.ts` @160861000):
/// when set, NEITHER the plugin-root `.mcp.json` NOR the manifest's declared
/// `mcpServers` is read — every other component slot (commands, agents,
/// skills, output styles, hooks, LSP servers) still auto-detects/resolves
/// normally, matching the oracle's "leaves other components enabled". See
/// [`resolve_skip_mcp_discovery`] for how the caller decides this bit.
///
/// `confined` mirrors oracle `pM(e)` ("directory-loaded plugin"): a
/// `mcpServers` MCPB/`.dxt` source is skipped outright (not resolved) when
/// set — see [`load_declared_mcp_servers`].
async fn detect_components(
    plugin_dir: &Path,
    parsed: &RawManifest,
    skip_mcp_discovery: bool,
    confined: bool,
) -> PluginComponents {
    let declared_skill_path_count = parsed
        .skills
        .as_ref()
        .map(path_decl_count)
        .unwrap_or_default();
    let declared_command_path_count = parsed
        .commands
        .as_ref()
        .map(CommandsDecl::declared_count)
        .unwrap_or_default();
    let declared_agent_path_count = parsed
        .agents
        .as_ref()
        .map(path_decl_count)
        .unwrap_or_default();
    let hooks_declared = parsed.hooks.is_some();
    let mcp_servers_declared = parsed.mcp_servers.is_some();
    let lsp_servers_declared = parsed.lsp_servers.is_some();
    let default_commands = glob_md(&plugin_dir.join("commands")).await;
    let default_agents = glob_md(&plugin_dir.join("agents")).await;
    let default_skills_dir = plugin_dir.join("skills");
    let mut default_skills = glob_skill_dirs(&default_skills_dir).await;
    if parsed.skills.is_none()
        && !tokio::fs::try_exists(&default_skills_dir)
            .await
            .unwrap_or(false)
    {
        let root_skill = plugin_dir.join("SKILL.md");
        if let Some(root_skill) = canonical_regular_path_under(plugin_dir, &root_skill) {
            default_skills.push(ComponentPath {
                path: root_skill,
                metadata: component_root_metadata(plugin_dir),
            });
        }
    }
    let default_output_styles = glob_md(&plugin_dir.join("output-styles")).await;
    // Themes (`.json`) and workflows (`.js`) auto-scan a SINGLE directory
    // level — unlike `glob_md`'s recursive DFS for commands/agents/output-
    // styles, the oracle's theme/workflow directory readers are flat
    // `readdir()` calls with no subdirectory recursion.
    let default_themes = glob_ext_flat(&plugin_dir.join("themes"), "json").await;
    let default_workflows = glob_ext_flat(&plugin_dir.join("workflows"), "js").await;
    let default_hooks = load_standard_hooks(plugin_dir).await;
    let default_mcp_servers = if skip_mcp_discovery {
        HashMap::new()
    } else {
        load_mcp_servers(plugin_dir).await
    };
    let default_lsp_servers = load_lsp_servers(plugin_dir).await;

    let commands = match &parsed.commands {
        Some(decl) => resolve_commands_declared(plugin_dir, decl.clone()).await,
        None => default_commands,
    };
    let agents = match &parsed.agents {
        Some(paths) => resolve_markdown_declared_paths(plugin_dir, paths.clone()).await,
        None => default_agents,
    };
    let mut skills = default_skills;
    if let Some(paths) = &parsed.skills {
        skills.extend(resolve_skill_declared_paths(plugin_dir, paths.clone()).await);
        dedup_component_paths(&mut skills);
    }
    let output_styles = match &parsed.output_styles {
        Some(paths) => resolve_markdown_declared_paths(plugin_dir, paths.clone()).await,
        None => default_output_styles,
    };
    // Oracle: `let nt = A.experimental?.themes ?? A.themes` (@162826143), with
    // the auto-scan suppressed on the SAME coalesced value
    // (`Ie=!(A.experimental?.themes??A.themes)&&me`, @162824976) — so
    // `experimental.themes` both wins over the top-level key AND hides the
    // `themes/` folder.
    let declared_themes = parsed
        .experimental
        .as_ref()
        .and_then(|e| e.themes.as_ref())
        .or(parsed.themes.as_ref());
    let themes = match declared_themes {
        Some(paths) => resolve_ext_declared_paths(plugin_dir, paths.clone(), "json").await,
        None => default_themes,
    };
    let workflows = match &parsed.workflows {
        Some(paths) => resolve_ext_declared_paths(plugin_dir, paths.clone(), "js").await,
        None => default_workflows,
    };
    let mut hooks = default_hooks;
    hooks.extend(load_declared_hooks(plugin_dir, parsed.hooks.clone()).await);
    let mut mcp_servers = default_mcp_servers;
    if !skip_mcp_discovery {
        mcp_servers.extend(
            load_declared_mcp_servers(
                plugin_dir,
                parsed.mcp_servers.clone(),
                &parsed.name,
                confined,
            )
            .await,
        );
    }
    let mut lsp_servers = default_lsp_servers;
    lsp_servers.extend(load_declared_lsp_servers(plugin_dir, parsed.lsp_servers.clone()).await);
    let hljs_languages = parsed
        .experimental
        .as_ref()
        .and_then(|e| e.syntax_highlighting.as_ref())
        .map(|s| s.hljs_languages.0.clone())
        .unwrap_or_default();
    let binaries = resolve_binaries(parsed.binaries.as_ref());
    let monitors = resolve_monitors(plugin_dir, parsed.monitors.as_ref()).await;

    PluginComponents {
        declared_skill_path_count,
        declared_command_path_count,
        declared_agent_path_count,
        commands,
        agents,
        skills,
        output_styles,
        themes,
        workflows,
        hljs_languages,
        binaries,
        monitors,
        hooks,
        hooks_declared,
        mcp_servers,
        mcp_servers_declared,
        lsp_servers,
        lsp_servers_declared,
        skip_mcp_discovery,
    }
}

fn path_decl_count(paths: &PathDecl) -> u32 {
    match paths {
        PathDecl::One(_) => 1,
        PathDecl::Many(values) => values.len() as u32,
    }
}

async fn canonical_plugin_root(plugin_dir: &Path) -> Option<PathBuf> {
    canonical_plain_directory(plugin_dir).await?;
    Some(plugin_dir.to_path_buf())
}

/// Return the canonical location of a plain directory, refusing symlink
/// roots. Component scans use this before opening a directory so a symlinked
/// `commands/`, `skills/`, or `themes/` folder cannot make discovery read
/// outside the plugin root.
async fn canonical_plain_directory(path: &Path) -> Option<PathBuf> {
    let metadata = tokio::fs::symlink_metadata(path).await.ok()?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return None;
    }
    tokio::fs::canonicalize(path).await.ok()
}

async fn confined_plugin_install_dir(path: &Path, roots: &[&Path]) -> Option<PathBuf> {
    let metadata = tokio::fs::symlink_metadata(path).await.ok()?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return None;
    }
    let canonical = tokio::fs::canonicalize(path).await.ok()?;
    for root in roots {
        let root_metadata = tokio::fs::symlink_metadata(root).await.ok()?;
        if !root_metadata.file_type().is_dir() || root_metadata.file_type().is_symlink() {
            continue;
        }
        let canonical_root = tokio::fs::canonicalize(root).await.ok()?;
        if canonical.starts_with(canonical_root) {
            return Some(path.to_path_buf());
        }
    }
    None
}

/// Resolve a declared component path and verify both lexical and canonical
/// containment. Symlink-spelled component roots are rejected altogether;
/// this gives every caller the same no-follow behavior without relying on a
/// later registry-specific read to notice an escape.
fn resolve_declared_relative_path(plugin_dir: &Path, raw: &str) -> Option<PathBuf> {
    let rel = raw.strip_prefix("./")?;
    if rel.is_empty() {
        return None;
    }
    let path = Path::new(rel);
    if path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return None;
    }
    let candidate = plugin_dir.join(path);
    let metadata = std::fs::symlink_metadata(&candidate).ok()?;
    if metadata.file_type().is_symlink() {
        return None;
    }
    let canonical_root = std::fs::canonicalize(plugin_dir).ok()?;
    let canonical = std::fs::canonicalize(&candidate).ok()?;
    canonical.starts_with(canonical_root).then_some(candidate)
}

fn canonical_regular_path_under(root: &Path, path: &Path) -> Option<PathBuf> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return None;
    }
    let canonical_root = std::fs::canonicalize(root).ok()?;
    let canonical = std::fs::canonicalize(path).ok()?;
    canonical
        .starts_with(canonical_root)
        .then_some(path.to_path_buf())
}

/// oracle `Jqt`: at most 64 valid `binaries` entries survive.
const MAX_BINARIES: usize = 64;

/// Safe basename charset (oracle `dCe`: `^[a-z0-9](?:[a-z0-9._-]*[a-z0-9_-])?$`):
/// first char alnum-lowercase; last char (when length >= 2) alnum-lowercase/
/// `_`/`-`; any interior chars additionally allow `.`.
fn is_valid_binary_basename(name: &str) -> bool {
    let bytes = name.as_bytes();
    let is_first = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let is_last = |b: u8| is_first(b) || b == b'_' || b == b'-';
    let is_mid = |b: u8| is_last(b) || b == b'.';
    match bytes {
        [] => false,
        [only] => is_first(*only),
        [first, .., last] => {
            is_first(*first)
                && is_last(*last)
                && bytes[1..bytes.len() - 1].iter().all(|&b| is_mid(b))
        }
    }
}

/// `^[0-9a-f]{64}$` (oracle `Yqt`) — lowercase hex only.
fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `binaries` (oracle `qs`/`n1e`): a LENIENT `.transform()`, never a parse
/// failure — an invalid key (bad basename) or invalid value (missing/
/// malformed `sha256`) is silently dropped, one entry at a time; only the
/// first [`MAX_BINARIES`] valid entries (in manifest key order — this port's
/// `serde_json::Map` preserves source insertion order, matching JS
/// `Object.entries`) survive, the rest are dropped without a diagnostic,
/// matching the oracle's own silent cap. Parsing + validation only: the
/// actual fetch into `bin/` at install time is a separate concern this
/// change does not wire.
fn resolve_binaries(value: Option<&Value>) -> HashMap<String, BinaryPin> {
    let mut out = HashMap::new();
    let Some(Value::Object(map)) = value else {
        return out;
    };
    for (name, entry) in map {
        if out.len() >= MAX_BINARIES {
            break;
        }
        if !is_valid_binary_basename(name) {
            continue;
        }
        let Some(sha256) = entry.get("sha256").and_then(Value::as_str) else {
            continue;
        };
        if !is_valid_sha256_hex(sha256) {
            continue;
        }
        out.insert(
            name.clone(),
            BinaryPin {
                sha256: sha256.to_string(),
            },
        );
    }
    out
}

/// Parse a bare JSON array of monitor objects — the same shape used both for
/// the manifest-inline form (already validated at `RawManifest`-parse time;
/// see [`RawMonitorsDecl`]) and for a path-form/auto-scanned FILE, which is a
/// runtime/filesystem concern: `None` on any shape/validation failure
/// (duplicate name, bad `when`, wrong JSON shape) — the caller warns and
/// treats it as empty rather than failing the whole plugin, since by this
/// point the synchronous manifest-parse stage has already succeeded.
fn parse_monitor_array(raw: &str) -> Option<Vec<PluginMonitor>> {
    let entries: Vec<RawPluginMonitor> = serde_json::from_str(raw).ok()?;
    let mut seen = BTreeSet::new();
    for entry in &entries {
        if !seen.insert(entry.name.clone()) {
            return None;
        }
    }
    Some(
        entries
            .into_iter()
            .map(RawPluginMonitor::into_monitor)
            .collect(),
    )
}

/// Auto-scan `monitors/monitors.json` (oracle `mt`'s own description: "When
/// omitted, monitors/monitors.json at the plugin root is loaded if
/// present") — a bare JSON array of monitor objects, same shape as the
/// inline manifest form. Only reached when the manifest declares no
/// `monitors` field at all (REPLACE, not merge — same rule
/// `outputStyles`/`themes`/`workflows` already follow).
async fn load_default_monitors(plugin_dir: &Path) -> Vec<PluginMonitor> {
    let path = plugin_dir.join("monitors").join("monitors.json");
    let Some(path) = canonical_regular_path_under(plugin_dir, &path) else {
        return Vec::new();
    };
    let Ok(raw) = tokio::fs::read_to_string(&path).await else {
        return Vec::new();
    };
    let raw = raw.strip_prefix(UTF8_BOM).unwrap_or(raw.as_str());
    if let Some(monitors) = parse_monitor_array(raw) {
        monitors
    } else {
        tracing::warn!(path = %path.display(), "skipping malformed monitors/monitors.json");
        Vec::new()
    }
}

/// Resolve the `monitors` field: an inline declaration is already fully
/// validated ([`RawMonitorsDecl::Inline`]); a declared PATH is read + parsed
/// here (warn + empty on any failure); an absent field falls back to the
/// [`load_default_monitors`] auto-scan.
async fn resolve_monitors(plugin_dir: &Path, decl: Option<&RawMonitorsDecl>) -> Vec<PluginMonitor> {
    match decl {
        None => load_default_monitors(plugin_dir).await,
        Some(RawMonitorsDecl::Inline(monitors)) => monitors.clone(),
        Some(RawMonitorsDecl::Path(raw_path)) => {
            let Some(abs) = resolve_declared_relative_path(plugin_dir, raw_path) else {
                tracing::warn!(path = %raw_path, "skipping invalid plugin manifest monitors path");
                return Vec::new();
            };
            let Ok(raw) = tokio::fs::read_to_string(&abs).await else {
                tracing::warn!(path = %abs.display(), "monitors file not found");
                return Vec::new();
            };
            let raw = raw.strip_prefix(UTF8_BOM).unwrap_or(raw.as_str());
            if let Some(monitors) = parse_monitor_array(raw) {
                monitors
            } else {
                tracing::warn!(path = %abs.display(), "skipping malformed monitors file");
                Vec::new()
            }
        }
    }
}

fn dedup_component_paths(paths: &mut Vec<ComponentPath>) {
    let mut seen = BTreeSet::new();
    paths.retain(|cp| seen.insert(cp.path.clone()));
}

fn component_root_metadata(root: &Path) -> Option<Value> {
    Some(serde_json::json!({ "root": root }))
}

fn stamp_component_root(paths: &mut [ComponentPath], root: &Path) {
    for path in paths {
        path.metadata = component_root_metadata(root);
    }
}

async fn resolve_markdown_declared_paths(plugin_dir: &Path, paths: PathDecl) -> Vec<ComponentPath> {
    let mut out = Vec::new();
    for raw in paths.into_vec() {
        let Some(abs) = resolve_declared_relative_path(plugin_dir, &raw) else {
            tracing::warn!(path = %raw, "skipping invalid plugin manifest markdown path");
            continue;
        };
        let Ok(meta) = tokio::fs::metadata(&abs).await else {
            tracing::warn!(path = %abs.display(), "skipping missing plugin manifest markdown path");
            continue;
        };
        if meta.is_dir() {
            let mut found = glob_md(&abs).await;
            stamp_component_root(&mut found, &abs);
            out.extend(found);
        } else if abs
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        {
            let root = abs.parent().unwrap_or(plugin_dir).to_path_buf();
            out.push(ComponentPath {
                path: abs,
                metadata: component_root_metadata(&root),
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    dedup_component_paths(&mut out);
    out
}

/// Resolve a `themes`/`workflows`-shaped declaration (oracle `union([path,
/// path[]])`): a directory entry is FLAT-scanned (one level, no recursion —
/// see [`glob_ext_flat`]) for files matching `ext`; a file entry is kept only
/// when its own extension matches `ext`. Mirrors
/// [`resolve_markdown_declared_paths`] but for a non-`.md` extension and a
/// non-recursive directory scan (the oracle's theme/workflow directory
/// readers are single-level `readdir()` calls, unlike the recursive
/// `.md` walk `glob_md` models for commands/agents/output-styles).
async fn resolve_ext_declared_paths(
    plugin_dir: &Path,
    paths: PathDecl,
    ext: &str,
) -> Vec<ComponentPath> {
    let mut out = Vec::new();
    for raw in paths.into_vec() {
        let Some(abs) = resolve_declared_relative_path(plugin_dir, &raw) else {
            tracing::warn!(path = %raw, ext, "skipping invalid plugin manifest path");
            continue;
        };
        let Ok(meta) = tokio::fs::metadata(&abs).await else {
            tracing::warn!(path = %abs.display(), "skipping missing plugin manifest path");
            continue;
        };
        if meta.is_dir() {
            let mut found = glob_ext_flat(&abs, ext).await;
            stamp_component_root(&mut found, &abs);
            out.extend(found);
        } else if abs
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case(ext))
        {
            let root = abs.parent().unwrap_or(plugin_dir).to_path_buf();
            out.push(ComponentPath {
                path: abs,
                metadata: component_root_metadata(&root),
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    dedup_component_paths(&mut out);
    out
}

/// Resolve the `commands` field for all three shapes `Rs` permits: a single
/// path / path array behaves exactly like the other markdown component
/// fields; the commands-only object-map form is handled by
/// [`resolve_command_entries`].
async fn resolve_commands_declared(plugin_dir: &Path, decl: CommandsDecl) -> Vec<ComponentPath> {
    match decl {
        CommandsDecl::One(raw) => {
            resolve_markdown_declared_paths(plugin_dir, PathDecl::One(raw)).await
        }
        CommandsDecl::Many(raws) => {
            resolve_markdown_declared_paths(plugin_dir, PathDecl::Many(raws)).await
        }
        CommandsDecl::Map(entries) => resolve_command_entries(plugin_dir, entries).await,
    }
}

/// Resolve the object-map form of `commands` (oracle `record(string, Ds)`):
/// each key names a slash command and its value carries either an on-disk
/// `source` markdown file or inline `content`.
///
/// `source` entries resolve to a real [`ComponentPath`] exactly like an
/// array entry, so they load correctly (the actual §6 fix — previously ANY
/// object-form `commands` value made deserialization of the whole
/// `plugin.json` fail, vanishing the plugin entirely regardless of shape).
///
/// `content` entries have no file for `PluginManager::load_plugin`
/// (`plugin/src/manager.rs`) to read — it always reads `ComponentPath::path`
/// off disk — so they are skipped with a diagnostic rather than silently
/// dropped or (worse) faked into an empty file. Wiring inline command bodies
/// (and honoring the map key as the resulting slash-command name, per the
/// oracle's `"about" → "/plugin:about"` behavior) requires a change to that
/// file and is deferred, not fixed here.
async fn resolve_command_entries(
    plugin_dir: &Path,
    entries: BTreeMap<String, RawCommandEntry>,
) -> Vec<ComponentPath> {
    let mut out = Vec::new();
    for (name, entry) in entries {
        match entry.source {
            Some(source) => {
                let Some(abs) = resolve_declared_relative_path(plugin_dir, &source) else {
                    tracing::warn!(
                        command = %name,
                        path = %source,
                        "skipping invalid plugin manifest command source path"
                    );
                    continue;
                };
                let root = abs.parent().unwrap_or(plugin_dir).to_path_buf();
                out.push(ComponentPath {
                    path: abs,
                    metadata: component_root_metadata(&root),
                });
            }
            None => {
                // The source/content refinement guarantees `content` is
                // `Some` here.
                tracing::warn!(
                    command = %name,
                    "skipping plugin command with inline `content` -- inline \
                     command bodies are not yet materialized"
                );
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    dedup_component_paths(&mut out);
    out
}

async fn resolve_skill_declared_paths(plugin_dir: &Path, paths: PathDecl) -> Vec<ComponentPath> {
    let mut out = Vec::new();
    for raw in paths.into_vec() {
        let Some(abs) = resolve_declared_relative_path(plugin_dir, &raw) else {
            tracing::warn!(path = %raw, "skipping invalid plugin manifest skill path");
            continue;
        };
        let Ok(meta) = tokio::fs::metadata(&abs).await else {
            tracing::warn!(path = %abs.display(), "skipping missing plugin manifest skill path");
            continue;
        };
        if meta.is_dir() {
            let direct_skill = abs.join("SKILL.md");
            if let Some(direct_skill) = canonical_regular_path_under(&abs, &direct_skill) {
                let root = abs.parent().unwrap_or(plugin_dir);
                out.push(ComponentPath {
                    path: direct_skill,
                    metadata: component_root_metadata(root),
                });
            } else {
                let mut found = glob_skill_dirs(&abs).await;
                stamp_component_root(&mut found, &abs);
                out.extend(found);
            }
        } else if abs.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") {
            let root = abs
                .parent()
                .and_then(Path::parent)
                .unwrap_or(plugin_dir)
                .to_path_buf();
            out.push(ComponentPath {
                path: abs,
                metadata: component_root_metadata(&root),
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    dedup_component_paths(&mut out);
    out
}

/// Collect plugin skills recursively, stopping at every directory that owns a
/// `SKILL.md`. Nested grouping directories are allowed, but content below a
/// discovered skill root is not scanned as another independent skill.
async fn glob_skill_dirs(skills_dir: &Path) -> Vec<ComponentPath> {
    let mut out = Vec::new();
    if canonical_plain_directory(skills_dir).await.is_none() {
        return out;
    }
    let mut stack = vec![skills_dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        if current != skills_dir {
            let skill_md = current.join("SKILL.md");
            if let Some(skill_md) = canonical_regular_path_under(skills_dir, &skill_md) {
                out.push(ComponentPath {
                    path: skill_md,
                    metadata: component_root_metadata(skills_dir),
                });
                continue;
            }
        }
        let Ok(mut entries) = tokio::fs::read_dir(&current).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(entry.path());
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

// ---------- §3 + §4: plugin MCP-discovery suppression ----------
//
// Oracle `OL` (`pluginLoader.ts` @160861000) gates plugin MCP-server
// discovery in this order (built-ins aside — `if(e.isBuiltin)return` is not
// modeled here: nothing in this file's discovery paths ever produces a
// `PluginSource::BuiltIn` plugin, so the check has no reachable call site to
// guard):
//
//   1. an SDK-host per-plugin `skipMcpDiscovery:true` request always wins
//      (`PluginConfigSchema`'s `local` variant, @155779385) — the SDK host
//      owns this plugin's MCP connections itself, so the engine must not open
//      a second copy;
//   2. otherwise, `CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS` (raw JS-string
//      truthiness: any non-empty value, not `isEnvTruthy`'s enumerated set —
//      confirmed against the oracle's own `if(a.CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS)`
//      bare property check) suppresses discovery for every non-built-in
//      plugin;
//   3. UNLESS `CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS_EXCEPT` (oracle `SCn`, same
//      offset) names this plugin: comma-split, each trimmed entry containing
//      `@` matches the plugin's `name@marketplace` install-source identity
//      case-insensitively (oracle `qy`: `e===n||e.toLowerCase()===n.toLowerCase()`);
//      an entry WITHOUT `@` matches the bare plugin name, but only when this
//      plugin has a known install-source identity at all (oracle `!pM(e)` —
//      `pM` gates the same "directory-loaded plugin" confinement checks a few
//      lines later in `OL`, i.e. a raw `--plugin-dir`/ad-hoc directory load
//      has no marketplace pedigree to match a bare name against).
//
// LINGXI_-prefixed aliases are accepted first, per this crate's env
// convention (`tools/ui/src/brief.rs`'s `LINGXI_BRIEF`/`CLAUDE_CODE_BRIEF`
// pair is the precedent this mirrors byte-for-byte on the truthiness rule).

/// Case-insensitive exact match — oracle `qy(e,n)`:
/// `e===n||e.toLowerCase()===n.toLowerCase()`.
fn qy_eq(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

/// Raw JS-string truthiness for an env var: unset or empty is falsy, any
/// other value (including `"0"`/`"false"`) is truthy. Distinct from
/// `lingxi_core::host::env::is_env_truthy`'s stricter `1|true|yes|on` allowlist — the
/// oracle reads `CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS` as a bare
/// `process.env` property, not through `isEnvTruthy`.
fn env_set_nonempty(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.is_empty())
}

fn env_value_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS` (+ `LINGXI_` alias) presence check.
fn skip_plugin_mcp_servers_env_set() -> bool {
    env_set_nonempty("LINGXI_SKIP_PLUGIN_MCP_SERVERS")
        || env_set_nonempty("CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS")
}

/// `CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS_EXCEPT` (+ `LINGXI_` alias) value.
fn skip_plugin_mcp_servers_except() -> Option<String> {
    env_value_nonempty("LINGXI_SKIP_PLUGIN_MCP_SERVERS_EXCEPT")
        .or_else(|| env_value_nonempty("CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS_EXCEPT"))
}

/// Oracle `SCn(e)`: does the `_EXCEPT` list re-admit this plugin past an
/// active `CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS` suppression?
fn except_exempts(name: &str, install_source_id: Option<&str>) -> bool {
    let Some(except) = skip_plugin_mcp_servers_except() else {
        return false;
    };
    // Oracle `r=!pM(e)`: bare-name matching is suppressed only for a plugin
    // auto-loaded from `.claude/skills/` (`scope==="project"` AND
    // `source.endsWith("@skills-dir")`), which this port never produces — so
    // `r` is uniformly true and a bare name matches `manifest.name` on every
    // load path, `--plugin-dir` included. See the `confined` note in
    // `load_plugin_from_path_with_mcp_gate`.
    except.split(',').any(|raw| {
        let entry = raw.trim();
        if entry.is_empty() {
            return false;
        }
        if entry.contains('@') {
            install_source_id.is_some_and(|id| qy_eq(entry, id))
        } else {
            qy_eq(entry, name)
        }
    })
}

/// Resolve whether this plugin's MCP server discovery (`.mcp.json` + manifest
/// `mcpServers`) should be suppressed for this load. See the module note
/// above for the full oracle-derived order; `name` is the manifest's declared
/// `name` (`RawManifest::name`), not a display label.
fn resolve_skip_mcp_discovery(
    name: &str,
    sdk_skip_mcp_discovery: bool,
    install_source_id: Option<&str>,
) -> bool {
    if sdk_skip_mcp_discovery {
        return true;
    }
    if !skip_plugin_mcp_servers_env_set() {
        return false;
    }
    !except_exempts(name, install_source_id)
}

/// Serialize tests that mutate the process-wide plugin MCP suppression vars.
#[cfg(test)]
pub(crate) fn skip_mcp_test_env_guard() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    std::env::remove_var("LINGXI_SKIP_PLUGIN_MCP_SERVERS");
    std::env::remove_var("CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS");
    std::env::remove_var("LINGXI_SKIP_PLUGIN_MCP_SERVERS_EXCEPT");
    std::env::remove_var("CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS_EXCEPT");
    guard
}

/// Read the plugin-root `.mcp.json` into `{ server name → McpServerConfig }`.
///
/// Mirrors `loadPluginMcpServers` (`mcpPluginIntegration.ts:137`): the file is
/// the standard `.mcp.json` shape `{ "mcpServers": { … } }`, but the shared
/// parser also accepts a bare top-level map of `{name: serverConfig}` via the
/// `parsed.mcpServers || parsed` fallback (`mcpPluginIntegration.ts:243`), so
/// plugin `.mcp.json` files in either form resolve here. A missing / malformed
/// file yields an empty map (non-fatal — claude-code logs and continues), and
/// an individual invalid entry is skipped while valid siblings are kept.
/// Manifest-declared `mcpServers` is merged separately by
/// [`load_declared_mcp_servers`].
async fn load_mcp_servers(plugin_dir: &Path) -> HashMap<String, mcp::McpServerConfig> {
    let path = plugin_dir.join(".mcp.json");
    let Some(path) = canonical_regular_path_under(plugin_dir, &path) else {
        return HashMap::new();
    };
    let Ok(raw) = tokio::fs::read_to_string(&path).await else {
        return HashMap::new();
    };
    let raw = raw.strip_prefix(UTF8_BOM).unwrap_or(raw.as_str());
    // Plugin MCP servers are dynamic-scoped (`addPluginScopeToServers` uses
    // `scope: 'dynamic'`, `mcpPluginIntegration.ts:353`).
    //
    // The PLUGIN layer validates against the full 8-arm union `KY`
    // (`vve`: `let B=KY().safeParse(U)`), not the 7-key config table `ZGn`
    // that `.mcp.json` / settings / `--mcp-config` go through — so the two
    // internal-only IDE transports are accepted here and rejected there.
    match mcp::parse_plugin_mcp_json_string(raw, mcp::ConfigScope::Dynamic) {
        Ok(configs) => configs.into_iter().map(|c| (c.name.clone(), c)).collect(),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "skipping malformed plugin .mcp.json");
            HashMap::new()
        }
    }
}

/// Read the plugin-root `.lsp.json` into `{ server name → LspServerConfig }`.
///
/// Mirrors `loadPluginLspServers` (`lspPluginIntegration.ts:64`): the file is a
/// `Record<name, LspServerConfig>`. The record key is the server name; if an
/// entry omits its own `name`, the key is stamped onto the config (the
/// registry keys by `config.name`). A missing / malformed file yields an empty
/// map. Manifest-declared `lspServers` is merged separately by
/// [`load_declared_lsp_servers`].
async fn load_lsp_servers(
    plugin_dir: &Path,
) -> IndexMap<String, lingxi_core::host::LspServerConfig> {
    let path = plugin_dir.join(".lsp.json");
    let Some(path) = canonical_regular_path_under(plugin_dir, &path) else {
        return IndexMap::new();
    };
    let Ok(raw) = tokio::fs::read_to_string(&path).await else {
        return IndexMap::new();
    };
    let raw = raw.strip_prefix(UTF8_BOM).unwrap_or(raw.as_str());
    let parsed: IndexMap<String, Value> = match serde_json::from_str(raw) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "skipping malformed plugin .lsp.json");
            return IndexMap::new();
        }
    };
    parse_lsp_records(parsed)
}

/// Collect every `*.md` file under `dir` **recursively** as a [`ComponentPath`],
/// sorted by path for deterministic ordering. Returns an empty vec when `dir`
/// does not exist (the auto-detect "directory absent" case).
///
/// Mirrors `walkPluginMarkdown` (`walkPluginMarkdown.ts:21-69`): it descends
/// into subdirectories so nested files carry their directory namespace
/// (`commands/git/commit.md` → command `git:commit`, then `{plugin}:git:commit`
/// via `command_name_from_path` + the manager's `{plugin}:` prefix), and it
/// matches the `.md` extension case-insensitively (TS `toLowerCase()`).
/// (`stopAtSkillDir` is not modeled here: LingXi discovers plugin skills only
/// from the dedicated `skills/` directory via [`glob_skill_dirs`], so a
/// `SKILL.md` nested under `commands/` is not a supported LingXi layout.)
async fn glob_md(dir: &Path) -> Vec<ComponentPath> {
    let mut out = Vec::new();
    let Some(canonical_root) = canonical_plain_directory(dir).await else {
        return out;
    };
    // Iterative DFS (avoids boxing for async recursion). A directory that
    // cannot be read is skipped, matching TS's swallowed readdir errors.
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&current).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let p = entry.path();
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                stack.push(p);
            } else if p
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("md"))
            {
                let Ok(metadata) = std::fs::symlink_metadata(&p) else {
                    continue;
                };
                let Ok(canonical) = std::fs::canonicalize(&p) else {
                    continue;
                };
                if metadata.file_type().is_symlink() || !canonical.starts_with(&canonical_root) {
                    continue;
                }
                out.push(ComponentPath {
                    path: p,
                    metadata: component_root_metadata(dir),
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Collect every file directly under `dir` whose extension matches `ext`
/// (case-insensitive), sorted by path. Unlike [`glob_md`], this does NOT
/// recurse into subdirectories — a bare, single-level `readdir()`, matching
/// the oracle's theme (`.json`) and workflow (`.js`) directory readers (both
/// a plain `fs.readdir(dir)` over just that directory's direct entries, with
/// no subdirectory walk). Returns an empty vec when `dir` does not exist.
async fn glob_ext_flat(dir: &Path, ext: &str) -> Vec<ComponentPath> {
    let mut out = Vec::new();
    let Some(canonical_root) = canonical_plain_directory(dir).await else {
        return out;
    };
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            continue;
        }
        let p = entry.path();
        if p.extension()
            .and_then(|s| s.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case(ext))
        {
            let Ok(metadata) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            let Ok(canonical) = std::fs::canonicalize(&p) else {
                continue;
            };
            if metadata.file_type().is_symlink() || !canonical.starts_with(&canonical_root) {
                continue;
            }
            out.push(ComponentPath {
                path: p,
                metadata: component_root_metadata(dir),
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Load the plugin settings understood by Claude Code's plugin runtime. A
/// valid plugin-root `settings.json` takes precedence over manifest `settings`;
/// an absent or malformed file falls back to the manifest. Unknown keys are
/// discarded instead of merging arbitrary plugin data into host settings.
async fn load_plugin_settings(
    plugin_dir: &Path,
    manifest_settings: Option<&HashMap<String, Value>>,
) -> HashMap<String, Value> {
    const ALLOWED: [&str; 2] = ["agent", "subagentStatusLine"];

    let settings_path = plugin_dir.join("settings.json");
    let selected = match canonical_regular_path_under(plugin_dir, &settings_path)
        .map(tokio::fs::read_to_string)
    {
        None => manifest_settings.cloned().unwrap_or_default(),
        Some(read) => match read.await {
            Ok(raw) => match serde_json::from_str::<HashMap<String, Value>>(&raw) {
                Ok(settings) => settings,
                Err(error) => {
                    tracing::warn!(
                        path = %settings_path.display(),
                        error = %error,
                        "ignoring malformed plugin settings.json"
                    );
                    manifest_settings.cloned().unwrap_or_default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                manifest_settings.cloned().unwrap_or_default()
            }
            Err(error) => {
                tracing::warn!(
                    path = %settings_path.display(),
                    error = %error,
                    "unable to read plugin settings.json"
                );
                manifest_settings.cloned().unwrap_or_default()
            }
        },
    };

    selected
        .into_iter()
        .filter(|(key, _)| ALLOWED.contains(&key.as_str()))
        .collect()
}

/// `hooks/hooks.json`'s wrapper shape (oracle `ITt`/`PluginHooksSchema`):
/// `{description?, hooks?, modules?}`, constrained to at most ONE `modules`
/// entry ("hooks.json `modules` names one hooks module per plugin; a second
/// entry is refused") and requiring `hooks` or `modules` (or both) —
/// "hooks.json must have `hooks` (the hook matchers) or `modules` (hooks
/// modules), or both". A `modules` entry names a path, relative to this
/// `hooks.json`, of a JS module exporting `register(on)` that PROGRAMMATICALLY
/// registers hooks when loaded — a wholly different mechanism from the
/// declarative `hooks` matcher tree this crate already parses into
/// [`hooks::HookDefinition`]s.
#[derive(Debug, Deserialize)]
struct RawHooksFile {
    #[serde(default)]
    hooks: Option<Value>,
    #[serde(default)]
    modules: Option<Vec<String>>,
}

/// Parse `hooks/hooks.json` into [`hooks::HookDefinition`]s, if present.
///
/// The file's `hooks` half (when present) is validated against the same
/// `HooksSettings` shape used by settings files and fed to the engine's
/// settings-hook parser as `{ "hooks": <hooks> }`, stamping
/// [`HookSource::Plugin`] — see [`RawHooksFile`] for the wrapper shape and
/// the `hooks`/`modules` constraints enforced here.
///
/// A `modules` entry is recognized and validated (the 1-entry cap; the
/// "hooks or modules, or both" requirement) but NOT executed: running the
/// JS module it names needs a JS-module-execution subsystem this crate does
/// not have, so its declared hooks are simply never registered (a warning
/// names the module path so this is not a silent gap for whoever authored
/// the plugin).
async fn load_standard_hooks(plugin_dir: &Path) -> Vec<hooks::HookDefinition> {
    let path = plugin_dir.join("hooks").join("hooks.json");
    let Some(path) = canonical_regular_path_under(plugin_dir, &path) else {
        return Vec::new();
    };
    let Ok(raw) = tokio::fs::read_to_string(&path).await else {
        return Vec::new();
    };
    let raw = raw.strip_prefix(UTF8_BOM).unwrap_or(raw.as_str());
    let wrapper: RawHooksFile = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "skipping malformed hooks.json");
            return Vec::new();
        }
    };
    let module_count = wrapper.modules.as_ref().map_or(0, Vec::len);
    if module_count > 1 {
        tracing::warn!(
            path = %path.display(),
            "skipping hooks.json: `modules` names one hooks module per plugin; \
             a second entry is refused"
        );
        return Vec::new();
    }
    if wrapper.hooks.is_none() && module_count == 0 {
        tracing::warn!(
            path = %path.display(),
            "skipping hooks.json: must have `hooks` (the hook matchers) or `modules` \
             (hooks modules), or both"
        );
        return Vec::new();
    }
    if let Some(module_path) = wrapper.modules.as_ref().and_then(|m| m.first()) {
        tracing::warn!(
            path = %path.display(),
            module = %module_path,
            "hooks.json declares a `modules` hooks module, but this engine cannot execute \
             hooks modules yet — its programmatically-registered hooks will not run"
        );
    }
    let Some(inner) = wrapper.hooks else {
        return Vec::new();
    };
    // The file wraps the settings-shaped hooks under a `hooks` key.
    parse_hooks_value(&serde_json::json!({ "hooks": inner }), &path)
}

fn parse_hooks_value(value: &Value, path: &Path) -> Vec<hooks::HookDefinition> {
    let inner = value.get("hooks").unwrap_or(value);
    let settings = serde_json::json!({ "hooks": inner });
    match parse_hooks_from_settings_json(&settings.to_string(), HookSource::Plugin) {
        Ok(hooks) => hooks,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "failed to parse plugin hooks");
            Vec::new()
        }
    }
}

async fn load_declared_hooks(
    plugin_dir: &Path,
    value: Option<Value>,
) -> Vec<hooks::HookDefinition> {
    let Some(value) = value else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match value {
        Value::String(path) => {
            out.extend(load_declared_hooks_from_path(plugin_dir, &path).await);
        }
        // Oracle `zs`: `hooks` is `union([path, inlineObject, array(union([path,
        // inlineObject]))])` — each ARRAY ITEM independently is a path or an
        // inline hooks object; a mixed array (some string paths, some inline
        // objects) is valid. Handle every item on its own merits instead of
        // requiring the whole array to be homogeneous, which previously threw
        // away ALL entries (both the paths and the inline objects) the moment
        // one item wasn't a string.
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::String(path) => {
                        out.extend(load_declared_hooks_from_path(plugin_dir, &path).await);
                    }
                    other => out.extend(parse_hooks_value(&other, plugin_dir)),
                }
            }
        }
        other => out.extend(parse_hooks_value(&other, plugin_dir)),
    }
    out
}

async fn load_declared_hooks_from_path(plugin_dir: &Path, raw: &str) -> Vec<hooks::HookDefinition> {
    let Some(path) = resolve_declared_relative_path(plugin_dir, raw) else {
        tracing::warn!(path = %raw, "skipping invalid plugin manifest hooks path");
        return Vec::new();
    };
    let Ok(raw_json) = tokio::fs::read_to_string(&path).await else {
        return Vec::new();
    };
    let raw_json = raw_json.strip_prefix(UTF8_BOM).unwrap_or(raw_json.as_str());
    let Ok(value) = serde_json::from_str::<Value>(raw_json) else {
        return Vec::new();
    };
    parse_hooks_value(&value, &path)
}

/// True when `raw` names an MCPB/`.dxt` bundle (oracle `DL(e){return
/// e.endsWith(".mcpb")||e.endsWith(".dxt")}`): a literal, case-SENSITIVE
/// suffix check on the raw string — deliberately NOT `Path::extension()`
/// (which would diverge on an edge case like the bare `".mcpb"`, no
/// basename, and does not apply to a URL string at all), to stay byte-exact
/// with the oracle.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn is_mcpb_source(raw: &str) -> bool {
    raw.ends_with(".mcpb") || raw.ends_with(".dxt")
}

/// True when `raw` is a URL rather than a relative path (oracle `ZHe`).
fn is_url_source(raw: &str) -> bool {
    raw.starts_with("http://") || raw.starts_with("https://")
}

/// `mcpServers` (oracle `js`): `union([V(), st(), record(string,KY()),
/// array(union([V(), st(), record(string,KY())]))])` — a `.json` path, an
/// MCPB/`.dxt` path or URL (oracle's `st`, §14 row 1's MCPB union arm), an
/// inline `{name: config}` record, or a mixed array of any of the three.
async fn load_declared_mcp_servers(
    plugin_dir: &Path,
    value: Option<Value>,
    plugin_name: &str,
    confined: bool,
) -> HashMap<String, mcp::McpServerConfig> {
    let Some(value) = value else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    match value {
        Value::String(raw) => {
            merge_one_declared_mcp_source(plugin_dir, &raw, plugin_name, confined, &mut out).await;
        }
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::String(raw) => {
                        merge_one_declared_mcp_source(
                            plugin_dir,
                            &raw,
                            plugin_name,
                            confined,
                            &mut out,
                        )
                        .await;
                    }
                    other => {
                        if let Ok(parsed) = mcp::parse_plugin_mcp_json_string(
                            &other.to_string(),
                            mcp::ConfigScope::Dynamic,
                        ) {
                            out.extend(parsed.into_iter().map(|cfg| (cfg.name.clone(), cfg)));
                        }
                    }
                }
            }
        }
        other => {
            if let Ok(parsed) =
                mcp::parse_plugin_mcp_json_string(&other.to_string(), mcp::ConfigScope::Dynamic)
            {
                out.extend(parsed.into_iter().map(|cfg| (cfg.name.clone(), cfg)));
            }
        }
    }
    out
}

/// Resolve ONE string-valued `mcpServers` entry (the bare field, or one array
/// item): dispatches to the MCPB loader when it names a `.mcpb`/`.dxt`
/// source (oracle `DL(F)`), otherwise falls back to the plain `.json`-path
/// loader shared with `mcpServers`'s non-MCPB forms.
async fn merge_one_declared_mcp_source(
    plugin_dir: &Path,
    raw: &str,
    plugin_name: &str,
    confined: bool,
    out: &mut HashMap<String, mcp::McpServerConfig>,
) {
    if is_mcpb_source(raw) {
        if confined {
            // Oracle `OL`'s `x(F)` closure, the `DL(F)` branch: "Skipping
            // MCPB source "{F}" for directory-loaded plugin "{name}": not
            // resolved without a pre-approval download here — declare MCP
            // servers inline or via a local in-dir .mcp.json."
            tracing::warn!(
                source = %raw,
                plugin = %plugin_name,
                "Skipping MCPB source \"{raw}\" for directory-loaded plugin \"{plugin_name}\": \
                 not resolved without a pre-approval download here — declare MCP servers inline \
                 or via a local in-dir .mcp.json."
            );
            return;
        }
        if let Some(cfg) = load_mcpb_mcp_server(plugin_dir, raw, plugin_name).await {
            out.insert(cfg.name.clone(), cfg);
        }
        return;
    }
    // §22 (oracle `chr`'s confinement test, the sibling of the MCPB skip
    // above): a directory-loaded plugin's non-MCPB source that would resolve
    // outside the plugin directory is skipped with this specific copy,
    // rather than falling through to `merge_declared_json_records`'s generic
    // "skipping invalid plugin manifest json path" (shared by every other
    // declared-path field, and worded for a malformed path, not a
    // confinement refusal).
    if confined && resolve_declared_relative_path(plugin_dir, raw).is_none() {
        let message = out_of_directory_mcp_source_message(raw, plugin_name);
        tracing::warn!(source = %raw, plugin = %plugin_name, "{message}");
        return;
    }
    let parse = |raw_json: &str| {
        mcp::parse_plugin_mcp_json_string(raw_json, mcp::ConfigScope::Dynamic).map(|v| {
            v.into_iter()
                .map(|cfg| (cfg.name.clone(), cfg))
                .collect::<HashMap<_, _>>()
        })
    };
    merge_declared_json_records(plugin_dir, raw, &parse, out).await;
}

/// §22: the oracle's exact copy for [`merge_one_declared_mcp_source`]'s
/// out-of-directory guard: `Skipping out-of-directory MCP source "${F}" for
/// directory-loaded plugin "${e.name}": it may only reference files inside
/// the plugin directory here.`
fn out_of_directory_mcp_source_message(raw: &str, plugin_name: &str) -> String {
    format!(
        "Skipping out-of-directory MCP source \"{raw}\" for directory-loaded plugin \
         \"{plugin_name}\": it may only reference files inside the plugin directory here."
    )
}

/// Resolve ONE path-form `.mcpb`/`.dxt` `mcpServers` entry into a single
/// named MCP server config (oracle `Pit`/`uct`, byte-source recovered from
/// the 2.1.251 Mach-O @159489593/@160861879). A URL source is recognized
/// (so it is not silently mistaken for a malformed path) but its network
/// fetch is DEFERRED — see the module-level notes in `mcpb.rs` — and this
/// returns `None` with a diagnostic instead.
async fn load_mcpb_mcp_server(
    plugin_dir: &Path,
    raw: &str,
    plugin_name: &str,
) -> Option<mcp::McpServerConfig> {
    if is_url_source(raw) {
        tracing::warn!(
            url = %raw,
            plugin = %plugin_name,
            "MCPB URL sources are not fetched during plugin discovery (deferred to the install \
             path); skipping declared MCP server"
        );
        return None;
    }
    let mcpb_path = resolve_declared_relative_path(plugin_dir, raw)?;
    let Ok(bytes) = tokio::fs::read(&mcpb_path).await else {
        // Oracle: "MCPB file not found: {path}".
        tracing::warn!(path = %mcpb_path.display(), "MCPB file not found: {}", mcpb_path.display());
        return None;
    };
    // Oracle logs the first 16 hex chars of the sha256 content hash.
    let full_hash = crate::mcpb::sha256_hex(&bytes);
    let short_hash: String = full_hash.chars().take(16).collect();
    tracing::info!("MCPB content hash: {short_hash}");

    // Oracle `uct`: `ge=tE(x,W)` where `x=join(pluginPath,".mcpb-cache")` and
    // `W` is the 16-char content-hash prefix — that directory IS the
    // `extensionPath` `${__dirname}` expands to, so keying it on the full
    // 64-hex digest diverges from claude-code's on-disk layout and argv.
    let cache_dir = plugin_dir.join(".mcpb-cache").join(&short_hash);
    let manifest_path = cache_dir.join("manifest.json");
    // Oracle `uct` gates its fast path on the cache METADATA record (`rje`,
    // written by `Zpe` only AFTER a successful extraction) plus the `k4t`
    // freshness check — never on bare directory existence — so an extraction
    // that threw is simply retried next session. This port has no metadata
    // store, so the extracted `manifest.json` is the completion marker, and a
    // failed unpack tears its own directory back down; otherwise the very
    // directory created one line before the unpack would cache the FAILURE
    // forever.
    if !tokio::fs::try_exists(&manifest_path).await.unwrap_or(false) {
        tokio::fs::create_dir_all(&cache_dir).await.ok()?;
        if let Err(e) = crate::mcpb::unpack_mcpb(&bytes, &cache_dir) {
            tracing::warn!(error = %e, path = %mcpb_path.display(), "failed to extract MCPB archive");
            tokio::fs::remove_dir_all(&cache_dir).await.ok();
            return None;
        }
    }

    let Ok(manifest_raw) = tokio::fs::read_to_string(&manifest_path).await else {
        // Oracle: "No manifest.json found in MCPB file: {source}".
        tracing::warn!(
            source = %raw,
            "No manifest.json found in MCPB file: {raw}"
        );
        return None;
    };
    let manifest: crate::mcpb::McpbManifest = match serde_json::from_str(&manifest_raw) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, path = %manifest_path.display(), "invalid MCPB manifest.json");
            return None;
        }
    };
    if manifest.server.is_none() {
        // Oracle: `MCPB manifest for "${pe.name}" does not define a server configuration`.
        tracing::warn!(
            plugin = %plugin_name,
            "MCPB manifest for \"{}\" does not define a server configuration",
            manifest.name
        );
        return None;
    }
    let generated = crate::mcpb::generate_mcp_config(&manifest, &cache_dir)?;
    let cfg =
        mcp::build_server_from_json_entry(&manifest.name, &generated, mcp::ConfigScope::Dynamic)?;
    // Oracle: `Loaded MCP server "{name}" from MCPB (extracted to {extractedPath})`.
    tracing::info!(
        "Loaded MCP server \"{}\" from MCPB (extracted to {})",
        cfg.name,
        cache_dir.display()
    );
    Some(cfg)
}

async fn load_declared_lsp_servers(
    plugin_dir: &Path,
    value: Option<Value>,
) -> IndexMap<String, lingxi_core::host::LspServerConfig> {
    let Some(value) = value else {
        return IndexMap::new();
    };
    let items = match value {
        Value::Array(items) => items,
        one => vec![one],
    };
    let mut out = IndexMap::new();
    for item in items {
        let parsed = match item {
            Value::String(raw_path) => {
                let Some(path) = resolve_declared_relative_path(plugin_dir, &raw_path) else {
                    tracing::warn!(path = %raw_path, "skipping invalid plugin manifest json path");
                    continue;
                };
                let Ok(raw) = tokio::fs::read_to_string(&path).await else {
                    continue;
                };
                serde_json::from_str::<IndexMap<String, Value>>(&raw)
            }
            inline @ Value::Object(_) => serde_json::from_value::<IndexMap<String, Value>>(inline),
            _ => continue,
        };
        if let Ok(parsed) = parsed {
            out.extend(parse_lsp_records(parsed));
        }
    }
    out
}

fn parse_lsp_records(
    records: IndexMap<String, Value>,
) -> IndexMap<String, lingxi_core::host::LspServerConfig> {
    records
        .into_iter()
        .filter_map(|(key, value)| {
            let mut config = match serde_json::from_value::<lingxi_core::host::LspServerConfig>(value) {
                Ok(config) => config,
                Err(error) => {
                    tracing::warn!(server = %key, %error, "skipping malformed plugin LSP server configuration");
                    return None;
                }
            };
            // The record key is authoritative in Claude Code's public shape;
            // an extra legacy `name` field must not rename or collide with a
            // sibling server.
            config.name.clone_from(&key);
            validate_lsp_config(&config).then(|| (config.name.clone(), config))
        })
        .collect()
}

fn validate_lsp_config(config: &lingxi_core::host::LspServerConfig) -> bool {
    let valid = !config.command.trim().is_empty()
        && !config.extension_to_language.is_empty()
        && matches!(config.transport.as_str(), "stdio" | "socket")
        && config.startup_timeout.is_none_or(|timeout| timeout > 0)
        && config.shutdown_timeout.is_none_or(|timeout| timeout > 0);
    if !valid {
        tracing::warn!(
            server = %config.name,
            "skipping invalid plugin LSP server configuration"
        );
    }
    valid
}

async fn load_declared_json_records<T, E>(
    plugin_dir: &Path,
    value: Option<Value>,
    parse: impl Fn(&str) -> Result<HashMap<String, T>, E>,
) -> HashMap<String, T> {
    let Some(value) = value else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    match value {
        Value::String(path) => {
            merge_declared_json_records(plugin_dir, &path, &parse, &mut out).await;
        }
        // Oracle `js`/`Ws`: `mcpServers`/`lspServers` accept
        // `array(union([path, inlineRecord]))` — each item is independently
        // a path OR an inline `{name: config}` record, so a mixed array (some
        // paths, some inline servers) is valid. Resolve every item on its own
        // terms instead of requiring the whole array to be all-string, which
        // previously dropped EVERY entry (paths included) as soon as one item
        // was an inline object.
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::String(path) => {
                        merge_declared_json_records(plugin_dir, &path, &parse, &mut out).await;
                    }
                    other => {
                        if let Ok(parsed) = parse(&other.to_string()) {
                            out.extend(parsed);
                        }
                    }
                }
            }
        }
        other => {
            if let Ok(parsed) = parse(&other.to_string()) {
                out.extend(parsed);
            }
        }
    }
    out
}

async fn merge_declared_json_records<T, E>(
    plugin_dir: &Path,
    raw_path: &str,
    parse: &impl Fn(&str) -> Result<HashMap<String, T>, E>,
    out: &mut HashMap<String, T>,
) {
    let Some(path) = resolve_declared_relative_path(plugin_dir, raw_path) else {
        tracing::warn!(path = %raw_path, "skipping invalid plugin manifest json path");
        return;
    };
    let Ok(raw) = tokio::fs::read_to_string(&path).await else {
        return;
    };
    let raw = raw.strip_prefix(UTF8_BOM).unwrap_or(raw.as_str());
    if let Ok(parsed) = parse(raw) {
        out.extend(parsed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::sync::Arc;
    use telemetry::{AnalyticsValue, InMemorySink};

    fn string_field<'a>(metadata: &'a telemetry::LogEventMetadata, key: &str) -> &'a str {
        match metadata.get(key) {
            Some(AnalyticsValue::String(value)) => value,
            other => panic!("expected string field {key}, got {other:?}"),
        }
    }

    fn bool_field(metadata: &telemetry::LogEventMetadata, key: &str) -> bool {
        match metadata.get(key) {
            Some(AnalyticsValue::Bool(value)) => *value,
            other => panic!("expected bool field {key}, got {other:?}"),
        }
    }

    fn int_field(metadata: &telemetry::LogEventMetadata, key: &str) -> i64 {
        match metadata.get(key) {
            Some(AnalyticsValue::Int(value)) => *value,
            other => panic!("expected integer field {key}, got {other:?}"),
        }
    }

    fn assert_no_raw_path(metadata: &telemetry::LogEventMetadata, path: &Path) {
        let raw_path = path.display().to_string();
        for (key, value) in metadata {
            if let AnalyticsValue::String(value) = value {
                assert!(
                    !value.contains(&raw_path),
                    "metadata field {key} leaked plugin path: {value}"
                );
            }
        }
    }

    struct SeedEnvRestore {
        previous_claude: Option<OsString>,
        previous_lingxi: Option<OsString>,
    }

    impl SeedEnvRestore {
        fn capture() -> Self {
            Self {
                previous_claude: std::env::var_os("CLAUDE_CODE_PLUGIN_SEED_DIR"),
                previous_lingxi: std::env::var_os("LINGXI_PLUGIN_SEED_DIR"),
            }
        }
    }

    impl Drop for SeedEnvRestore {
        fn drop(&mut self) {
            match self.previous_claude.as_ref() {
                Some(value) => std::env::set_var("CLAUDE_CODE_PLUGIN_SEED_DIR", value),
                None => std::env::remove_var("CLAUDE_CODE_PLUGIN_SEED_DIR"),
            }
            match self.previous_lingxi.as_ref() {
                Some(value) => std::env::set_var("LINGXI_PLUGIN_SEED_DIR", value),
                None => std::env::remove_var("LINGXI_PLUGIN_SEED_DIR"),
            }
        }
    }

    /// §22: byte-exact copy for the out-of-directory MCP source skip (the
    /// oracle string recovered from the 2.1.251 Mach-O @160863770), sibling
    /// of the already-landed MCPB skip a few lines above.
    #[test]
    fn out_of_directory_mcp_source_message_matches_the_oracle_copy() {
        assert_eq!(
            out_of_directory_mcp_source_message("../escape.json", "demo"),
            "Skipping out-of-directory MCP source \"../escape.json\" for directory-loaded \
             plugin \"demo\": it may only reference files inside the plugin directory here."
        );
    }

    // NOTE: no behavioral test calls `merge_one_declared_mcp_source` with
    // `confined: true` here. `resolve_declared_relative_path` already refuses
    // any `..`/absolute path for EVERY plugin regardless of `confined` (the
    // general path-confinement net `merge_declared_json_records` falls back
    // on), so `out` ends up empty either way — a test asserting only
    // `out.is_empty()` would stay green even with the new `if confined && …`
    // branch above deleted entirely, which I confirmed by deleting it and
    // re-running: no compile error, no red. The only OBSERVABLE difference
    // the new branch makes is which `tracing::warn!` line fires, which this
    // crate has no subscriber-capture harness to assert on (nothing else
    // here does either — see the MCPB skip a few lines up, also untested
    // this way). The copy itself is pinned by the test above; the dead-until-
    // `confined` `if` is unreachable in production today, exactly like its
    // already-landed MCPB sibling — see the `confined` doc note at this
    // file's `load_plugin_from_path`.

    #[tokio::test]
    async fn declared_commands_replace_defaults_and_declared_skills_extend_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::create_dir_all(plugin.join("commands")).unwrap();
        fs::create_dir_all(plugin.join("custom-commands")).unwrap();
        fs::create_dir_all(plugin.join("skills/base")).unwrap();
        fs::create_dir_all(plugin.join("extra-skills/extra")).unwrap();

        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{
                "name":"demo",
                "commands":"./custom-commands",
                "skills":"./extra-skills"
            }"#,
        )
        .unwrap();
        fs::write(plugin.join("commands/default.md"), "default").unwrap();
        fs::write(plugin.join("custom-commands/custom.md"), "custom").unwrap();
        fs::write(plugin.join("skills/base/SKILL.md"), "base").unwrap();
        fs::write(plugin.join("extra-skills/extra/SKILL.md"), "extra").unwrap();

        let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap();
        let commands: Vec<_> = manifest
            .components
            .commands
            .iter()
            .map(|c| c.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(commands, vec!["custom.md"]);

        let skills: Vec<_> = manifest
            .components
            .skills
            .iter()
            .map(|c| {
                c.path
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(skills, vec!["base", "extra"]);
    }

    /// Themes (`.json`) and workflows (`.js`) auto-scan when the manifest
    /// declares neither — and the scan is FLAT (one level): a file nested one
    /// directory deeper than the component root is not picked up, matching
    /// the oracle's plain (non-recursive) `readdir()` reader for both
    /// component kinds (unlike the recursive `.md` walk `commands`/`agents`/
    /// `output-styles` use).
    #[tokio::test]
    async fn themes_and_workflows_auto_scan_flat_when_undeclared() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::create_dir_all(plugin.join("themes/nested")).unwrap();
        fs::create_dir_all(plugin.join("workflows/nested")).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"demo"}"#,
        )
        .unwrap();
        fs::write(plugin.join("themes/dark.json"), "{}").unwrap();
        fs::write(plugin.join("themes/nested/deep.json"), "{}").unwrap();
        fs::write(plugin.join("workflows/deploy.js"), "// workflow").unwrap();
        fs::write(plugin.join("workflows/nested/deep.js"), "// workflow").unwrap();

        let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap();
        let themes: Vec<_> = manifest
            .components
            .themes
            .iter()
            .map(|c| c.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(themes, vec!["dark.json"], "flat scan only, no nested/");

        let workflows: Vec<_> = manifest
            .components
            .workflows
            .iter()
            .map(|c| c.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(workflows, vec!["deploy.js"], "flat scan only, no nested/");
    }

    /// A manifest-declared `themes`/`workflows` path REPLACES (does not merge
    /// with) the directory auto-scan — same rule as `outputStyles`/`agents`.
    /// Also exercises the `union([path, path[]])` array form and the
    /// single-file-path form.
    #[tokio::test]
    async fn declared_themes_and_workflows_replace_the_auto_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::create_dir_all(plugin.join("themes")).unwrap();
        fs::create_dir_all(plugin.join("custom-themes")).unwrap();
        fs::create_dir_all(plugin.join("workflows")).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{
                "name":"demo",
                "themes":["./custom-themes"],
                "workflows":"./workflows/solo.js"
            }"#,
        )
        .unwrap();
        fs::write(plugin.join("themes/auto.json"), "{}").unwrap();
        fs::write(plugin.join("custom-themes/custom.json"), "{}").unwrap();
        fs::write(plugin.join("workflows/solo.js"), "// solo").unwrap();
        fs::write(plugin.join("workflows/ignored.js"), "// ignored").unwrap();

        let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap();
        let themes: Vec<_> = manifest
            .components
            .themes
            .iter()
            .map(|c| c.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(themes, vec!["custom.json"], "auto-scan suppressed");

        let workflows: Vec<_> = manifest
            .components
            .workflows
            .iter()
            .map(|c| c.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(
            workflows,
            vec!["solo.js"],
            "single declared .js file, not the whole directory"
        );
    }

    /// `themes` is declared at TWO layers — `pt` is spread into both the
    /// top-level manifest schema `jpe` and `Vs`'s inner `experimental` object
    /// — and the oracle's record builder PREFERS the experimental one:
    /// `let nt = A.experimental?.themes ?? A.themes` (@162826143). The
    /// `themes/` auto-scan is suppressed on the same coalesced value
    /// (`Ie=!(A.experimental?.themes??A.themes)&&me`, @162824976).
    ///
    /// So all three arms below are one oracle fact: `experimental.themes`
    /// wins over the top-level key, and hides the `themes/` folder even when
    /// the top-level key is absent.
    ///
    /// ⚠️ Workflows are DIFFERENT: `Be=!A.workflows&&fe` /
    /// `if(A.workflows){…}` (@162826334) read the top-level key alone, with
    /// no `experimental` alias — the last arm pins that asymmetry so a
    /// "make it symmetric" tidy-up cannot land silently.
    #[tokio::test]
    async fn experimental_themes_outrank_the_top_level_themes_key() {
        let themes_of = |manifest: &PluginManifest| -> Vec<String> {
            manifest
                .components
                .themes
                .iter()
                .map(|c| c.path.file_name().unwrap().to_str().unwrap().to_string())
                .collect()
        };
        let seed = |plugin: &std::path::Path| {
            fs::create_dir_all(plugin.join("themes")).unwrap();
            fs::create_dir_all(plugin.join("palettes")).unwrap();
            fs::create_dir_all(plugin.join("custom-themes")).unwrap();
            fs::create_dir_all(plugin.join("workflows")).unwrap();
            fs::write(plugin.join("themes/legacy.json"), "{}").unwrap();
            fs::write(plugin.join("palettes/purple.json"), "{}").unwrap();
            fs::write(plugin.join("custom-themes/custom.json"), "{}").unwrap();
            fs::write(plugin.join("workflows/auto.js"), "// auto").unwrap();
        };

        // (1) `experimental.themes` alone — beats the `themes/` auto-scan.
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path());
        write_manifest(
            tmp.path(),
            &serde_json::json!({
                "name": "acme",
                "experimental": {"themes": "./palettes/purple.json"}
            }),
        );
        let (_id, manifest) = load_plugin_from_path(tmp.path()).await.unwrap();
        assert_eq!(
            themes_of(&manifest),
            vec!["purple.json"],
            "experimental.themes must suppress the themes/ auto-scan"
        );

        // (2) BOTH layers present — `??` takes the experimental one.
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path());
        write_manifest(
            tmp.path(),
            &serde_json::json!({
                "name": "acme",
                "themes": ["./custom-themes"],
                "experimental": {"themes": ["./palettes"]}
            }),
        );
        let (_id, manifest) = load_plugin_from_path(tmp.path()).await.unwrap();
        assert_eq!(
            themes_of(&manifest),
            vec!["purple.json"],
            "experimental.themes must outrank the top-level themes key"
        );

        // (3) `experimental` present but WITHOUT `themes` — the top-level key
        //     still resolves (`??` only coalesces on nullish).
        let tmp = tempfile::tempdir().unwrap();
        seed(tmp.path());
        write_manifest(
            tmp.path(),
            &serde_json::json!({
                "name": "acme",
                "themes": ["./custom-themes"],
                "experimental": {"workflows": "./palettes"}
            }),
        );
        let (_id, manifest) = load_plugin_from_path(tmp.path()).await.unwrap();
        assert_eq!(
            themes_of(&manifest),
            vec!["custom.json"],
            "an experimental block with no themes key must not shadow the top-level one"
        );
        // …and `experimental.workflows` is NOT read (no oracle alias): the
        // `workflows/` auto-scan still wins.
        let workflows: Vec<_> = manifest
            .components
            .workflows
            .iter()
            .map(|c| c.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(
            workflows,
            vec!["auto.js"],
            "workflows has no experimental alias in the oracle"
        );
    }

    /// Oracle `gt`: `type`, `title`, and `description` are all required (no
    /// `.optional()`) on a `userConfig` field. A field missing any of the
    /// three fails the WHOLE `plugin.json` parse — the same "one bad entry
    /// sinks the manifest" convention `RawCommandEntry` already establishes —
    /// rather than silently defaulting to `None`/empty string.
    #[tokio::test]
    async fn user_config_field_missing_type_title_or_description_fails_the_whole_manifest() {
        for missing in ["type", "title", "description"] {
            let mut field = serde_json::json!({
                "type": "string",
                "title": "API token",
                "description": "token",
            });
            field.as_object_mut().unwrap().remove(missing);
            let manifest_json = serde_json::json!({
                "name": "demo",
                "userConfig": { "API_TOKEN": field },
            });

            let tmp = tempfile::tempdir().unwrap();
            let plugin = tmp.path();
            fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
            fs::write(
                plugin
                    .join(branding::PLUGIN_MANIFEST_DIR)
                    .join("plugin.json"),
                manifest_json.to_string(),
            )
            .unwrap();

            assert!(
                load_plugin_from_path(plugin).await.is_none(),
                "missing {missing:?} must sink the whole manifest, not just the field"
            );
        }
    }

    /// §8: the load path — NOT just `plugin tag`/`plugin init` — must reject a
    /// manifest whose declared `name` fails the shared oracle validator (here,
    /// a space; the other failure modes are unit-tested directly against
    /// [`validate_plugin_name`] above).
    #[tokio::test]
    async fn load_path_skips_a_manifest_with_an_invalid_name() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"my plugin"}"#,
        )
        .unwrap();

        assert!(
            load_plugin_from_path(plugin).await.is_none(),
            "a space-containing name must sink the whole manifest at the LOAD path, \
             not just at `plugin tag`/`plugin init`"
        );
    }

    #[tokio::test]
    async fn invalid_name_emits_plugin_load_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"my plugin"}"#,
        )
        .unwrap();
        let sink = Arc::new(InMemorySink::new());
        let bus = Arc::new(AnalyticsBus::new());
        bus.attach_sink(sink.clone()).await;

        assert!(load_plugin_from_path_with_bus(plugin, Some(&bus))
            .await
            .is_none());

        let events = sink.events().await;
        assert_eq!(events.len(), 1, "one load failure event expected");
        let record = &events[0];
        assert_eq!(record.name, plugin_telemetry::LOAD_FAILED);
        assert_eq!(
            string_field(&record.metadata, "error_category"),
            "invalid-name"
        );
        assert_eq!(string_field(&record.metadata, "plugin_scope"), "user-local");
        assert!(!bool_field(&record.metadata, "cache_only"));
        assert!(!bool_field(&record.metadata, "is_official_plugin"));
        assert_eq!(
            string_field(&record.metadata, "plugin_name_redacted"),
            "(redacted)"
        );
        assert_eq!(
            string_field(&record.metadata, "_PROTO_plugin_name"),
            "my plugin"
        );
        for (key, value) in &record.metadata {
            if key.starts_with("_PROTO_") {
                continue;
            }
            if let AnalyticsValue::String(value) = value {
                assert!(
                    !value.contains("my plugin"),
                    "non-proto field {key} leaked raw plugin name: {value}"
                );
            }
        }
        assert_no_raw_path(&record.metadata, plugin);
    }

    #[tokio::test]
    async fn malformed_manifest_emits_plugin_load_failed_to_analytics_bus() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"demo""#,
        )
        .unwrap();
        let sink = Arc::new(InMemorySink::new());
        let bus = Arc::new(AnalyticsBus::new());
        bus.attach_sink(sink.clone()).await;

        assert!(load_plugin_from_path_with_mcp_gate_and_bus(
            plugin,
            false,
            Some("demo@claude-code-marketplace"),
            Some(&bus),
        )
        .await
        .is_none());

        let events = sink.events().await;
        assert_eq!(events.len(), 1, "one load failure event expected");
        let record = &events[0];
        assert_eq!(record.name, plugin_telemetry::LOAD_FAILED);
        assert_eq!(
            string_field(&record.metadata, "error_category"),
            "malformed-plugin-json"
        );
        assert!(bool_field(&record.metadata, "cache_only"));
        assert!(bool_field(&record.metadata, "is_official_plugin"));
        assert_eq!(
            string_field(&record.metadata, "plugin_scope"),
            "cache-installed"
        );
        assert_eq!(string_field(&record.metadata, "component"), "manifest");
        assert_eq!(
            string_field(&record.metadata, "_PROTO_marketplace_name"),
            "claude-code-marketplace"
        );
        assert_eq!(
            string_field(&record.metadata, "marketplace_name_redacted"),
            "(redacted)"
        );
        assert_no_raw_path(&record.metadata, plugin);
    }

    #[tokio::test]
    async fn successful_load_does_not_emit_plugin_load_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"demo"}"#,
        )
        .unwrap();
        let sink = Arc::new(InMemorySink::new());
        let bus = Arc::new(AnalyticsBus::new());
        bus.attach_sink(sink.clone()).await;

        assert!(load_plugin_from_path_with_bus(plugin, Some(&bus))
            .await
            .is_some());
        assert!(
            sink.events().await.is_empty(),
            "successful load must not emit load_failed"
        );
    }

    async fn load_collision_plugin(
        root: &Path,
        marketplace: &str,
        name: &str,
    ) -> (PluginId, PluginManifest, PathBuf) {
        let dir = root
            .join("cache")
            .join(marketplace)
            .join(name)
            .join("1.0.0");
        fs::create_dir_all(dir.join("commands")).unwrap();
        write_manifest(&dir, &serde_json::json!({"name": name, "version": "1.0.0"}));
        fs::write(dir.join("commands/ping.md"), "# ping").unwrap();
        let source_id = format!("{name}@{marketplace}");
        let (id, manifest) =
            load_plugin_from_path_with_mcp_gate(&dir, false, Some(source_id.as_str()))
                .await
                .expect("collision fixture loads");
        (id, manifest, dir)
    }

    #[tokio::test]
    async fn component_collision_winner_is_order_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let first = load_collision_plugin(tmp.path(), "market-a", "collision").await;
        let second = load_collision_plugin(tmp.path(), "market-b", "collision").await;

        let forward = resolve_discovered_plugins(vec![first.clone(), second.clone()], None).await;
        let reverse = resolve_discovered_plugins(vec![second.clone(), first.clone()], None).await;

        assert_eq!(forward.len(), 1);
        assert_eq!(reverse.len(), 1);
        assert_eq!(forward[0].2, reverse[0].2);
        assert_eq!(forward[0].1.name, "collision");
    }

    #[tokio::test]
    async fn component_collision_emits_one_privacy_safe_event_per_name() {
        let tmp = tempfile::tempdir().unwrap();
        let first = load_collision_plugin(tmp.path(), "market-a", "collision").await;
        let second = load_collision_plugin(tmp.path(), "market-b", "collision").await;
        let sink = Arc::new(InMemorySink::new());
        let bus = Arc::new(AnalyticsBus::new());
        bus.attach_sink(sink.clone()).await;

        let resolved = resolve_discovered_plugins(vec![first, second], Some(&bus)).await;
        assert_eq!(resolved.len(), 1);

        let events = sink.events().await;
        let collisions = events
            .iter()
            .filter(|event| event.name == plugin_telemetry::NAME_COLLISION)
            .collect::<Vec<_>>();
        assert_eq!(
            collisions.len(),
            1,
            "one event for one resolved command name"
        );
        let metadata = &collisions[0].metadata;
        assert_eq!(string_field(metadata, "item_type"), "command");
        assert_eq!(
            string_field(metadata, "_PROTO_skill_name"),
            "collision:ping"
        );
        assert_eq!(int_field(metadata, "source_count"), 2);
        let sources = string_field(metadata, "sources");
        assert!(!sources.contains(tmp.path().to_string_lossy().as_ref()));
        let winner = string_field(metadata, "winner_source");
        assert!(sources.split(',').any(|source| source == winner));
        assert!(!winner.contains('/'));

        let shadows = events
            .iter()
            .filter(|event| event.name == plugin_telemetry::FOLDER_SHADOWED)
            .collect::<Vec<_>>();
        assert_eq!(
            shadows.len(),
            1,
            "only the losing source folder is shadowed"
        );
        assert_eq!(string_field(&shadows[0].metadata, "component"), "commands");
        assert_no_raw_path(&shadows[0].metadata, tmp.path());
    }

    #[tokio::test]
    async fn local_source_precedes_cache_source_for_same_plugin_name() {
        let tmp = tempfile::tempdir().unwrap();
        let local_path = tmp.path().join("local/collision");
        fs::create_dir_all(local_path.join("commands")).unwrap();
        write_manifest(&local_path, &serde_json::json!({"name": "collision"}));
        fs::write(local_path.join("commands/ping.md"), "# local").unwrap();
        let (local_id, local_manifest) = load_plugin_from_path(&local_path).await.unwrap();
        let local = (local_id, local_manifest, local_path);

        let cached = load_collision_plugin(tmp.path(), "market", "collision").await;
        let forward = resolve_discovered_plugins(vec![cached.clone(), local.clone()], None).await;
        let reverse = resolve_discovered_plugins(vec![local, cached], None).await;
        assert_eq!(forward.len(), 1);
        assert_eq!(reverse.len(), 1);
        assert!(forward[0].2.ends_with("local/collision"));
        assert_eq!(forward[0].2, reverse[0].2);
    }

    #[tokio::test]
    async fn seed_cache_is_used_only_when_primary_is_missing_and_is_shadowed_otherwise() {
        let _guard = crate::plugin_seed_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Declared after the mutex guard so it restores both process-global
        // variables before the lock is released, including during unwinding.
        let _restore = SeedEnvRestore::capture();
        std::env::remove_var("LINGXI_PLUGIN_SEED_DIR");

        let tmp = tempfile::tempdir().unwrap();
        let primary = tmp.path().join("plugins/cache/market/seeded/1.0.0");
        let seed = tmp.path().join("seed/cache/market/seeded/1.0.0");
        for (dir, body) in [(&primary, "# primary"), (&seed, "# seed")] {
            fs::create_dir_all(dir.join("commands")).unwrap();
            write_manifest(dir, &serde_json::json!({"name": "seeded"}));
            fs::write(dir.join("commands/ping.md"), body).unwrap();
        }
        std::env::set_var("CLAUDE_CODE_PLUGIN_SEED_DIR", tmp.path().join("seed"));

        let sink = Arc::new(InMemorySink::new());
        let bus = Arc::new(AnalyticsBus::new());
        bus.attach_sink(sink.clone()).await;
        let enabled = BTreeMap::from([(String::from("seeded@market"), true)]);
        let discovered = discover_enabled_plugins_with_bus(
            tmp.path().join("plugins").as_path(),
            &enabled,
            Some(&bus),
        )
        .await;
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].2, primary);
        assert_eq!(
            fs::read_to_string(discovered[0].2.join("commands/ping.md")).unwrap(),
            "# primary"
        );
        let shadows = sink
            .events()
            .await
            .into_iter()
            .filter(|event| event.name == plugin_telemetry::FOLDER_SHADOWED)
            .collect::<Vec<_>>();
        assert_eq!(shadows.len(), 1);
        assert_eq!(string_field(&shadows[0].metadata, "component"), "commands");
        assert_no_raw_path(&shadows[0].metadata, tmp.path());

        // Removing the primary cache causes the first configured seed root to
        // become the actual loaded source.
        fs::remove_dir_all(tmp.path().join("plugins/cache")).unwrap();
        let loaded_from_seed =
            discover_enabled_plugins_with_bus(tmp.path().join("plugins").as_path(), &enabled, None)
                .await;
        assert_eq!(loaded_from_seed.len(), 1);
        assert_eq!(loaded_from_seed[0].2, seed);
    }

    #[tokio::test]
    async fn rename_resolution_follows_chain_and_reports_cycles_and_depth() {
        let mut renames = HashMap::from([
            ("old".to_string(), Some("middle".to_string())),
            ("middle".to_string(), Some("new".to_string())),
        ]);
        let known = HashSet::from(["new".to_string()]);
        assert_eq!(
            resolve_rename_chain("old", &renames, &known),
            Some(RenameResolution::Renamed {
                to: "new".to_string(),
                chain_depth: 2,
            })
        );

        renames.insert("new".to_string(), None);
        assert_eq!(
            resolve_rename_chain("old", &renames, &known),
            Some(RenameResolution::Removed { chain_depth: 3 })
        );

        let cycle = HashMap::from([
            ("old".to_string(), Some("middle".to_string())),
            ("middle".to_string(), Some("old".to_string())),
        ]);
        assert_eq!(
            resolve_rename_chain("old", &cycle, &HashSet::new()),
            Some(RenameResolution::Unresolved { reason: "cycle" })
        );

        let missing = HashMap::from([("old".to_string(), Some("missing".to_string()))]);
        assert_eq!(
            resolve_rename_chain("old", &missing, &HashSet::new()),
            Some(RenameResolution::Unresolved {
                reason: "target-missing"
            })
        );

        let mut deep = HashMap::new();
        for index in 0..16 {
            deep.insert(format!("n{index}"), Some(format!("n{}", index + 1)));
        }
        deep.insert("n16".to_string(), Some("n17".to_string()));
        assert_eq!(
            resolve_rename_chain("n0", &deep, &HashSet::from(["n17".to_string()])),
            Some(RenameResolution::Unresolved {
                reason: "chain-too-deep"
            })
        );
    }

    #[tokio::test]
    async fn rename_updates_identity_preserves_install_provenance_and_emits_once() {
        let tmp = tempfile::tempdir().unwrap();
        let install_dir = tmp.path().join("cache/market/old/1.0.0");
        fs::create_dir_all(install_dir.join("commands")).unwrap();
        write_manifest(&install_dir, &serde_json::json!({"name": "new"}));
        fs::write(install_dir.join("commands/ping.md"), "# ping").unwrap();
        let catalog = tmp.path().join("marketplaces/market/.lingxi-plugin");
        fs::create_dir_all(&catalog).unwrap();
        fs::write(
            catalog.join("marketplace.json"),
            serde_json::json!({
                "plugins": [{"name": "new"}],
                "renames": {"old": "new"}
            })
            .to_string(),
        )
        .unwrap();

        let sink = Arc::new(InMemorySink::new());
        let bus = Arc::new(AnalyticsBus::new());
        bus.attach_sink(sink.clone()).await;
        let source_id = "old@market";
        let loaded = load_plugin_from_path_with_mcp_gate_and_bus(
            &install_dir,
            false,
            Some(source_id),
            Some(&bus),
        )
        .await
        .expect("renamed plugin loads");
        assert_eq!(loaded.1.name, "new");
        match &loaded.1.source {
            PluginSource::LocalPath { path } => assert_eq!(path, &install_dir),
            source => panic!("renamed cache source changed unexpectedly: {source:?}"),
        }

        let events = sink.events().await;
        let renamed = events
            .iter()
            .filter(|event| event.name == plugin_telemetry::RENAMED)
            .collect::<Vec<_>>();
        assert_eq!(renamed.len(), 1);
        assert_eq!(string_field(&renamed[0].metadata, "outcome"), "renamed");
        assert_eq!(int_field(&renamed[0].metadata, "chain_depth"), 1);
        assert_eq!(
            string_field(&renamed[0].metadata, "_PROTO_plugin_name"),
            "old"
        );
        assert_no_raw_path(&renamed[0].metadata, tmp.path());

        // Re-reading the same source must not duplicate the diagnostic on the
        // shared bus, but it must continue to preserve the old install path.
        let loaded_again = load_plugin_from_path_with_mcp_gate_and_bus(
            &install_dir,
            false,
            Some(source_id),
            Some(&bus),
        )
        .await
        .expect("renamed plugin reloads");
        assert_eq!(loaded_again.1.name, "new");
        match &loaded_again.1.source {
            PluginSource::LocalPath { path } => assert_eq!(path, &install_dir),
            source => panic!("renamed cache source changed unexpectedly: {source:?}"),
        }
        assert_eq!(
            sink.events()
                .await
                .iter()
                .filter(|event| event.name == plugin_telemetry::RENAMED)
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn component_scans_refuse_symlink_escape() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path().join("plugin");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::create_dir_all(plugin.join("commands")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        write_manifest(&plugin, &serde_json::json!({"name": "safe"}));
        fs::write(outside.join("escape.md"), "# escaped").unwrap();
        symlink(outside.join("escape.md"), plugin.join("commands/escape.md")).unwrap();
        assert!(load_plugin_from_path(&plugin)
            .await
            .expect("plugin manifest loads")
            .1
            .components
            .commands
            .is_empty());

        let linked_commands = plugin.join("linked-commands");
        symlink(&outside, &linked_commands).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            serde_json::json!({"name": "safe", "commands": "./linked-commands"}).to_string(),
        )
        .unwrap();
        assert!(load_plugin_from_path(&plugin)
            .await
            .expect("plugin manifest loads")
            .1
            .components
            .commands
            .is_empty());
    }

    /// Oracle `gt`'s `type` is a fixed enum (`string`/`number`/`boolean`/
    /// `directory`/`file`), not an arbitrary string.
    #[tokio::test]
    async fn user_config_field_type_outside_the_fixed_enum_fails_the_whole_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{
                "name":"demo",
                "userConfig":{"API_TOKEN":{"type":"enum","title":"t","description":"d"}}
            }"#,
        )
        .unwrap();

        assert!(
            load_plugin_from_path(plugin).await.is_none(),
            "\"enum\" is not one of the fixed userConfig field types"
        );
    }

    /// Oracle `Fs`: the TOP-LEVEL `userConfig` map's keys must be identifiers
    /// (`^[A-Za-z_]\w*$`); a channel's `userConfig` keys carry no such
    /// constraint (bare `record(i(), gt())`) — a real asymmetry in the
    /// oracle schema, not a port shortcut.
    #[tokio::test]
    async fn top_level_user_config_keys_must_be_identifiers_but_channel_keys_are_unconstrained() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{
                "name":"demo",
                "userConfig":{"my-token":{"type":"string","title":"t","description":"d"}}
            }"#,
        )
        .unwrap();
        assert!(
            load_plugin_from_path(plugin).await.is_none(),
            "a hyphenated top-level userConfig key is not a valid identifier"
        );

        // Channel-level `userConfig` (oracle `Bs`) carries no key-shape
        // constraint (bare `record(i(), gt())`). Parse `RawPluginChannel`
        // directly rather than round-tripping through the full
        // `load_plugin_from_path` -> `validate_plugin_channels` path: that
        // path ALSO requires the channel's `server` to resolve against this
        // plugin's discovered `mcpServers`, which — elsewhere in this same
        // test module (`skip_plugin_mcp_servers_env_suppresses_discovery_for_every_plugin`
        // and neighbors) — is gated by process-global
        // `CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS`/`LINGXI_SKIP_PLUGIN_MCP_SERVERS`
        // env vars those tests mutate without serialization; racing a plugin
        // load with a declared `mcpServers` map against one of them turns an
        // unrelated concurrent test into a spurious failure here. Testing at
        // the `RawPluginChannel` grain exercises exactly the mechanism under
        // test (the key-shape difference) without that hazard.
        let channel: RawPluginChannel = serde_json::from_str(
            r#"{"server":"telegram","userConfig":{"my-token":{"type":"string","title":"t","description":"d"}}}"#,
        )
        .expect(
            "a hyphenated CHANNEL userConfig key is fine — the oracle applies no key-shape \
             constraint there",
        );
        assert!(channel.user_config.unwrap().contains_key("my-token"));
    }

    #[tokio::test]
    async fn root_skill_is_discovered_for_single_skill_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"single-skill"}"#,
        )
        .unwrap();
        fs::write(plugin.join("SKILL.md"), "root skill").unwrap();

        let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap();
        assert_eq!(manifest.components.skills.len(), 1);
        assert_eq!(manifest.components.skills[0].path, plugin.join("SKILL.md"));
    }

    #[tokio::test]
    async fn manifest_preserves_display_name_and_default_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"deployment-tools","displayName":"Deployment Tools","defaultEnabled":false}"#,
        )
        .unwrap();

        let (_, manifest) = load_plugin_from_path(plugin).await.unwrap();
        assert_eq!(manifest.display_name.as_deref(), Some("Deployment Tools"));
        assert!(!manifest.default_enabled);
    }

    #[tokio::test]
    async fn effective_discovery_applies_explicit_state_before_manifest_default() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path();
        let enabled_path = plugins_dir.join("cache/mkt/enabled/1.0.0");
        let disabled_path = plugins_dir.join("cache/mkt/disabled/1.0.0");
        for path in [&enabled_path, &disabled_path] {
            fs::create_dir_all(path.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        }
        fs::write(
            enabled_path
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"enabled"}"#,
        )
        .unwrap();
        fs::write(
            disabled_path
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"disabled","defaultEnabled":false}"#,
        )
        .unwrap();
        fs::write(
            crate::installed::path(plugins_dir),
            serde_json::to_vec(&serde_json::json!({
                "version": 2,
                "plugins": {
                    "enabled@mkt": [{
                        "scope": "user",
                        "installPath": enabled_path,
                        "version": "1.0.0"
                    }],
                    "disabled@mkt": [{
                        "scope": "user",
                        "installPath": disabled_path,
                        "version": "1.0.0"
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let defaults = discover_effective_plugins(plugins_dir, &BTreeMap::new()).await;
        assert_eq!(
            defaults
                .iter()
                .map(|(_, manifest, _)| manifest.name.as_str())
                .collect::<Vec<_>>(),
            vec!["enabled"]
        );

        let overrides = BTreeMap::from([
            ("enabled@mkt".to_string(), false),
            ("disabled@mkt".to_string(), true),
        ]);
        let explicit = discover_effective_plugins(plugins_dir, &overrides).await;
        assert_eq!(
            explicit
                .iter()
                .map(|(_, manifest, _)| manifest.name.as_str())
                .collect::<Vec<_>>(),
            vec!["disabled"]
        );
    }

    #[tokio::test]
    async fn effective_discovery_reads_legacy_filename_and_persists_canonical_v2() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path();
        let cache_path = plugins_dir.join("cache/mkt/weather/1.0.0");
        fs::create_dir_all(cache_path.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            cache_path
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"weather"}"#,
        )
        .unwrap();
        fs::write(
            plugins_dir.join("installed_plugins_v2.json"),
            serde_json::to_vec(&serde_json::json!({
                "plugins": {
                    "mkt": {
                        "weather": {
                            "version": "1.0.0",
                            "installPath": "cache/mkt/weather/1.0.0",
                            "added": "2026-08-31T00:00:00.000Z"
                        }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let discovered = discover_effective_plugins(plugins_dir, &BTreeMap::new()).await;

        assert_eq!(
            discovered
                .iter()
                .map(|(_, manifest, _)| manifest.name.as_str())
                .collect::<Vec<_>>(),
            vec!["weather"]
        );
        assert_eq!(
            serde_json::from_slice::<Value>(
                &fs::read(crate::installed::path(plugins_dir)).unwrap()
            )
            .unwrap(),
            serde_json::json!({
                "version": 2,
                "plugins": {
                    "weather@mkt": [{
                        "scope": "user",
                        "version": "1.0.0",
                        "installPath": "cache/mkt/weather/1.0.0",
                        "installedAt": "2026-08-31T00:00:00.000Z",
                        "lastUpdated": "2026-08-31T00:00:00.000Z"
                    }]
                }
            })
        );
    }

    #[tokio::test]
    async fn declared_skill_directory_may_contain_skill_md_directly() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::create_dir_all(plugin.join("custom-skill")).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"demo","skills":"./custom-skill"}"#,
        )
        .unwrap();
        fs::write(plugin.join("custom-skill/SKILL.md"), "custom skill").unwrap();

        let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap();
        assert_eq!(manifest.components.skills.len(), 1);
        assert_eq!(
            manifest.components.skills[0].path,
            plugin.join("custom-skill/SKILL.md")
        );
    }

    #[tokio::test]
    async fn public_lsp_schema_loads_without_redundant_internal_fields() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(".lsp.json"),
            r#"{
                "typescript": {
                    "command": "typescript-language-server",
                    "args": ["--stdio"],
                    "extensionToLanguage": {".ts": "typescript"},
                    "transport": "socket",
                    "initializationOptions": {"hostInfo": "claude"},
                    "settings": {"typescript": {"format": {"enable": true}}},
                    "workspaceFolder": "./workspace",
                    "startupTimeout": 1234,
                    "shutdownTimeout": 4321,
                    "restartOnCrash": false,
                    "maxRestarts": 7,
                    "diagnostics": false
                }
            }"#,
        )
        .unwrap();

        let configs = load_lsp_servers(tmp.path()).await;
        let cfg = configs.get("typescript").expect("public config loads");
        assert_eq!(cfg.name, "typescript");
        assert_eq!(cfg.transport, "socket");
        assert_eq!(cfg.extension_to_language[".ts"], "typescript");
        assert_eq!(cfg.workspace_folder.as_deref(), Some("./workspace"));
        assert_eq!(cfg.startup_timeout, Some(1234));
        assert_eq!(cfg.shutdown_timeout, Some(4321));
        assert_eq!(cfg.restart_on_crash, Some(false));
        assert_eq!(cfg.max_restarts, Some(7));
        assert_eq!(cfg.diagnostics, Some(false));
    }

    #[tokio::test]
    async fn declared_lsp_array_preserves_path_and_inline_order() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("from-file.json"),
            r#"{"first":{"command":"one","extensionToLanguage":{".a":"a"}}}"#,
        )
        .unwrap();
        let configs = load_declared_lsp_servers(
            tmp.path(),
            Some(serde_json::json!([
                "./from-file.json",
                {"second":{"command":"two","extensionToLanguage":{".b":"b"}}}
            ])),
        )
        .await;
        assert_eq!(
            configs.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }

    #[tokio::test]
    async fn malformed_lsp_server_does_not_hide_valid_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(".lsp.json"),
            r#"{
                "broken": {"args": ["--stdio"], "extensionToLanguage": {".bad": "bad"}},
                "working": {"name": "spoofed", "command": "good-lsp", "extensionToLanguage": {".ok": "ok"}}
            }"#,
        )
        .unwrap();

        let configs = load_lsp_servers(tmp.path()).await;
        assert!(!configs.contains_key("broken"));
        assert!(!configs.contains_key("spoofed"));
        assert_eq!(configs["working"].name, "working");
        assert_eq!(configs["working"].command, "good-lsp");
    }

    #[tokio::test]
    async fn declared_relative_paths_are_confined_and_inline_mcp_lsp_are_loaded() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin.join(branding::PLUGIN_MANIFEST_DIR).join("plugin.json"),
            r#"{
                "name":"demo",
                "commands":"../escape",
                "mcpServers":{"inline":{"type":"stdio","command":"echo"}},
                "lspServers":{"rust":{"command":"rust-analyzer","extensionToLanguage":{".rs":"rust"}}}
            }"#,
        )
        .unwrap();

        let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap();
        assert!(manifest.components.commands.is_empty());
        assert!(manifest.components.mcp_servers.contains_key("inline"));
        assert!(manifest.components.lsp_servers.contains_key("rust"));
    }

    #[tokio::test]
    async fn channels_bind_declared_plugin_mcp_servers_and_preserve_user_config() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{
                "name":"channel-demo",
                "mcpServers":{"telegram":{"type":"stdio","command":"telegram-channel"}},
                "channels":[{
                    "server":"telegram",
                    "userConfig":{
                        "bot_token":{
                            "type":"string",
                            "title":"Bot token",
                            "description":"Telegram token",
                            "sensitive":true
                        }
                    }
                }]
            }"#,
        )
        .unwrap();

        let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap();
        assert_eq!(manifest.channels.len(), 1);
        assert_eq!(manifest.channels[0].server, "telegram");
        let field = manifest.channels[0]
            .user_config
            .as_ref()
            .unwrap()
            .fields
            .get("bot_token")
            .unwrap();
        assert_eq!(field.value_type.as_deref(), Some("string"));
        assert_eq!(field.title.as_deref(), Some("Bot token"));
        assert!(field.sensitive);
    }

    #[tokio::test]
    async fn channels_cannot_bind_undeclared_or_duplicate_mcp_servers() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{
                "name":"channel-demo",
                "mcpServers":{"telegram":{"type":"stdio","command":"telegram-channel"}},
                "channels":[
                    {"server":"telegram"},
                    {"server":"telegram"},
                    {"server":"user-server"}
                ]
            }"#,
        )
        .unwrap();

        let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap();
        assert_eq!(manifest.channels.len(), 1);
        assert_eq!(manifest.channels[0].server, "telegram");
    }

    // ---------- UTF-8 BOM tolerance (§5, 2.1.246 upstream fix) ----------
    //
    // serde_json does not treat U+FEFF as whitespace, so a BOM-prefixed
    // manifest/config file previously failed to deserialize and the whole
    // plugin (or the individual component file) was silently dropped. Each
    // test below prefixes the exact fixture used by an existing non-BOM test
    // above with `\u{feff}` and asserts the same successful outcome.

    #[tokio::test]
    async fn plugin_json_with_utf8_bom_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            "\u{feff}{\"name\":\"demo\"}",
        )
        .unwrap();

        let loaded = load_plugin_from_path(plugin).await;
        assert!(
            loaded.is_some(),
            "BOM-prefixed plugin.json must still deserialize instead of being skipped as malformed"
        );
        assert_eq!(loaded.unwrap().1.name, "demo");
    }

    #[tokio::test]
    async fn root_mcp_json_with_utf8_bom_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::write(
            plugin.join(".mcp.json"),
            "\u{feff}{\"echo\":{\"type\":\"stdio\",\"command\":\"echo\"}}",
        )
        .unwrap();

        let servers = load_mcp_servers(plugin).await;
        assert!(
            servers.contains_key("echo"),
            "BOM-prefixed root .mcp.json must still parse its server map, got {servers:?}"
        );
    }

    #[tokio::test]
    async fn root_lsp_json_with_utf8_bom_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::write(
            plugin.join(".lsp.json"),
            "\u{feff}{\"rust\":{\"name\":\"rust\",\"command\":\"rust-analyzer\",\"args\":[],\"env\":{},\"trigger_languages\":[\"rust\"],\"root_dir_markers\":[\"Cargo.toml\"],\"initialization_options\":null,\"extensionToLanguage\":{\".rs\":\"rust\"}}}",
        )
        .unwrap();

        let servers = load_lsp_servers(plugin).await;
        assert!(
            servers.contains_key("rust"),
            "BOM-prefixed root .lsp.json must still parse its server map, got {servers:?}"
        );
    }

    #[tokio::test]
    async fn standard_hooks_json_with_utf8_bom_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join("hooks")).unwrap();
        fs::write(
            plugin.join("hooks").join("hooks.json"),
            "\u{feff}{\"hooks\":{\"PreToolUse\":[{\"matcher\":\"Write\",\"hooks\":[{\"type\":\"command\",\"command\":\"./fmt.sh\"}]}]}}",
        )
        .unwrap();

        let hooks = load_standard_hooks(plugin).await;
        assert_eq!(
            hooks.len(),
            1,
            "BOM-prefixed hooks/hooks.json must still parse instead of yielding zero hooks"
        );
    }

    #[tokio::test]
    async fn declared_hooks_path_with_utf8_bom_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::write(
            plugin.join("custom-hooks.json"),
            "\u{feff}{\"hooks\":{\"PreToolUse\":[{\"matcher\":\"Write\",\"hooks\":[{\"type\":\"command\",\"command\":\"./fmt.sh\"}]}]}}",
        )
        .unwrap();

        let hooks = load_declared_hooks_from_path(plugin, "./custom-hooks.json").await;
        assert_eq!(
            hooks.len(),
            1,
            "BOM-prefixed manifest-declared hooks file must still parse instead of yielding zero hooks"
        );
    }

    #[tokio::test]
    async fn declared_mcp_servers_path_with_utf8_bom_still_loads() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::write(
            plugin.join("custom-mcp.json"),
            "\u{feff}{\"echo\":{\"type\":\"stdio\",\"command\":\"echo\"}}",
        )
        .unwrap();

        let servers = load_declared_mcp_servers(
            plugin,
            Some(Value::String("./custom-mcp.json".to_string())),
            "demo",
            false,
        )
        .await;
        assert!(
            servers.contains_key("echo"),
            "BOM-prefixed manifest-declared mcpServers file must still parse, got {servers:?}"
        );
    }

    // ---------- §6: object-form `commands` map + mixed path/inline arrays ----------
    //
    // Oracle `Rs`: `commands: union([path, path[], record(string, Ds)])`. The
    // port's `PathDecl` was `untagged {One(String), Many(Vec<String>)}` with no
    // object-map arm, so ANY object-form `commands` value failed to
    // deserialize `RawManifest` as a whole and `load_plugin_from_path` silently
    // dropped the entire plugin, not just its `commands`. The fix forks a
    // `commands`-only `CommandsDecl` carrying the object-map arm (`Ds`), while
    // `skills`/`agents`/`outputStyles` keep the narrower `PathDecl` (the oracle
    // does not offer a record form for those).

    #[tokio::test]
    async fn commands_object_map_resolves_source_entries_without_dropping_the_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::create_dir_all(plugin.join("cmds")).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{
                "name":"demo",
                "commands":{
                    "about":{"source":"./cmds/about.md","description":"About this plugin"},
                    "inline-only":{"content":"Inline body, no file"}
                }
            }"#,
        )
        .unwrap();
        fs::write(plugin.join("cmds/about.md"), "About body").unwrap();

        let loaded = load_plugin_from_path(plugin).await;
        assert!(
            loaded.is_some(),
            "an object-form `commands` map with valid Ds entries must not delete the whole plugin (the §6 defect)"
        );
        let (_id, manifest) = loaded.unwrap();
        let commands: Vec<_> = manifest
            .components
            .commands
            .iter()
            .map(|c| c.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(
            commands,
            vec!["about.md"],
            "the `source`-backed entry must resolve to its markdown file; the \
             `content`-only entry has no on-disk file to materialize and is \
             skipped rather than fabricated, got {commands:?}"
        );
    }

    #[tokio::test]
    async fn commands_object_map_entry_with_both_source_and_content_drops_whole_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::create_dir_all(plugin.join("cmds")).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{
                "name":"demo",
                "commands":{"bad":{"source":"./cmds/about.md","content":"Also inline"}}
            }"#,
        )
        .unwrap();
        fs::write(plugin.join("cmds/about.md"), "About body").unwrap();

        let loaded = load_plugin_from_path(plugin).await;
        assert!(
            loaded.is_none(),
            "a command entry with BOTH source and content violates the oracle's \
             refine (\"Command must have either source or content, but not both\") \
             and must fail manifest parsing like any other malformed plugin.json"
        );
    }

    #[tokio::test]
    async fn commands_object_map_entry_with_neither_source_nor_content_drops_whole_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{
                "name":"demo",
                "commands":{"bad":{"description":"missing both source and content"}}
            }"#,
        )
        .unwrap();

        let loaded = load_plugin_from_path(plugin).await;
        assert!(
            loaded.is_none(),
            "a command entry with NEITHER source nor content violates the oracle's \
             refine and must fail manifest parsing, not silently load with an empty command"
        );
    }

    #[tokio::test]
    async fn declared_mcp_servers_mixed_array_merges_path_and_inline_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::write(
            plugin.join("extra-mcp.json"),
            r#"{"file-server":{"type":"stdio","command":"echo"}}"#,
        )
        .unwrap();

        let value = serde_json::json!([
            "./extra-mcp.json",
            {"inline-server": {"type": "stdio", "command": "echo"}}
        ]);

        let servers = load_declared_mcp_servers(plugin, Some(value), "demo", false).await;
        assert!(
            servers.contains_key("file-server") && servers.contains_key("inline-server"),
            "a mixed [path, inlineObject] array (oracle `js`) must merge BOTH the \
             file-loaded and inline server entries, got {servers:?}"
        );
    }

    /// The PLUGIN layer must keep using `parse_plugin_mcp_json_string`, not the
    /// config-layer `parse_mcp_json_string`.
    ///
    /// Oracle has two schema layers wearing the same filename: the config table
    /// `ZGn` (7 keys, no IDE transports) that `.mcp.json` / settings /
    /// `--mcp-config` go through, and the 8-arm union `KY` (`vve`:
    /// `let B=KY().safeParse(U)`) that plugin-declared servers go through — which
    /// DOES have `sse-ide` and `ws-ide` arms.
    ///
    /// This is a MERGE-RESOLUTION pin. The layer split and the MCPB dispatch
    /// rewrite of `load_declared_mcp_servers` were written in two separate
    /// branches; integrating them re-introduced the config-layer parser at three
    /// call sites here. Without this test, swapping them back leaves every other
    /// test green, so nothing would catch the regression.
    ///
    /// Covers all three declared-value shapes that reach a parser:
    /// bare object, array item object, and a `./path`-loaded file.
    #[tokio::test]
    async fn declared_mcp_servers_keep_the_plugin_schema_layer_for_ide_transports() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::write(
            plugin.join("ide-mcp.json"),
            r#"{"from-path":{"type":"sse-ide","url":"http://127.0.0.1:1/sse","ideName":"vscode"}}"#,
        )
        .unwrap();

        // 1. bare inline object
        let bare = serde_json::json!({
            "from-bare": {"type": "sse-ide", "url": "http://127.0.0.1:1/sse", "ideName": "vscode"}
        });
        let servers = load_declared_mcp_servers(plugin, Some(bare), "demo", false).await;
        assert!(
            servers.contains_key("from-bare"),
            "`sse-ide` is an arm of the plugin union `KY`; a bare inline declared entry must be \
             kept, not dropped by the config-layer table `ZGn`, got {servers:?}"
        );

        // 2. array: a ./path entry and an inline object entry
        let mixed = serde_json::json!([
            "./ide-mcp.json",
            {"from-array": {"type": "ws-ide", "url": "ws://127.0.0.1:1", "ideName": "vscode"}}
        ]);
        let servers = load_declared_mcp_servers(plugin, Some(mixed), "demo", false).await;
        assert!(
            servers.contains_key("from-path") && servers.contains_key("from-array"),
            "both the ./path-loaded `sse-ide` and the inline `ws-ide` array item must survive \
             the plugin layer, got {servers:?}"
        );
    }

    #[tokio::test]
    async fn declared_lsp_servers_mixed_array_merges_path_and_inline_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::write(
            plugin.join("extra-lsp.json"),
            r#"{"rust":{"name":"rust","command":"rust-analyzer","args":[],"env":{},"trigger_languages":["rust"],"root_dir_markers":["Cargo.toml"],"initialization_options":null,"extensionToLanguage":{".rs":"rust"}}}"#,
        )
        .unwrap();

        let value = serde_json::json!([
            "./extra-lsp.json",
            {"python": {"name": "python", "command": "pyright", "args": [], "env": {}, "trigger_languages": ["python"], "root_dir_markers": ["pyproject.toml"], "initialization_options": null, "extensionToLanguage": {".py": "python"}}}
        ]);

        let servers = load_declared_lsp_servers(plugin, Some(value)).await;
        assert!(
            servers.contains_key("rust") && servers.contains_key("python"),
            "a mixed [path, inlineObject] array (oracle `Ws`) must merge BOTH the \
             file-loaded and inline lsp entries, got {servers:?}"
        );
    }

    #[tokio::test]
    async fn declared_hooks_mixed_array_merges_path_and_inline_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::write(
            plugin.join("extra-hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Write","hooks":[{"type":"command","command":"./fmt.sh"}]}]}}"#,
        )
        .unwrap();

        let value = serde_json::json!([
            "./extra-hooks.json",
            {"PostToolUse": [{"matcher": "Read", "hooks": [{"type": "command", "command": "./log.sh"}]}]}
        ]);

        let hooks = load_declared_hooks(plugin, Some(value)).await;
        assert_eq!(
            hooks.len(),
            2,
            "a mixed [path, inlineObject] array (oracle `zs`) must merge BOTH the \
             file-loaded and inline hook entries, got {hooks:?}"
        );
    }

    // ---------- §3 + §4: plugin MCP-discovery suppression ----------

    /// A plugin whose `.mcp.json`, manifest `mcpServers`, and a default
    /// `commands/` dir are all present, so a test can assert the MCP slot was
    /// suppressed while every other component slot still loaded.
    fn write_plugin_with_mcp_and_command(plugin: &Path, name: &str) {
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::create_dir_all(plugin.join("commands")).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            format!(
                r#"{{
                    "name":"{name}",
                    "mcpServers":{{"declared":{{"type":"stdio","command":"echo"}}}}
                }}"#
            ),
        )
        .unwrap();
        fs::write(plugin.join("commands/hello.md"), "hello").unwrap();
        fs::write(
            plugin.join(".mcp.json"),
            r#"{"root":{"type":"stdio","command":"echo"}}"#,
        )
        .unwrap();
    }

    /// Serializes the `_env`-mutating tests in this block and resets the four
    /// env vars ([`resolve_skip_mcp_discovery`]'s two names, each with its
    /// `LINGXI_`/`CLAUDE_CODE_` alias) to "unset" at entry. Mirrors
    /// `tools/ui/src/brief.rs`'s `brief_guard` / `push_notification.rs`'s
    /// `guard`.
    #[tokio::test]
    async fn sdk_skip_mcp_discovery_suppresses_mcp_but_not_other_components() {
        let _g = super::skip_mcp_test_env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        write_plugin_with_mcp_and_command(plugin, "demo");

        // No env suppression active — the SDK-host `skipMcpDiscovery:true`
        // request alone must suppress BOTH `.mcp.json` and manifest
        // `mcpServers`, while `commands/` still auto-detects (oracle: "leaves
        // other components enabled").
        let (_id, manifest) = load_plugin_from_path_with_mcp_gate(plugin, true, None)
            .await
            .unwrap();
        assert!(
            manifest.components.mcp_servers.is_empty(),
            "skipMcpDiscovery must drop both .mcp.json and declared mcpServers, got {:?}",
            manifest.components.mcp_servers
        );
        assert!(manifest.components.skip_mcp_discovery);
        assert_eq!(
            manifest.components.commands.len(),
            1,
            "non-MCP components must still load when only MCP discovery is skipped"
        );

        // REVERT proof: the same fixture with `sdk_skip_mcp_discovery: false`
        // (the plain wrapper every pre-existing call site uses) must load
        // BOTH servers — showing the assertions above are actually exercising
        // the gate, not tautologically true for this fixture.
        let (_id2, unskipped) = load_plugin_from_path(plugin).await.unwrap();
        assert_eq!(unskipped.components.mcp_servers.len(), 2);
        assert!(!unskipped.components.skip_mcp_discovery);
    }

    #[tokio::test]
    async fn skip_plugin_mcp_servers_env_suppresses_discovery_for_every_plugin() {
        let _g = super::skip_mcp_test_env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        write_plugin_with_mcp_and_command(plugin, "demo");

        // Baseline: no env set, no install-source id — normal discovery.
        let (_id, baseline) = load_plugin_from_path(plugin).await.unwrap();
        assert_eq!(baseline.components.mcp_servers.len(), 2);

        // `CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS` set to ANY non-empty value
        // (raw JS truthiness, not `isEnvTruthy`'s enumerated set) suppresses
        // MCP discovery even though nothing requested `skipMcpDiscovery` and
        // no `_EXCEPT` was given.
        std::env::set_var("CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS", "0");
        let (_id, suppressed) = load_plugin_from_path(plugin).await.unwrap();
        assert!(
            suppressed.components.mcp_servers.is_empty(),
            "CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS=\"0\" is a non-empty string and must \
             still suppress discovery (plain process.env truthiness)"
        );
        assert!(suppressed.components.skip_mcp_discovery);

        // Unsetting restores discovery — the env var, not something else
        // about the fixture, is what suppressed it above.
        std::env::remove_var("CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS");
        let (_id, restored) = load_plugin_from_path(plugin).await.unwrap();
        assert_eq!(restored.components.mcp_servers.len(), 2);

        // The `LINGXI_` alias is honored too.
        std::env::set_var("LINGXI_SKIP_PLUGIN_MCP_SERVERS", "1");
        let (_id, suppressed_alias) = load_plugin_from_path(plugin).await.unwrap();
        assert!(suppressed_alias.components.mcp_servers.is_empty());
    }

    #[tokio::test]
    async fn except_with_at_sign_matches_install_source_id_directory_loaded_or_not() {
        let _g = super::skip_mcp_test_env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        write_plugin_with_mcp_and_command(plugin, "demo");
        std::env::set_var("CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS", "1");
        std::env::set_var(
            "CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS_EXCEPT",
            "other@mkt, demo@marketplace-x",
        );

        // An "@"-containing entry matches the plugin's `name@marketplace`
        // install-source identity and re-admits it past the suppression.
        let (_id, exempted) =
            load_plugin_from_path_with_mcp_gate(plugin, false, Some("demo@marketplace-x"))
                .await
                .unwrap();
        assert_eq!(
            exempted.components.mcp_servers.len(),
            2,
            "an _EXCEPT entry naming this plugin's install-source id must re-admit it"
        );
        assert!(!exempted.components.skip_mcp_discovery);

        // A non-matching install-source id stays suppressed.
        let (_id, still_suppressed) =
            load_plugin_from_path_with_mcp_gate(plugin, false, Some("demo@some-other-marketplace"))
                .await
                .unwrap();
        assert!(still_suppressed.components.mcp_servers.is_empty());
    }

    /// Oracle `SCn`: `r=!pM(e)` and a bare-name entry matches when
    /// `r && qy(u, e.name)`. `pM(e)` is
    /// `e.scope==="project" && e.source.endsWith("@skills-dir")` — TRUE only
    /// for a plugin auto-loaded from `.claude/skills/`. A `--plugin-dir`
    /// session plugin is stamped `<name>@inline` and an installed plugin
    /// `<name>@<marketplace>`, so `r` is TRUE for both and a bare name
    /// re-admits them whether or not this port resolved an install-source id.
    #[tokio::test]
    async fn except_bare_name_exempts_a_plugin_with_or_without_an_install_source_id() {
        let _g = super::skip_mcp_test_env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        write_plugin_with_mcp_and_command(plugin, "demo");
        std::env::set_var("CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS", "1");
        std::env::set_var("CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS_EXCEPT", "demo");

        let (_id, exempted) =
            load_plugin_from_path_with_mcp_gate(plugin, false, Some("demo@marketplace-x"))
                .await
                .unwrap();
        assert_eq!(exempted.components.mcp_servers.len(), 2);

        // An ad-hoc directory load (`--plugin-dir`) has no install-source id
        // here, but its oracle source is `demo@inline`, NOT `demo@skills-dir`
        // — so `pM` is false and the bare name re-admits it too.
        let (_id, also_exempted) = load_plugin_from_path_with_mcp_gate(plugin, false, None)
            .await
            .unwrap();
        assert_eq!(
            also_exempted.components.mcp_servers.len(),
            2,
            "a bare-name _EXCEPT entry must exempt a `--plugin-dir` plugin too \
             (oracle `pM` is false for `<name>@inline`), got {:?}",
            also_exempted.components.mcp_servers
        );

        // Guard against a tautology: a NON-matching bare name leaves the
        // same fixture suppressed on both paths.
        std::env::set_var("CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS_EXCEPT", "someone-else");
        let (_id, suppressed) = load_plugin_from_path_with_mcp_gate(plugin, false, None)
            .await
            .unwrap();
        assert!(suppressed.components.mcp_servers.is_empty());
    }

    // ---------- §14 row 2: hooks.json `modules` ----------

    #[tokio::test]
    async fn hooks_json_with_only_modules_and_no_hooks_is_accepted_but_registers_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join("hooks")).unwrap();
        fs::write(
            plugin.join("hooks/hooks.json"),
            r#"{"modules": ["./register.js"]}"#,
        )
        .unwrap();
        let hooks = load_standard_hooks(plugin).await;
        assert!(
            hooks.is_empty(),
            "a modules-only hooks.json has no declarative hooks to register (the module \
             itself is not executed by this engine), got {hooks:?}"
        );
    }

    #[tokio::test]
    async fn hooks_json_with_two_modules_entries_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join("hooks")).unwrap();
        fs::write(
            plugin.join("hooks/hooks.json"),
            r#"{"modules": ["./a.js", "./b.js"]}"#,
        )
        .unwrap();
        assert!(
            load_standard_hooks(plugin).await.is_empty(),
            "oracle: \"a second entry is refused\" — a 2-entry `modules` array must not load"
        );
    }

    #[tokio::test]
    async fn hooks_json_with_neither_hooks_nor_modules_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join("hooks")).unwrap();
        fs::write(
            plugin.join("hooks/hooks.json"),
            r#"{"description": "empty"}"#,
        )
        .unwrap();
        assert!(
            load_standard_hooks(plugin).await.is_empty(),
            "oracle: hooks.json must have `hooks` or `modules`, or both"
        );
    }

    #[tokio::test]
    async fn hooks_json_with_too_many_modules_refuses_even_the_valid_declarative_half() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join("hooks")).unwrap();
        fs::write(
            plugin.join("hooks/hooks.json"),
            r#"{
                "modules": ["./a.js", "./b.js"],
                "hooks": {"PreToolUse":[{"matcher":"Write","hooks":[{"type":"command","command":"./fmt.sh"}]}]}
            }"#,
        )
        .unwrap();
        assert!(
            load_standard_hooks(plugin).await.is_empty(),
            "a 2-entry `modules` array must refuse the WHOLE hooks.json, even a valid \
             declarative `hooks` half — an unvalidated port would keep parsing `hooks` \
             and ignore the modules-count violation entirely"
        );
    }

    #[tokio::test]
    async fn hooks_json_with_modules_and_hooks_registers_the_declarative_half() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join("hooks")).unwrap();
        fs::write(
            plugin.join("hooks/hooks.json"),
            r#"{
                "modules": ["./register.js"],
                "hooks": {"PreToolUse":[{"matcher":"Write","hooks":[{"type":"command","command":"./fmt.sh"}]}]}
            }"#,
        )
        .unwrap();
        let hooks = load_standard_hooks(plugin).await;
        assert_eq!(
            hooks.len(),
            1,
            "both halves declared: the declarative `hooks` still registers, got {hooks:?}"
        );
    }

    // ---------- §14 row 3: `experimental.syntaxHighlighting.hljsLanguages` ----------

    fn write_manifest(plugin: &Path, manifest: &serde_json::Value) {
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            manifest.to_string(),
        )
        .unwrap();
    }

    /// The oracle's `PluginManifestSchema` (`jpe`) composes
    /// `Cs,zs,Rs,Us,Es,ct,pt,Ls,Bs,js,Ws,mt,Ys,Fs,qs,Vs` — `Hs`
    /// (`syntaxHighlighting`) is NOT among them, and `Hs` is referenced
    /// exactly once in the binary, inside `Vs` = `experimental`. `jpe` is a
    /// plain `f()` (z.object, no catchall), so a TOP-LEVEL
    /// `syntaxHighlighting` is silently STRIPPED: the plugin still loads and
    /// contributes no hljs languages.
    #[tokio::test]
    async fn top_level_syntax_highlighting_is_stripped_not_read() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        write_manifest(
            plugin,
            &serde_json::json!({
                "name": "demo",
                "syntaxHighlighting": {
                    "hljsLanguages": [{"id": "mylang", "remote": "npm:hljs-mylang@1.2.3"}]
                }
            }),
        );
        let (_id, manifest) = load_plugin_from_path(plugin)
            .await
            .expect("a top-level syntaxHighlighting key must not sink the manifest");
        assert!(
            manifest.components.hljs_languages.is_empty(),
            "a top-level syntaxHighlighting key is not in `jpe` and must be stripped, got {:?}",
            manifest.components.hljs_languages
        );
    }

    /// The §0(b) schema-layer trap: because the top-level key is stripped
    /// BEFORE any shape check, a manifest claude-code loads fine must not
    /// make the whole plugin vanish here. Each of the three shapes that the
    /// `experimental` arm does reject is exercised at top level and must load.
    #[tokio::test]
    async fn top_level_syntax_highlighting_shape_violations_do_not_sink_the_manifest() {
        let entries: Vec<_> = (0..17)
            .map(|i| serde_json::json!({"id": format!("lang{i}")}))
            .collect();
        for (label, bad) in [
            (
                "invalid id",
                serde_json::json!({"hljsLanguages": [{"id": "Not-Valid"}]}),
            ),
            (
                "unknown key",
                serde_json::json!({"hljsLanguages": [], "extra": true}),
            ),
            ("17 entries", serde_json::json!({"hljsLanguages": entries})),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let plugin = tmp.path();
            write_manifest(
                plugin,
                &serde_json::json!({"name": "demo", "syntaxHighlighting": bad}),
            );
            let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap_or_else(|| {
                panic!("a top-level syntaxHighlighting with {label} must still load the plugin")
            });
            assert!(manifest.components.hljs_languages.is_empty());
        }
    }

    /// `Vs` = `f({experimental: Sa(…, f({…pt, …Hs, …mt, …ct, evals}).passthrough()…)})`:
    /// `syntaxHighlighting` is read ONLY out of `experimental` (`Hs` is not in
    /// `jpe`). ⚠️ That is NOT true of every key inside `experimental` — see
    /// [`experimental_themes_outrank_the_top_level_themes_key`] for `themes`,
    /// which the oracle reads from BOTH layers with `experimental` winning.
    #[tokio::test]
    async fn experimental_syntax_highlighting_valid_entry_is_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        write_manifest(
            plugin,
            &serde_json::json!({
                "name": "demo",
                "experimental": {
                    "syntaxHighlighting": {
                        "hljsLanguages": [
                            {"id": "mylang", "remote": "npm:hljs-mylang@1.2.3", "integrity": "sha256-abc123=="}
                        ]
                    }
                }
            }),
        );
        let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap();
        assert_eq!(manifest.components.hljs_languages.len(), 1);
        assert_eq!(manifest.components.hljs_languages[0].id, "mylang");
        assert_eq!(
            manifest.components.hljs_languages[0].remote.as_deref(),
            Some("npm:hljs-mylang@1.2.3")
        );
    }

    /// Inside `experimental`, `Hs` is still `.strict()` at both levels, so a
    /// shape violation there DOES fail the whole `plugin.json`.
    #[tokio::test]
    async fn experimental_syntax_highlighting_shape_violations_sink_the_whole_manifest() {
        let entries: Vec<_> = (0..17)
            .map(|i| serde_json::json!({"id": format!("lang{i}")}))
            .collect();
        for (label, bad) in [
            (
                "an id violating ^[a-z][a-z0-9_-]*$",
                serde_json::json!({"hljsLanguages": [{"id": "Not-Valid"}]}),
            ),
            (
                "an unknown sibling key (`.strict()`)",
                serde_json::json!({"hljsLanguages": [], "extra": true}),
            ),
            (
                "17 entries (cap is 16)",
                serde_json::json!({"hljsLanguages": entries}),
            ),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let plugin = tmp.path();
            write_manifest(
                plugin,
                &serde_json::json!({
                    "name": "demo",
                    "experimental": {"syntaxHighlighting": bad}
                }),
            );
            assert!(
                load_plugin_from_path(plugin).await.is_none(),
                "{label} under experimental.syntaxHighlighting must sink the whole plugin.json parse"
            );
        }
    }

    /// `Vs`'s preprocess `Sa((e)=>He(e)?e:void 0,…)` drops a NON-OBJECT
    /// `experimental` to `undefined` instead of rejecting it, and the inner
    /// object is `.passthrough()`, so an unknown key inside `experimental`
    /// is kept rather than failing.
    #[tokio::test]
    async fn non_object_experimental_is_dropped_and_unknown_experimental_keys_pass_through() {
        for value in [
            serde_json::json!("nope"),
            serde_json::json!([1, 2, 3]),
            serde_json::json!(7),
            serde_json::json!({"somethingElse": {"whatever": true}}),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let plugin = tmp.path();
            write_manifest(
                plugin,
                &serde_json::json!({"name": "demo", "experimental": value}),
            );
            let (_id, manifest) = load_plugin_from_path(plugin)
                .await
                .unwrap_or_else(|| panic!("experimental = {value} must not sink the manifest"));
            assert!(manifest.components.hljs_languages.is_empty());
        }
    }

    // ---------- §14 row 4: `binaries` parsing + validation ----------

    #[tokio::test]
    async fn binaries_valid_entry_is_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        let sha = "a".repeat(64);
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            serde_json::json!({
                "name": "demo",
                "binaries": {"mytool-x86_64-linux": {"sha256": sha}}
            })
            .to_string(),
        )
        .unwrap();
        let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap();
        assert_eq!(manifest.components.binaries.len(), 1);
        assert_eq!(
            manifest.components.binaries["mytool-x86_64-linux"].sha256,
            "a".repeat(64)
        );
    }

    #[tokio::test]
    async fn binaries_invalid_entries_are_silently_dropped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            serde_json::json!({
                "name": "demo",
                "binaries": {
                    "Bad-Basename!": {"sha256": "a".repeat(64)},
                    "good-tool": {"sha256": "not-a-valid-hash"},
                    "good-tool2": {"sha256": "b".repeat(64)}
                }
            })
            .to_string(),
        )
        .unwrap();
        let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap();
        assert_eq!(
            manifest.components.binaries.len(),
            1,
            "only the one fully-valid entry should survive, got {:?}",
            manifest.components.binaries
        );
        assert!(manifest.components.binaries.contains_key("good-tool2"));
    }

    #[tokio::test]
    async fn binaries_caps_at_64_valid_entries_in_source_order() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        let mut map = serde_json::Map::new();
        for i in 0..70 {
            map.insert(
                format!("tool{i:02}"),
                serde_json::json!({"sha256": "c".repeat(64)}),
            );
        }
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            serde_json::json!({"name": "demo", "binaries": Value::Object(map)}).to_string(),
        )
        .unwrap();
        let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap();
        assert_eq!(manifest.components.binaries.len(), 64);
        assert!(manifest.components.binaries.contains_key("tool00"));
        assert!(
            !manifest.components.binaries.contains_key("tool69"),
            "only the first 64 (in source order) survive the oracle's silent cap"
        );
    }

    // ---------- §14 row 5: `monitors` parsing + validation ----------

    #[tokio::test]
    async fn monitors_inline_array_is_parsed_with_default_when() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            serde_json::json!({
                "name": "demo",
                "monitors": [{"name": "watch-log", "command": "tail -f log", "description": "watch it"}]
            })
            .to_string(),
        )
        .unwrap();
        let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap();
        assert_eq!(manifest.components.monitors.len(), 1);
        assert_eq!(manifest.components.monitors[0].when, MonitorTrigger::Always);
    }

    #[tokio::test]
    async fn monitors_duplicate_names_sink_the_whole_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            serde_json::json!({
                "name": "demo",
                "monitors": [
                    {"name": "dup", "command": "a", "description": "a"},
                    {"name": "dup", "command": "b", "description": "b"}
                ]
            })
            .to_string(),
        )
        .unwrap();
        assert!(
            load_plugin_from_path(plugin).await.is_none(),
            "oracle: \"Monitor names must be unique within a plugin\" must sink the whole manifest"
        );
    }

    #[tokio::test]
    async fn monitors_on_skill_invoke_trigger_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            serde_json::json!({
                "name": "demo",
                "monitors": [{"name": "m", "command": "c", "description": "d", "when": "on-skill-invoke:deploy"}]
            })
            .to_string(),
        )
        .unwrap();
        let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap();
        assert_eq!(
            manifest.components.monitors[0].when,
            MonitorTrigger::OnSkillInvoke("deploy".to_string())
        );
    }

    #[tokio::test]
    async fn monitors_default_auto_scan_loads_when_field_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::create_dir_all(plugin.join("monitors")).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            serde_json::json!({"name": "demo"}).to_string(),
        )
        .unwrap();
        fs::write(
            plugin.join("monitors/monitors.json"),
            serde_json::json!([{"name": "auto", "command": "c", "description": "d"}]).to_string(),
        )
        .unwrap();
        let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap();
        assert_eq!(manifest.components.monitors.len(), 1);
        assert_eq!(manifest.components.monitors[0].name, "auto");
    }

    #[tokio::test]
    async fn monitors_declared_field_replaces_the_default_auto_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::create_dir_all(plugin.join("monitors")).unwrap();
        fs::write(
            plugin.join("monitors/monitors.json"),
            serde_json::json!([{"name": "auto", "command": "c", "description": "d"}]).to_string(),
        )
        .unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            serde_json::json!({
                "name": "demo",
                "monitors": [{"name": "declared", "command": "x", "description": "y"}]
            })
            .to_string(),
        )
        .unwrap();
        let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap();
        assert_eq!(manifest.components.monitors.len(), 1);
        assert_eq!(
            manifest.components.monitors[0].name, "declared",
            "a declared `monitors` field REPLACES the auto-scan, same rule as themes/workflows"
        );
    }

    // ---------- §14 row 1: `mcpServers` MCPB/`.dxt` union arm ----------

    fn build_mcpb_zip(manifest_json: &serde_json::Value) -> Vec<u8> {
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            w.start_file("manifest.json", zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(manifest_json.to_string().as_bytes()).unwrap();
            w.finish().unwrap();
        }
        buf
    }

    #[tokio::test]
    async fn mcp_servers_mcpb_path_resolves_to_a_named_server_for_a_non_confined_plugin() {
        // Every MCPB test asserts on the CONTENTS of `components.mcp_servers`,
        // which `CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS` empties process-wide.
        // The env tests set that var while other tests run in parallel, so
        // take the same serializing guard here — otherwise these assertions
        // are scheduling-dependent.
        let _g = super::skip_mcp_test_env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            serde_json::json!({"name": "demo", "mcpServers": "./bundle.mcpb"}).to_string(),
        )
        .unwrap();
        let bundle = build_mcpb_zip(&serde_json::json!({
            "name": "bundled-server",
            "server": {"mcp_config": {"command": "node", "args": ["${__dirname}/index.js"]}}
        }));
        fs::write(plugin.join("bundle.mcpb"), bundle).unwrap();

        // A resolved install-source identity marks this "not directory-loaded"
        // (oracle `pM(e)` false) — the MCPB source is actually resolved.
        let (_id, manifest) =
            load_plugin_from_path_with_mcp_gate(plugin, false, Some("demo@marketplace-x"))
                .await
                .unwrap();
        assert_eq!(manifest.components.mcp_servers.len(), 1);
        let server = manifest
            .components
            .mcp_servers
            .get("bundled-server")
            .unwrap_or_else(|| {
                panic!(
                    "server must be keyed by the MCPB manifest's own name, got {:?}",
                    manifest.components.mcp_servers
                )
            });
        match &server.spec {
            lingxi_core::host::McpTransportSpec::Stdio { command, args, .. } => {
                assert_eq!(command, "node");
                assert!(
                    args[0].ends_with("/index.js") && args[0].contains(plugin.to_str().unwrap()),
                    "${{__dirname}} must substitute the extracted bundle path, got {args:?}"
                );
            }
            other => panic!("expected a stdio spec, got {other:?}"),
        }
    }

    /// Oracle `pM(e)` is `e.scope==="project" && e.source.endsWith("@skills-dir")`,
    /// so a `--plugin-dir` plugin (source `<name>@inline`) is NOT confined:
    /// `OL`'s `x(F)` closure returns false and `Pit` resolves the bundle.
    /// This port has no `.claude/skills/` plugin auto-loader, so nothing it
    /// loads is confined — an ad-hoc directory load with no install-source
    /// id must still resolve its MCPB source.
    #[tokio::test]
    async fn mcp_servers_mcpb_source_resolves_for_a_plugin_dir_load_too() {
        let _g = super::skip_mcp_test_env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            serde_json::json!({"name": "demo", "mcpServers": "./bundle.mcpb"}).to_string(),
        )
        .unwrap();
        let bundle = build_mcpb_zip(&serde_json::json!({
            "name": "bundled-server",
            "server": {"mcp_config": {"command": "node", "args": []}}
        }));
        fs::write(plugin.join("bundle.mcpb"), bundle).unwrap();

        let (_id, manifest) = load_plugin_from_path(plugin).await.unwrap();
        assert!(
            manifest
                .components
                .mcp_servers
                .contains_key("bundled-server"),
            "a `--plugin-dir` plugin is not `pM(e)`, so its MCPB source must resolve, got {:?}",
            manifest.components.mcp_servers
        );
    }

    /// Oracle `uct`: `W=sha256(F).substring(0,16)` and the extract dir is
    /// `tE(x,W)` = `<plugin>/.mcpb-cache/<16 hex>`. That directory IS the
    /// `extensionPath` `${__dirname}` expands to, so a 64-hex path diverges
    /// byte-for-byte from claude-code's.
    #[tokio::test]
    async fn mcpb_extract_dir_is_keyed_on_the_16_hex_short_hash() {
        let _g = super::skip_mcp_test_env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            serde_json::json!({"name": "demo", "mcpServers": "./bundle.mcpb"}).to_string(),
        )
        .unwrap();
        let bundle = build_mcpb_zip(&serde_json::json!({
            "name": "bundled-server",
            "server": {"mcp_config": {"command": "node", "args": ["${__dirname}/index.js"]}}
        }));
        fs::write(plugin.join("bundle.mcpb"), &bundle).unwrap();
        let expected: String = crate::mcpb::sha256_hex(&bundle).chars().take(16).collect();

        let (_id, manifest) =
            load_plugin_from_path_with_mcp_gate(plugin, false, Some("demo@marketplace-x"))
                .await
                .unwrap();
        let server = manifest
            .components
            .mcp_servers
            .get("bundled-server")
            .unwrap();
        let lingxi_core::host::McpTransportSpec::Stdio { args, .. } = &server.spec else {
            panic!("expected a stdio spec, got {:?}", server.spec)
        };
        assert_eq!(
            args[0],
            plugin
                .join(".mcpb-cache")
                .join(&expected)
                .join("index.js")
                .display()
                .to_string(),
            "${{__dirname}} must be `<plugin>/.mcpb-cache/<16-hex>`, not the full 64-hex digest"
        );
        assert!(
            plugin.join(".mcpb-cache").join(&expected).is_dir(),
            "the on-disk cache layout must use the 16-hex short hash"
        );
    }

    /// Oracle `uct` writes its cache record (`Zpe`) only AFTER a successful
    /// extraction and gates the fast path on that METADATA (`rje` plus the
    /// `k4t` freshness check), never on bare directory existence — so an
    /// extraction that threw is simply retried next session. A gate that keys
    /// off the directory the port itself created just before unpacking caches
    /// the FAILURE forever.
    #[tokio::test]
    async fn a_stale_cache_dir_does_not_suppress_re_extraction() {
        let _g = super::skip_mcp_test_env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            serde_json::json!({"name": "demo", "mcpServers": "./bundle.mcpb"}).to_string(),
        )
        .unwrap();
        let bundle = build_mcpb_zip(&serde_json::json!({
            "name": "bundled-server",
            "server": {"mcp_config": {"command": "node", "args": []}}
        }));
        fs::write(plugin.join("bundle.mcpb"), &bundle).unwrap();

        // Exactly the state an interrupted / failed unpack leaves behind: the
        // hash-keyed cache directory exists but holds no extracted manifest.
        let short: String = crate::mcpb::sha256_hex(&bundle).chars().take(16).collect();
        fs::create_dir_all(plugin.join(".mcpb-cache").join(&short)).unwrap();

        let (_id, manifest) =
            load_plugin_from_path_with_mcp_gate(plugin, false, Some("demo@marketplace-x"))
                .await
                .unwrap();
        assert!(
            manifest
                .components
                .mcp_servers
                .contains_key("bundled-server"),
            "an empty cache directory must be re-extracted into, not treated as a \
             completed extraction, got {:?}",
            manifest.components.mcp_servers
        );
    }

    /// The other half of the same rule: a failed unpack must not leave the
    /// directory it created behind, or the next load reads it as a cache hit.
    #[tokio::test]
    async fn a_failed_mcpb_extraction_leaves_no_cache_dir_behind() {
        let _g = super::skip_mcp_test_env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            serde_json::json!({"name": "demo", "mcpServers": "./bundle.mcpb"}).to_string(),
        )
        .unwrap();
        let bad = b"not a zip archive";
        fs::write(plugin.join("bundle.mcpb"), bad).unwrap();

        let (_id, manifest) =
            load_plugin_from_path_with_mcp_gate(plugin, false, Some("demo@marketplace-x"))
                .await
                .unwrap();
        assert!(manifest.components.mcp_servers.is_empty());
        let short: String = crate::mcpb::sha256_hex(bad).chars().take(16).collect();
        assert!(
            !plugin.join(".mcpb-cache").join(&short).exists(),
            "a failed extraction must not leave a directory a later load mistakes \
             for a completed extraction"
        );
    }

    #[tokio::test]
    async fn mcp_servers_mcpb_manifest_with_no_server_yields_no_server() {
        let _g = super::skip_mcp_test_env_guard();
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path();
        fs::create_dir_all(plugin.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        fs::write(
            plugin
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            serde_json::json!({"name": "demo", "mcpServers": "./bundle.mcpb"}).to_string(),
        )
        .unwrap();
        let bundle = build_mcpb_zip(&serde_json::json!({"name": "bundled-server"}));
        fs::write(plugin.join("bundle.mcpb"), bundle).unwrap();
        let (_id, manifest) =
            load_plugin_from_path_with_mcp_gate(plugin, false, Some("demo@marketplace-x"))
                .await
                .unwrap();
        assert!(
            manifest.components.mcp_servers.is_empty(),
            "an MCPB manifest with no `server` must yield no server, got {:?}",
            manifest.components.mcp_servers
        );
    }
}
