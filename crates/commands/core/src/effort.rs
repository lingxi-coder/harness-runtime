//! `/effort` — set or show the model effort level.
//!
//! Ported from the claude-code TS local-jsx command
//! `src/commands/effort/effort.tsx` (+ `src/utils/effort.ts`). The TS `call()`
//! trims the args, then branches:
//!   * `help` / `-h` / `--help` → a static Usage block;
//!   * `''` / `current` / `status` → `showCurrentEffort`;
//!   * `auto` / `unset` → clear the persisted level;
//!   * a valid level → set it;
//!   * anything else → an invalid-argument message.
//!
//! ## Persistence + resolver
//!
//! The TS set/clear paths call
//! `updateSettingsForSource('userSettings', { effortLevel })`. The Rust
//! settings crate is a read-only loader, but a write is not load-bearing on
//! it: we persist with the same direct-fs seam `export.rs` uses, mirroring
//! `updateSettingsForSource` byte-for-byte (merge into the existing
//! `~/.lingxi/settings.json`, treat a missing value as a delete, never
//! overwrite a JSON-syntax-broken file). [`persist_effort_level`] is the port.
//!
//! Per [`to_persistable`] (TS `Tve` / `toPersistableEffort`) only
//! `low`/`medium`/`high`/`xhigh` are persistable for non-ant users; `max` is
//! session-scoped, so setting `max` keeps the `" (this session only)"` suffix
//! and writes nothing — 1:1 with the TS `persistable === undefined` branch. A
//! persistable level instead carries the `" (saved as your default for new
//! sessions)"` suffix (TS `Ium`, v2.1.183).
//!
//! The `auto (currently {level})` computed level is driven by the ported
//! [`get_displayed_effort_level`] → [`resolve_applied_effort`] →
//! [`get_default_effort_for_model`] + [`model_supports_max_effort`] chain.
//! Three TS branches of `getDefaultEffortForModel` are seam-blocked and
//! documented on that fn (Pro/Max/Team → medium needs subscriber-auth +
//! `GrowthBook`; ultrathink → medium needs the ultrathink seam) — none is
//! reachable in-tree, so every reachable model resolves to the API default
//! `high`, computed rather than hard-coded.
//!
//! The env override (`LINGXI_EFFORT_LEVEL`) is honoured in every branch.

use async_trait::async_trait;
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;
use platform_api::OrchestratorHandle;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The `${nyn}` model-family interpolation used by the `xhigh` Usage/description
/// lines: `getEffortHelpText` (`XVn`) and `getEffortLevelDescription` (`NXu`)
/// both embed it. 2.1.220 value.
const XHIGH_MODELS: &str = "Fable 5, Opus 4.7+, Sonnet 5";

/// The `${gAi}` model-family interpolation used by the `- max:` Usage line
/// (`XVn`). 2.1.220 value.
const MAX_MODELS: &str = "Fable 5, Opus 4.6+, Sonnet 4.6+";

/// Render the `/effort` help block — a 1:1 port of `getEffortHelpText` (`XVn`).
///
/// The `- max:` line renders `Maximum capability with deepest reasoning
/// (${gAi})`; the `- xhigh:` line renders `Extended reasoning with thorough
/// analysis (${nyn})`. The `- ultracode:` line is appended only when
/// `e = x4(js())` is true; `xhigh` is
/// unconditional. `XVn` uses square brackets (`[...]`) for the bracketed list —
/// the angle-bracket (`<...>`) form lives in the separate non-interactive
/// empty-arg fallback (`tdm`), which this port routes to `show_current`.
///
/// `getEffortHelpText` (`XVn`) is parameterized on the `e = x4(js())` gate so
/// both gated-off and gated-on renderings stay unit-testable without a global.
fn usage_with(dynamic_workflows: bool) -> String {
    let ultracode_list = if dynamic_workflows { "|ultracode" } else { "" };
    let ultracode_line = if dynamic_workflows {
        "- ultracode: xhigh + dynamic workflow orchestration (this session only)\n"
    } else {
        ""
    };
    format!(
        "Usage: /effort [low|medium|high|xhigh|max{ultracode_list}|auto]\n\n\
Effort levels:\n\
- low: Quick, straightforward implementation\n\
- medium: Balanced approach with standard testing\n\
- high: Comprehensive implementation with extensive testing\n\
- xhigh: Extended reasoning with thorough analysis ({XHIGH_MODELS})\n\
- max: Maximum capability with deepest reasoning ({MAX_MODELS})\n\
{ultracode_line}- auto: Use the default effort level for your model"
    )
}

/// Environment variable that pins / clears the effort level for the session.
const EFFORT_ENV_VAR: &str = "LINGXI_EFFORT_LEVEL";

/// The discrete effort levels (`effort.ts` `EFFORT_LEVELS` / `nP =
/// ["low","medium","high","xhigh","max"]`, v2.1.183).
///
/// `ultracode` is NOT a member here: in claude it is a parser pseudo-level that
/// maps onto `xhigh` (`Hum`/`Pum`/`ZVn`), not a distinct `EFFORT_LEVELS` value.
/// Numeric efforts are ANT-only and intentionally omitted from this port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffortLevel {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl EffortLevel {
    /// Rank in oracle `Ic` (`low < medium < high < xhigh < max`).
    fn rank(self) -> u8 {
        match self {
            EffortLevel::Low => 0,
            EffortLevel::Medium => 1,
            EffortLevel::High => 2,
            EffortLevel::Xhigh => 3,
            EffortLevel::Max => 4,
        }
    }

    /// The canonical lowercase string for this level (`String(e)` / `Jse`).
    fn as_str(self) -> &'static str {
        match self {
            EffortLevel::Low => "low",
            EffortLevel::Medium => "medium",
            EffortLevel::High => "high",
            EffortLevel::Xhigh => "xhigh",
            EffortLevel::Max => "max",
        }
    }

    /// User-facing description, verbatim from `effort.ts`
    /// (`getEffortLevelDescription` → `NXu`). The `xhigh` case renders the TS
    /// template `` `Deeper reasoning than high, just below maximum (${nyn})` ``;
    /// the `max` case renders `` `Maximum capability with deepest reasoning.
    /// ${qHt}` `` with `qHt` = the v2.1.183 overthinking caveat.
    fn description(self) -> String {
        match self {
            EffortLevel::Low => {
                "Quick, straightforward implementation with minimal overhead".to_string()
            }
            EffortLevel::Medium => {
                "Balanced approach with standard implementation and testing".to_string()
            }
            EffortLevel::High => {
                "Comprehensive implementation with extensive testing and documentation".to_string()
            }
            EffortLevel::Xhigh => {
                format!("Deeper reasoning than high, just below maximum ({XHIGH_MODELS})")
            }
            EffortLevel::Max => "Maximum capability with deepest reasoning. May use excessive tokens resulting in long response times or overthinking. Use sparingly for the hardest tasks.".to_string(),
        }
    }
}

