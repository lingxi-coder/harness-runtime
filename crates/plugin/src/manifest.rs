//! Plugin manifest — the Claude-Code-compatible component surface.
//!
//! A `PluginManifest` is the canonical description of a plugin: identity,
//! source provenance, declared components (commands / agents / skills /
//! hooks / output styles / MCP servers / LSP servers / workflows), trust
//! level, and optional user-config schema. Materialization into the engine
//! registries is handled by [`crate::manager::PluginManager::load_plugin`],
//! including the shared plugin-workflow registry.
//!
//! See spec §15.1.

use crate::source::PluginSource;
use crate::trust::PluginTrustLevel;
use crate::PluginDependency;
use hooks::HookDefinition;
use indexmap::IndexMap;
use lingxi_core::host::LspServerConfig;
use lingxi_core::types::PluginId;
use mcp::McpServerConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

fn default_plugin_enabled() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

/// Top-level plugin manifest.
///
/// Loaded once at install/enable time and held by the [`crate::lifecycle::PluginState`]
/// machine throughout the plugin's lifetime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Engine-assigned stable identifier.
    pub id: PluginId,
    /// Human-readable plugin name.
    pub name: String,
    /// Optional display label used by plugin-facing UI. Falls back to
    /// [`Self::name`] when absent and never participates in namespacing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Fallback activation state when no `enabledPlugins` scope has an
    /// explicit value for this plugin.
    #[serde(default = "default_plugin_enabled", skip_serializing_if = "is_true")]
    pub default_enabled: bool,
    /// Semver-style version string.
    pub version: String,
    /// Free-form description.
    pub description: String,
    /// Optional author display name (`author.name`, or the bare-string form).
    pub author: Option<String>,
    /// Optional author contact email (`author.email`). Oracle: "Contact
    /// email for support or feedback." Parsed but not yet consumed by any
    /// engine surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_email: Option<String>,
    /// Optional author website/profile URL (`author.url`). Oracle: "Website,
    /// GitHub profile, or organization URL."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_url: Option<String>,
    /// Optional homepage / repo URL.
    pub homepage: Option<String>,
    /// Where the plugin came from.
    pub source: PluginSource,
    /// Declared components (the 8-tuple materialised into engine registries).
    pub components: PluginComponents,
    /// Trust classification (defaults to [`crate::trust::default_trust_for_source`]).
    pub trust_level: PluginTrustLevel,
    /// Plugin ids this plugin depends on.
    pub depends_on: Vec<PluginId>,
    /// Versioned dependency declarations from `plugin.json`.
    #[serde(default)]
    pub dependencies: Vec<PluginDependency>,
    /// Optional user-config schema. Sensitive fields are resolved through
    /// the [`secret::CredentialManager`] at load time.
    pub user_config: Option<UserConfigSchema>,
    /// Plugin-declared channels (each binds an MCP server to a channel name).
    pub channels: Vec<PluginChannel>,
    /// Free-form settings the plugin author wants to ship with the manifest.
    pub settings: HashMap<String, serde_json::Value>,
    /// Whether the manifest declared a top-level `settings` key, even if it
    /// normalized to an empty map. This preserves the oracle's
    /// declaration-presence distinction for telemetry.
    #[serde(default)]
    pub settings_declared: bool,
    /// Discovery/categorization tags (`keywords` in `plugin.json`). Oracle:
    /// "Tags for plugin discovery and categorization." Not consumed by any
    /// engine surface today; carried for marketplace-search parity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    /// SPDX license identifier (`license` in `plugin.json`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Source-code repository URL (`repository` in `plugin.json`). Oracle
    /// schema `Cs`: a plain string ("Source code repository URL"), not the
    /// npm-style `{type,url,directory}` object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Free-form author-owned metadata (`metadata` in `plugin.json`).
    /// Oracle: "Free-form metadata for the plugin author's own use (e.g.
    /// entitlement or catalog fields). Preserved on the parsed manifest but
    /// not read by Claude Code." Only object-shaped values survive parsing
    /// (see `discovery::load_plugin_from_path_with_mcp_gate`); anything else
    /// is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// One `experimental.syntaxHighlighting.hljsLanguages` entry (oracle `Ks`,
