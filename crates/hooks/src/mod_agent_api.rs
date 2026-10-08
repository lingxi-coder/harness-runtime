//! Host-only input and cancellation context for the direct `$.agent` APIs.
//!
//! The worker wrapper normalizes the Agent tool payload before calling the
//! Host. Keep optional fields presence-aware so an omitted option stays
//! omitted and an explicit JSON `null` is not silently rewritten.

use lingxi_core::host::subagent_spawn::AgentSpawnProvenance;
use lingxi_core::host::task_registry::FieldPresence;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::ModError;

const MAX_CONCURRENT_SUBAGENTS_ENV: &str = "LINGXI_MAX_CONCURRENT_SUBAGENTS";

/// The exact Agent tool input constructed by Native `$.agent.spawn` before
/// entering its launch-only execution path.
#[derive(Debug, Clone, PartialEq)]
pub struct ModAgentSpawnInput {
    /// Required non-empty prompt after the Native wrapper check.
    pub prompt: String,
    /// `input.description ?? firstFivePromptWords`; retained as JSON so the
    /// canonical Agent schema remains the authority for its value type.
    pub description: Value,
    /// Optional wrapper fields retain missing/null/value distinctions until
    /// the canonical Agent runner validates them.
    pub model: FieldPresence<Value>,
    pub model_profile: FieldPresence<Value>,
    pub subagent_type: FieldPresence<Value>,
    pub name: FieldPresence<Value>,
    pub cwd: FieldPresence<Value>,
}

impl ModAgentSpawnInput {
    /// Parse the object emitted by Native's `mf(input, prompt)` wrapper.
    /// Unknown wrapper keys are discarded, matching the Native projection.
    pub fn from_native_wrapper(input: &Value) -> Result<Self, ModError> {
        let object = input
            .as_object()
            .ok_or_else(|| ModError::Protocol("agent.spawn input must be an object".into()))?;
        if object.get("tool").and_then(Value::as_str) != Some("Agent") {
            return Err(ModError::Protocol(
                "agent.spawn wrapper must select the Agent tool".into(),
            ));
        }
        let prompt = object
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|prompt| !prompt.trim().is_empty())
            .ok_or_else(|| {
                ModError::Protocol("agent.spawn wrapper needs a non-empty prompt".into())
            })?
            .to_owned();
        let description = object
            .get("description")
            .filter(|description| !description.is_null())
            .cloned()
            .ok_or_else(|| {
                ModError::Protocol("agent.spawn wrapper needs its resolved description".into())
            })?;
        if object.get("run_in_background") != Some(&Value::Bool(true)) {
            return Err(ModError::Protocol(
                "agent.spawn wrapper must request background launch".into(),
            ));
        }

        Ok(Self {
            prompt,
            description,
            model: field_presence(object, "model"),
            model_profile: field_presence(object, "model_profile"),
            subagent_type: field_presence(object, "subagent_type"),
            name: field_presence(object, "name"),
            cwd: field_presence(object, "cwd"),
        })
    }

    /// Rebuild the Native Agent tool input in wrapper insertion order. This is
    /// the value the Orchestrator's canonical Agent runner consumes.
    #[must_use]
    pub fn as_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("tool".into(), Value::String("Agent".into()));
        object.insert("prompt".into(), Value::String(self.prompt.clone()));
        object.insert("description".into(), self.description.clone());
        object.insert("run_in_background".into(), Value::Bool(true));
        insert_presence(&mut object, "model", &self.model);
        insert_presence(&mut object, "model_profile", &self.model_profile);
        insert_presence(&mut object, "subagent_type", &self.subagent_type);
        insert_presence(&mut object, "name", &self.name);
        insert_presence(&mut object, "cwd", &self.cwd);
        Value::Object(object)
    }
}

fn field_presence(object: &Map<String, Value>, key: &str) -> FieldPresence<Value> {
    match object.get(key) {
        None => FieldPresence::Missing,
        Some(Value::Null) => FieldPresence::Null,
        Some(value) => FieldPresence::Value(value.clone()),
    }
}

fn insert_presence(object: &mut Map<String, Value>, key: &str, value: &FieldPresence<Value>) {
    match value {
        FieldPresence::Missing => {}
        FieldPresence::Null => {
            object.insert(key.into(), Value::Null);
        }
        FieldPresence::Value(value) => {
            object.insert(key.into(), value.clone());
        }
    }
}