/// Parse a single token into an [`EffortLevel`] — a 1:1 port of `hQe`:
/// `n = _Ai[trim().toLowerCase()] ?? trim().toLowerCase(); wFe(n) ? n : void 0`.
/// `_Ai = {med: "medium"}` is the only alias; `wFe` = `nP.includes`.
///
/// `ultracode` is NOT handled here — claude routes it separately (`ZVn`/`Hum`),
/// mapping it to `xhigh` only when [`dynamic_workflows_enabled`] is true. See
/// the `handle` dispatch.
fn parse_effort_level(s: &str) -> Option<EffortLevel> {
    // `_Ai[t] ?? t` — apply the `med → medium` alias before the membership test.
    let normalized = match s.trim().to_lowercase().as_str() {
        "med" => "medium".to_string(),
        other => other.to_string(),
    };
    match normalized.as_str() {
        "low" => Some(EffortLevel::Low),
        "medium" => Some(EffortLevel::Medium),
        "high" => Some(EffortLevel::High),
        "xhigh" => Some(EffortLevel::Xhigh),
        "max" => Some(EffortLevel::Max),
        _ => None,
    }
}

/// Resolved state of the `LINGXI_EFFORT_LEVEL` env override
/// (`getEffortEnvOverride`).
enum EnvOverride {
    /// Env unset or unparseable — TS `undefined`.
    Unset,
    /// Env set to `unset` / `auto` — TS `null` (clears effort).
    Cleared,
    /// Env pins a concrete level — TS the parsed `EffortValue`. Carries the
    /// raw (un-normalized) string for the user-facing `={raw}` messages.
    Pinned { level: EffortLevel, raw: String },
}

/// `toPersistableEffort` / `Tve` (`Tve(e){if(e==="low"||e==="medium"||
/// e==="high"||e==="xhigh")return e;return}`) — the persistable subset of a
/// level. `low`/`medium`/`high`/`xhigh` persist; `max` is session-scoped, so it
/// returns `None`. Numeric efforts are ANT-only and already absent from
/// [`EffortLevel`].
fn to_persistable(level: EffortLevel) -> Option<EffortLevel> {
    match level {
        EffortLevel::Low | EffortLevel::Medium | EffortLevel::High | EffortLevel::Xhigh => {
            Some(level)
        }
        EffortLevel::Max => None,
    }
}

/// `<config-home>/settings.json` — `$LINGXI_CONFIG_DIR` when set (claude-code
/// `tr()` `??`: an empty value is honored verbatim), else `~/.claude`.
/// Byte-identical to the engine settings loader
/// (`core/src/settings/loader.rs` `config_home_dir` + `user_settings_path`)
/// so `/effort`'s persisted `effortLevel` lands in the SAME file the loader and
/// `/config` read. `None` if neither the env override nor `HOME` resolves (TS
/// `getSettingsFilePathForSource` → `null` → `{ error: null }`).
fn user_settings_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(branding::CONFIG_DIR_ENV) {
        return Some(PathBuf::from(dir).join("settings.json"));
    }
    std::env::var_os("HOME").map(|h| {
        PathBuf::from(h)
            .join(branding::DOT_DIR)
            .join("settings.json")
    })
}

/// Persist `effortLevel` into the user `settings.json`, mirroring
/// `updateSettingsForSource('userSettings', { effortLevel })`
/// (`settings.ts` L416). `Some(level)` writes the key; `None` deletes it
/// (TS `mergeWith` treats `undefined` as a delete, L483). All other keys are
/// preserved.
///
/// Faithful to the TS error contract: a missing/empty file merges into an
/// empty object, but a file whose JSON is syntactically broken is left
/// untouched and surfaces `Invalid JSON syntax …` (L459) rather than being
/// overwritten.
fn persist_effort_level(level: Option<EffortLevel>) -> Result<(), String> {
    // TS: filePath === null → { error: null }.
    let Some(path) = user_settings_path() else {
        return Ok(());
    };

    let selection = level.map(|level| platform_api::ReasoningSelection::Level {
        id: level.as_str().to_string(),
    });
    persist_reasoning_default_selection_at(&path, selection.as_ref())
}

