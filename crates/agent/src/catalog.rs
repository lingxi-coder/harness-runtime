//! Agent catalog loader (M6-07).
//!
//! Reads markdown files from one or more `agents/` directories, parses the
//! YAML frontmatter, and projects each file into an
//! [`crate::definition::AgentDefinition`].
//!
//! Frontmatter shape (claude-code compatible — subset relevant to v0.7.0):
//! ```yaml
//! ---
//! name: reviewer
//! description: Reviews code for security and correctness.
//! tools: [Read, Grep, Bash]
//! model: sonnet
//! ---
//! Body of the system prompt goes here.
//! ```
//!
//! Files without a `---`-delimited YAML frontmatter block, or with
//! malformed YAML, are skipped at the loader level (logged via
//! `tracing::warn!`). The exposed [`parse_agent_markdown`] returns a
//! typed error so unit tests can assert on the failure mode.

use crate::definition::{
    parse_effort_value, parse_int_radix10, AgentCacheTtl, AgentDefinition, AgentEffort,
    AgentIsolation, AgentMcpServerSpec, AgentModel, AgentPermissionMode, AgentSource,
    AgentToolPolicy, ObserverSpec, EFFORT_LEVELS,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};

const UTF8_BOM: char = '\u{feff}';

/// Validated agent color names — claude `AGENT_COLORS` (agentColorManager.ts).
const AGENT_COLOR_NAMES: [&str; 8] = [
    "red", "blue", "green", "yellow", "purple", "orange", "pink", "cyan",
];

/// User-addressable permission-mode strings — claude `PERMISSION_MODES`.
/// Used to validate and preserve the raw frontmatter / `--agents` value.
const PERMISSION_MODES: [&str; 6] = [
    "acceptEdits",
    "auto",
    "bypassPermissions",
    "default",
    "dontAsk",
    "plan",
];

/// Valid memory scopes — claude `VALID_MEMORY_SCOPES = ['user','project','local']`.
const VALID_MEMORY_SCOPES: [&str; 3] = ["user", "project", "local"];

/// Errors raised while loading an agent file.
#[derive(Debug, thiserror::Error)]
pub enum AgentLoadError {
    /// File could not be read.
    #[error("read {path}: {source}")]
    Io {
        /// Offending path.
        path: PathBuf,
        /// Underlying io error.
        #[source]
        source: std::io::Error,
    },
    /// Frontmatter was missing or its `---` terminator was not found.
    #[error("no valid frontmatter in {0}")]
    NoFrontmatter(PathBuf),
    /// Frontmatter YAML failed to deserialize.
    #[error("deserialize frontmatter {path}: {source}")]
    Yaml {
        /// Offending path.
        path: PathBuf,
        /// Underlying yaml error.
        #[source]
        source: serde_yaml::Error,
    },
    /// Required `name` field was missing/non-string. Mirrors claude
    /// `parseAgentFromMarkdown` returning `null` (a SILENT skip — the file is
    /// likely co-located reference documentation, not an agent attempt).
    #[error("missing required \"name\" field in {0}")]
    MissingName(PathBuf),
    /// `name` failed validation: leading `-` (long-standing), or a `:` after
    /// NFKC normalization (cc 2.1.218 — `:` is reserved for the
    /// `plugin:agent` namespacing built by the plugin manager). The byte-exact
    /// claude error line is logged at the rejection site inside
    /// [`parse_agent_markdown`] (`iju` logs via `w(…, {level:"error"})` then
    /// returns `null`), so callers skip the file WITHOUT logging again.
    #[error("invalid \"name\" field in {0}")]
    InvalidName(PathBuf),
    /// Required `description` field was missing/non-string. claude logs and
    /// returns `null`.
    #[error("missing required \"description\" field in {0}")]
    MissingDescription(PathBuf),
}

/// An agent's YAML frontmatter. Each claude `BaseAgentDefinition` key is read
/// as an untyped `serde_yaml::Value` (matching claude reading
/// `frontmatter['...']` directly off the parsed object) so the bespoke claude
/// coercions can be applied per-field rather than relying on serde.
#[derive(Debug, Default, Deserialize)]
struct Frontmatter {
    #[serde(default)]
    name: Option<serde_yaml::Value>,
    #[serde(default)]
    description: Option<serde_yaml::Value>,
    #[serde(default)]
    tools: Option<serde_yaml::Value>,
    #[serde(default)]
    model: Option<serde_yaml::Value>,
    #[serde(default, rename = "disallowedTools")]
    disallowed_tools: Option<serde_yaml::Value>,
    #[serde(default)]
    skills: Option<serde_yaml::Value>,
    #[serde(default, rename = "mcpServers")]
    mcp_servers: Option<serde_yaml::Value>,
    #[serde(default)]
    hooks: Option<serde_yaml::Value>,
    #[serde(default, rename = "maxTurns")]
    max_turns: Option<serde_yaml::Value>,
    #[serde(default)]
    background: Option<serde_yaml::Value>,
    #[serde(default)]
    memory: Option<serde_yaml::Value>,
    #[serde(default)]
    isolation: Option<serde_yaml::Value>,
    #[serde(default, rename = "permissionMode")]
    permission_mode: Option<serde_yaml::Value>,
    #[serde(default)]
    effort: Option<serde_yaml::Value>,
    #[serde(default)]
    color: Option<serde_yaml::Value>,
    #[serde(default, rename = "initialPrompt")]
    initial_prompt: Option<serde_yaml::Value>,
    #[serde(default)]
    experimental: Option<serde_yaml::Value>,
    #[serde(default)]
    observer: Option<serde_yaml::Value>,
    #[serde(default, rename = "observerMessage")]
    observer_message: Option<serde_yaml::Value>,
    #[serde(default, rename = "observeSubagents")]
    observe_subagents: Option<serde_yaml::Value>,
}

