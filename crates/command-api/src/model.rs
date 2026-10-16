//! Data model for slash commands: registry entries, frontmatter, results, and
//! the trait every built-in handler implements.

use crate::parser::ParsedSlashCommand;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{Effect, McpConnectionId, PluginId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// Owning session context for a dynamic bundled skill prompt. Every production
/// expansion supplies its model and directories; preloading keeps activation off.
#[derive(Clone, Copy)]
pub struct BundledPromptContext<'a> {
    /// Owning session's project root.
    pub project_root: &'a std::path::Path,
    /// Current execution directory, including isolated child worktrees.
    pub cwd: &'a std::path::Path,
    /// Loading instructions must not activate the skill.
    pub is_preload: bool,
    /// Dispatching session or child's selected model, rather than its parent.
    pub main_loop_model: Option<&'a str>,
}

/// A context-aware dynamic bundled skill prompt.
pub trait BundledPromptFn: Send + Sync {
    /// Whether expansion consumes the selected model. Stateless templates do
    /// not need route lookup merely to produce their fixed instructions.
    fn needs_model_context(&self) -> bool {
        false
    }

    /// Produce the model-facing prompt for the exact raw args source. Builders
    /// retain UTF-16 code units when inserting user text into the prompt.
    fn build(&self, args: &Utf16JsonProjection) -> Utf16JsonProjection;

    /// Build instructions for agent skill preloading, without treating the load
    /// as a user invocation. Stateless builders can use the normal expansion.
    fn build_for_preload(&self, args: &Utf16JsonProjection) -> Utf16JsonProjection {
        self.build(args)
    }

    /// Expand in the owning session's project root and current directory.
    /// File-backed builders propagate read errors instead of silently selecting
    /// different instructions. Preloading must not activate a command.
    fn try_build_at(
        &self,
        args: &Utf16JsonProjection,
        context: BundledPromptContext<'_>,
    ) -> std::io::Result<Utf16JsonProjection> {
        Ok(if context.is_preload {
            self.build_for_preload(args)
        } else {
            self.build(args)
        })
    }
}

/// Build an exact root-string projection from UTF-16 units.
pub(crate) fn source_from_units(units: Vec<u16>) -> Utf16JsonProjection {
    let display = String::from_utf16_lossy(&units);
    Utf16JsonProjection::root_string(display, units)
        .expect("display projection is derived from the same UTF-16 units")
}

/// Append generated Rust text to a source prompt's UTF-16 buffer.
pub(crate) fn append_text_units(output: &mut Vec<u16>, text: &str) {
    output.extend(text.encode_utf16());
}

/// Append the exact units from a root-string source projection.
pub(crate) fn append_source_units(output: &mut Vec<u16>, source: &Utf16JsonProjection) {
    output.extend_from_slice(&source.string_units("").unwrap_or_default());
}

/// One exact text fragment in a generated prompt.
pub(crate) enum PromptPart<'a> {
    /// Generated UTF-8 Rust text.
    Text(&'a str),
    /// User or provider text that must retain its source UTF-16 units.
    Source(&'a Utf16JsonProjection),
}

/// Concatenate generated text and source projections into one exact prompt.
pub(crate) fn source_concat<'a>(
    parts: impl IntoIterator<Item = PromptPart<'a>>,
) -> Utf16JsonProjection {
    let mut units = Vec::new();
    for part in parts {
        match part {
            PromptPart::Text(text) => append_text_units(&mut units, text),
            PromptPart::Source(source) => append_source_units(&mut units, source),
        }
    }
    source_from_units(units)
}

/// Trim JavaScript whitespace from both ends without decoding the source.
#[cfg(any(test, feature = "builtin-handlers"))]
pub(crate) fn trim_source_text(source: &Utf16JsonProjection) -> Utf16JsonProjection {
    let units = source.string_units("").unwrap_or_default();
    let mut start = 0;
    let mut end = units.len();
    while start < end && source_whitespace_unit(units[start]) {
        start += 1;
    }
    while end > start && source_whitespace_unit(units[end - 1]) {
        end -= 1;
    }
    source_from_units(units[start..end].to_vec())
}

#[cfg(test)]
pub(crate) trait SourceTextTestExt {
    fn contains(&self, needle: &str) -> bool;
    fn starts_with(&self, prefix: &str) -> bool;
    fn ends_with(&self, suffix: &str) -> bool;
}

#[cfg(test)]
impl SourceTextTestExt for Utf16JsonProjection {
    fn contains(&self, needle: &str) -> bool {
        self.string_at("").is_some_and(|text| text.contains(needle))
    }

    fn starts_with(&self, prefix: &str) -> bool {
        self.string_at("")
            .is_some_and(|text| text.starts_with(prefix))
    }