/// Persist the structured reasoning default used by new sessions.
///
/// `None` and `Automatic` remove both the structured override and the legacy
/// root `effortLevel` mirror.  Only the provider-neutral discrete levels that
/// can be represented by the legacy setting are mirrored; toggles and budgets
/// intentionally clear that key so an older engine cannot apply a stale value.
pub fn persist_reasoning_default_selection_at(
    path: &Path,
    selection: Option<&platform_api::ReasoningSelection>,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to read raw settings from {}: {e}", path.display()))?;
    }
    let mut map: serde_json::Map<String, Value> = match std::fs::read_to_string(path) {
        Ok(content) if content.trim().is_empty() => serde_json::Map::new(),
        Ok(content) => serde_json::from_str(&content)
            .map_err(|_| format!("Invalid JSON syntax in settings file at {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(e) => {
            return Err(format!(
                "Failed to read raw settings from {}: {e}",
                path.display()
            ));
        }
    };

    let mut persisted = None;
    let default_selection = selection.and_then(|selection| match selection {
        platform_api::ReasoningSelection::Level { id }
            if matches!(id.as_str(), "low" | "medium" | "high" | "xhigh") =>
        {
            persisted = Some(json!(id));
            Some(json!({ "type": "level", "id": id }))
        }
        platform_api::ReasoningSelection::Disabled => Some(json!({ "type": "disabled" })),
        platform_api::ReasoningSelection::Enabled => Some(json!({ "type": "enabled" })),
        platform_api::ReasoningSelection::TokenBudget { tokens } => {
            Some(json!({ "type": "token_budget", "tokens": tokens }))
        }
        platform_api::ReasoningSelection::Automatic
        | platform_api::ReasoningSelection::Level { .. } => None,
    });

    if let Some(value) = persisted {
        map.insert("effortLevel".to_string(), value);
    } else {
        map.remove("effortLevel");
    }

    match default_selection {
        Some(value) => {
            let reasoning = map
                .entry("reasoning".to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if !reasoning.is_object() {
                *reasoning = Value::Object(serde_json::Map::new());
            }
            reasoning
                .as_object_mut()
                .expect("reasoning object normalized")
                .insert("defaultSelection".to_string(), value);
        }
        None => {
            let mut remove_reasoning = false;
            if let Some(Value::Object(reasoning)) = map.get_mut("reasoning") {
                reasoning.remove("defaultSelection");
                remove_reasoning = reasoning.is_empty();
            }
            if remove_reasoning {
                map.remove("reasoning");
            }
        }
    }

    let serialized = serde_json::to_string_pretty(&map)
        .map_err(|e| format!("Failed to read raw settings from {}: {e}", path.display()))?;
    std::fs::write(path, serialized + "\n")
        .map_err(|e| format!("Failed to read raw settings from {}: {e}", path.display()))?;
    Ok(())
}

/// Read the structured reasoning default, with a compatibility fallback for
/// the legacy root `effortLevel` setting.  Unrepresentable/session-only values
/// (notably `max`) are ignored so they cannot become a new-session default.
pub fn load_reasoning_default_selection_at(
    path: &Path,
) -> Option<platform_api::ReasoningSelection> {
    let content = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&content).ok()?;
    if let Some(default) = value
        .get("reasoning")
        .and_then(Value::as_object)
        .and_then(|reasoning| reasoning.get("defaultSelection"))
    {
        if let Ok(selection) =
            serde_json::from_value::<platform_api::ReasoningSelection>(default.clone())
        {
            return Some(selection);
        }
    }
    match value.get("effortLevel").and_then(Value::as_str) {
        Some(id @ ("low" | "medium" | "high" | "xhigh")) => {
            Some(platform_api::ReasoningSelection::Level { id: id.to_string() })
        }
        _ => None,
    }
}

/// `modelSupportsMaxEffort` (`effort.ts` L53) — the non-ant reachable subset:
/// `max` is Opus-4.6-only for public models. The 3P-override and ANT branches
/// are seam-blocked (no `get3PModelCapabilityOverride` / `resolveAntModel` in
/// this port).
fn model_supports_max_effort(model: &str) -> bool {
    model.to_lowercase().contains("opus-4-6")
}

/// `getDefaultEffortForModel` (`effort.ts` L279) — the non-ant reachable
/// subset.
///
/// Every TS branch that can return a non-`undefined` default is seam-blocked
/// in this port and so returns `None`:
///   * ANT model overrides (`resolveAntModel` / `getAntModelOverrideConfig`);
///   * Opus-4.6 → `medium` for Pro/Max/Team subscribers — needs the
///     subscriber-auth (`isProSubscriber` …) and `GrowthBook`
///     (`getOpusDefaultEffortConfig`) seams;
///   * ultrathink → `medium` — needs the `isUltrathinkEnabled` seam.
///
/// With none of those reachable in-tree the TS fallback (`return undefined`,
/// L328 — "resolve to high in the API") is the only live path, so this
/// returns `None` for every model. The fn exists to wire the precedence
/// chain; flipping any seam on later changes the answer without touching the
/// call sites.
fn get_default_effort_for_model(_model: &str) -> Option<EffortLevel> {
    None
}

/// `convertEffortValueToLevel` (`effort.ts` L202) — for the string levels this
/// port carries it is a passthrough (the numeric-coercion + `GrowthBook`
/// `'high'` guard only apply to numeric/remote values, which are ANT-only and
/// absent from [`EffortLevel`]).
fn convert_effort_value_to_level(level: EffortLevel) -> EffortLevel {
    level
}

/// `resolveAppliedEffort` (`effort.ts` L152) — the effort that would actually
/// be sent for `model`, following `env → app-state → model default`. `None`
/// means "send no effort param" (env cleared, or no default).
///
/// `app_state` mirrors the TS `appStateEffortValue` argument and is read from
/// the live orchestrator handle by `/effort`.
fn resolve_applied_effort(model: &str, app_state: Option<EffortLevel>) -> Option<EffortLevel> {
    match effort_env_override() {
        // envOverride === null → undefined.
        EnvOverride::Cleared => None,
        // envOverride ?? appState ?? getDefaultEffortForModel(model).
        env => {
            let resolved = match env {
                EnvOverride::Pinned { level, .. } => Some(level),
                _ => app_state.or_else(|| get_default_effort_for_model(model)),
            };
            // API rejects 'max' on non-Opus-4.6 — downgrade to 'high' (L163).
            match resolved {
                Some(EffortLevel::Max) if !model_supports_max_effort(model) => {
                    Some(EffortLevel::High)
                }
                other => other,
            }
        }
    }
}

/// `getDisplayedEffortLevel` (`effort.ts` L174) — [`resolve_applied_effort`]
/// with the `?? 'high'` API-default fallback, then `convertEffortValueToLevel`.
fn get_displayed_effort_level(model: &str, app_state: Option<EffortLevel>) -> EffortLevel {
    let resolved = resolve_applied_effort(model, app_state).unwrap_or(EffortLevel::High);
    clamp_effort(
        convert_effort_value_to_level(resolved),
        fold_max_effort_cap_from_disk(model),
    )
}

fn clamp_effort(level: EffortLevel, cap: Option<EffortLevel>) -> EffortLevel {
    match cap {
        Some(cap) if level.rank() > cap.rank() => cap,
        _ => level,
    }
}

fn model_settings_key_matches(key: &str, model: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let model = model.to_ascii_lowercase();
    key == model || (!key.is_empty() && (model.starts_with(&key) || key.starts_with(&model)))
}

fn parse_max_effort_token(raw: &str) -> Option<EffortLevel> {
    parse_effort_level(raw)
}