/// claude 2.1.220 `UVe`: sanitize a path/name for log display — replace
/// control (`\p{Cc}`) and format (`\p{Cf}`) characters with a space, collapse
/// whitespace runs to one space, trim, and cap at 200 characters (JS
/// `slice(0,200)` counts UTF-16 units; a `chars` cap is the closest faithful
/// equivalent for non-astral input).
fn sanitize_for_display(raw: &str) -> String {
    /// Unicode `Cf` (format) — `char::is_control` only covers `Cc`, so the
    /// `\p{Cf}` half of the `UVe` character class is spelled out (Unicode 15
    /// Cf set).
    fn is_format_char(c: char) -> bool {
        matches!(
            c,
            '\u{00AD}'
                | '\u{0600}'..='\u{0605}'
                | '\u{061C}'
                | '\u{06DD}'
                | '\u{070F}'
                | '\u{0890}'..='\u{0891}'
                | '\u{08E2}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{110BD}'
                | '\u{110CD}'
                | '\u{13430}'..='\u{1343F}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0001}'
                | '\u{E0020}'..='\u{E007F}'
        )
    }
    let replaced: String = raw
        .chars()
        .map(|c| {
            if c.is_control() || is_format_char(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let mut out = String::with_capacity(replaced.len());
    let mut in_ws = false;
    for c in replaced.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out.trim().chars().take(200).collect()
}

/// Parse a single agent markdown buffer.
///
/// `source` and `base_dir` are supplied by the caller (so the loader can
/// tag files coming from `~/.lingxi/agents/` differently from project
/// files). `path_for_error` is used purely for error messages.
///
/// Frontmatter format: leading `---\n…\n---\n` (followed by the markdown
/// body). The body is stored as `system_prompt`.
pub fn parse_agent_markdown(
    raw: &str,
    source: AgentSource,
    base_dir: PathBuf,
    path_for_error: &Path,
) -> Result<AgentDefinition, AgentLoadError> {
    let raw = raw.strip_prefix(UTF8_BOM).unwrap_or(raw);
    let rest = raw
        .strip_prefix("---")
        .ok_or_else(|| AgentLoadError::NoFrontmatter(path_for_error.to_path_buf()))?;
    // Accept either `\n---\n` or `\n---` at EOF.
    let (yaml, body) = if let Some(idx) = rest.find("\n---\n") {
        (&rest[..idx], rest[idx + 5..].trim_start().to_string())
    } else if let Some(idx) = rest.find("\n---") {
        // EOF terminator (no trailing newline after the closing ---).
        let after = idx + 4;
        let body = if after >= rest.len() {
            String::new()
        } else {
            rest[after..].trim_start().to_string()
        };
        (&rest[..idx], body)
    } else {
        return Err(AgentLoadError::NoFrontmatter(path_for_error.to_path_buf()));
    };

    let fm: Frontmatter = serde_yaml::from_str(yaml).map_err(|e| AgentLoadError::Yaml {
        path: path_for_error.to_path_buf(),
        source: e,
    })?;

    // (1) `name` required: missing/non-string/EMPTY -> silent skip. claude's
    // `!agentType` truthiness treats the empty string `""` as falsy, so an
    // empty `name` is dropped exactly like an absent one (likely co-located
    // reference documentation, not an agent attempt).
    let Some(agent_type) = fm
        .name
        .as_ref()
        .and_then(yaml_as_string)
        .filter(|s| !s.is_empty())
    else {
        return Err(AgentLoadError::MissingName(path_for_error.to_path_buf()));
    };

    // (1b) Name validation (claude `iju` @231502134, order preserved): a
    // leading `-` (long-standing check), then a `:` anywhere in the
    // NFKC-normalized name (added in cc 2.1.218 — `:` is the plugin-namespace
    // delimiter, so a raw name carrying one would collide with the
    // `plugin:agent` types built by the plugin manager; NFKC also catches
    // look-alikes such as U+FF1A `：` and U+FE55). Order matters: a name that
    // is both dash-led and colon-bearing reports the DASH problem, because
    // that check runs first. Both log the byte-exact claude error line — and
    // claude runs BOTH the path and the name through the `UVe` display
    // sanitizer (`Agent file ${UVe(e)} has invalid name '${UVe(i)}': …`) —
    // then drop ONLY this agent.
    if agent_type.starts_with('-') {
        tracing::error!(
            "Agent file {} has invalid name '{}': names must not start with '-'",
            sanitize_for_display(&path_for_error.display().to_string()),
            sanitize_for_display(&agent_type)
        );
        return Err(AgentLoadError::InvalidName(path_for_error.to_path_buf()));
    }
    {
        use unicode_normalization::UnicodeNormalization;
        let nfkc: String = agent_type.nfkc().collect();
        if nfkc.contains(':') {
            tracing::error!(
                "Agent file {} has invalid name '{}': names must not contain ':' \
(reserved for plugin namespacing)",
                sanitize_for_display(&path_for_error.display().to_string()),
                sanitize_for_display(&agent_type)
            );
            return Err(AgentLoadError::InvalidName(path_for_error.to_path_buf()));
        }
    }

    // (2) `description` required: missing/non-string/EMPTY -> log + skip. claude
    // `!whenToUse` is falsy for `""`, so an empty description drops the agent.
    let Some(mut when_to_use) = fm
        .description
        .as_ref()
        .and_then(yaml_as_string)
        .filter(|s| !s.is_empty())
    else {
        tracing::debug!(
            path = %path_for_error.display(),
            "Agent file is missing required 'description' in frontmatter"
        );
        return Err(AgentLoadError::MissingDescription(
            path_for_error.to_path_buf(),
        ));
    };

    // (3) Unescape `\n` -> newline in description (claude
    // `whenToUse.replace(/\\n/g, '\n')`).
    when_to_use = when_to_use.replace("\\n", "\n");

    // (15) color: validate against AGENT_COLORS; only set if string && in set.
    let color = fm
        .color
        .as_ref()
        .and_then(yaml_as_string)
        .filter(|c| AGENT_COLOR_NAMES.contains(&c.as_str()));

    // (4) model: trim; case-insensitive 'inherit' -> Inherit; else Alias.
    let model = match fm.model.as_ref().and_then(yaml_as_string) {
        Some(s) if !s.trim().is_empty() => {
            let trimmed = s.trim();
            if trimmed.eq_ignore_ascii_case("inherit") {
                AgentModel::Inherit
            } else {
                AgentModel::Alias(trimmed.to_string())
            }
        }
        _ => AgentModel::Inherit,
    };

    // (5) background: true only for literal `true`/`"true"`; log on other
    // non-bool values; default false.
    let background = parse_background(fm.background.as_ref(), path_for_error);

    // (6) memory: validate against VALID_MEMORY_SCOPES; invalid -> log + None.
    let memory = parse_memory(fm.memory.as_ref(), path_for_error);

    // (7) isolation: valid set is ['worktree'] (3P) / ['worktree','remote']
    // (ant); invalid -> log + None.
    let isolation = parse_isolation(fm.isolation.as_ref(), path_for_error);

    // (8) effort: parse_effort_value; raw present but None -> log invalid.
    let effort = match fm.effort.as_ref() {
        Some(raw) => {
            let parsed = parse_effort_value(raw);
            if parsed.is_none() {
                tracing::debug!(
                    path = %path_for_error.display(),
                    "Agent file has invalid effort. Valid options: {} or an integer",
                    EFFORT_LEVELS.join(", ")
                );
            }
            parsed
        }
        None => None,
    };

    // (9) permissionMode: validate against claude PERMISSION_MODES; invalid ->
    // log. Map the valid raw string onto the (lossy) AgentPermissionMode enum:
    // only 'plan' has a behavioral analog (Plan); the others collapse to Bubble.
    let permission_mode = parse_permission_mode(fm.permission_mode.as_ref(), path_for_error);

    // (10) maxTurns: positive int; invalid -> log; absent -> keep default 100.
    let max_turns = match fm.max_turns.as_ref() {
        Some(raw) => match parse_positive_int_from_frontmatter(raw) {
            Some(n) => n,
            None => {
                tracing::debug!(
                    path = %path_for_error.display(),
                    "Agent file has invalid maxTurns. Must be a positive integer."
                );
                100
            }
        },
        None => 100,
    };

    // (11) tools: All-vs-Explicit per claude's coercion. Auto-memory tool
    // injection (Write/Edit/Read when `memory:` is set) is applied at spawn by
    // `AgentToolResolver::resolve`, not here — this is the parse layer.
    let tools_vec = parse_agent_tools_from_frontmatter(fm.tools.as_ref());
    let (tools_policy, allowed_tools) = match &tools_vec {
        // undefined = all tools
        None => (
            AgentToolPolicy::All {
                use_exact_tools: false,
            },
            Vec::new(),
        ),
        Some(list) => (AgentToolPolicy::Explicit(list.clone()), list.clone()),
    };

    // (12) disallowedTools: tool-list coercion -> Vec (None -> empty).
    let disallowed_tools = if fm.disallowed_tools.is_some() {
        parse_agent_tools_from_frontmatter(fm.disallowed_tools.as_ref()).unwrap_or_default()
    } else {
        Vec::new()
    };

    // (13) skills: comma/list coercion (missing -> []).
    let skills = parse_slash_command_tools_from_frontmatter(fm.skills.as_ref());

    // (14) initialPrompt: keep the UNtrimmed raw value when its trim is non-empty.
    let initial_prompt = fm
        .initial_prompt
        .as_ref()
        .and_then(yaml_as_string)
        .filter(|s| !s.trim().is_empty());

    // (16) mcpServers: parse each array item as ByName/Record; invalid -> log + skip.
    let mcp_servers = parse_mcp_servers(fm.mcp_servers.as_ref(), path_for_error);

    // (17) hooks: parse via the hooks crate (HookSource::FrontMatter).
    let frontmatter_hooks = parse_hooks_from_frontmatter(fm.hooks.as_ref(), &agent_type);
    let observer = parse_observer_frontmatter(
        fm.observer.as_ref(),
        fm.observer_message.as_ref(),
        fm.observe_subagents.as_ref(),
        &agent_type,
    );

    let system_prompt = body.trim().to_string();

    Ok(AgentDefinition {
        agent_type,
        when_to_use,
        tools: tools_policy,
        max_turns,
        model,
        permission_mode,
        source,
        base_dir,
        system_prompt: if system_prompt.is_empty() {
            None
        } else {
            Some(system_prompt)
        },
        mcp_servers,
        frontmatter_hooks,
        icon: None,
        allowed_tools,
        worktree_requirement: None,
        disallowed_tools,
        skills,
        // claude does NOT parse requiredMcpServers from frontmatter.
        required_mcp_servers: Vec::new(),
        background,
        isolation,
        memory,
        effort,
        initial_prompt,
        color,
        observer,
        cache_ttl: parse_cache_ttl_yaml(fm.experimental.as_ref()),
    })
}

/// `experimental.cacheTtl` from YAML frontmatter.
///
/// Upstream's schema is `.loose()` — unknown keys inside `experimental` are
/// tolerated — so an unparseable `cacheTtl` yields `None` (env/setting decides)
/// rather than dropping the whole agent. Both the documented `cacheTtl` spelling
/// and upstream's normalized `cachettl` are accepted.
fn parse_cache_ttl_yaml(experimental: Option<&serde_yaml::Value>) -> Option<AgentCacheTtl> {
    let mapping = experimental?.as_mapping()?;
    for key in ["cacheTtl", "cachettl"] {
        if let Some(raw) = mapping
            .get(serde_yaml::Value::String(key.to_string()))
            .and_then(serde_yaml::Value::as_str)
        {
            return AgentCacheTtl::parse(raw);
        }
    }
    None
}

/// `experimental.cacheTtl` from a JSON agent definition. Same tolerance.
fn parse_cache_ttl_json(experimental: Option<&serde_json::Value>) -> Option<AgentCacheTtl> {
    let object = experimental?.as_object()?;
    for key in ["cacheTtl", "cachettl"] {
        if let Some(raw) = object.get(key).and_then(serde_json::Value::as_str) {
            return AgentCacheTtl::parse(raw);
        }
    }
    None
}

fn parse_observer_frontmatter(
    observer: Option<&serde_yaml::Value>,
    message: Option<&serde_yaml::Value>,
    observe_subagents: Option<&serde_yaml::Value>,
    owner: &str,
) -> Option<ObserverSpec> {
    let Some(agent) = observer
        .and_then(yaml_as_string)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        if observer.is_some() {
            tracing::warn!(
                "[agentObserver] Agent {owner} has invalid observer; expected a non-empty agent name"
            );
        }
        return None;
    };
    let message = match message {
        None | Some(serde_yaml::Value::Null) => None,
        Some(value) => match yaml_as_string(value) {
            Some(raw) if !raw.trim().is_empty() => Some(raw),
            Some(_) => None,
            None => {
                tracing::warn!(
                    "[agentObserver] Agent {owner} has invalid observerMessage; ignoring it"
                );
                None
            }
        },
    };
    let observe_subagents = match observe_subagents {
        None | Some(serde_yaml::Value::Null) => true,
        Some(serde_yaml::Value::Bool(value)) => *value,
        Some(_) => {
            tracing::warn!(
                "[agentObserver] Agent {owner} has invalid observeSubagents; defaulting to true"
            );
            true
        }
    };
    tracing::debug!(
        "[agentObserver] Agent {owner} declares observer {agent} (observeSubagents={observe_subagents})"
    );
    Some(ObserverSpec {
        schema_version: platform_api::subagent_spawn::OBSERVER_SCHEMA_VERSION,
        agent,
        message,
        observe_subagents,
    })
}

// ─── Frontmatter coercion helpers (claude-faithful) ──────────────────────────

/// Read a YAML value as a string if (and only if) it is a YAML string.
/// Mirrors claude's `typeof x === 'string'` guards.
fn yaml_as_string(v: &serde_yaml::Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// claude `parseToolListFromCLI`: split a tool string on top-level commas AND
/// whitespace (separators inside `(...)` are preserved), trimming each entry
/// and dropping empties. So `"Read Grep"` -> `["Read", "Grep"]` while
/// `"Bash(git:*)"` stays one token.
fn parse_tool_list_from_cli(tools: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    for tool_string in tools {
        if tool_string.is_empty() {
            continue;
        }
        let mut current = String::new();
        let mut in_parens = false;
        for ch in tool_string.chars() {
            match ch {
                '(' => {
                    in_parens = true;
                    current.push(ch);
                }
                ')' => {
                    in_parens = false;
                    current.push(ch);
                }
                ',' => {
                    if in_parens {
                        current.push(ch);
                    } else if !current.trim().is_empty() {
                        result.push(current.trim().to_string());
                        current.clear();
                    } else {
                        current.clear();
                    }
                }
                ' ' => {
                    if in_parens {
                        current.push(ch);
                    } else if !current.trim().is_empty() {
                        // Space separator (outside parens): flush the current
                        // token and start a new one (claude pushes
                        // `current.trim()` and resets).
                        result.push(current.trim().to_string());
                        current.clear();
                    }
                    // else: leading/separator space outside parens is dropped.
                }
                _ => current.push(ch),
            }
        }
        if !current.trim().is_empty() {
            result.push(current.trim().to_string());
        }
    }
    result
}

/// claude `parseToolListString`: `None` (= caller default) for missing/null;
/// `Some([])` for present-but-falsy; `Some(['*'])` when the parsed list
/// contains `'*'`; otherwise `Some(parsed)`.
fn parse_tool_list_string(value: Option<&serde_yaml::Value>) -> Option<Vec<String>> {
    match value {
        // undefined / null -> null (caller decides default)
        None | Some(serde_yaml::Value::Null) => None,
        Some(v) => {
            // Falsy values (empty string, false, 0) -> [] (no tools).
            let is_falsy = match v {
                serde_yaml::Value::String(s) => s.is_empty(),
                serde_yaml::Value::Bool(b) => !b,
                serde_yaml::Value::Number(n) => n.as_f64() == Some(0.0),
                _ => false,
            };
            if is_falsy {
                return Some(Vec::new());
            }
            let tools_array: Vec<String> = match v {
                serde_yaml::Value::String(s) => vec![s.clone()],
                serde_yaml::Value::Sequence(seq) => seq
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect(),
                _ => Vec::new(),
            };
            if tools_array.is_empty() {
                return Some(Vec::new());
            }
            let parsed = parse_tool_list_from_cli(&tools_array);
            if parsed.iter().any(|t| t == "*") {
                return Some(vec!["*".to_string()]);
            }
            Some(parsed)
        }
    }
}

/// claude `parseAgentToolsFromFrontmatter`: `None` = all tools (the field was
/// missing); `Some([])` = no tools; `'*'` -> all tools (`None`).
fn parse_agent_tools_from_frontmatter(value: Option<&serde_yaml::Value>) -> Option<Vec<String>> {
    match parse_tool_list_string(value) {
        None => {
            // For agents: undefined = all tools (None); null = no tools ([]).
            // (`value === undefined ? undefined : []`)
            value.map(|_| Vec::new())
        }
        Some(parsed) => {
            if parsed.iter().any(|t| t == "*") {
                None
            } else {
                Some(parsed)
            }
        }
    }
}

/// claude `parseSlashCommandToolsFromFrontmatter`: missing/empty -> `[]`.
fn parse_slash_command_tools_from_frontmatter(value: Option<&serde_yaml::Value>) -> Vec<String> {
    parse_tool_list_string(value).unwrap_or_default()
}

/// claude `parsePositiveIntFromFrontmatter`: number or numeric string; must be
/// a positive integer.
fn parse_positive_int_from_frontmatter(value: &serde_yaml::Value) -> Option<u32> {
    match value {
        serde_yaml::Value::Null => None,
        serde_yaml::Value::Number(n) => {
            // Number.isInteger(parsed) && parsed > 0
            n.as_i64()
                .and_then(|i| if i > 0 { u32::try_from(i).ok() } else { None })
        }
        serde_yaml::Value::String(s) => {
            // parseInt(String(value), 10); Number.isInteger && > 0
            parse_int_radix10(s).and_then(|i| if i > 0 { u32::try_from(i).ok() } else { None })
        }
        // booleans: String(true)='true' -> parseInt NaN -> None
        _ => None,
    }
}

/// claude background coercion: `true` only for literal `true`/`"true"`; log on
/// any other non-bool, non-`"false"` value; default `false`.
fn parse_background(value: Option<&serde_yaml::Value>, path: &Path) -> bool {
    let Some(v) = value else {
        return false;
    };
    let is_true_literal = matches!(v, serde_yaml::Value::Bool(true))
        || matches!(v, serde_yaml::Value::String(s) if s == "true");
    let is_false_literal = matches!(v, serde_yaml::Value::Bool(false))
        || matches!(v, serde_yaml::Value::String(s) if s == "false");
    if !is_true_literal && !is_false_literal {
        tracing::debug!(
            path = %path.display(),
            "Agent file has invalid background value. Must be 'true', 'false', or omitted."
        );
    }
    is_true_literal
}

/// claude memory coercion: validate against VALID_MEMORY_SCOPES; invalid -> log.
fn parse_memory(value: Option<&serde_yaml::Value>, path: &Path) -> Option<protocol::WritableScope> {
    let Some(raw) = value.and_then(yaml_as_string) else {
        // A non-string (or absent) value: claude reads `as string | undefined`,
        // so a non-string is treated as defined-but-invalid only when it is a
        // string. Absent / non-string -> None without logging here (claude only
        // logs when the *string* value is not in the valid set).
        return None;
    };
    match raw.as_str() {
        "user" => Some(protocol::WritableScope::User),
        "project" => Some(protocol::WritableScope::Project),
        "local" => Some(protocol::WritableScope::Local),
        _ => {
            tracing::debug!(
                path = %path.display(),
                "Agent file has invalid memory value. Valid options: {}",
                VALID_MEMORY_SCOPES.join(", ")
            );
            None
        }
    }
}

/// claude isolation coercion: valid set ['worktree'] (3P) / ['worktree','remote']
/// (ant); invalid -> log.
fn parse_isolation(value: Option<&serde_yaml::Value>, path: &Path) -> Option<AgentIsolation> {
    let Some(raw) = value.and_then(yaml_as_string) else {
        return None;
    };
    let ant = std::env::var("USER_TYPE").as_deref() == Ok("ant");
    match raw.as_str() {
        "worktree" => Some(AgentIsolation::Worktree),
        "remote" if ant => Some(AgentIsolation::Remote),
        _ => {
            let valid = if ant { "worktree, remote" } else { "worktree" };
            tracing::debug!(
                path = %path.display(),
                "Agent file has invalid isolation value. Valid options: {valid}"
            );
            None
        }
    }
}

/// claude permissionMode coercion: validate against PERMISSION_MODES; invalid
/// -> log. Preserve the valid raw string as the corresponding
/// [`AgentPermissionMode`] so both subagent spawns and main-thread `--agent`
/// sessions can apply the oracle's permission-mode precedence.
fn parse_permission_mode(value: Option<&serde_yaml::Value>, path: &Path) -> AgentPermissionMode {
    let Some(raw) = value.and_then(yaml_as_string) else {
        return AgentPermissionMode::Bubble;
    };
    if !PERMISSION_MODES.contains(&raw.as_str()) {
        tracing::debug!(
            path = %path.display(),
            "Agent file has invalid permissionMode '{raw}'. Valid options: {}",
            PERMISSION_MODES.join(", ")
        );
        return AgentPermissionMode::Bubble;
    }
    match raw.as_str() {
        "default" => AgentPermissionMode::Default,
        "acceptEdits" => AgentPermissionMode::AcceptEdits,
        "auto" => AgentPermissionMode::Auto,
        "dontAsk" => AgentPermissionMode::DontAsk,
        "bypassPermissions" => AgentPermissionMode::BypassPermissions,
        "plan" => AgentPermissionMode::Plan,
        _ => AgentPermissionMode::Bubble,
    }
}

/// claude mcpServers coercion: each array item is either a string (ByName) or
/// an inline `{ name: config }` map (Record); invalid items are logged +
/// skipped. Non-array input contributes nothing.
fn parse_mcp_servers(value: Option<&serde_yaml::Value>, path: &Path) -> Vec<AgentMcpServerSpec> {
    let Some(serde_yaml::Value::Sequence(seq)) = value else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in seq {
        match parse_mcp_server_spec(item) {
            Some(spec) => out.push(spec),
            None => {
                tracing::debug!(
                    path = %path.display(),
                    "Agent file has invalid mcpServers item; skipping"
                );
            }
        }
    }
    out
}

/// Parse one `AgentMcpServerSpec` item: a string -> `ByName`; a mapping ->
/// `Record` (kept RAW — the body is validated and converted by
/// [`crate::mcp_servers::agent_mcp_specs_to_scoped_configs`], which also
/// rejects records with more than one key, exactly like claude `obs`).
fn parse_mcp_server_spec(item: &serde_yaml::Value) -> Option<AgentMcpServerSpec> {
    match item {
        serde_yaml::Value::String(s) => Some(AgentMcpServerSpec::ByName(s.clone())),
        serde_yaml::Value::Mapping(_) => {
            // YAML mapping → JSON record (non-string keys / YAML tags fail the
            // conversion → item skipped, like a zod safeParse failure).
            let json: serde_json::Value = serde_yaml::from_value(item.clone()).ok()?;
            let serde_json::Value::Object(map) = json else {
                return None;
            };
            Some(AgentMcpServerSpec::Record(map))
        }
        _ => None,
    }
}

/// claude `parseHooksFromFrontmatter`: validate `frontmatter.hooks` and on
/// failure log + drop (empty). Reuses the hooks crate's settings-JSON loader by
/// wrapping the value as `{"hooks": <value>}`.
fn parse_hooks_from_frontmatter(
    value: Option<&serde_yaml::Value>,
    agent_type: &str,
) -> Vec<hooks::HookDefinition> {
    let Some(hooks_value) = value else {
        return Vec::new();
    };
    // Convert the untyped YAML value to JSON (can fail on non-string map keys
    // or YAML tags — guard + drop like claude's safeParse).
    let json_value: serde_json::Value = match serde_yaml::from_value(hooks_value.clone()) {
        Ok(jv) => jv,
        Err(e) => {
            tracing::debug!("Invalid hooks in agent '{agent_type}': {e}");
            return Vec::new();
        }
    };
    let wrapper = serde_json::json!({ "hooks": json_value });
    let raw = match serde_json::to_string(&wrapper) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("Invalid hooks in agent '{agent_type}': {e}");
            return Vec::new();
        }
    };
    match hooks::parse_hooks_from_settings_json(&raw, hooks::HookSource::FrontMatter) {
        Ok(defs) => defs,
        Err(e) => {
            tracing::debug!("Invalid hooks in agent '{agent_type}': {e}");
            Vec::new()
        }
    }
}

/// Parse an agent definition from JSON data (claude `parseAgentFromJson`,
/// loadAgentsDir.ts:445-516). JSON agents use `prompt` for the system-prompt
/// claude's `z.number().int()` (`Number.isInteger`): a JSON number is a valid
/// integer iff it has no fractional part. JS has no int/float split, so `7.0`
/// is accepted exactly like `7`; `7.5` and non-finite values are rejected.
/// serde_json *does* distinguish `7` from `7.0`, so we accept both an `i64` and
/// an integer-valued finite `f64`.
fn json_int_value(n: &serde_json::Number) -> Option<i64> {
    if let Some(i) = n.as_i64() {
        return Some(i);
    }
    n.as_f64()
        .filter(|f| f.is_finite() && f.fract() == 0.0)
        .map(|f| f as i64)
}

/// body (NOT markdown content). Returns `None` and logs on any validation
/// failure (required `description`/`prompt` non-empty). `source` defaults to
/// [`AgentSource::Flag`] in claude (`flagSettings`).
#[must_use]
pub fn parse_agent_from_json(
    name: &str,
    definition: &serde_json::Value,
    source: AgentSource,
) -> Option<AgentDefinition> {
    let obj = match definition.as_object() {
        Some(o) => o,
        None => {
            tracing::debug!("Error parsing agent '{name}' from JSON: definition is not an object");
            return None;
        }
    };

    // claude validates JSON agents with a THROWING zod schema
    // (`AgentJsonSchema().parse(definition)` in parseAgentFromJson): ANY invalid
    // field throws and the WHOLE agent is dropped (the catch returns null). So
    // unlike the lenient markdown path, every optional field below validates
    // STRICTLY and `return None` (= zod throw) on a bad value.

    // Required: description (`z.string().min(1)`).
    let when_to_use = match obj.get("description").and_then(json_as_string) {
        Some(d) if !d.is_empty() => d,
        _ => {
            tracing::debug!("Error parsing agent '{name}' from JSON: Description cannot be empty");
            return None;
        }
    };

    // Required: prompt (`z.string().min(1)`) -> system_prompt.
    let system_prompt = match obj.get("prompt").and_then(json_as_string) {
        Some(p) if !p.is_empty() => p,
        _ => {
            tracing::debug!("Error parsing agent '{name}' from JSON: Prompt cannot be empty");
            return None;
        }
    };

    // tools / disallowedTools / skills: `z.array(z.string()).optional()` —
    // absent is fine; otherwise it MUST be an array of strings. A non-array (or
    // an array with a non-string element) throws -> drop the whole agent.
    let tools_field = match json_string_array_strict(obj.get("tools")) {
        Ok(v) => v,
        Err(()) => {
            tracing::debug!(
                "Error parsing agent '{name}' from JSON: tools must be an array of strings"
            );
            return None;
        }
    };
    // The validated (string-array) value still flows through the same tool-list
    // coercion as the markdown path.
    let tools_yaml = tools_field.map(json_string_array_to_yaml);
    let tools_vec = parse_agent_tools_from_frontmatter(tools_yaml.as_ref());
    let (tools_policy, allowed_tools) = match &tools_vec {
        None => (
            AgentToolPolicy::All {
                use_exact_tools: false,
            },
            Vec::new(),
        ),
        Some(list) => (AgentToolPolicy::Explicit(list.clone()), list.clone()),
    };

    let disallowed_tools = match json_string_array_strict(obj.get("disallowedTools")) {
        Ok(Some(arr)) => {
            let yv = json_string_array_to_yaml(arr);
            parse_agent_tools_from_frontmatter(Some(&yv)).unwrap_or_default()
        }
        Ok(None) => Vec::new(),
        Err(()) => {
            tracing::debug!(
                "Error parsing agent '{name}' from JSON: disallowedTools must be an array of strings"
            );
            return None;
        }
    };

    let skills = match json_string_array_strict(obj.get("skills")) {
        Ok(Some(arr)) => {
            parse_slash_command_tools_from_frontmatter(Some(&json_string_array_to_yaml(arr)))
        }
        Ok(None) => Vec::new(),
        Err(()) => {
            tracing::debug!(
                "Error parsing agent '{name}' from JSON: skills must be an array of strings"
            );
            return None;
        }
    };

    // model: `z.string().trim().min(1).optional()` — absent is fine; present
    // must be a string whose TRIMMED form is non-empty (whitespace-only throws).
    let model = match obj.get("model") {
        None | Some(serde_json::Value::Null) => AgentModel::Inherit,
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => {
            let trimmed = s.trim();
            if trimmed.eq_ignore_ascii_case("inherit") {
                AgentModel::Inherit
            } else {
                AgentModel::Alias(trimmed.to_string())
            }
        }
        _ => {
            tracing::debug!("Error parsing agent '{name}' from JSON: Model cannot be empty");
            return None;
        }
    };

    // effort: `z.union([z.enum(EFFORT_LEVELS), z.number().int()])` — a STRICT
    // union. Only an exact level string (case-sensitive) or an integer number
    // is accepted; numeric strings like "7" and fractional floats throw. An
    // integer-valued float like `7.0` IS accepted (`Number.isInteger`).
    let effort = match obj.get("effort") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) if EFFORT_LEVELS.contains(&s.as_str()) => {
            Some(AgentEffort::Level(s.clone()))
        }
        Some(serde_json::Value::Number(n)) if json_int_value(n).is_some() => {
            Some(AgentEffort::Numeric(json_int_value(n).unwrap()))
        }
        _ => {
            tracing::debug!("Error parsing agent '{name}' from JSON: invalid effort");
            return None;
        }
    };

    // permissionMode: `z.enum(PERMISSION_MODES).optional()`.
    let permission_mode = match obj.get("permissionMode") {
        None | Some(serde_json::Value::Null) => AgentPermissionMode::Bubble,
        Some(serde_json::Value::String(raw)) if PERMISSION_MODES.contains(&raw.as_str()) => {
            match raw.as_str() {
                "default" => AgentPermissionMode::Default,
                "acceptEdits" => AgentPermissionMode::AcceptEdits,
                "auto" => AgentPermissionMode::Auto,
                "dontAsk" => AgentPermissionMode::DontAsk,
                "bypassPermissions" => AgentPermissionMode::BypassPermissions,
                "plan" => AgentPermissionMode::Plan,
                _ => AgentPermissionMode::Bubble,
            }
        }
        _ => {
            tracing::debug!("Error parsing agent '{name}' from JSON: invalid permissionMode");
            return None;
        }
    };

    // mcpServers: `z.array(AgentMcpServerSpecSchema()).optional()` — absent is
    // fine; otherwise an array where EVERY item is a valid spec (string, or a
    // record of name->McpServerConfig). Any invalid item throws -> drop agent.
    let mcp_servers = match parse_mcp_servers_json_strict(obj.get("mcpServers")) {
        Ok(v) => v,
        Err(()) => {
            tracing::debug!("Error parsing agent '{name}' from JSON: invalid mcpServers");
            return None;
        }
    };

    // hooks: `HooksSchema().optional()` — invalid hooks throw -> drop agent.
    let frontmatter_hooks = match obj.get("hooks") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(v) => match parse_hooks_from_frontmatter_strict(&json_to_yaml(v), name) {
            Some(h) => h,
            None => {
                tracing::debug!("Error parsing agent '{name}' from JSON: invalid hooks");
                return None;
            }
        },
    };

    // maxTurns: `z.number().int().positive().optional()` — absent is fine;
    // otherwise must be a positive integer NUMBER (numeric strings throw).
    let max_turns = match obj.get("maxTurns") {
        None | Some(serde_json::Value::Null) => 100,
        Some(serde_json::Value::Number(n)) => match json_int_value(n) {
            Some(i) if i > 0 => match u32::try_from(i) {
                Ok(v) => v,
                Err(_) => {
                    tracing::debug!("Error parsing agent '{name}' from JSON: invalid maxTurns");
                    return None;
                }
            },
            _ => {
                tracing::debug!("Error parsing agent '{name}' from JSON: invalid maxTurns");
                return None;
            }
        },
        Some(_) => {
            tracing::debug!("Error parsing agent '{name}' from JSON: invalid maxTurns");
            return None;
        }
    };

    // initialPrompt (raw, when trim non-empty). `z.string().optional()` — any
    // non-string throws.
    let initial_prompt = match obj.get("initialPrompt") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => {
            if s.trim().is_empty() {
                None
            } else {
                Some(s.clone())
            }
        }
        Some(_) => {
            tracing::debug!("Error parsing agent '{name}' from JSON: invalid initialPrompt");
            return None;
        }
    };

    // memory: `z.enum(['user','project','local']).optional()`.
    let memory = match obj.get("memory") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => match s.as_str() {
            "user" => Some(protocol::WritableScope::User),
            "project" => Some(protocol::WritableScope::Project),
            "local" => Some(protocol::WritableScope::Local),
            _ => {
                tracing::debug!("Error parsing agent '{name}' from JSON: invalid memory");
                return None;
            }
        },
        Some(_) => {
            tracing::debug!("Error parsing agent '{name}' from JSON: invalid memory");
            return None;
        }
    };

    // background: `z.boolean().optional()` — only a JSON boolean is accepted.
    let background = match obj.get("background") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(_) => {
            tracing::debug!("Error parsing agent '{name}' from JSON: invalid background");
            return None;
        }
    };

    // isolation: `z.enum(['worktree','remote'])`. Availability of the remote
    // runner is checked later; the public JSON schema accepts both values.
    let isolation = match obj.get("isolation") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => match s.as_str() {
            "worktree" => Some(AgentIsolation::Worktree),
            "remote" => Some(AgentIsolation::Remote),
            _ => {
                tracing::debug!("Error parsing agent '{name}' from JSON: invalid isolation");
                return None;
            }
        },
        Some(_) => {
            tracing::debug!("Error parsing agent '{name}' from JSON: invalid isolation");
            return None;
        }
    };

    // observer / observerMessage / observeSubagents are independently optional
    // schema fields. Message/fanout values without an observer are valid and
    // simply have no runtime effect; present values with the wrong type still
    // drop the whole definition.
    let observer = match obj.get("observer") {
        None | Some(serde_json::Value::Null) => {
            if !matches!(
                obj.get("observerMessage"),
                None | Some(serde_json::Value::Null | serde_json::Value::String(_))
            ) {
                tracing::debug!("Error parsing agent '{name}' from JSON: invalid observerMessage");
                return None;
            }
            if !matches!(
                obj.get("observeSubagents"),
                None | Some(serde_json::Value::Null | serde_json::Value::Bool(_))
            ) {
                tracing::debug!("Error parsing agent '{name}' from JSON: invalid observeSubagents");
                return None;
            }
            None
        }
        Some(serde_json::Value::String(agent)) if !agent.trim().is_empty() => {
            let message = match obj.get("observerMessage") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(value)) => {
                    (!value.trim().is_empty()).then(|| value.clone())
                }
                Some(_) => {
                    tracing::debug!(
                        "Error parsing agent '{name}' from JSON: invalid observerMessage"
                    );
                    return None;
                }
            };
            let observe_subagents = match obj.get("observeSubagents") {
                None | Some(serde_json::Value::Null) => true,
                Some(serde_json::Value::Bool(value)) => *value,
                Some(_) => {
                    tracing::debug!(
                        "Error parsing agent '{name}' from JSON: invalid observeSubagents"
                    );
                    return None;
                }
            };
            Some(ObserverSpec {
                schema_version: platform_api::subagent_spawn::OBSERVER_SCHEMA_VERSION,
                agent: agent.trim().to_string(),
                message,
                observe_subagents,
            })
        }
        Some(_) => {
            tracing::debug!("Error parsing agent '{name}' from JSON: invalid observer");
            return None;
        }
    };

    Some(AgentDefinition {
        agent_type: name.to_string(),
        when_to_use,
        tools: tools_policy,
        max_turns,
        model,
        permission_mode,
        source,
        base_dir: PathBuf::new(),
        system_prompt: Some(system_prompt),
        mcp_servers,
        frontmatter_hooks,
        icon: None,
        allowed_tools,
        worktree_requirement: None,
        disallowed_tools,
        skills,
        required_mcp_servers: Vec::new(),
        background,
        isolation,
        memory,
        effort,
        initial_prompt,
        color: None,
        observer,
        cache_ttl: parse_cache_ttl_json(obj.get("experimental")),
    })
}