    fn ends_with(&self, suffix: &str) -> bool {
        self.string_at("")
            .is_some_and(|text| text.ends_with(suffix))
    }
}

#[cfg(any(test, feature = "builtin-handlers"))]
fn source_whitespace_unit(unit: u16) -> bool {
    char::from_u32(u32::from(unit)).is_some_and(lingxi_core::host::effort::javascript_whitespace)
}

impl std::fmt::Debug for dyn BundledPromptFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BundledPromptFn")
    }
}

/// Native workspace requirement carried from the command factory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NativeCommandRequirements {
    /// `None` preserves an omitted Native property.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<bool>,
}

/// Native static command policy-gate descriptor. The verdict remains a
/// separate per-session host fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeCommandPolicyGate {
    /// Policy key (for example `allow_insights`).
    pub policy: String,
}

/// A registered slash command (built-in, markdown-defined, plugin-supplied,
/// or MCP-derived).
///
/// The trailing metadata fields mirror the TS `CommandBase` shape
/// (`claude-code/src/types/command.ts:175`): `disableModelInvocation`,
/// `hasUserSpecifiedDescription`, `loadedFrom`, `whenToUse`, `aliases`, and
/// `argumentHint`. They drive model-invocable filtering, alias resolution, and
/// the source-annotated description (see [`crate::describe`]). Each carries
/// `#[serde(default, skip_serializing_if = …)]` so that when absent the wire
/// JSON is byte-identical to the pre-existing `{name, description, source,
/// kind}` shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommand {
    /// Command name (without the leading `/`), retained as exact JavaScript text.
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub name: Utf16JsonProjection,
    /// Short user-facing description, retained as exact JavaScript text.
    #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
    pub description: Utf16JsonProjection,
    /// Origin classification.
    pub source: CommandSource,
    /// Concrete dispatch shape — built-in handler, markdown template, etc.
    pub kind: SlashCommandKind,
    /// Whether the native command object is hidden from command discovery.
    /// This is internal registry metadata; client listing DTOs carry their own
    /// display-specific `hidden` field.
    #[serde(skip)]
    pub is_hidden: bool,
    /// Result of the native command's availability predicate in the current
    /// host context. Disabled commands remain dispatchable by exact name when
    /// the native implementation permits it, but do not appear in discovery.
    #[serde(skip, default = "command_enabled_by_default")]
    pub is_enabled: bool,
    /// Native command execution type used by Headless's `MPe` admission
    /// predicate. `None` means this producer has no matching Native command
    /// object, so it is not admitted to the Native Headless command pool.
    #[serde(skip)]
    pub native_type: Option<NativeCommandType>,
    /// Native `disableNonInteractive` property. For prompt commands, a missing
    /// source property is false; for other types this field has no effect.
    #[serde(skip)]
    pub disable_non_interactive: bool,
    /// Native `supportsNonInteractive` property. For local commands, a missing
    /// source property is false; for other types this field has no effect.
    #[serde(skip)]
    pub supports_non_interactive: bool,
    /// Whether the model is forbidden from invoking this command.
    #[serde(default, skip_serializing_if = "is_false")]
    pub disable_model_invocation: bool,
    /// Whether the description came from an explicit user/frontmatter field.
    #[serde(default, skip_serializing_if = "is_false")]
    pub has_user_specified_description: bool,
    /// Where the command was loaded from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded_from: Option<String>,
    /// Advisory model guidance from skill metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    /// Gitignore-style conditional skill path patterns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    /// Alternate names this command also resolves by, with exact identity units.
    #[serde(
        default,
        with = "projection_string_array_serde",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub aliases: Vec<Utf16JsonProjection>,
    /// Exact argument hint text shown after the command name.
    #[serde(
        default,
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub argument_hint: Option<Utf16JsonProjection>,
    /// Native prompt command's explicit `userFacingName`, when its factory
    /// supplies one. This is internal model-listing metadata; it is not part of
    /// the Bridge command wire shape.
    #[serde(skip)]
    pub user_facing_name: Option<Utf16JsonProjection>,
    /// Native prompt command's exact `unqualifiedName`, when its factory
    /// supplies one (currently relevant to synced-skill name resolution).
    #[serde(skip)]
    pub unqualified_name: Option<Utf16JsonProjection>,
    /// Native plugin prompt's manifest name, when the plugin factory owns it.
    /// It is separate from the namespaced command name and plugin ID.
    #[serde(skip)]
    pub plugin_name: Option<Utf16JsonProjection>,
    /// Native model-facing source tag used by skill listing diagnostics.
    /// Command factories set it only when the source is authoritative.
    #[serde(skip)]
    pub native_source: Option<Utf16JsonProjection>,
    /// Stable order of the selected Native command factory within its source group.
    #[serde(skip)]
    pub native_catalog_order: Option<usize>,
    /// Native prompt discriminator used by descriptor projection. The
    /// dispatcher continues to use `SlashCommandKind`.
    #[serde(skip)]
    pub native_slash_kind: Option<NativeSlashKind>,
    /// Native synced-skill reservation fact used when resolving a bare name.
    #[serde(skip)]
    pub bare_name_reserved: bool,
    /// Exact declared positional argument names.
    #[serde(
        default,
        with = "projection_string_array_serde",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub argument_names: Vec<Utf16JsonProjection>,
    /// Directory root for a directory-format skill command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_root: Option<PathBuf>,
    /// Whether a skill command is user-invocable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_invocable: Option<bool>,
    /// Whether the current command descriptor belongs to the terminal host.
    #[serde(default, skip_serializing_if = "is_false")]
    pub terminal_oriented: bool,
    /// Raw markdown byte length for file-backed skills.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_length: Option<usize>,
    /// Native `progressMessage`, retained as exact source text.
    #[serde(
        default,
        serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "lingxi_core::types::utf16_json::raw_json_serde::deserialize_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub progress_message: Option<Utf16JsonProjection>,
    /// Static Native workspace requirement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires: Option<NativeCommandRequirements>,
    /// Static Native policy-gate descriptor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_gate: Option<NativeCommandPolicyGate>,
    /// Compact UI label, absent when the full description is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu_description: Option<String>,
}