fn layer_max_effort_cap(value: &Value, model: &str, any_per_model: bool) -> Option<EffortLevel> {
    let mut per_model: Option<EffortLevel> = None;
    if any_per_model {
        if let Some(settings) = value.get("modelSettings").and_then(Value::as_object) {
            for (key, entry) in settings {
                if !model_settings_key_matches(key, model) {
                    continue;
                }
                let Some(level) = entry
                    .get("maxEffortLevel")
                    .and_then(Value::as_str)
                    .and_then(parse_max_effort_token)
                else {
                    continue;
                };
                per_model = Some(match per_model {
                    Some(prev) if level.rank() < prev.rank() => level,
                    Some(prev) => prev,
                    None => level,
                });
            }
        }
    }
    per_model.or_else(|| {
        value
            .get("maxEffortLevel")
            .and_then(Value::as_str)
            .and_then(parse_max_effort_token)
    })
}

/// Lowest applicable `maxEffortLevel` across settings JSON documents.
/// `"max"` does not cap. Per-model caps replace the top-level value inside
/// the same file when any layer carries `modelSettings.*.maxEffortLevel`.
fn fold_max_effort_cap<'a>(
    raws: impl IntoIterator<Item = &'a str>,
    model: &str,
) -> Option<EffortLevel> {
    let parsed: Vec<Value> = raws
        .into_iter()
        .filter_map(|raw| serde_json::from_str(raw).ok())
        .collect();
    let any_per_model = parsed.iter().any(|value| {
        value
            .get("modelSettings")
            .and_then(Value::as_object)
            .is_some_and(|settings| {
                settings
                    .values()
                    .any(|entry| entry.get("maxEffortLevel").is_some())
            })
    });
    let mut cap: Option<EffortLevel> = None;
    for value in &parsed {
        let Some(level) = layer_max_effort_cap(value, model, any_per_model) else {
            continue;
        };
        if level == EffortLevel::Max {
            continue;
        }
        cap = Some(match cap {
            Some(prev) if level.rank() < prev.rank() => level,
            Some(prev) => prev,
            None => level,
        });
    }
    cap
}

fn fold_max_effort_cap_from_disk(model: &str) -> Option<EffortLevel> {
    let mut raws = Vec::new();
    if let Some(path) = user_settings_path() {
        if let Ok(raw) = std::fs::read_to_string(path) {
            raws.push(raw);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        for name in ["settings.json", "settings.local.json"] {
            let path = cwd.join(branding::DOT_DIR).join(name);
            if let Ok(raw) = std::fs::read_to_string(path) {
                raws.push(raw);
            }
        }
    }
    if let Ok(level) = std::env::var("LINGXI_MAX_EFFORT_LEVEL") {
        if !level.trim().is_empty() {
            raws.push(format!(
                r#"{{"maxEffortLevel":{}}}"#,
                serde_json::Value::String(level)
            ));
        }
    }
    fold_max_effort_cap(raws.iter().map(String::as_str), model)
}

fn effort_exceeds_cap_message(requested: EffortLevel, model: &str, cap: EffortLevel) -> String {
    format!(
        "Effort '{}' exceeds the cap for {model} set by your settings or organization; using '{}'.",
        requested.as_str(),
        cap.as_str()
    )
}

/// Read and classify `LINGXI_EFFORT_LEVEL` (`getEffortEnvOverride`).
fn effort_env_override() -> EnvOverride {
    let Ok(raw) = std::env::var(EFFORT_ENV_VAR) else {
        return EnvOverride::Unset;
    };
    let normalized = raw.to_lowercase();
    if normalized == "unset" || normalized == "auto" {
        return EnvOverride::Cleared;
    }
    match parse_effort_level(&normalized) {
        Some(level) => EnvOverride::Pinned { level, raw },
        None => EnvOverride::Unset,
    }
}

/// `/effort` handler — sets or shows the effort level (session-only port).
#[derive(Clone)]
pub struct EffortHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl EffortHandler {
    /// Construct an `EffortHandler` bound to the given orchestrator handle.
    ///
    /// The handle is used to read the current model string for the
    /// `/effort` / `/effort current` branch (`getDisplayedEffortLevel`).
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }

    async fn supports_level(&self, level: EffortLevel) -> bool {
        let Some(controls) = self.handle.conversation_controls().await else {
            // Lightweight command hosts do not expose controls. Preserve their
            // legacy behavior; production orchestrators always return a spec.
            return true;
        };
        let selection = platform_api::ReasoningSelection::Level {
            id: level.as_str().to_string(),
        };
        controls
            .reasoning_spec
            .available
            .iter()
            .any(|candidate| candidate == &selection)
    }

    /// `showCurrentEffort` (`effort.tsx` L62-75) — the `''`/`current`/`status`
    /// branch. The app-state value is the orchestrator's live request effort.
    async fn show_current(&self) -> String {
        match effort_env_override() {
            EnvOverride::Pinned { level, .. } => {
                // env pins a level → it is the effective value.
                format!(
                    "Current effort level: {} ({})",
                    level.as_str(),
                    level.description()
                )
            }
            EnvOverride::Cleared => {
                let model = self.handle.get_status_snapshot().await.model;
                format!(
                    "Effort level: auto (currently {})",
                    get_displayed_effort_level(&model, None).as_str()
                )
            }
            EnvOverride::Unset => {
                // Effective value is undefined → TS renders
                // `Effort level: auto (currently {level})` where `{level}` is
                // `getDisplayedEffortLevel(model, appStateEffort)` (effort.ts
                // L178). The handle exposes the live app-state value directly;
                // this deliberately does not re-read `settings.json`.
                let model = self.handle.get_status_snapshot().await.model;
                let app_state = self
                    .handle
                    .current_effort()
                    .await
                    .as_deref()
                    .and_then(parse_effort_level);
                format!(
                    "Effort level: auto (currently {})",
                    get_displayed_effort_level(&model, app_state).as_str()
                )
            }
        }
    }

    /// `unsetEffortLevel` (`effort.tsx` L76-106) — the `auto`/`unset` branch.
    /// Deletes the persisted `effortLevel`; only the env-conflict note varies.
    async fn clear_effort(&self) -> String {
        // updateSettingsForSource('userSettings', { effortLevel: undefined }).
        let env = effort_env_override();
        let live = match &env {
            EnvOverride::Pinned { level, .. } => Some(level.as_str().to_string()),
            EnvOverride::Cleared | EnvOverride::Unset => None,
        };
        let previous = self.handle.current_effort().await;
        if let Err(error) = self.handle.set_effort_level(live).await {
            return format!("Failed to set effort level: {error}");
        }
        if let Err(msg) = persist_effort_level(None) {
            let _ = self.handle.set_effort_level(previous).await;
            return format!("Failed to set effort level: {msg}");
        }
        match env {
            EnvOverride::Pinned { raw, .. } => format!(
                "Cleared effort from settings, but {EFFORT_ENV_VAR}={raw} still controls this session"
            ),
            EnvOverride::Cleared | EnvOverride::Unset => "Effort level set to auto".to_string(),
        }
    }

    /// `setEffortValue` (`effort.tsx` L16-61) — the valid-level branch.
    async fn set_effort(&self, level: EffortLevel) -> String {
        let model = self.handle.get_status_snapshot().await.model;
        let cap = fold_max_effort_cap_from_disk(&model);
        let applied = clamp_effort(level, cap);
        let cap_notice =
            (applied != level).then(|| effort_exceeds_cap_message(level, &model, applied));
        // toPersistableEffort: low/medium/high persist, max is session-only.
        let persistable = to_persistable(applied);
        if !self.supports_level(applied).await {
            return format!(
                "Failed to set effort level: {} is unsupported for the active model",
                applied.as_str()
            );
        }

        let env = effort_env_override();
        let live = match &env {
            EnvOverride::Pinned {
                level: env_level, ..
            } => Some(clamp_effort(*env_level, cap).as_str().to_string()),
            EnvOverride::Cleared => None,
            EnvOverride::Unset => Some(applied.as_str().to_string()),
        };
        let previous = self.handle.current_effort().await;
        if let Err(error) = self.handle.set_effort_level(live).await {
            return format!("Failed to set effort level: {error}");
        }
        if persistable.is_some() {
            if let Err(msg) = persist_effort_level(Some(applied)) {
                let _ = self.handle.set_effort_level(previous).await;
                return format!("Failed to set effort level: {msg}");
            }
        }

        // TS flags env conflict only when env pins a *different* level than the
        // one the user asked for (`envOverride !== effortValue`). The note
        // wording then branches on whether the level was persistable.
        match env {
            EnvOverride::Pinned {
                level: env_level,
                raw,
            } if env_level != applied => {
                if persistable.is_none() {
                    // Session-only level can't outlast the env (L38).
                    format!(
                        "Not applied: {EFFORT_ENV_VAR}={raw} overrides effort this session, and {} is session-only (nothing saved)",
                        applied.as_str()
                    )
                } else {
                    // Persisted, but env wins until cleared (L47).
                    format!(
                        "{EFFORT_ENV_VAR}={raw} overrides this session — clear it and {} takes over",
                        applied.as_str()
                    )
                }
            }
            // No conflict → `Set effort level to {x}{suffix}: {desc}`. Per `Ium`
            // (v2.1.183) the suffix is `" (saved as your default for new
            // sessions)"` for a persistable level and `" (this session only)"`
            // for a session-only (`max`) level.
            _ => {
                if let Some(notice) = cap_notice {
                    return notice;
                }
                let suffix = if persistable.is_some() {
                    " (saved as your default for new sessions)"
                } else {
                    " (this session only)"
                };
                format!(
                    "Set effort level to {}{suffix}: {}",
                    applied.as_str(),
                    applied.description()
                )
            }
        }
    }
}

