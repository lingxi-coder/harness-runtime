//! `ConfigTool` — get/set LingXi settings against `~/.lingxi/settings.json`.
//!
//! no-truncation: ConfigTool returns a single bounded value — one setting's
//! value (get) or a short confirmation/status (set/list) — never large content,
//! so it intentionally opts out of output truncation (matches claude-code,
//! whose ConfigTool result is a small status object).
//!
//! Ports `ConfigTool/supportedSettings.ts` (the `SUPPORTED_SETTINGS` registry +
//! `isSupported`/`getConfig`/`getOptionsForSetting`/`getPath`) and
//! `ConfigTool/ConfigTool.ts` (get/set semantics, boolean string-coercion,
//! options validation, `buildNestedObject`/`getValue`).
//!
//! Input shape (TS `{setting, value?}`): `value` omitted ⇒ GET, present ⇒ SET.
//!
//! Backing store: this Rust stub keeps a single file (`~/.lingxi/settings.json`)
//! for both `source: 'global'` and `source: 'settings'` entries — the upstream
//! global-config (`~/.lingxi.json`) vs settings-file split has no substrate
//! here. Dotted `path` (e.g. `permissions.defaultMode`) is honored via
//! `build_nested_object`/`get_value`.
//!
//! Known gaps vs TS (no Rust substrate): the `feature()`-gated voice/bridge/
//! kairos/ant settings, `validateOnWrite` (async model API check),
//! `formatOnRead`, `appStateKey` AppState sync, and the global-config /
//! settings-file source split. The registry below carries an extension hook
//! (`SettingConfig`) so the gated entries can be added when those flags land.
//!
//! This tool operates directly on the JSON and does NOT consult the frozen
//! `settings::SettingsJson` schema — preserving that boundary explicitly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::{PermissionMetadata, PermissionPrompt};
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Map, Value};
use telemetry::pii::{PiiTagged, Verified};
use telemetry::sink::{AnalyticsValue, LogEventMetadata};
use telemetry::tengu::tool::{CONFIG_COMPLETED, CONFIG_FAILED, CONFIG_STARTED};
use telemetry::AnalyticsBus;

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext, ValidationError,
};

/// Tool name byte-lock (`ConfigTool/constants.ts:1`).
pub const CONFIG_TOOL_NAME: &str = "Config";
/// Backing-store filename.
pub const CONFIG_FILE_NAME: &str = "settings.json";
/// `~/.lingxi/` subdirectory.
pub const CONFIG_SUBDIR: &str = branding::DOT_DIR;

// ─── legacy wire-identifier surface (pre-registry M4-08 parity fixture) ───
//
// These four constants predate the `SUPPORTED_SETTINGS` registry and no longer
// drive `call` (the tool now keys off the registry + `{setting, value?}` input
// shape). They are retained only as the byte-locked wire identifiers asserted by
// the `parity/fixtures/system_tools.json` driver in the `test-harness` crate.

/// Legacy allowed config field: model.
pub const CONFIG_FIELD_MODEL: &str = "model";
/// Legacy allowed config field: output style (camelCase wire key).
pub const CONFIG_FIELD_OUTPUT_STYLE: &str = "outputStyle";
/// Legacy allowed config field: theme.
pub const CONFIG_FIELD_THEME: &str = "theme";
/// Legacy allowed config field: verbose toggle.
pub const CONFIG_FIELD_VERBOSE: &str = "verbose";
/// Legacy order-locked allowlist (the original M4-08 4-field set). Superseded
/// by [`SUPPORTED_SETTINGS`]; kept for the parity fixture.
pub const CONFIG_FIELDS_ALLOWED: [&str; 4] = [
    CONFIG_FIELD_MODEL,
    CONFIG_FIELD_OUTPUT_STYLE,
    CONFIG_FIELD_THEME,
    CONFIG_FIELD_VERBOSE,
];

// ─── byte-faithful option allowlists (utils/configConstants.ts, utils/theme.ts) ───

/// `THEME_NAMES` (`utils/theme.ts:91-98`).
const THEME_NAMES: &[&str] = &[
    "dark",
    "light",
    "light-daltonized",
    "dark-daltonized",
    "light-ansi",
    "dark-ansi",
];
/// `EDITOR_MODES` (`utils/configConstants.ts:15`).
const EDITOR_MODES: &[&str] = &["normal", "vim"];
/// `NOTIFICATION_CHANNELS` (`utils/configConstants.ts:4-12`).
const NOTIFICATION_CHANNELS: &[&str] = &[
    "auto",
    "iterm2",
    "iterm2_with_bell",
    "terminal_bell",
    "kitty",
    "ghostty",
    "notifications_disabled",
];
/// `TEAMMATE_MODES` (`utils/configConstants.ts:21`).
const TEAMMATE_MODES: &[&str] = &["auto", "tmux", "in-process"];
/// `permissions.defaultMode` options (non-`TRANSCRIPT_CLASSIFIER` branch,
/// `supportedSettings.ts:117-119`).
const PERMISSION_DEFAULT_MODES: &[&str] = &["default", "plan", "acceptEdits", "dontAsk"];
/// `askUserQuestionTimeout` options — byte-faithful to the settings schema
/// enum `askUserQuestionTimeout:E.enum(["60s","5m","10m","never"])` (oracle
/// 2.1.201). Default is `never` (block on the user; no auto-continue).
const ASK_USER_QUESTION_TIMEOUTS: &[&str] = &["60s", "5m", "10m", "never"];
/// `dialogExpiry` — 2.1.232 `_Vp`.
const DIALOG_EXPIRY: &[&str] = lingxi_core::host::live_sessions::DIALOG_EXPIRY_OPTIONS;
/// `crossSessionInbound` — 2.1.232 `bVp`.
const CROSS_SESSION_INBOUND: &[&str] =
    lingxi_core::host::live_sessions::CROSS_SESSION_INBOUND_OPTIONS;