/// `.strict()`): a
/// custom highlight.js language grammar the plugin registers, fetched from an
/// integrity-pinned `npm:`/`github:` source. Parsed + validated only — no
/// engine surface fetches or registers these grammars yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HljsLanguageEntry {
    /// highlight.js language id. Oracle `As`: `^[a-z][a-z0-9_-]*$`, <=64 chars.
    pub id: String,
    /// `npm:<pkg>[@version]` or `github:<owner>/<repo>@<ref>#<path>.js`,
    /// <=256 chars. Absent when the plugin ships the grammar file itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// Subresource-integrity hash gating the fetched grammar: `sha256-`,
    /// `sha384-`, or `sha512-` followed by base64, <=512 chars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
}

/// One `binaries` entry (oracle `Gs`): a sha256-pinned file fetched into
/// `bin/` at install time, keyed by basename (the target triple is encoded in
/// the name itself, e.g. `mytool-x86_64-apple-darwin`). Parsed + validated
/// only here — [`crate::discovery`]'s parser enforces the basename charset,
/// the 64-hex digest shape, and the 64-entry cap (oracle `n1e`/`Jqt`) the same
/// way the oracle's lenient `.transform()` does (silently dropping an invalid
/// entry rather than failing the whole manifest); the actual network fetch
/// into `bin/` is an install-path concern this change does not wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryPin {
    /// Lowercase 64-hex sha256 digest the fetched file must match.
    pub sha256: String,
}

/// A `monitors` entry's arm trigger (oracle `$s.when`): `"always"` arms at
/// session start and on plugin reload; `"on-skill-invoke:<skill>"` arms the
/// first time that skill is dispatched. Defaults to `Always`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MonitorTrigger {
    /// Arms at session start and on plugin reload.
    #[default]
    Always,
    /// Arms the first time the named skill is dispatched.
    OnSkillInvoke(String),
}

impl MonitorTrigger {
    /// Parse the oracle `when` string: `"always"`, or `"on-skill-invoke:"`
    /// followed by a non-empty skill name (oracle: `.refine(e=>e.length>16)`
    /// — the literal prefix itself is 16 chars, so this is exactly "at least
    /// one char after the colon").
    ///
    /// # Errors
    /// Returns a byte-faithful message when `raw` matches neither shape.
    pub fn parse(raw: &str) -> Result<Self, String> {
        if raw == "always" {
            return Ok(Self::Always);
        }
        match raw.strip_prefix("on-skill-invoke:") {
            Some(skill) if !skill.is_empty() => Ok(Self::OnSkillInvoke(skill.to_string())),
            Some(_) => Err("on-skill-invoke: must specify a skill name".to_string()),
            None => Err(format!(
                "monitor \"when\" must be \"always\" or \"on-skill-invoke:<skill>\", got {raw:?}"
            )),
        }
    }

    /// Render back to the oracle's on-wire string form.
    #[must_use]
    pub fn as_str(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Always => std::borrow::Cow::Borrowed("always"),
            Self::OnSkillInvoke(skill) => {
                std::borrow::Cow::Owned(format!("on-skill-invoke:{skill}"))
            }
        }
    }
}