/// Parse multiple agents from a `{ name: definition }` JSON object (claude
/// `parseAgentsFromJson`, loadAgentsDir.ts:521-536). Non-object input -> `[]`
/// (+ log). Entries that fail validation are filtered out.
#[must_use]
pub fn parse_agents_from_json(
    agents_json: &serde_json::Value,
    source: AgentSource,
) -> Vec<AgentDefinition> {
    let Some(obj) = agents_json.as_object() else {
        tracing::debug!("Error parsing agents from JSON: top-level value is not an object");
        return Vec::new();
    };
    obj.iter()
        .filter_map(|(name, def)| parse_agent_from_json(name, def, source))
        .collect()
}

/// Return Claude/Zod's JSON type label for an input value.
fn json_input_type(value: Option<&serde_json::Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(serde_json::Value::Null) => "null",
        Some(serde_json::Value::Bool(_)) => "boolean",
        Some(serde_json::Value::Number(_)) => "number",
        Some(serde_json::Value::String(_)) => "string",
        Some(serde_json::Value::Array(_)) => "array",
        Some(serde_json::Value::Object(_)) => "object",
    }
}

fn invalid_json_type(path: &str, expected: &str, value: Option<&serde_json::Value>) -> String {
    format!(
        "{path}: Invalid input: expected {expected}, received {}",
        json_input_type(value)
    )
}

