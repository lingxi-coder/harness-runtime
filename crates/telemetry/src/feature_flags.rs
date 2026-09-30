//! Feature-flag client with cache + background refresh.
//!
//! See spec §26.4. A pluggable [`FeatureFlagsFetcher`] (`Statsig`,
//! `LaunchDarkly`, file-backed) supplies values; the client caches them and
//! refreshes on a fixed interval via the [`RuntimeSpawner`] trait so engine
//! code never spawns tasks directly.

use lingxi_core::host::{RuntimeError, RuntimeSpawner};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::RwLock as StdRwLock;
use std::time::Duration;
use tokio::sync::RwLock;

// ── Synchronous GrowthBook-style flag reader (binary `nt`) ───────────────────
//
// Binary `nt(key,default)` (cc_all.txt:504507) is a SYNC read of a cached flag
// snapshot with default-on-miss: it checks an override layer `ROt()` (the static
// `Uvi` map, normally null), then — if GrowthBook is enabled (`$4()`) — reads
// `Dt().cachedGrowthBookFeatures?.[key]`, returning the passed `default` whenever
// the flag is absent or GrowthBook is disabled. There is NO env layer inside `nt`
// itself; env overrides (e.g. `CLAUDE_CODE_LOOP_PERSISTENT`) are applied by the
// CALLER (`YIn`/`iKi`), not here.
//
// The port mirrors this with two process-global maps:
//   - `FLAG_SNAPSHOT`  = `Dt().cachedGrowthBookFeatures` — populated by the async
//     refresh loop if/when a real fetcher is wired; EMPTY by default (no fetcher
//     in prod), so `flag_bool` returns its `default` — byte-identical to the
//     shipped binary (GrowthBook-absent / flag-at-default).
//   - `FLAG_TEST_OVERRIDE` = `ROt()`'s `Uvi` — a test-only override layer so
//     tests can flip a flag without env vars; checked FIRST, exactly like `nt`.

fn flag_snapshot() -> &'static StdRwLock<HashMap<String, FeatureValue>> {
    static SNAP: OnceLock<StdRwLock<HashMap<String, FeatureValue>>> = OnceLock::new();
    SNAP.get_or_init(|| StdRwLock::new(HashMap::new()))
}

fn flag_test_override() -> &'static StdRwLock<HashMap<String, bool>> {
    static OVR: OnceLock<StdRwLock<HashMap<String, bool>>> = OnceLock::new();
    OVR.get_or_init(|| StdRwLock::new(HashMap::new()))
}

/// `nt(key, default)` (cc_all.txt:504507) — synchronous boolean flag read with
/// default-on-miss. Checks the test-override layer first (binary `ROt()`/`Uvi`),
/// then the cached snapshot (binary `cachedGrowthBookFeatures`), else returns
/// `default`. With no fetcher wired (the prod default) the snapshot is empty and
/// every read returns `default` — matching the shipped binary's GrowthBook-absent
/// behavior.
/// claude-code `oJ()` (2.1.263; `Yke()` in the older builds) — whether the
/// PushNotification surface is live for this session:
/// `nt("tengu_kairos_push_notifications", false) && mc("agentPushNotifEnabled",
/// false).value`.
///
/// Hosted here rather than in `cron` because three unrelated callers need the
/// SAME answer: the Monitor tool's description addendum (`ybn`), the
/// `ScheduleWakeup`/loop copy, and the orchestrator's per-monitor-event hint
/// (`GM`). `cron::is_push_notif_enabled` forwards to it.
#[must_use]
pub fn push_notifications_enabled() -> bool {
    flag_bool("tengu_kairos_push_notifications", false)
        && lingxi_core::host::session_flags::agent_push_notif_enabled()
}

#[must_use]
pub fn flag_bool(key: &str, default: bool) -> bool {
    if let Some(v) = flag_test_override().read().unwrap().get(key) {
        return *v;
    }
    match flag_snapshot().read().unwrap().get(key) {
        Some(FeatureValue::Bool(b)) => *b,
        _ => default,
    }
}

/// Test-only: set a flag in the override layer (binary `ROt()`/`Uvi`). Lets tests
/// exercise flag-gated paths without process-wide env vars.
pub fn test_set_flag(key: &str, value: bool) {
    flag_test_override()
        .write()
        .unwrap()
        .insert(key.to_string(), value);
}

/// Test-only: clear a flag from the override layer.
pub fn test_clear_flag(key: &str) {
    flag_test_override().write().unwrap().remove(key);
}