/// Storage source for a setting. Both currently back to the single
/// `~/.lingxi/settings.json` file in this Rust stub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Upstream: `~/.lingxi.json` global config.
    Global,
    /// Upstream: the user settings file.
    Settings,
}

/// Value type of a setting (`SettingConfig.type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingType {
    /// `'boolean'` — accepts bool, coerces `"true"`/`"false"` strings.
    Boolean,
    /// `'string'`.
    String,
}

/// One row of the supported-settings registry — the ported subset of
/// `supportedSettings.ts`'s `SettingConfig`.
///
/// Extension hook: gated voice/bridge/kairos/ant entries (and the
/// `validateOnWrite`/`formatOnRead`/`appStateKey` machinery) are intentionally
/// omitted; add them here when the corresponding feature-flag substrate exists.
#[derive(Debug, Clone, Copy)]
pub struct SettingConfig {
    /// Where the value is stored (`source`).
    pub source: Source,
    /// Value type (`type`).
    pub ty: SettingType,
    /// Allowlist of valid string values, if constrained (`options`).
    pub options: Option<&'static [&'static str]>,
    /// Explicit dotted storage path, if it differs from `key.split('.')`
    /// (`path`). `None` ⇒ derive from the key.
    pub path: Option<&'static [&'static str]>,
}

/// `SUPPORTED_SETTINGS` — the static, non-feature-gated core registry
/// (`supportedSettings.ts:29-133`). Feature-gated entries (`ant`/`VOICE_MODE`/
/// `BRIDGE_MODE`/`KAIROS`) are omitted: no build-flag substrate.
pub static SUPPORTED_SETTINGS: Lazy<HashMap<&'static str, SettingConfig>> = Lazy::new(|| {
    let mut m: HashMap<&'static str, SettingConfig> = HashMap::new();
    // theme (options: THEME_NAMES — non-AUTO_THEME branch)
    m.insert(
        "theme",
        SettingConfig {
            source: Source::Global,
            ty: SettingType::String,
            options: Some(THEME_NAMES),
            path: None,
        },
    );
    // editorMode
    m.insert(
        "editorMode",
        SettingConfig {
            source: Source::Global,
            ty: SettingType::String,
            options: Some(EDITOR_MODES),
            path: None,
        },
    );
    // verbose
    m.insert(
        "verbose",
        SettingConfig {
            source: Source::Global,
            ty: SettingType::Boolean,
            options: None,
            path: None,
        },
    );
    // preferredNotifChannel
    m.insert(
        "preferredNotifChannel",
        SettingConfig {
            source: Source::Global,
            ty: SettingType::String,
            options: Some(NOTIFICATION_CHANNELS),
            path: None,
        },
    );
    // autoCompactEnabled
    m.insert(
        "autoCompactEnabled",
        SettingConfig {
            source: Source::Global,
            ty: SettingType::Boolean,
            options: None,
            path: None,
        },
    );
    // autoMemoryEnabled
    m.insert(
        "autoMemoryEnabled",
        SettingConfig {
            source: Source::Settings,
            ty: SettingType::Boolean,
            options: None,
            path: None,
        },
    );
    // autoDreamEnabled
    //
    // ⚠️ MEM-4, re-verified at the 2.1.270 oracle 2026-09-14: this key is
    // settable and readable here, but NOTHING schedules an auto-dream run —
    // `tasks::handlers::dream` is a complete HANDLER with no scheduler in front
    // of it. That is deliberate, and the reason is upstream's own gate:
    //
    // ```js
    // function tQn(){ return H("tengu_onyx_plover", null) }      // server object, default null
    // function _Qe(){ let e = tQn(); return e?.enabled===true || e?.available===true }
    // function oRe(){ if (!_Qe()) return false;                  // ← flag first
    //                 let e = Ge().autoDreamEnabled;
    //                 if (e !== undefined) return e;
    //                 return tQn()?.enabled === true }
    // ```
    //
    // With the flag null — which is what an unconfigured install sees — `_Qe()`
    // is false and the USER SETTING IS IGNORED ENTIRELY. So auto-dream is
    // dormant in the shipping binary too, and a faithful port of the scheduler
    // would be code that can never run.
    //
    // ⇒ Building it is a product decision (drop the server gate this port has
    // no analogue for, and let the setting alone arm it), not a parity fix. The
    // missing pieces are the 24h + 5-session gate and the consolidation LOCK —
    // a file whose mtime is `lastConsolidatedAt` — which `dream.rs`'s `kill`
    // already documents as belonging to the scheduler, not the handler.
    m.insert(
        "autoDreamEnabled",
        SettingConfig {
            source: Source::Settings,
            ty: SettingType::Boolean,
            options: None,
            path: None,
        },
    );
    // fileCheckpointingEnabled
    m.insert(
        "fileCheckpointingEnabled",
        SettingConfig {
            source: Source::Global,
            ty: SettingType::Boolean,
            options: None,
            path: None,
        },
    );
    // showTurnDuration
    m.insert(
        "showTurnDuration",
        SettingConfig {
            source: Source::Global,
            ty: SettingType::Boolean,
            options: None,
            path: None,
        },
    );
    // terminalProgressBarEnabled
    m.insert(
        "terminalProgressBarEnabled",
        SettingConfig {
            source: Source::Global,
            ty: SettingType::Boolean,
            options: None,
            path: None,
        },
    );
    // todoFeatureEnabled
    m.insert(
        "todoFeatureEnabled",
        SettingConfig {
            source: Source::Global,
            ty: SettingType::Boolean,
            options: None,
            path: None,
        },
    );
    // model (getOptions/validateOnWrite/formatOnRead/appStateKey omitted — no substrate)
    m.insert(
        "model",
        SettingConfig {
            source: Source::Settings,
            ty: SettingType::String,
            options: None,
            path: None,
        },
    );
    // alwaysThinkingEnabled
    m.insert(
        "alwaysThinkingEnabled",
        SettingConfig {
            source: Source::Settings,
            ty: SettingType::Boolean,
            options: None,
            path: None,
        },
    );
    // permissions.defaultMode (non-TRANSCRIPT_CLASSIFIER options)
    m.insert(
        "permissions.defaultMode",
        SettingConfig {
            source: Source::Settings,
            ty: SettingType::String,
            options: Some(PERMISSION_DEFAULT_MODES),
            path: None,
        },
    );
    // language
    m.insert(
        "language",
        SettingConfig {
            source: Source::Settings,
            ty: SettingType::String,
            options: None,
            path: None,
        },
    );
    // teammateMode
    m.insert(
        "teammateMode",
        SettingConfig {
            source: Source::Global,
            ty: SettingType::String,
            options: Some(TEAMMATE_MODES),
            path: None,
        },
    );
    // askUserQuestionTimeout — /config "Input & controls" row "Question
    // auto-continue timeout" (getter `Yye()`/`uSn`). Stored in user settings;
    // enum 60s|5m|10m|never, default never (block on the user).
    m.insert(
        "askUserQuestionTimeout",
        SettingConfig {
            source: Source::Settings,
            ty: SettingType::String,
            options: Some(ASK_USER_QUESTION_TIMEOUTS),
            path: None,
        },
    );
    m.insert(
        "dialogExpiry",
        SettingConfig {
            source: Source::Settings,
            ty: SettingType::String,
            options: Some(DIALOG_EXPIRY),
            path: None,
        },
    );
    m.insert(
        "crossSessionInbound",
        SettingConfig {
            source: Source::Settings,
            ty: SettingType::String,
            options: Some(CROSS_SESSION_INBOUND),
            path: None,
        },
    );
    // Kairos push notifications are a two-part gate in Claude Code: the
    // setting is exposed only when the feature flag is active.
    if telemetry::flag_bool("tengu_kairos_push_notifications", false) {
        m.insert(
            "agentPushNotifEnabled",
            SettingConfig {
                source: Source::Settings,
                ty: SettingType::Boolean,
                options: None,
                path: None,
            },
        );
    }
    m
});

