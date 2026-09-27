//! `tengu_settings_*` event schemas — 3 events (M3-01 emits).
//!
//! Spec §7 line 752-792. M3-01 (`crates/core/src/settings/mod.rs`) emits exactly
//! these three event names: `tengu_settings_loaded` on every successful load,
//! `tengu_settings_invalid_env` once per unrecognized env var, and
//! `tengu_settings_parse_error` before returning a settings-file parse failure.

use crate::pii::Verified;
use serde::{Deserialize, Serialize};

/// `tengu_settings_loaded` — settings merged from all layers and ready to use.
pub const LOADED: &str = "tengu_settings_loaded";
/// `tengu_settings_invalid_env` — one env var failed parse (logged, not fatal).
pub const INVALID_ENV: &str = "tengu_settings_invalid_env";
/// `tengu_settings_parse_error` — a settings-file layer failed to parse.
pub const PARSE_ERROR: &str = "tengu_settings_parse_error";

pub(crate) const NAMES: &[&str] = &[LOADED, INVALID_ENV, PARSE_ERROR];

/// Payload for [`LOADED`]: how many of the 4 layers contributed and total time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadedPayload {
    /// How many of the 4 layers (env, user, project, defaults) contributed values.
    pub layers_present: u32,
    /// Wall-clock load duration in milliseconds.
    pub duration_ms: u64,
}

/// Payload for [`INVALID_ENV`]: variable name + reason, both PII-verified safe.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvalidEnvPayload {
    /// The env-var name (whitelisted, no user secret values).
    pub var: Verified,
    /// Why the var was rejected (e.g. `bad_bool`, `unknown_key`).
    pub reason: Verified,
}

/// Payload for [`PARSE_ERROR`]: which layer failed and the error string.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParseErrorPayload {
    /// Which settings layer failed to parse.
    pub layer: SettingsLayer,
    /// Whitelisted error kind (no raw IO details).
    pub error: Verified,
}

/// Settings layer source identifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SettingsLayer {
    /// `LINGXI_*` environment variables.
    Env,
    /// User-scope settings file (e.g. `~/.lingxi/settings.json`).
    User,
    /// Project-scope settings file (e.g. `.lingxi/settings.json`).
    Project,
    /// Built-in default values.
    Defaults,
}