impl Default for SlashCommand {
    fn default() -> Self {
        Self {
            name: String::new().into(),
            description: String::new().into(),
            source: CommandSource::default(),
            kind: SlashCommandKind::default(),
            is_hidden: false,
            is_enabled: true,
            native_type: None,
            disable_non_interactive: false,
            supports_non_interactive: false,
            disable_model_invocation: false,
            has_user_specified_description: false,
            loaded_from: None,
            when_to_use: None,
            paths: None,
            aliases: Vec::new(),
            argument_hint: None,
            user_facing_name: None,
            unqualified_name: None,
            plugin_name: None,
            native_source: None,
            native_catalog_order: None,
            native_slash_kind: None,
            bare_name_reserved: false,
            argument_names: Vec::new(),
            skill_root: None,
            user_invocable: None,
            terminal_oriented: false,
            content_length: None,
            progress_message: None,
            requires: None,
            policy_gate: None,
            menu_description: None,
        }
    }
}

/// Native command execution kind. This is distinct from [`SlashCommandKind`],
/// which selects the Harness dispatcher and does not establish Native
/// Headless availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeCommandType {
    /// A prompt command backed by content injected into the conversation.
    Prompt,
    /// A text/local command that can run without the interactive TUI only when
    /// `supports_non_interactive` is true.
    Local,
    /// An interactive renderer command, excluded by Native Headless filtering.
    LocalJsx,
}

/// Native prompt kind consumed by Native descriptor projection, independently
/// of the Harness dispatch kind and command type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeSlashKind {
    /// The prompt launches a dynamic workflow through the Workflow tool.
    Workflow,
}

impl SlashCommand {
    /// Whether this command has Native `type === "prompt"` semantics.
    #[must_use]
    pub const fn is_prompt(&self) -> bool {
        matches!(self.native_type, Some(NativeCommandType::Prompt))
    }

    /// The known result of Native's Headless `bge` pool-builder predicate.
    /// `None` preserves unresolved producer metadata instead of treating it as
    /// proof that the source command is unavailable.
    #[must_use]
    pub const fn native_non_interactive_admission(&self) -> Option<bool> {
        match self.native_type {
            Some(NativeCommandType::Prompt) => Some(!self.disable_non_interactive),
            Some(NativeCommandType::Local) => Some(self.supports_non_interactive),
            Some(NativeCommandType::LocalJsx) => Some(false),
            None => None,
        }
    }
}

fn command_enabled_by_default() -> bool {
    true
}

/// The Native headless command catalog used when an unknown slash command is
/// treated as ordinary user input. `available_command_count` counts all
/// available commands; `prompt_command_names` contains the ordered subset
/// Native can invoke as prompt commands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadlessCommandCatalog {
    /// Native prompt command names, in registry/assembly order.
    pub prompt_command_names: Vec<Utf16JsonProjection>,
    /// All available slash commands before prompt-only/model-invocation gates.
    pub available_command_count: usize,
}