fn validate_string_array(
    obj: &serde_json::Map<String, serde_json::Value>,
    agent: &str,
    field: &str,
) -> Result<(), String> {
    let Some(value) = obj.get(field) else {
        return Ok(());
    };
    let path = format!("{agent}.{field}");
    let serde_json::Value::Array(items) = value else {
        return Err(invalid_json_type(&path, "array", Some(value)));
    };
    for (index, item) in items.iter().enumerate() {
        if !item.is_string() {
            return Err(invalid_json_type(
                &format!("{path}.{index}"),
                "string",
                Some(item),
            ));
        }
    }
    Ok(())
}

fn validate_mcp_servers_schema(value: &serde_json::Value, agent: &str) -> Result<(), String> {
    let path = format!("{agent}.mcpServers");
    let serde_json::Value::Array(items) = value else {
        return Err(invalid_json_type(&path, "array", Some(value)));
    };
    for (index, item) in items.iter().enumerate() {
        match item {
            serde_json::Value::String(_) => {}
            serde_json::Value::Object(entries) => {
                if entries
                    .values()
                    .any(|entry| !mcp::server_entry_shape_is_valid(entry))
                {
                    return Err(format!("{path}.{index}: Invalid input"));
                }
            }
            _ => return Err(format!("{path}.{index}: Invalid input")),
        }
    }
    Ok(())
}

fn validate_hooks_schema(value: &serde_json::Value, agent: &str) -> Result<(), String> {
    let path = format!("{agent}.hooks");
    let serde_json::Value::Object(events) = value else {
        return Err(invalid_json_type(&path, "record", Some(value)));
    };
    for (event, groups) in events {
        let event_path = format!("{path}.{event}");
        let serde_json::Value::Array(groups) = groups else {
            return Err(invalid_json_type(&event_path, "array", Some(groups)));
        };
        for (index, group) in groups.iter().enumerate() {
            let group_path = format!("{event_path}.{index}");
            let Some(group) = group.as_object() else {
                return Err(invalid_json_type(&group_path, "object", Some(group)));
            };
            if let Some(matcher) = group.get("matcher") {
                if !matcher.is_string() {
                    return Err(invalid_json_type(
                        &format!("{group_path}.matcher"),
                        "string",
                        Some(matcher),
                    ));
                }
            }
            let hooks_path = format!("{group_path}.hooks");
            let hooks = group.get("hooks");
            let Some(serde_json::Value::Array(entries)) = hooks else {
                return Err(invalid_json_type(&hooks_path, "array", hooks));
            };
            for (hook_index, entry) in entries.iter().enumerate() {
                if !entry.is_object() {
                    return Err(invalid_json_type(
                        &format!("{hooks_path}.{hook_index}"),
                        "object",
                        Some(entry),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn validate_flag_agent_schema(name: &str, definition: &serde_json::Value) -> Result<(), String> {
    let Some(obj) = definition.as_object() else {
        return Err(invalid_json_type(name, "object", Some(definition)));
    };

    for (field, empty_message) in [
        ("description", "Description cannot be empty"),
        ("prompt", "Prompt cannot be empty"),
    ] {
        let path = format!("{name}.{field}");
        match obj.get(field) {
            Some(serde_json::Value::String(value)) if !value.is_empty() => {}
            Some(serde_json::Value::String(_)) => {
                return Err(format!("{path}: {empty_message}"));
            }
            value => return Err(invalid_json_type(&path, "string", value)),
        }
    }

    for field in ["tools", "disallowedTools", "skills"] {
        validate_string_array(obj, name, field)?;
    }

    if let Some(value) = obj.get("model") {
        let path = format!("{name}.model");
        match value {
            serde_json::Value::String(model) if !model.trim().is_empty() => {}
            serde_json::Value::String(_) => return Err(format!("{path}: Model cannot be empty")),
            other => return Err(invalid_json_type(&path, "string", Some(other))),
        }
    }

    if let Some(value) = obj.get("permissionMode") {
        let valid = value
            .as_str()
            .is_some_and(|mode| PERMISSION_MODES.contains(&mode));
        if !valid {
            return Err(format!(
                "{name}.permissionMode: Invalid option: expected one of \"acceptEdits\"|\"auto\"|\"bypassPermissions\"|\"default\"|\"dontAsk\"|\"plan\""
            ));
        }
    }

    if let Some(value) = obj.get("maxTurns") {
        let path = format!("{name}.maxTurns");
        let serde_json::Value::Number(number) = value else {
            return Err(invalid_json_type(&path, "number", Some(value)));
        };
        let Some(turns) = json_int_value(number) else {
            return Err(format!(
                "{path}: Invalid input: expected int, received number"
            ));
        };
        if turns <= 0 {
            return Err(format!("{path}: Too small: expected number to be >0"));
        }
    }

    if let Some(value) = obj.get("background") {
        if !value.is_boolean() {
            return Err(invalid_json_type(
                &format!("{name}.background"),
                "boolean",
                Some(value),
            ));
        }
    }

    for (field, values) in [
        ("memory", &["user", "project", "local"][..]),
        ("isolation", &["worktree", "remote"][..]),
    ] {
        if let Some(value) = obj.get(field) {
            let valid = value
                .as_str()
                .is_some_and(|candidate| values.contains(&candidate));
            if !valid {
                let joined = values
                    .iter()
                    .map(|value| format!("\"{value}\""))
                    .collect::<Vec<_>>()
                    .join("|");
                return Err(format!(
                    "{name}.{field}: Invalid option: expected one of {joined}"
                ));
            }
        }
    }

    if let Some(value) = obj.get("effort") {
        let valid = value
            .as_str()
            .is_some_and(|level| EFFORT_LEVELS.contains(&level))
            || value.as_number().and_then(json_int_value).is_some();
        if !valid {
            return Err(format!("{name}.effort: Invalid input"));
        }
    }

    for field in ["initialPrompt", "observer", "observerMessage"] {
        if let Some(value) = obj.get(field) {
            if !value.is_string() {
                return Err(invalid_json_type(
                    &format!("{name}.{field}"),
                    "string",
                    Some(value),
                ));
            }
        }
    }
    if let Some(value) = obj.get("observeSubagents") {
        if !value.is_boolean() {
            return Err(invalid_json_type(
                &format!("{name}.observeSubagents"),
                "boolean",
                Some(value),
            ));
        }
    }
    if let Some(value) = obj.get("mcpServers") {
        validate_mcp_servers_schema(value, name)?;
    }
    if let Some(value) = obj.get("hooks") {
        validate_hooks_schema(value, name)?;
    }
    if name.starts_with('-') {
        return Err(format!("{name}: agent names must not start with '-'"));
    }
    Ok(())
}

/// Strict `--agents <json>` parser. Claude Code 2.1.245 and earlier logged and
/// ignored schema failures; 2.1.246 rejects them before runtime construction.
/// The error payload is the line printed after `Invalid --agents configuration:`.
pub fn parse_agents_from_flag_json_checked(raw: &str) -> Result<Vec<AgentDefinition>, String> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|error| {
        let trimmed = raw.trim();
        let detail = if trimmed.starts_with('{') && !trimmed.ends_with('}') {
            "Expected '}'".to_string()
        } else if trimmed.starts_with('[') && !trimmed.ends_with(']') {
            "Expected ']'".to_string()
        } else {
            error.to_string()
        };
        format!("invalid JSON: JSON Parse error: {detail}")
    })?;
    let Some(obj) = value.as_object() else {
        return Err(format!(
            "Invalid input: expected record, received {}",
            json_input_type(Some(&value))
        ));
    };
    for (name, definition) in obj {
        validate_flag_agent_schema(name, definition)?;
    }
    let mut out = Vec::with_capacity(obj.len());
    for (name, definition) in obj {
        let parsed = parse_agent_from_json(name, definition, AgentSource::Flag)
            .ok_or_else(|| format!("{name}: Invalid input"))?;
        out.push(parsed);
    }
    Ok(out)
}

/// Parse the `--agents <json>` CLI flag payload.
///
/// The checked variant owns the 2.1.246 fatal error payload. This compatibility
/// wrapper remains for non-CLI composition roots and logs before returning an
/// empty catalog; the CLI calls the checked variant and exits before build.
#[must_use]
pub fn parse_agents_from_flag_json(raw: &str) -> Vec<AgentDefinition> {
    match parse_agents_from_flag_json_checked(raw) {
        Ok(agents) => agents,
        Err(error) => {
            tracing::error!("Invalid --agents configuration: {error}");
            Vec::new()
        }
    }
}

/// Read a JSON value as a string only when it is a JSON string.
fn json_as_string(v: &serde_json::Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// Strictly validate `z.array(z.string()).optional()`: `None` field -> `Ok(None)`;
/// an array of strings -> `Ok(Some(arr))`; a JSON `null` -> `Ok(None)` (optional);
/// a non-array OR an array containing a non-string element -> `Err(())` (zod throw).
#[allow(clippy::result_unit_err)]
fn json_string_array_strict(value: Option<&serde_json::Value>) -> Result<Option<Vec<String>>, ()> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => return Err(()),
                }
            }
            Ok(Some(out))
        }
        Some(_) => Err(()),
    }
}

/// Lift a validated `Vec<String>` into a YAML sequence so the shared tool-list
/// coercion helpers (operating on `serde_yaml::Value`) can consume it.
fn json_string_array_to_yaml(arr: Vec<String>) -> serde_yaml::Value {
    serde_yaml::Value::Sequence(arr.into_iter().map(serde_yaml::Value::String).collect())
}

/// Strictly validate `z.array(AgentMcpServerSpecSchema()).optional()` for JSON
/// agents: `None`/`null` -> `Ok([])`; strings and record entries are retained
/// for the conversion layer to validate independently. This is intentionally
/// entry-tolerant: 2.1.220 keeps a JSON agent containing `mcpServers:[{}]`,
/// warns for that entry, and skips only the entry.
///
/// A multi-key record `{ a: cfgA, b: cfgB }` is valid for zod's
/// `z.record(...)` and is kept as ONE [`AgentMcpServerSpec::Record`] — claude
/// `obs` later warns + skips it (`expected exactly one key`).
#[allow(clippy::result_unit_err)]
fn parse_mcp_servers_json_strict(
    value: Option<&serde_json::Value>,
) -> Result<Vec<AgentMcpServerSpec>, ()> {
    let arr = match value {
        None | Some(serde_json::Value::Null) => return Ok(Vec::new()),
        Some(serde_json::Value::Array(arr)) => arr,
        Some(_) => return Err(()),
    };
    let mut out = Vec::new();
    for item in arr {
        match item {
            serde_json::Value::String(s) => out.push(AgentMcpServerSpec::ByName(s.clone())),
            serde_json::Value::Object(map) => {
                if map
                    .values()
                    .any(|entry| !mcp::server_entry_shape_is_valid(entry))
                {
                    return Err(());
                }
                // Keep the raw record. Empty/multi-key records are diagnosed
                // and skipped by `agent_mcp_specs_to_scoped_configs`.
                out.push(AgentMcpServerSpec::Record(map.clone()));
            }
            _ => return Err(()),
        }
    }
    Ok(out)
}

/// Strictly validate `HooksSchema().optional()` for JSON agents: returns the
/// parsed hooks on success, or `None` when the value is invalid (claude's zod
/// throws and the whole agent is dropped — caller maps `None` -> drop).
fn parse_hooks_from_frontmatter_strict(
    value: &serde_yaml::Value,
    agent_type: &str,
) -> Option<Vec<hooks::HookDefinition>> {
    let json_value: serde_json::Value = serde_yaml::from_value(value.clone()).ok()?;
    let wrapper = serde_json::json!({ "hooks": json_value });
    let raw = serde_json::to_string(&wrapper).ok()?;
    match hooks::parse_hooks_from_settings_json(&raw, hooks::HookSource::FrontMatter) {
        Ok(defs) => Some(defs),
        Err(e) => {
            tracing::debug!("Invalid hooks in agent '{agent_type}': {e}");
            None
        }
    }
}

/// Convert a JSON value into the YAML value space so the shared frontmatter
/// coercion helpers (which operate on `serde_yaml::Value`) can be reused for
/// JSON agents.
fn json_to_yaml(v: &serde_json::Value) -> serde_yaml::Value {
    serde_yaml::to_value(v).unwrap_or(serde_yaml::Value::Null)
}

/// claude `SN(Jb(),".claude",e)` — the managed policy agent directory under the
/// OS policy root (`getManagedFilePath`).
///
/// This is `Z$`'s TOP tier: `C=[built-in, plugin, userSettings, projectSettings,
/// flagSettings, policySettings]` is applied later-wins, so an org-provisioned
/// definition outranks every other source, `--agents` included. It is therefore
/// merged AFTER the flag agents, not alongside the dir tiers.
///
/// `wQr` loads it with no `Fr(...)` / `ku("agents")` gate of its own — unlike
/// the user and project tiers — because org policy is not user customization.
/// The safe-mode / `--bare` arm still applies: `jto` returns built-ins only
/// before any of this runs.
#[must_use]
pub fn policy_agent_dir(managed_dir: &Path) -> PathBuf {
    managed_dir.join(branding::DOT_DIR).join("agents")
}

/// Fold `incoming` over `agents` with claude's later-wins tier semantics: a
/// same-named definition REPLACES the one already there, a new name appends.
///
/// The in-place replace matters — appending instead would leave two entries for
/// one `agent_type`, and every consumer downstream (`agent_listing_entries`,
/// `lookup_definition`, `/agents`) picks by name, so which one they saw would
/// depend on iteration order.
pub fn merge_agents_later_wins(agents: &mut Vec<AgentDefinition>, incoming: Vec<AgentDefinition>) {
    for def in incoming {
        if let Some(slot) = agents.iter_mut().find(|e| e.agent_type == def.agent_type) {
            *slot = def;
        } else {
            agents.push(def);
        }
    }
}

/// Load every `*.md` agent file under each path in `paths`, in order.
///
/// Files with no frontmatter or invalid YAML are logged at `warn!` and
/// skipped. On `agent_type` collision, **later paths win** — pass the
/// global path FIRST and the project path SECOND so project agents
/// override user-globals.
///
/// The returned list is sorted alphabetically by `agent_type` for stable
/// display order in `/agents`.
/// claude `wie` — the per-file byte cap `vG` applies before parsing a markdown
/// definition (1 MiB).
const MARKDOWN_FILE_MAX_BYTES: u64 = 1_048_576;

/// Every `*.md` file under `root`, RECURSIVELY.
///
/// claude's `vG(dir)` scans with
/// `rg --files --hidden --follow --no-ignore --glob "*.md"`, whose fallback
/// `TQr` is an explicit recursive walk that follows symlinks and keeps a
/// `dev:ino` visited set so a symlink loop cannot hang it. The port read the
/// TOP LEVEL only, so `<DOT_DIR>/agents/reviewers/api.md` — an ordinary way to
/// group definitions — was invisible.
///
/// The visited key here is the canonical path rather than `dev:ino`: `TQr` uses
/// `dev:ino` when the platform reports it and falls back to `realpath`
/// otherwise, and the two are interchangeable for loop detection. Following
/// symlinks is `--follow`, so entries are classified through
/// `metadata` (which resolves them), not `symlink_metadata`.
///
/// Results are sorted. claude leaves this to ripgrep's traversal order; the
/// port's caller inserts later-wins into a map, so an unsorted walk would make
/// the winner between two files declaring the same `name` depend on filesystem
/// order. Sorting pins it without changing WHICH definitions exist.
async fn collect_markdown_files(root: &Path) -> Vec<(PathBuf, String)> {
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    let mut visited: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        // Canonicalize BEFORE the visited check so two paths reaching the same
        // directory through different symlinks collapse to one entry.
        let Ok(key) = tokio::fs::canonicalize(&dir).await else {
            continue;
        };
        if !visited.insert(key) {
            continue;
        }
        // missing dir = empty contribution (`O5`/`vG`'s ENOENT arm)
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            // `--follow`: resolve through a symlink before classifying it.
            let meta = match tokio::fs::metadata(&path).await {
                Ok(m) => m,
                Err(e) => {
                    // `TQr`: `Failed to follow symlink ${C}: ${D}` — a dangling
                    // link is reported, not fatal.
                    tracing::warn!(error = %e, path = %path.display(), "failed to follow symlink");
                    continue;
                }
            };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() && path.extension().and_then(|s| s.to_str()) == Some("md") {
                if meta.len() > MARKDOWN_FILE_MAX_BYTES {
                    tracing::warn!(
                        path = %path.display(),
                        limit = MARKDOWN_FILE_MAX_BYTES,
                        "loadMarkdownFilesFromDir: skipping: not a regular file or exceeds the byte limit"
                    );
                    continue;
                }
                let identity = file_identity(&meta, &path);
                out.push((path, identity));
            }
        }
    }
    out.sort();
    out
}

