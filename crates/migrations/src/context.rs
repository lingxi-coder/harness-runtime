//! Cross-migration context: provider/subscription gates + JS-semantics
//! helpers + the per-run environment bundle.

use std::path::PathBuf;
use std::sync::Arc;

use llm_runtime::auth::anthropic::limits::SubscriptionType;
use serde_json::Value;
use telemetry::AnalyticsBus;

/// `isEnvTruthy` (`envUtils.ts:32-37`): unset/empty ⇒ false; else
/// lowercase-trim ∈ {1, true, yes, on}. Same semantics as
/// `lingxi_core::host::env::is_env_truthy`; consolidation blocked: no `traits` dep (and
/// this is pub API of the crate).
#[must_use]
pub fn is_env_truthy(value: Option<&str>) -> bool {
    let Some(v) = value else { return false };
    matches!(
        v.trim().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Read + truthy-test an env var in one go.
#[must_use]
pub fn env_var_truthy(name: &str) -> bool {
    is_env_truthy(std::env::var(name).ok().as_deref())
}

/// JS `Boolean(x)` over a JSON value (for raw-map reads where TS relies on
/// truthiness, e.g. `Boolean(oldValue)` / `!!userSettings.env?.X`).
#[must_use]
pub fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Gates derived from the host environment (TS `getAPIProvider` +
/// subscription state).
#[derive(Debug, Clone)]
pub struct MigrationContext {
    /// `getAPIProvider() === 'firstParty'` (`providers.ts:6-14`): true unless
    /// `CLAUDE_CODE_USE_BEDROCK` / `CLAUDE_CODE_USE_VERTEX` /
    /// `CLAUDE_CODE_USE_FOUNDRY` is env-truthy.
    pub first_party: bool,
    /// Subscription tier. STRUCTURALLY `None` today: the Rust keychain token
    /// (`secret/src/credential.rs::OAuthTokens`) has no `subscriptionType`
    /// field. TS itself fails closed on unknown tier
    /// (`model.ts:322-330`), so `None` ⇒ the gated migrations take their
    /// faithful not-eligible branches. A future tier-persistence batch
    /// lights the eligible paths up without API change.
    pub subscription_type: Option<SubscriptionType>,
}

impl MigrationContext {
    /// Derive the context from process env. Tier is `None` (see field doc);
    /// the CLI deliberately does NOT read the keychain pre-boot for this
    /// (avoids a second keychain prompt).
    #[must_use]
    pub fn from_env() -> Self {
        let third_party = env_var_truthy("CLAUDE_CODE_USE_BEDROCK")
            || env_var_truthy("CLAUDE_CODE_USE_VERTEX")
            || env_var_truthy("CLAUDE_CODE_USE_FOUNDRY");
        Self {
            first_party: !third_party,
            subscription_type: None,
        }
    }
}

/// Everything one migration run needs: explicit paths (so tests never touch
/// process env), gates, and an optional telemetry bus.
pub struct MigrationEnv {
    /// `~/.lingxi.json` (resolved by `global_config::global_config_path`).
    pub global_config_path: PathBuf,
    /// `~/.claude` (config home — settings.json + cache/ live here).
    pub lingxi_config_home: PathBuf,
    /// Project directory (for `settings.local.json` + the project-config key).
    pub project_dir: PathBuf,
    /// Provider/subscription gates.
    pub ctx: MigrationContext,
    /// Telemetry sink; `None` = no events (unit tests, and the CLI today —
    /// no pre-boot bus substrate exists, same as the startup deprecation
    /// notice; names are registered for the future wiring).
    pub bus: Option<Arc<AnalyticsBus>>,
}

impl MigrationEnv {
    /// Emit a tengu event if a bus is wired.
    pub(crate) async fn emit(&self, name: &str, metadata: telemetry::sink::LogEventMetadata) {
        if let Some(bus) = &self.bus {
            bus.log_event(name, metadata).await;
        }
    }

    /// Epoch milliseconds (`Date.now()` parity for the `*Timestamp` keys).
    pub(crate) fn now_ms() -> i64 {
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        )
        .unwrap_or(i64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::env_lock;

    #[test]
    fn env_truthy_values() {
        assert!(is_env_truthy(Some("1")));
        assert!(is_env_truthy(Some("TRUE")));
        assert!(is_env_truthy(Some(" yes ")));
        assert!(is_env_truthy(Some("on")));
        assert!(!is_env_truthy(Some("0")));
        assert!(!is_env_truthy(Some("")));
        assert!(!is_env_truthy(Some("anything")));
        assert!(!is_env_truthy(None));
    }

    #[test]
    fn js_truthy_values() {
        use serde_json::json;
        assert!(!js_truthy(&json!(null)));
        assert!(!js_truthy(&json!(false)));
        assert!(!js_truthy(&json!(0)));
        assert!(!js_truthy(&json!("")));
        assert!(js_truthy(&json!(true)));
        assert!(js_truthy(&json!(1)));
        assert!(js_truthy(&json!("x")));
        assert!(js_truthy(&json!([])));
        assert!(js_truthy(&json!({})));
    }

    #[test]
    fn first_party_unless_third_party_env() {
        let _g = env_lock();
        for var in [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ] {
            std::env::remove_var(var);
        }
        assert!(MigrationContext::from_env().first_party);
        std::env::set_var("CLAUDE_CODE_USE_BEDROCK", "1");
        assert!(!MigrationContext::from_env().first_party);
        std::env::remove_var("CLAUDE_CODE_USE_BEDROCK");
    }
}