/// Render Native's unknown-command system reminder without converting source
/// command names through UTF-8 or changing their UTF-16 code units.
#[must_use]
pub fn render_unknown_command_reminder(
    command_name: &Utf16JsonProjection,
    suggestion: Option<&Utf16JsonProjection>,
    catalog: &HeadlessCommandCatalog,
) -> Utf16JsonProjection {
    use PromptPart::{Source, Text};

    let command_name_is_safe = is_native_reminder_command_name(command_name);
    let listed = native_unknown_command_fallback_available_names(catalog);
    let remaining = catalog.available_command_count.saturating_sub(listed.len());
    let remaining_count = remaining.to_string();
    let safe_suggestion = native_unknown_command_fallback_suggestion(suggestion);
    let mut parts = vec![Text("<system-reminder>\n")];
    if command_name_is_safe {
        parts.push(Text("The user's message starts with the slash command /"));
        parts.push(Source(command_name));
        parts.push(Text(
            ", but no command with that name is available in this session, so it did not run. ",
        ));
    } else {
        parts.push(Text(
            "The user's message starts with a slash command, but no command with that name is available in this session, so it did not run. ",
        ));
    }
    if let Some(suggestion) = safe_suggestion.as_ref() {
        parts.push(Text("The closest available command is /"));
        parts.push(Source(suggestion));
        parts.push(Text(". Ask the user before running it. "));
    }

    if listed.is_empty() {
        if remaining == 0 {
            parts.push(Text("No slash commands are available in this session."));
        } else {
            parts.push(Text("This session has "));
            parts.push(Text(&remaining_count));
            parts.push(Text(" slash "));
            parts.push(Text(if remaining == 1 {
                "command"
            } else {
                "commands"
            }));
            parts.push(Text(", but none can be named here."));
        }
    } else {
        parts.push(Text("Slash commands available in this session: "));
        for (index, name) in listed.iter().enumerate() {
            if index > 0 {
                parts.push(Text(", "));
            }
            parts.push(Text("/"));
            parts.push(Source(name));
        }
        if remaining > 0 {
            parts.push(Text(", and "));
            parts.push(Text(&remaining_count));
            parts.push(Text(" more"));
        }
        parts.push(Text("."));
    }
    parts.push(Text(
        " Treat the message as a plain request and do the task with the tools you have. If the task needs that command, tell the user it is not installed in this session. The user can add it as an organization plugin or a project skill. Do not give installation steps you are not sure of. Do not say the command ran.\n</system-reminder>",
    ));
    source_concat(parts)
}

/// Native `et` filters display names with `kKe` before taking its first 40;
/// `OWr` renders this supplied list and computes the remainder from the raw
/// available count. Keep that projection at the fallback boundary so both the
/// reminder and its transcript attachment receive the same names.
#[must_use]
pub fn native_unknown_command_fallback_available_names(
    catalog: &HeadlessCommandCatalog,
) -> Vec<Utf16JsonProjection> {
    catalog
        .prompt_command_names
        .iter()
        .filter(|name| is_native_reminder_command_name(name))
        .take(40)
        .cloned()
        .collect()
}

/// Native `et` omits an unsafe nearest candidate after selection; it does not
/// continue searching for a safer, farther name.
#[must_use]
pub fn native_unknown_command_fallback_suggestion(
    suggestion: Option<&Utf16JsonProjection>,
) -> Option<Utf16JsonProjection> {
    suggestion
        .filter(|name| is_native_reminder_command_name(name))
        .cloned()
}

/// Native 2.1.295 kKe: names shown in an unknown-command reminder must be at
/// most 256 UTF-16 code units and contain only ASCII letters, digits, `_`,
/// `:`, `.`, or `-`.
fn is_native_reminder_command_name(name: &Utf16JsonProjection) -> bool {
    let Some(units) = name.string_units("") else {
        return false;
    };
    !units.is_empty()
        && units.len() <= 256
        && units.iter().all(|unit| is_native_reminder_name_unit(*unit))
}

fn is_native_reminder_name_unit(unit: u16) -> bool {
    matches!(
        unit,
        0x30..=0x39 | 0x41..=0x5a | 0x5f | 0x2d | 0x2e | 0x3a | 0x61..=0x7a
    )
}

fn deserialize_prompt_template<'de, D>(deserializer: D) -> Result<Utf16JsonProjection, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let projection = lingxi_core::types::utf16_json::raw_json_serde::deserialize(deserializer)?;
    if projection.string_units("").is_none() {
        return Err(serde::de::Error::custom(
            "expected a JSON string prompt template",
        ));
    }
    Ok(projection)
}