/// claude `SQr(filePath)` — the identity `wQr` de-duplicates markdown files on,
/// so one file reachable through a symlink or a hard link from two of the
/// directories is loaded once.
///
/// `dev:ino` where the platform reports it, the path otherwise. `TQr` makes the
/// same substitution for its directory visited set.
#[cfg(unix)]
fn file_identity(meta: &std::fs::Metadata, path: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    let _ = path;
    format!("{}:{}", meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn file_identity(_meta: &std::fs::Metadata, path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub async fn load_agents_from_dirs(paths: &[(PathBuf, AgentSource)]) -> Vec<AgentDefinition> {
    use std::collections::HashMap;
    let mut by_name: HashMap<String, AgentDefinition> = HashMap::new();
    // `wQr` de-duplicates by inode across ALL tiers at once, keeping the FIRST
    // occurrence and logging each skip. Same inode means the same bytes, so the
    // choice only decides which `source`/`baseDir` gets recorded.
    let mut by_identity: HashMap<String, AgentSource> = HashMap::new();
    let mut deduplicated = 0usize;
    for (dir, source) in paths {
        for (p, identity) in collect_markdown_files(dir).await {
            if let Some(already) = by_identity.get(&identity) {
                tracing::debug!(
                    path = %p.display(),
                    source = ?source,
                    already_loaded_from = ?already,
                    "Skipping duplicate file (same inode already loaded)"
                );
                deduplicated += 1;
                continue;
            }
            by_identity.insert(identity, *source);
            let raw = match tokio::fs::read_to_string(&p).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        path = %p.display(),
                        "skipping unreadable agent file"
                    );
                    continue;
                }
            };
            match parse_agent_markdown(&raw, *source, dir.clone(), &p) {
                Ok(def) => {
                    by_name.insert(def.agent_type.clone(), def);
                }
                // claude SILENTLY skips files without a `name` field (likely
                // co-located reference docs, not agent attempts).
                Err(AgentLoadError::MissingName(_)) => {}
                // Invalid name (leading `-` / `:` namespacing collision): the
                // byte-exact claude error was already logged inside
                // `parse_agent_markdown` (`iju` logs then returns null) — skip
                // without double-logging.
                Err(AgentLoadError::InvalidName(_)) => {}
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        path = %p.display(),
                        "skipping malformed agent file"
                    );
                }
            }
        }
    }
    if deduplicated > 0 {
        tracing::debug!(
            count = deduplicated,
            "Deduplicated agent files (same inode via symlinks or hard links)"
        );
    }
    let mut out: Vec<AgentDefinition> = by_name.into_values().collect();
    out.sort_by(|a, b| a.agent_type.cmp(&b.agent_type));
    out
}

/// claude `O5(kind, cwd)` (src_162329786.js @1338241) — every project-scoped
/// agent directory between `cwd` and the enclosing project root:
///
/// ```js
/// let r=i9e(yQr()).normalize("NFC"),o=kQr(n),d=i9e(n),p=[];
/// while(!0){
///   if(Pf(d)===Pf(r))break;            // the home dir is the ceiling, NOT collected
///   p.push(SN(d,".claude",e));
///   if(o&&Pf(d)===Pf(o))break;         // the project root IS collected, then stop
///   let C=_Qr(d); if(C===d)break;      // filesystem root
///   d=C}
/// ```
///
/// The port used to read ONE project directory (`<cwd>/<DOT_DIR>/agents`), so
/// in a monorepo an agent defined at `packages/foo/<DOT_DIR>/agents/` was
/// invisible whenever the session's cwd was the repo root, and one defined at
/// the repo root was invisible from inside a package. Both are ordinary
/// layouts.
///
/// Returned DEEPEST first, matching `O5`. `project_root` is `kQr(cwd)` — the
/// outermost enclosing project; `None` walks to the home ceiling (or the
/// filesystem root, whichever comes first). Existence is NOT checked here:
/// [`load_agents_from_dirs`] already treats an unreadable directory as an empty
/// contribution, which is `O5`'s ENOENT arm.
#[must_use]
pub fn project_agent_dirs(cwd: &Path, home: &Path, project_root: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dir = cwd;
    loop {
        if dir == home {
            break;
        }
        out.push(dir.join(branding::DOT_DIR).join("agents"));
        if project_root.is_some_and(|root| dir == root) {
            break;
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent,
            _ => break,
        }
    }
    out
}

/// The agent directories to hand [`load_agents_from_dirs`], LOWEST priority
/// FIRST (that function is later-wins).
///
/// claude's tier map (`Z$`) is `[built-in, plugin, userSettings,
/// projectSettings, flagSettings, policySettings]`, and WITHIN the
/// projectSettings tier it sorts by `ITe` — the separator count of `baseDir`,
/// ascending — so the DEEPEST project directory wins. Emitting the walk
/// reversed (shallowest first) gives later-wins the same answer without a
/// second sort.
///
/// `additional_dirs` are the `--add-dir` roots. For `kind === "agents"` ONLY,
/// `wQr` gives each of them a second projectSettings source —
/// `<dir>/.claude/agents`, tagged `fromAdditionalDirectory: true` — minus any
/// the upward walk already covers, and `Z$` ranks those BELOW the ordinary
/// project directories:
///
/// ```js
/// let C=e==="agents"?K(await Promise.all(Rp().map(async(De)=>{
///       let Le=SN(i9e(De),".claude",e); return await s9e(Le).catch(()=>Le)})))
///     .filter((De)=>!E.has(Pf(De))):[];
/// …
/// let p=[...e.filter((D)=>D.source==="projectSettings"&&D.fromAdditionalDirectory),
///        ...e.filter((D)=>D.source==="projectSettings"&&!D.fromAdditionalDirectory).sort(ITe)];
/// ```
///
/// claude compares REALPATHS when excluding an add-dir the walk already
/// covered; this compares the joined paths. The two differ only when the same
/// directory is reached under two spellings, and then the port merely reads it
/// twice — the ordinary project entry still comes later and still wins, which
/// is the same answer claude reaches by dropping the duplicate.
///
/// ⛔ One of claude's sources is still missing: the managed policy directory
/// (`SN(Jb(),".claude",e)`, the LOWEST tier). The port has no managed-settings
/// DIRECTORY helper, only `managed-settings.json` content handling.
///
/// Answers: which agent directory wins a name clash. One of several orderings
/// over these rungs; `protocol::scope`'s module docs index them all and say
/// which question each answers.
#[must_use]
pub fn agent_dir_precedence(
    user_agents_dir: PathBuf,
    cwd: &Path,
    home: &Path,
    project_root: Option<&Path>,
    additional_dirs: &[PathBuf],
) -> Vec<(PathBuf, AgentSource)> {
    let mut project = project_agent_dirs(cwd, home, project_root);
    project.reverse();

    let mut dirs = vec![(
        user_agents_dir,
        AgentSource::Settings(protocol::SettingsScope::User),
    )];
    let mut seen_additional: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for extra in additional_dirs {
        let dir = extra.join(branding::DOT_DIR).join("agents");
        if project.contains(&dir) || !seen_additional.insert(dir.clone()) {
            continue;
        }
        dirs.push((dir, AgentSource::AdditionalDirectory));
    }
    dirs.extend(
        project
            .into_iter()
            .map(|d| (d, AgentSource::Settings(protocol::SettingsScope::Project))),
    );
    dirs
}