#[async_trait]
impl BuiltinCommandHandler for EffortHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        let trimmed = args.raw_args.trim();
        // Help args are matched on the raw (trimmed) token, mirroring
        // COMMON_HELP_ARGS.includes(args) in TS (case-sensitive there).
        if matches!(trimmed, "help" | "-h" | "--help") {
            return CommandResult::Done {
                display: Some(usage_with(self.handle.dynamic_workflows_enabled().await)),
            };
        }

        let normalized = trimmed.to_lowercase();
        let workflows_enabled = self.handle.dynamic_workflows_enabled().await;
        let display = if normalized.is_empty() || normalized == "current" || normalized == "status"
        {
            self.show_current().await
        } else if normalized == "auto" || normalized == "unset" {
            self.clear_effort().await
        } else if normalized == "ultracode" {
            // `ZVn`: `ultracode` routes to `Pum`, the gated handler. When the
            // dynamic-workflow seam is off it returns the "needs dynamic
            // workflows enabled" guidance (`Pum` L1); when on it maps to `xhigh`
            // (with the dynamic-orchestration suffix).
            if workflows_enabled {
                // ultracode → xhigh (Hum/Pum). Session-only orchestration.
                self.set_effort(EffortLevel::Xhigh).await
            } else {
                // `Pum` gate-off message (verbatim, v2.1.183).
                "Ultracode needs dynamic workflows enabled (see /config). Valid options are: low, medium, high, xhigh, max, auto".to_string()
            }
        } else if let Some(level) = parse_effort_level(&normalized) {
            self.set_effort(level).await
        } else {
            // `ZVn` invalid-arg branch — uses the original (un-normalized,
            // trimmed) argument text. The `ultracode,` hint is appended only
            // when the dynamic-workflow seam is on (`x4(js())`).
            let ultracode_hint = if workflows_enabled { " ultracode," } else { "" };
            format!(
                "Invalid argument: {trimmed}. Valid options are: low, medium, high, xhigh, max,{ultracode_hint} auto"
            )
        };

        CommandResult::Done {
            display: Some(display),
        }
    }

    fn name(&self) -> &str {
        "effort"
    }

    fn description(&self) -> &str {
        // Verbatim TS metadata (effort/index.ts) — these handlers are actually
        // implemented, so they carry the real description rather than the
        // `core_description` "(unimplemented)" fallback the pass-1 stubs use.
        "Set effort level for model usage"
    }
}