impl Serialize for MonitorTrigger {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for MonitorTrigger {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// One `monitors` entry (oracle `$s`, a strict object): a persistent
/// background watch script the host can arm as a Monitor task ("unsandboxed,
/// same trust tier as hooks"). [`crate::discovery`] enforces the strict shape
/// and unique-`name` constraint (oracle `kAn`); `PluginManager` submits the
/// resulting registration through the host's shared task registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMonitor {
    /// Identifier for this monitor, unique within the plugin.
    pub name: String,
    /// Shell command to run as a persistent background monitor.
    pub command: String,
    /// Short human-readable description of what is being monitored.
    pub description: String,
    /// Arm trigger. Defaults to [`MonitorTrigger::Always`].
    #[serde(default)]
    pub when: MonitorTrigger,
}

/// The component slots a plugin can populate.
///
/// Every slot is materialized by [`crate::manager::PluginManager::load_plugin`]
/// when its corresponding registry is wired by the composition root.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginComponents {
    /// Number of manifest-declared skill paths (`skills`), before expansion.
    #[serde(default)]
    pub declared_skill_path_count: u32,
    /// Number of manifest-declared command entries (`commands`), before
    /// expansion/materialization.
    #[serde(default)]
    pub declared_command_path_count: u32,
    /// Number of manifest-declared agent paths (`agents`), before expansion.
    #[serde(default)]
    pub declared_agent_path_count: u32,
    /// Markdown slash-command files.
    pub commands: Vec<ComponentPath>,
    /// Markdown agent files.
    pub agents: Vec<ComponentPath>,
    /// Markdown skill files.
    pub skills: Vec<ComponentPath>,
    /// Output-style files.
    pub output_styles: Vec<ComponentPath>,
    /// Theme definition files (`.json`; `themes` in `plugin.json`). Oracle:
    /// "Path to a themes directory or file, relative to the plugin root.
    /// When set, the themes/ directory is not auto-loaded — list its files
    /// here if you want both." A manifest declaration REPLACES (does not
    /// merge with) the `themes/` auto-scan, matching [`Self::output_styles`].
    /// ⚠️ `experimental.themes` is the HIGHER-precedence spelling
    /// (`A.experimental?.themes ?? A.themes`) — see `discovery::RawExperimental`.
    ///
    /// `PluginManager::load_plugin` validates each file and materializes it
    /// into [`crate::theme_registry::PluginThemeRegistry`] as
    /// `{plugin}:{basename}`. That registry has NO reader outside the
    /// manager yet: the TUI theme-selection surface (`/theme`, a persisted
    /// choice, the `custom:` wire encoding) is a separate deferred feature,
    /// so a plugin theme is registered but not yet selectable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub themes: Vec<ComponentPath>,
    /// Workflow script files (`.js`; `workflows` in `plugin.json`). Oracle:
    /// "Path to a workflows directory or .js file, relative to the plugin
    /// root. When set, the workflows/ directory is not auto-loaded — list
    /// its files here if you want both." Replaces (does not merge with) the
    /// `workflows/` auto-scan, matching [`Self::output_styles`]. Unlike
    /// [`Self::themes`], there is NO `experimental.workflows` alias in the
    /// oracle's record builder.
    ///
    /// `PluginManager::load_plugin` materializes each file into the shared
    /// `workflow::PluginWorkflowRegistry` as `{plugin}:{meta.name}` (dropping
    /// any file that is not a regular file, exceeds
    /// `workflow::MAX_WORKFLOW_SCRIPT_BYTES`, or whose `meta` block does not
    /// parse), where the `Workflow` tool, its launcher, and nested
    /// `workflow({name})` all resolve it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<ComponentPath>,
    /// Custom highlight.js language grammars this plugin registers
    /// (`experimental.syntaxHighlighting.hljsLanguages` in `plugin.json`,
    /// oracle `Hs` — accepted ONLY under `experimental`, never at the top
    /// level). Parsed + validated only — nothing fetches or registers these
    /// grammars yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hljs_languages: Vec<HljsLanguageEntry>,
    /// sha256-pinned files fetched into `bin/` at install time, keyed by
    /// basename (`binaries` in `plugin.json`, oracle `qs`). Parsed +
    /// validated only — the actual fetch is an install-path concern this
    /// change does not wire.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub binaries: HashMap<String, BinaryPin>,
    /// Background watch scripts the host can arm as persistent Monitor tasks
    /// (`monitors` in `plugin.json`, or the `monitors/monitors.json`
    /// auto-scan when the field is absent; oracle `mt`). The manager arms
    /// `always` entries during enable and exposes on-skill activation through
    /// its shared task-registry seam.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub monitors: Vec<PluginMonitor>,
    /// Inline hook definitions.
    pub hooks: Vec<HookDefinition>,
    /// The single programmatic hooks module declared by `hooks/hooks.json`.
    /// Its path is resolved against that file and confined to the plugin root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mod_module: Option<PathBuf>,
    /// Whether `plugin.json` declared `hooks`, even if it normalized to zero
    /// live hook definitions.
    #[serde(default)]
    pub hooks_declared: bool,
    /// MCP servers contributed by this plugin, keyed by logical name. Always
    /// empty when [`Self::skip_mcp_discovery`] is `true`.
    pub mcp_servers: HashMap<String, McpServerConfig>,
    /// Whether `plugin.json` declared `mcpServers`, even if it normalized to
    /// an empty map.
    #[serde(default)]
    pub mcp_servers_declared: bool,
    /// LSP servers contributed by this plugin, keyed by logical name.
    pub lsp_servers: IndexMap<String, LspServerConfig>,
    /// Whether `plugin.json` declared `lspServers`, even if it normalized to
    /// an empty map.
    #[serde(default)]
    pub lsp_servers_declared: bool,
    /// Whether this load suppressed MCP server discovery for this plugin —
    /// neither the plugin-root `.mcp.json` nor the manifest's declared
    /// `mcpServers` was read, so [`Self::mcp_servers`] is empty regardless of
    /// what the plugin actually declares. Every other component slot still
    /// loads normally.
    ///
    /// Set when an SDK host declared this plugin instance with
    /// `skipMcpDiscovery: true` (it owns the plugin's MCP connections itself
    /// — oracle `PluginConfigSchema`'s `local` variant, @155779385: *"the
    /// engine loads skills/hooks/agents/commands from this plugin but does
    /// NOT read its .mcp.json or manifest mcpServers"*), or when
    /// `CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS` / `LINGXI_SKIP_PLUGIN_MCP_SERVERS`
    /// suppressed discovery process-wide and this plugin was not exempted via
    /// the `_EXCEPT` sibling (see `discovery::resolve_skip_mcp_discovery`).
    ///
    /// Deliberately lives here rather than on [`PluginManifest`] itself: this
    /// struct derives `Default` and its one construction site
    /// (`discovery::detect_components`) is fully owned by the same change, so
    /// adding a field cannot break another `PluginManifest { .. }` literal
    /// elsewhere in the crate that lists every field by hand and would
    /// otherwise need updating too (`plugin/src/loader.rs`'s test fixture,
    /// notably — outside this change's file ownership). A later telemetry
    /// batch reads this to derive the oracle's `has_mcp`
    /// (`!skip_mcp_discovery && !mcp_servers.is_empty()`) and `host_owned_mcp`
    /// (`skip_mcp_discovery`) fields; this change adds no telemetry itself.
    #[serde(default)]
    pub skip_mcp_discovery: bool,
}