/// Raw JSON adapter for arrays of exact root-string projections.
mod projection_string_array_serde {
    use super::Utf16JsonProjection;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(values: &Vec<Utf16JsonProjection>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let array =
            Utf16JsonProjection::array(values.clone()).map_err(serde::ser::Error::custom)?;
        lingxi_core::types::utf16_json::raw_json_serde::serialize(&array, serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<Utf16JsonProjection>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let array = lingxi_core::types::utf16_json::raw_json_serde::deserialize(deserializer)?;
        let len = array
            .array_len("")
            .ok_or_else(|| serde::de::Error::custom("expected an array of strings"))?;
        (0..len)
            .map(|index| {
                let value = array
                    .subprojection(&format!("/{index}"))
                    .map_err(serde::de::Error::custom)?;
                if value.string_units("").is_none() {
                    return Err(serde::de::Error::custom("expected an array of strings"));
                }
                Ok(value)
            })
            .collect()
    }
}

/// Serde `skip_serializing_if` predicate for `bool` fields that default to
/// `false` — keeps absent flags out of the wire JSON.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// What a `SlashCommand` actually dispatches to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SlashCommandKind {
    /// Dispatched to a Rust handler keyed by id.
    Builtin {
        /// Registry key used by [`crate::registry::CommandRegistry::get_handler`].
        handler_id: String,
    },
    /// Markdown file with frontmatter — body is injected after argument substitution.
    Markdown {
        /// Source file on disk.
        file_path: PathBuf,
        /// Parsed YAML frontmatter.
        frontmatter: CommandFrontmatter,
        /// Markdown body (template with `$ARGUMENTS` / `$N` placeholders).
        #[serde(
            serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize",
            deserialize_with = "deserialize_prompt_template"
        )]
        prompt_template: Utf16JsonProjection,
    },
    /// Markdown command shipped by a plugin.
    Plugin {
        /// Plugin that owns this command.
        plugin_id: PluginId,
        /// Source file on disk.
        file_path: PathBuf,
        /// Parsed YAML frontmatter.
        frontmatter: CommandFrontmatter,
        /// Markdown body (template).
        #[serde(
            serialize_with = "lingxi_core::types::utf16_json::raw_json_serde::serialize",
            deserialize_with = "deserialize_prompt_template"
        )]
        prompt_template: Utf16JsonProjection,
    },
    /// Bridged to an MCP server's named prompt.
    Mcp {
        /// MCP connection that owns the prompt.
        connection_id: McpConnectionId,
        /// Prompt name to fetch via `prompts/get`.
        #[serde(with = "lingxi_core::types::utf16_json::raw_json_serde")]
        prompt_name: Utf16JsonProjection,
        /// Server-declared named arguments, in wire order.
        #[serde(default)]
        arguments: Vec<lingxi_core::host::McpPromptArgumentDto>,
    },
    /// Programmatically-registered bundled skill (port of the reference
    /// `registerBundledSkill`, `bundledSkills.ts`). Unlike [`Self::Markdown`],
    /// its body is produced dynamically by `prompt_fn.build(args)` rather than
    /// templated, so it can branch on empty vs non-empty args (`loop.ts:84-90`).
    Bundled {
        /// Carries `model` / `allowed_tools` for descriptor parity with
        /// [`Self::Markdown`] (the reference bundled-skill spec also accepts
        /// these fields).
        frontmatter: CommandFrontmatter,
        /// Dynamic prompt builder. `#[serde(skip)]` keeps `SlashCommandKind`
        /// (de)serializable — bundled commands are reconstructed at boot, never
        /// loaded from disk, so a deserialized one is inert (`None`) by design.
        #[serde(skip)]
        prompt_fn: Option<Arc<dyn BundledPromptFn>>,
    },
}

impl Default for SlashCommandKind {
    /// An empty built-in dispatch shape. Exists so [`SlashCommand`] can derive
    /// [`Default`] (the derive macro cannot pick a default for an enum whose
    /// variants all carry fields).
    fn default() -> Self {
        Self::Builtin {
            handler_id: String::new(),
        }
    }
}