// ── Array-valued flag reader (binary `Ot(e,t)` / `I(e,[])`) ─────────────────
//
// A handful of oracle flags are ARRAY-valued rather than boolean — e.g. the
// MCP era-negotiation server denylist (`tengu_mcp_negotiation_server_denylist`,
// cc_all.txt @~182279107: `let o=I(e,[]); if(!Array.isArray(o)...)`). Same
// idiom as `flag_bool`/`nt`: a synchronous, default-on-miss read backed by the
// same process-global snapshot, plus its own test-override layer so tests can
// flip a list flag without touching the boolean one.

fn flag_list_test_override() -> &'static StdRwLock<HashMap<String, Vec<String>>> {
    static OVR: OnceLock<StdRwLock<HashMap<String, Vec<String>>>> = OnceLock::new();
    OVR.get_or_init(|| StdRwLock::new(HashMap::new()))
}

/// Look up a string-array flag, returning `default` on miss or type mismatch.
/// Checks the test-override layer first, then the cached snapshot's
/// `FeatureValue::Json` array (non-string elements are dropped), else
/// returns `default`. With no fetcher wired (the prod default) every read
/// returns `default` — an empty list for the denylist, matching the shipped
/// binary's GrowthBook-absent behavior.
#[must_use]
pub fn flag_string_list(key: &str, default: &[&str]) -> Vec<String> {
    if let Some(v) = flag_list_test_override().read().unwrap().get(key) {
        return v.clone();
    }
    match flag_snapshot().read().unwrap().get(key) {
        Some(FeatureValue::Json(serde_json::Value::Array(items))) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => default.iter().map(|s| (*s).to_string()).collect(),
    }
}

/// Test-only: set a string-array flag in the override layer.
pub fn test_set_flag_list(key: &str, value: Vec<String>) {
    flag_list_test_override()
        .write()
        .unwrap()
        .insert(key.to_string(), value);
}

/// Test-only: clear a string-array flag from the override layer.
pub fn test_clear_flag_list(key: &str) {
    flag_list_test_override().write().unwrap().remove(key);
}

/// Polymorphic feature-flag value.
///
/// Encoded with `#[serde(untagged)]` to match raw JSON shapes from
/// `Statsig` / `LaunchDarkly` responses without tag wrappers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum FeatureValue {
    /// Boolean flag.
    Bool(bool),
    /// Numeric flag (any JSON number).
    Number(f64),
    /// String flag (e.g. variant name).
    String(String),
    /// Arbitrary JSON for structured config flags.
    Json(serde_json::Value),
}

/// Fetcher abstraction backing [`FeatureFlagsClient`].
///
/// Implementations call out to whatever provider (Statsig, etc.) and return a
/// full snapshot of flag values. Errors are reported as `Result<_, String>`
/// to keep the trait provider-agnostic.
#[async_trait::async_trait]
pub trait FeatureFlagsFetcher: Send + Sync {
    /// Return a fresh snapshot of every flag known to the provider.
    async fn fetch(&self) -> Result<HashMap<String, FeatureValue>, String>;
}

/// Cached feature-flag client with a background refresh loop.
///
/// Construct via [`Self::new`], then call [`Self::start_refresh_loop`] once
/// during platform init to begin periodic refresh.
pub struct FeatureFlagsClient {
    cache: RwLock<HashMap<String, FeatureValue>>,
    cache_ttl: Duration,
    fetcher: Arc<dyn FeatureFlagsFetcher>,
    runtime: Arc<dyn RuntimeSpawner>,
}

impl FeatureFlagsClient {
    /// Construct a client with a 5-minute refresh cadence.
    #[must_use]
    pub fn new(fetcher: Arc<dyn FeatureFlagsFetcher>, runtime: Arc<dyn RuntimeSpawner>) -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
            cache_ttl: Duration::from_secs(300),
            fetcher,
            runtime,
        }
    }

    /// Look up a boolean flag, returning `default` on miss or type mismatch.
    pub async fn get_bool(&self, key: &str, default: bool) -> bool {
        match self.cache.read().await.get(key) {
            Some(FeatureValue::Bool(b)) => *b,
            _ => default,
        }
    }

    /// Spawn the background refresh loop. Returns once the spawn has been
    /// registered with the runtime; the loop then runs until the runtime
    /// shuts down.
    pub async fn start_refresh_loop(self: Arc<Self>) -> Result<(), RuntimeError> {
        let me = self.clone();
        self.runtime
            .spawn(
                "feature-flags-refresh",
                Box::pin(async move {
                    loop {
                        if let Ok(values) = me.fetcher.fetch().await {
                            // Mirror into the sync snapshot so `flag_bool` (binary
                            // `nt`) sees live values once a real fetcher is wired.
                            if let Ok(mut snap) = flag_snapshot().write() {
                                *snap = values.clone();
                            }
                            *me.cache.write().await = values;
                        }
                        tokio::time::sleep(me.cache_ttl).await;
                    }
                }),
            )
            .await?;
        Ok(())
    }
}