/// On-disk component reference plus arbitrary metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentPath {
    /// Path to the component file (relative to the plugin install dir).
    pub path: PathBuf,
    /// Optional manifest-side metadata for the component.
    pub metadata: Option<serde_json::Value>,
}

/// User-config schema declared by a plugin.
///
/// Sensitive fields are routed through [`secret::CredentialManager`]'s
/// plugin-secret storage; non-sensitive values are pulled from the settings-file
/// `pluginConfigs[plugin].options` map (falling back to a field's declared
/// `default`) at load time. See [`crate::loader::resolve_user_config`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserConfigSchema {
    /// Map of field name to declared field.
    pub fields: HashMap<String, UserConfigField>,
}

/// One declared user-config field.
///
/// Deserialization is hand-written (see the `Deserialize` impl below) rather
/// than derived: oracle schema `gt` requires `type`/`title`/`description` and
/// restricts `type` to a fixed enum — see [`UserConfigField::deserialize`].
#[derive(Debug, Clone, Default, Serialize)]
pub struct UserConfigField {
    /// Declared input type: one of `string`, `number`, `boolean`,
    /// `directory`, `file` (oracle `gt`'s `type` — a required, fixed enum,
    /// not an arbitrary string). Kept as `Option<String>` at the type level
    /// only for [`Default`]/round-trip convenience; a value straight off
    /// `plugin.json` is always `Some` after [`UserConfigField::deserialize`]
    /// validates it.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub value_type: Option<String>,
    /// Label shown in the configuration dialog. Oracle: required (no
    /// `.optional()`); see [`Self::value_type`] on the `Option` wrapper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Help text for the host UI. Oracle: required (no `.optional()`).
    #[serde(default)]
    pub description: String,
    /// If `true` the value is fetched through the secret-storage backend.
    #[serde(default)]
    pub sensitive: bool,
    /// If `true` the loader fails when the value is missing.
    #[serde(default)]
    pub required: bool,
    /// Optional default value applied when no configured value is present
    /// (non-sensitive only — secrets are never defaulted). Mirrors claude-code's
    /// `if(p.default!==void 0)u[d]=p.default` substitution-context seeding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    /// Whether a string field accepts multiple values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
    /// Optional numeric lower bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    /// Optional numeric upper bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

/// The fixed `type` enum oracle schema `gt` accepts for a `userConfig` field.
/// Any other string (including one merely absent) fails the field the same
/// way a malformed `commands` entry fails the whole `plugin.json` parse (see
/// `RawCommandEntry` in `discovery.rs`) — not silently coerced or dropped.
const USER_CONFIG_FIELD_TYPES: [&str; 5] = ["string", "number", "boolean", "directory", "file"];

impl<'de> Deserialize<'de> for UserConfigField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(rename = "type", default)]
            value_type: Option<String>,
            #[serde(default)]
            title: Option<String>,
            #[serde(default)]
            description: Option<String>,
            #[serde(default)]
            sensitive: bool,
            #[serde(default)]
            required: bool,
            #[serde(default)]
            default: Option<serde_json::Value>,
            #[serde(default)]
            multiple: Option<bool>,
            #[serde(default)]
            min: Option<f64>,
            #[serde(default)]
            max: Option<f64>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let value_type = raw.value_type.ok_or_else(|| {
            serde::de::Error::custom("userConfig field is missing required \"type\"")
        })?;
        if !USER_CONFIG_FIELD_TYPES.contains(&value_type.as_str()) {
            return Err(serde::de::Error::custom(format!(
                "userConfig field \"type\" must be one of {USER_CONFIG_FIELD_TYPES:?}, got {value_type:?}"
            )));
        }
        let title = raw.title.ok_or_else(|| {
            serde::de::Error::custom("userConfig field is missing required \"title\"")
        })?;
        let description = raw.description.ok_or_else(|| {
            serde::de::Error::custom("userConfig field is missing required \"description\"")
        })?;
        Ok(UserConfigField {
            value_type: Some(value_type),
            title: Some(title),
            description,
            sensitive: raw.sensitive,
            required: raw.required,
            default: raw.default,
            multiple: raw.multiple,
            min: raw.min,
            max: raw.max,
        })
    }
}