/// YAML frontmatter shape for markdown-defined slash commands.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandFrontmatter {
    /// Short user-facing description.
    pub description: String,
    /// Optional allow-list of tool names.
    #[serde(alias = "tools_allowed")]
    pub allowed_tools: Option<Vec<String>>,
    /// Tools REMOVED from this command's agent (frontmatter `disallowed-tools`).
    /// For a `context: fork` skill this is half the scoping the fork runs
    /// under. `None` ⇒ nothing extra denied.
    pub disallowed_tools: Option<Vec<String>>,
    /// Optional pinned model name.
    pub model: Option<String>,
    /// Session capability profiles this command may run under. Absent means the
    /// command remains code-only in mobile Chat/Code mode filtering.
    pub session_modes: Option<Vec<String>>,
    /// Free-form hints describing positional argument shape.
    pub argument_hints: Vec<String>,
    /// Declared positional argument names (frontmatter `arguments`). Used by the
    /// loader's named-argument substitution. Mirrors the TS frontmatter
    /// `arguments: string | string[]` union that feeds `parseArgumentNames`.
    pub argument_names: Vec<String>,
    /// Optional thinking-token budget.
    pub thinking: Option<u32>,
    /// Shell to route embedded shell-expansion blocks through. When absent the
    /// runtime defaults to bash (mirrors the TS `frontmatterParser`'s
    /// `shell?: FrontmatterShell`).
    pub shell: Option<FrontmatterShell>,
    /// SLASH.1: TS `disable-model-invocation` frontmatter
    /// (`parseBooleanFrontmatter`). When true the command is excluded from
    /// model-driven invocation (carried onto [`SlashCommand::disable_model_invocation`]).
    pub disable_model_invocation: bool,
    /// SLASH.4: TS `when_to_use` frontmatter — advisory text describing when the
    /// command applies (carried onto [`SlashCommand::when_to_use`]).
    pub when_to_use: Option<String>,
    /// Gitignore-style `paths:` patterns, carried onto
    /// [`SlashCommand::paths`]. `Some` marks a CONDITIONAL skill.
    pub paths: Option<Vec<String>>,
    /// Execution context: `Some("fork")` runs the command as a subagent under
    /// its own permission scoping. Anything else (including `None`) is inline.
    pub context: Option<String>,
    /// Whether a forking command runs in the BACKGROUND. `None` ⇒ background
    /// (claude's `background ?? true`).
    pub background: Option<bool>,
    /// Agent type a forking command spawns. `None` ⇒ `general-purpose`.
    pub agent: Option<String>,
    /// Frontmatter `effort`, raw. Upstream's skill→command builder carries it
    /// (`wXe({… effort: De …})`, 2.1.267 `src_163219561.js` @4555866) and a
    /// forked skill's scoping record spreads it in conditionally
    /// (`…n.effort !== void 0 && { effort: n.effort }`). Validated at the
    /// conversion site, not here.
    pub effort: Option<String>,
    /// Frontmatter `user-invocable`. `None` ⇒ the key was absent, which upstream
    /// reads as invocable (`userInvocable: dt===void 0 ? !0 : htt(dt)`); a
    /// declared value has already been through the `htt` coercion, so a
    /// `Some(false)` here hides the skill from the `/` menu.
    pub user_invocable: Option<bool>,
}

/// Shell selected by a markdown command's frontmatter for embedded shell
/// expansion. Mirrors the TS `FrontmatterShell` union (`bash` | `powershell`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrontmatterShell {
    /// Route embedded shell commands through bash (the default).
    Bash,
    /// Route embedded shell commands through `PowerShell`.
    PowerShell,
}

/// Where the command came from.
///
/// The settings rungs are the shared [`lingxi_core::types::SettingsScope`]; `Builtin`,
/// `Bundled`, `Plugin` and `Mcp` are this subsystem's own producers.
///
/// `Builtin` and `Bundled` are both "in the binary" and are NOT
/// interchangeable: the reference reports them as `"builtin"` and `"bundled"`,
/// and [`crate::dispatcher`] sends whichever applies on the
/// `UserPromptExpansion` hook payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CommandSource {
    /// Compiled-in handler.
    #[default]
    Builtin,
    /// Read from a settings tier's `commands/` directory.
    Settings(lingxi_core::types::SettingsScope),
    /// Loaded by an installed plugin.
    Plugin,
    /// Derived from an MCP server prompt.
    Mcp,
    /// Programmatically-registered bundled skill (TS `loadedFrom: 'bundled'` /
    /// `registerBundledSkill`). Model-invocable; never truncated in the skill
    /// listing.
    Bundled,
}

/// Possible outcomes from invoking a slash command.
#[derive(Debug)]
pub enum CommandResult {
    /// A source-preserving settings/context failure.
    Failed {
        /// The exact failure variant and its projection payload.
        source: lingxi_core::settings::SettingsError,
    },
    /// Synchronous completion with an optional inline display string.
    Done {
        /// Optional exact source text to show or serialize.
        display: Option<Utf16JsonProjection>,
    },
    /// Inject `content` into the conversation as the next user message.
    InjectMessage {
        /// Content to inject as a user message.
        content: Utf16JsonProjection,
    },
    /// Emit effects to be processed by the run loop.
    EmitEffects {
        /// Effects to route through the engine.
        effects: Vec<Effect>,
        /// Optional exact source text to show before effects apply.
        display: Option<Utf16JsonProjection>,
    },
    /// Ask the user to confirm before applying the supplied effects.
    RequestConfirmation {
        /// Prompt shown to the user.
        prompt: String,
        /// Effects applied iff the user confirms.
        on_confirm: Vec<Effect>,
    },
}