/// `isSupported(key)` (`supportedSettings.ts:188-190`).
#[must_use]
pub fn is_supported(key: &str) -> bool {
    SUPPORTED_SETTINGS.contains_key(key)
}

/// `getConfig(key)` (`supportedSettings.ts:192-194`).
#[must_use]
pub fn get_config(key: &str) -> Option<SettingConfig> {
    SUPPORTED_SETTINGS.get(key).copied()
}

/// `getOptionsForSetting(key)` (`supportedSettings.ts:200-206`).
///
/// `getOptions()` (dynamic model list) has no substrate here, so this returns
/// only static `options`.
#[must_use]
pub fn get_options_for_setting(key: &str) -> Option<Vec<String>> {
    let config = SUPPORTED_SETTINGS.get(key)?;
    config
        .options
        .map(|opts| opts.iter().map(|s| (*s).to_string()).collect())
}

/// `getPath(key)` (`supportedSettings.ts:208-211`): explicit `path` else
/// `key.split('.')`.
#[must_use]
pub fn get_path(key: &str) -> Vec<String> {
    match get_config(key).and_then(|c| c.path) {
        Some(p) => p.iter().map(|s| (*s).to_string()).collect(),
        None => key.split('.').map(str::to_string).collect(),
    }
}

/// `buildNestedObject(path, value)` (`ConfigTool.ts:455-467`).
fn build_nested_object(path: &[String], value: Value) -> Value {
    if path.is_empty() {
        return Value::Object(Map::new());
    }
    let key = path[0].clone();
    if path.len() == 1 {
        let mut m = Map::new();
        m.insert(key, value);
        return Value::Object(m);
    }
    let mut m = Map::new();
    m.insert(key, build_nested_object(&path[1..], value));
    Value::Object(m)
}

/// `getValue(source, path)` (`ConfigTool.ts:436-453`).
///
/// In this stub both sources read from the same settings object, so `source`
/// only governs the lookup shape: `'global'` reads the flat top-level key
/// (`path[0]`); `'settings'` walks the dotted path. Returns `Value::Null` when
/// absent (the JSON image of TS `undefined`).
fn get_value(source: Source, path: &[String], settings: &Map<String, Value>) -> Value {
    match source {
        Source::Global => match path.first() {
            Some(key) => settings.get(key).cloned().unwrap_or(Value::Null),
            None => Value::Null,
        },
        Source::Settings => {
            // Walk the dotted path through the settings object.
            let mut current = Value::Object(settings.clone());
            for key in path {
                match current {
                    Value::Object(ref m) => match m.get(key) {
                        Some(v) => current = v.clone(),
                        None => return Value::Null,
                    },
                    _ => return Value::Null,
                }
            }
            current
        }
    }
}