/// A plugin's persisted `userConfig` state at one settings scope: the
/// on-disk `pluginConfigs[plugin]` object.
///
/// Byte-parity with claude-code's zod
/// `pluginConfigs:record(string,object({mcpServers,options}))`: `options` holds
/// top-level non-sensitive userConfig values; `mcp_servers` holds per-server
/// overrides keyed by logical server name. Sensitive values are NEVER stored
/// here — they live in secure storage (see [`secret::CredentialManager`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginUserConfig {
    /// Top-level non-sensitive userConfig values (`options`).
    #[serde(default)]
    pub options: serde_json::Map<String, serde_json::Value>,
    /// Per-server non-sensitive overrides (`mcpServers[server]`).
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: HashMap<String, serde_json::Map<String, serde_json::Value>>,
}

impl PluginUserConfig {
    /// Extract the `pluginConfigs` map from a settings-file JSON object (the
    /// shape [`migrations::settings_update::read_settings_map`] returns),
    /// yielding `plugin-id -> PluginUserConfig`. Missing / malformed entries are
    /// skipped. This is the read side of the settings `pluginConfigs` scope.
    #[must_use]
    pub fn from_settings_map(
        settings: &serde_json::Map<String, serde_json::Value>,
    ) -> HashMap<String, PluginUserConfig> {
        let mut out = HashMap::new();
        let Some(configs) = settings.get("pluginConfigs").and_then(|v| v.as_object()) else {
            return out;
        };
        for (plugin, entry) in configs {
            if let Ok(parsed) = serde_json::from_value::<PluginUserConfig>(entry.clone()) {
                out.insert(plugin.clone(), parsed);
            }
        }
        out
    }
}

/// A channel the plugin binds to an MCP server.
///
/// This is the public `plugin.json` shape used by Claude Code 2.1.220. There
/// is no separate channel name: the referenced MCP server identifies the
/// channel, and optional per-channel configuration uses the same schema as
/// top-level plugin `userConfig`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginChannel {
    /// MCP server name (must match a key in [`PluginComponents::mcp_servers`]).
    pub server: String,
    /// Human-readable label for the config dialog title. Oracle: "Defaults
    /// to the server name" when absent — this field only carries an
    /// explicit override; a consumer wanting the effective label should
    /// fall back to [`Self::server`] itself when this is `None`.
    #[serde(
        rename = "displayName",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub display_name: Option<String>,
    /// Optional channel-scoped user-config schema.
    #[serde(
        rename = "userConfig",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub user_config: Option<UserConfigSchema>,
}