/// Source-authored preparation performed inside a Native prompt producer.
#[derive(Debug, Clone, PartialEq)]
pub enum NativePromptPreparation {
    /// Check the live cwd before expanding the producer's embedded shell text.
    /// Outside a worktree, the source returns this text as the prompt body.
    GitWorktreeShell {
        /// Exact Native text preceding the live cwd.
        outside_worktree_prefix: Utf16JsonProjection,
        /// Exact Native text following the live cwd.
        outside_worktree_suffix: Utf16JsonProjection,
    },
}

/// Trait implemented by every built-in slash-command handler.
#[async_trait::async_trait]
pub trait BuiltinCommandHandler: Send + Sync {
    /// Execute the command with parsed arguments and return a result.
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult;
    /// Execute with a caller-owned foreground cancellation token.
    async fn handle_with_cancel(
        &self,
        args: &ParsedSlashCommand,
        _cancel: lingxi_core::host::CancellationToken,
    ) -> CommandResult {
        self.handle(args).await
    }
    /// Canonical command name (without the leading `/`).
    fn name(&self) -> &str;
    /// Short user-facing description.
    fn description(&self) -> &str;
    /// Optional hint text shown after the command name in slash listings.
    fn argument_hint(&self) -> Option<&str> {
        None
    }
    /// Source aliases for this built-in command descriptor. Registration uses
    /// the same values for both descriptor output and command resolution.
    fn aliases(&self) -> Vec<Utf16JsonProjection> {
        Vec::new()
    }
    /// Native command object type for Headless command-pool admission. A
    /// producer without an exact Native command object returns `None`.
    fn native_type(&self) -> Option<NativeCommandType> {
        None
    }
    /// Preparation explicitly owned by this prompt's source producer.
    /// Other Native prompt bodies must not be scanned for shell expressions.
    fn native_prompt_preparation(&self) -> Option<NativePromptPreparation> {
        None
    }
    /// Native `disableNonInteractive` property for prompt command objects.
    fn disable_non_interactive(&self) -> bool {
        false
    }
    /// Native `supportsNonInteractive` property for local command objects.
    fn supports_non_interactive(&self) -> bool {
        false
    }
    /// Native `isHidden` value for the current host context.
    fn is_hidden(&self) -> bool {
        false
    }
    /// Evaluated Native availability predicate for the current host context.
    fn is_enabled(&self) -> bool {
        true
    }
    /// Native `disableModelInvocation` control.
    fn disable_model_invocation(&self) -> bool {
        false
    }
    /// Native command content length, when supplied by the factory.
    fn content_length(&self) -> Option<usize> {
        None
    }
    /// Native `progressMessage` as exact source text.
    fn progress_message(&self) -> Option<Utf16JsonProjection> {
        None
    }
    /// Native `requires` descriptor.
    fn requires(&self) -> Option<NativeCommandRequirements> {
        None
    }
    /// Native static `policyGate` descriptor.
    fn policy_gate(&self) -> Option<NativeCommandPolicyGate> {
        None
    }
    /// This command's declared allowed-tools (TS `command.allowedTools`), in the
    /// permission-rule syntax (e.g. `Bash(git add:*)`). Injected on top of the
    /// base policy for a fresh per-command effective policy before its embedded
    /// `!`cmd`` shell bodies are expanded (see
    /// [`crate::shell_expansion::ShellExpansionProvider`]).
    ///
    /// Defaults to empty: only the shell-embedding prompt builtins (`/commit`,
    /// `/commit-push-pr`, `/security-review`) override it, so every other
    /// handler is unaffected.
    fn allowed_tools(&self) -> &'static [&'static str] {
        &[]
    }
}

#[cfg(test)]
mod source_tests {
    use super::*;