/// Host-only authority for one direct spawn API call. The request token is
/// scoped to launch admission; Native launch-only settlement gets a fresh
/// signal and must not inherit this caller token.
#[derive(Debug)]
pub struct ModAgentSpawnContext {
    /// Trusted plugin caller and origin from the host-issued API ticket.
    pub provenance: AgentSpawnProvenance,
    /// Cancellation authority for the in-flight API request only.
    pub request_cancellation: lingxi_core::host::CancellationToken,
    spawn_slot: ModAgentSpawnSlotLease,
    left_running_registered: AtomicBool,
}

impl ModAgentSpawnContext {
    /// Construct a host-only context from resolved provenance and a fresh
    /// request-scoped cancellation token.
    pub(super) fn new(
        provenance: AgentSpawnProvenance,
        request_cancellation: lingxi_core::host::CancellationToken,
        spawn_slot: ModAgentSpawnSlotLease,
    ) -> Self {
        Self {
            provenance,
            request_cancellation,
            spawn_slot,
            left_running_registered: AtomicBool::new(false),
        }
    }

    /// Retain this plugin's Native `$.agent.spawn` slot for the detached
    /// settlement promise that is passed to `leftRunning`. The caller must
    /// capture the returned guard in that same settlement task. If no child
    /// launches, or admission fails before `leftRunning`, do not call this;
    /// the request releases the slot when it returns.
    pub fn retain_left_running_lease(&self) -> Result<ModAgentSpawnSlotLease, ModError> {
        self.left_running_registered
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                ModError::Protocol(
                    "agent.spawn leftRunning settlement was registered more than once".into(),
                )
            })?;
        Ok(self.spawn_slot.clone())
    }
}

/// A host-only, cloneable lease. Clones share one permit, so the plugin slot
/// is released only after the request context and every settlement holder are
/// dropped.
#[derive(Clone, Debug)]
pub struct ModAgentSpawnSlotLease {
    _inner: Arc<ModAgentSpawnSlotLeaseInner>,
}

#[derive(Debug)]
struct ModAgentSpawnSlotLeaseInner {
    running: Arc<Mutex<HashMap<String, usize>>>,
    plugin: String,
}

impl Drop for ModAgentSpawnSlotLeaseInner {
    fn drop(&mut self) {
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = running.entry(self.plugin.clone()).or_default();
        *count = count.saturating_sub(1);
    }
}

#[derive(Default)]
pub(super) struct ModAgentSpawnBudget {
    running: Arc<Mutex<HashMap<String, usize>>>,
}

impl ModAgentSpawnBudget {
    pub(super) fn acquire(&self, plugin: &str) -> Result<ModAgentSpawnSlotLease, ModError> {
        self.acquire_with_policy(plugin, NativeSpawnLimit::from_process_environment)
    }

    #[cfg(test)]
    fn acquire_with_limit(
        &self,
        plugin: &str,
        limit: NativeSpawnLimit,
    ) -> Result<ModAgentSpawnSlotLease, ModError> {
        self.acquire_with_policy(plugin, || limit)
    }

    fn acquire_with_policy(
        &self,
        plugin: &str,
        read_limit: impl FnOnce() -> NativeSpawnLimit,
    ) -> Result<ModAgentSpawnSlotLease, ModError> {
        // Native reads the current limit for every admission and performs the
        // check-plus-increment synchronously. Keep both operations under one
        // lock so concurrent Rust API calls cannot over-admit the same plugin.
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = running.get(plugin).copied().unwrap_or_default();
        let limit = read_limit();
        if (count as f64) >= limit.comparison {
            return Err(ModError::Native(format!(
                "{plugin}: $.agent.spawn refused: {} spawns are running at once",
                limit.display
            )));
        }
        running.insert(plugin.to_owned(), count.saturating_add(1));
        drop(running);
        Ok(ModAgentSpawnSlotLease {
            _inner: Arc::new(ModAgentSpawnSlotLeaseInner {
                running: self.running.clone(),
                plugin: plugin.to_owned(),
            }),
        })
    }