#[cfg(test)]
// The env-serialization guard is deliberately held across the command's
// `.await`: it keeps `LINGXI_EFFORT_LEVEL` stable for the duration of
// `handle()` so the process-global env var can't race between parallel tests.
// `#[tokio::test]` runs on a single-thread runtime and nothing re-locks
// `ENV_LOCK` inside the awaited future, so there is no deadlock risk.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;

    /// Env-mutating tests must run serialized: they share the one process-wide
    /// `LINGXI_EFFORT_LEVEL` *and* `HOME` (now that the set/clear paths
    /// write `~/.lingxi/settings.json`). A module-level mutex serializes them.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII test fixture: holds [`ENV_LOCK`], redirects `HOME` to a fresh
    /// per-test temp dir (so persistence never touches the real `~/.claude`),
    /// and clears `LINGXI_EFFORT_LEVEL`. On drop it restores the prior
    /// `HOME` and removes the temp dir. Mirrors the `HOME_LOCK` pattern in
    /// `core/src/settings`; uses `std::env::temp_dir()` rather than the
    /// `tempfile` crate, matching the `export.rs` test precedent (no new dep).
    struct TestEnv {
        _guard: std::sync::MutexGuard<'static, ()>,
        home: PathBuf,
        prev_home: Option<std::ffi::OsString>,
    }

    impl TestEnv {
        fn new() -> Self {
            let guard = ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prev_home = std::env::var_os("HOME");
            // Unique per process + per nanosecond so parallel binaries / repeat
            // runs never collide.
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let home = std::env::temp_dir()
                .join(format!("lingxi-effort-test-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&home).unwrap();
            std::env::set_var("HOME", &home);
            std::env::remove_var(EFFORT_ENV_VAR);
            Self {
                _guard: guard,
                home,
                prev_home,
            }
        }

        /// The redirected `~/.lingxi/settings.json` path.
        fn settings_path(&self) -> PathBuf {
            self.home.join(".lingxi").join("settings.json")
        }

        /// Pre-seed `settings.json` with the given raw bytes (for the
        /// merge / broken-JSON tests).
        fn write_settings(&self, raw: &str) {
            let path = self.settings_path();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, raw).unwrap();
        }

        /// Read `settings.json` back as a JSON object, or `None` if absent.
        fn read_settings(&self) -> Option<serde_json::Map<String, Value>> {
            std::fs::read_to_string(self.settings_path())
                .ok()
                .map(|c| serde_json::from_str(&c).unwrap())
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            match &self.prev_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
            std::env::remove_var(EFFORT_ENV_VAR);
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    fn args(raw: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "effort".to_string(),
            raw_args: raw.to_string(),
            positional_args: vec![],
        }
    }

    fn handler() -> EffortHandler {
        EffortHandler::new(Arc::new(MockOrchestratorHandle::new()))
    }

    fn handler_with_gate(enabled: bool) -> EffortHandler {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_dynamic_workflows_gate(enabled, false);
        EffortHandler::new(mock)
    }

    async fn run(raw: &str) -> String {
        match handler().handle(&args(raw)).await {
            CommandResult::Done { display: Some(s) } => s,
            other => panic!("expected Done with display, got {other:?}"),
        }
    }

    async fn run_with_gate(raw: &str, enabled: bool) -> String {
        match handler_with_gate(enabled).handle(&args(raw)).await {
            CommandResult::Done { display: Some(s) } => s,
            other => panic!("expected Done with display, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn help_args_render_usage() {
        let _env = TestEnv::new();
        for raw in ["help", "-h", "--help", "  help  "] {
            assert_eq!(run(raw).await, usage_with(false));
        }
    }

    #[test]
    fn usage_is_byte_exact_with_gate_off() {
        // `getEffortHelpText` (`XVn`) with `e = x4(js()) === false`:
        // `xhigh` present, `ultracode` absent. Square brackets per `XVn`.
        assert_eq!(
            usage_with(false),
            "Usage: /effort [low|medium|high|xhigh|max|auto]\n\n\
Effort levels:\n\
- low: Quick, straightforward implementation\n\
- medium: Balanced approach with standard testing\n\
- high: Comprehensive implementation with extensive testing\n\
- xhigh: Extended reasoning with thorough analysis (Fable 5, Opus 4.7+, Sonnet 5)\n\
- max: Maximum capability with deepest reasoning (Fable 5, Opus 4.6+, Sonnet 4.6+)\n\
- auto: Use the default effort level for your model"
        );
    }

    #[test]
    fn usage_is_byte_exact_with_gate_on() {
        // `XVn` with `e === true`: `|ultracode` joins the bracket list and the
        // `- ultracode:` line is appended before `- auto:`.
        assert_eq!(
            usage_with(true),
            "Usage: /effort [low|medium|high|xhigh|max|ultracode|auto]\n\n\
Effort levels:\n\
- low: Quick, straightforward implementation\n\
- medium: Balanced approach with standard testing\n\
- high: Comprehensive implementation with extensive testing\n\
- xhigh: Extended reasoning with thorough analysis (Fable 5, Opus 4.7+, Sonnet 5)\n\
- max: Maximum capability with deepest reasoning (Fable 5, Opus 4.6+, Sonnet 4.6+)\n\
- ultracode: xhigh + dynamic workflow orchestration (this session only)\n\
- auto: Use the default effort level for your model"
        );
    }

    #[test]
    fn xhigh_parses_and_describes() {
        // `hQe("xhigh")` / `NXu("xhigh")`.
        assert_eq!(parse_effort_level("xhigh"), Some(EffortLevel::Xhigh));
        assert_eq!(parse_effort_level("  XHIGH  "), Some(EffortLevel::Xhigh));
        assert_eq!(
            EffortLevel::Xhigh.description(),
            "Deeper reasoning than high, just below maximum (Fable 5, Opus 4.7+, Sonnet 5)"
        );
        // `_Ai = {med: "medium"}` alias.
        assert_eq!(parse_effort_level("med"), Some(EffortLevel::Medium));
        // `ultracode` is NOT a parse-level member (routed separately).
        assert_eq!(parse_effort_level("ultracode"), None);
    }

    #[test]
    fn max_effort_cap_lowest_wins_and_skips_max() {
        assert_eq!(
            fold_max_effort_cap([r#"{"maxEffortLevel":"high"}"#], "claude-opus-4-7"),
            Some(EffortLevel::High)
        );
        assert_eq!(
            fold_max_effort_cap(
                [
                    r#"{"maxEffortLevel":"high"}"#,
                    r#"{"maxEffortLevel":"low"}"#,
                ],
                "claude-opus-4-7"
            ),
            Some(EffortLevel::Low)
        );
        assert_eq!(
            fold_max_effort_cap([r#"{"maxEffortLevel":"max"}"#], "claude-opus-4-7"),
            None
        );
        assert_eq!(
            fold_max_effort_cap(
                [
                    r#"{"maxEffortLevel":"high","modelSettings":{"claude-opus-4-7":{"maxEffortLevel":"low"}}}"#
                ],
                "claude-opus-4-7"
            ),
            Some(EffortLevel::Low)
        );
        assert_eq!(
            fold_max_effort_cap(
                [
                    r#"{"maxEffortLevel":"high","modelSettings":{"claude-opus-4-7":{"maxEffortLevel":"max"}}}"#
                ],
                "claude-opus-4-7"
            ),
            None,
            "per-model max exempts that model"
        );
    }

    #[tokio::test]
    async fn max_effort_level_clamps_xhigh_to_high() {
        let env = TestEnv::new();
        env.write_settings(r#"{"maxEffortLevel":"high"}"#);
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_status_snapshot(platform_api::StatusSnapshot {
            model: "claude-opus-4-7".into(),
            ..Default::default()
        });
        let handler = EffortHandler::new(mock.clone());
        match handler.handle(&args("xhigh")).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(
                    s,
                    "Effort 'xhigh' exceeds the cap for claude-opus-4-7 set by your settings or organization; using 'high'."
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
        assert_eq!(mock.current_effort().await.as_deref(), Some("high"));
        assert_eq!(
            env.read_settings().unwrap().get("effortLevel"),
            Some(&json!("high"))
        );
    }

    #[tokio::test]
    async fn set_xhigh_is_persisted_as_default() {
        let env = TestEnv::new();
        // xhigh is persistable (`Tve`) → "(saved as your default …)" suffix +
        // written to settings.json.
        assert_eq!(
            run("xhigh").await,
            "Set effort level to xhigh (saved as your default for new sessions): Deeper reasoning than high, just below maximum (Fable 5, Opus 4.7+, Sonnet 5)"
        );
        assert_eq!(
            env.read_settings().unwrap().get("effortLevel"),
            Some(&json!("xhigh"))
        );
        assert_eq!(
            env.read_settings()
                .unwrap()
                .get("reasoning")
                .and_then(Value::as_object)
                .and_then(|reasoning| reasoning.get("defaultSelection")),
            Some(&json!({"type": "level", "id": "xhigh"}))
        );
    }

    #[test]
    fn structured_default_migrates_and_clears_legacy_mirror() {
        let env = TestEnv::new();
        let path = env.settings_path();
        persist_reasoning_default_selection_at(
            &path,
            Some(&platform_api::ReasoningSelection::TokenBudget { tokens: 12_345 }),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            value["reasoning"]["defaultSelection"]["type"],
            "token_budget"
        );
        assert!(value.get("effortLevel").is_none());
        assert_eq!(
            load_reasoning_default_selection_at(&path),
            Some(platform_api::ReasoningSelection::TokenBudget { tokens: 12_345 })
        );

        persist_reasoning_default_selection_at(
            &path,
            Some(&platform_api::ReasoningSelection::Automatic),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(value.get("reasoning").is_none());
        assert!(value.get("effortLevel").is_none());
    }

    #[tokio::test]
    async fn set_and_clear_update_the_live_effort_state() {
        let _env = TestEnv::new();
        let mock = Arc::new(MockOrchestratorHandle::new());
        let handler = EffortHandler::new(mock.clone());

        let result = handler.handle(&args("medium")).await;
        assert!(matches!(result, CommandResult::Done { .. }));
        assert_eq!(mock.current_effort().await.as_deref(), Some("medium"));

        let result = handler.handle(&args("auto")).await;
        assert!(matches!(result, CommandResult::Done { .. }));
        assert_eq!(mock.current_effort().await, None);
    }

    #[tokio::test]
    async fn ultracode_gated_off_renders_guidance() {
        let _env = TestEnv::new();
        // `Pum` gate-off branch (dynamic workflows disabled in this port).
        assert_eq!(
            run("ultracode").await,
            "Ultracode needs dynamic workflows enabled (see /config). Valid options are: low, medium, high, xhigh, max, auto"
        );
        assert_eq!(
            run("ULTRACODE").await,
            "Ultracode needs dynamic workflows enabled (see /config). Valid options are: low, medium, high, xhigh, max, auto"
        );
    }

    #[tokio::test]
    async fn current_with_no_env_renders_auto_subset() {
        let _env = TestEnv::new();
        for raw in ["", "  ", "current", "status", "CURRENT"] {
            assert_eq!(run(raw).await, "Effort level: auto (currently high)");
        }
    }

    #[tokio::test]
    async fn current_with_env_pinned_renders_effective_level() {
        let _env = TestEnv::new();
        std::env::set_var(EFFORT_ENV_VAR, "high");
        assert_eq!(
            run("current").await,
            "Current effort level: high (Comprehensive implementation with extensive testing and documentation)"
        );
    }

    #[tokio::test]
    async fn current_with_env_cleared_renders_auto_subset() {
        let _env = TestEnv::new();
        std::env::set_var(EFFORT_ENV_VAR, "unset");
        assert_eq!(run("").await, "Effort level: auto (currently high)");
    }

    #[tokio::test]
    async fn set_valid_level_no_env_is_session_only() {
        let env = TestEnv::new();
        // low/medium/high/xhigh are persistable → "(saved as your default …)".
        assert_eq!(
            run("medium").await,
            "Set effort level to medium (saved as your default for new sessions): Balanced approach with standard implementation and testing"
        );
        // max stays session-only (toPersistableEffort(max) === undefined for
        // non-ant) → suffix kept, nothing written.
        assert_eq!(
            run("MAX").await,
            "Set effort level to max (this session only): Maximum capability with deepest reasoning. May use excessive tokens resulting in long response times or overthinking. Use sparingly for the hardest tasks."
        );
        // Persisted value is the last *persistable* set (medium); max didn't
        // overwrite it.
        assert_eq!(
            env.read_settings().unwrap().get("effortLevel"),
            Some(&json!("medium"))
        );
    }

    #[tokio::test]
    async fn set_level_conflicting_env_is_not_applied() {
        let _env = TestEnv::new();
        // env=low, ask max (non-persistable) → "Not applied … nothing saved".
        std::env::set_var(EFFORT_ENV_VAR, "low");
        assert_eq!(
            run("max").await,
            "Not applied: LINGXI_EFFORT_LEVEL=low overrides effort this session, and max is session-only (nothing saved)"
        );
    }

    #[tokio::test]
    async fn set_level_matching_env_has_no_conflict_note() {
        let _env = TestEnv::new();
        std::env::set_var(EFFORT_ENV_VAR, "high");
        assert_eq!(
            run("high").await,
            "Set effort level to high (saved as your default for new sessions): Comprehensive implementation with extensive testing and documentation"
        );
    }

    #[tokio::test]
    async fn clear_no_env_sets_auto() {
        let _env = TestEnv::new();
        assert_eq!(run("auto").await, "Effort level set to auto");
        assert_eq!(run("unset").await, "Effort level set to auto");
    }

    #[tokio::test]
    async fn clear_with_env_pinned_warns() {
        let _env = TestEnv::new();
        std::env::set_var(EFFORT_ENV_VAR, "high");
        assert_eq!(
            run("auto").await,
            "Cleared effort from settings, but LINGXI_EFFORT_LEVEL=high still controls this session"
        );
    }

    #[tokio::test]
    async fn invalid_arg_message() {
        let _env = TestEnv::new();
        assert_eq!(
            run("bogus").await,
            "Invalid argument: bogus. Valid options are: low, medium, high, xhigh, max, auto"
        );
    }

    #[tokio::test]
    async fn name_and_description() {
        let _env = TestEnv::new();
        let h = handler();
        assert_eq!(h.name(), "effort");
        assert_eq!(h.description(), "Set effort level for model usage");
    }

    #[test]
    fn max_description_and_usage_match_binary_v2_1_220() {
        // `getEffortLevelDescription("max")` renders the TS template
        // `Maximum capability with deepest reasoning. ${qHt}`.
        assert_eq!(
            EffortLevel::Max.description(),
            "Maximum capability with deepest reasoning. May use excessive tokens resulting in long response times or overthinking. Use sparingly for the hardest tasks."
        );
        // The Usage block's `- max:` line renders the Usage template
        // `Maximum capability with deepest reasoning (${gAi})` with
        // `gAi="Fable 5, Opus 4.6+, Sonnet 4.6+"`.
        assert!(usage_with(false).contains(
            "- max: Maximum capability with deepest reasoning (Fable 5, Opus 4.6+, Sonnet 4.6+)\n"
        ));
    }

    #[tokio::test]
    async fn help_and_invalid_arg_follow_the_handle_gate() {
        let _env = TestEnv::new();
        assert_eq!(run_with_gate("help", true).await, usage_with(true));
        assert_eq!(
            run_with_gate("bogus", true).await,
            "Invalid argument: bogus. Valid options are: low, medium, high, xhigh, max, ultracode, auto"
        );
    }

    // ---- persistence (updateSettingsForSource) parity ----

    #[tokio::test]
    async fn persist_creates_settings_and_writes_effort_level() {
        let env = TestEnv::new();
        assert_eq!(
            run("high").await,
            "Set effort level to high (saved as your default for new sessions): Comprehensive implementation with extensive testing and documentation"
        );
        let map = env.read_settings().expect("settings.json written");
        assert_eq!(map.get("effortLevel"), Some(&json!("high")));
        assert_eq!(map.len(), 2);
    }

    #[tokio::test]
    async fn persist_merges_into_existing_settings() {
        let env = TestEnv::new();
        env.write_settings("{\"model\":\"opus\"}");
        run("low").await;
        let map = env.read_settings().unwrap();
        assert_eq!(map.get("model"), Some(&json!("opus")));
        assert_eq!(map.get("effortLevel"), Some(&json!("low")));
    }

    #[tokio::test]
    async fn persist_max_is_session_only_not_written() {
        let env = TestEnv::new();
        env.write_settings("{}");
        assert_eq!(
            run("max").await,
            "Set effort level to max (this session only): Maximum capability with deepest reasoning. May use excessive tokens resulting in long response times or overthinking. Use sparingly for the hardest tasks."
        );
        assert!(env.read_settings().unwrap().get("effortLevel").is_none());
    }

    #[tokio::test]
    async fn clear_removes_effort_level() {
        let env = TestEnv::new();
        env.write_settings("{\"effortLevel\":\"high\",\"model\":\"x\"}");
        assert_eq!(run("auto").await, "Effort level set to auto");
        let map = env.read_settings().unwrap();
        assert!(map.get("effortLevel").is_none());
        assert_eq!(map.get("model"), Some(&json!("x")));
    }

    #[tokio::test]
    async fn broken_settings_json_not_overwritten() {
        let env = TestEnv::new();
        let raw = "{ bad json";
        env.write_settings(raw);
        let msg = run("high").await;
        let path = env.settings_path();
        assert_eq!(
            msg,
            format!(
                "Failed to set effort level: Invalid JSON syntax in settings file at {}",
                path.display()
            )
        );
        // File bytes must be untouched (parity with TS L459).
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
    }

    #[tokio::test]
    async fn set_conflicting_env_with_persistable_uses_override_note() {
        let env = TestEnv::new();
        std::env::set_var(EFFORT_ENV_VAR, "low");
        assert_eq!(
            run("high").await,
            "LINGXI_EFFORT_LEVEL=low overrides this session — clear it and high takes over"
        );
        // Persistable set still wrote to disk (env only wins at resolve time).
        assert_eq!(
            env.read_settings().unwrap().get("effortLevel"),
            Some(&json!("high"))
        );
    }

    #[tokio::test]
    async fn current_after_persist_still_renders_auto_high() {
        let _env = TestEnv::new();
        // Persist a level…
        run("high").await;
        // …then `current` still renders auto, because showCurrentEffort reads
        // appStateEffort (None here), not settings.json.
        assert_eq!(run("current").await, "Effort level: auto (currently high)");
    }

    // ---- resolver unit (effort.ts) ----

    #[test]
    fn resolver_unit() {
        // `get_displayed_effort_level` reads `LINGXI_EFFORT_LEVEL`, so
        // serialize against the env-mutating tests (TestEnv clears it).
        let _env = TestEnv::new();
        assert_eq!(get_default_effort_for_model("claude-opus-4-6"), None);
        assert_eq!(
            get_displayed_effort_level("claude-opus-4-6", None),
            EffortLevel::High
        );
        assert!(model_supports_max_effort("claude-opus-4-6"));
        assert!(!model_supports_max_effort("claude-sonnet-4-6"));
    }
}