    #[test]
    fn slash_command_raw_serde_keeps_exact_name_text_and_route_aliases() {
        let lone_name = Utf16JsonProjection::root_string(
            "cmd�".into(),
            vec![b'c' as u16, b'm' as u16, b'd' as u16, 0xd800],
        )
        .unwrap();
        let lone_description = Utf16JsonProjection::root_string(
            "desc�".into(),
            vec![b'd' as u16, b'e' as u16, b's' as u16, b'c' as u16, 0xdfff],
        )
        .unwrap();
        let command = SlashCommand {
            name: lone_name.clone(),
            description: lone_description.clone(),
            aliases: vec![Utf16JsonProjection::root_string(
                "alt�".into(),
                vec![b'a' as u16, b'l' as u16, b't' as u16, 0xd800],
            )
            .unwrap()],
            argument_hint: Some(
                Utf16JsonProjection::root_string(
                    "<arg�>".into(),
                    vec![
                        b'<' as u16,
                        b'a' as u16,
                        b'r' as u16,
                        b'g' as u16,
                        0xdfff,
                        b'>' as u16,
                    ],
                )
                .unwrap(),
            ),
            argument_names: vec![Utf16JsonProjection::root_string(
                "arg�".into(),
                vec![b'a' as u16, b'r' as u16, b'g' as u16, 0xd800],
            )
            .unwrap()],
            kind: SlashCommandKind::Mcp {
                connection_id: McpConnectionId::new(),
                prompt_name: lone_name.clone(),
                arguments: Vec::new(),
            },
            ..SlashCommand::default()
        };

        let wire = serde_json::to_string(&command).unwrap();
        assert!(wire.contains("\\ud800"));
        assert!(wire.contains("\\udfff"));
        let decoded: SlashCommand = serde_json::from_str(&wire).unwrap();
        assert_eq!(decoded.name.string_units(""), lone_name.string_units(""));
        assert_eq!(
            decoded.description.string_units(""),
            lone_description.string_units("")
        );
        assert_eq!(
            decoded.aliases[0].string_units(""),
            command.aliases[0].string_units("")
        );
        assert_eq!(
            decoded.argument_hint.unwrap().string_units(""),
            command.argument_hint.unwrap().string_units("")
        );
        assert_eq!(
            decoded.argument_names[0].string_units(""),
            command.argument_names[0].string_units("")
        );
        let SlashCommandKind::Mcp { prompt_name, .. } = decoded.kind else {
            panic!("expected MCP prompt")
        };
        assert_eq!(prompt_name.string_units(""), lone_name.string_units(""));
    }

    #[test]
    fn markdown_and_plugin_templates_round_trip_exact_utf16_units() {
        let template = Utf16JsonProjection::parse(r#""prompt \ud800 text""#).unwrap();
        let kinds = [
            SlashCommandKind::Markdown {
                file_path: PathBuf::from("command.md"),
                frontmatter: CommandFrontmatter::default(),
                prompt_template: template.clone(),
            },
            SlashCommandKind::Plugin {
                plugin_id: PluginId::new(),
                file_path: PathBuf::from("plugin-command.md"),
                frontmatter: CommandFrontmatter::default(),
                prompt_template: template.clone(),
            },
        ];

        for kind in kinds {
            let command = SlashCommand {
                kind,
                ..SlashCommand::default()
            };
            let wire = serde_json::to_string(&command).unwrap();
            assert!(wire.contains(r#""prompt \ud800 text""#), "{wire}");
            let decoded: SlashCommand = serde_json::from_str(&wire).unwrap();
            let decoded_template = match decoded.kind {
                SlashCommandKind::Markdown {
                    prompt_template, ..
                }
                | SlashCommandKind::Plugin {
                    prompt_template, ..
                } => prompt_template,
                _ => panic!("expected a markdown-backed command"),
            };
            assert_eq!(decoded_template.string_units(""), template.string_units(""));
        }
    }

    #[test]
    fn markdown_template_deserialization_requires_a_root_string() {
        let invalid =
            r#"{"Markdown":{"file_path":"command.md","frontmatter":{},"prompt_template":{}}}"#;
        assert!(serde_json::from_str::<SlashCommandKind>(invalid).is_err());
    }
}

#[cfg(test)]
mod native_insights_metadata_tests {
    use super::*;

    #[test]
    fn optional_native_metadata_round_trips_and_default_stays_omitted() {
        let command = SlashCommand {
            content_length: Some(0),
            progress_message: Some("analyzing your sessions".into()),
            requires: Some(NativeCommandRequirements {
                workspace: Some(true),
            }),
            policy_gate: Some(NativeCommandPolicyGate {
                policy: "allow_insights".into(),
            }),
            ..SlashCommand::default()
        };
        let encoded = serde_json::to_string(&command).unwrap();
        let decoded: SlashCommand = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.content_length, Some(0));
        assert_eq!(
            decoded.progress_message.unwrap().string_at(""),
            Some("analyzing your sessions")
        );
        assert_eq!(decoded.requires.unwrap().workspace, Some(true));
        assert_eq!(decoded.policy_gate.unwrap().policy, "allow_insights");

        let default_encoded = serde_json::to_string(&SlashCommand::default()).unwrap();
        assert!(!default_encoded.contains("progress_message"));
        assert!(!default_encoded.contains("policy_gate"));
        assert!(!default_encoded.contains("requires"));
    }
}