    #[cfg(test)]
    pub(super) fn running_for(&self, plugin: &str) -> usize {
        self.running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(plugin)
            .copied()
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
struct NativeSpawnLimit {
    comparison: f64,
    display: String,
}

impl NativeSpawnLimit {
    fn from_process_environment() -> Self {
        match std::env::var_os(MAX_CONCURRENT_SUBAGENTS_ENV) {
            Some(value) => Self::from_environment_value(&value),
            None => Self::default(),
        }
    }

    fn from_environment_value(value: &OsStr) -> Self {
        let display = value.to_string_lossy().into_owned();
        Self {
            comparison: js_number(&display),
            display,
        }
    }
}

impl Default for NativeSpawnLimit {
    fn default() -> Self {
        Self {
            comparison: lingxi_core::host::subagent_spawn::DEFAULT_MAX_CONCURRENT_SUBAGENTS as f64,
            display: lingxi_core::host::subagent_spawn::DEFAULT_MAX_CONCURRENT_SUBAGENTS
                .to_string(),
        }
    }
}

/// JavaScript's `>=` converts the process.env string with `Number()` before
/// comparison. Invalid numeric text becomes NaN, so the comparison is false;
/// the unparsed string is still used in Native's error interpolation.
fn js_number(value: &str) -> f64 {
    let value = value.trim_matches(is_js_whitespace);
    if value.is_empty() {
        return 0.0;
    }
    match value {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }

    for (prefix, radix) in [
        ("0x", 16_u32),
        ("0X", 16),
        ("0b", 2),
        ("0B", 2),
        ("0o", 8),
        ("0O", 8),
    ] {
        if let Some(digits) = value.strip_prefix(prefix) {
            if digits.is_empty() {
                return f64::NAN;
            }
            let mut parsed = 0.0;
            for digit in digits.chars() {
                let Some(digit) = digit.to_digit(radix) else {
                    return f64::NAN;
                };
                parsed = parsed * f64::from(radix) + f64::from(digit);
            }
            return parsed;
        }
    }

    if is_js_decimal_number(value) {
        value.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

fn is_js_decimal_number(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = if bytes
        .first()
        .is_some_and(|byte| matches!(*byte, b'+' | b'-'))
    {
        1
    } else {
        0
    };
    let integer_start = index;
    while bytes.get(index).is_some_and(|byte| byte.is_ascii_digit()) {
        index += 1;
    }
    let has_integer = index > integer_start;
    let mut has_fraction = false;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fraction_start = index;
        while bytes.get(index).is_some_and(|byte| byte.is_ascii_digit()) {
            index += 1;
        }
        has_fraction = index > fraction_start;
    }
    if !has_integer && !has_fraction {
        return false;
    }
    if bytes
        .get(index)
        .is_some_and(|byte| matches!(*byte, b'e' | b'E'))
    {
        index += 1;
        if bytes
            .get(index)
            .is_some_and(|byte| matches!(*byte, b'+' | b'-'))
        {
            index += 1;
        }
        let exponent_start = index;
        while bytes.get(index).is_some_and(|byte| byte.is_ascii_digit()) {
            index += 1;
        }
        if index == exponent_start {
            return false;
        }
    }
    index == bytes.len()
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

pub(super) struct CancelModAgentSpawnRequestOnDrop(lingxi_core::host::CancellationToken);

impl CancelModAgentSpawnRequestOnDrop {
    pub(super) fn new(token: lingxi_core::host::CancellationToken) -> Self {
        Self(token)
    }
}

impl Drop for CancelModAgentSpawnRequestOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_preserves_model_profile_presence() {
        for profile in [None, Some(Value::Null), Some(Value::String("openai-custom".into()))] {
            let mut wrapper = serde_json::json!({
                "tool": "Agent", "prompt": "inspect", "description": "inspect",
                "run_in_background": true, "model": "gpt-model"
            });
            if let Some(value) = profile.clone() { wrapper["model_profile"] = value; }
            let input = ModAgentSpawnInput::from_native_wrapper(&wrapper).unwrap();
            assert_eq!(input.as_json().get("model_profile"), profile.as_ref());
        }
    }

    #[test]
    fn spawn_input_round_trips_optional_field_presence_in_native_order() {
        let input = ModAgentSpawnInput::from_native_wrapper(&serde_json::json!({
            "tool":"Agent",
            "prompt":"inspect this",
            "description":"inspect this",
            "run_in_background":true,
            "model":null,
            "subagent_type":"Explore",
            "cwd":"/tmp/project",
            "discarded":"not in the Native wrapper payload",
        }))
        .unwrap();

        assert_eq!(input.model, FieldPresence::Null);
        assert_eq!(
            input.subagent_type,
            FieldPresence::Value(Value::String("Explore".into()))
        );
        assert_eq!(input.name, FieldPresence::Missing);
        assert_eq!(
            input.as_json().to_string(),
            r#"{"tool":"Agent","prompt":"inspect this","description":"inspect this","run_in_background":true,"model":null,"subagent_type":"Explore","cwd":"/tmp/project"}"#
        );
    }

    #[test]
    fn spawn_input_rejects_non_wrapper_required_fields() {
        for input in [
            serde_json::json!(null),
            serde_json::json!({"tool":"Task","prompt":"inspect","description":"inspect","run_in_background":true}),
            serde_json::json!({"tool":"Agent","prompt":"   ","description":"inspect","run_in_background":true}),
            serde_json::json!({"tool":"Agent","prompt":"inspect","run_in_background":true}),
            serde_json::json!({"tool":"Agent","prompt":"inspect","description":"inspect","run_in_background":false}),
        ] {
            assert!(
                ModAgentSpawnInput::from_native_wrapper(&input).is_err(),
                "{input}"
            );
        }
    }

    #[test]
    fn native_spawn_limit_uses_default_and_javascript_number_comparison() {
        assert_eq!(NativeSpawnLimit::default().comparison, 20.0);
        assert_eq!(NativeSpawnLimit::default().display, "20");
        assert_eq!(
            NativeSpawnLimit::from_environment_value(OsStr::new(" 1.5 ")).comparison,
            1.5
        );
        assert_eq!(
            NativeSpawnLimit::from_environment_value(OsStr::new("0x10")).comparison,
            16.0
        );
        assert_eq!(
            NativeSpawnLimit::from_environment_value(OsStr::new(" ")).comparison,
            0.0
        );
        assert!(
            NativeSpawnLimit::from_environment_value(OsStr::new("invalid"))
                .comparison
                .is_nan()
        );
        assert_eq!(
            NativeSpawnLimit::from_environment_value(OsStr::new("invalid")).display,
            "invalid"
        );
    }

    #[test]
    fn spawn_slot_is_per_plugin_and_retained_until_left_running_lease_drops() {
        let budget = ModAgentSpawnBudget::default();
        let limit = NativeSpawnLimit::default();
        let plugin_slot = budget
            .acquire_with_limit("plugin-a", limit.clone())
            .unwrap();
        let other_plugin_slot = budget.acquire_with_limit("plugin-b", limit).unwrap();
        let context = ModAgentSpawnContext::new(
            AgentSpawnProvenance::default(),
            lingxi_core::host::CancellationToken::new(),
            plugin_slot,
        );
        let retained = context.retain_left_running_lease().unwrap();
        assert!(context.retain_left_running_lease().is_err());
        drop(context);

        assert_eq!(budget.running_for("plugin-a"), 1);
        assert_eq!(budget.running_for("plugin-b"), 1);
        drop(retained);
        assert_eq!(budget.running_for("plugin-a"), 0);
        drop(other_plugin_slot);
        assert_eq!(budget.running_for("plugin-b"), 0);
    }

    #[test]
    fn plugin_spawn_limit_refuses_before_increment_with_native_error_text() {
        let budget = ModAgentSpawnBudget::default();
        let one = NativeSpawnLimit {
            comparison: 1.0,
            display: "1".into(),
        };
        let first = budget
            .acquire_with_limit("busy-plugin", one.clone())
            .unwrap();
        let refused = budget.acquire_with_limit("busy-plugin", one).unwrap_err();
        assert_eq!(
            refused.to_string(),
            "busy-plugin: $.agent.spawn refused: 1 spawns are running at once"
        );
        assert_eq!(budget.running_for("busy-plugin"), 1);
        drop(first);
        assert_eq!(budget.running_for("busy-plugin"), 0);
    }

    #[test]
    fn concurrent_spawn_admissions_never_exceed_the_native_plugin_limit() {
        let budget = Arc::new(ModAgentSpawnBudget::default());
        let barrier = Arc::new(std::sync::Barrier::new(12));
        let mut workers = Vec::new();
        for _ in 0..12 {
            let budget = budget.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                let lease = budget.acquire_with_limit(
                    "same-plugin",
                    NativeSpawnLimit {
                        comparison: 1.0,
                        display: "1".into(),
                    },
                );
                let admitted = lease.is_ok();
                barrier.wait();
                drop(lease);
                admitted
            }));
        }
        let admitted = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|admitted| *admitted)
            .count();
        assert_eq!(admitted, 1);
        assert_eq!(budget.running_for("same-plugin"), 0);
    }
}