fn home_dir_or_internal() -> Result<PathBuf, ToolError> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| ToolError::Internal("Config: HOME directory not available".into()))
}

#[must_use]
pub(crate) fn config_path(home: &Path) -> PathBuf {
    config_home_dir(home).join(CONFIG_FILE_NAME)
}

/// User config-home: `$LINGXI_CONFIG_DIR` when set (claude-code `tr()` `??`: an
/// empty value is honored verbatim → cwd-relative), else `<home>/<CONFIG_SUBDIR>`.
#[must_use]
fn config_home_dir(home: &Path) -> PathBuf {
    match std::env::var_os(branding::CONFIG_DIR_ENV) {
        Some(dir) => PathBuf::from(dir),
        None => home.join(CONFIG_SUBDIR),
    }
}

/// `ConfigTool` — get/set LingXi settings.
pub struct ConfigTool {
    pub(crate) ctx: tool_api::BuiltinToolContext,
}

impl ConfigTool {
    /// Construct.
    #[must_use]
    pub fn new(ctx: tool_api::BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

static SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "setting": {
                "type": "string",
                "description": "The setting key (e.g., \"theme\", \"model\", \"permissions.defaultMode\")"
            },
            "value": {
                "description": "The new value. Omit to get current value."
            }
        },
        "required": ["setting"],
        "additionalProperties": false
    })
});

fn pii_str(s: &str) -> AnalyticsValue {
    AnalyticsValue::String(PiiTagged::assert_pii_tagged_column(s.to_string()).into_inner())
}

fn verified_str(s: &str) -> AnalyticsValue {
    AnalyticsValue::String(Verified::assert_safe(s.to_string()).into_inner())
}

async fn emit_failed(bus: &Arc<AnalyticsBus>, kind: &str, duration_ms: u64) {
    let mut md: LogEventMetadata = HashMap::new();
    md.insert("error_kind".into(), verified_str(kind));
    md.insert(
        "duration_ms".into(),
        AnalyticsValue::Int(duration_ms as i64),
    );
    bus.log_event(CONFIG_FAILED, md).await;
}

async fn read_settings_obj(path: &Path) -> Result<Map<String, Value>, ToolError> {
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        return Ok(Map::new());
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| ToolError::Io(format!("Config: io error at {}: {e}", path.display())))?;
    if bytes.is_empty() {
        return Ok(Map::new());
    }
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| {
        ToolError::Io(format!(
            "Config: settings.json at {} is invalid JSON: {e}",
            path.display()
        ))
    })?;
    match v {
        Value::Object(m) => Ok(m),
        other => Err(ToolError::Io(format!(
            "Config: settings.json at {} is not a JSON object (got {})",
            path.display(),
            match &other {
                Value::Null => "null",
                Value::Bool(_) => "bool",
                Value::Number(_) => "number",
                Value::String(_) => "string",
                Value::Array(_) => "array",
                Value::Object(_) => unreachable!(),
            }
        ))),
    }
}

async fn write_settings_obj(path: &Path, obj: &Map<String, Value>) -> Result<usize, ToolError> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| ToolError::Io(format!("Config: io error at {}: {e}", dir.display())))?;
    }
    let body = serde_json::to_vec_pretty(&Value::Object(obj.clone()))
        .map_err(|e| ToolError::Internal(format!("Config: serde error: {e}")))?;
    tokio::fs::write(path, &body)
        .await
        .map_err(|e| ToolError::Io(format!("Config: io error at {}: {e}", path.display())))?;
    Ok(body.len())
}