/// Recipient comparison used by implicit teammate name reservation.
pub fn normalize_teammate_recipient(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    fn js_whitespace(c: char) -> bool {
        matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
    }
    let normalized: String = name.nfkc().filter(|c| {
        js_whitespace(*c) || (!c.is_control() && !matches!(*c, '\u{00ad}' | '\u{0600}'..='\u{0605}' | '\u{06dd}' | '\u{070f}' | '\u{0890}'..='\u{0891}' | '\u{08e2}' | '\u{110bd}' | '\u{110cd}' | '\u{13430}'..='\u{1343f}' | '\u{1bca0}'..='\u{1bca3}' | '\u{1d173}'..='\u{1d17a}' | '\u{e0001}' | '\u{e0020}'..='\u{e007f}' | '\u{061c}' | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' | '\u{feff}'))
    }).collect();
    normalized
        .split(js_whitespace)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::AgentEffort;
    use tempfile::TempDir;

    #[test]
    fn recipient_normalizer_matches_javascript_nfkc_and_whitespace() {
        assert_eq!(normalize_teammate_recipient(" ＭＡＩＮ "), "main");
        assert_eq!(
            normalize_teammate_recipient("team\u{feff}lead"),
            "team-lead"
        );
        assert_eq!(normalize_teammate_recipient("ma\u{0085}in"), "main");
        assert_eq!(normalize_teammate_recipient("ma\u{200b}in"), "main");
    }

    fn parse_name(name: &str) -> Result<AgentDefinition, AgentLoadError> {
        let raw = format!("---\nname: \"{name}\"\ndescription: d\n---\nBody");
        parse_agent_markdown(
            &raw,
            AgentSource::Settings(protocol::SettingsScope::User),
            PathBuf::from("/tmp"),
            Path::new("a.md"),
        )
    }

    /// `:` is RESERVED for plugin namespacing (`plugin:agent`), so an agent
    /// file claiming it must be dropped — otherwise the validator and the
    /// namespace splitter disagree about where the name ends.
    #[test]
    fn a_name_containing_a_colon_is_rejected() {
        for n in ["plugin:agent", "a:b:c", ":leading", "trailing:"] {
            assert!(
                matches!(parse_name(n), Err(AgentLoadError::InvalidName(_))),
                "{n} must be rejected"
            );
        }
    }

    /// The oracle tests the NFKC-NORMALIZED name, so a Unicode colon lookalike
    /// is rejected too. Testing the raw string would let these through.
    #[test]
    fn unicode_colon_lookalikes_are_rejected_after_nfkc() {
        for n in ["plugin\u{ff1a}agent", "plugin\u{fe55}agent"] {
            assert!(
                matches!(parse_name(n), Err(AgentLoadError::InvalidName(_))),
                "{n:?} must be rejected after NFKC"
            );
        }
    }

    #[test]
    fn a_name_starting_with_a_dash_is_rejected() {
        assert!(matches!(
            parse_name("-agent"),
            Err(AgentLoadError::InvalidName(_))
        ));
    }

    /// A name that is BOTH dash-led and colon-bearing is still dropped exactly
    /// once. The oracle checks the dash FIRST, so it is the dash message that
    /// gets logged — but that ordering is observable only in the emitted log
    /// line (`iju` logs then returns `null`), not in the error value, which is
    /// the same `InvalidName` either way. This pins the rejection; the message
    /// text itself is pinned at the call site.
    #[test]
    fn dash_and_colon_together_are_rejected_once() {
        assert!(matches!(
            parse_name("-a:b"),
            Err(AgentLoadError::InvalidName(_))
        ));
    }

    #[test]
    fn ordinary_names_still_parse() {
        for n in ["reviewer", "code-architect", "test_runner", "a-b-c"] {
            assert!(parse_name(n).is_ok(), "{n} must parse");
        }
    }

    #[test]
    fn parse_minimal_frontmatter() {
        let raw = "---\nname: reviewer\ndescription: review code\n---\nBody";
        let def = parse_agent_markdown(
            raw,
            AgentSource::Settings(protocol::SettingsScope::User),
            PathBuf::from("/tmp"),
            Path::new("reviewer.md"),
        )
        .unwrap();
        assert_eq!(def.agent_type, "reviewer");
        assert_eq!(def.when_to_use, "review code");
        assert_eq!(def.system_prompt.as_deref(), Some("Body"));
    }

    #[test]
    fn utf8_bom_before_frontmatter_is_ignored() {
        let raw = "\u{feff}---\nname: reviewer\ndescription: review code\n---\nBody";
        let def = parse_agent_markdown(
            raw,
            AgentSource::Settings(protocol::SettingsScope::User),
            PathBuf::from("/tmp"),
            Path::new("reviewer.md"),
        )
        .expect("parse with bom");
        assert_eq!(def.agent_type, "reviewer");
        assert_eq!(def.when_to_use, "review code");
        assert_eq!(def.system_prompt.as_deref(), Some("Body"));
    }

    #[test]
    fn markdown_observer_defaults_to_fanout_and_preserves_message() {
        let raw = "---\nname: worker\ndescription: work\nobserver: reviewer\nobserverMessage: watch for unsupported claims\n---\nBody";
        let def = parse_agent_markdown(
            raw,
            AgentSource::Settings(protocol::SettingsScope::User),
            PathBuf::from("/tmp"),
            Path::new("worker.md"),
        )
        .unwrap();
        let observer = def.observer.expect("observer");
        assert_eq!(observer.schema_version, 1);
        assert_eq!(observer.agent, "reviewer");
        assert_eq!(
            observer.message.as_deref(),
            Some("watch for unsupported claims")
        );
        assert!(observer.observe_subagents);
    }

    #[test]
    fn markdown_observer_can_stop_descendant_fanout() {
        let raw = "---\nname: worker\ndescription: work\nobserver: reviewer\nobserveSubagents: false\n---\nBody";
        let def = parse_agent_markdown(
            raw,
            AgentSource::Settings(protocol::SettingsScope::User),
            PathBuf::from("/tmp"),
            Path::new("worker.md"),
        )
        .unwrap();
        assert!(!def.observer.expect("observer").observe_subagents);
    }

    #[test]
    fn json_observer_schema_is_strict() {
        let valid = serde_json::json!({
            "description": "work",
            "prompt": "body",
            "observer": "reviewer",
            "observerMessage": "watch",
            "observeSubagents": false
        });
        let def = parse_agent_from_json("worker", &valid, AgentSource::Flag).unwrap();
        let observer = def.observer.expect("observer");
        assert_eq!(observer.agent, "reviewer");
        assert_eq!(observer.message.as_deref(), Some("watch"));
        assert!(!observer.observe_subagents);

        for valid_without_observer in [
            serde_json::json!({"description":"d","prompt":"p","observerMessage":"watch"}),
            serde_json::json!({"description":"d","prompt":"p","observeSubagents":false}),
        ] {
            let parsed =
                parse_agent_from_json("worker", &valid_without_observer, AgentSource::Flag)
                    .expect("independently optional observer fields are schema-valid");
            assert!(parsed.observer.is_none());
        }

        for invalid in [
            serde_json::json!({"description":"d","prompt":"p","observer":true}),
            serde_json::json!({"description":"d","prompt":"p","observer":"x","observerMessage":3}),
            serde_json::json!({"description":"d","prompt":"p","observer":"x","observeSubagents":"yes"}),
            serde_json::json!({"description":"d","prompt":"p","observerMessage":3}),
        ] {
            assert!(parse_agent_from_json("worker", &invalid, AgentSource::Flag).is_none());
        }
    }

    #[test]
    fn missing_frontmatter_errors() {
        let raw = "no frontmatter here";
        let err = parse_agent_markdown(
            raw,
            AgentSource::Settings(protocol::SettingsScope::User),
            PathBuf::from("/tmp"),
            Path::new("x.md"),
        )
        .unwrap_err();
        assert!(matches!(err, AgentLoadError::NoFrontmatter(_)));
    }

    #[test]
    fn tool_list_splits_on_space_and_comma_preserving_parens() {
        // claude parseToolListFromCLI splits on whitespace AND commas (outside
        // parens). A single space-separated string -> multiple tokens.
        assert_eq!(
            parse_tool_list_from_cli(&["Read Grep".to_string()]),
            vec!["Read".to_string(), "Grep".to_string()]
        );
        // Comma-separated.
        assert_eq!(
            parse_tool_list_from_cli(&["Read, Grep".to_string()]),
            vec!["Read".to_string(), "Grep".to_string()]
        );
        // Mixed space + comma, with a paren-containing entry kept whole.
        assert_eq!(
            parse_tool_list_from_cli(&["Read Grep, Bash(git:*) Write".to_string()]),
            vec![
                "Read".to_string(),
                "Grep".to_string(),
                "Bash(git:*)".to_string(),
                "Write".to_string(),
            ]
        );
        // Commas inside parens are preserved.
        assert_eq!(
            parse_tool_list_from_cli(&["Bash(git:*,npm:*)".to_string()]),
            vec!["Bash(git:*,npm:*)".to_string()]
        );
        // Leading/multiple spaces collapse; no empty tokens.
        assert_eq!(
            parse_tool_list_from_cli(&["   Read    Grep   ".to_string()]),
            vec!["Read".to_string(), "Grep".to_string()]
        );
    }

    #[test]
    fn space_separated_tools_in_frontmatter_string() {
        // A frontmatter `tools: "Read Grep"` string now yields two tools.
        let def = md("---\nname: a\ndescription: d\ntools: \"Read Grep\"\n---\n");
        match &def.tools {
            AgentToolPolicy::Explicit(v) => {
                assert_eq!(v, &vec!["Read".to_string(), "Grep".to_string()]);
            }
            other => panic!("expected Explicit, got {other:?}"),
        }
        // disallowedTools and skills share the same coercion.
        let def = md("---\nname: a\ndescription: d\ndisallowedTools: \"Read Bash\"\n---\n");
        assert_eq!(def.disallowed_tools, vec!["Read", "Bash"]);
        let def = md("---\nname: a\ndescription: d\nskills: \"alpha beta\"\n---\n");
        assert_eq!(def.skills, vec!["alpha", "beta"]);
    }

    #[test]
    fn empty_name_is_silent_skip() {
        // claude `!agentType` is falsy for "" -> dropped like an absent name.
        let raw = "---\nname: \"\"\ndescription: d\n---\nBody";
        let err = parse_agent_markdown(
            raw,
            AgentSource::Settings(protocol::SettingsScope::User),
            PathBuf::from("/tmp"),
            Path::new("x.md"),
        )
        .unwrap_err();
        assert!(matches!(err, AgentLoadError::MissingName(_)));
    }

    #[test]
    fn name_with_colon_is_invalid() {
        // cc 2.1.218 `iju`: `name.normalize("NFKC").includes(":")` -> logged
        // error + dropped (":" is reserved for plugin namespacing).
        let raw = "---\nname: \"a:b\"\ndescription: d\n---\nBody";
        let err = parse_agent_markdown(
            raw,
            AgentSource::Settings(protocol::SettingsScope::User),
            PathBuf::from("/tmp"),
            Path::new("x.md"),
        )
        .unwrap_err();
        assert!(matches!(err, AgentLoadError::InvalidName(_)));
    }

    #[test]
    fn name_with_fullwidth_colon_is_invalid() {
        // NFKC normalizes U+FF1A FULLWIDTH COLON to ':' — the look-alike is
        // rejected exactly like a raw colon.
        let raw = "---\nname: \"a\u{FF1A}b\"\ndescription: d\n---\nBody";
        let err = parse_agent_markdown(
            raw,
            AgentSource::Settings(protocol::SettingsScope::User),
            PathBuf::from("/tmp"),
            Path::new("x.md"),
        )
        .unwrap_err();
        assert!(matches!(err, AgentLoadError::InvalidName(_)));
    }

    #[test]
    fn name_with_leading_dash_is_invalid() {
        // `iju` rejects a leading '-' BEFORE the ':' check (flag-look-alike
        // names). The flag-JSON path keeps its own '-'-only guard.
        let raw = "---\nname: \"-x\"\ndescription: d\n---\nBody";
        let err = parse_agent_markdown(
            raw,
            AgentSource::Settings(protocol::SettingsScope::User),
            PathBuf::from("/tmp"),
            Path::new("x.md"),
        )
        .unwrap_err();
        assert!(matches!(err, AgentLoadError::InvalidName(_)));
    }

    #[tokio::test]
    async fn invalid_name_drops_only_that_file() {
        // A directory mixing an invalid-name agent with a valid one loads the
        // valid one (per-file drop, not a directory abort).
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("bad.md"),
            "---\nname: \"a:b\"\ndescription: d\n---\nBody",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("good.md"),
            "---\nname: good\ndescription: d\n---\nBody",
        )
        .unwrap();
        let defs = load_agents_from_dirs(&[(
            dir.path().to_path_buf(),
            AgentSource::Settings(protocol::SettingsScope::User),
        )])
        .await;
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].agent_type, "good");
    }

    #[test]
    fn sanitize_for_display_strips_controls_and_caps() {
        // `UVe`: Cc/Cf -> space, whitespace collapsed, trimmed, capped at 200.
        assert_eq!(sanitize_for_display("  a\u{0007}b\u{200B}c  "), "a b c");
        let long = "x".repeat(300);
        assert_eq!(sanitize_for_display(&long).chars().count(), 200);
    }

    #[test]
    fn empty_description_is_skip() {
        // claude `!whenToUse` is falsy for "" -> logged + dropped.
        let raw = "---\nname: a\ndescription: \"\"\n---\nBody";
        let err = parse_agent_markdown(
            raw,
            AgentSource::Settings(protocol::SettingsScope::User),
            PathBuf::from("/tmp"),
            Path::new("x.md"),
        )
        .unwrap_err();
        assert!(matches!(err, AgentLoadError::MissingDescription(_)));
    }

    #[test]
    fn frontmatter_tools_become_explicit_policy() {
        let raw = "---\nname: r\ndescription: d\ntools: [Read, Grep]\n---\n";
        let def = parse_agent_markdown(
            raw,
            AgentSource::Settings(protocol::SettingsScope::Project),
            PathBuf::from("/tmp"),
            Path::new("r.md"),
        )
        .unwrap();
        match &def.tools {
            AgentToolPolicy::Explicit(v) => {
                assert_eq!(v, &vec!["Read".to_string(), "Grep".to_string()]);
            }
            other => panic!("expected Explicit, got {other:?}"),
        }
        assert_eq!(
            def.allowed_tools,
            vec!["Read".to_string(), "Grep".to_string()]
        );
    }

    /// `vG` scans with `rg --files --glob "*.md"`, which RECURSES. Grouping
    /// definitions into subdirectories is ordinary; the port used to read the
    /// top level only, so those files were invisible with no diagnostic.
    #[tokio::test]
    async fn agent_files_are_found_in_subdirectories() {
        let tmp = tempfile::tempdir().unwrap();
        let agents = tmp.path().join("agents");
        let nested = agents.join("reviewers").join("backend");
        tokio::fs::create_dir_all(&nested).await.unwrap();
        tokio::fs::write(
            agents.join("top.md"),
            "---\nname: top\ndescription: t\n---\nbody\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            nested.join("api.md"),
            "---\nname: api\ndescription: a\n---\nbody\n",
        )
        .await
        .unwrap();
        // A non-markdown neighbour and an oversized definition are both skipped.
        tokio::fs::write(agents.join("notes.txt"), "not an agent")
            .await
            .unwrap();
        tokio::fs::write(
            agents.join("huge.md"),
            format!(
                "---\nname: huge\ndescription: h\n---\n{}",
                "x".repeat(MARKDOWN_FILE_MAX_BYTES as usize + 1)
            ),
        )
        .await
        .unwrap();

        let loaded = load_agents_from_dirs(&[(
            agents,
            AgentSource::Settings(protocol::SettingsScope::Project),
        )])
        .await;
        let names: Vec<&str> = loaded.iter().map(|a| a.agent_type.as_str()).collect();
        assert_eq!(
            names,
            vec!["api", "top"],
            "the nested definition must load and the over-cap one must not",
        );
    }

    /// `TQr` keeps a `dev:ino` visited set precisely so a symlink cycle cannot
    /// hang the walk. Without one this test does not finish.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_cycle_does_not_hang_the_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let agents = tmp.path().join("agents");
        let sub = agents.join("sub");
        tokio::fs::create_dir_all(&sub).await.unwrap();
        tokio::fs::write(
            agents.join("a.md"),
            "---\nname: a\ndescription: d\n---\nbody\n",
        )
        .await
        .unwrap();
        // sub/loop -> agents, i.e. agents/sub/loop/sub/loop/...
        std::os::unix::fs::symlink(&agents, sub.join("loop")).unwrap();

        let loaded = load_agents_from_dirs(&[(
            agents,
            AgentSource::Settings(protocol::SettingsScope::Project),
        )])
        .await;
        assert_eq!(loaded.len(), 1, "the cycle must not duplicate or hang");
        assert_eq!(loaded[0].agent_type, "a");
    }

    /// `wQr` de-duplicates by inode across tiers, so one definition reachable
    /// from two directories through a symlink is loaded once — not twice under
    /// two different `source` labels.
    #[cfg(unix)]
    #[tokio::test]
    async fn one_file_reachable_from_two_dirs_is_loaded_once() {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path().join("user");
        let project = tmp.path().join("project");
        tokio::fs::create_dir_all(&user).await.unwrap();
        tokio::fs::create_dir_all(&project).await.unwrap();
        let real = user.join("shared.md");
        tokio::fs::write(&real, "---\nname: shared\ndescription: d\n---\nbody\n")
            .await
            .unwrap();
        // The project dir reaches the SAME inode through a symlink.
        std::os::unix::fs::symlink(&real, project.join("shared.md")).unwrap();

        let loaded = load_agents_from_dirs(&[
            (user, AgentSource::Settings(protocol::SettingsScope::User)),
            (
                project,
                AgentSource::Settings(protocol::SettingsScope::Project),
            ),
        ])
        .await;
        assert_eq!(loaded.len(), 1, "one inode, one definition: {loaded:?}");
        assert_eq!(
            loaded[0].source,
            AgentSource::Settings(protocol::SettingsScope::User),
            "the FIRST occurrence is kept, matching `wQr`'s scan order",
        );
    }

    /// The negative half: two SEPARATE files declaring the same name are not
    /// inode duplicates, so both load and ordinary later-wins decides.
    #[tokio::test]
    async fn two_distinct_files_with_one_name_still_resolve_by_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path().join("user");
        let project = tmp.path().join("project");
        tokio::fs::create_dir_all(&user).await.unwrap();
        tokio::fs::create_dir_all(&project).await.unwrap();
        tokio::fs::write(
            user.join("a.md"),
            "---\nname: dup\ndescription: USER\n---\nbody\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            project.join("b.md"),
            "---\nname: dup\ndescription: PROJECT\n---\nbody\n",
        )
        .await
        .unwrap();

        let loaded = load_agents_from_dirs(&[
            (user, AgentSource::Settings(protocol::SettingsScope::User)),
            (
                project,
                AgentSource::Settings(protocol::SettingsScope::Project),
            ),
        ])
        .await;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].when_to_use, "PROJECT", "later tier wins");
    }

    /// `policy_agent_dir` is `Z$`'s TOP tier, so it must beat a `--agents`
    /// definition of the same name — which is why the composition root merges
    /// it AFTER the flag agents.
    #[test]
    fn merge_agents_later_wins_replaces_in_place() {
        let named = |ty: &str, desc: &str, source: AgentSource| {
            parse_agent_markdown(
                &format!("---\nname: {ty}\ndescription: {desc}\n---\nbody\n"),
                source,
                PathBuf::from("/base"),
                Path::new("x.md"),
            )
            .expect("fixture parses")
        };
        let mut agents = vec![
            named("keeper", "FLAG", AgentSource::Flag),
            named(
                "other",
                "PROJECT",
                AgentSource::Settings(protocol::SettingsScope::Project),
            ),
        ];
        merge_agents_later_wins(
            &mut agents,
            vec![
                named(
                    "keeper",
                    "POLICY",
                    AgentSource::Settings(protocol::SettingsScope::Managed),
                ),
                named(
                    "fresh",
                    "POLICY",
                    AgentSource::Settings(protocol::SettingsScope::Managed),
                ),
            ],
        );
        assert_eq!(agents.len(), 3, "replace in place, then append: {agents:?}");
        let keeper = agents.iter().find(|a| a.agent_type == "keeper").unwrap();
        assert_eq!(keeper.when_to_use, "POLICY");
        assert_eq!(
            keeper.source,
            AgentSource::Settings(protocol::SettingsScope::Managed)
        );
        assert_eq!(
            agents[1].agent_type, "other",
            "untouched entries keep order"
        );
    }

    #[test]
    fn policy_agent_dir_sits_under_the_managed_root() {
        assert_eq!(
            policy_agent_dir(Path::new("/Library/Application Support/LingXi")),
            PathBuf::from("/Library/Application Support/LingXi")
                .join(branding::DOT_DIR)
                .join("agents")
        );
    }

    /// `O5`'s three stop conditions, and the order it returns.
    #[test]
    fn project_agent_dirs_walks_up_to_the_project_root() {
        let d = |p: &str| PathBuf::from(p);
        let agents = |p: &str| d(p).join(branding::DOT_DIR).join("agents");

        // cwd inside a package: every level from cwd to the project root,
        // deepest first, root INCLUDED.
        assert_eq!(
            project_agent_dirs(
                &d("/home/u/repo/packages/foo"),
                &d("/home/u"),
                Some(&d("/home/u/repo")),
            ),
            vec![
                agents("/home/u/repo/packages/foo"),
                agents("/home/u/repo/packages"),
                agents("/home/u/repo"),
            ]
        );

        // At the project root: just the one.
        assert_eq!(
            project_agent_dirs(&d("/home/u/repo"), &d("/home/u"), Some(&d("/home/u/repo"))),
            vec![agents("/home/u/repo")]
        );

        // No project root: the HOME ceiling stops the walk, and home itself is
        // never collected — `if(Pf(d)===Pf(r))break` runs before the push.
        assert_eq!(
            project_agent_dirs(&d("/home/u/scratch/x"), &d("/home/u"), None),
            vec![agents("/home/u/scratch/x"), agents("/home/u/scratch")]
        );

        // cwd IS home: nothing at all.
        assert!(project_agent_dirs(&d("/home/u"), &d("/home/u"), None).is_empty());

        // Neither home nor root on the path: the filesystem root ends it.
        assert_eq!(
            project_agent_dirs(&d("/srv/a"), &d("/home/u"), None),
            vec![agents("/srv/a"), agents("/srv"), agents("/")]
        );
    }

    /// `load_agents_from_dirs` is later-wins, and `ITe` makes the DEEPEST
    /// project directory win, so the walk must be handed over reversed.
    #[test]
    fn agent_dir_precedence_puts_the_deepest_project_dir_last() {
        let d = |p: &str| PathBuf::from(p);
        let got = agent_dir_precedence(
            d("/home/u/.lingxi/agents"),
            &d("/home/u/repo/packages/foo"),
            &d("/home/u"),
            Some(&d("/home/u/repo")),
            &[],
        );
        let names: Vec<String> = got
            .iter()
            .map(|(p, _)| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "/home/u/.lingxi/agents".to_string(),
                format!("/home/u/repo/{}/agents", branding::DOT_DIR),
                format!("/home/u/repo/packages/{}/agents", branding::DOT_DIR),
                format!("/home/u/repo/packages/foo/{}/agents", branding::DOT_DIR),
            ],
            "user first (lowest), then project shallow -> deep",
        );
        assert_eq!(
            got[0].1,
            AgentSource::Settings(protocol::SettingsScope::User)
        );
        assert!(got[1..]
            .iter()
            .all(|(_, s)| *s == AgentSource::Settings(protocol::SettingsScope::Project)));
    }

    /// `--add-dir` agents rank between the user tier and the ordinary project
    /// tier, and a root the upward walk already covers is not added twice.
    #[test]
    fn agent_dir_precedence_places_add_dirs_below_the_project_tier() {
        let d = |p: &str| PathBuf::from(p);
        let agents = |p: &str| d(p).join(branding::DOT_DIR).join("agents");
        let got = agent_dir_precedence(
            d("/home/u/.lingxi/agents"),
            &d("/home/u/repo"),
            &d("/home/u"),
            Some(&d("/home/u/repo")),
            // `/other` is new; `/home/u/repo` is already the project walk's
            // only entry and must not be repeated.
            &[d("/other"), d("/home/u/repo"), d("/other")],
        );
        assert_eq!(
            got,
            vec![
                (
                    d("/home/u/.lingxi/agents"),
                    AgentSource::Settings(protocol::SettingsScope::User)
                ),
                (agents("/other"), AgentSource::AdditionalDirectory),
                (
                    agents("/home/u/repo"),
                    AgentSource::Settings(protocol::SettingsScope::Project)
                ),
            ],
            "user < additionalDirectory < projectSettings, deduped",
        );
    }

    /// The deepest definition of a name must be the one that survives — the
    /// behaviour `ITe` + later-wins produce together, asserted end to end
    /// through the real loader rather than on the ordering alone.
    #[tokio::test]
    async fn a_nested_project_agent_overrides_a_shallower_one() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        let deep = root.join("packages").join("foo");
        for (dir, body) in [(&root, "ROOT"), (&deep, "DEEP")] {
            let agents = dir.join(branding::DOT_DIR).join("agents");
            tokio::fs::create_dir_all(&agents).await.unwrap();
            tokio::fs::write(
                agents.join("reviewer.md"),
                format!("---\nname: reviewer\ndescription: {body}\n---\nbody\n"),
            )
            .await
            .unwrap();
        }
        let dirs = agent_dir_precedence(
            tmp.path().join("no-such-user-dir"),
            &deep,
            tmp.path(),
            Some(&root),
            &[],
        );
        let loaded = load_agents_from_dirs(&dirs).await;
        let reviewer = loaded
            .iter()
            .find(|a| a.agent_type == "reviewer")
            .expect("the nested walk must find it at all");
        assert_eq!(
            reviewer.when_to_use, "DEEP",
            "the definition closest to the cwd wins",
        );
        assert_eq!(loaded.len(), 1, "one name, one entry: {loaded:?}");
    }

    #[tokio::test]
    async fn load_agents_from_dirs_merges_user_and_project() {
        let dir = TempDir::new().unwrap();
        let user = dir.path().join("user");
        let project = dir.path().join("project");
        tokio::fs::create_dir_all(&user).await.unwrap();
        tokio::fs::create_dir_all(&project).await.unwrap();
        tokio::fs::write(
            user.join("alpha.md"),
            "---\nname: alpha\ndescription: from user\n---\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            project.join("alpha.md"),
            "---\nname: alpha\ndescription: from project\n---\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            project.join("beta.md"),
            "---\nname: beta\ndescription: project-only\n---\n",
        )
        .await
        .unwrap();

        let defs = load_agents_from_dirs(&[
            (
                user.clone(),
                AgentSource::Settings(protocol::SettingsScope::User),
            ),
            (
                project.clone(),
                AgentSource::Settings(protocol::SettingsScope::Project),
            ),
        ])
        .await;

        assert_eq!(defs.len(), 2);
        // Sorted alphabetically.
        assert_eq!(defs[0].agent_type, "alpha");
        assert_eq!(defs[0].when_to_use, "from project"); // project wins
        assert_eq!(defs[1].agent_type, "beta");
    }

    #[tokio::test]
    async fn load_agents_from_missing_dir_yields_empty() {
        let defs = load_agents_from_dirs(&[(
            PathBuf::from("/does/not/exist"),
            AgentSource::Settings(protocol::SettingsScope::User),
        )])
        .await;
        assert!(defs.is_empty());
    }

    // ── extended frontmatter field parsing ──

    fn md(body: &str) -> AgentDefinition {
        parse_agent_markdown(
            body,
            AgentSource::Settings(protocol::SettingsScope::User),
            PathBuf::from("/tmp"),
            Path::new("a.md"),
        )
        .unwrap()
    }

    #[test]
    fn missing_name_is_silent_skip() {
        let raw = "---\ndescription: d\n---\nBody";
        let err = parse_agent_markdown(
            raw,
            AgentSource::Settings(protocol::SettingsScope::User),
            PathBuf::from("/tmp"),
            Path::new("notes.md"),
        )
        .unwrap_err();
        assert!(matches!(err, AgentLoadError::MissingName(_)));
    }

    #[test]
    fn missing_description_errors() {
        let raw = "---\nname: x\n---\nBody";
        let err = parse_agent_markdown(
            raw,
            AgentSource::Settings(protocol::SettingsScope::User),
            PathBuf::from("/tmp"),
            Path::new("x.md"),
        )
        .unwrap_err();
        assert!(matches!(err, AgentLoadError::MissingDescription(_)));
    }

    #[test]
    fn disallowed_tools_string_and_list() {
        let def = md("---\nname: a\ndescription: d\ndisallowedTools: \"Read, Bash\"\n---\n");
        assert_eq!(def.disallowed_tools, vec!["Read", "Bash"]);
        let def = md("---\nname: a\ndescription: d\ndisallowedTools: [Read, Bash]\n---\n");
        assert_eq!(def.disallowed_tools, vec!["Read", "Bash"]);
    }

    #[test]
    fn frontmatter_disallowed_tools_parsed() {
        // claude applies `tools` (allowlist → Explicit) and `disallowedTools`
        // (denylist) ORTHOGONALLY: an agent can carry both. The resolver
        // subtracts disallowed_tools from the Explicit-resolved pool.
        let def =
            md("---\nname: a\ndescription: d\ntools: [Read, Bash]\ndisallowedTools: [Bash]\n---\n");
        match &def.tools {
            AgentToolPolicy::Explicit(names) => {
                assert_eq!(names, &vec!["Read".to_string(), "Bash".to_string()]);
            }
            other => panic!("expected Explicit policy, got {other:?}"),
        }
        assert_eq!(def.disallowed_tools, vec!["Bash"]);
    }

    #[test]
    fn skills_comma_and_list() {
        let def = md("---\nname: a\ndescription: d\nskills: \"alpha, beta\"\n---\n");
        assert_eq!(def.skills, vec!["alpha", "beta"]);
        let def = md("---\nname: a\ndescription: d\nskills: [alpha, beta]\n---\n");
        assert_eq!(def.skills, vec!["alpha", "beta"]);
        // missing -> []
        let def = md("---\nname: a\ndescription: d\n---\n");
        assert!(def.skills.is_empty());
    }

    #[test]
    fn background_true_variants() {
        assert!(md("---\nname: a\ndescription: d\nbackground: true\n---\n").background);
        assert!(md("---\nname: a\ndescription: d\nbackground: \"true\"\n---\n").background);
        assert!(!md("---\nname: a\ndescription: d\nbackground: false\n---\n").background);
        // invalid value -> false (logged)
        assert!(!md("---\nname: a\ndescription: d\nbackground: yes\n---\n").background);
        // omitted -> false
        assert!(!md("---\nname: a\ndescription: d\n---\n").background);
    }

    #[test]
    fn memory_valid_and_invalid() {
        assert_eq!(
            md("---\nname: a\ndescription: d\nmemory: project\n---\n").memory,
            Some(protocol::WritableScope::Project)
        );
        assert_eq!(
            md("---\nname: a\ndescription: d\nmemory: bogus\n---\n").memory,
            None
        );
    }

    #[test]
    fn isolation_worktree_and_remote_rejected_on_non_ant() {
        // NOTE: relies on USER_TYPE != "ant" in the test env.
        assert_eq!(
            md("---\nname: a\ndescription: d\nisolation: worktree\n---\n").isolation,
            Some(AgentIsolation::Worktree)
        );
        if std::env::var("USER_TYPE").as_deref() != Ok("ant") {
            assert_eq!(
                md("---\nname: a\ndescription: d\nisolation: remote\n---\n").isolation,
                None
            );
        }
    }

    #[test]
    fn effort_level_and_integer_and_invalid() {
        assert_eq!(
            md("---\nname: a\ndescription: d\neffort: high\n---\n").effort,
            Some(AgentEffort::Level("high".into()))
        );
        assert_eq!(
            md("---\nname: a\ndescription: d\neffort: 7\n---\n").effort,
            Some(AgentEffort::Numeric(7))
        );
        assert_eq!(
            md("---\nname: a\ndescription: d\neffort: bogus\n---\n").effort,
            None
        );
    }

    #[test]
    fn color_valid_and_invalid() {
        assert_eq!(
            md("---\nname: a\ndescription: d\ncolor: blue\n---\n").color,
            Some("blue".to_string())
        );
        assert_eq!(
            md("---\nname: a\ndescription: d\ncolor: mauve\n---\n").color,
            None
        );
    }

    #[test]
    fn initial_prompt_keeps_raw_when_trim_nonempty() {
        // Leading space preserved; raw (untrimmed) value kept.
        let def = md("---\nname: a\ndescription: d\ninitialPrompt: \"  hello\"\n---\n");
        assert_eq!(def.initial_prompt.as_deref(), Some("  hello"));
        // whitespace-only -> None
        let def = md("---\nname: a\ndescription: d\ninitialPrompt: \"   \"\n---\n");
        assert_eq!(def.initial_prompt, None);
    }

    #[test]
    fn permission_mode_plan_and_default_and_invalid() {
        assert_eq!(
            md("---\nname: a\ndescription: d\npermissionMode: plan\n---\n").permission_mode,
            AgentPermissionMode::Plan
        );
        assert_eq!(
            md("---\nname: a\ndescription: d\npermissionMode: default\n---\n").permission_mode,
            AgentPermissionMode::Default
        );
        assert_eq!(
            md("---\nname: a\ndescription: d\npermissionMode: acceptEdits\n---\n").permission_mode,
            AgentPermissionMode::AcceptEdits
        );
        assert_eq!(
            md("---\nname: a\ndescription: d\npermissionMode: dontAsk\n---\n").permission_mode,
            AgentPermissionMode::DontAsk
        );
        assert_eq!(
            md("---\nname: a\ndescription: d\npermissionMode: bypassPermissions\n---\n")
                .permission_mode,
            AgentPermissionMode::BypassPermissions
        );
        // invalid -> Bubble (logged)
        assert_eq!(
            md("---\nname: a\ndescription: d\npermissionMode: bogus\n---\n").permission_mode,
            AgentPermissionMode::Bubble
        );
    }

    #[test]
    fn model_inherit_coercion() {
        assert!(matches!(
            md("---\nname: a\ndescription: d\nmodel: inherit\n---\n").model,
            AgentModel::Inherit
        ));
        assert!(matches!(
            md("---\nname: a\ndescription: d\nmodel: INHERIT\n---\n").model,
            AgentModel::Inherit
        ));
        assert!(
            matches!(md("---\nname: a\ndescription: d\nmodel: sonnet\n---\n").model, AgentModel::Alias(m) if m == "sonnet")
        );
    }

    #[test]
    fn tools_star_is_all_and_empty_is_explicit_none() {
        // missing -> All
        assert!(matches!(
            md("---\nname: a\ndescription: d\n---\n").tools,
            AgentToolPolicy::All { .. }
        ));
        // '*' -> All
        assert!(matches!(
            md("---\nname: a\ndescription: d\ntools: \"*\"\n---\n").tools,
            AgentToolPolicy::All { .. }
        ));
        // present-but-empty -> Explicit([]) (no tools)
        match md("---\nname: a\ndescription: d\ntools: []\n---\n").tools {
            AgentToolPolicy::Explicit(v) => assert!(v.is_empty()),
            other => panic!("expected Explicit([]), got {other:?}"),
        }
    }

    #[test]
    fn hooks_frontmatter_parsed_with_frontmatter_source() {
        let raw = "---\nname: a\ndescription: d\nhooks:\n  PreToolUse:\n    - hooks:\n        - type: command\n          command: echo hi\n---\nBody";
        let def = md(raw);
        assert!(
            !def.frontmatter_hooks.is_empty(),
            "expected hooks parsed from frontmatter"
        );
        assert!(def
            .frontmatter_hooks
            .iter()
            .all(|h| h.source == hooks::HookSource::FrontMatter));
    }

    #[test]
    fn mcp_servers_by_name_and_skip_invalid() {
        let def = md("---\nname: a\ndescription: d\nmcpServers: [slack]\n---\n");
        assert!(
            matches!(def.mcp_servers.as_slice(), [AgentMcpServerSpec::ByName(n)] if n == "slack")
        );
    }

    /// (M7 cc2.1.220) A real-world inline server body (`{command, args}` — one
    /// `.mcp.json` entry, NOT a serialized `McpServerConfig`) parses to a raw
    /// `Record`; the scoped-config conversion (`obs`) validates it later.
    #[test]
    fn mcp_servers_inline_record_kept_raw() {
        let def = md(concat!(
            "---\nname: a\ndescription: d\nmcpServers:\n",
            "  - docs:\n      command: npx\n      args: [\"-y\", \"docs-mcp\"]\n",
            "---\n"
        ));
        match def.mcp_servers.as_slice() {
            [AgentMcpServerSpec::Record(map)] => {
                assert_eq!(map.len(), 1);
                let body = map.get("docs").expect("keyed by server name");
                assert_eq!(body.get("command").and_then(|v| v.as_str()), Some("npx"));
            }
            other => panic!("expected one Record spec, got {other:?}"),
        }
    }

    // ── JSON agents ──

    #[test]
    fn json_agent_full_field_set() {
        let json = serde_json::json!({
            "description": "does things",
            "prompt": "you are a helper",
            "tools": ["Read", "Grep"],
            "disallowedTools": ["Bash"],
            "model": "inherit",
            "effort": "high",
            "permissionMode": "plan",
            "maxTurns": 5,
            "skills": ["alpha"],
            "initialPrompt": "  start",
            "memory": "project",
            "background": true,
            "isolation": "worktree",
        });
        let def = parse_agent_from_json("myagent", &json, AgentSource::Flag).unwrap();
        assert_eq!(def.agent_type, "myagent");
        assert_eq!(def.when_to_use, "does things");
        assert_eq!(def.system_prompt.as_deref(), Some("you are a helper"));
        assert!(matches!(def.tools, AgentToolPolicy::Explicit(_)));
        assert_eq!(def.disallowed_tools, vec!["Bash"]);
        assert!(matches!(def.model, AgentModel::Inherit));
        assert_eq!(def.effort, Some(AgentEffort::Level("high".into())));
        assert_eq!(def.permission_mode, AgentPermissionMode::Plan);
        assert_eq!(def.max_turns, 5);
        assert_eq!(def.skills, vec!["alpha"]);
        assert_eq!(def.initial_prompt.as_deref(), Some("  start"));
        assert_eq!(def.memory, Some(protocol::WritableScope::Project));
        assert!(def.background);
        assert_eq!(def.isolation, Some(AgentIsolation::Worktree));
        assert_eq!(def.source, AgentSource::Flag);
    }

    #[test]
    fn json_agent_missing_description_is_none() {
        let json = serde_json::json!({ "prompt": "x" });
        assert!(parse_agent_from_json("a", &json, AgentSource::Flag).is_none());
        let json = serde_json::json!({ "description": "", "prompt": "x" });
        assert!(parse_agent_from_json("a", &json, AgentSource::Flag).is_none());
    }

    #[test]
    fn json_agent_missing_prompt_is_none() {
        let json = serde_json::json!({ "description": "d" });
        assert!(parse_agent_from_json("a", &json, AgentSource::Flag).is_none());
        let json = serde_json::json!({ "description": "d", "prompt": "" });
        assert!(parse_agent_from_json("a", &json, AgentSource::Flag).is_none());
    }

    #[test]
    fn json_agent_strict_invalid_fields_drop_whole_agent() {
        // Each of these is a single invalid field that claude's throwing zod
        // schema rejects -> the WHOLE agent is dropped (None).
        let base = |extra: serde_json::Value| {
            let mut obj = serde_json::Map::new();
            obj.insert("description".into(), serde_json::json!("d"));
            obj.insert("prompt".into(), serde_json::json!("p"));
            if let serde_json::Value::Object(m) = extra {
                for (k, v) in m {
                    obj.insert(k, v);
                }
            }
            serde_json::Value::Object(obj)
        };

        // bad effort: numeric string "7" is NOT a level and NOT a number.
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"effort": "7"})),
            AgentSource::Flag
        )
        .is_none());
        // bad effort: float is not an int.
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"effort": 1.5})),
            AgentSource::Flag
        )
        .is_none());
        // bad effort: wrong-case level (zod enum is case-sensitive).
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"effort": "HIGH"})),
            AgentSource::Flag
        )
        .is_none());
        // bad maxTurns: zero / negative / non-number string.
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"maxTurns": 0})),
            AgentSource::Flag
        )
        .is_none());
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"maxTurns": -3})),
            AgentSource::Flag
        )
        .is_none());
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"maxTurns": "5"})),
            AgentSource::Flag
        )
        .is_none());
        // bad permissionMode.
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"permissionMode": "bogus"})),
            AgentSource::Flag
        )
        .is_none());
        // bad memory.
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"memory": "bogus"})),
            AgentSource::Flag
        )
        .is_none());
        // bad isolation. `remote` is schema-valid for every user type; runtime
        // availability is checked later.
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"isolation": "bogus"})),
            AgentSource::Flag
        )
        .is_none());
        // non-array tools / disallowedTools / skills.
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"tools": "Read"})),
            AgentSource::Flag
        )
        .is_none());
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"tools": [1, 2]})),
            AgentSource::Flag
        )
        .is_none());
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"disallowedTools": "Bash"})),
            AgentSource::Flag
        )
        .is_none());
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"skills": "alpha"})),
            AgentSource::Flag
        )
        .is_none());
        // non-bool background.
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"background": "true"})),
            AgentSource::Flag
        )
        .is_none());
        // whitespace-only model.
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"model": "   "})),
            AgentSource::Flag
        )
        .is_none());
        // invalid mcpServers item (number is neither string nor record).
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"mcpServers": [123]})),
            AgentSource::Flag
        )
        .is_none());
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"mcpServers": [{"srv": 123}]})),
            AgentSource::Flag
        )
        .is_none());
        // mcpServers not an array.
        assert!(parse_agent_from_json(
            "a",
            &base(serde_json::json!({"mcpServers": "slack"})),
            AgentSource::Flag
        )
        .is_none());
    }

    #[test]
    fn json_agent_invalid_mcp_entry_keeps_agent() {
        let json = serde_json::json!({
            "description": "d",
            "prompt": "p",
            "mcpServers": [{}]
        });
        let def = parse_agent_from_json("a", &json, AgentSource::Flag)
            .expect("invalid MCP entry must not drop the agent");
        assert_eq!(def.mcp_servers.len(), 1);
        assert!(
            crate::mcp_servers::agent_mcp_specs_to_scoped_configs(&def, false, false, &[])
                .is_empty()
        );
    }

    #[test]
    fn json_agent_strict_valid_fields_kept() {
        // Sanity: the strict path STILL accepts well-formed values.
        let json = serde_json::json!({
            "description": "d",
            "prompt": "p",
            "effort": 7,
            "maxTurns": 3,
            "tools": ["Read"],
            "background": false,
            "memory": "user",
            "mcpServers": ["slack", "github"],
        });
        let def = parse_agent_from_json("a", &json, AgentSource::Flag).unwrap();
        assert_eq!(def.effort, Some(AgentEffort::Numeric(7)));
        assert_eq!(def.max_turns, 3);
        assert!(!def.background);
        assert_eq!(def.memory, Some(protocol::WritableScope::User));
        assert_eq!(def.mcp_servers.len(), 2);
        assert!(matches!(&def.mcp_servers[0], AgentMcpServerSpec::ByName(n) if n == "slack"));
        assert!(matches!(&def.mcp_servers[1], AgentMcpServerSpec::ByName(n) if n == "github"));
    }

    #[test]
    fn json_agents_object_filters_invalid() {
        let json = serde_json::json!({
            "good": { "description": "d", "prompt": "p" },
            "bad": { "description": "d" }, // missing prompt
        });
        let defs = parse_agents_from_json(&json, AgentSource::Flag);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].agent_type, "good");
    }

    #[test]
    fn json_agents_non_object_is_empty() {
        let json = serde_json::json!([1, 2, 3]);
        assert!(parse_agents_from_json(&json, AgentSource::Flag).is_empty());
    }

    #[test]
    fn agent_definition_legacy_json_roundtrip() {
        // A legacy serialized AgentDefinition missing the new keys must still
        // deserialize via #[serde(default)].
        let legacy = serde_json::json!({
            "agent_type": "legacy",
            "when_to_use": "w",
            "tools": { "All": { "use_exact_tools": false } },
            "max_turns": 10,
            "model": "Inherit",
            "permission_mode": "Bubble",
            "source": "BuiltIn",
            "base_dir": "/tmp",
            "system_prompt": null,
            "mcp_servers": [],
            "frontmatter_hooks": [],
            "icon": null,
            "allowed_tools": [],
            "worktree_requirement": null
        });
        let def: AgentDefinition = serde_json::from_value(legacy).unwrap();
        assert_eq!(def.agent_type, "legacy");
        assert!(def.disallowed_tools.is_empty());
        assert!(!def.background);
        assert_eq!(def.color, None);
        // Round-trips back out and back in.
        let s = serde_json::to_string(&def).unwrap();
        let def2: AgentDefinition = serde_json::from_str(&s).unwrap();
        assert_eq!(def2.agent_type, "legacy");
    }

    // ── `--agents <json>` flag parser (`QXt`, cc 2.1.198 M4) ────────────────

    #[test]
    fn flag_json_valid_agents_parse_with_flag_source() {
        // The binary's own help example.
        let raw =
            r#"{"reviewer": {"description": "Reviews code", "prompt": "You are a code reviewer"}}"#;
        let out = parse_agents_from_flag_json(raw);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].agent_type, "reviewer");
        assert_eq!(out[0].when_to_use, "Reviews code");
        assert_eq!(
            out[0].system_prompt.as_deref(),
            Some("You are a code reviewer")
        );
        assert_eq!(out[0].source, AgentSource::Flag);
    }

    #[test]
    fn flag_json_syntax_error_yields_no_agents() {
        // `Ba(r)` throws → `De(g)` (logged), no agents, no abort.
        assert!(parse_agents_from_flag_json("{not json").is_empty());
    }

    #[test]
    fn flag_json_non_object_yields_no_agents() {
        // `DBm()` is a record schema — arrays/scalars fail the record parse.
        assert!(parse_agents_from_flag_json("[1,2]").is_empty());
        assert!(parse_agents_from_flag_json("\"x\"").is_empty());
    }

    #[test]
    fn flag_json_one_invalid_agent_drops_all() {
        // Record-level zod parse is all-or-nothing (unlike the per-entry
        // filtering of `parseAgentsFromJson`): `good` is dropped too.
        let raw = r#"{"good": {"description": "d", "prompt": "p"},
                      "bad": {"description": "", "prompt": "p"}}"#;
        assert!(parse_agents_from_flag_json(raw).is_empty());
    }

    #[test]
    fn flag_json_dash_name_rejects_the_record() {
        let raw = r#"{"-bad": {"description": "d", "prompt": "p"},
                      "good": {"description": "d", "prompt": "p"}}"#;
        assert_eq!(
            parse_agents_from_flag_json_checked(raw).unwrap_err(),
            "-bad: agent names must not start with '-'"
        );
        assert!(parse_agents_from_flag_json(raw).is_empty());
    }

    #[test]
    fn checked_flag_json_reports_2_1_246_schema_errors() {
        assert_eq!(
            parse_agents_from_flag_json_checked("{bad").unwrap_err(),
            "invalid JSON: JSON Parse error: Expected '}'"
        );
        assert_eq!(
            parse_agents_from_flag_json_checked("[]").unwrap_err(),
            "Invalid input: expected record, received array"
        );
        assert_eq!(
            parse_agents_from_flag_json_checked(r#"{"x":{"description":3,"prompt":"p"}}"#)
                .unwrap_err(),
            "x.description: Invalid input: expected string, received number"
        );
        assert_eq!(
            parse_agents_from_flag_json_checked(
                r#"{"x":{"description":"d","prompt":"p","tools":null}}"#
            )
            .unwrap_err(),
            "x.tools: Invalid input: expected array, received null"
        );
        assert_eq!(
            parse_agents_from_flag_json_checked(
                r#"{"x":{"description":"d","prompt":"p","hooks":{"PreToolUse":[{"foo":"bar"}]}}}"#
            )
            .unwrap_err(),
            "x.hooks.PreToolUse.0.hooks: Invalid input: expected array, received undefined"
        );
        assert_eq!(
            parse_agents_from_flag_json_checked(
                r#"{"x":{"description":"d","prompt":"p","mcpServers":[{"srv":123}]}}"#
            )
            .unwrap_err(),
            "x.mcpServers.0: Invalid input"
        );
    }

    #[test]
    fn checked_flag_json_accepts_auto_and_remote() {
        let parsed = parse_agents_from_flag_json_checked(
            r#"{"x":{"description":"d","prompt":"p","permissionMode":"auto","isolation":"remote"}}"#,
        )
        .unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].permission_mode, AgentPermissionMode::Auto);
        assert_eq!(parsed[0].isolation, Some(AgentIsolation::Remote));

        for raw in [
            r#"{"x":{"description":"d","prompt":"p","observerMessage":"hi"}}"#,
            r#"{"x":{"description":"d","prompt":"p","observeSubagents":false}}"#,
        ] {
            let parsed = parse_agents_from_flag_json_checked(raw).unwrap();
            assert_eq!(parsed.len(), 1);
            assert!(parsed[0].observer.is_none());
        }
    }
}