/// Merge a nested update object into `target` in place (deep merge of objects,
/// scalar values overwrite). Mirrors the effect of
/// `updateSettingsForSource('userSettings', buildNestedObject(path, value))`.
fn merge_into(target: &mut Map<String, Value>, update: &Map<String, Value>) {
    for (k, v) in update {
        match (target.get_mut(k), v) {
            (Some(Value::Object(existing)), Value::Object(incoming)) => {
                merge_into(existing, incoming);
            }
            _ => {
                target.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Coerce a boolean-typed value per `ConfigTool.ts:185-201`: a `"true"`/
/// `"false"` string (lowercased, trimmed) becomes the bool; everything else
/// passes through unchanged.
fn coerce_boolean(value: &Value) -> Value {
    if let Some(s) = value.as_str() {
        let lower = s.trim().to_lowercase();
        if lower == "true" {
            return Value::Bool(true);
        }
        if lower == "false" {
            return Value::Bool(false);
        }
    }
    value.clone()
}

/// `String(finalValue)` for options/error messages — JS `String()` of a JSON
/// value: strings as-is, bool/number lexically, null ⇒ "null".
fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// `jsonStringify(value)` — `JSON.stringify`, used in the `Set X to Y` prompt.
fn json_stringify(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// Build the success/error output envelope (`outputSchema`,
/// `ConfigTool.ts:51-61`).
fn err_data(setting: &str, error: &str) -> Value {
    json!({ "success": false, "operation": "set", "setting": setting, "error": error })
}

#[async_trait]
impl Tool for ConfigTool {
    fn name(&self) -> &str {
        CONFIG_TOOL_NAME
    }
    fn search_hint(&self) -> Option<&str> {
        Some("get or set LingXi settings (theme, model)")
    }
    fn input_schema(&self) -> &Value {
        &SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn should_defer(&self) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        100_000
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, input: &Value) -> bool {
        input.get("value").is_none()
    }
    fn is_destructive(&self, _: &Value) -> bool {
        false
    }
    fn is_open_world(&self, _: &Value) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        InterruptBehavior::Block
    }

    async fn check_permissions(&self, input: &Value, _: &ToolUseContext) -> PermissionResult {
        // Auto-allow reading configs (value omitted).
        if input.get("value").is_none() {
            return PermissionResult::Allow {
                reason: PermissionDecisionReason::Other {
                    reason: "Config reads ~/.lingxi/settings.json (read-only)".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: PermissionMetadata::default(),
            };
        }
        let setting = input
            .get("setting")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let value = input.get("value").cloned().unwrap_or(Value::Null);
        let message = format!("Set {setting} to {}", json_stringify(&value));
        PermissionResult::Ask {
            reason: PermissionDecisionReason::Other {
                reason: "Config writes ~/.lingxi/settings.json".into(),
            },
            prompt: PermissionPrompt {
                title: "Config".into(),
                message,
                options: vec![],
            },
            pending_classifier_check: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "Get or set LingXi configuration settings.".into()
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        "Get or set LingXi configuration settings.".into()
    }

    async fn validate_input(
        &self,
        input: &Value,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        let _ = input
            .get("setting")
            .and_then(Value::as_str)
            .ok_or_else(|| ValidationError("Config: missing or non-string setting".into()))?;
        Ok(())
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let started = Instant::now();
        let bus = self.ctx.bus.clone();

        let setting = match input.get("setting").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                emit_failed(
                    &bus,
                    "missing_setting",
                    started.elapsed().as_millis() as u64,
                )
                .await;
                return Err(ToolError::InvalidInput(
                    "Config: missing or non-string setting".into(),
                ));
            }
        };
        // value omitted ⇒ GET, present ⇒ SET. JSON null is a present value.
        let value = input.get("value").cloned();
        let is_get = value.is_none();

        let mut md: LogEventMetadata = HashMap::new();
        md.insert(
            "op".into(),
            verified_str(if is_get { "get" } else { "set" }),
        );
        md.insert("setting".into(), verified_str(&setting));
        bus.log_event(CONFIG_STARTED, md).await;

        // 1. Check if setting is supported.
        if !is_supported(&setting) {
            emit_failed(
                &bus,
                "unknown_setting",
                started.elapsed().as_millis() as u64,
            )
            .await;
            return Ok(done(
                json!({ "success": false, "error": format!("Unknown setting: \"{setting}\"") }),
            ));
        }

        let config = get_config(&setting).expect("supported ⇒ config present");
        let path = get_path(&setting);

        let home = match home_dir_or_internal() {
            Ok(h) => h,
            Err(e) => {
                emit_failed(&bus, "no_home", started.elapsed().as_millis() as u64).await;
                return Err(e);
            }
        };
        let file = config_path(&home);

        // 2. GET operation.
        if is_get {
            let settings = match read_settings_obj(&file).await {
                Ok(o) => o,
                Err(e) => {
                    emit_failed(&bus, "io_read", started.elapsed().as_millis() as u64).await;
                    return Err(e);
                }
            };
            let current = get_value(config.source, &path, &settings);
            emit_completed(&bus, started, "get", &setting, &file).await;
            return Ok(done(json!({
                "success": true,
                "operation": "get",
                "setting": setting,
                "value": current,
            })));
        }

        // 3. SET operation.
        let raw_value = value.expect("set ⇒ value present");
        let mut final_value = raw_value.clone();

        // Coerce and validate boolean values (ConfigTool.ts:185-201).
        if config.ty == SettingType::Boolean {
            final_value = coerce_boolean(&raw_value);
            if !final_value.is_boolean() {
                emit_failed(&bus, "not_boolean", started.elapsed().as_millis() as u64).await;
                return Ok(done(err_data(
                    &setting,
                    &format!("{setting} requires true or false."),
                )));
            }
        }

        // Check options (ConfigTool.ts:204-214).
        if let Some(options) = get_options_for_setting(&setting) {
            let candidate = js_string(&final_value);
            if !options.iter().any(|o| o == &candidate) {
                emit_failed(&bus, "invalid_option", started.elapsed().as_millis() as u64).await;
                return Ok(done(err_data(
                    &setting,
                    &format!(
                        "Invalid value \"{}\". Options: {}",
                        js_string(&raw_value),
                        options.join(", ")
                    ),
                )));
            }
        }

        // validateOnWrite (async model API check) — no substrate; skipped.

        // 4. Write to storage. previousValue is read pre-write.
        let mut settings = match read_settings_obj(&file).await {
            Ok(o) => o,
            Err(e) => {
                emit_failed(&bus, "io_read", started.elapsed().as_millis() as u64).await;
                return Err(e);
            }
        };
        let previous_value = get_value(config.source, &path, &settings);

        match config.source {
            Source::Global => {
                // global: flat top-level key (path[0]).
                let key = match path.first() {
                    Some(k) => k.clone(),
                    None => {
                        emit_failed(&bus, "invalid_path", started.elapsed().as_millis() as u64)
                            .await;
                        return Ok(done(err_data(&setting, "Invalid setting path")));
                    }
                };
                settings.insert(key, final_value.clone());
            }
            Source::Settings => {
                // settings: deep-merge the nested update object.
                let update = build_nested_object(&path, final_value.clone());
                if let Value::Object(update_map) = update {
                    merge_into(&mut settings, &update_map);
                }
            }
        }

        if let Err(e) = write_settings_obj(&file, &settings).await {
            emit_failed(&bus, "io_write", started.elapsed().as_millis() as u64).await;
            return Err(e);
        }
        if setting == "agentPushNotifEnabled" {
            lingxi_core::host::session_flags::set_agent_push_notif_enabled(
                final_value.as_bool().unwrap_or(false),
            );
        }
        if setting == "includeGitInstructions" {
            // Republish so the git/PR sections follow the setting inside the
            // running session, like the attribution trailers below.
            lingxi_core::host::session_flags::set_include_git_instructions(final_value.as_bool());
        }
        if setting == "includeCoAuthoredBy" {
            // Republish so the attribution trailers follow the setting inside
            // the running session, like the output caps below.
            lingxi_core::host::session_flags::set_include_co_authored_by(final_value.as_bool());
        }
        if setting == "attribution" {
            let commit = final_value
                .get("commit")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let pr = final_value
                .get("pr")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            lingxi_core::host::session_flags::set_attribution(commit, pr);
        }
        if setting == "bashOutputMaxChars" {
            // Republish so the Bash cap moves with the setting inside the
            // running session, exactly as `taskOutputMaxChars` does below.
            lingxi_core::host::session_flags::set_bash_output_max_chars(
                final_value.as_u64().and_then(|n| u32::try_from(n).ok()),
            );
        }
        if setting == "taskOutputMaxChars" {
            // Republish so `TaskOutput`'s cap moves with the setting inside the
            // running session, exactly as the push flag above does.
            lingxi_core::host::session_flags::set_task_output_max_chars(
                final_value.as_u64().and_then(|n| u32::try_from(n).ok()),
            );
        }

        emit_completed(&bus, started, "set", &setting, &file).await;
        Ok(done(json!({
            "success": true,
            "operation": "set",
            "setting": setting,
            "previousValue": previous_value,
            "newValue": final_value,
        })))
    }
}

/// Wrap result `data` in a `ToolCallResult` envelope.
fn done(data: Value) -> ToolCallResult {
    ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
        data,
        model_content: None,
        new_messages: vec![],
        context_modifier: None,
        is_error: false,
        mcp_meta: None,
    }
}

/// Emit the `tengu_tool_config_completed` event.
async fn emit_completed(
    bus: &Arc<AnalyticsBus>,
    started: Instant,
    op: &str,
    setting: &str,
    file: &Path,
) {
    let mut md: LogEventMetadata = HashMap::new();
    md.insert(
        "duration_ms".into(),
        AnalyticsValue::Int(started.elapsed().as_millis() as i64),
    );
    md.insert("op".into(), verified_str(op));
    md.insert("setting".into(), verified_str(setting));
    md.insert("_PROTO_path".into(), pii_str(&file.display().to_string()));
    bus.log_event(CONFIG_COMPLETED, md).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobile_linux_api::ProcessOutput;
    use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx, HOME_LOCK};

    fn dummy_out() -> ProcessOutput {
        ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        }
    }

    fn tool() -> ConfigTool {
        ConfigTool::new(shell_test_ctx(dummy_out()))
    }

    // ─── registry / pure-fn tests ───

    #[test]
    fn constants_locked() {
        assert_eq!(CONFIG_TOOL_NAME, "Config");
        assert_eq!(CONFIG_FILE_NAME, "settings.json");
        assert_eq!(CONFIG_SUBDIR, ".lingxi");
        // legacy parity-fixture surface
        assert_eq!(CONFIG_FIELDS_ALLOWED.len(), 4);
        assert_eq!(
            CONFIG_FIELDS_ALLOWED,
            ["model", "outputStyle", "theme", "verbose"]
        );
    }

    #[test]
    fn registry_has_core_set() {
        for key in [
            "theme",
            "editorMode",
            "verbose",
            "preferredNotifChannel",
            "autoCompactEnabled",
            "autoMemoryEnabled",
            "autoDreamEnabled",
            "fileCheckpointingEnabled",
            "showTurnDuration",
            "terminalProgressBarEnabled",
            "todoFeatureEnabled",
            "model",
            "alwaysThinkingEnabled",
            "permissions.defaultMode",
            "language",
            "teammateMode",
            "askUserQuestionTimeout",
            "dialogExpiry",
            "crossSessionInbound",
        ] {
            assert!(is_supported(key), "missing setting {key}");
        }
        assert_eq!(SUPPORTED_SETTINGS.len(), 19);
    }

    #[test]
    fn feature_gated_entries_absent() {
        for key in [
            "voiceEnabled",
            "remoteControlAtStartup",
            "taskCompleteNotifEnabled",
            "inputNeededNotifEnabled",
            "agentPushNotifEnabled",
            "classifierPermissionsEnabled",
        ] {
            assert!(!is_supported(key), "unexpected gated setting {key}");
        }
    }

    #[test]
    fn get_config_shapes() {
        let theme = get_config("theme").unwrap();
        assert_eq!(theme.source, Source::Global);
        assert_eq!(theme.ty, SettingType::String);
        assert_eq!(theme.options, Some(THEME_NAMES));

        let verbose = get_config("verbose").unwrap();
        assert_eq!(verbose.ty, SettingType::Boolean);
        assert!(verbose.options.is_none());

        let model = get_config("model").unwrap();
        assert_eq!(model.source, Source::Settings);
        assert_eq!(model.ty, SettingType::String);

        assert!(get_config("nope").is_none());
    }

    #[test]
    fn options_for_setting() {
        assert_eq!(
            get_options_for_setting("theme"),
            Some(
                vec![
                    "dark",
                    "light",
                    "light-daltonized",
                    "dark-daltonized",
                    "light-ansi",
                    "dark-ansi"
                ]
                .into_iter()
                .map(String::from)
                .collect()
            )
        );
        assert_eq!(
            get_options_for_setting("permissions.defaultMode"),
            Some(
                vec!["default", "plan", "acceptEdits", "dontAsk"]
                    .into_iter()
                    .map(String::from)
                    .collect()
            )
        );
        // askUserQuestionTimeout — enum 60s|5m|10m|never (default never).
        assert_eq!(
            get_options_for_setting("askUserQuestionTimeout"),
            Some(
                vec!["60s", "5m", "10m", "never"]
                    .into_iter()
                    .map(String::from)
                    .collect()
            )
        );
        assert!(is_supported("askUserQuestionTimeout"));
        assert_eq!(
            get_options_for_setting("dialogExpiry"),
            Some(
                vec!["default", "60s", "5m", "10m", "never"]
                    .into_iter()
                    .map(String::from)
                    .collect()
            )
        );
        assert_eq!(
            get_options_for_setting("crossSessionInbound"),
            Some(
                vec!["default", "accept", "hold", "refuse"]
                    .into_iter()
                    .map(String::from)
                    .collect()
            )
        );
        // boolean / free-string settings have no options
        assert!(get_options_for_setting("verbose").is_none());
        assert!(get_options_for_setting("model").is_none());
        assert!(get_options_for_setting("language").is_none());
        assert!(get_options_for_setting("nope").is_none());
    }

    #[test]
    fn path_derivation() {
        assert_eq!(get_path("theme"), vec!["theme".to_string()]);
        assert_eq!(
            get_path("permissions.defaultMode"),
            vec!["permissions".to_string(), "defaultMode".to_string()]
        );
        // unknown keys still split on '.'
        assert_eq!(
            get_path("a.b.c"),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn build_nested_object_shapes() {
        assert_eq!(
            build_nested_object(&["model".to_string()], json!("opus")),
            json!({ "model": "opus" })
        );
        assert_eq!(
            build_nested_object(
                &["permissions".to_string(), "defaultMode".to_string()],
                json!("plan")
            ),
            json!({ "permissions": { "defaultMode": "plan" } })
        );
        assert_eq!(build_nested_object(&[], json!("x")), json!({}));
    }

    #[test]
    fn coerce_boolean_rules() {
        assert_eq!(coerce_boolean(&json!("true")), json!(true));
        assert_eq!(coerce_boolean(&json!("FALSE")), json!(false));
        assert_eq!(coerce_boolean(&json!("  True  ")), json!(true));
        assert_eq!(coerce_boolean(&json!(true)), json!(true));
        // non-bool strings pass through unchanged (caller rejects)
        assert_eq!(coerce_boolean(&json!("yes")), json!("yes"));
        assert_eq!(coerce_boolean(&json!(5)), json!(5));
    }

    #[test]
    fn js_string_of_values() {
        assert_eq!(js_string(&json!("dark")), "dark");
        assert_eq!(js_string(&json!(true)), "true");
        assert_eq!(js_string(&json!(false)), "false");
        assert_eq!(js_string(&json!(7)), "7");
        assert_eq!(js_string(&json!(null)), "null");
    }

    #[test]
    fn get_value_global_and_settings() {
        let mut s = Map::new();
        s.insert("theme".into(), json!("dark"));
        let mut perms = Map::new();
        perms.insert("defaultMode".into(), json!("plan"));
        s.insert("permissions".into(), Value::Object(perms));

        assert_eq!(
            get_value(Source::Global, &["theme".to_string()], &s),
            json!("dark")
        );
        assert_eq!(
            get_value(
                Source::Settings,
                &["permissions".to_string(), "defaultMode".to_string()],
                &s
            ),
            json!("plan")
        );
        // missing ⇒ null (TS undefined)
        assert_eq!(
            get_value(Source::Global, &["missing".to_string()], &s),
            Value::Null
        );
        assert_eq!(
            get_value(
                Source::Settings,
                &["permissions".to_string(), "nope".to_string()],
                &s
            ),
            Value::Null
        );
    }

    // ─── call() integration tests ───

    #[tokio::test]
    async fn unknown_setting_error() {
        let _g = HOME_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        let out = tool()
            .call(
                json!({ "setting": "telemetry_enabled" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok envelope");
        assert_eq!(out.data["success"], json!(false));
        assert_eq!(
            out.data["error"],
            json!("Unknown setting: \"telemetry_enabled\"")
        );
    }

    #[tokio::test]
    async fn get_missing_returns_null_value() {
        let _g = HOME_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        let out = tool()
            .call(json!({ "setting": "model" }), fresh_ctx(), fresh_tx())
            .await
            .expect("ok");
        assert_eq!(out.data["success"], json!(true));
        assert_eq!(out.data["operation"], json!("get"));
        assert_eq!(out.data["setting"], json!("model"));
        assert_eq!(out.data["value"], Value::Null);
    }

    #[tokio::test]
    async fn set_then_get_string_roundtrip() {
        let _g = HOME_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        let t = tool();
        let set = t
            .call(
                json!({ "setting": "model", "value": "claude-sonnet-4-5" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("set ok");
        assert_eq!(set.data["success"], json!(true));
        assert_eq!(set.data["operation"], json!("set"));
        assert_eq!(set.data["previousValue"], Value::Null);
        assert_eq!(set.data["newValue"], json!("claude-sonnet-4-5"));

        let get = t
            .call(json!({ "setting": "model" }), fresh_ctx(), fresh_tx())
            .await
            .expect("get ok");
        assert_eq!(get.data["value"], json!("claude-sonnet-4-5"));
    }

    #[tokio::test]
    async fn set_boolean_coerces_true_string() {
        let _g = HOME_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        let t = tool();
        let set = t
            .call(
                json!({ "setting": "verbose", "value": "true" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("set ok");
        assert_eq!(set.data["success"], json!(true));
        assert_eq!(set.data["newValue"], json!(true)); // coerced to bool

        let get = t
            .call(json!({ "setting": "verbose" }), fresh_ctx(), fresh_tx())
            .await
            .expect("get ok");
        assert_eq!(get.data["value"], json!(true));
    }

    #[tokio::test]
    async fn set_boolean_accepts_native_bool() {
        let _g = HOME_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        let out = tool()
            .call(
                json!({ "setting": "verbose", "value": false }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok");
        assert_eq!(out.data["success"], json!(true));
        assert_eq!(out.data["newValue"], json!(false));
    }

    #[tokio::test]
    async fn verbose_rejects_non_bool_string() {
        let _g = HOME_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        let out = tool()
            .call(
                json!({ "setting": "verbose", "value": "yes" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok envelope");
        assert_eq!(out.data["success"], json!(false));
        assert_eq!(out.data["operation"], json!("set"));
        assert_eq!(out.data["setting"], json!("verbose"));
        assert_eq!(out.data["error"], json!("verbose requires true or false."));
    }

    #[tokio::test]
    async fn options_rejection_message_format() {
        let _g = HOME_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        let out = tool()
            .call(
                json!({ "setting": "theme", "value": "neon" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("ok envelope");
        assert_eq!(out.data["success"], json!(false));
        assert_eq!(
            out.data["error"],
            json!("Invalid value \"neon\". Options: dark, light, light-daltonized, dark-daltonized, light-ansi, dark-ansi")
        );
    }

    #[tokio::test]
    async fn dotted_permissions_default_mode_roundtrips_nested() {
        let _g = HOME_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        let t = tool();
        let set = t
            .call(
                json!({ "setting": "permissions.defaultMode", "value": "plan" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("set ok");
        assert_eq!(set.data["success"], json!(true));
        assert_eq!(set.data["newValue"], json!("plan"));

        // On-disk JSON nests under permissions.defaultMode.
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        let raw = tokio::fs::read(config_path(&home)).await.unwrap();
        let v: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["permissions"]["defaultMode"], json!("plan"));

        let get = t
            .call(
                json!({ "setting": "permissions.defaultMode" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("get ok");
        assert_eq!(get.data["value"], json!("plan"));
    }

    #[tokio::test]
    async fn nested_set_preserves_sibling_keys() {
        let _g = HOME_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        let home = tmp.path().to_path_buf();
        // Seed an existing permissions sibling.
        let p = config_path(&home);
        tokio::fs::create_dir_all(p.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &p,
            serde_json::to_vec_pretty(&json!({ "permissions": { "allow": ["Bash"] } })).unwrap(),
        )
        .await
        .unwrap();

        tool()
            .call(
                json!({ "setting": "permissions.defaultMode", "value": "acceptEdits" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("set ok");

        let raw = tokio::fs::read(&p).await.unwrap();
        let v: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["permissions"]["defaultMode"], json!("acceptEdits"));
        // sibling survived the deep-merge
        assert_eq!(v["permissions"]["allow"], json!(["Bash"]));
    }

    #[tokio::test]
    async fn set_returns_previous_value() {
        let _g = HOME_LOCK.lock().await;
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        let t = tool();
        t.call(
            json!({ "setting": "model", "value": "opus" }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .expect("first set");
        let second = t
            .call(
                json!({ "setting": "model", "value": "haiku" }),
                fresh_ctx(),
                fresh_tx(),
            )
            .await
            .expect("second set");
        assert_eq!(second.data["previousValue"], json!("opus"));
        assert_eq!(second.data["newValue"], json!("haiku"));
    }

    #[tokio::test]
    async fn is_read_only_reflects_value_presence() {
        let t = tool();
        assert!(t.is_read_only(&json!({ "setting": "theme" })));
        assert!(!t.is_read_only(&json!({ "setting": "theme", "value": "dark" })));
        // explicit null value is still a SET (present)
        assert!(!t.is_read_only(&json!({ "setting": "theme", "value": null })));
    }

    #[tokio::test]
    async fn check_permissions_allows_read_asks_on_write() {
        let t = tool();
        let read = t
            .check_permissions(&json!({ "setting": "theme" }), &fresh_ctx())
            .await;
        assert!(matches!(read, PermissionResult::Allow { .. }));

        let write = t
            .check_permissions(
                &json!({ "setting": "theme", "value": "dark" }),
                &fresh_ctx(),
            )
            .await;
        match write {
            PermissionResult::Ask { prompt, .. } => {
                assert_eq!(prompt.message, "Set theme to \"dark\"");
            }
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_metadata_flags() {
        let t = tool();
        assert_eq!(t.name(), "Config");
        assert_eq!(
            t.search_hint(),
            Some("get or set LingXi settings (theme, model)")
        );
        assert!(t.should_defer());
        assert_eq!(t.max_result_size_chars(), 100_000);
        assert!(t.is_concurrency_safe(&json!({})));
    }
}
